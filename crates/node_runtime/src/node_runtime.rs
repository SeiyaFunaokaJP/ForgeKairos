use anyhow::{Context as _, Result, anyhow, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use chrono::{DateTime, Utc};
use futures::{AsyncReadExt, FutureExt as _, channel::oneshot, future::Shared};
use http_client::{Host, HttpClient, Url};
use log::Level;
use semver::{Version, VersionReq};
use serde::Deserialize;
use smol::io::BufReader;
use smol::{fs, lock::Mutex};
use std::collections::HashMap;
use std::fmt::Display;
use std::{
    env::{self, consts},
    ffi::{OsStr, OsString},
    io,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
};
use util::ResultExt;
use util::archive::extract_zip;

const NODE_CA_CERTS_ENV_VAR: &str = "NODE_EXTRA_CA_CERTS";

/// The Node.js this fork installs and runs agents with. The version is pinned,
/// which is what makes the digests in [`PINNED_NODE_ARCHIVES`] possible.
pub const NODE_VERSION: &str = "v24.21.0";

/// ADR-F49 決定 21: the SHA-256 of each [`NODE_VERSION`] archive, as
/// `nodejs.org/dist/<version>/SHASUMS256.txt` publishes them, by
/// `(os, arch, extension)` in the naming nodejs.org uses.
///
/// The download is checked against these before anything is unpacked, so a
/// tampered or truncated archive never becomes the interpreter Zed runs agents
/// with. They are written out rather than fetched because a digest fetched
/// over the same connection proves nothing the connection does not already.
/// A platform missing from the table is downloaded unverified, with a warning
/// (this fork ships for the three platforms above plus the Linux runtime it
/// stages into WSL, so in practice only an unusual host lands there).
const PINNED_NODE_ARCHIVES: &[(&str, &str, &str, &str)] = &[
    (
        "linux",
        "x64",
        "tar.gz",
        "6e1db87ef58b8819e5d5402eff1536491b18edd8eb7bee5ef7897876e88dc5ff",
    ),
    (
        "linux",
        "arm64",
        "tar.gz",
        "724282c3b43aec998aa9527380465b45d229e021b58035f5f4f63095eabfe5d5",
    ),
    (
        "win",
        "x64",
        "zip",
        "158f7685b44de51f6c0df1d153526cbcd3e1bc739a8dfc607721cef75de9e541",
    ),
    (
        "win",
        "arm64",
        "zip",
        "8779b1bde1d39f8d420e3b57aa657b39891af434d3de44a919044cec06785921",
    ),
    (
        "darwin",
        "arm64",
        "tar.gz",
        "bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057",
    ),
    (
        "darwin",
        "x64",
        "tar.gz",
        "1462cb3b3046b815cf8ea436d3da450ec1a9f11dac7e5a46b0ada5305d7e8097",
    ),
];

/// ADR-F49 決定 21: one [`NODE_VERSION`] archive and the digest it has to have.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedNodeArchive {
    /// `node-<version>-<os>-<arch>.<extension>`, which is also the name of the
    /// single directory a `tar.gz` unpacks into.
    pub file_name: String,
    pub url: String,
    /// Lowercase hex.
    pub sha256: &'static str,
    /// Whether the archive is a `zip` rather than a `tar.gz`.
    pub is_zip: bool,
}

impl PinnedNodeArchive {
    /// The archive's single top-level directory, which is also the name of the
    /// installation directory Zed keeps it in.
    pub fn directory_name(&self) -> &str {
        let extension = if self.is_zip { ".zip" } else { ".tar.gz" };
        self.file_name
            .strip_suffix(extension)
            .unwrap_or(&self.file_name)
    }
}

/// ADR-F49 決定 21: the pinned archive for `os` / `arch` in nodejs.org's own
/// naming (`linux`/`win`/`darwin` and `x64`/`arm64`), or `None` for a platform
/// this fork has no digest for.
///
/// Public so the Windows client can obtain the **Linux** build for the WSL
/// distro it runs agents in (決定 20), which is not the platform it is itself.
pub fn pinned_node_archive(os: &str, arch: &str) -> Option<PinnedNodeArchive> {
    let (_, _, extension, sha256) = PINNED_NODE_ARCHIVES
        .iter()
        .find(|(entry_os, entry_arch, _, _)| *entry_os == os && *entry_arch == arch)?;
    let file_name = format!("node-{NODE_VERSION}-{os}-{arch}.{extension}");
    Some(PinnedNodeArchive {
        url: format!("https://nodejs.org/dist/{NODE_VERSION}/{file_name}"),
        file_name,
        sha256,
        is_zip: *extension == "zip",
    })
}

