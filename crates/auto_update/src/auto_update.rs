use anyhow::{Context as _, Result};
use client::Client;
use db::kvp::KeyValueStore;
// `smol::fs` is imported below under the same name, so the crate has to be
// named through the crate root here.
use ::fs::Fs;
use futures_lite::StreamExt;
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, EventEmitter, Global,
    Task, TaskExt, Window, actions,
};
use http_client::{
    AsyncBody, EgressDenied, HttpClient, HttpRequestExt as _, RedirectPolicy, Request, RequestHost,
    Response, StatusCode, UPDATE_REPO, Url,
    github::{GithubRelease, get_release_by_tag_name, latest_github_release},
    http::{HeaderMap, header},
    ide_lane,
};
use paths::remote_servers_dir;
use release_channel::{AppCommitSha, ReleaseChannel};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsStore};
use sha2::{Digest as _, Sha256};
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
        Err(_) => return i18n::t!("Please install rsync using your package manager"),
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
        Some(i18n::t!("Install it with: sudo pacman -S rsync"))
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "debian" || distribution_id == "ubuntu")
    {
        Some(i18n::t!("Install it with: sudo apt install rsync"))
    } else if distribution_ids.iter().any(|distribution_id| {
        distribution_id == "fedora"
            || distribution_id == "rhel"
            || distribution_id == "centos"
            || distribution_id == "rocky"
            || distribution_id == "almalinux"
    }) {
        Some(i18n::t!("Install it with: sudo dnf install rsync"))
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "nixos")
    {
        Some(i18n::t!("Install pkgs.rsync from nixpkgs"))
    } else {
        None
    };

    package_manager_hint.unwrap_or(i18n::t!("Please install rsync using your package manager"))
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

pub enum AutoUpdateEvent {
    /// A manual check received a release response and found no newer version.
    UpToDate,
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
    /// GitHub's `digest` for the asset (`sha256:<hex>`), when it lists one.
    pub digest: Option<String>,
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

#[derive(Clone, Copy, Debug, RegisterSetting)]
struct IncludePrereleasesSetting(bool);

impl Settings for IncludePrereleasesSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update_include_prereleases.unwrap_or(false))
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
            i18n::t!("Zed was installed via a package manager."),
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
            i18n::t!("Could not check for updates"),
            Some(i18n::t!("Auto-updates disabled for non-bundled app.")),
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
        ReleaseChannel::Stable | ReleaseChannel::Preview if let Some(tag) = RELEASE_TAG => {
            format!("https://github.com/{UPDATE_REPO}/releases/tag/{tag}")
        }
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

