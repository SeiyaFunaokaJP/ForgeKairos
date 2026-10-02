//! The names this application presents itself under, to the user and to the
//! operating system: the product name, the directories it keeps its data in,
//! the identifiers the OS groups its windows, instances and links by, and the
//! names it leaves on the hosts it connects to.
//!
//! Everything that spells one of these names reads it from [`CURRENT`], so that
//! renaming touches this file instead of every place a name is spelled. The
//! names this fork used before the rename stay in [`LEGACY`]: data has to be
//! moved out of those places, and the sandbox has to keep protecting them for
//! as long as an older build may still run from them.
//!
//! This crate has no dependencies so that the static remote-server helper
//! (`sandbox`) can use it.

/// One name per release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelNames {
    pub dev: &'static str,
    pub nightly: &'static str,
    pub preview: &'static str,
    pub stable: &'static str,
}

impl ChannelNames {
    pub const fn all(&self) -> [&'static str; 4] {
        [self.dev, self.nightly, self.preview, self.stable]
    }
}

/// A complete set of the names one build presents itself under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Names {
    /// The product name shown to the user, without a release channel.
    pub product_name: &'static str,
    /// The product name with the release channel, as in the About window.
    pub display_names: ChannelNames,
    /// The directory name inside the platform's config/data/state/cache
    /// directories (Windows and macOS spelling).
    pub app_directory_name: &'static str,
    /// [`Self::app_directory_name`] in lowercase, for the XDG directories.
    pub app_directory_name_lowercase: &'static str,
    /// The prefix of the Windows single-instance mutex and named pipe.
    pub app_identifiers: ChannelNames,
    /// The Wayland application ID / X11 `WM_CLASS`, which is also the macOS
    /// bundle identifier.
    pub app_ids: ChannelNames,
    /// The Windows AppUserModelID shortcuts, file associations and the
    /// Explorer context menu package are registered under.
    pub app_user_model_ids: ChannelNames,
    /// The Windows executable's file name without `.exe`.
    pub executable_stem: &'static str,
    /// The command-line launcher's name.
    pub cli_name: &'static str,
    /// The URL scheme registered with the operating system.
    pub url_scheme: &'static str,
    /// What every remote server binary installed on a remote host is named
    /// with first, whatever its channel and version.
    pub remote_server_binary_prefix: &'static str,
    /// The cache directory on a Linux host (the WSL distro or an SSH host),
    /// relative to the account's home: the sandbox helper, the staged agent
    /// binaries and the IDE bridge's rendezvous sockets.
    pub remote_cache_home_relative: &'static str,
    /// Where the sandboxed agent sessions keep their configuration on a Linux
    /// host, relative to the account's home.
    pub sandboxed_agent_config_home_relative: &'static str,
}

/// The names this fork used before it was renamed. Fields it took over from
/// upstream unchanged (the display names, the application IDs) are upstream's.
pub const ZED_JP_ENHANCE: Names = Names {
    product_name: "Zed",
    display_names: ChannelNames {
        dev: "Zed Dev",
        nightly: "Zed Nightly",
        preview: "Zed Preview",
        stable: "Zed",
    },
    app_directory_name: "Zed-JP-Enhance",
    app_directory_name_lowercase: "zed-jp-enhance",
    app_identifiers: ChannelNames {
        dev: "Zed-JP-Enhance-Editor-Dev",
        nightly: "Zed-JP-Enhance-Editor-Nightly",
        preview: "Zed-JP-Enhance-Editor-Preview",
        stable: "Zed-JP-Enhance-Editor-Stable",
    },
    app_ids: ChannelNames {
        dev: "dev.zed.Zed-Dev",
        nightly: "dev.zed.Zed-Nightly",
        preview: "dev.zed.Zed-Preview",
        stable: "dev.zed.Zed",
    },
    app_user_model_ids: ChannelNames {
        dev: "ZedIndustries.Zed.Dev",
        nightly: "ZedIndustries.Zed.Nightly",
        preview: "ZedIndustries.Zed.Preview",
        stable: "ZedIndustries.Zed",
    },
    executable_stem: "Zed",
    cli_name: "zed",
    url_scheme: "zed",
    remote_server_binary_prefix: "zed-remote-server-",
    remote_cache_home_relative: ".cache/zed-jp-enhance",
    sandboxed_agent_config_home_relative: ".zed-jp-enhance-claude",
};

