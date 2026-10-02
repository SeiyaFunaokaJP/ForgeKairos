use anyhow::{Context as _, Result};
use client::Client;
use db::kvp::KeyValueStore;
// `smol::fs` is imported below under the same name, so the crate has to be
// named through the crate root here.
use ::fs::Fs;
use futures_lite::StreamExt;
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, Global, Task, TaskExt,
    Window, actions,
};
use http_client::{
    EgressDenied, HttpClient, RequestHost, UPDATE_REPO,
    github::{GithubRelease, get_release_by_tag_name, latest_github_release},
    ide_lane,
};
use paths::remote_servers_dir;
use release_channel::{AppCommitSha, ReleaseChannel};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsStore};
use smol::fs::File;
use smol::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use std::mem;
use std::{
    env::{
        self,
        consts::{ARCH, OS},
    },
    ffi::OsStr,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use util::command::new_command;
use workspace::Workspace;

const SHOULD_SHOW_UPDATE_NOTIFICATION_KEY: &str = "auto-updater-should-show-updated-notification";

#[derive(Debug)]
struct MissingDependencyError(String);

impl std::fmt::Display for MissingDependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MissingDependencyError {}
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const NIGHTLY_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
const REMOTE_SERVER_CACHE_LIMIT: usize = 5;

/// `network_allowlist.json` is watched asynchronously, so the policy this
/// process enforces only catches up a moment after the consent flow writes to
/// it. Retrying the update before it does would land straight back in
/// [`AutoUpdateStatus::NeedsEgressConsent`], so the retry waits — briefly, and
/// then goes ahead regardless rather than hanging on a watcher that may never
/// fire.
const ALLOWLIST_RELOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ALLOWLIST_RELOAD_ATTEMPTS: usize = 50;

#[cfg(target_os = "linux")]
fn linux_rsync_install_hint() -> &'static str {
    let os_release = match std::fs::read_to_string("/etc/os-release") {
        Ok(os_release) => os_release,
        Err(_) => return "Please install rsync using your package manager",
    };

    let mut distribution_ids = Vec::new();
    for line in os_release.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("ID=") {
            distribution_ids.push(value.trim_matches('"').to_ascii_lowercase());
        } else if let Some(value) = trimmed.strip_prefix("ID_LIKE=") {
            for id in value.trim_matches('"').split_whitespace() {
                distribution_ids.push(id.to_ascii_lowercase());
            }
        }
    }

    let package_manager_hint = if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "arch")
    {
        Some("Install it with: sudo pacman -S rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "debian" || distribution_id == "ubuntu")
    {
        Some("Install it with: sudo apt install rsync")
    } else if distribution_ids.iter().any(|distribution_id| {
        distribution_id == "fedora"
            || distribution_id == "rhel"
            || distribution_id == "centos"
            || distribution_id == "rocky"
            || distribution_id == "almalinux"
    }) {
        Some("Install it with: sudo dnf install rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "nixos")
    {
        Some("Install pkgs.rsync from nixpkgs")
    } else {
        None
    };

    package_manager_hint.unwrap_or("Please install rsync using your package manager")
}

actions!(
    auto_update,
    [
        /// Checks for available updates.
        Check,
        /// Dismisses the update error message.
        DismissMessage,
        /// Opens the release notes for the current version in a browser.
        ViewReleaseNotes,
    ]
);

#[derive(Clone, Debug)]
pub enum AutoUpdateStatus {
    Idle,
    Checking,
    /// A newer release exists, but downloading it would reach hosts the
    /// deny-by-default egress policy does not permit (ADR-F03/F16). The
    /// download is held here until the user explicitly opens them; nothing is
    /// added to `network_allowlist.json` without that confirmation.
    NeedsEgressConsent {
        version: Version,
        hosts: Vec<String>,
    },
    Downloading {
        version: Version,
        /// Download progress as a fraction in the range `0.0..=1.0`, or `None`
        /// when the total download size is not yet known.
        progress: Option<f32>,
    },
    Installing {
        version: Version,
    },
    Updated {
        version: Version,
    },
    Errored {
        error: Arc<anyhow::Error>,
    },
}

impl PartialEq for AutoUpdateStatus {
    // `progress` is deliberately not compared: two `Downloading` statuses for
    // the same version are equal regardless of how far the download is.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AutoUpdateStatus::Idle, AutoUpdateStatus::Idle) => true,
            (AutoUpdateStatus::Checking, AutoUpdateStatus::Checking) => true,
            (
                AutoUpdateStatus::NeedsEgressConsent {
                    version: v1,
                    hosts: h1,
                },
                AutoUpdateStatus::NeedsEgressConsent {
                    version: v2,
                    hosts: h2,
                },
            ) => v1 == v2 && h1 == h2,
            (
                AutoUpdateStatus::Downloading { version: v1, .. },
                AutoUpdateStatus::Downloading { version: v2, .. },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Installing { version: v1 },
                AutoUpdateStatus::Installing { version: v2 },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Updated { version: v1 },
                AutoUpdateStatus::Updated { version: v2 },
            ) => v1 == v2,
            (AutoUpdateStatus::Errored { error: e1 }, AutoUpdateStatus::Errored { error: e2 }) => {
                e1.to_string() == e2.to_string()
            }
            _ => false,
        }
    }
}

impl AutoUpdateStatus {
    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

pub struct AutoUpdater {
    status: AutoUpdateStatus,
    current_version: Version,
    client: Arc<Client>,
    pending_poll: Option<Task<Option<()>>>,
    quit_subscription: Option<gpui::Subscription>,
    update_check_type: UpdateCheckType,
    _wake_subscription: gpui::Subscription,
    dismissed_status: Option<AutoUpdateStatus>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
}

struct MacOsUnmounter<'a> {
    mount_path: PathBuf,
    background_executor: &'a BackgroundExecutor,
}

impl MacOsUnmounter<'_> {
    /// Unmounts the disk image and waits for completion. This must happen
    /// before the `InstallerDir` is dropped: deleting the temp dir while the
    /// image is still mounted inside it fails silently and leaks the
    /// directory (and the downloaded DMG) in the system temp dir.
    async fn unmount(mut self) {
        let mount_path = mem::take(&mut self.mount_path);
        unmount_disk_image(&mount_path).await;
    }
}

impl Drop for MacOsUnmounter<'_> {
    fn drop(&mut self) {
        let mount_path = mem::take(&mut self.mount_path);
        // Safety net for early exits and cancellation; the happy path calls
        // `unmount`, which leaves the path empty.
        if mount_path.as_os_str().is_empty() {
            return;
        }
        self.background_executor
            .spawn(async move { unmount_disk_image(&mount_path).await })
            .detach();
    }
}

