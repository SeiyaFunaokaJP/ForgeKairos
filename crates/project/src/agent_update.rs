//! ADR-F99: what each place an external agent from the ACP registry is
//! installed in remembers about that install, so that a start launches it as
//! it is — with no `npm install`, download or version query — and a newer
//! registry version is only installed once the user has agreed to it.
//!
//! Every place has an install of its own: this host, each WSL distro the
//! sandbox runs agents in (on Windows), and each remote host (whose
//! `remote_server` keeps its own records, next to its own installs).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};
use util::ResultExt as _;

pub use settings::AgentServersUpdateCheck;

use crate::AgentId;
use crate::agent_server_store::{registry_archive_installation_dir, sanitize_path_component};

/// The record's file name, next to the install it describes.
pub const INSTALLED_RECORD_FILE_NAME: &str = "forgekairos-installed.json";

/// What one place remembers about the install of one registry agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledAgentRecord {
    /// The registry version this install was made for. It is compared with
    /// the registry's own version as text: npm may resolve a version below
    /// the registry's pin (`bounded_npm_package_spec`), so what is on disk
    /// would nag for an update the registry has already been given.
    pub registry_version: String,
    pub launch: InstalledAgentLaunch,
    /// When this place last started the agent, in seconds since the Unix
    /// epoch (UTC). The update check leaves out a place that has not started
    /// it for [`STALE_PLACE_AFTER_SECONDS`]: a distro no longer used would
    /// otherwise ask for an update no start there ever takes. Records written
    /// before it existed have none, and count as started.
    #[serde(default)]
    pub last_started: Option<u64>,
}

impl InstalledAgentRecord {
    /// The record a start writes for what it installed.
    pub fn started_now(registry_version: String, launch: InstalledAgentLaunch) -> Self {
        Self {
            registry_version,
            launch,
            last_started: Some(unix_now()),
        }
    }
}

/// How long a place may go without starting an agent before the update check
/// stops asking about its install.
pub const STALE_PLACE_AFTER_SECONDS: u64 = 30 * 24 * 60 * 60;

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// What a start that launches a recorded install writes back: the same
/// record, started now.
pub async fn record_start(fs: &dyn Fs, path: &Path, mut record: InstalledAgentRecord) {
    record.last_started = Some(unix_now());
    write_installed_record(fs, path, &record).await.log_err();
}

/// The kind of distribution an install was made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDistributionKind {
    Npx,
    Archive,
}

/// Where one install of an agent is. A remote host's installs are that
/// host's, and its store keeps the agreements for them as [`Self::Host`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentInstallPlace {
    Host,
    /// A WSL distro the sandbox runs agents in, by the directory its records
    /// are kept under ([`wsl_record_path`]).
    Wsl {
        distro_directory: String,
    },
}

impl AgentInstallPlace {
    /// `distro`: `None` for the default distro.
    pub fn wsl(distro: Option<&str>) -> Self {
        Self::Wsl {
            distro_directory: wsl_distro_directory(distro),
        }
    }

    /// Where a start for `target` installs the agent: the WSL distro of the
    /// sandbox on Windows, and this host everywhere else.
    pub fn for_launch_target(target: &crate::AgentLaunchTarget) -> Self {
        #[cfg(target_os = "windows")]
        if let crate::AgentLaunchTarget::Sandbox { wsl_distro } = target {
            return Self::wsl(wsl_distro.as_deref());
        }
        #[cfg(not(target_os = "windows"))]
        let _ = target;
        Self::Host
    }
}

/// How to launch exactly that install without asking the registry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstalledAgentLaunch {
    Npx {
        /// The registry's package specification (`name@version`).
        package: String,
        /// The package's executable, run with Node.js. On the host an
        /// absolute path; in a WSL distro the Linux path there.
        executable: String,
        args: Vec<String>,
        /// The distribution's own environment, as the registry gave it.
        env: HashMap<String, String>,
    },
    Archive {
        archive_url: String,
        /// The digest the download was checked against, when there was one.
        sha256: Option<String>,
        /// The directory the build was unpacked in. On the host an absolute
        /// path; for the WSL sandbox the path below the distro's data
        /// directory it was staged at.
        directory: String,
        cmd: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
}