/// The names after the rename.
pub const FORGEKAIROS: Names = Names {
    product_name: "ForgeKairos",
    display_names: ChannelNames {
        dev: "ForgeKairos Dev",
        nightly: "ForgeKairos Nightly",
        preview: "ForgeKairos Preview",
        stable: "ForgeKairos",
    },
    app_directory_name: "ForgeKairos",
    app_directory_name_lowercase: "forgekairos",
    app_identifiers: ChannelNames {
        dev: "ForgeKairos-Editor-Dev",
        nightly: "ForgeKairos-Editor-Nightly",
        preview: "ForgeKairos-Editor-Preview",
        stable: "ForgeKairos-Editor-Stable",
    },
    app_ids: ChannelNames {
        dev: "io.github.SeiyaFunaokaJP.ForgeKairos-Dev",
        nightly: "io.github.SeiyaFunaokaJP.ForgeKairos-Nightly",
        preview: "io.github.SeiyaFunaokaJP.ForgeKairos-Preview",
        stable: "io.github.SeiyaFunaokaJP.ForgeKairos",
    },
    app_user_model_ids: ChannelNames {
        dev: "SeiyaFunaokaJP.ForgeKairos.Dev",
        nightly: "SeiyaFunaokaJP.ForgeKairos.Nightly",
        preview: "SeiyaFunaokaJP.ForgeKairos.Preview",
        stable: "SeiyaFunaokaJP.ForgeKairos",
    },
    executable_stem: "ForgeKairos",
    cli_name: "forgekairos",
    url_scheme: "forgekairos",
    remote_server_binary_prefix: "forgekairos-remote-server-",
    remote_cache_home_relative: ".cache/forgekairos",
    sandboxed_agent_config_home_relative: ".forgekairos-claude",
};

/// The names this build uses. The rename is carried out one part at a time:
/// each field is taken from either [`ZED_JP_ENHANCE`] or [`FORGEKAIROS`], and a
/// part is switched by pointing its field at [`FORGEKAIROS`].
pub const CURRENT: Names = Names {
    product_name: FORGEKAIROS.product_name,
    display_names: FORGEKAIROS.display_names,
    app_directory_name: FORGEKAIROS.app_directory_name,
    app_directory_name_lowercase: FORGEKAIROS.app_directory_name_lowercase,
    app_identifiers: FORGEKAIROS.app_identifiers,
    app_ids: FORGEKAIROS.app_ids,
    app_user_model_ids: FORGEKAIROS.app_user_model_ids,
    executable_stem: FORGEKAIROS.executable_stem,
    cli_name: FORGEKAIROS.cli_name,
    url_scheme: FORGEKAIROS.url_scheme,
    remote_server_binary_prefix: FORGEKAIROS.remote_server_binary_prefix,
    remote_cache_home_relative: FORGEKAIROS.remote_cache_home_relative,
    sandboxed_agent_config_home_relative: FORGEKAIROS.sandboxed_agent_config_home_relative,
};

/// Every set of names an earlier build of this fork used, newest first. Data
/// is moved out of their directories, and their directories stay protected.
pub const LEGACY: &[Names] = &[ZED_JP_ENHANCE];

/// The scheme of the links this application writes for itself (agent mentions
/// stored in thread databases, settings and schema links). It stays upstream's
/// whatever [`Names::url_scheme`] the operating system knows the app by.
pub const INTERNAL_URL_SCHEME: &str = "zed";

/// The GitHub repository this fork's releases are published from.
pub const UPDATE_REPO: &str = "SeiyaFunaokaJP/ForgeKairos";

/// The loopback port the macOS single-instance check of the builds before the
/// rename starts from: upstream's, so an official Zed listens there too. Each
/// release channel adds 100 (dev, preview, stable, nightly in that order) and
/// then the user ID, wrapped below port 65535. Every port from here up belongs
/// to those builds, so the current builds stay below it.
pub const LEGACY_MAC_INSTANCE_BASE_PORT: u16 = 43737;