async fn unmount_disk_image(mount_path: &Path) {
    let unmount_output = new_command("hdiutil")
        .args(["detach", "-force"])
        .arg(mount_path)
        .output()
        .await;
    match unmount_output {
        Ok(output) if output.status.success() => {
            log::info!("Successfully unmounted the disk image");
        }
        Ok(output) => {
            log::error!(
                "Failed to unmount disk image: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(error) => {
            log::error!("Error while trying to unmount disk image: {:?}", error);
        }
    }
}

#[derive(Clone, Copy, Debug, RegisterSetting)]
struct AutoUpdateSetting(bool);

/// Whether or not to automatically check for updates.
///
/// Default: true
impl Settings for AutoUpdateSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update.unwrap())
    }
}

#[derive(Default)]
struct GlobalAutoUpdate(Option<Entity<AutoUpdater>>);

impl Global for GlobalAutoUpdate {}

pub fn init(client: Arc<Client>, cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_, action, window, cx| check(action, window, cx));

        workspace.register_action(|_, action, _, cx| {
            view_release_notes(action, cx);
        });
    })
    .detach();

    let version = release_channel::AppVersion::global(cx);
    let auto_updater = cx.new(|cx| {
        let updater = AutoUpdater::new(version, client, cx);

        let poll_for_updates = ReleaseChannel::try_global(cx)
            .map(|channel| channel.poll_for_updates())
            .unwrap_or(false);

        if option_env!("ZED_UPDATE_EXPLANATION").is_none()
            && env::var("ZED_UPDATE_EXPLANATION").is_err()
            && poll_for_updates
        {
            let mut update_subscription = AutoUpdateSetting::get_global(cx)
                .0
                .then(|| updater.start_polling(cx));

            cx.observe_global::<SettingsStore>(move |updater: &mut AutoUpdater, cx| {
                if AutoUpdateSetting::get_global(cx).0 {
                    if update_subscription.is_none() {
                        update_subscription = Some(updater.start_polling(cx))
                    }
                } else {
                    update_subscription.take();
                }
            })
            .detach();
        }

        updater
    });
    cx.set_global(GlobalAutoUpdate(Some(auto_updater)));
}

pub fn check(_: &Check, window: &mut Window, cx: &mut App) {
    if let Some(message) = option_env!("ZED_UPDATE_EXPLANATION")
        .map(ToOwned::to_owned)
        .or_else(|| env::var("ZED_UPDATE_EXPLANATION").ok())
    {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Zed was installed via a package manager.",
            Some(&message),
            &["OK"],
            cx,
        ));
        return;
    }

    if !ReleaseChannel::try_global(cx)
        .map(|channel| channel.poll_for_updates())
        .unwrap_or(false)
    {
        return;
    }

    if let Some(updater) = AutoUpdater::get(cx) {
        updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));
    } else {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Could not check for updates",
            Some("Auto-updates disabled for non-bundled app."),
            &["OK"],
            cx,
        ));
    }
}

/// Where this fork's release notes live: its own repository's GitHub
/// Releases, the same place [`AutoUpdater`] fetches updates from (ADR-F16).
pub fn release_notes_url(cx: &mut App) -> Option<String> {
    let release_channel = ReleaseChannel::try_global(cx)?;
    let url = match release_channel {
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            let auto_updater = AutoUpdater::get(cx)?;
            let mut current_version = auto_updater.read(cx).current_version.clone();
            current_version.pre = semver::Prerelease::EMPTY;
            current_version.build = semver::BuildMetadata::EMPTY;
            format!("https://github.com/{UPDATE_REPO}/releases/tag/v{current_version}")
        }
        // A nightly or dev build is not published as a tagged release, so
        // there is no single note to link to — the index is the honest answer.
        ReleaseChannel::Nightly | ReleaseChannel::Dev => {
            format!("https://github.com/{UPDATE_REPO}/releases")
        }
    };
    Some(url)
}

pub fn view_release_notes(_: &ViewReleaseNotes, cx: &mut App) -> Option<()> {
    let url = release_notes_url(cx)?;
    cx.open_url(&url);
    None
}

#[cfg(not(target_os = "windows"))]
const INSTALLER_DIR_PREFIX: &str = "zed-auto-update";

#[cfg(not(target_os = "windows"))]
struct InstallerDir(tempfile::TempDir);

#[cfg(not(target_os = "windows"))]
impl InstallerDir {
    async fn new() -> Result<Self> {
        Ok(Self(
            tempfile::Builder::new()
                .prefix(INSTALLER_DIR_PREFIX)
                .tempdir()?,
        ))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(target_os = "windows")]
struct InstallerDir(PathBuf);

#[cfg(target_os = "windows")]
impl InstallerDir {
    async fn new() -> Result<Self> {
        let installer_dir = std::env::current_exe()?
            .parent()
            .context("No parent dir for the running executable")?
            .join("updates");
        if smol::fs::metadata(&installer_dir).await.is_ok() {
            smol::fs::remove_dir_all(&installer_dir).await?;
        }
        smol::fs::create_dir(&installer_dir).await?;
        Ok(Self(installer_dir))
    }