impl InstalledAgentLaunch {
    pub fn kind(&self) -> AgentDistributionKind {
        match self {
            Self::Npx { .. } => AgentDistributionKind::Npx,
            Self::Archive { .. } => AgentDistributionKind::Archive,
        }
    }
}

/// What a start does with the install a place has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchDecision {
    /// Launch the recorded install as it is. `newer` is the registry's version
    /// when it is not the recorded one, which the start reports so that the
    /// thread can offer the update.
    Recorded { newer: Option<String> },
    /// Install the registry's version and record it.
    Install,
}

/// `recorded_version` is the version of a usable record (its files are still
/// there), `None` when there is none.
pub fn launch_decision(
    recorded_version: Option<&str>,
    registry_version: &str,
    update_approved: bool,
) -> LaunchDecision {
    match recorded_version {
        None => LaunchDecision::Install,
        Some(recorded) if recorded == registry_version => LaunchDecision::Recorded { newer: None },
        Some(_) if update_approved => LaunchDecision::Install,
        Some(_) => LaunchDecision::Recorded {
            newer: Some(registry_version.to_string()),
        },
    }
}

/// What the WSL sandbox does with the npm package its distro has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslNpmPlan {
    pub install: bool,
    pub newer: Option<String>,
}

/// [`launch_decision`] for an npm package in a WSL distro. A record only
/// counts while the package it names can still be read there
/// (`installed.is_some()`). Without one, the rule from before the records
/// (ADR-F49 決定 22) decides once — install unless what is there already
/// satisfies the registry's pin — and the record written after it settles
/// every later start.
pub fn wsl_npm_plan(
    recorded_version: Option<&str>,
    installed: Option<&semver::Version>,
    pinned: Option<&semver::Version>,
    registry_version: &str,
    update_approved: bool,
) -> WslNpmPlan {
    match recorded_version.filter(|_| installed.is_some()) {
        Some(recorded) => {
            match launch_decision(Some(recorded), registry_version, update_approved) {
                LaunchDecision::Recorded { newer } => WslNpmPlan {
                    install: false,
                    newer,
                },
                LaunchDecision::Install => WslNpmPlan {
                    install: true,
                    newer: None,
                },
            }
        }
        None => WslNpmPlan {
            install: match (installed, pinned) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(installed), Some(pinned)) => installed < pinned,
            },
            newer: None,
        },
    }
}

/// The record at `path`, or `None` when there is none or it cannot be read
/// (which a start treats the same way: it installs and writes a new one).
pub async fn read_installed_record(fs: &dyn Fs, path: &Path) -> Option<InstalledAgentRecord> {
    if !fs.is_file(path).await {
        return None;
    }
    let text = fs
        .load(path)
        .await
        .with_context(|| format!("reading {}", path.display()))
        .log_err()?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))
        .log_err()
}

pub async fn write_installed_record(
    fs: &dyn Fs,
    path: &Path,
    record: &InstalledAgentRecord,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(record)?;
    fs.atomic_write(path.to_path_buf(), text)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

/// Where this host installs the npm package of the registry agent
/// `registry_id`.
pub fn host_npx_install_dir(registry_id: &str) -> PathBuf {
    paths::external_agents_dir()
        .join("registry")
        .join("npx")
        .join(sanitize_path_component(registry_id))
}

pub fn host_npx_record_path(registry_id: &str) -> PathBuf {
    host_npx_install_dir(registry_id).join(INSTALLED_RECORD_FILE_NAME)
}

pub fn host_archive_record_path(registry_id: &str) -> PathBuf {
    registry_archive_installation_dir(registry_id).join(INSTALLED_RECORD_FILE_NAME)
}

/// The records of what the WSL sandbox installed, kept on this host (the
/// installs themselves are in the distros, where only a `wsl.exe` round trip
/// could read a record): one directory per distro.
pub fn wsl_records_dir() -> PathBuf {
    paths::external_agents_dir().join("wsl-sandbox-installed")
}

/// The directory the records of the WSL distro `distro` (`None`: the default
/// distro) are kept under. `@` never survives [`sanitize_path_component`], so
/// the default distro's directory cannot be one a named distro maps to.
fn wsl_distro_directory(distro: Option<&str>) -> String {
    distro
        .map(sanitize_path_component)
        .unwrap_or_else(|| "@default".to_string())
}

/// The record of the registry agent `registry_id` in the WSL distro `distro`
/// (`None`: the default distro).
pub fn wsl_record_path(distro: Option<&str>, registry_id: &str) -> PathBuf {
    wsl_records_dir()
        .join(wsl_distro_directory(distro))
        .join(format!("{}.json", sanitize_path_component(registry_id)))
}

/// What one place's record says an agent is installed at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedAgentVersion {
    pub place: AgentInstallPlace,
    pub registry_version: String,
}