/// What a running instance of the builds before the rename (and an official
/// Zed) sends on [`LEGACY_MAC_INSTANCE_BASE_PORT`]'s ports.
pub const LEGACY_MAC_INSTANCE_HANDSHAKES: ChannelNames = ChannelNames {
    dev: "Zed Editor Dev Instance Running",
    nightly: "Zed Editor Nightly Instance Running",
    preview: "Zed Editor Preview Instance Running",
    stable: "Zed Editor Stable Instance Running",
};

/// One loopback port per release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPorts {
    pub dev: u16,
    pub nightly: u16,
    pub preview: u16,
    pub stable: u16,
}

impl ChannelPorts {
    /// In the order of [`ChannelNames::all`].
    pub const fn all(&self) -> [u16; 4] {
        [self.dev, self.nightly, self.preview, self.stable]
    }
}

/// The range the macOS single-instance check picks its loopback ports from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacInstancePorts {
    pub base_port: u16,
    /// The first port above the range: the user ID is wrapped below it.
    pub port_ceiling: u16,
}

impl MacInstancePorts {
    const CHANNEL_SPACING: u16 = 100;

    /// Each release channel's port for `user_id`. The channels are spaced
    /// [`Self::CHANNEL_SPACING`] apart (dev, preview, stable, nightly in that
    /// order, as upstream does) and then offset by the user ID, so that users
    /// and channels on one machine do not answer each other.
    pub const fn ports(&self, user_id: Option<u32>) -> ChannelPorts {
        ChannelPorts {
            dev: self.port(0, user_id),
            preview: self.port(1, user_id),
            stable: self.port(2, user_id),
            nightly: self.port(3, user_id),
        }
    }

    const fn port(&self, channel_block: u16, user_id: Option<u32>) -> u16 {
        let port = self.base_port + channel_block * Self::CHANNEL_SPACING;
        match user_id {
            Some(user_id) => {
                let ports_left = (self.port_ceiling - port) as u32;
                port + (user_id % ports_left) as u16
            }
            None => port,
        }
    }
}

/// The ports of the builds before the rename and of an official Zed:
/// upstream's computation, wrapped below the highest port.
pub const LEGACY_MAC_INSTANCE_PORTS: MacInstancePorts = MacInstancePorts {
    base_port: LEGACY_MAC_INSTANCE_BASE_PORT,
    port_ceiling: u16::MAX,
};

/// The ports of the current builds, kept below every legacy port so that they
/// never answer, or get answered by, an official Zed.
pub const MAC_INSTANCE_PORTS: MacInstancePorts = MacInstancePorts {
    base_port: 33737,
    port_ceiling: LEGACY_MAC_INSTANCE_BASE_PORT,
};

/// What a running instance of the current builds sends on
/// [`MAC_INSTANCE_PORTS`], given its channel's entry of
/// [`Names::app_identifiers`].
pub fn mac_instance_handshake(app_identifier: &str) -> String {
    format!("{app_identifier} Instance Running")
}

/// Concatenates `&str` constants into a `&'static str` at compile time, for
/// paths that have to be spelled in `const` items and tables.
#[macro_export]
macro_rules! concat_const {
    ($($part:expr),+ $(,)?) => {{
        const PARTS: &[&str] = &[$($part),+];
        const LENGTH: usize = $crate::__concatenated_length(PARTS);
        const BYTES: [u8; LENGTH] = $crate::__concatenate::<LENGTH>(PARTS);
        match ::core::str::from_utf8(&BYTES) {
            Ok(text) => text,
            Err(_) => panic!("a concatenation of str is UTF-8"),
        }
    }};
}

#[doc(hidden)]
pub const fn __concatenated_length(parts: &[&str]) -> usize {
    let mut length = 0;
    let mut part_index = 0;
    while part_index < parts.len() {
        length += parts[part_index].len();
        part_index += 1;
    }
    length
}