    fn path(&self) -> &Path {
        self.0.as_path()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UpdateCheckType {
    Automatic,
    Manual,
}

impl UpdateCheckType {
    pub fn is_manual(self) -> bool {
        self == Self::Manual
    }
}

impl AutoUpdater {
    pub fn get(cx: &mut App) -> Option<Entity<Self>> {
        cx.default_global::<GlobalAutoUpdate>().0.clone()
    }

    fn new(current_version: Version, client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        // On windows, executable files cannot be overwritten while they are
        // running, so we must wait to overwrite the application until quitting
        // or restarting. When quitting the app, we spawn the auto update helper
        // to finish the auto update process after Zed exits. When restarting
        // the app after an update, we use `set_restart_path` to run the auto
        // update helper instead of the app, so that it can overwrite the app
        // and then spawn the new binary.
        #[cfg(target_os = "windows")]
        let quit_subscription = Some(cx.on_app_quit(|_, _| finalize_auto_update_on_quit()));
        #[cfg(not(target_os = "windows"))]
        let quit_subscription = None;

        cx.on_app_restart(|this, _| {
            this.quit_subscription.take();
        })
        .detach();

        // A download or check that was in flight when the machine went to sleep
        // is almost certainly riding a TCP connection that silently died during
        // suspend, so it would otherwise appear to stall indefinitely.
        let wake_subscription = cx.on_system_wake({
            let this = cx.entity().downgrade();
            move |cx| {
                this.update(cx, |this, cx| this.restart_after_wake(cx)).ok();
            }
        });

        Self {
            status: AutoUpdateStatus::Idle,
            current_version,
            client,
            pending_poll: None,
            quit_subscription,
            update_check_type: UpdateCheckType::Automatic,
            _wake_subscription: wake_subscription,
            dismissed_status: None,
        }
    }

    fn restart_after_wake(&mut self, cx: &mut Context<Self>) {
        // Only network phases can be safely restarted. `Installing` is a local
        // operation (mounting a dmg, rsync, etc.) that must not be interrupted.
        if !matches!(
            self.status,
            AutoUpdateStatus::Checking | AutoUpdateStatus::Downloading { .. }
        ) {
            return;
        }

        let check_type = self.update_check_type;
        self.pending_poll.take();
        self.status = AutoUpdateStatus::Idle;
        self.poll(check_type, cx);
    }

    pub fn start_polling(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let poll_interval =
            ReleaseChannel::try_global(cx).map_or(POLL_INTERVAL, |channel| match channel {
                ReleaseChannel::Nightly => NIGHTLY_POLL_INTERVAL,
                _ => POLL_INTERVAL,
            });

        cx.spawn(async move |this, cx| {
            if cfg!(target_os = "windows") {
                use util::ResultExt;

                cleanup_windows()
                    .await
                    .context("failed to cleanup old directories")
                    .log_err();
            }

            #[cfg(all(not(target_os = "windows"), not(test)))]
            cx.background_spawn(cleanup_stale_installer_dirs()).detach();

            loop {
                this.update(cx, |this, cx| this.poll(UpdateCheckType::Automatic, cx))?;
                cx.background_executor().timer(poll_interval).await;
            }
        })
    }

    pub fn update_check_type(&self) -> UpdateCheckType {
        self.update_check_type
    }

    pub fn poll(&mut self, check_type: UpdateCheckType, cx: &mut Context<Self>) {
        if check_type.is_manual() {
            self.dismissed_status = None;
        }
        if self.pending_poll.is_some() {
            if self.update_check_type == UpdateCheckType::Automatic {
                self.update_check_type = check_type;
                cx.notify();
            }
            return;
        }
        self.update_check_type = check_type;

        cx.notify();

        self.pending_poll = Some(cx.spawn(async move |this, cx| {
            let result = Self::update(this.upgrade()?, cx).await;
            this.update(cx, |this, cx| {
                this.pending_poll = None;
                if let Err(error) = result {
                    let is_missing_dependency =
                        error.downcast_ref::<MissingDependencyError>().is_some();
                    this.status = match check_type {
                        UpdateCheckType::Automatic if is_missing_dependency => {
                            log::warn!("auto-update: {}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                        // Be quiet if the check was automated (e.g. when offline)
                        UpdateCheckType::Automatic => {
                            log::info!("auto-update check failed: error:{:?}", error);
                            AutoUpdateStatus::Idle
                        }
                        UpdateCheckType::Manual => {
                            log::error!("auto-update failed: error:{:?}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                    };

                    cx.notify();
                }
            })
            .ok()
        }));
    }

    pub fn current_version(&self) -> Version {
        self.current_version.clone()
    }

    pub fn status(&self) -> AutoUpdateStatus {
        self.status.clone()
    }

    pub fn dismissed_status(&self) -> Option<AutoUpdateStatus> {
        self.dismissed_status.clone()
    }

    pub fn dismiss_status(&mut self, status: AutoUpdateStatus, cx: &mut Context<Self>) {
        self.dismissed_status = Some(status);
        cx.notify();
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if let AutoUpdateStatus::Idle = self.status {
            return false;
        }
        self.status = AutoUpdateStatus::Idle;
        cx.notify();
        true
    }

    // If you are packaging Zed and need to override the place it downloads SSH remotes from,
    // you can override this function. You should also update get_remote_server_release_url to return
    // Ok(None).
    pub async fn download_remote_server_release(
        release_channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        set_status: impl Fn(&str, &mut AsyncApp) + Send + 'static,
        cx: &mut AsyncApp,
    ) -> Result<PathBuf> {
        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        set_status(i18n::t!("Fetching remote server release"), cx);
        let release = Self::get_release_asset(
            &this,
            release_channel,
            version,
            "zed-remote-server",
            os,
            arch,
            cx,
        )
        .await?;

        let servers_dir = paths::remote_servers_dir();
        let channel_dir = servers_dir.join(release_channel.dev_name());
        let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
        let version_path = platform_dir.join(format!("{}.gz", release.version));
        smol::fs::create_dir_all(&platform_dir).await.ok();

        let client = this.read_with(cx, |this, _| ide_lane(this.client.http_client()));

        if smol::fs::metadata(&version_path).await.is_err() {
            log::info!(
                "downloading zed-remote-server {os} {arch} version {}",
                release.version
            );
            set_status(i18n::t!("Downloading remote server"), cx);
            download_remote_server_binary(&version_path, release, client).await?;
        }

        if let Err(error) =
            cleanup_remote_server_cache(&platform_dir, &version_path, REMOTE_SERVER_CACHE_LIMIT)
                .await
        {
            log::warn!(
                "Failed to clean up remote server cache in {:?}: {error:#}",
                platform_dir
            );
        }

        Ok(version_path)
    }

    pub async fn get_remote_server_release_url(
        channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<Option<String>> {
        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        let release =
            Self::get_release_asset(&this, channel, version, "zed-remote-server", os, arch, cx)
                .await?;

        Ok(Some(release.url))
    }

    /// Resolves one published asset on [`UPDATE_REPO`]'s GitHub Releases.
    ///
    /// This fork updates itself from its own repository only (ADR-F16), so
    /// the upstream Zed Cloud endpoint — which picked the file server-side
    /// from an `asset`/`os`/`arch` query and carried telemetry identifiers
    /// while doing it — is gone, and the naming convention it encoded now
    /// lives in [`asset_name`].
    async fn get_release_asset(
        this: &Entity<Self>,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        asset: &str,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<ReleaseAsset> {
        let http_client = this.read_with(cx, |this, _| ide_lane(this.client.http_client()));
        let asset_name = asset_name(asset, os, arch)?;

        let release = match version {
            Some(mut version) => {
                version.pre = semver::Prerelease::EMPTY;
                version.build = semver::BuildMetadata::EMPTY;
                get_release_by_tag_name(UPDATE_REPO, &format!("v{version}"), http_client).await
            }
            // The API returns releases newest first, so this is the most
            // recent one that actually carries assets.
            None => {
                latest_github_release(
                    UPDATE_REPO,
                    true,
                    !matches!(release_channel, ReleaseChannel::Stable),
                    http_client,
                )
                .await
            }
        }
        .with_context(|| format!("fetching the {UPDATE_REPO} release carrying {asset_name}"))?;

        asset_from_release(release, &asset_name)
    }

    /// Adds the hosts the held-back download needs to `network_allowlist.json`
    /// and checks again.
    ///
    /// Only reached from a UI path that has already asked the user
    /// (ADR-F16): the allowlist is never widened without an explicit
    /// confirmation, so this must not be called speculatively.
    pub fn allow_download_hosts_and_retry(&mut self, cx: &mut Context<Self>) {
        let AutoUpdateStatus::NeedsEgressConsent { hosts, .. } = &self.status else {
            return;
        };
        let hosts = hosts.clone();
        let fs = <dyn Fs>::global(cx);
        let path = paths::network_allowlist_file().clone();
        let policy = self.client.http_client().egress_policy().cloned();

        self.update_check_type = UpdateCheckType::Manual;
        self.status = AutoUpdateStatus::Checking;
        cx.notify();

        cx.spawn(async move |this, cx| {
            if let Err(error) = egress_policy::add_allowed_hosts(&fs, &path, &hosts).await {
                log::error!("auto-update: failed to add {hosts:?} to the allowlist: {error:#}");
                // `Errored`'s tooltip renders `to_string()`, which drops the
                // cause chain, so the reason is folded into the message.
                this.update(cx, |this, cx| {
                    this.status = AutoUpdateStatus::Errored {
                        error: Arc::new(anyhow::anyhow!(i18n::t!(
                            "Couldn't add the hosts to the allowlist: {error:#}"
                        ))),
                    };
                    cx.notify();
                })
                .ok();
                return;
            }

            if let Some(policy) = policy {
                for _ in 0..ALLOWLIST_RELOAD_ATTEMPTS {
                    let allowed = hosts.iter().all(|host| {
                        RequestHost::parse(host).is_some_and(|host| policy.allows(&host))
                    });
                    if allowed {
                        break;
                    }
                    cx.background_executor()
                        .timer(ALLOWLIST_RELOAD_POLL_INTERVAL)
                        .await;
                }
            }

            this.update(cx, |this, cx| this.poll(UpdateCheckType::Manual, cx))
                .ok();
        })
        .detach();
    }

    async fn update(this: Entity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let (client, installed_version, previous_status, release_channel) =
            this.read_with(cx, |this, cx| {
                (
                    // ADR-F38: updating the editor is IDE infrastructure.
                    ide_lane(this.client.http_client()),
                    this.current_version.clone(),
                    this.status.clone(),
                    ReleaseChannel::try_global(cx).unwrap_or(ReleaseChannel::Stable),
                )
            });

        Self::check_dependencies()?;

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Checking;
            log::info!("Auto Update: checking for updates");
            cx.notify();
        });

        let fetched_release_data =
            Self::get_release_asset(&this, release_channel, None, "zed", OS, ARCH, cx).await?;
        let fetched_version = fetched_release_data.clone().version;
        let app_commit_sha = Ok(cx.update(|cx| AppCommitSha::try_global(cx).map(|sha| sha.full())));
        let newer_version = Self::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            previous_status.clone(),
        )?;

        let Some(newer_version) = newer_version else {
            this.update(cx, |this, cx| {
                let status = match previous_status {
                    AutoUpdateStatus::Updated { .. } => previous_status,
                    _ => AutoUpdateStatus::Idle,
                };
                this.status = status;
                cx.notify();
            });
            return Ok(());
        };

        // ADR-F16 used to hold the download here until the user put GitHub's
        // asset hosts on the allowlist. ADR-F38 narrowed the allowlist to
        // AI-originated traffic, so updating the editor is no longer something
        // the user has to permit; the consent path below survives only for a
        // denial this lane is not supposed to produce.
        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Downloading {
                version: newer_version.clone(),
                progress: None,
            };
            cx.notify();
        });

        let installer_dir = InstallerDir::new()
            .await
            .context("Failed to create installer dir")?;
        let target_path = Self::target_path(&installer_dir).await?;
        let progress_entity = this.clone();
        let mut progress_cx = cx.clone();
        let download_result = download_release(
            &target_path,
            fetched_release_data,
            client,
            move |progress| {
                progress_entity.update(&mut progress_cx, |this, cx| {
                    if let AutoUpdateStatus::Downloading {
                        progress: current_progress,
                        ..
                    } = &mut this.status
                    {
                        *current_progress = progress;
                        cx.notify();
                    }
                });
            },
        )
        .await;

        // A redirect hop `hosts_required_to_download` could not predict. The
        // policy names the host it refused, so ask for that one instead of
        // reporting a dead end.
        if let Err(error) = &download_result
            && let Some(denied) = error
                .chain()
                .find_map(|error| error.downcast_ref::<EgressDenied>())
        {
            let host = denied.host.clone();
            this.update(cx, |this, cx| {
                this.status = AutoUpdateStatus::NeedsEgressConsent {
                    version: newer_version,
                    hosts: vec![host],
                };
                cx.notify();
            });
            return Ok(());
        }
        download_result
            .with_context(|| format!("Failed to download update to {}", target_path.display()))?;

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Installing {
                version: newer_version.clone(),
            };
            cx.notify();
        });

        #[cfg(test)]
        let install_result = match cx
            .try_read_global::<tests::InstallOverride, _>(|g, _| g.0.clone())
            .map(|test_install| test_install(&target_path, cx))
        {
            Some(result) => result,
            None => return Ok(()),
        };

        #[cfg(not(test))]
        let install_result = {
            let running_app_path = cx.update(|cx| cx.app_path())?;
            let background_executor = cx.background_executor().clone();
            let channel = cx.update(|cx| ReleaseChannel::global(cx).dev_name());
            cx.background_spawn(Self::install_release(
                installer_dir,
                target_path.clone(),
                running_app_path,
                channel,
                background_executor,
            ))
            .await
        };
        let new_binary_path = install_result
            .with_context(|| format!("Failed to install update at: {}", target_path.display()))?;
        if let Some(new_binary_path) = new_binary_path {
            cx.update(|cx| cx.set_restart_path(new_binary_path));
        }

        this.update(cx, |this, cx| {
            this.set_should_show_update_notification(true, cx)
                .detach_and_log_err(cx);
            this.status = AutoUpdateStatus::Updated {
                version: newer_version,
            };
            cx.notify();
        });
        Ok(())
    }

    fn check_if_fetched_version_is_newer(
        release_channel: ReleaseChannel,
        app_commit_sha: Result<Option<String>>,
        installed_version: Version,
        fetched_version: String,
        status: AutoUpdateStatus,
    ) -> Result<Option<Version>> {
        let fetched_version = fetched_version.parse::<Version>()?;

        match release_channel {
            ReleaseChannel::Nightly => {
                let should_download = if let AutoUpdateStatus::Updated { version } = status {
                    fetched_version != version
                } else {
                    let fetched_sha = fetched_version.build.as_str().rsplit('.').next();
                    app_commit_sha
                        .ok()
                        .flatten()
                        .is_none_or(|sha| fetched_sha != Some(sha.as_str()))
                };
                Ok(should_download.then_some(fetched_version))
            }
            _ => {
                let current_version = if let AutoUpdateStatus::Updated { version } = status {
                    version
                } else {
                    installed_version
                };
                Ok(Self::check_if_fetched_version_is_newer_non_nightly(
                    current_version,
                    fetched_version,
                ))
            }
        }
    }

    fn check_dependencies() -> Result<()> {
        #[cfg(target_os = "linux")]
        if which::which("rsync").is_err() {
            let install_hint = linux_rsync_install_hint();
            return Err(MissingDependencyError(format!(
                "rsync is required for auto-updates but is not installed. {install_hint}"
            ))
            .into());
        }

        #[cfg(target_os = "macos")]
        anyhow::ensure!(
            which::which("rsync").is_ok(),
            "Could not auto-update because the required rsync utility was not found."
        );

        Ok(())
    }

    async fn target_path(installer_dir: &InstallerDir) -> Result<PathBuf> {
        let filename = match OS {
            "macos" => anyhow::Ok("Zed.dmg"),
            "linux" => Ok("zed.tar.gz"),
            "windows" => Ok(WINDOWS_INSTALLER_FILE_NAME),
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }?;

        Ok(installer_dir.path().join(filename))
    }

    #[cfg_attr(test, allow(dead_code))]
    async fn install_release(
        installer_dir: InstallerDir,
        target_path: PathBuf,
        running_app_path: PathBuf,
        channel: &str,
        background_executor: BackgroundExecutor,
    ) -> Result<Option<PathBuf>> {
        match OS {
            "macos" => {
                install_release_macos(
                    &installer_dir,
                    &target_path,
                    running_app_path,
                    &background_executor,
                )
                .await
            }
            "linux" => {
                install_release_linux(&installer_dir, &target_path, channel, running_app_path).await
            }
            "windows" => install_release_windows(&target_path).await,
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }
    }

    fn check_if_fetched_version_is_newer_non_nightly(
        mut installed_version: Version,
        fetched_version: Version,
    ) -> Option<Version> {
        // For non-nightly releases, ignore build and pre-release fields as they're not provided by our endpoints right now.
        installed_version.pre = semver::Prerelease::EMPTY;
        installed_version.build = semver::BuildMetadata::EMPTY;
        (fetched_version > installed_version).then_some(fetched_version)
    }

    pub fn set_should_show_update_notification(
        &self,
        should_show: bool,
        cx: &App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            if should_show {
                kvp.write_kvp(
                    SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string(),
                    "".to_string(),
                )
                .await?;
            } else {
                kvp.delete_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string())
                    .await?;
            }
            Ok(())
        })
    }