/// What this host's records say the registry agent `registry_id` is
/// installed at, on this host and in every WSL distro the sandbox installed
/// it in (a remote host's records are on that host) — for the update check,
/// so only the places that still matter: a record of another distribution
/// kind than the registry's `kind` describes an install no start makes any
/// more, and a place that has not started the agent for
/// [`STALE_PLACE_AFTER_SECONDS`] before `now` (Unix seconds) is not asked
/// about.
pub async fn recorded_agent_versions(
    fs: &dyn Fs,
    registry_id: &str,
    kind: AgentDistributionKind,
    now: u64,
) -> Vec<RecordedAgentVersion> {
    let host_record = match kind {
        AgentDistributionKind::Npx => host_npx_record_path(registry_id),
        AgentDistributionKind::Archive => host_archive_record_path(registry_id),
    };
    let mut paths = vec![(AgentInstallPlace::Host, host_record)];
    let wsl_records = wsl_records_dir();
    if fs.is_dir(&wsl_records).await
        && let Some(mut entries) = fs.read_dir(&wsl_records).await.log_err()
    {
        let file_name = format!("{}.json", sanitize_path_component(registry_id));
        while let Some(entry) = entries.next().await {
            if let Some(directory) = entry.log_err()
                && let Some(distro_directory) = directory.file_name()
            {
                paths.push((
                    AgentInstallPlace::Wsl {
                        distro_directory: distro_directory.to_string_lossy().into_owned(),
                    },
                    directory.join(&file_name),
                ));
            }
        }
    }

    let mut versions = Vec::new();
    for (place, path) in paths {
        let Some(record) = read_installed_record(fs, &path).await else {
            continue;
        };
        let stale = record
            .last_started
            .is_some_and(|started| now.saturating_sub(started) > STALE_PLACE_AFTER_SECONDS);
        if record.launch.kind() == kind && !stale {
            versions.push(RecordedAgentVersion {
                place,
                registry_version: record.registry_version,
            });
        }
    }
    versions
}

/// The updates the user agreed to, by agent and the place whose install it
/// replaces, until a start of that agent there installs it. Shared by every
/// agent of one [`crate::AgentServerStore`]: those are rebuilt whenever the
/// settings or the registry change, and the agreement has to outlive that.
#[derive(Clone, Default)]
pub struct AgentUpdateApprovals(Arc<parking_lot::Mutex<HashSet<(AgentId, AgentInstallPlace)>>>);

impl AgentUpdateApprovals {
    pub fn approve(&self, agent_id: &AgentId, place: AgentInstallPlace) {
        self.0.lock().insert((agent_id.clone(), place));
    }

    pub fn is_approved(&self, agent_id: &AgentId, place: &AgentInstallPlace) -> bool {
        self.0.lock().contains(&(agent_id.clone(), place.clone()))
    }

    /// Whether an update of `agent_id` was agreed to anywhere.
    pub fn is_approved_anywhere(&self, agent_id: &AgentId) -> bool {
        self.0
            .lock()
            .iter()
            .any(|(approved, _)| approved == agent_id)
    }