impl EventEmitter<AutoUpdateEvent> for AutoUpdater {}

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
        let asset_names = asset_names(asset, os, arch)?;
        let asset_name = asset_names.join(" or ");

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

        asset_from_release(release, &asset_names)
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
        let (client, installed_version, previous_status, release_channel, include_prereleases) =
            this.read_with(cx, |this, cx| {
                let release_channel =
                    ReleaseChannel::try_global(cx).unwrap_or(ReleaseChannel::Stable);
                (
                    // ADR-F38: updating the editor is IDE infrastructure.
                    ide_lane(this.client.http_client()),
                    this.current_version.clone(),
                    this.status.clone(),
                    release_channel,
                    // A preview or nightly build already runs pre-releases.
                    IncludePrereleasesSetting::get_global(cx).0
                        || !matches!(release_channel, ReleaseChannel::Stable),
                )
            });

        Self::check_dependencies()?;

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Checking;
            log::info!("Auto Update: checking for updates");
            cx.notify();
        });

        let asset_name = asset_name("zed", OS, ARCH)?;
        let releases = fetch_published_releases(client.clone()).await?;
        let fetched_release_data =
            newest_update_release(releases, include_prereleases, &asset_name);
        let newer_version = match &fetched_release_data {
            Some(fetched_release_data) => {
                let app_commit_sha =
                    Ok(cx.update(|cx| AppCommitSha::try_global(cx).map(|sha| sha.full())));
                Self::check_if_fetched_version_is_newer(
                    release_channel,
                    app_commit_sha,
                    installed_release_version(&installed_version, RELEASE_TAG),
                    fetched_release_data.version.clone(),
                    previous_status.clone(),
                )?
            }
            None => {
                log::info!("Auto Update: no release of {UPDATE_REPO} carries {asset_name}");
                None
            }
        };

        let (Some(newer_version), Some(fetched_release_data)) =
            (newer_version, fetched_release_data)
        else {
            this.update(cx, |this, cx| {
                this.status = match previous_status {
                    AutoUpdateStatus::Updated { .. } => previous_status,
                    _ => {
                        if this.update_check_type.is_manual() {
                            cx.emit(AutoUpdateEvent::UpToDate);
                        }
                        AutoUpdateStatus::Idle
                    }
                };
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
            return Err(MissingDependencyError(i18n::t!(
                "rsync is required for auto-updates but is not installed. {install_hint}"
            ))
            .into());
        }

        #[cfg(target_os = "macos")]
        anyhow::ensure!(
            which::which("rsync").is_ok(),
            i18n::t!("Could not auto-update because the required rsync utility was not found.")
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
        installed_version: Version,
        fetched_version: Version,
    ) -> Option<Version> {
        // The pre-release field is compared, so that `1.2.0` replaces a
        // `1.2.0-pre` build; build metadata only names the channel and commit.
        (fetched_version.cmp_precedence(&installed_version) == std::cmp::Ordering::Greater)
            .then_some(fetched_version)
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

/// The names a release may carry an asset under, the one [`asset_name`] gives
/// first. Earlier packages published the remote server under the legacy
/// prefix (upstream's `zed-remote-server-`), which is still accepted.
fn asset_names(asset: &str, os: &str, arch: &str) -> Result<Vec<String>> {
    let mut names = vec![asset_name(asset, os, arch)?];
    if asset == "zed-remote-server" {
        for legacy in branding::LEGACY {
            let name = format!("{}{os}-{arch}.gz", legacy.remote_server_binary_prefix);
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Ok(names)
}

/// What the downloaded Windows installer is saved as in the `updates` directory.
const WINDOWS_INSTALLER_FILE_NAME: &str =
    branding::concat_const!(branding::CURRENT.executable_stem, "-setup.exe");

/// The file name this fork publishes an asset under on its GitHub Releases.
///
/// The upstream endpoint this replaced chose the file server-side, so the
/// convention has to be spelled out on the client now. `zed-remote-server`
/// is named after `branding::CURRENT`, the name ADR-F15 looks for first next
/// to the executable, so a single file in a release serves both the bundled
/// and the downloaded path.
fn asset_name(asset: &str, os: &str, arch: &str) -> Result<String> {
    match asset {
        "zed-remote-server" => Ok(format!(
            "{}{os}-{arch}.gz",
            branding::CURRENT.remote_server_binary_prefix
        )),
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

/// The tag of the GitHub release this build was made for (`v1.2.0-pre`), set
/// in the environment of the release build. The Cargo version alone cannot
/// tell a pre-release build from the release that later ships with the same
/// number.
const RELEASE_TAG: Option<&str> = option_env!("FORGEKAIROS_RELEASE_TAG");

/// GitHub lists releases newest-created first, which is not the same as
/// highest version, so one page is read and compared as a whole. An update
/// check is one request per poll, well inside the 60 an hour GitHub allows
/// without signing in.
const RELEASES_PER_PAGE: usize = 30;
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

fn release_tag_version(tag: &str) -> Option<Version> {
    tag.strip_prefix('v').unwrap_or(tag).parse().ok()
}

/// The version an update has to be newer than. Without a release tag the
/// build is taken to be the release of its Cargo version.
fn installed_release_version(installed_version: &Version, release_tag: Option<&str>) -> Version {
    let tagged = release_tag.and_then(|tag| {
        let version = release_tag_version(tag);
        if version.is_none() {
            log::warn!("auto-update: ignoring the release tag {tag:?}, it is not a version");
        }
        version
    });
    let mut version = tagged.unwrap_or_else(|| {
        let mut version = installed_version.clone();
        version.pre = semver::Prerelease::EMPTY;
        version
    });
    version.build = semver::BuildMetadata::EMPTY;
    version
}

/// One entry of `GET /repos/{repo}/releases`, with only the fields the update
/// check reads. Unlike [`GithubRelease`] it keeps `draft`: drafts are listed to
/// a `GITHUB_TOKEN` with push access and must never be installed.
#[derive(Deserialize, Debug)]
struct PublishedRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<PublishedAsset>,
}

#[derive(Deserialize, Debug)]
struct PublishedAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

/// Why the release list could not be read. Its `Display` is what a manual
/// check shows, so it has to be the outermost error, without added context.
#[derive(Debug)]
enum UpdateCheckError {
    Unreachable(anyhow::Error),
    NotFound,
    RateLimited { retry_after: Option<Duration> },
    UnexpectedStatus(u16),
    UnexpectedResponse(serde_json::Error),
}

impl std::fmt::Display for UpdateCheckError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let repository = UPDATE_REPO;
        let message = match self {
            Self::Unreachable(error) => i18n::t!(
                "Couldn't check for updates: GitHub could not be reached ({error:#})."
            ),
            Self::NotFound => i18n::t!(
                "Couldn't check for updates: GitHub has no release list for {repository}. The repository may be private or not published yet."
            ),
            Self::RateLimited {
                retry_after: Some(retry_after),
            } => {
                let minutes = retry_after.as_secs().div_ceil(60).max(1);
                i18n::tn!(
                    minutes,
                    "Couldn't check for updates: GitHub's limit for requests without signing in was reached. Try again in {count} minute.",
                    "Couldn't check for updates: GitHub's limit for requests without signing in was reached. Try again in {count} minutes."
                )
            }
            Self::RateLimited { retry_after: None } => i18n::t!(
                "Couldn't check for updates: GitHub's limit for requests without signing in was reached. Try again later."
            ).to_string(),
            Self::UnexpectedStatus(status) => {
                i18n::t!("Couldn't check for updates: GitHub answered with HTTP status {status}.")
            }
            Self::UnexpectedResponse(_) => i18n::t!(
                "Couldn't check for updates: GitHub's release list was not in the expected format."
            ).to_string(),
        };
        formatter.write_str(&message)
    }
}

impl std::error::Error for UpdateCheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreachable(error) => Some(error.as_ref()),
            Self::UnexpectedResponse(error) => Some(error),
            Self::NotFound | Self::RateLimited { .. } | Self::UnexpectedStatus(_) => None,
        }
    }
}

/// `Some` when GitHub refused the request for its rate limit, holding how
/// long until it lifts when the response says. GitHub answers 403 or 429 for
/// it; a 403 is only the limit when the limit headers say so.
fn rate_limit_retry_after(
    status: StatusCode,
    headers: &HeaderMap,
    now: SystemTime,
) -> Option<Option<Duration>> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let retry_after = header("retry-after").and_then(|value| value.parse::<u64>().ok());
    let limit_exhausted = header("x-ratelimit-remaining") == Some("0");
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && (limit_exhausted || retry_after.is_some()));
    if !rate_limited {
        return None;
    }
    let until_reset = || {
        let reset = header("x-ratelimit-reset")?.parse::<u64>().ok()?;
        let now = now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
        Some(Duration::from_secs(reset.saturating_sub(now)))
    };
    Some(retry_after.map(Duration::from_secs).or_else(until_reset))
}