    pub fn should_show_update_notification(&self, cx: &App) -> Task<Result<bool>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            Ok(kvp.read_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY)?.is_some())
        })
    }
}

/// What the downloaded Windows installer is saved as in the `updates` directory.
const WINDOWS_INSTALLER_FILE_NAME: &str =
    branding::concat_const!(branding::CURRENT.executable_stem, "-setup.exe");

/// The file name this fork publishes an asset under on its GitHub Releases.
///
/// The upstream endpoint this replaced chose the file server-side, so the
/// convention has to be spelled out on the client now. `zed-remote-server`
/// keeps the name upstream's own bundle scripts produce, which is also the
/// one ADR-F15 looks for next to the executable, so a single file in a
/// release serves both the bundled and the downloaded path.
fn asset_name(asset: &str, os: &str, arch: &str) -> Result<String> {
    match asset {
        "zed-remote-server" => Ok(format!("zed-remote-server-{os}-{arch}.gz")),
        "zed" => match os {
            // The installer's OutputBaseFilename ($appSetupName in
            // script/bundle-windows.ps1).
            "windows" => Ok(format!(
                "{}-{os}-{arch}-setup.exe",
                branding::CURRENT.cli_name
            )),
            "macos" => Ok(format!("{}-{os}-{arch}.dmg", branding::CURRENT.cli_name)),
            "linux" => Ok(format!("{}-{os}-{arch}.tar.gz", branding::CURRENT.cli_name)),
            unsupported => anyhow::bail!("no update asset is published for {unsupported}"),
        },
        other => anyhow::bail!("unknown release asset {other:?}"),
    }
}