    /// Whether an update of `agent_id` at `place` was agreed to, taking the
    /// agreement: the start there that asks is the one that installs it.
    pub fn take(&self, agent_id: &AgentId, place: &AgentInstallPlace) -> bool {
        self.0.lock().remove(&(agent_id.clone(), place.clone()))
    }
}

/// What one start may do with the agreements: the agent's own start takes
/// the one for the place it installs in; a command for anything else (a
/// terminal sign-in) takes none.
#[derive(Clone)]
pub struct UpdateApprovalClaim {
    approvals: Option<AgentUpdateApprovals>,
    agent_id: AgentId,
}

impl UpdateApprovalClaim {
    pub fn for_start(approvals: &AgentUpdateApprovals, agent_id: AgentId) -> Self {
        Self {
            approvals: Some(approvals.clone()),
            agent_id,
        }
    }

    pub fn none(agent_id: AgentId) -> Self {
        Self {
            approvals: None,
            agent_id,
        }
    }

    pub fn take(&self, place: &AgentInstallPlace) -> bool {
        self.approvals
            .as_ref()
            .is_some_and(|approvals| approvals.take(&self.agent_id, place))
    }
}

struct GlobalAgentUpdateApprovals(AgentUpdateApprovals);

impl Global for GlobalAgentUpdateApprovals {}

/// The agreements for the agents this process installs itself. Every local
/// project's store shares them, because the install (and its record) is the
/// same for all of them; a remote host's agents are agreed to through the
/// store that talks to that host.
pub fn local_agent_update_approvals(cx: &mut App) -> AgentUpdateApprovals {
    if let Some(global) = cx.try_global::<GlobalAgentUpdateApprovals>() {
        return global.0.clone();
    }
    let approvals = AgentUpdateApprovals::default();
    cx.set_global(GlobalAgentUpdateApprovals(approvals.clone()));
    approvals
}

/// `agent.agent_servers_update_check`, as the registry and the update check
/// read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, RegisterSetting)]
pub struct AgentServersUpdateCheckSettings {
    pub check: AgentServersUpdateCheck,
}

impl Settings for AgentServersUpdateCheckSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            check: content
                .agent
                .as_ref()
                .and_then(|agent| agent.agent_servers_update_check)
                .unwrap_or_default(),
        }
    }
}