async fn fetch_published_releases(
    http_client: Arc<dyn HttpClient>,
) -> Result<Vec<PublishedRelease>, UpdateCheckError> {
    let url =
        format!("https://api.github.com/repos/{UPDATE_REPO}/releases?per_page={RELEASES_PER_PAGE}");
    let request = Request::get(&url)
        .header("Accept", "application/vnd.github+json")
        .follow_redirects(RedirectPolicy::FollowAll)
        .timeout(UPDATE_CHECK_TIMEOUT)
        // The same token the editor's other GitHub API calls send; it lifts
        // the rate limit and lets the owner read a private repository.
        .when_some(env::var("GITHUB_TOKEN").ok(), |builder, token| {
            builder.header("Authorization", format!("Bearer {token}"))
        })
        .body(AsyncBody::default())
        .map_err(|error| UpdateCheckError::Unreachable(error.into()))?;

    let mut response = http_client
        .send(request)
        .await
        .map_err(UpdateCheckError::Unreachable)?;
    let status = response.status();
    if let Some(retry_after) = rate_limit_retry_after(status, response.headers(), SystemTime::now())
    {
        log::info!("auto-update: GitHub's rate limit was reached, retry after {retry_after:?}");
        return Err(UpdateCheckError::RateLimited { retry_after });
    }
    if status == StatusCode::NOT_FOUND {
        return Err(UpdateCheckError::NotFound);
    }
    if !status.is_success() {
        return Err(UpdateCheckError::UnexpectedStatus(status.as_u16()));
    }

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .map_err(|error| UpdateCheckError::Unreachable(error.into()))?;
    serde_json::from_slice(&body).map_err(|error| {
        log::error!(
            "auto-update: unexpected release list: {error}: {:?}",
            String::from_utf8_lossy(&body)
        );
        UpdateCheckError::UnexpectedResponse(error)
    })
}

/// The highest-versioned published release that carries `asset_name`.
/// Drafts are never candidates, and pre-releases only when asked for.
fn newest_update_release(
    releases: Vec<PublishedRelease>,
    include_prereleases: bool,
    asset_name: &str,
) -> Option<ReleaseAsset> {
    releases
        .into_iter()
        .filter(|release| !release.draft && (include_prereleases || !release.prerelease))
        .filter_map(|release| {
            let Some(version) = release_tag_version(&release.tag_name) else {
                log::warn!(
                    "auto-update: skipping release {:?}, its tag is not a version",
                    release.tag_name
                );
                return None;
            };
            let asset = release
                .assets
                .into_iter()
                .find(|asset| asset.name == asset_name)?;
            Some((version, asset))
        })
        .max_by(|(left, _), (right, _)| left.cmp_precedence(right))
        .map(|(version, asset)| ReleaseAsset {
            version: version.to_string(),
            url: asset.browser_download_url,
            digest: asset.digest,
        })
}

/// Maps a GitHub release onto the shape the updater consumes: the tag with a
/// leading `v` stripped as the version, and the download URL of the first of
/// `asset_names` the release carries.
fn asset_from_release(release: GithubRelease, asset_names: &[String]) -> Result<ReleaseAsset> {
    let GithubRelease {
        tag_name,
        mut assets,
        ..
    } = release;
    let version = tag_name
        .strip_prefix('v')
        .unwrap_or(tag_name.as_str())
        .to_string();
    let index = asset_names
        .iter()
        .find_map(|name| assets.iter().position(|asset| &asset.name == name))
        .with_context(|| {
            format!(
                "release {tag_name} has no asset named {}",
                asset_names.join(" or ")
            )
        })?;
    let asset = assets.swap_remove(index);

    Ok(ReleaseAsset {
        version,
        url: asset.browser_download_url,
        digest: asset.digest,
    })
}

/// How many redirects a release download may take. GitHub answers the
/// download URL with one, to its asset host.
const RELEASE_DOWNLOAD_REDIRECT_LIMIT: usize = 5;

/// Requests a release asset, following redirects one hop at a time.
///
/// The URL comes from the release list, and the IDE lane lets a request and
/// its redirects go anywhere, so without this check a tampered or unexpected
/// answer could have the updater download — and then run — a file from any
/// host. The first hop has to be one of [`UPDATE_REPO`]'s release downloads
/// and every later one has to stay on GitHub, over HTTPS.
async fn send_release_download(
    client: &Arc<dyn HttpClient>,
    download_url: &str,
) -> Result<Response<AsyncBody>> {
    let mut url = Url::parse(download_url)
        .with_context(|| format!("invalid release download URL {download_url:?}"))?;
    let release_downloads = format!("/{UPDATE_REPO}/releases/download/").to_ascii_lowercase();
    anyhow::ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url
                .path()
                .to_ascii_lowercase()
                .starts_with(&release_downloads),
        "refusing to download {url}: it is not a release download of {UPDATE_REPO}"
    );

    for _ in 0..=RELEASE_DOWNLOAD_REDIRECT_LIMIT {
        let request = Request::get(url.as_str())
            .follow_redirects(RedirectPolicy::NoFollow)
            .body(AsyncBody::default())?;
        let response = client.send(request).await?;
        if !matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            return Ok(response);
        }
        let location = response
            .headers()
            .get(header::LOCATION)
            .context("the release download redirected without a location")?
            .to_str()
            .context("the release download redirected to an unreadable location")?;
        let target = url
            .join(location)
            .context("the release download redirected to an invalid location")?;
        let on_github = target.scheme() == "https"
            && target.port().is_none()
            && target.host_str().is_some_and(|host| {
                host == "github.com" || host.ends_with(".githubusercontent.com")
            });
        // Only the host is named: the asset host's URL carries a signature.
        anyhow::ensure!(
            on_github,
            "refusing to follow the release download to {}: it is not a GitHub host",
            target.host_str().unwrap_or("an unnamed host")
        );
        url = target;
    }
    anyhow::bail!("the release download redirected too many times")
}

