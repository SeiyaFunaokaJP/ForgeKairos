//! The Windows installer, the Explorer context menu package and the scripts
//! that build them cannot read `branding`, so they spell its names out. These
//! tests read those files and hold them to `branding::CURRENT`, and keep them
//! from claiming names an official Zed install owns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use branding::{CURRENT, ChannelNames, LEGACY};
use sha2::{Digest as _, Sha256};

const CHANNELS: [&str; 4] = ["dev", "nightly", "preview", "stable"];

/// The CLSIDs upstream's context menu package registers.
const UPSTREAM_CLSIDS: [&str; 4] = [
    "6a1f6b13-3b82-48a1-9e06-7bb0a6d0bffd",
    "af8e85ea-fb20-4db2-93cf-56513c1ec697",
    "266f2cfe-1653-42af-b55c-fe3590c83871",
    "685f4d49-6718-4c55-b271-ebb5c6a48d6f",
];

/// The installer's AppId for the stable channel. It keys the uninstall entry,
/// so changing it would install beside an existing install instead of
/// updating it.
const STABLE_APP_ID: &str = "{{9924BCC9-2563-4389-B8F4-37F62D0954C7}";

fn repository_file(relative: &str) -> (PathBuf, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    (path, text)
}

fn channel_name(names: &ChannelNames, channel: &str) -> &'static str {
    match channel {
        "dev" => names.dev,
        "nightly" => names.nightly,
        "preview" => names.preview,
        "stable" => names.stable,
        other => panic!("unknown channel {other}"),
    }
}

/// The `$name = "value"` assignments of one channel's block in
/// `BuildInstaller` in `script/bundle-windows.ps1`.
fn installer_variables(channel: &str) -> BTreeMap<String, String> {
    let (_, script) = repository_file("script/bundle-windows.ps1");
    let build_installer = script
        .split_once("function BuildInstaller {")
        .expect("bundle-windows.ps1 defines BuildInstaller")
        .1;
    let block = build_installer
        .split_once(&format!("        \"{channel}\" {{\n"))
        .unwrap_or_else(|| panic!("BuildInstaller has a {channel} block"))
        .1;
    let block = block
        .split_once("\n        }")
        .expect("the channel block is closed")
        .0;
    block
        .lines()
        .filter_map(|line| {
            let (name, value) = line.trim().strip_prefix('$')?.split_once(" = ")?;
            Some((name.to_string(), value.trim_matches('"').to_string()))
        })
        .collect()
}

fn installer_variable(channel: &str, name: &str) -> String {
    installer_variables(channel)
        .remove(name)
        .unwrap_or_else(|| panic!("the {channel} block of BuildInstaller sets ${name}"))
}

fn manifest_file_name(channel: &str) -> &'static str {
    match channel {
        "dev" => "AppxManifest-Dev.xml",
        "nightly" => "AppxManifest-Nightly.xml",
        "preview" => "AppxManifest-Preview.xml",
        _ => "AppxManifest.xml",
    }
}

fn manifest(channel: &str) -> String {
    repository_file(&format!(
        "crates/explorer_command_injector/{}",
        manifest_file_name(channel)
    ))
    .1
}

/// Every value of `attribute="..."` in `text`.
fn attribute_values<'a>(text: &'a str, attribute: &str) -> Vec<&'a str> {
    let needle = format!(" {attribute}=\"");
    text.split(needle.as_str())
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(value, _)| value))
        .collect()
}

fn single_attribute<'a>(text: &'a str, attribute: &str) -> &'a str {
    let values = attribute_values(text, attribute);
    assert_eq!(values.len(), 1, "{attribute} appears once: {values:?}");
    values[0]
}

/// The publisher ID Windows derives from a package's `Publisher`: the first
/// 8 bytes of the SHA-256 of its UTF-16LE encoding, in Crockford's base32.
fn publisher_id(publisher: &str) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let utf16: Vec<u8> = publisher
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let digest = Sha256::digest(&utf16);
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    // 64 bits padded to 65 make 13 groups of 5.
    let bits = u128::from(u64::from_be_bytes(prefix)) << 1;
    (0..13)
        .map(|group| {
            let index = (bits >> (60 - group * 5)) & 0b11111;
            char::from(ALPHABET[index as usize])
        })
        .collect()
}