impl AgentServersUpdateCheckSettings {
    /// Whether the IDE looks for updates when it starts. A process with the
    /// setting unregistered (a test's) does what the default says.
    pub fn checks_on_startup(cx: &App) -> bool {
        Self::try_get(cx)
            .is_none_or(|settings| settings.check == AgentServersUpdateCheck::OnStartup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use semver::Version;

    #[test]
    fn a_start_launches_the_recorded_install_unless_an_update_was_agreed_to() {
        assert_eq!(
            launch_decision(None, "2.0.0", false),
            LaunchDecision::Install
        );
        assert_eq!(
            launch_decision(None, "2.0.0", true),
            LaunchDecision::Install
        );
        assert_eq!(
            launch_decision(Some("2.0.0"), "2.0.0", false),
            LaunchDecision::Recorded { newer: None }
        );
        assert_eq!(
            launch_decision(Some("2.0.0"), "2.0.0", true),
            LaunchDecision::Recorded { newer: None },
            "agreeing to an update that is already installed installs nothing"
        );
        assert_eq!(
            launch_decision(Some("1.0.0"), "2.0.0", false),
            LaunchDecision::Recorded {
                newer: Some("2.0.0".to_string())
            }
        );
        assert_eq!(
            launch_decision(Some("1.0.0"), "2.0.0", true),
            LaunchDecision::Install
        );
        assert_eq!(
            launch_decision(Some("3.0.0"), "2.0.0", false),
            LaunchDecision::Recorded {
                newer: Some("2.0.0".to_string())
            },
            "a registry that went back is offered too, never installed unasked"
        );
    }

    #[test]
    fn the_wsl_npm_package_follows_its_record() {
        let v = |text: &str| Version::parse(text).unwrap();
        let (one, two) = (v("1.0.0"), v("2.0.0"));

        assert_eq!(
            wsl_npm_plan(Some("1.0.0"), Some(&one), Some(&two), "2.0.0", false),
            WslNpmPlan {
                install: false,
                newer: Some("2.0.0".to_string())
            },
            "below the pin, but recorded and not agreed to: kept"
        );
        assert_eq!(
            wsl_npm_plan(Some("1.0.0"), Some(&one), Some(&two), "2.0.0", true),
            WslNpmPlan {
                install: true,
                newer: None
            }
        );
        assert_eq!(
            wsl_npm_plan(Some("2.0.0"), Some(&one), Some(&two), "2.0.0", false),
            WslNpmPlan {
                install: false,
                newer: None
            },
            "npm resolved below the pin for this version: no nagging"
        );
        assert_eq!(
            wsl_npm_plan(Some("2.0.0"), None, Some(&two), "2.0.0", false),
            WslNpmPlan {
                install: true,
                newer: None
            },
            "a record whose package is gone is no record"
        );
        assert_eq!(
            wsl_npm_plan(None, Some(&one), Some(&two), "2.0.0", false),
            WslNpmPlan {
                install: true,
                newer: None
            },
            "without a record, the rule from before decides"
        );
        assert_eq!(
            wsl_npm_plan(None, Some(&two), Some(&two), "2.0.0", false),
            WslNpmPlan {
                install: false,
                newer: None
            }
        );
        assert_eq!(
            wsl_npm_plan(None, None, Some(&two), "2.0.0", false),
            WslNpmPlan {
                install: true,
                newer: None
            }
        );
    }

    #[test]
    fn the_default_distro_has_a_directory_no_named_distro_maps_to() {
        let default = wsl_record_path(None, "claude-acp");
        for distro in ["@default", "-default", "default"] {
            assert_ne!(wsl_record_path(Some(distro), "claude-acp"), default);
        }
        assert_ne!(
            wsl_record_path(Some("Ubuntu"), "claude-acp"),
            wsl_record_path(Some("Debian"), "claude-acp")
        );
    }

    #[test]
    fn an_agreement_is_taken_by_one_start_at_its_place() {
        let approvals = AgentUpdateApprovals::default();
        let agent = AgentId::new("claude-acp");
        let host = AgentInstallPlace::Host;
        let ubuntu = AgentInstallPlace::wsl(Some("Ubuntu"));
        assert!(!approvals.take(&agent, &host));
        approvals.approve(&agent, ubuntu.clone());
        assert!(approvals.is_approved_anywhere(&agent));
        assert!(
            !approvals.take(&agent, &host),
            "a start on this host does not take the distro's agreement"
        );
        assert!(!approvals.take(&agent, &AgentInstallPlace::wsl(None)));
        assert!(approvals.take(&agent, &ubuntu));
        assert!(
            !approvals.take(&agent, &ubuntu),
            "the next start is not an update"
        );
        assert!(!approvals.is_approved_anywhere(&agent));

        approvals.approve(&agent, host.clone());
        let sign_in = UpdateApprovalClaim::none(agent.clone());
        assert!(!sign_in.take(&host), "a sign-in command takes nothing");
        assert!(approvals.is_approved(&agent, &host));
        assert!(UpdateApprovalClaim::for_start(&approvals, agent.clone()).take(&host));
        assert!(!approvals.is_approved(&agent, &host));
    }

    fn npx_record(version: &str, last_started: Option<u64>) -> InstalledAgentRecord {
        InstalledAgentRecord {
            registry_version: version.to_string(),
            launch: InstalledAgentLaunch::Npx {
                package: format!("test-agent@{version}"),
                executable: "/x.js".to_string(),
                args: Vec::new(),
                env: HashMap::default(),
            },
            last_started,
        }
    }

    fn archive_record(version: &str, last_started: Option<u64>) -> InstalledAgentRecord {
        InstalledAgentRecord {
            registry_version: version.to_string(),
            launch: InstalledAgentLaunch::Archive {
                archive_url: "https://example.test/agent".to_string(),
                sha256: None,
                directory: "/v_1".to_string(),
                cmd: "./agent".to_string(),
                args: Vec::new(),
                env: HashMap::default(),
            },
            last_started,
        }
    }

    /// ADR-F99: the update check asks about every place this host keeps a
    /// record for, and only those: not a record of another distribution kind
    /// than the registry's (no start makes that install any more), and not a
    /// place that has not started the agent for 30 days. A record from before
    /// `last_started` counts.
    #[gpui::test]
    async fn the_update_check_reads_the_records_of_places_still_in_use(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let now = 100 * 24 * 60 * 60;
        let day = 24 * 60 * 60;
        let check = async |kind| {
            let mut versions = recorded_agent_versions(fs.as_ref(), "test-agent", kind, now).await;
            versions.sort_by(|left, right| left.registry_version.cmp(&right.registry_version));
            versions
        };
        assert!(check(AgentDistributionKind::Npx).await.is_empty());

        for (path, record) in [
            (
                host_npx_record_path("test-agent"),
                npx_record("1.0.0", Some(now - day)),
            ),
            (
                host_archive_record_path("test-agent"),
                archive_record("0.1.0", Some(now)),
            ),
            (
                wsl_record_path(Some("Ubuntu"), "test-agent"),
                npx_record("0.9.0", None),
            ),
            (
                wsl_record_path(Some("Debian"), "test-agent"),
                npx_record("0.8.0", Some(now - 31 * day)),
            ),
            (
                wsl_record_path(None, "test-agent"),
                archive_record("0.7.0", Some(now)),
            ),
            (
                wsl_record_path(Some("Ubuntu"), "other-agent"),
                npx_record("5.0.0", Some(now)),
            ),
        ] {
            write_installed_record(fs.as_ref(), &path, &record)
                .await
                .unwrap();
        }

        assert_eq!(
            check(AgentDistributionKind::Npx).await,
            vec![
                RecordedAgentVersion {
                    place: AgentInstallPlace::wsl(Some("Ubuntu")),
                    registry_version: "0.9.0".to_string(),
                },
                RecordedAgentVersion {
                    place: AgentInstallPlace::Host,
                    registry_version: "1.0.0".to_string(),
                },
            ]
        );
        assert_eq!(
            check(AgentDistributionKind::Archive).await,
            vec![
                RecordedAgentVersion {
                    place: AgentInstallPlace::Host,
                    registry_version: "0.1.0".to_string(),
                },
                RecordedAgentVersion {
                    place: AgentInstallPlace::wsl(None),
                    registry_version: "0.7.0".to_string(),
                },
            ]
        );
    }

    #[gpui::test]
    async fn records_round_trip_and_unreadable_ones_count_as_none(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let path = Path::new("/agents/npx/claude-acp").join(INSTALLED_RECORD_FILE_NAME);
        assert_eq!(read_installed_record(fs.as_ref(), &path).await, None);

        let record = InstalledAgentRecord {
            registry_version: "0.6.0".to_string(),
            launch: InstalledAgentLaunch::Npx {
                package: "@agentclientprotocol/claude-agent-acp@0.6.0".to_string(),
                executable: "/agents/npx/claude-acp/node_modules/x/dist/index.js".to_string(),
                args: vec!["--acp".to_string()],
                env: HashMap::from_iter([("A".to_string(), "1".to_string())]),
            },
            last_started: Some(1),
        };
        write_installed_record(fs.as_ref(), &path, &record)
            .await
            .unwrap();
        assert_eq!(
            read_installed_record(fs.as_ref(), &path).await,
            Some(record)
        );

        fs.insert_file(&path, b"{ not json".to_vec()).await;
        assert_eq!(read_installed_record(fs.as_ref(), &path).await, None);

        fs.insert_file(
            &path,
            br#"{"registry_version":"0.5.0","launch":{"kind":"npx","package":"p@0.5.0","executable":"/x.js","args":[],"env":{}}}"#
                .to_vec(),
        )
        .await;
        assert_eq!(
            read_installed_record(fs.as_ref(), &path)
                .await
                .map(|record| (record.registry_version, record.last_started)),
            Some(("0.5.0".to_string(), None)),
            "a record from before `last_started` is still read"
        );
    }
}