/// A digest in a form other than GitHub's `sha256:` is let through with a
/// warning rather than failing every update on a format change.
fn check_release_digest(expected: Option<&str>, hasher: Sha256) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = format!("{:x}", hasher.finalize());
    match expected.strip_prefix("sha256:") {
        Some(expected) => anyhow::ensure!(
            expected.eq_ignore_ascii_case(&actual),
            "the download does not match the digest GitHub lists for it"
        ),
        None => log::warn!("auto-update: cannot check the release digest {expected:?}"),
    }
    Ok(())
}

async fn download_remote_server_binary(
    target_path: &PathBuf,
    release: ReleaseAsset,
    client: Arc<dyn HttpClient>,
) -> Result<()> {
    let temp = tempfile::Builder::new().tempfile_in(remote_servers_dir())?;
    let mut temp_file = File::create(&temp).await?;

    let mut response = send_release_download(&client, &release.url).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download remote server release: {:?}",
        response.status()
    );
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    let body = response.body_mut();
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        let Some(chunk) = buffer.get(..bytes_read).filter(|chunk| !chunk.is_empty()) else {
            break;
        };
        hasher.update(chunk);
        temp_file.write_all(chunk).await?;
    }
    temp_file.flush().await?;
    drop(temp_file);
    check_release_digest(release.digest.as_deref(), hasher)?;
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

    let mut response = send_release_download(&client, &release.url).await?;
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
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    let body = response.body_mut();
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        target_file.write_all(&buffer[..bytes_read]).await?;
        hasher.update(&buffer[..bytes_read]);
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
    drop(target_file);
    // The installer is run next, so a cut-off or altered one has to stop here.
    if let Some(total_bytes) = total_bytes
        && downloaded_bytes != total_bytes
    {
        anyhow::bail!(
            "the update download stopped after {downloaded_bytes} of {total_bytes} bytes"
        );
    }
    check_release_digest(release.digest.as_deref(), hasher)?;
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
    use http_client::{
        AllowlistedHttpClient, EgressPolicy, EgressPolicyHandle, FakeHttpClient, HttpClientWithUrl,
        Response,
    };
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
                "draft": false,
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
            "forgekairos-remote-server-linux-aarch64.gz"
        );
        // Releases published before the rename are still found.
        assert_eq!(
            asset_names("zed-remote-server", "linux", "aarch64").unwrap(),
            [
                "forgekairos-remote-server-linux-aarch64.gz",
                "zed-remote-server-linux-aarch64.gz",
            ]
        );
        assert_eq!(
            asset_names("zed", "linux", "x86_64").unwrap(),
            ["forgekairos-linux-x86_64.tar.gz"]
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

    /// script/bundle.ps1 copies the packages out under these names and the
    /// release workflow uploads them as release assets, so a name that drifts
    /// from `asset_name` would publish a release no updater finds.
    #[test]
    fn the_bundle_script_and_release_workflow_use_the_published_names() {
        let bundle = repository_script("script/bundle.ps1");
        // Linux is copied out twice, after the native and the container build.
        for (os, copies) in [("linux", 2), ("macos", 1)] {
            for asset in ["zed", "zed-remote-server"] {
                let name = asset_name(asset, os, "$arch").unwrap();
                assert_eq!(
                    bundle.matches(&name).count(),
                    copies,
                    "script/bundle.ps1 does not copy {name} after each build"
                );
            }
        }

        let container = repository_script("script/lib/linux-bundle-in-container.sh");
        for asset in ["zed", "zed-remote-server"] {
            let name = asset_name(asset, "linux", "${arch}").unwrap();
            assert!(
                container.contains(&name),
                "linux-bundle-in-container.sh does not copy {name}"
            );
        }

        let workflow = repository_script(".github/workflows/forgekairos_release.yml");
        let matrix_arch = "${{ matrix.arch }}";
        for (asset, os, arch) in [
            ("zed", "windows", "x86_64"),
            ("zed", "linux", "x86_64"),
            ("zed-remote-server", "linux", "x86_64"),
            ("zed", "macos", matrix_arch),
            ("zed-remote-server", "macos", matrix_arch),
        ] {
            let name = asset_name(asset, os, arch).unwrap();
            for expected in [format!("name: {name}"), format!("path: target/{name}")] {
                assert!(
                    workflow.contains(&expected),
                    "forgekairos_release.yml does not upload {expected:?}"
                );
            }
        }
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
        let mapped = asset_from_release(release, &[asset_name]).unwrap();
        assert_eq!(mapped.version, "1.2.3", "the `v` prefix is not a version");
        assert_eq!(mapped.url, "https://test.example/zed");
    }

    fn release_with_assets(names: &[&str]) -> GithubRelease {
        let assets: Vec<String> = names
            .iter()
            .map(|name| {
                format!(
                    r#"{{ "name": "{name}", "browser_download_url": "https://test.example/{name}" }}"#
                )
            })
            .collect();
        serde_json::from_str(&format!(
            r#"{{
                "tag_name": "v1.2.3",
                "draft": false,
                "prerelease": false,
                "tarball_url": "https://api.github.com/tarball/v1.2.3",
                "zipball_url": "https://api.github.com/zipball/v1.2.3",
                "assets": [{}]
            }}"#,
            assets.join(",")
        ))
        .expect("the fixture should parse as the GitHub API shape")
    }

    #[test]
    fn the_remote_server_is_found_under_its_new_name_first_and_its_old_name_after() {
        let names = asset_names("zed-remote-server", "linux", "x86_64").unwrap();
        let current = "forgekairos-remote-server-linux-x86_64.gz";
        let legacy = "zed-remote-server-linux-x86_64.gz";

        let mapped = asset_from_release(release_with_assets(&[legacy, current]), &names).unwrap();
        assert_eq!(mapped.url, format!("https://test.example/{current}"));

        let mapped = asset_from_release(release_with_assets(&[legacy]), &names).unwrap();
        assert_eq!(mapped.url, format!("https://test.example/{legacy}"));

        let error = asset_from_release(release_with_assets(&["unrelated.gz"]), &names)
            .expect_err("a release without the remote server is an error");
        assert!(error.to_string().contains(current), "{error:#}");
    }

    #[test]
    fn a_release_without_this_platform_s_asset_is_an_error() {
        let release = parse_fixture("v1.2.3", "https://test.example/zed");

        let error = asset_from_release(release, &["zed-plan9-x86_64.tar.gz".to_string()])
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
                        let tag = if release_available {
                            "v0.100.1"
                        } else {
                            "v0.100.0"
                        };
                        let download = format!(
                            "https://github.com/{UPDATE_REPO}/releases/download/{tag}/installer"
                        );
                        return Ok(Response::builder()
                            .status(200)
                            .body(github_releases_body(tag, &download).into())
                            .unwrap());
                    } else if req.uri().path()
                        == format!("/{UPDATE_REPO}/releases/download/v0.100.1/installer")
                    {
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
            url: format!("https://github.com/{UPDATE_REPO}/releases/download/v1.0.0/installer"),
            digest: None,
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
            url: format!("https://github.com/{UPDATE_REPO}/releases/download/v1.0.0/installer"),
            digest: None,
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

    fn release_json(
        tag: &str,
        draft: bool,
        prerelease: bool,
        asset: &str,
        url: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "tarball_url": format!("https://api.github.com/tarball/{tag}"),
            "zipball_url": format!("https://api.github.com/zipball/{tag}"),
            "assets": [{ "name": asset, "browser_download_url": url }],
        })
    }

    fn parse_releases(releases: Vec<serde_json::Value>) -> Vec<PublishedRelease> {
        serde_json::from_value(serde_json::Value::Array(releases))
            .expect("the fixture should parse as the GitHub API shape")
    }

    #[test]
    fn the_update_is_the_highest_published_version_carrying_this_platform_s_asset() {
        let asset = asset_name("zed", "windows", "x86_64").unwrap();
        let releases = || {
            parse_releases(vec![
                // GitHub lists by creation date, so the first entry is not
                // necessarily the highest version.
                release_json("v1.1.0", false, false, &asset, "https://example/1.1.0"),
                release_json("v9.0.0", true, false, &asset, "https://example/draft"),
                release_json(
                    "v1.3.0-pre",
                    false,
                    true,
                    &asset,
                    "https://example/1.3.0-pre",
                ),
                release_json("v1.2.0", false, false, &asset, "https://example/1.2.0"),
                release_json(
                    "v5.0.0",
                    false,
                    false,
                    "forgekairos-plan9-x86_64.tar.gz",
                    "https://example/other-platform",
                ),
                release_json("nightly", false, false, &asset, "https://example/nightly"),
            ])
        };

        let release = newest_update_release(releases(), false, &asset).expect("a release");
        assert_eq!(
            (release.version.as_str(), release.url.as_str()),
            ("1.2.0", "https://example/1.2.0"),
            "releases only by default, and never a draft"
        );

        let release = newest_update_release(releases(), true, &asset).expect("a release");
        assert_eq!(
            (release.version.as_str(), release.url.as_str()),
            ("1.3.0-pre", "https://example/1.3.0-pre"),
        );

        assert!(newest_update_release(Vec::new(), true, &asset).is_none());
    }

    #[test]
    fn a_release_replaces_the_pre_release_of_the_same_version() {
        let cargo_version: Version = "1.2.0+stable.0123abc".parse().unwrap();
        let newer = |installed_tag: Option<&str>, fetched: &str| {
            AutoUpdater::check_if_fetched_version_is_newer(
                ReleaseChannel::Stable,
                Ok(None),
                installed_release_version(&cargo_version, installed_tag),
                fetched.to_string(),
                AutoUpdateStatus::Idle,
            )
            .unwrap()
        };

        assert_eq!(
            newer(Some("v1.2.0-pre"), "1.2.0"),
            Some(Version::new(1, 2, 0))
        );
        assert_eq!(newer(Some("v1.2.0-pre"), "1.2.0-pre"), None);
        assert_eq!(newer(Some("v1.2.0"), "1.2.0-pre"), None);
        assert_eq!(
            newer(None, "1.2.0"),
            None,
            "a build without a release tag is the release of its Cargo version"
        );
        assert_eq!(newer(None, "1.3.0-pre"), Some("1.3.0-pre".parse().unwrap()));
        assert_eq!(newer(None, "1.2.0+other.build"), None);
        assert_eq!(
            newer(Some("not a version"), "1.2.0"),
            None,
            "an unreadable tag falls back to the Cargo version"
        );
        assert_eq!(
            installed_release_version(&"1.2.0-dev".parse().unwrap(), None),
            Version::new(1, 2, 0),
            "without a release tag, a pre-release suffix on the Cargo version does not make 1.2.0 an update"
        );
    }

    #[test]
    fn github_s_rate_limit_is_told_apart_from_other_refusals() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, value.parse().unwrap());
            }
            headers
        };

        assert_eq!(
            rate_limit_retry_after(
                StatusCode::FORBIDDEN,
                &headers(&[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "1600")
                ]),
                now,
            ),
            Some(Some(Duration::from_secs(600)))
        );
        assert_eq!(
            rate_limit_retry_after(
                StatusCode::TOO_MANY_REQUESTS,
                &headers(&[("retry-after", "120")]),
                now,
            ),
            Some(Some(Duration::from_secs(120)))
        );
        assert_eq!(
            rate_limit_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers(&[]), now),
            Some(None)
        );
        assert_eq!(
            rate_limit_retry_after(
                StatusCode::FORBIDDEN,
                &headers(&[("x-ratelimit-remaining", "12")]),
                now,
            ),
            None,
            "a 403 with requests left is not the rate limit"
        );
        assert_eq!(
            rate_limit_retry_after(StatusCode::NOT_FOUND, &headers(&[]), now),
            None
        );
    }

    fn answering(
        status: u16,
        headers: &'static [(&'static str, &'static str)],
        body: &'static str,
    ) -> Arc<HttpClientWithUrl> {
        FakeHttpClient::create(move |_| async move {
            let mut response = Response::builder().status(status);
            for (name, value) in headers {
                response = response.header(*name, *value);
            }
            Ok(response.body(body.into()).unwrap())
        })
    }

    #[gpui::test]
    async fn the_release_list_request_is_one_page_of_the_update_repository(
        _cx: &mut TestAppContext,
    ) {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let client = FakeHttpClient::create({
            let seen = seen.clone();
            move |request| {
                seen.lock().push((
                    request.uri().to_string(),
                    request
                        .headers()
                        .get("accept")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                ));
                async move { Ok(Response::builder().status(200).body("[]".into()).unwrap()) }
            }
        });

        let releases = fetch_published_releases(client).await.unwrap();
        assert!(releases.is_empty());
        assert_eq!(
            *seen.lock(),
            vec![(
                "https://api.github.com/repos/SeiyaFunaokaJP/ForgeKairos/releases?per_page=30"
                    .to_string(),
                Some("application/vnd.github+json".to_string()),
            )]
        );
    }

    #[gpui::test]
    async fn a_release_list_that_cannot_be_read_is_a_failed_check(_cx: &mut TestAppContext) {
        let not_found = fetch_published_releases(answering(404, &[], r#"{"message":"Not Found"}"#))
            .await
            .expect_err("a missing repository is not an empty one");
        assert!(
            matches!(not_found, UpdateCheckError::NotFound),
            "{not_found:?}"
        );
        assert_eq!(
            not_found.to_string(),
            "Couldn't check for updates: GitHub has no release list for SeiyaFunaokaJP/ForgeKairos. \
             The repository may be private or not published yet."
        );

        let rate_limited = fetch_published_releases(answering(
            403,
            &[("x-ratelimit-remaining", "0")],
            r#"{"message":"API rate limit exceeded"}"#,
        ))
        .await
        .expect_err("the rate limit is a failure");
        assert!(
            matches!(rate_limited, UpdateCheckError::RateLimited { .. }),
            "{rate_limited:?}"
        );

        let server_error = fetch_published_releases(answering(502, &[], ""))
            .await
            .expect_err("a server error is a failure");
        assert!(
            matches!(server_error, UpdateCheckError::UnexpectedStatus(502)),
            "{server_error:?}"
        );

        let wrong_shape =
            fetch_published_releases(answering(200, &[], r#"{"message":"Moved Permanently"}"#))
                .await
                .expect_err("an object is not a release list");
        assert!(
            matches!(wrong_shape, UpdateCheckError::UnexpectedResponse(_)),
            "{wrong_shape:?}"
        );
    }

    fn init_updater(
        cx: &mut TestAppContext,
        release_channel: ReleaseChannel,
        user_settings: &str,
        http_client: Arc<HttpClientWithUrl>,
    ) -> Entity<AutoUpdater> {
        cx.update(|cx| {
            settings::init(cx);
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(user_settings, cx)
                    .expect("valid user settings");
            });
            release_channel::init_test(Version::new(1, 2, 0), release_channel, cx);
            let client = Client::new(Arc::new(FakeSystemClock::new()), http_client, cx);
            crate::init(client, cx);
            AutoUpdater::get(cx).expect("the updater exists")
        })
    }

    async fn wait_for_poll(updater: &Entity<AutoUpdater>, cx: &mut TestAppContext) {
        for _ in 0..1000 {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            if updater.read_with(cx, |updater, _| updater.pending_poll.is_none()) {
                return;
            }
        }
        panic!("the update check did not finish");
    }

    #[gpui::test]
    async fn a_failed_check_is_shown_only_when_asked_for(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let updater = init_updater(
            cx,
            ReleaseChannel::Stable,
            "{}",
            answering(404, &[], r#"{"message":"Not Found"}"#),
        );

        updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));
        wait_for_poll(&updater, cx).await;
        let status = updater.read_with(cx, |updater, _| updater.status());
        let AutoUpdateStatus::Errored { error } = status else {
            panic!("a manual check that could not read the list must say so, got {status:?}");
        };
        assert!(
            error.to_string().starts_with("Couldn't check for updates:"),
            "the shown message is the check's own, not a wrapper: {error}"
        );

        updater.update(cx, |updater, cx| {
            updater.dismiss(cx);
            updater.poll(UpdateCheckType::Automatic, cx)
        });
        wait_for_poll(&updater, cx).await;
        assert_eq!(
            updater.read_with(cx, |updater, _| updater.status()),
            AutoUpdateStatus::Idle,
            "an automatic check fails quietly"
        );
    }

    #[gpui::test]
    async fn a_dev_build_never_checks(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let requests = Arc::new(atomic::AtomicUsize::new(0));
        let client = FakeHttpClient::create({
            let requests = requests.clone();
            move |_| {
                requests.fetch_add(1, atomic::Ordering::SeqCst);
                async move { Ok(Response::builder().status(200).body("[]".into()).unwrap()) }
            }
        });
        init_updater(
            cx,
            ReleaseChannel::Dev,
            r#"{ "auto_update": true }"#,
            client,
        );

        // The first poll starts after the Windows cleanup, which runs on the
        // real file system outside the test executor, so this waits in real
        // time for as long as a poll would take to show up.
        cx.background_executor.advance_clock(POLL_INTERVAL);
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(10));
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            if requests.load(atomic::Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert_eq!(requests.load(atomic::Ordering::SeqCst), 0);
    }

    /// The update runs on the IDE lane, so neither the release list nor the
    /// download's redirect to GitHub's asset host needs the user's allowlist;
    /// on the gated lane only the list is open, through the built-in entry.
    #[gpui::test]
    async fn the_update_path_passes_the_egress_policy_with_an_empty_allowlist(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let asset = asset_name("zed", OS, ARCH).unwrap();
        let download_url =
            format!("https://github.com/{UPDATE_REPO}/releases/download/v1.3.0/{asset}");
        let body = serde_json::Value::Array(vec![release_json(
            "v1.3.0",
            false,
            false,
            &asset,
            &download_url,
        )])
        .to_string();
        let fake = FakeHttpClient::create(move |request| {
            let body = body.clone();
            async move {
                let response = match request.uri().host() {
                    Some("api.github.com") => Response::builder().status(200).body(body.into()),
                    Some("github.com") => Response::builder()
                        .status(302)
                        .header(
                            "location",
                            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2",
                        )
                        .body(AsyncBody::default()),
                    Some("release-assets.githubusercontent.com") => {
                        Response::builder().status(200).body("<installer>".into())
                    }
                    _ => Response::builder().status(404).body(AsyncBody::default()),
                };
                Ok(response.unwrap())
            }
        });
        let (empty_policy, errors) = EgressPolicy::from_host_strings(Vec::<String>::new());
        assert!(errors.is_empty());
        let gated: Arc<dyn HttpClient> = Arc::new(AllowlistedHttpClient::new(
            fake,
            EgressPolicyHandle::new(empty_policy),
        ));

        let releases = fetch_published_releases(ide_lane(gated.clone()))
            .await
            .expect("the release list is reachable");
        let release = newest_update_release(releases, false, &asset).expect("the release");
        assert_eq!(release.url, download_url);

        let directory = tempdir().unwrap();
        let target = directory.path().join("installer");
        download_release(&target, release.clone(), ide_lane(gated.clone()), |_| {})
            .await
            .expect("the download follows GitHub's redirect");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "<installer>");

        fetch_published_releases(gated.clone())
            .await
            .expect("the built-in entry opens the release list on the gated lane");
        let denied = download_release(&directory.path().join("denied"), release, gated, |_| {})
            .await
            .expect_err("the gated lane does not open the download");
        assert!(
            denied
                .chain()
                .any(|error| error.downcast_ref::<EgressDenied>().is_some()),
            "{denied:?}"
        );
    }

    #[gpui::test]
    async fn pre_releases_are_offered_when_asked_for_or_on_a_pre_release_channel(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let asset = asset_name("zed", OS, ARCH).unwrap();
        let body = serde_json::Value::Array(vec![release_json(
            "v1.3.0-pre",
            false,
            true,
            &asset,
            &format!("https://github.com/{UPDATE_REPO}/releases/download/v1.3.0-pre/{asset}"),
        )])
        .to_string();

        // The download itself is refused, so a check that picked the
        // pre-release ends as a failed download rather than as idle.
        for (release_channel, user_settings, offered) in [
            (ReleaseChannel::Stable, "{}", false),
            (
                ReleaseChannel::Stable,
                r#"{ "auto_update_include_prereleases": true }"#,
                true,
            ),
            (ReleaseChannel::Preview, "{}", true),
        ] {
            let client = FakeHttpClient::create({
                let body = body.clone();
                move |request| {
                    let response = match request.uri().host() {
                        Some("api.github.com") => {
                            Response::builder().status(200).body(body.clone().into())
                        }
                        _ => Response::builder().status(404).body(AsyncBody::default()),
                    };
                    async move { Ok(response.unwrap()) }
                }
            });
            let updater = init_updater(cx, release_channel, user_settings, client);
            updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));
            // Preparing the installer folder touches the real file system,
            // outside the test executor, so this waits in real time.
            for _ in 0..500 {
                std::thread::sleep(Duration::from_millis(10));
                cx.background_executor.timer(Duration::from_millis(0)).await;
                cx.run_until_parked();
                if updater.read_with(cx, |updater, _| updater.pending_poll.is_none()) {
                    break;
                }
            }
            let status = updater.read_with(cx, |updater, _| updater.status());
            assert_eq!(
                matches!(status, AutoUpdateStatus::Errored { .. }),
                offered,
                "{release_channel:?} with {user_settings}: {status:?}"
            );
        }
    }

    /// A GitHub-shaped download host that redirects to `redirect_to` and
    /// records every URL it is asked for.
    fn release_host(
        redirect_to: &'static str,
        body: &'static str,
    ) -> (Arc<dyn HttpClient>, Arc<parking_lot::Mutex<Vec<String>>>) {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let client = FakeHttpClient::create({
            let seen = seen.clone();
            move |request| {
                seen.lock().push(request.uri().to_string());
                let response = if request.uri().host() == Some("github.com") {
                    Response::builder()
                        .status(302)
                        .header("location", redirect_to)
                        .body(AsyncBody::default())
                } else {
                    Response::builder().status(200).body(body.into())
                };
                async move { Ok(response.unwrap()) }
            }
        });
        (client, seen)
    }

    fn release_at(url: &str, digest: Option<&str>) -> ReleaseAsset {
        ReleaseAsset {
            version: "1.3.0".to_string(),
            url: url.to_string(),
            digest: digest.map(str::to_string),
        }
    }

    #[gpui::test]
    async fn a_release_download_never_leaves_github(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let directory = tempdir().unwrap();
        let target = directory.path().join("installer");
        let download_url =
            format!("https://github.com/{UPDATE_REPO}/releases/download/v1.3.0/installer");

        let (client, seen) = release_host(
            "https://release-assets.githubusercontent.com/asset?signature=1",
            "<installer>",
        );
        download_release(&target, release_at(&download_url, None), client, |_| {})
            .await
            .expect("GitHub's own redirect is followed");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "<installer>");
        assert_eq!(seen.lock().len(), 2);

        for refused in [
            "https://evil.example/installer".to_string(),
            "http://github.com/SeiyaFunaokaJP/ForgeKairos/releases/download/v1.3.0/installer"
                .to_string(),
            "https://github.com/someone-else/ForgeKairos/releases/download/v1.3.0/installer"
                .to_string(),
            "https://github.com:8443/SeiyaFunaokaJP/ForgeKairos/releases/download/v1.3.0/installer"
                .to_string(),
            "https://github.com.evil.example/SeiyaFunaokaJP/ForgeKairos/releases/download/v1.3.0/installer"
                .to_string(),
        ] {
            let (client, seen) = release_host("https://unused.example/", "<other>");
            download_release(&target, release_at(&refused, None), client, |_| {})
                .await
                .expect_err(&format!("{refused} is not this repository's release download"));
            assert!(seen.lock().is_empty(), "{refused} was requested");
        }

        for redirect in [
            "https://evil.example/installer",
            "http://release-assets.githubusercontent.com/asset",
            "https://githubusercontent.com.evil.example/asset",
        ] {
            let (client, seen) = release_host(redirect, "<other>");
            let error = download_release(&target, release_at(&download_url, None), client, |_| {})
                .await
                .expect_err(&format!("the redirect to {redirect} is not followed"));
            assert!(
                error.to_string().contains("refusing to follow"),
                "{redirect}: {error:#}"
            );
            assert_eq!(seen.lock().len(), 1, "{redirect} was requested");
        }
    }

    #[gpui::test]
    async fn a_release_download_has_to_match_what_github_lists(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let directory = tempdir().unwrap();
        let target = directory.path().join("installer");
        let download_url =
            format!("https://github.com/{UPDATE_REPO}/releases/download/v1.3.0/installer");
        let digest = format!("sha256:{:x}", Sha256::digest(b"<installer>"));

        let (client, _) = release_host("https://objects.githubusercontent.com/a", "<installer>");
        download_release(
            &target,
            release_at(&download_url, Some(&digest)),
            client,
            |_| {},
        )
        .await
        .expect("the published digest matches");

        let (client, _) = release_host("https://objects.githubusercontent.com/a", "<altered>");
        download_release(
            &target,
            release_at(&download_url, Some(&digest)),
            client,
            |_| {},
        )
        .await
        .expect_err("a download that does not match its digest is not installed");

        let truncated = FakeHttpClient::create(|_| async {
            Ok(Response::builder()
                .status(200)
                .header(http_client::http::header::CONTENT_LENGTH, "100")
                .body("<cut".into())
                .unwrap())
        });
        download_release(&target, release_at(&download_url, None), truncated, |_| {})
            .await
            .expect_err("a download shorter than announced is not installed");

        let mut listed = release_json("v1.3.0", false, false, "installer", &download_url);
        listed["assets"][0]["digest"] = serde_json::Value::String(digest.clone());
        let release = newest_update_release(parse_releases(vec![listed]), false, "installer")
            .expect("the release");
        assert_eq!(release.digest, Some(digest));
    }
}