/// The arms of a `cfg_select!` on the channel features in
/// `explorer_command_injector.rs`, keyed by channel (`_` is the dev build).
fn injector_arms(constant: &str) -> BTreeMap<&'static str, String> {
    let (_, source) =
        repository_file("crates/explorer_command_injector/src/explorer_command_injector.rs");
    let block = source
        .split_once(&format!("const {constant}: "))
        .unwrap_or_else(|| panic!("the injector defines {constant}"))
        .1;
    let block = block.split_once("};").expect("cfg_select! is closed").0;
    let mut arms = BTreeMap::new();
    for line in block.lines() {
        let line = line.trim();
        let channel = if line.starts_with("_ =>") {
            "dev"
        } else if let Some(rest) = line.strip_prefix("feature = \"") {
            let feature = rest.split_once('"').expect("a quoted feature").0;
            CHANNELS
                .into_iter()
                .find(|channel| *channel == feature)
                .unwrap_or_else(|| panic!("unknown feature {feature}"))
        } else {
            continue;
        };
        let value = line
            .split_once("=> {")
            .expect("an arm")
            .1
            .trim()
            .trim_end_matches(',')
            .trim_end_matches('}')
            .trim();
        arms.insert(channel, value.to_string());
    }
    assert_eq!(arms.len(), 4, "one {constant} per channel: {arms:?}");
    arms
}

fn injector_clsid(channel: &str) -> String {
    let arm = &injector_arms("MODULE_ID")[channel];
    let hex: String = arm
        .split_once("0x")
        .expect("a hexadecimal GUID")
        .1
        .chars()
        .take_while(|character| *character != ')')
        .filter(|character| *character != '_')
        .collect();
    assert_eq!(hex.len(), 32, "{arm}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
    .to_ascii_lowercase()
}

fn injector_registry_path(channel: &str) -> String {
    let arm = &injector_arms("REG_PATH")[channel];
    arm.trim_start_matches("r#\"")
        .trim_end_matches("\"#")
        .to_string()
}

#[test]
fn the_installer_uses_the_names_the_app_runs_under() {
    for channel in CHANNELS {
        assert_eq!(
            installer_variable(channel, "appMutex"),
            format!(
                "{}-Instance-Mutex",
                channel_name(&CURRENT.app_identifiers, channel)
            ),
            "{channel}: the installer waits on the mutex the running app holds"
        );
        assert!(
            LEGACY
                .iter()
                .any(|legacy| installer_variable(channel, "appLegacyMutex")
                    == format!(
                        "{}-Instance-Mutex",
                        channel_name(&legacy.app_identifiers, channel)
                    )),
            "{channel}: the legacy mutex is one an earlier build held"
        );
        assert_eq!(
            installer_variable(channel, "appUserId"),
            channel_name(&CURRENT.app_user_model_ids, channel),
            "{channel}: shortcuts carry the AppUserModelID the app sets"
        );
        assert_eq!(
            installer_variable(channel, "appExeName"),
            CURRENT.executable_stem,
            "{channel}"
        );
    }
    assert_eq!(installer_variable("stable", "appId"), STABLE_APP_ID);
}

#[test]
fn the_installer_ships_the_executables_under_their_names() {
    let (_, script) = repository_file("script/bundle-windows.ps1");
    let executable = format!("\"$innoDir\\{}.exe\"", CURRENT.executable_stem);
    assert!(
        script.contains(&format!("-Destination {executable}")),
        "the cargo binary is copied in as {executable}"
    );
    for launcher in [
        format!("\"$innoDir\\bin\\{}.exe\"", CURRENT.cli_name),
        format!("\"$innoDir\\bin\\{}\"", CURRENT.cli_name),
    ] {
        assert!(
            script.contains(&format!("-Destination {launcher}")),
            "the launcher is installed as {launcher}"
        );
    }

    // Installed as bin\{cli_name} for WSL and Git Bash, it runs the CLI next
    // to it.
    let (_, shell_launcher) = repository_file("crates/zed/resources/windows/zed.sh");
    let invocations: Vec<&str> = shell_launcher
        .split("\"$ZED_PATH/")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(file, _)| file))
        .collect();
    assert!(!invocations.is_empty(), "zed.sh runs the CLI");
    for invoked in invocations {
        assert_eq!(invoked, format!("{}.exe", CURRENT.cli_name));
    }
}

