//! The macOS bundle and the Linux packages must not present themselves under
//! an official Zed's identifiers, or the OS would treat the two as one app.

use branding::{ChannelNames, FORGEKAIROS};

const CARGO_TOML: &str = include_str!("../../Cargo.toml");
const DESKTOP_ENTRY: &str = include_str!("../../resources/zed.desktop.in");
const BUNDLE_LINUX: &str = include_str!("../../../../script/bundle-linux");
const BUNDLE_FLATPAK: &str = include_str!("../../../../script/flatpak/bundle-flatpak");
const FLATPAK_MANIFEST: &str = include_str!("../../resources/flatpak/manifest-template.json");
const FLATPAK_METAINFO: &str = include_str!("../../resources/flatpak/zed.metainfo.xml.in");
const SNAPCRAFT: &str = include_str!("../../resources/snap/snapcraft.yaml.in");
const SNAP_BUILD: &str = include_str!("../../../../script/snap-build");
const BUNDLE_MAC: &str = include_str!("../../../../script/bundle-mac");

fn channels(names: &ChannelNames) -> [(&'static str, &'static str); 4] {
    [
        ("dev", names.dev),
        ("nightly", names.nightly),
        ("preview", names.preview),
        ("stable", names.stable),
    ]
}

/// The `key = value` lines of one `[section]` of a TOML file.
fn toml_section(toml: &str, section: &str) -> Vec<(String, String)> {
    let header = format!("[{section}]");
    toml.lines()
        .skip_while(|line| line.trim() != header)
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn toml_value<'a>(section: &'a [(String, String)], key: &str) -> Option<&'a str> {
    section
        .iter()
        .find(|(found, _)| found == key)
        .map(|(_, value)| value.as_str())
}

#[test]
fn macos_bundles_use_the_new_identifiers_and_scheme() {
    for (channel, identifier) in channels(&FORGEKAIROS.app_ids) {
        let section_name = format!("package.metadata.bundle-{channel}");
        let section = toml_section(CARGO_TOML, &section_name);
        assert!(!section.is_empty(), "missing [{section_name}]");
        assert_eq!(
            toml_value(&section, "identifier"),
            Some(format!("\"{identifier}\"").as_str()),
            "[{section_name}] identifier"
        );
        assert_eq!(
            toml_value(&section, "osx_url_schemes"),
            Some(format!("[\"{}\"]", FORGEKAIROS.url_scheme).as_str()),
            "[{section_name}] osx_url_schemes"
        );
    }
    for (channel, display_name) in channels(&FORGEKAIROS.display_names) {
        let section_name = format!("package.metadata.bundle-{channel}");
        let section = toml_section(CARGO_TOML, &section_name);
        assert_eq!(
            toml_value(&section, "name"),
            Some(format!("\"{display_name}\"").as_str()),
            "[{section_name}] name"
        );
    }
}

#[test]
fn the_linux_bundle_installs_under_the_new_app_ids() {
    let app_ids: Vec<&str> = BUNDLE_LINUX
        .lines()
        .filter_map(|line| line.trim().strip_prefix("APP_ID=\""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .collect();
    let mut expected = FORGEKAIROS.app_ids.all().to_vec();
    let mut found = app_ids.clone();
    expected.sort_unstable();
    found.sort_unstable();
    assert_eq!(found, expected);
}

#[test]
fn the_desktop_entry_claims_the_new_url_scheme() {
    let mime_types = DESKTOP_ENTRY
        .lines()
        .find_map(|line| line.strip_prefix("MimeType="))
        .unwrap_or_default();
    let schemes: Vec<&str> = mime_types
        .split(';')
        .filter_map(|mime_type| mime_type.strip_prefix("x-scheme-handler/"))
        .collect();
    assert_eq!(schemes, [FORGEKAIROS.url_scheme]);
}

#[test]
fn no_linux_package_names_an_official_zed() {
    let files = [
        ("zed.desktop.in", DESKTOP_ENTRY),
        ("script/bundle-linux", BUNDLE_LINUX),
        ("script/flatpak/bundle-flatpak", BUNDLE_FLATPAK),
        ("flatpak/manifest-template.json", FLATPAK_MANIFEST),
        ("flatpak/zed.metainfo.xml.in", FLATPAK_METAINFO),
        ("snap/snapcraft.yaml.in", SNAPCRAFT),
        ("script/snap-build", SNAP_BUILD),
    ];
    for (file, contents) in files {
        for official in [
            "dev.zed.Zed",
            "x-scheme-handler/zed;",
            "zed.dev",
            "zed-industries/zed/issues",
            "zed-industries/zed/releases",
            "Zed Industries, Inc.",
        ] {
            assert!(
                !contents.contains(official),
                "{file} still contains {official:?}"
            );
        }
    }
}

/// Wayland's app_id and X11's WM_CLASS come from the running build, and the
/// desktop entry and the bundle install under `FORGEKAIROS.app_ids`: the two
/// must agree, or the window would not be matched to its launcher and icon.
#[test]
fn the_running_build_presents_the_installed_app_ids() {
    use release_channel::ReleaseChannel;
    for (channel, (_, app_id)) in [
        ReleaseChannel::Dev,
        ReleaseChannel::Nightly,
        ReleaseChannel::Preview,
        ReleaseChannel::Stable,
    ]
    .into_iter()
    .zip(channels(&FORGEKAIROS.app_ids))
    {
        assert_eq!(channel.app_id(), app_id, "{channel:?}");
    }
}

/// The updater downloads `forgekairos-macos-<arch>.dmg` from a release, so the
/// bundle has to be published under that name.
#[test]
fn the_macos_disk_image_has_the_name_the_updater_looks_for() {
    let expected = format!(
        "dmg_file_path=\"${{dmg_target_directory}}/{}-macos-${{arch_suffix}}.dmg\"",
        FORGEKAIROS.cli_name
    );
    assert!(
        BUNDLE_MAC.lines().any(|line| line.trim() == expected),
        "script/bundle-mac does not create {expected}"
    );
    for arch in ["x86_64", "aarch64"] {
        assert!(
            BUNDLE_MAC.contains(&format!("arch_suffix=\"{arch}\"")),
            "script/bundle-mac has no {arch} suffix"
        );
    }
}

/// The remote server these scripts publish beside the package is the release
/// asset the client downloads (`asset_name` in crates/auto_update) and the
/// name crates/remote looks for first, both named after `branding::CURRENT`.
#[test]
fn the_unix_bundles_publish_the_remote_server_under_the_new_name() {
    let prefix = branding::CURRENT.remote_server_binary_prefix;
    for (script, contents, expected) in [
        (
            "script/bundle-linux",
            BUNDLE_LINUX,
            format!("remote_server_artifact=\"{prefix}linux-${{arch}}.gz\""),
        ),
        (
            "script/bundle-mac",
            BUNDLE_MAC,
            format!("> target/{prefix}macos-$arch_suffix.gz"),
        ),
    ] {
        assert!(
            contents.contains(&expected),
            "{script} does not publish {expected}"
        );
        for legacy in branding::LEGACY {
            assert!(
                !contents.contains(legacy.remote_server_binary_prefix),
                "{script} still names a file {}...",
                legacy.remote_server_binary_prefix
            );
        }
    }
}