/// Downloads `url` into `destination`, never holding it in memory, and — when
/// `expected_sha256` is given — refusing it unless the bytes hash to exactly
/// that (ADR-F49 決定 21). A mismatch removes the file and says what the user
/// can do about it; nothing is unpacked from an archive that failed. Returns
/// the digest of what was written, which a caller with no digest of its own
/// (a registry archive published without one, 決定 25) uses to check the copy
/// it then makes into a WSL distro.
///
/// Public because the Windows client stages the same kind of archive — the
/// Linux Node.js and a registry entry's Linux build — into the distro it runs
/// agents in (ADR-F49 第 2b 段).
pub async fn download_verified_file(
    http: &dyn HttpClient,
    url: &str,
    expected_sha256: Option<&str>,
    destination: &Path,
) -> Result<String> {
    use futures::AsyncWriteExt as _;
    use sha2::{Digest as _, Sha256};

    let mut response = http
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        i18n::t!(
            "Could not download {url} (HTTP {status}).",
            status = response.status()
        )
    );

    // Streamed through a hasher into the file, so neither the archive nor its
    // digest is ever held whole in memory.
    let write = async {
        let mut file = fs::File::create(destination)
            .await
            .with_context(|| format!("creating {}", destination.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let body = response.body_mut();
        loop {
            let read = body
                .read(&mut buffer)
                .await
                .with_context(|| format!("downloading {url}"))?;
            if read == 0 {
                break;
            }
            let chunk = buffer.get(..read).context("bug: read past the buffer")?;
            hasher.update(chunk);
            file.write_all(chunk)
                .await
                .with_context(|| format!("writing {}", destination.display()))?;
        }
        file.flush()
            .await
            .with_context(|| format!("writing {}", destination.display()))?;
        anyhow::Ok(
            hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
    }
    .await;

    let digest = match write {
        Ok(digest) => digest,
        Err(error) => {
            fs::remove_file(destination).await.ok();
            return Err(error);
        }
    };
    if let Some(expected) = expected_sha256
        && !digest.eq_ignore_ascii_case(expected)
    {
        fs::remove_file(destination).await.ok();
        bail!(i18n::t!(
            "The SHA-256 of the file downloaded from {url} does not match the expected one \
             (expected {expected}, got {digest}). It may have been tampered with, so the \
             downloaded file was deleted. Check the network path (proxies, corporate TLS \
             interception) and try again."
        ));
    }
    Ok(digest)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NodeBinaryOptions {
    pub allow_path_lookup: bool,
    pub allow_binary_download: bool,
    pub use_paths: Option<(PathBuf, PathBuf)>,
}

/// Use this when you need to launch npm as a long-lived process (for example, an agent server),
/// so the invocation and environment stay consistent with the Node runtime's proxy and CA setup.
#[derive(Clone, Debug)]
pub struct NpmCommand {
    pub path: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Which proxy an npm invocation is pointed at.
///
/// ADR-F14 (F2-7): an AI agent harness is installed and run inside the fork's
/// egress gate, so its npm traffic — and the traffic of whatever npm shells out
/// to — has to be judged by the same allowlist as the app's own requests.
/// Everything else npm is used for here (language servers, formatters, debug
/// adapters, extensions) sits outside that gate and keeps reaching the registry
/// the way it did before ADR-F14: routing it through the allowlist would deny
/// `registry.npmjs.org`, which the ADR never meant to cover.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NpmProxyRouting {
    /// Only the user's configured `http.proxy`, if any.
    Direct,
    /// The fork's child-process egress proxy while it is running, which chains
    /// onward to the user's configured proxy rather than around it.
    ThroughEgressProxy,
}

pub enum VersionStrategy<'a> {
    /// Install if current version doesn't match pinned version
    Pin(&'a Version),
    /// Install if current version is older than latest version
    Latest(&'a Version),
}

#[derive(Clone)]
pub struct NodeRuntime(Arc<Mutex<NodeRuntimeState>>);

struct NodeRuntimeState {
    http: Arc<dyn HttpClient>,
    instance: Option<Box<dyn NodeRuntimeTrait>>,
    last_options: Option<NodeBinaryOptions>,
    options: watch::Receiver<Option<NodeBinaryOptions>>,
    shell_env_loaded: Shared<oneshot::Receiver<()>>,
}

impl NodeRuntime {
    pub fn new(
        http: Arc<dyn HttpClient>,
        shell_env_loaded: Option<oneshot::Receiver<()>>,
        options: watch::Receiver<Option<NodeBinaryOptions>>,
    ) -> Self {
        NodeRuntime(Arc::new(Mutex::new(NodeRuntimeState {
            http,
            instance: None,
            last_options: None,
            options,
            shell_env_loaded: shell_env_loaded.unwrap_or(oneshot::channel().1).shared(),
        })))
    }

    pub fn unavailable() -> Self {
        NodeRuntime(Arc::new(Mutex::new(NodeRuntimeState {
            http: Arc::new(http_client::BlockedHttpClient),
            instance: None,
            last_options: None,
            options: watch::channel(Some(NodeBinaryOptions::default())).1,
            shell_env_loaded: oneshot::channel().1.shared(),
        })))
    }

    async fn instance(&self) -> Box<dyn NodeRuntimeTrait> {
        let mut state = self.0.lock().await;

        let options = loop {
            if let Some(options) = state.options.borrow().as_ref() {
                break options.clone();
            }
            match state.options.changed().await {
                Ok(()) => {}
                // failure case not cached
                Err(err) => {
                    return Box::new(UnavailableNodeRuntime {
                        error_message: err.to_string().into(),
                    }) as Box<dyn NodeRuntimeTrait>;
                }
            }
        };

        if state.last_options.as_ref() != Some(&options) {
            state.instance.take();
        }
        if let Some(instance) = state.instance.as_ref() {
            return instance.boxed_clone();
        }

        if let Some((node, npm)) = options.use_paths.as_ref() {
            let instance = match SystemNodeRuntime::new(node.clone(), npm.clone()).await {
                Ok(instance) => {
                    log::info!("using Node.js from `node.path` in settings: {:?}", instance);
                    Box::new(instance)
                }
                Err(err) => {
                    // failure case not cached, since it's cheap to check again
                    return Box::new(UnavailableNodeRuntime {
                        error_message: format!(
                            "failure checking Node.js from `node.path` in settings ({}): {:?}",
                            node.display(),
                            err
                        )
                        .into(),
                    });
                }
            };
            state.instance = Some(instance.boxed_clone());
            state.last_options = Some(options);
            return instance;
        }

        let system_node_error = if options.allow_path_lookup {
            state.shell_env_loaded.clone().await.ok();
            match SystemNodeRuntime::detect().await {
                Ok(instance) => {
                    log::info!("using Node.js found on PATH: {:?}", instance);
                    state.instance = Some(instance.boxed_clone());
                    state.last_options = Some(options);
                    return Box::new(instance) as Box<dyn NodeRuntimeTrait>;
                }
                Err(err) => Some(err),
            }
        } else {
            None
        };

        let instance = if options.allow_binary_download {
            let (log_level, why_using_managed) = match system_node_error {
                Some(err @ DetectError::Other(_)) => (Level::Warn, err.to_string()),
                Some(err @ DetectError::NotInPath(_)) => (Level::Info, err.to_string()),
                None => (
                    Level::Info,
                    "`node.ignore_system_version` is `true` in settings".to_string(),
                ),
            };
            match ManagedNodeRuntime::install_if_needed(&state.http).await {
                Ok(instance) => {
                    log::log!(
                        log_level,
                        "using Zed managed Node.js at {} since {}",
                        instance.installation_path.display(),
                        why_using_managed
                    );
                    Box::new(instance) as Box<dyn NodeRuntimeTrait>
                }
                Err(err) => {
                    // failure case is cached, since downloading + installing may be expensive. The
                    // downside of this is that it may fail due to an intermittent network issue.
                    //
                    // TODO: Have `install_if_needed` indicate which failure cases are retryable
                    // and/or have shared tracking of when internet is available.
                    Box::new(UnavailableNodeRuntime {
                        error_message: format!(
                            "failure while downloading and/or installing Zed managed Node.js, \
                            restart Zed to retry: {}",
                            err
                        )
                        .into(),
                    }) as Box<dyn NodeRuntimeTrait>
                }
            }
        } else if let Some(system_node_error) = system_node_error {
            // failure case not cached, since it's cheap to check again
            //
            // TODO: When support is added for setting `options.allow_binary_download`, update this
            // error message.
            return Box::new(UnavailableNodeRuntime {
                error_message: format!(
                    "failure while checking system Node.js from PATH: {}",
                    system_node_error
                )
                .into(),
            });
        } else {
            // failure case is cached because it will always happen with these options
            //
            // TODO: When support is added for setting `options.allow_binary_download`, update this
            // error message.
            Box::new(UnavailableNodeRuntime {
                error_message: "`node` settings do not allow any way to use Node.js"
                    .to_string()
                    .into(),
            })
        };

        state.instance = Some(instance.boxed_clone());
        state.last_options = Some(options);
        instance
    }

    pub async fn binary_path(&self) -> Result<PathBuf> {
        self.instance().await.binary_path()
    }

    pub async fn run_npm_subcommand(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output> {
        self.run_npm_subcommand_with_proxy_routing(
            directory,
            subcommand,
            args,
            NpmProxyRouting::Direct,
        )
        .await
    }

    /// For the one npm invocation that happens inside the fork's egress gate:
    /// installing an AI agent harness. See [`NpmProxyRouting`].
    pub async fn run_npm_subcommand_through_egress_proxy(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output> {
        self.run_npm_subcommand_with_proxy_routing(
            directory,
            subcommand,
            args,
            NpmProxyRouting::ThroughEgressProxy,
        )
        .await
    }

    async fn run_npm_subcommand_with_proxy_routing(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        args: &[&str],
        proxy_routing: NpmProxyRouting,
    ) -> Result<Output> {
        let http = self.0.lock().await.http.clone();
        self.instance()
            .await
            .run_npm_subcommand(directory, http.proxy(), proxy_routing, subcommand, args)
            .await
    }

    pub async fn npm_package_installed_version(
        &self,
        local_package_directory: &Path,
        name: &str,
    ) -> Result<Option<Version>> {
        self.instance()
            .await
            .npm_package_installed_version(local_package_directory, name)
            .await
    }

    pub async fn npm_command(
        &self,
        prefix_dir: Option<&Path>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<NpmCommand> {
        let http = self.0.lock().await.http.clone();
        self.instance()
            .await
            .npm_command(
                prefix_dir,
                http.proxy(),
                NpmProxyRouting::Direct,
                subcommand,
                args,
            )
            .await
    }

    pub async fn npm_package_latest_version(&self, name: &str) -> Result<Version> {
        self.npm_package_latest_version_with_requirement(name, None)
            .await
    }

    pub async fn npm_package_latest_version_with_requirement(
        &self,
        name: &str,
        version_requirement: Option<&VersionReq>,
    ) -> Result<Version> {
        let http = self.0.lock().await.http.clone();
        let instance = self.instance().await;
        let output = instance
            .run_npm_subcommand(
                None,
                http.proxy(),
                NpmProxyRouting::Direct,
                "info",
                &[
                    name,
                    "--json",
                    "--fetch-retry-mintimeout",
                    "2000",
                    "--fetch-retry-maxtimeout",
                    "5000",
                    "--fetch-timeout",
                    "5000",
                ],
            )
            .await?;

        let info: NpmInfo = deserialize_npm_info_from_response(&output.stdout).map_err(|e| {
            anyhow::anyhow!(
                "failed to parse npm info response: {e}\nstdout: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })?;
        let before = npm_config_before(instance.as_ref(), http.proxy(), NpmProxyRouting::Direct)
            .await
            .context("getting npm before config")
            .log_err()
            .flatten();
        let latest_dist_tag = info.dist_tags.latest.clone();
        let selected_version =
            select_npm_package_version(name, info, before.as_deref(), version_requirement)?;
        log::debug!(
            "selected latest npm package version package={name:?} version_requirement={version_requirement:?} before={before:?} dist_tag_latest={latest_dist_tag:?} selected={selected_version}"
        );
        Ok(selected_version)
    }

    pub async fn npm_install_packages(
        &self,
        directory: &Path,
        packages: &[(&str, &str)],
    ) -> Result<()> {
        if packages.is_empty() {
            return Ok(());
        }

        log::debug!(
            "installing npm packages directory={} packages={packages:?}",
            directory.display()
        );

        let packages: Vec<_> = packages
            .iter()
            .map(|(name, version)| format!("{name}@{version}"))
            .collect();

        let arguments: Vec<_> = packages
            .iter()
            .map(|p| p.as_str())
            .chain([
                "--no-package-lock",
                "--save-exact",
                "--fetch-retry-mintimeout",
                "2000",
                "--fetch-retry-maxtimeout",
                "5000",
                "--fetch-timeout",
                "5000",
            ])
            .collect();

        // This is also wrong because the directory is wrong.
        self.run_npm_subcommand(Some(directory), "install", &arguments)
            .await?;
        Ok(())
    }

    pub async fn npm_install_latest_packages(
        &self,
        directory: &Path,
        package_names: &[&str],
    ) -> Result<()> {
        // Let npm apply user config such as `before` and `min-release-age` during resolution.
        log::debug!(
            "installing latest npm packages directory={} packages={package_names:?}",
            directory.display()
        );
        let packages = package_names
            .iter()
            .map(|package_name| (*package_name, "latest"))
            .collect::<Vec<_>>();
        self.npm_install_packages(directory, &packages).await
    }

    pub async fn should_install_npm_package(
        &self,
        package_name: &str,
        local_executable_path: &Path,
        local_package_directory: &Path,
        version_strategy: VersionStrategy<'_>,
    ) -> bool {
        // In the case of the local system not having the package installed,
        // or in the instances where we fail to parse package.json data,
        // we attempt to install the package.
        if fs::metadata(local_executable_path).await.is_err() {
            log::debug!(
                "npm package cache miss package={package_name:?} reason=missing-executable executable={}",
                local_executable_path.display()
            );
            return true;
        }

        let Some(installed_version) = self
            .npm_package_installed_version(local_package_directory, package_name)
            .await
            .log_err()
            .flatten()
        else {
            log::debug!(
                "npm package cache miss package={package_name:?} reason=missing-installed-version package_dir={}",
                local_package_directory.display()
            );
            return true;
        };

        let version_strategy_label = match &version_strategy {
            VersionStrategy::Pin(version) => format!("pin:{version}"),
            VersionStrategy::Latest(version) => format!("latest:{version}"),
        };
        let should_install =
            should_install_npm_package_version(&installed_version, version_strategy);
        log::debug!(
            "npm package cache check package={package_name:?} installed={installed_version} strategy={version_strategy_label} should_install={should_install}"
        );
        should_install
    }
}

fn should_install_npm_package_version(
    installed_version: &Version,
    version_strategy: VersionStrategy<'_>,
) -> bool {
    match version_strategy {
        VersionStrategy::Pin(pinned_version) => installed_version != pinned_version,
        VersionStrategy::Latest(latest_version) => installed_version < latest_version,
    }
}

enum ArchiveType {
    TarGz,
    Zip,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct NpmInfo {
    #[serde(default)]
    dist_tags: NpmInfoDistTags,
    versions: Vec<Version>,
    #[serde(default, deserialize_with = "deserialize_npm_info_time")]
    time: HashMap<String, String>,
}

/// Parse NpmInfo from npm info --json output, handling both v11 and >= v12 formats.
fn deserialize_npm_info_from_response(data: &[u8]) -> Result<NpmInfo, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_slice(data)?;

    // npm >= 12 returns an array with one object: [ { ... } ]
    if let serde_json::Value::Array(arr) = &value {
        if arr.len() == 1 {
            return NpmInfo::deserialize(&arr[0]);
        }
    }

    // npm <= v11 returns a bare JSON object: { ... }
    NpmInfo::deserialize(value)
}

#[derive(Debug, Deserialize, Default)]
pub struct NpmInfoDistTags {
    latest: Option<Version>,
}

// Some registries put non-string values in the `time` map: JFrog Artifactory emits
// `"unpublished": null`, and npm itself reports `unpublished` as an object when a
// package has had versions unpublished. Only version keys map to the RFC 3339 strings
// we read, so keep the string entries and drop the rest rather than failing to parse
// the entire `npm info` response (which would block language server installation).
fn deserialize_npm_info_time<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let entries = HashMap::<String, serde_json::Value>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(value) => Some((key, value)),
            _ => None,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct NpmConfig {
    #[serde(default)]
    before: Option<String>,
}

async fn npm_config_before(
    node_runtime: &dyn NodeRuntimeTrait,
    proxy: Option<&Url>,
    proxy_routing: NpmProxyRouting,
) -> Result<Option<String>> {
    // `npm config get before` renders Date values for display. The JSON config output keeps the
    // computed cutoff in the same ISO format used by `npm info --json` release times.
    let output = node_runtime
        .run_npm_subcommand(None, proxy, proxy_routing, "config", &["list", "--json"])
        .await?;
    let config: NpmConfig = serde_json::from_slice(&output.stdout)?;
    Ok(config
        .before
        .filter(|before| !before.trim().is_empty() && before != "null"))
}

fn select_npm_package_version(
    package_name: &str,
    mut info: NpmInfo,
    before: Option<&str>,
    version_requirement: Option<&VersionReq>,
) -> Result<Version> {
    if let Some(version_requirement) = version_requirement {
        info.versions
            .retain(|version| version_requirement.matches(version));
        info.versions.sort();
        info.dist_tags.latest = info
            .dist_tags
            .latest
            .take()
            .filter(|version| version_requirement.matches(version));
    }

    if let Some(before) = before
        && !info.time.is_empty()
    {
        let before_timestamp = DateTime::parse_from_rfc3339(before)
            .with_context(|| format!("parsing npm before config timestamp {before:?}"))?
            .with_timezone(&Utc);
        let latest_version = info.dist_tags.latest.as_ref();

        if let Some(version) = latest_version
            && npm_version_was_published_before(version, &info.time, &before_timestamp)?
        {
            return Ok(version.clone());
        }

        for version in info.versions.iter().rev() {
            if is_allowed_npm_version_before(
                version,
                latest_version,
                &info.time,
                &before_timestamp,
                version_requirement.is_some(),
            )? {
                return Ok(version.clone());
            }
        }

        bail!("no version found for npm package {package_name} before {before}");
    }

    info.dist_tags
        .latest
        .or_else(|| info.versions.pop())
        .with_context(|| format!("no version found for npm package {package_name}"))
}

fn is_allowed_npm_version_before(
    version: &Version,
    latest_version: Option<&Version>,
    published_at_by_version: &HashMap<String, String>,
    before: &DateTime<Utc>,
    allow_prereleases: bool,
) -> Result<bool> {
    if (!allow_prereleases && !version.pre.is_empty())
        || latest_version.is_some_and(|latest_version| version > latest_version)
    {
        return Ok(false);
    }

    npm_version_was_published_before(version, published_at_by_version, before)
}

fn npm_version_was_published_before(
    version: &Version,
    published_at_by_version: &HashMap<String, String>,
    before: &DateTime<Utc>,
) -> Result<bool> {
    let Some(published_at) = published_at_by_version.get(&version.to_string()) else {
        return Ok(false);
    };
    let published_at = DateTime::parse_from_rfc3339(published_at)
        .with_context(|| format!("parsing npm release timestamp for version {version}"))?
        .with_timezone(&Utc);
    Ok(&published_at <= before)
}

#[async_trait::async_trait]
trait NodeRuntimeTrait: Send + Sync {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait>;
    fn binary_path(&self) -> Result<PathBuf>;

    async fn run_npm_subcommand(
        &self,
        directory: Option<&Path>,
        proxy: Option<&Url>,
        proxy_routing: NpmProxyRouting,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output>;

    async fn npm_command(
        &self,
        prefix_dir: Option<&Path>,
        proxy: Option<&Url>,
        proxy_routing: NpmProxyRouting,
        subcommand: &str,
        args: &[&str],
    ) -> Result<NpmCommand>;

    async fn npm_package_installed_version(
        &self,
        local_package_directory: &Path,
        name: &str,
    ) -> Result<Option<Version>>;
}

#[derive(Clone)]
struct ManagedNodeRuntime {
    installation_path: PathBuf,
}

impl ManagedNodeRuntime {
    const VERSION: &str = NODE_VERSION;

    #[cfg(not(windows))]
    const NODE_PATH: &str = "bin/node";
    #[cfg(windows)]
    const NODE_PATH: &str = "node.exe";

    #[cfg(not(windows))]
    const NPM_PATH: &str = "bin/npm";
    #[cfg(windows)]
    const NPM_PATH: &str = "node_modules/npm/bin/npm-cli.js";

    async fn install_if_needed(http: &Arc<dyn HttpClient>) -> Result<Self> {
        log::info!("Node runtime install_if_needed");

        let os = match consts::OS {
            "macos" => "darwin",
            "linux" => "linux",
            "windows" => "win",
            other => bail!("Running on unsupported os: {other}"),
        };

        let arch = match consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => bail!("Running on unsupported architecture: {other}"),
        };

        let version = Self::VERSION;
        let folder_name = format!("node-{version}-{os}-{arch}");
        let node_containing_dir = paths::data_dir().join("node");
        let node_dir = node_containing_dir.join(folder_name);
        let node_binary = node_dir.join(Self::NODE_PATH);
        let npm_file = node_dir.join(Self::NPM_PATH);
        let node_ca_certs = env::var(NODE_CA_CERTS_ENV_VAR).unwrap_or_else(|_| String::new());

        let valid = if fs::metadata(&node_binary).await.is_ok() {
            let result = util::command::new_command(&node_binary)
                .env(NODE_CA_CERTS_ENV_VAR, node_ca_certs)
                .arg(npm_file)
                .arg("--version")
                .args(["--cache".into(), node_dir.join("cache")])
                .args(["--userconfig".into(), node_dir.join("blank_user_npmrc")])
                .args(["--globalconfig".into(), node_dir.join("blank_global_npmrc")])
                .output()
                .await;
            match result {
                Ok(output) => {
                    if output.status.success() {
                        true
                    } else {
                        log::warn!(
                            "Zed managed Node.js binary at {} failed check with output: {:?}",
                            node_binary.display(),
                            output
                        );
                        false
                    }
                }
                Err(err) => {
                    log::warn!(
                        "Zed managed Node.js binary at {} failed check, so re-downloading it. \
                        Error: {}",
                        node_binary.display(),
                        err
                    );
                    false
                }
            }
        } else {
            false
        };

        if !valid {
            _ = fs::remove_dir_all(&node_containing_dir).await;
            fs::create_dir(&node_containing_dir)
                .await
                .context("error creating node containing dir")?;

            let archive_type = match consts::OS {
                "macos" | "linux" => ArchiveType::TarGz,
                "windows" => ArchiveType::Zip,
                other => bail!("Running on unsupported os: {other}"),
            };

            let version = Self::VERSION;
            let file_name = format!(
                "node-{version}-{os}-{arch}.{extension}",
                extension = match archive_type {
                    ArchiveType::TarGz => "tar.gz",
                    ArchiveType::Zip => "zip",
                }
            );

            // ADR-F49 決定 21: downloaded to a file and checked against the
            // digest this build pins before a single entry is unpacked — what
            // comes out of this archive is the interpreter Zed runs agents
            // with. A platform with no pinned digest is downloaded as before,
            // saying so.
            let pinned = pinned_node_archive(os, arch);
            let url = match &pinned {
                Some(pinned) => pinned.url.clone(),
                None => format!("https://nodejs.org/dist/{version}/{file_name}"),
            };
            if pinned.is_none() {
                log::warn!(
                    "no pinned SHA-256 for Node.js {version} on {os}-{arch}; \
                     downloading {url} without verifying it"
                );
            }
            let archive_path = node_containing_dir.join(format!("{file_name}.download"));
            log::info!("Downloading Node.js binary from {url}");
            download_verified_file(
                http.as_ref(),
                &url,
                pinned.as_ref().map(|pinned| pinned.sha256),
                &archive_path,
            )
            .await?;
            log::info!("Download of Node.js complete, extracting...");

            let archive_file = fs::File::open(&archive_path)
                .await
                .with_context(|| format!("opening {}", archive_path.display()))?;
            let extracted = match archive_type {
                ArchiveType::TarGz => {
                    let decompressed_bytes = GzipDecoder::new(BufReader::new(archive_file));
                    let archive = Archive::new(decompressed_bytes);
                    archive.unpack(&node_containing_dir).await
                }
                ArchiveType::Zip => extract_zip(&node_containing_dir, archive_file)
                    .await
                    .map_err(|error| io::Error::other(format!("{error:#}"))),
            };
            fs::remove_file(&archive_path).await.log_err();
            extracted.context("extracting the Node.js archive")?;
            log::info!("Extracted Node.js to {}", node_containing_dir.display())
        }

        _ = fs::remove_dir_all(node_dir.join("cache")).await;

        // Note: Not in the `if !valid {}` so we can populate these for existing installations
        _ = fs::create_dir(node_dir.join("cache")).await;
        _ = fs::write(node_dir.join("blank_user_npmrc"), []).await;
        _ = fs::write(node_dir.join("blank_global_npmrc"), []).await;

        anyhow::Ok(ManagedNodeRuntime {
            installation_path: node_dir,
        })
    }
}

fn path_with_node_binary_prepended(node_binary: &Path) -> Option<OsString> {
    let existing_path = env::var_os("PATH");
    let node_bin_dir = node_binary.parent().map(|dir| dir.as_os_str());
    match (existing_path, node_bin_dir) {
        (Some(existing_path), Some(node_bin_dir)) => {
            if let Ok(joined) = env::join_paths(
                [PathBuf::from(node_bin_dir)]
                    .into_iter()
                    .chain(env::split_paths(&existing_path)),
            ) {
                Some(joined)
            } else {
                Some(existing_path)
            }
        }
        (Some(existing_path), None) => Some(existing_path),
        (None, Some(node_bin_dir)) => Some(node_bin_dir.to_owned()),
        _ => None,
    }
}

#[async_trait::async_trait]
impl NodeRuntimeTrait for ManagedNodeRuntime {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait> {
        Box::new(self.clone())
    }

    fn binary_path(&self) -> Result<PathBuf> {
        Ok(self.installation_path.join(Self::NODE_PATH))
    }

    async fn run_npm_subcommand(
        &self,
        directory: Option<&Path>,
        proxy: Option<&Url>,
        proxy_routing: NpmProxyRouting,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output> {
        let attempt = || async {
            let npm_command = self
                .npm_command(directory, proxy, proxy_routing, subcommand, args)
                .await?;
            let mut command = util::command::new_command(npm_command.path);
            command.args(npm_command.args);
            command.envs(npm_command.env);
            if let Some(directory) = directory {
                command.current_dir(directory);
            }
            command.output().await.map_err(|e| anyhow!("{e}"))
        };

        let mut output = attempt().await;
        if output.is_err() {
            output = attempt().await;
            anyhow::ensure!(
                output.is_ok(),
                "failed to launch npm subcommand {subcommand} subcommand\nerr: {:?}",
                output.err()
            );
        }

        if let Ok(output) = &output {
            anyhow::ensure!(
                output.status.success(),
                "failed to execute npm {subcommand} subcommand:\nstdout: {:?}\nstderr: {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        output.map_err(|e| anyhow!("{e}"))
    }

    async fn npm_command(
        &self,
        prefix_dir: Option<&Path>,
        proxy: Option<&Url>,
        proxy_routing: NpmProxyRouting,
        subcommand: &str,
        args: &[&str],
    ) -> Result<NpmCommand> {
        let node_binary = self.installation_path.join(Self::NODE_PATH);
        let npm_file = self.installation_path.join(Self::NPM_PATH);

        anyhow::ensure!(
            smol::fs::metadata(&node_binary).await.is_ok(),
            "missing node binary file"
        );
        anyhow::ensure!(
            smol::fs::metadata(&npm_file).await.is_ok(),
            "missing npm file"
        );

        let command_args = build_npm_command_args(
            Some(&npm_file),
            prefix_dir,
            &self.installation_path.join("cache"),
            Some(&self.installation_path.join("blank_user_npmrc")),
            Some(&self.installation_path.join("blank_global_npmrc")),
            proxy,
            proxy_routing,
            subcommand,
            args,
        );
        let command_env = npm_command_env(&node_binary, proxy_routing);

        Ok(NpmCommand {
            path: node_binary,
            args: command_args,
            env: command_env,
        })
    }

    async fn npm_package_installed_version(
        &self,
        local_package_directory: &Path,
        name: &str,
    ) -> Result<Option<Version>> {
        read_package_installed_version(local_package_directory.join("node_modules"), name).await
    }
}

/// A system Node.js executable whose version has been checked.
///
/// Discovery does not require npm, initialize a cache, or download a runtime.
#[derive(Debug, Clone)]
pub struct SystemNode {
    path: PathBuf,
    version: Version,
}

/// A failure to locate or validate a system Node.js executable.
#[derive(Debug, thiserror::Error)]
pub enum NodeDiscoveryError {
    #[error("Could not read the current directory while looking for Node.js: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error(
        "Node.js executable {path:?} was not found. Install Node.js {minimum} or newer, or configure its executable path.",
        minimum = SystemNode::MIN_VERSION,
    )]
    NotFound {
        path: PathBuf,
        #[source]
        source: which::Error,
    },
    #[error(
        "Could not run Node.js at {path:?}: {source}. Check that the configured executable can run."
    )]
    Probe {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "Node.js at {path:?} failed its version check ({status}). Check the Node.js installation.\nstdout: {stdout}\nstderr: {stderr}",
        status = .output.status,
        stdout = String::from_utf8_lossy(&.output.stdout),
        stderr = String::from_utf8_lossy(&.output.stderr),
    )]
    UnsuccessfulExit { path: PathBuf, output: Output },
    #[error(
        "Node.js at {path:?} reported an invalid version: {output:?}. Configure a Node.js {minimum} or newer executable.",
        minimum = SystemNode::MIN_VERSION,
    )]
    InvalidVersion {
        path: PathBuf,
        output: String,
        #[source]
        source: semver::Error,
    },
    #[error(
        "Node.js at {path:?} is too old ({version}). Install Node.js {minimum} or newer, or configure a newer executable.",
        minimum = SystemNode::MIN_VERSION,
    )]
    TooOld { path: PathBuf, version: Version },
}

impl SystemNode {
    const MIN_VERSION: Version = Version::new(22, 0, 0);

    /// Finds and validates Node.js using an explicit executable or search path.
    ///
    /// An explicit executable takes precedence and never falls back to another
    /// candidate on failure. Otherwise the first executable named `node` in
    /// `search_path` is checked, preserving PATH order even if it is too old.
    /// `None` disables PATH lookup; it does not read the process's PATH.
    ///
    /// A configured basename is also looked up in `search_path`; use `./name`
    /// to select one in the current directory. Relative paths containing
    /// separators are resolved against that directory. On Windows, lookup
    /// honors PATHEXT, including batch-file wrappers.
    ///
    /// The caller must finish loading its login-shell environment before calling
    /// this method. The version probe inherits that environment, with PATH
    /// replaced by `search_path` when supplied. Discovery does not launch a login
    /// shell to obtain the environment.
    ///
    /// # Errors
    ///
    /// Returns a typed failure if lookup or execution fails, the version cannot
    /// be parsed, or the executable reports a version older than 22.0.0.
    pub async fn discover(
        configured_path: Option<PathBuf>,
        search_path: Option<OsString>,
    ) -> std::result::Result<Self, NodeDiscoveryError> {
        let path = smol::unblock({
            let search_path = search_path.clone();
            move || {
                find_node_path(
                    configured_path.as_deref(),
                    search_path.as_deref(),
                    Path::new("."),
                )
            }
        })
        .await?;
        let mut command = util::command::new_command(&path);
        command.arg("--version").kill_on_drop(true);
        if let Some(search_path) = search_path {
            command.env("PATH", search_path);
        }
        let output = command
            .output()
            .await
            .map_err(|source| NodeDiscoveryError::Probe {
                path: path.clone(),
                source,
            })?;
        let version = check_node_version(&path, output)?;
        Ok(Self { path, version })
    }

    /// Returns the absolute executable path without resolving symlinks.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the version reported by the executable.
    pub fn version(&self) -> &Version {
        &self.version
    }
}

fn find_node_path(
    configured_path: Option<&Path>,
    search_path: Option<&OsStr>,
    directory: &Path,
) -> std::result::Result<PathBuf, NodeDiscoveryError> {
    let path = configured_path.unwrap_or_else(|| Path::new("node"));
    let path = which::which_in(path, search_path, directory).map_err(|source| {
        NodeDiscoveryError::NotFound {
            path: path.to_path_buf(),
            source,
        }
    })?;
    // The cwd may have been deleted even when Node's absolute path still works.
    // Only relative results need it, but those must be anchored before launch
    // so the command cannot search PATH a second time.
    std::path::absolute(directory.join(path)).map_err(NodeDiscoveryError::CurrentDirectory)
}

fn check_node_version(
    path: &Path,
    output: Output,
) -> std::result::Result<Version, NodeDiscoveryError> {
    if !output.status.success() {
        return Err(NodeDiscoveryError::UnsuccessfulExit {
            path: path.to_path_buf(),
            output,
        });
    }
    let version_string = String::from_utf8_lossy(&output.stdout);
    let version =
        Version::parse(version_string.trim().trim_start_matches('v')).map_err(|source| {
            NodeDiscoveryError::InvalidVersion {
                path: path.to_path_buf(),
                output: version_string.into_owned(),
                source,
            }
        })?;
    if version < SystemNode::MIN_VERSION {
        return Err(NodeDiscoveryError::TooOld {
            path: path.to_path_buf(),
            version,
        });
    }
    Ok(version)
}

#[derive(Debug, Clone)]
pub struct SystemNodeRuntime {
    node: PathBuf,
    npm: PathBuf,
    scratch_dir: PathBuf,
}

impl SystemNodeRuntime {
    async fn new(node: PathBuf, npm: PathBuf) -> Result<Self> {
        let node = SystemNode::discover(Some(node), env::var_os("PATH"))
            .await?
            .path;
        let scratch_dir = paths::data_dir().join("node");
        fs::create_dir(&scratch_dir).await.ok();
        _ = fs::remove_dir_all(scratch_dir.join("cache")).await;
        fs::create_dir(scratch_dir.join("cache")).await.ok();

        Ok(Self {
            node,
            npm,
            scratch_dir,
        })
    }

    async fn detect() -> std::result::Result<Self, DetectError> {
        let node = which::which("node").map_err(DetectError::NotInPath)?;
        let npm = which::which("npm").map_err(DetectError::NotInPath)?;
        Self::new(node, npm).await.map_err(DetectError::Other)
    }
}

enum DetectError {
    NotInPath(which::Error),
    Other(anyhow::Error),
}

impl Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectError::NotInPath(err) => {
                write!(f, "system Node.js wasn't found on PATH: {}", err)
            }
            DetectError::Other(err) => {
                write!(f, "checking system Node.js failed with error: {}", err)
            }
        }
    }
}

#[async_trait::async_trait]
impl NodeRuntimeTrait for SystemNodeRuntime {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait> {
        Box::new(self.clone())
    }

    fn binary_path(&self) -> Result<PathBuf> {
        Ok(self.node.clone())
    }

    async fn run_npm_subcommand(
        &self,
        directory: Option<&Path>,
        proxy: Option<&Url>,
        proxy_routing: NpmProxyRouting,
        subcommand: &str,
        args: &[&str],
    ) -> anyhow::Result<Output> {
        let npm_command = self
            .npm_command(directory, proxy, proxy_routing, subcommand, args)
            .await?;
        let mut command = util::command::new_command(npm_command.path);
        command.args(npm_command.args);
        command.envs(npm_command.env);
        if let Some(directory) = directory {
            command.current_dir(directory);
        }
        let output = command.output().await?;
        anyhow::ensure!(
            output.status.success(),
            "failed to execute npm {subcommand} subcommand:\nstdout: {:?}\nstderr: {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output)
    }

    async fn npm_command(
        &self,
        prefix_dir: Option<&Path>,
        proxy: Option<&Url>,
        proxy_routing: NpmProxyRouting,
        subcommand: &str,
        args: &[&str],
    ) -> Result<NpmCommand> {
        let command_args = build_npm_command_args(
            None,
            prefix_dir,
            &self.scratch_dir.join("cache"),
            None,
            None,
            proxy,
            proxy_routing,
            subcommand,
            args,
        );
        let command_env = npm_command_env(&self.node, proxy_routing);

        Ok(NpmCommand {
            path: self.npm.clone(),
            args: command_args,
            env: command_env,
        })
    }

    async fn npm_package_installed_version(
        &self,
        local_package_directory: &Path,
        name: &str,
    ) -> Result<Option<Version>> {
        read_package_installed_version(local_package_directory.join("node_modules"), name).await
        // todo: allow returning a globally installed version (requires callers not to hard-code the path)
    }
}

pub async fn read_package_installed_version(
    node_module_directory: PathBuf,
    name: &str,
) -> Result<Option<Version>> {
    let package_json_path = node_module_directory.join(name).join("package.json");

    let mut file = match fs::File::open(package_json_path).await {
        Ok(file) => file,
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }

            Err(err)?
        }
    };

    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    Ok(Some(package_json_version(&contents)?))
}