#[doc(hidden)]
pub const fn __concatenate<const LENGTH: usize>(parts: &[&str]) -> [u8; LENGTH] {
    let mut bytes = [0u8; LENGTH];
    let mut position = 0;
    let mut part_index = 0;
    while part_index < parts.len() {
        let part = parts[part_index].as_bytes();
        let mut byte_index = 0;
        while byte_index < part.len() {
            bytes[position] = part[byte_index];
            position += 1;
            byte_index += 1;
        }
        part_index += 1;
    }
    assert!(position == LENGTH, "LENGTH must be the parts' total length");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_set() -> Vec<(&'static str, Names)> {
        let mut sets = vec![("CURRENT", CURRENT), ("FORGEKAIROS", FORGEKAIROS)];
        sets.extend(LEGACY.iter().map(|names| ("LEGACY", *names)));
        sets
    }

    fn fields(names: &Names) -> Vec<(&'static str, &'static str)> {
        let mut fields = vec![
            ("product_name", names.product_name),
            ("app_directory_name", names.app_directory_name),
            (
                "app_directory_name_lowercase",
                names.app_directory_name_lowercase,
            ),
            ("executable_stem", names.executable_stem),
            ("cli_name", names.cli_name),
            ("url_scheme", names.url_scheme),
            (
                "remote_server_binary_prefix",
                names.remote_server_binary_prefix,
            ),
            (
                "remote_cache_home_relative",
                names.remote_cache_home_relative,
            ),
            (
                "sandboxed_agent_config_home_relative",
                names.sandboxed_agent_config_home_relative,
            ),
        ];
        for (field, channel_names) in [
            ("display_names", names.display_names),
            ("app_identifiers", names.app_identifiers),
            ("app_ids", names.app_ids),
            ("app_user_model_ids", names.app_user_model_ids),
        ] {
            fields.extend(channel_names.all().into_iter().map(|name| (field, name)));
        }
        fields
    }

    #[test]
    fn the_new_names_do_not_say_zed() {
        for (field, name) in fields(&FORGEKAIROS) {
            assert!(
                !name.to_ascii_lowercase().contains("zed"),
                "FORGEKAIROS.{field} = {name:?} still names Zed"
            );
        }
    }

    #[test]
    fn every_current_field_comes_from_one_of_the_two_sets() {
        let current = fields(&CURRENT);
        let old = fields(&ZED_JP_ENHANCE);
        let new = fields(&FORGEKAIROS);
        assert_eq!(current.len(), old.len());
        for ((current, old), new) in current.iter().zip(&old).zip(&new) {
            assert!(
                current == old || current == new,
                "CURRENT.{} = {:?} is neither {:?} nor {:?}",
                current.0,
                current.1,
                old.1,
                new.1
            );
        }
    }

    #[test]
    fn the_legacy_sets_are_older_names_only() {
        assert!(LEGACY.contains(&ZED_JP_ENHANCE));
        assert!(!LEGACY.contains(&FORGEKAIROS));
    }

    #[test]
    fn directory_names_can_be_interpolated_into_paths_and_shell_commands() {
        for (set, names) in every_set() {
            let name = names.app_directory_name;
            assert!(!name.is_empty(), "{set}: empty app_directory_name");
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                "{set}: app_directory_name {name:?} must be ASCII letters, digits and '-'"
            );
            assert_eq!(
                names.app_directory_name_lowercase,
                name.to_ascii_lowercase(),
                "{set}: app_directory_name_lowercase"
            );
        }
    }

    #[test]
    fn home_relative_paths_stay_inside_the_home() {
        for (set, names) in every_set() {
            for path in [
                names.remote_cache_home_relative,
                names.sandboxed_agent_config_home_relative,
            ] {
                assert!(!path.is_empty(), "{set}: empty home-relative path");
                assert!(!path.starts_with('/'), "{set}: {path:?} is absolute");
                assert!(
                    path.split('/').all(|component| !component.is_empty()
                        && component != ".."
                        && component != "."),
                    "{set}: {path:?} has an empty, '.' or '..' component"
                );
                assert!(
                    path.bytes().all(|byte| byte.is_ascii_alphanumeric()
                        || matches!(byte, b'.' | b'_' | b'-' | b'/')),
                    "{set}: {path:?} has a character a shell or a glob would read"
                );
            }
        }
    }

    #[test]
    fn launcher_names_and_schemes_are_plain_lowercase_words() {
        for (set, names) in every_set() {
            for (field, name) in [
                ("cli_name", names.cli_name),
                ("url_scheme", names.url_scheme),
            ] {
                assert!(
                    name.starts_with(|character: char| character.is_ascii_lowercase())
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
                    "{set}: {field} {name:?} must be a lowercase ASCII word"
                );
            }
            let prefix = names.remote_server_binary_prefix;
            assert!(
                prefix.ends_with('-')
                    && prefix
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'-'),
                "{set}: remote_server_binary_prefix {prefix:?} is globbed unquoted on the host"
            );
        }
    }

    #[test]
    fn identifiers_are_valid_where_the_os_uses_them() {
        for (set, names) in every_set() {
            for identifier in names.app_identifiers.all() {
                assert!(
                    identifier
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                    "{set}: app identifier {identifier:?} names a mutex and a pipe"
                );
            }
            for identifier in names
                .app_ids
                .all()
                .into_iter()
                .chain(names.app_user_model_ids.all())
            {
                let segments: Vec<&str> = identifier.split('.').collect();
                assert!(
                    segments.len() >= 2
                        && segments.iter().all(|segment| {
                            !segment.is_empty()
                                && segment
                                    .bytes()
                                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                        }),
                    "{set}: {identifier:?} is not a dotted identifier"
                );
            }
            let all_channel_names = [
                names.display_names,
                names.app_identifiers,
                names.app_ids,
                names.app_user_model_ids,
            ];
            for channel_names in all_channel_names {
                let mut distinct = channel_names.all().to_vec();
                distinct.sort_unstable();
                distinct.dedup();
                assert_eq!(distinct.len(), 4, "{set}: {channel_names:?} repeats a name");
            }
        }
    }

    #[test]
    fn display_names_start_with_the_product_name() {
        for (set, names) in every_set() {
            assert_eq!(names.display_names.stable, names.product_name, "{set}");
            for display_name in names.display_names.all() {
                assert!(
                    display_name.starts_with(names.product_name),
                    "{set}: {display_name:?}"
                );
            }
        }
    }

    #[test]
    fn the_new_names_differ_from_every_legacy_name() {
        let new = fields(&FORGEKAIROS);
        for legacy in LEGACY {
            for ((field, legacy_name), (_, new_name)) in fields(legacy).iter().zip(&new) {
                assert_ne!(
                    legacy_name.to_ascii_lowercase(),
                    new_name.to_ascii_lowercase(),
                    "{field} would not move away from {legacy_name:?}"
                );
            }
        }
    }

    #[test]
    fn mac_instance_ports_stay_in_their_ranges_and_apart() {
        let user_ids = (0..70_000)
            .chain((u32::MAX - 70_000)..=u32::MAX)
            .map(Some)
            .chain([None]);
        for user_id in user_ids {
            let current = MAC_INSTANCE_PORTS.ports(user_id).all();
            let legacy = LEGACY_MAC_INSTANCE_PORTS.ports(user_id).all();
            for port in current {
                assert!(
                    (MAC_INSTANCE_PORTS.base_port..LEGACY_MAC_INSTANCE_BASE_PORT).contains(&port),
                    "user {user_id:?}: current port {port}"
                );
            }
            for port in legacy {
                assert!(
                    port >= LEGACY_MAC_INSTANCE_BASE_PORT,
                    "user {user_id:?}: legacy port {port}"
                );
            }
            for ports in [current, legacy] {
                let mut distinct = ports.to_vec();
                distinct.sort_unstable();
                distinct.dedup();
                assert_eq!(distinct.len(), 4, "user {user_id:?}: {ports:?}");
            }
        }
    }

    #[test]
    fn mac_instance_ports_follow_upstream_s_channel_order() {
        let ports = LEGACY_MAC_INSTANCE_PORTS.ports(Some(501));
        assert_eq!(
            ports,
            ChannelPorts {
                dev: 43737 + 501,
                preview: 43837 + 501,
                stable: 43937 + 501,
                nightly: 44037 + 501,
            }
        );
        // Upstream wraps a user ID above the room left below port 65535.
        assert_eq!(
            LEGACY_MAC_INSTANCE_PORTS.ports(Some(30_000)).dev,
            43737 + (30_000 % (65535 - 43737))
        );
        assert_eq!(MAC_INSTANCE_PORTS.ports(None).stable, 33937);
    }

    #[test]
    fn concat_const_joins_its_parts() {
        const JOINED: &str =
            concat_const!(".local/share/", FORGEKAIROS.app_directory_name_lowercase);
        assert_eq!(JOINED, ".local/share/forgekairos");
        assert_eq!(concat_const!("a"), "a");
        assert_eq!(concat_const!("", "b", ""), "b");
    }
}