#[test]
fn the_context_menu_package_matches_the_installer_and_the_dll() {
    for channel in CHANNELS {
        let manifest = manifest(channel);
        let identity = manifest
            .split_once("<Identity")
            .and_then(|(_, rest)| rest.split_once("/>"))
            .expect("the manifest has an Identity")
            .0;
        let name = single_attribute(identity, "Name");
        let publisher = single_attribute(identity, "Publisher");
        let version = single_attribute(identity, "Version");
        assert_eq!(
            name,
            channel_name(&CURRENT.app_user_model_ids, channel),
            "{channel}"
        );
        assert_eq!(
            installer_variable(channel, "appAppxFullName"),
            format!("{name}_{version}_neutral__{}", publisher_id(publisher)),
            "{channel}: the installer removes the package by this full name"
        );
        assert_eq!(
            single_attribute(&manifest, "Executable"),
            format!("{}.exe", CURRENT.executable_stem),
            "{channel}"
        );

        let clsid = injector_clsid(channel);
        assert!(
            !UPSTREAM_CLSIDS.contains(&clsid.as_str()),
            "{channel}: {clsid} is upstream's, so it would collide with an official Zed's package"
        );
        let mut manifest_clsids = attribute_values(&manifest, "Clsid");
        manifest_clsids.extend(
            attribute_values(&manifest, "Id")
                .into_iter()
                .filter(|id| id.contains('-')),
        );
        assert_eq!(manifest_clsids.len(), 4, "{channel}: {manifest_clsids:?}");
        for manifest_clsid in manifest_clsids {
            assert_eq!(manifest_clsid, clsid, "{channel}");
        }

        assert_eq!(
            injector_registry_path(channel),
            format!(
                "Software\\Classes\\{}ContextMenu",
                installer_variable(channel, "regValueName")
            ),
            "{channel}: the DLL reads its title where the installer writes it"
        );
    }
    let mut clsids: Vec<String> = CHANNELS.into_iter().map(injector_clsid).collect();
    clsids.sort();
    clsids.dedup();
    assert_eq!(clsids.len(), 4, "every channel has its own CLSID");
}

#[test]
fn the_publisher_id_is_computed_the_way_windows_does() {
    // Upstream's package full name ends in this ID.
    assert_eq!(
        publisher_id("CN=Zed Industries Inc, O=Zed Industries Inc, L=Denver, S=Colorado, C=US"),
        "japxn1gcva8rg"
    );
}

#[test]
fn nothing_claims_names_an_official_zed_owns() {
    let (_, iss) = repository_file("crates/zed/resources/windows/zed.iss");
    // The [Code] section removes or restores these names for an install
    // updated from an earlier build; the sections before it are what gets
    // registered.
    let (registered, _) = iss.split_once("\n[Code]\n").expect("zed.iss has [Code]");
    let mut files = vec![
        repository_file("script/bundle-windows.ps1"),
        (
            PathBuf::from("zed.iss (before [Code])"),
            registered.to_string(),
        ),
    ];
    files.extend(CHANNELS.into_iter().map(|channel| {
        (
            PathBuf::from(manifest_file_name(channel)),
            manifest(channel),
        )
    }));
    for (path, text) in &files {
        for forbidden in [
            "ZedIndustries",
            "Zed Industries",
            "Software\\Classes\\zed\"",
            "Applications\\Zed.exe",
            "\"Zed.exe\"",
        ] {
            assert!(
                !text.contains(forbidden),
                "{} still contains {forbidden:?}",
                path.display()
            );
        }
    }
    for forbidden in ["ZedIndustries", "Zed Industries"] {
        assert!(
            !iss.contains(forbidden),
            "zed.iss still contains {forbidden:?}"
        );
    }

    let scheme = CURRENT.url_scheme;
    assert!(registered.contains(&format!("Subkey: \"Software\\Classes\\{scheme}\";")));
    assert!(registered.contains(&format!("\"URL:{scheme} Protocol\"")));
}