/// Maps a GitHub release onto the shape the updater consumes: the tag with a
/// leading `v` stripped as the version, and the named asset's download URL.
fn asset_from_release(release: GithubRelease, asset_name: &str) -> Result<ReleaseAsset> {
    let GithubRelease {
        tag_name, assets, ..
    } = release;
    let version = tag_name
        .strip_prefix('v')
        .unwrap_or(tag_name.as_str())
        .to_string();
    let asset = assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .with_context(|| format!("release {tag_name} has no asset named {asset_name}"))?;

    Ok(ReleaseAsset {
        version,
        url: asset.browser_download_url,
    })
}

async fn download_remote_server_binary(
    target_path: &PathBuf,
    release: ReleaseAsset,
    client: Arc<dyn HttpClient>,
) -> Result<()> {
    let temp = tempfile::Builder::new().tempfile_in(remote_servers_dir())?;
    let mut temp_file = File::create(&temp).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download remote server release: {:?}",
        response.status()
    );
    smol::io::copy(response.body_mut(), &mut temp_file).await?;
    smol::fs::rename(&temp, &target_path).await?;

    Ok(())
}

async fn cleanup_remote_server_cache(
    platform_dir: &Path,
    keep_path: &Path,
    limit: usize,
) -> Result<()> {
    if limit == 0 {
        return Ok(());
    }

    let mut entries = smol::fs::read_dir(platform_dir).await?;
    let now = SystemTime::now();
    let mut candidates = Vec::new();

    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("gz")) {
            continue;
        }

        let mtime = if path == keep_path {
            now
        } else {
            smol::fs::metadata(&path)
                .await
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        };

        candidates.push((path, mtime));
    }

    if candidates.len() <= limit {
        return Ok(());
    }

    candidates.sort_by(|(path_a, time_a), (path_b, time_b)| {
        time_b.cmp(time_a).then_with(|| path_a.cmp(path_b))
    });

    for (index, (path, _)) in candidates.into_iter().enumerate() {
        if index < limit || path == keep_path {
            continue;
        }

        if let Err(error) = smol::fs::remove_file(&path).await {
            log::warn!(
                "Failed to remove old remote server archive {:?}: {}",
                path,
                error
            );
        }
    }

    Ok(())
}

async fn download_release(
    target_path: &Path,
    release: ReleaseAsset,
    client: Arc<dyn HttpClient>,
    mut on_progress: impl FnMut(Option<f32>),
) -> Result<()> {
    let mut target_file = File::create(&target_path).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download update: {:?}",
        response.status()
    );

    let total_bytes = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|total_bytes| *total_bytes > 0);

    let mut downloaded_bytes: u64 = 0;
    let mut last_reported_percent: Option<u8> = None;
    let mut buffer = [0u8; 8192];
    let body = response.body_mut();
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        target_file.write_all(&buffer[..bytes_read]).await?;
        downloaded_bytes += bytes_read as u64;

        if let Some(total_bytes) = total_bytes {
            let fraction = (downloaded_bytes as f32 / total_bytes as f32).clamp(0.0, 1.0);
            // Only report when the whole-number percentage changes to avoid notifying the UI on every chunk.
            let percent = (fraction * 100.0) as u8;
            if last_reported_percent != Some(percent) {
                last_reported_percent = Some(percent);
                on_progress(Some(fraction));
            }
        }
    }
    target_file.flush().await?;
    if total_bytes.is_some() && last_reported_percent != Some(100) {
        on_progress(Some(1.0));
    }
    log::info!("downloaded update. path:{:?}", target_path);

    Ok(())
}