/// The `version` an npm package's `package.json` declares.
///
/// ADR-F49 第 2b 段: split out of [`read_package_installed_version`] so a
/// Windows client can apply the same rule to a `package.json` it read out of a
/// WSL distro, where there is no local file for it to open.
pub fn package_json_version(package_json: &str) -> Result<Version> {
    #[derive(Deserialize)]
    struct PackageJson {
        version: Version,
    }

    let parsed: PackageJson = serde_json::from_str(package_json)?;
    Ok(parsed.version)
}

pub async fn read_package_executable(
    node_module_directory: PathBuf,
    name: &str,
) -> Result<PathBuf> {
    let package_directory = node_module_directory.join(name);

    let package_json_path = package_directory.join("package.json");
    let mut file = fs::File::open(&package_json_path)
        .await
        .with_context(|| format!("opening {}", package_json_path.display()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let relative_path = package_json_executable(&contents, name)
        .with_context(|| format!("reading {}", package_json_path.display()))?;

    Ok(package_directory.join(relative_path))
}

/// The executable an npm package's `package.json` declares, relative to the
/// package's own directory: its single `bin`, the one named after the package,
/// or the only one it declares.
///
/// ADR-F49 第 2b 段: split out of [`read_package_executable`] so a Windows
/// client can apply the same rule to a `package.json` it read out of a WSL
/// distro — where it must run nothing of the package's to find out — and so
/// the rule itself is unit tested.
pub fn package_json_executable(package_json: &str, name: &str) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Bin {
        Path(String),
        Named(HashMap<String, String>),
    }

    #[derive(Deserialize)]
    struct PackageJson {
        bin: Option<Bin>,
    }

    let parsed: PackageJson = serde_json::from_str(package_json).context("parsing package.json")?;

    match parsed.bin {
        Some(Bin::Path(path)) => Ok(path),
        Some(Bin::Named(bins)) => {
            let unscoped_name = name.rsplit('/').next().unwrap_or(name);
            let path = if bins.len() == 1 {
                bins.values().next()
            } else {
                bins.get(unscoped_name)
            };
            Ok(path
                .with_context(|| {
                    format!("npm package {name} declares no executable named {unscoped_name}")
                })?
                .clone())
        }
        None => bail!("npm package {name} declares no executable"),
    }
}

#[derive(Clone)]
pub struct UnavailableNodeRuntime {
    error_message: Arc<String>,
}

#[async_trait::async_trait]
impl NodeRuntimeTrait for UnavailableNodeRuntime {
    fn boxed_clone(&self) -> Box<dyn NodeRuntimeTrait> {
        Box::new(self.clone())
    }
    fn binary_path(&self) -> Result<PathBuf> {
        bail!("{}", self.error_message)
    }

    async fn run_npm_subcommand(
        &self,
        _: Option<&Path>,
        _: Option<&Url>,
        _: NpmProxyRouting,
        _: &str,
        _: &[&str],
    ) -> anyhow::Result<Output> {
        bail!("{}", self.error_message)
    }

    async fn npm_command(
        &self,
        _: Option<&Path>,
        _proxy: Option<&Url>,
        _proxy_routing: NpmProxyRouting,
        _subcommand: &str,
        _args: &[&str],
    ) -> Result<NpmCommand> {
        bail!("{}", self.error_message)
    }

    async fn npm_package_installed_version(
        &self,
        _local_package_directory: &Path,
        _: &str,
    ) -> Result<Option<Version>> {
        bail!("{}", self.error_message)
    }
}

fn proxy_argument(proxy_routing: NpmProxyRouting, proxy: Option<&Url>) -> Option<String> {
    let child_process_proxy = match proxy_routing {
        NpmProxyRouting::ThroughEgressProxy => http_client::child_process_proxy(),
        NpmProxyRouting::Direct => None,
    };
    npm_proxy_argument(child_process_proxy, proxy)
}

/// ADR-F14 (F2-7): for the invocations that [`NpmProxyRouting`] places inside
/// the egress gate, npm's own requests go through the fork's child-process
/// proxy instead of straight out, so that `registry.npmjs.org` — a hole in
/// ADR-F03 precisely because npm is a child process — is judged by the same
/// allowlist as everything else. Nothing is lost by preferring it: the fork's
/// proxy chains through `configured` rather than around it.
///
/// Split from [`proxy_argument`] so it can be tested without the process-wide
/// endpoint slot, which the tests here would otherwise race each other over.
fn npm_proxy_argument(
    child_process_proxy: Option<Url>,
    configured: Option<&Url>,
) -> Option<String> {
    let mut proxy = match child_process_proxy {
        Some(endpoint) => endpoint,
        None => configured.cloned()?,
    };
    // Map proxy settings from `http://localhost:10809` to `http://127.0.0.1:10809`
    // NodeRuntime without environment information can not parse `localhost`
    // correctly.
    // TODO: map to `[::1]` if we are using ipv6
    if matches!(proxy.host(), Some(Host::Domain(domain)) if domain.eq_ignore_ascii_case("localhost"))
    {
        // When localhost is a valid Host, so is `127.0.0.1`
        let _ = proxy.set_ip_host(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    Some(proxy.as_str().to_string())
}

fn build_npm_command_args(
    entrypoint: Option<&Path>,
    prefix_dir: Option<&Path>,
    cache_dir: &Path,
    user_config: Option<&Path>,
    global_config: Option<&Path>,
    proxy: Option<&Url>,
    proxy_routing: NpmProxyRouting,
    subcommand: &str,
    args: &[&str],
) -> Vec<String> {
    let mut command_args = Vec::new();
    if let Some(entrypoint) = entrypoint {
        command_args.push(entrypoint.to_string_lossy().into_owned());
    }
    if let Some(prefix_dir) = prefix_dir {
        command_args.push("--prefix".into());
        command_args.push(prefix_dir.to_string_lossy().into_owned());
    }
    command_args.push(subcommand.to_string());
    command_args.push(format!("--cache={}", cache_dir.display()));
    if let Some(user_config) = user_config {
        command_args.push("--userconfig".into());
        command_args.push(user_config.to_string_lossy().into_owned());
    }
    if let Some(global_config) = global_config {
        command_args.push("--globalconfig".into());
        command_args.push(global_config.to_string_lossy().into_owned());
    }
    if let Some(proxy_arg) = proxy_argument(proxy_routing, proxy) {
        command_args.push("--proxy".into());
        command_args.push(proxy_arg);
    }
    command_args.extend(args.into_iter().map(|a| a.to_string()));
    command_args
}

pub fn npm_command_env(
    node_binary: &Path,
    proxy_routing: NpmProxyRouting,
) -> HashMap<String, String> {
    let mut command_env = HashMap::new();
    let env_path = path_with_node_binary_prepended(node_binary).unwrap_or_default();
    command_env.insert("PATH".into(), env_path.to_string_lossy().into_owned());

    // ADR-F14 (F2-7). `--proxy` (see `proxy_argument`) covers npm's own
    // requests; these cover what npm shells out to — lifecycle scripts, and
    // the `npx` package an ACP agent is installed as when it later runs. Scoped
    // the same way as `--proxy`: only the invocations inside the egress gate.
    if proxy_routing == NpmProxyRouting::ThroughEgressProxy {
        http_client::apply_child_process_proxy_env(&mut command_env);
    }

    if let Ok(node_ca_certs) = env::var(NODE_CA_CERTS_ENV_VAR) {
        if !node_ca_certs.is_empty() {
            command_env.insert(NODE_CA_CERTS_ENV_VAR.to_string(), node_ca_certs);
        }
    }

    #[cfg(windows)]
    {
        if let Some(val) = env::var("SYSTEMROOT")
            .context("Missing environment variable: SYSTEMROOT!")
            .log_err()
        {
            command_env.insert("SYSTEMROOT".into(), val);
        }
        if let Some(val) = env::var("ComSpec")
            .context("Missing environment variable: ComSpec!")
            .log_err()
        {
            command_env.insert("ComSpec".into(), val);
        }
    }

    command_env
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process::{ExitStatus, Output},
    };

    use anyhow::{Result, bail};
    use http_client::Url;
    use semver::{Version, VersionReq};

    use super::{
        NodeDiscoveryError, NpmInfo, NpmProxyRouting, VersionStrategy, build_npm_command_args,
        check_node_version, deserialize_npm_info_from_response, find_node_path, npm_command_env,
        npm_proxy_argument, proxy_argument, select_npm_package_version,
        should_install_npm_package_version,
    };

    #[test]
    fn the_child_process_egress_proxy_wins_over_the_configured_one() {
        // ADR-F14: for an invocation inside the egress gate, npm's own requests
        // must go through the fork's proxy, which chains onward to the user's,
        // rather than skipping the allowlist.
        let egress = Url::parse("http://127.0.0.1:54321/").unwrap();
        let corporate = Url::parse("http://corp.example:3128/").unwrap();

        assert_eq!(
            npm_proxy_argument(Some(egress.clone()), Some(&corporate)).as_deref(),
            Some("http://127.0.0.1:54321/")
        );
        assert_eq!(
            npm_proxy_argument(Some(egress), None).as_deref(),
            Some("http://127.0.0.1:54321/")
        );
        // With the feature off, the configured proxy is used exactly as before.
        assert_eq!(
            npm_proxy_argument(None, Some(&corporate)).as_deref(),
            Some("http://corp.example:3128/")
        );
        assert_eq!(npm_proxy_argument(None, None), None);
    }

    /// The only test here that touches the process-wide endpoint slot; the rest
    /// route [`NpmProxyRouting::Direct`], which never reads it.
    #[test]
    fn only_the_agent_install_path_is_routed_through_the_egress_proxy() {
        // ADR-F14, scoped: a language server's npm invocation must not be
        // pointed at the allowlist proxy, which would deny `registry.npmjs.org`
        // and fail the install for a feature the egress gate does not cover.
        let endpoint = Url::parse("http://127.0.0.1:54321/").unwrap();
        http_client::set_child_process_proxy(Some(endpoint));

        let language_server_args = build_npm_command_args(
            None,
            None,
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            NpmProxyRouting::Direct,
            "install",
            &["yaml-language-server@latest"],
        );
        assert!(!language_server_args.iter().any(|arg| arg == "--proxy"));

        let agent_install_args = build_npm_command_args(
            None,
            None,
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            NpmProxyRouting::ThroughEgressProxy,
            "install",
            &["some-acp-agent"],
        );
        let proxy_position = agent_install_args
            .iter()
            .position(|arg| arg == "--proxy")
            .expect("the agent install path keeps the egress proxy argument");
        assert_eq!(
            agent_install_args
                .get(proxy_position + 1)
                .map(String::as_str),
            Some("http://127.0.0.1:54321/")
        );

        let node_binary = Path::new("/usr/bin/node");
        assert!(
            !npm_command_env(node_binary, NpmProxyRouting::Direct).contains_key("HTTP_PROXY"),
            "the egress proxy must not leak into a generic npm environment"
        );
        assert_eq!(
            npm_command_env(node_binary, NpmProxyRouting::ThroughEgressProxy)
                .get("HTTP_PROXY")
                .map(String::as_str),
            Some("http://127.0.0.1:54321/")
        );

        http_client::set_child_process_proxy(None);
    }

    #[test]
    fn test_node_lookup_distinguishes_basenames_and_relative_paths() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let binary_directory = directory.path().join("bin");
        fs::create_dir(&binary_directory)?;
        let filename = format!("node{}", env::consts::EXE_SUFFIX);
        let node = binary_directory.join(&filename);
        fs::write(&node, b"not executed by lookup tests")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&node, fs::Permissions::from_mode(0o755))?;
        }
        for (configured_path, search_path) in [
            (PathBuf::from(&filename), Some(binary_directory.as_os_str())),
            (Path::new("bin").join(&filename), None),
        ] {
            assert_eq!(
                find_node_path(Some(&configured_path), search_path, directory.path())?,
                node
            );
        }
        Ok(())
    }

    #[test]
    fn test_node_version_validation() -> Result<()> {
        let path = Path::new("node");
        for (stdout, expected) in [
            ("v22.0.0\n", Version::new(22, 0, 0)),
            (" \t22.1.0\r\n", Version::new(22, 1, 0)),
        ] {
            assert_eq!(check_node_version(path, node_output(stdout))?, expected);
        }
        for version in ["21.7.3", "22.0.0-rc.1"] {
            let error = check_node_version(path, node_output(&format!("v{version}\n")))
                .expect_err("unsupported version");
            assert!(
                matches!(&error, NodeDiscoveryError::TooOld { path: actual_path, version: actual_version }
                    if actual_path == path && actual_version == &Version::parse(version)?)
            );
            assert!(
                error
                    .to_string()
                    .contains("Install Node.js 22.0.0 or newer")
            );
        }
        let stdout = "not node\n";
        assert!(matches!(
            check_node_version(path, node_output(stdout)),
            Err(NodeDiscoveryError::InvalidVersion { path: actual_path, output, .. })
                if actual_path == path && output == stdout
        ));
        Ok(())
    }

    // Map localhost to 127.0.0.1
    // NodeRuntime without environment information can not parse `localhost` correctly.
    #[test]
    fn test_proxy_argument_map_localhost_proxy() {
        const CASES: [(&str, &str); 4] = [
            // Map localhost to 127.0.0.1
            ("http://localhost:9090/", "http://127.0.0.1:9090/"),
            ("https://google.com/", "https://google.com/"),
            (
                "http://username:password@proxy.thing.com:8080/",
                "http://username:password@proxy.thing.com:8080/",
            ),
            // Test when localhost is contained within a different part of the URL
            (
                "http://username:localhost@localhost:8080/",
                "http://username:localhost@127.0.0.1:8080/",
            ),
        ];

        for (proxy, mapped_proxy) in CASES {
            let proxy = Url::parse(proxy).unwrap();
            let proxy = proxy_argument(NpmProxyRouting::Direct, Some(&proxy))
                .expect("Proxy was not passed correctly");
            assert_eq!(
                proxy, mapped_proxy,
                "Incorrectly mapped localhost to 127.0.0.1"
            );
        }
    }

    #[test]
    fn test_build_npm_command_args_inserts_prefix_before_subcommand() {
        let args = build_npm_command_args(
            None,
            Some(Path::new("/tmp/zed-prefix")),
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            NpmProxyRouting::Direct,
            "exec",
            &["--yes", "--", "agent-package"],
        );

        assert_eq!(
            args,
            vec![
                "--prefix".to_string(),
                "/tmp/zed-prefix".to_string(),
                "exec".to_string(),
                "--cache=/tmp/cache".to_string(),
                "--yes".to_string(),
                "--".to_string(),
                "agent-package".to_string(),
            ]
        );
    }

    #[test]
    fn test_build_npm_command_args_keeps_entrypoint_before_prefix() {
        let args = build_npm_command_args(
            Some(Path::new("/tmp/npm-cli.js")),
            Some(Path::new("/tmp/zed-prefix")),
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            NpmProxyRouting::Direct,
            "exec",
            &["--yes"],
        );

        assert_eq!(
            args,
            vec![
                "/tmp/npm-cli.js".to_string(),
                "--prefix".to_string(),
                "/tmp/zed-prefix".to_string(),
                "exec".to_string(),
                "--cache=/tmp/cache".to_string(),
                "--yes".to_string(),
            ]
        );
    }

    #[test]
    fn test_latest_version_strategy_accepts_newer_installed_versions() -> Result<()> {
        let target_version = Version::parse("2.0.0")?;

        assert!(!should_install_npm_package_version(
            &Version::parse("2.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));
        assert!(should_install_npm_package_version(
            &Version::parse("1.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));
        assert!(!should_install_npm_package_version(
            &Version::parse("3.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));

        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_uses_dist_tag_without_before() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_npm_package_version("test-package", info, None, None)?,
            Version::parse("3.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_npm_info_skips_non_string_time_entries() -> Result<()> {
        // Registries such as JFrog Artifactory include `"unpublished": null` in `time`;
        // parsing must tolerate this rather than rejecting the whole response.
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0"],
                "time": {
                    "unpublished": null,
                    "created": "2024-01-01T00:00:00.000Z",
                    "modified": "2024-02-01T00:00:00.000Z",
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                None
            )?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_uses_latest_before_npm_before_config() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                None
            )?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_keeps_allowed_latest_dist_tag() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                None
            )?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_keeps_allowed_prerelease_latest_dist_tag() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0-beta.1" },
                "versions": ["1.0.0", "2.0.0-beta.1"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0-beta.1": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                None
            )?,
            Version::parse("2.0.0-beta.1")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_ignores_prereleases_before_cutoff() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0-beta.1", "2.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0-beta.1": "2024-02-01T00:00:00.000Z",
                    "2.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                None
            )?,
            Version::parse("1.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_ignores_versions_above_latest_dist_tag() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-03-01T00:00:00.000Z",
                    "3.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                None
            )?,
            Version::parse("1.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_errors_when_no_version_matches_before() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        let Err(error) = select_npm_package_version(
            "test-package",
            info,
            Some("2023-12-01T00:00:00.000Z"),
            None,
        ) else {
            bail!("expected cutoff to reject all package versions");
        };
        assert_eq!(
            error.to_string(),
            "no version found for npm package test-package before 2023-12-01T00:00:00.000Z"
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_selects_latest_matching_requirement() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["6.0.3", "7.0.0", "5.9.3", "6.0.2"]
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        assert_eq!(
            select_npm_package_version("test-package", info, None, Some(&version_requirement))?,
            Version::parse("6.0.3")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_applies_before_to_matching_versions() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["6.0.3", "7.0.0", "6.0.2"],
                "time": {
                    "6.0.2": "2024-02-01T00:00:00.000Z",
                    "6.0.3": "2024-03-01T00:00:00.000Z",
                    "7.0.0": "2024-04-01T00:00:00.000Z"
                }
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                Some(&version_requirement),
            )?,
            Version::parse("6.0.2")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_allows_requested_prerelease_before_cutoff() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["7.1.0-beta.1", "7.1.0-beta.2", "7.0.0"],
                "time": {
                    "7.0.0": "2024-01-01T00:00:00.000Z",
                    "7.1.0-beta.1": "2024-02-01T00:00:00.000Z",
                    "7.1.0-beta.2": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;
        let version_requirement = VersionReq::parse(">=7.1.0-beta.1, <7.1.0")?;

        assert_eq!(
            select_npm_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                Some(&version_requirement),
            )?,
            Version::parse("7.1.0-beta.1")?
        );
        Ok(())
    }

    #[test]
    fn test_select_npm_package_version_errors_without_matching_version() -> Result<()> {
        let info: NpmInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["5.9.3", "7.0.0"]
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        let error =
            select_npm_package_version("test-package", info, None, Some(&version_requirement))
                .expect_err("expected version requirement to reject all package versions");
        assert_eq!(
            error.to_string(),
            "no version found for npm package test-package"
        );
        Ok(())
    }

    #[test]
    fn test_pinned_version_strategy_replaces_different_installed_version() -> Result<()> {
        let pinned_version = Version::parse("6.0.3")?;

        assert!(!should_install_npm_package_version(
            &pinned_version,
            VersionStrategy::Pin(&pinned_version)
        ));
        assert!(should_install_npm_package_version(
            &Version::parse("7.0.0")?,
            VersionStrategy::Pin(&pinned_version)
        ));
        Ok(())
    }

    #[test]
    fn test_deserialize_npm_info_npm11_format() -> Result<()> {
        let json = r#"{
            "dist-tags": { "latest": "3.0.0" },
            "versions": ["1.0.0", "2.0.0", "3.0.0"]
        }"#;

        let info = deserialize_npm_info_from_response(json.as_bytes())?;
        assert_eq!(info.dist_tags.latest, Some(Version::parse("3.0.0")?));
        assert_eq!(
            info.versions,
            vec![
                Version::parse("1.0.0")?,
                Version::parse("2.0.0")?,
                Version::parse("3.0.0")?
            ]
        );
        Ok(())
    }

    #[test]
    fn test_deserialize_npm_v12_format() -> Result<()> {
        let json = r#"[
            {
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"]
            }
        ]"#;

        let info = deserialize_npm_info_from_response(json.as_bytes())?;
        assert_eq!(info.dist_tags.latest, Some(Version::parse("3.0.0")?));
        assert_eq!(
            info.versions,
            vec![
                Version::parse("1.0.0")?,
                Version::parse("2.0.0")?,
                Version::parse("3.0.0")?
            ]
        );
        Ok(())
    }

    /// ADR-F49 決定 21: every platform this fork ships for, plus the Linux
    /// build it stages into WSL, has a pinned archive whose name and URL are
    /// the ones nodejs.org publishes for the pinned version.
    #[test]
    fn every_shipped_platform_has_a_pinned_node_archive() {
        for (os, arch, extension) in [
            ("linux", "x64", "tar.gz"),
            ("linux", "arm64", "tar.gz"),
            ("win", "x64", "zip"),
            ("win", "arm64", "zip"),
            ("darwin", "x64", "tar.gz"),
            ("darwin", "arm64", "tar.gz"),
        ] {
            let archive = super::pinned_node_archive(os, arch)
                .unwrap_or_else(|| panic!("no pinned archive for {os}-{arch}"));
            assert_eq!(
                archive.file_name,
                format!("node-{}-{os}-{arch}.{extension}", super::NODE_VERSION)
            );
            assert_eq!(
                archive.url,
                format!(
                    "https://nodejs.org/dist/{}/{}",
                    super::NODE_VERSION,
                    archive.file_name
                )
            );
            assert_eq!(archive.is_zip, extension == "zip");
            assert_eq!(
                archive.directory_name(),
                format!("node-{}-{os}-{arch}", super::NODE_VERSION)
            );
            assert_eq!(archive.sha256.len(), 64);
            assert!(
                archive
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "{os}-{arch} has a malformed digest"
            );
        }
        assert!(super::pinned_node_archive("linux", "riscv64").is_none());
        assert!(super::pinned_node_archive("plan9", "x64").is_none());
    }

    /// The archive Zed installs for this very platform is one of the pinned
    /// ones, so the check is never silently skipped on a platform we ship.
    #[test]
    fn this_platform_has_a_pinned_node_archive() {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => other,
        };
        assert!(
            super::pinned_node_archive(os, arch).is_some(),
            "no pinned Node.js archive for {os}-{arch}"
        );
    }

    /// ADR-F49 第 2b 段: the rule the Windows client applies to a
    /// `package.json` it read out of a WSL distro is the one
    /// `read_package_executable` applies locally.
    #[test]
    fn a_package_json_names_its_executable() {
        assert_eq!(
            super::package_json_executable(r#"{"bin": "./dist/cli.js"}"#, "some-package")
                .expect("a single bin path"),
            "./dist/cli.js"
        );
        assert_eq!(
            super::package_json_executable(
                r#"{"bin": {"claude-agent-acp": "bin/acp.js"}}"#,
                "@agentclientprotocol/claude-agent-acp"
            )
            .expect("the only named bin"),
            "bin/acp.js"
        );
        assert_eq!(
            super::package_json_executable(
                r#"{"bin": {"other": "bin/other.js", "pkg": "bin/pkg.js"}}"#,
                "@scope/pkg"
            )
            .expect("the bin named after the unscoped package"),
            "bin/pkg.js"
        );
        assert!(
            super::package_json_executable(r#"{"bin": {"a": "bin/a.js", "b": "bin/b.js"}}"#, "pkg")
                .is_err(),
            "a package with several bins and none of its own name has no executable"
        );
        assert!(
            super::package_json_executable(r#"{"version": "1.0.0"}"#, "pkg").is_err(),
            "a package with no bin has no executable"
        );
        assert_eq!(
            super::package_json_version(r#"{"version": "0.76.1"}"#)
                .expect("a version")
                .to_string(),
            "0.76.1"
        );
    }

    fn node_output(stdout: &str) -> Output {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }
}