/// What remote connections upload to a host is looked for in `{app}\bin`
/// under the `CURRENT` names (crates/remote's `bundled_server_artifact_names`
/// and slirp4netns), and the download that would replace a missing one is
/// blocked. The installer build has to refuse to finish without them, and has
/// to check only after they are made and before the installer takes `bin\*`.
#[test]
fn the_installer_build_requires_what_remote_connections_upload() {
    let (_, script) = repository_file("script/bundle-windows.ps1");
    let check = script
        .split_once("function AssertBundledRemoteArtifacts {")
        .expect("bundle-windows.ps1 defines AssertBundledRemoteArtifacts")
        .1
        .split_once("\n}\n")
        .expect("AssertBundledRemoteArtifacts is closed")
        .0;
    let prefix = CURRENT.remote_server_binary_prefix;
    for name in [
        format!("{prefix}windows-$Architecture.zip"),
        format!("{prefix}linux-x86_64.gz"),
        "slirp4netns-linux-x86_64".to_string(),
    ] {
        let path = format!("\"$innoDir\\bin\\{name}\"");
        assert!(
            check.contains(&path),
            "AssertBundledRemoteArtifacts does not require {path}"
        );
        assert!(
            script.matches(&path).count() >= 2,
            "nothing in bundle-windows.ps1 writes {path}"
        );
    }

    let steps: Vec<&str> = script
        .split_once("\nCheckEnvironmentVariables\n")
        .expect("bundle-windows.ps1 runs its steps after CheckEnvironmentVariables")
        .1
        .lines()
        .map(str::trim)
        .collect();
    let position = |step: &str| {
        steps
            .iter()
            .position(|line| *line == step)
            .unwrap_or_else(|| panic!("bundle-windows.ps1 does not run {step}"))
    };
    let check_position = position("AssertBundledRemoteArtifacts");
    for producer in [
        "BuildRemoteServer",
        "BuildLinuxRemoteServer",
        "DownloadSlirp4netns",
        "CopyClangFormat",
    ] {
        assert!(
            position(producer) < check_position,
            "{producer} runs after AssertBundledRemoteArtifacts"
        );
    }
    assert!(check_position < position("BuildInstaller"));

    let (_, iss) = repository_file("crates/zed/resources/windows/zed.iss");
    assert!(
        iss.contains(r#"Source: "{#ResourcesDir}\bin\*"; DestDir: "{code:GetInstallDir}\bin""#),
        "zed.iss installs bin\\* into {{app}}\\bin"
    );
}

#[test]
fn every_installer_definition_zed_iss_uses_is_passed() {
    let (_, iss) = repository_file("crates/zed/resources/windows/zed.iss");
    let (_, script) = repository_file("script/bundle-windows.ps1");
    let definitions = script
        .split_once("$definitions = @{")
        .expect("bundle-windows.ps1 passes definitions")
        .1
        .split_once('}')
        .expect("the definitions are closed")
        .0;
    let defined: Vec<&str> = definitions
        .lines()
        .filter_map(|line| line.trim().strip_prefix('"')?.split_once('"'))
        .map(|(name, _)| name)
        .collect();
    let used = iss
        .split("{#")
        .skip(1)
        .filter_map(|rest| rest.split_once('}'))
        .map(|(name, _)| name)
        .chain(iss.lines().filter_map(|line| {
            line.strip_prefix("#if ")?
                .split_whitespace()
                .next()
                .filter(|name| name.chars().all(|character| character.is_alphanumeric()))
        }));
    for name in used {
        assert!(
            defined.contains(&name),
            "zed.iss uses {name}, which is not passed"
        );
    }
    for channel in CHANNELS {
        let variables = installer_variables(channel);
        for name in ["appLegacyMutex", "legacyRegValueName", "legacyAppName"] {
            assert!(variables.contains_key(name), "{channel} sets ${name}");
        }
    }
}

#[test]
fn the_development_context_menu_does_not_share_a_key_with_an_installer() {
    let (_, script) = repository_file("script/register-explorer-menu.ps1");
    let development_name = script
        .lines()
        .find_map(|line| line.trim().strip_prefix("$RegValueName = "))
        .expect("register-explorer-menu.ps1 sets $RegValueName")
        .trim_matches('"');
    for channel in CHANNELS {
        for variable in ["regValueName", "legacyRegValueName"] {
            let installer_name = installer_variable(channel, variable);
            // Registry key names are case-insensitive.
            assert!(
                !development_name.eq_ignore_ascii_case(&installer_name),
                "{development_name:?} is the {channel} installer's ${variable}"
            );
        }
    }
}