/// The editor binary inside a Linux install, next to which `bin/` holds the
/// launcher.
const LINUX_EDITOR_NAME: &str = branding::concat_const!(branding::CURRENT.cli_name, "-editor");

/// The folder a Linux release unpacks to and is installed as under
/// `~/.local`. It is this fork's own, because the update replaces it with
/// `rsync --delete` and an official Zed installs to `~/.local/zed.app`.
fn linux_app_folder_name(channel: &str) -> String {
    let suffix = if channel != "stable" {
        format!("-{}", channel)
    } else {
        String::default()
    };
    format!("{}{suffix}.app", branding::CURRENT.cli_name)
}

/// The volume name `script/bundle-mac` gives the disk image, which
/// `hdiutil attach -mountroot` mounts the update under.
const MACOS_DMG_VOLUME_NAME: &str = branding::CURRENT.executable_stem;

async fn install_release_linux(
    temp_dir: &InstallerDir,
    downloaded_tar_gz: &Path,
    channel: &str,
    running_app_path: PathBuf,
) -> Result<Option<PathBuf>> {
    let home_dir = PathBuf::from(env::var("HOME").context("no HOME env var set")?);

    let extracted = temp_dir.path().join("zed");
    fs::create_dir_all(&extracted)
        .await
        .context("failed to create directory into which to extract update")?;

    let mut cmd = new_command("tar");
    cmd.arg("-xzf")
        .arg(&downloaded_tar_gz)
        .arg("-C")
        .arg(&extracted);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to extract: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to extract {:?} to {:?}: {:?}",
        downloaded_tar_gz,
        extracted,
        String::from_utf8_lossy(&output.stderr)
    );

    let app_folder_name = linux_app_folder_name(channel);

    let from = extracted.join(&app_folder_name);
    let mut to = home_dir.join(".local");

    let expected_suffix = format!("{app_folder_name}/libexec/{LINUX_EDITOR_NAME}");

    if let Some(prefix) = running_app_path
        .to_str()
        .and_then(|str| str.strip_suffix(&expected_suffix))
    {
        to = PathBuf::from(prefix);
    }

    let mut cmd = new_command("rsync");
    cmd.args(["-av", "--delete"]).arg(&from).arg(&to);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to rsync: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to copy Zed update from {:?} to {:?}: {:?}",
        from,
        to,
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(Some(to.join(expected_suffix)))
}

async fn install_release_macos(
    temp_dir: &InstallerDir,
    downloaded_dmg: &Path,
    running_app_path: PathBuf,
    background_executor: &BackgroundExecutor,
) -> Result<Option<PathBuf>> {
    let running_app_filename = running_app_path
        .file_name()
        .with_context(|| format!("invalid running app path {running_app_path:?}"))?;

    let mount_path = temp_dir.path().join(MACOS_DMG_VOLUME_NAME);
    let mut mounted_app_path: OsString = mount_path.join(running_app_filename).into();

    mounted_app_path.push("/");
    let mut cmd = new_command("hdiutil");
    cmd.args(["attach", "-nobrowse"])
        .arg(&downloaded_dmg)
        .arg("-mountroot")
        .arg(temp_dir.path());
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to mount: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to mount: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let unmounter = MacOsUnmounter {
        mount_path: mount_path.clone(),
        background_executor,
    };

    let mut cmd = new_command("rsync");
    cmd.args(["-av", "--delete", "--exclude", "Icon?"])
        .arg(&mounted_app_path)
        .arg(&running_app_path);
    let rsync_output = cmd.output().await;

    // Await the unmount (even if rsync failed) so that the installer temp dir
    // can be deleted once this function returns.
    unmounter.unmount().await;

    let output = rsync_output.with_context(|| "failed to rsync: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to copy app: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(None)
}

/// Removes stale installer dirs from the system temp dir. Older Zed versions
/// leaked one per update by deleting the dir while the downloaded disk image
/// was still mounted inside it, which made the deletion fail silently.
#[cfg(any(rust_analyzer, all(not(target_os = "windows"), not(test))))]
async fn cleanup_stale_installer_dirs() {
    const STALE_INSTALLER_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);

    let temp_dir = std::env::temp_dir();
    let Ok(mut entries) = fs::read_dir(&temp_dir).await else {
        log::warn!("failed to read temp dir {temp_dir:?} while cleaning up installer dirs");
        return;
    };
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(INSTALLER_DIR_PREFIX)
        {
            continue;
        }
        // Leave recent dirs alone, as they may belong to an update currently
        // in progress in another Zed instance.
        let is_stale = entry.metadata().await.ok().is_some_and(|metadata| {
            metadata.is_dir()
                && metadata.modified().ok().is_some_and(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .is_ok_and(|age| age > STALE_INSTALLER_DIR_AGE)
                })
        });
        if is_stale {
            if let Err(error) = fs::remove_dir_all(entry.path()).await {
                log::warn!(
                    "failed to remove stale installer dir {:?}: {error}",
                    entry.path()
                );
            } else {
                log::info!("removed stale installer dir {:?}", entry.path());
            }
        }
    }
}

async fn cleanup_windows() -> Result<()> {
    let parent = std::env::current_exe()?
        .parent()
        .context("No parent dir for the running executable")?
        .to_owned();

    // keep in sync with crates/auto_update_helper/src/updater.rs
    _ = smol::fs::remove_dir(parent.join("updates")).await;
    _ = smol::fs::remove_dir(parent.join("install")).await;
    _ = smol::fs::remove_dir(parent.join("old")).await;

    Ok(())
}

async fn install_release_windows(downloaded_installer: &Path) -> Result<Option<PathBuf>> {
    let mut cmd = new_command(downloaded_installer);
    cmd.arg("/verysilent")
        .arg("/update=true")
        .arg("/MERGETASKS=!desktopicon");
    let output = cmd.output().await?;
    anyhow::ensure!(
        output.status.success(),
        "failed to start installer: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    // We return the path to the update helper program, because it will
    // perform the final steps of the update process, copying the new binary,
    // deleting the old one, and launching the new binary.
    let helper_path = std::env::current_exe()?
        .parent()
        .context("No parent dir for the running executable")?
        .join("tools")
        .join("auto_update_helper.exe");
    Ok(Some(helper_path))
}

pub async fn finalize_auto_update_on_quit() {
    let Some(installer_path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("updates")))
    else {
        return;
    };

    // The installer will create a flag file after it finishes updating
    let flag_file = installer_path.join("versions.txt");
    if flag_file.exists()
        && let Some(helper) = installer_path
            .parent()
            .map(|p| p.join("tools").join("auto_update_helper.exe"))
    {
        let mut command = util::command::new_command(helper);
        command.arg("--launch");
        command.arg("false");
        if let Ok(mut cmd) = command.spawn() {
            _ = cmd.status().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use client::Client;
    use clock::FakeSystemClock;
    use futures::channel::oneshot;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use http_client::{FakeHttpClient, Response};
    use settings::default_settings;
    use std::{
        rc::Rc,
        sync::{
            Arc,
            atomic::{self, AtomicBool},
        },
    };
    use tempfile::tempdir;

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    use super::*;

    pub(super) struct InstallOverride(pub Rc<dyn Fn(&Path, &AsyncApp) -> Result<Option<PathBuf>>>);
    impl Global for InstallOverride {}

    /// One GitHub release, shaped the way `GET /repos/{repo}/releases`
    /// answers, carrying the asset this platform's updater looks for.
    fn github_releases_body(tag: &str, download_url: &str) -> String {
        let asset = asset_name("zed", OS, ARCH).expect("the test host should be supported");
        format!(
            r#"[{{
                "tag_name": "{tag}",
                "prerelease": false,
                "tarball_url": "https://api.github.com/tarball/{tag}",
                "zipball_url": "https://api.github.com/zipball/{tag}",
                "assets": [
                    {{
                        "name": "zed-remote-server-linux-x86_64.gz",
                        "browser_download_url": "https://test.example/remote-server"
                    }},
                    {{ "name": "{asset}", "browser_download_url": "{download_url}" }}
                ]
            }}]"#
        )
    }

    #[test]
    fn asset_names_follow_the_published_naming_convention() {
        assert_eq!(
            asset_name("zed", "windows", "x86_64").unwrap(),
            "forgekairos-windows-x86_64-setup.exe"
        );
        assert_eq!(
            asset_name("zed", "macos", "aarch64").unwrap(),
            "forgekairos-macos-aarch64.dmg"
        );
        assert_eq!(
            asset_name("zed", "linux", "x86_64").unwrap(),
            "forgekairos-linux-x86_64.tar.gz"
        );
        // ADR-F15 looks for this exact name next to the executable, so the
        // bundled and downloaded paths can share one file.
        assert_eq!(
            asset_name("zed-remote-server", "linux", "aarch64").unwrap(),
            "zed-remote-server-linux-aarch64.gz"
        );
        assert!(asset_name("zed", "freebsd", "x86_64").is_err());
        assert!(asset_name("something-else", "linux", "x86_64").is_err());
    }

    #[test]
    fn a_linux_update_replaces_only_this_fork_s_install() {
        assert_eq!(linux_app_folder_name("stable"), "forgekairos.app");
        assert_eq!(linux_app_folder_name("preview"), "forgekairos-preview.app");
        assert_eq!(LINUX_EDITOR_NAME, "forgekairos-editor");
        for channel in ["stable", "preview", "nightly", "dev"] {
            // Where an official Zed installs; rsync --delete would replace it.
            assert!(
                !linux_app_folder_name(channel).starts_with("zed"),
                "{channel}"
            );
        }
    }

    fn repository_script(relative: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative);
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn the_linux_update_unpacks_what_the_linux_bundle_packs() {
        let script = repository_script("script/bundle-linux");
        let folder = linux_app_folder_name("stable");
        let folder_pattern = format!(
            "{}$suffix.app",
            folder.strip_suffix(".app").expect("an .app folder")
        );
        assert!(
            script.contains(&format!("zed_dir=\"${{temp_dir}}/{folder_pattern}\"")),
            "bundle-linux packs its files under {folder_pattern}"
        );
        assert!(
            script.contains(&format!("-C ${{temp_dir}} \"{folder_pattern}\"")),
            "bundle-linux archives {folder_pattern}"
        );
        assert!(
            script.contains("suffix=\"-$channel\""),
            "bundle-linux suffixes the folder with the channel, as linux_app_folder_name does"
        );
        let archive = asset_name("zed", "linux", "${arch}").unwrap();
        assert!(
            script.contains(&format!("archive=\"{archive}\"")),
            "bundle-linux names the archive {archive}, the published asset name"
        );
        assert!(
            script.contains(&format!("\"${{zed_dir}}/libexec/{LINUX_EDITOR_NAME}\"")),
            "bundle-linux installs the editor as libexec/{LINUX_EDITOR_NAME}"
        );
    }

    #[test]
    fn the_macos_update_mounts_the_volume_the_disk_image_is_made_with() {
        let script = repository_script("script/bundle-mac");
        assert!(
            script.contains(&format!("hdiutil create -volname {MACOS_DMG_VOLUME_NAME} ")),
            "bundle-mac names the disk image's volume {MACOS_DMG_VOLUME_NAME}"
        );
    }

    #[test]
    fn the_windows_asset_is_what_the_installer_build_names_its_output() {
        let script_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../script/bundle-windows.ps1");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let setup_names: Vec<&str> = script
            .lines()
            .filter_map(|line| line.trim().strip_prefix("$appSetupName = "))
            .map(|value| value.trim_matches('"'))
            .collect();
        assert_eq!(setup_names.len(), 4, "one setup name per release channel");
        for arch in ["x86_64", "aarch64"] {
            for setup_name in &setup_names {
                assert_eq!(
                    format!("{}.exe", setup_name.replace("$Architecture", arch)),
                    asset_name("zed", "windows", arch).unwrap(),
                );
            }
        }
    }

    fn parse_fixture(tag: &str, download_url: &str) -> GithubRelease {
        let releases: Vec<GithubRelease> =
            serde_json::from_str(&github_releases_body(tag, download_url))
                .expect("the fixture should parse as the GitHub API shape");
        releases.into_iter().next().expect("one release")
    }

    #[test]
    fn a_release_maps_onto_its_tag_and_matching_asset() {
        let release = parse_fixture("v1.2.3", "https://test.example/zed");

        let asset_name = asset_name("zed", OS, ARCH).unwrap();
        let mapped = asset_from_release(release, &asset_name).unwrap();
        assert_eq!(mapped.version, "1.2.3", "the `v` prefix is not a version");
        assert_eq!(mapped.url, "https://test.example/zed");
    }

    #[test]
    fn a_release_without_this_platform_s_asset_is_an_error() {
        let release = parse_fixture("v1.2.3", "https://test.example/zed");

        let error = asset_from_release(release, "zed-plan9-x86_64.tar.gz")
            .expect_err("a missing asset must not be silently skipped");
        assert!(error.to_string().contains("zed-plan9-x86_64.tar.gz"));
    }

    /// This fork never updates itself (ADR-F01), so `auto_update` ships as
    /// `false` — see DESIGN.md §4.
    #[gpui::test]
    fn test_auto_update_defaults_to_false(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = SettingsStore::new(cx, &settings::default_settings());
            store
                .set_default_settings(&default_settings(), cx)
                .expect("Unable to set default settings");
            store
                .set_user_settings("{}", cx)
                .expect("Unable to set user settings");
            cx.set_global(store);
            assert!(!AutoUpdateSetting::get_global(cx).0);
        });
    }

    #[gpui::test]
    async fn test_auto_update_downloads(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        zlog::init_test();
        let release_available = Arc::new(AtomicBool::new(false));

        let (dmg_tx, dmg_rx) = oneshot::channel::<String>();

        cx.update(|cx| {
            settings::init(cx);
            // `auto_update` ships as false in this fork (DESIGN.md §4), and
            // `init` below only starts polling when it is on.
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(r#"{ "auto_update": true }"#, cx)
                    .expect("auto_update must be enabled for this test");
            });

            let current_version = semver::Version::new(0, 100, 0);
            release_channel::init_test(current_version, ReleaseChannel::Stable, cx);

            let clock = Arc::new(FakeSystemClock::new());
            let release_available = Arc::clone(&release_available);
            let dmg_rx = Arc::new(parking_lot::Mutex::new(Some(dmg_rx)));
            let fake_client_http = FakeHttpClient::create(move |req| {
                let release_available = release_available.load(atomic::Ordering::Relaxed);
                let dmg_rx = dmg_rx.clone();
                async move {
                    if req.uri().path() == format!("/repos/{UPDATE_REPO}/releases") {
                        let (tag, download) = if release_available {
                            ("v0.100.1", "https://test.example/new-download")
                        } else {
                            ("v0.100.0", "https://test.example/old-download")
                        };
                        return Ok(Response::builder()
                            .status(200)
                            .body(github_releases_body(tag, download).into())
                            .unwrap());
                    } else if req.uri().path() == "/new-download" {
                        return Ok(Response::builder()
                            .status(200)
                            .body({
                                let dmg_rx = dmg_rx.lock().take().unwrap();
                                dmg_rx.await.unwrap().into()
                            })
                            .unwrap());
                    }
                    Ok(Response::builder().status(404).body("".into()).unwrap())
                }
            });
            let client = Client::new(clock, fake_client_http, cx);
            crate::init(client, cx);
        });

        let auto_updater = cx.update(|cx| AutoUpdater::get(cx).expect("auto updater should exist"));

        cx.background_executor.run_until_parked();

        auto_updater.read_with(cx, |updater, _| {
            assert_eq!(updater.status(), AutoUpdateStatus::Idle);
            assert_eq!(updater.current_version(), semver::Version::new(0, 100, 0));
        });

        release_available.store(true, atomic::Ordering::SeqCst);
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.background_executor.run_until_parked();

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if !matches!(status, AutoUpdateStatus::Idle) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Downloading {
                version: semver::Version::new(0, 100, 1),
                progress: None,
            }
        );

        dmg_tx.send("<fake-zed-update>".to_owned()).unwrap();

        let tmp_dir = Arc::new(tempdir().unwrap());

        cx.update(|cx| {
            let tmp_dir = tmp_dir.clone();
            cx.set_global(InstallOverride(Rc::new(move |target_path, _cx| {
                let tmp_dir = tmp_dir.clone();
                let dest_path = tmp_dir.path().join("zed");
                std::fs::copy(&target_path, &dest_path)?;
                Ok(Some(dest_path))
            })));
        });

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if !matches!(status, AutoUpdateStatus::Downloading { .. }) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Updated {
                version: semver::Version::new(0, 100, 1)
            }
        );
        let will_restart = cx.expect_restart();
        cx.update(|cx| cx.restart());
        let (path, arguments) = will_restart.await.unwrap();
        assert!(arguments.is_empty());
        let path = path.unwrap();
        assert_eq!(path, tmp_dir.path().join("zed"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "<fake-zed-update>");
    }

    #[gpui::test]
    async fn test_download_release_reports_progress(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(
                        http_client::http::header::CONTENT_LENGTH,
                        body.len().to_string(),
                    )
                    .body(body.into())
                    .unwrap())
            }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<f32>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                if let Some(fraction) = fraction {
                    reported.borrow_mut().push(fraction);
                }
            }
        })
        .await
        .unwrap();

        let reported = reported.borrow();
        assert!(
            reported.len() >= 2,
            "expected progress to be reported across multiple reads, got {reported:?}"
        );
        assert_eq!(
            reported.last().copied(),
            Some(1.0),
            "download should finish at 100%"
        );
        for fraction in reported.iter() {
            assert!(
                (0.0..=1.0).contains(fraction),
                "progress {fraction} out of range"
            );
        }
        for pair in reported.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "progress must not decrease: {reported:?}"
            );
        }

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }

    #[gpui::test]
    async fn test_download_release_without_content_length_reports_no_progress(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move { Ok(Response::builder().status(200).body(body.into()).unwrap()) }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<Option<f32>>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                reported.borrow_mut().push(fraction);
            }
        })
        .await
        .unwrap();

        assert!(
            reported.borrow().is_empty(),
            "progress should not be reported when the total size is unknown, got {:?}",
            reported.borrow()
        );

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }

    #[test]
    fn test_stable_does_not_update_when_fetched_version_is_not_higher() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = semver::Version::new(1, 0, 0);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_stable_does_update_when_fetched_version_is_higher() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = semver::Version::new(1, 0, 1);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), Some(fetched_version));
    }

    #[test]
    fn test_stable_does_not_update_when_fetched_version_is_not_higher_than_cached() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: semver::Version::new(1, 0, 1),
        };
        let fetched_version = semver::Version::new(1, 0, 1);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_stable_does_update_when_fetched_version_is_higher_than_cached() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: semver::Version::new(1, 0, 1),
        };
        let fetched_version = semver::Version::new(1, 0, 2);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), Some(fetched_version));
    }

    #[test]
    fn test_nightly_does_not_update_when_fetched_sha_is_same() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+a".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_fetched_sha_is_not_same() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_update_when_fetched_version_is_same_as_cached() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_fetched_sha_is_not_same_as_cached() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+c".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_redownload_after_updating_to_fetched_version() {
        let release_channel = ReleaseChannel::Nightly;
        let installed_version = semver::Version::new(1, 0, 0);
        let fetched_version = "1.0.0+nightly.b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            Ok(Some("a".to_string())),
            installed_version.clone(),
            fetched_version.clone(),
            AutoUpdateStatus::Idle,
        )
        .unwrap()
        .expect("a newer nightly version should be available");

        let next_check = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            Ok(Some("a".to_string())),
            installed_version,
            fetched_version,
            AutoUpdateStatus::Updated {
                version: newer_version,
            },
        );

        assert_eq!(next_check.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_installed_versions_sha_cannot_be_retrieved() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+a".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_update_when_cached_update_is_same_as_fetched_and_installed_versions_sha_cannot_be_retrieved()
     {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_cached_update_is_not_same_as_fetched_and_installed_versions_sha_cannot_be_retrieved()
     {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+c".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }
}
