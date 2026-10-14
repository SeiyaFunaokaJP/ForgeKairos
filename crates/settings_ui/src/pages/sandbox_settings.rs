use std::path::{Path, PathBuf};

use agent_settings::AgentSettings;
use gpui::{ReadGlobal as _, ScrollHandle, prelude::*};
use http_proxy::HostPattern;
use settings::{Settings as _, SettingsStore};
use ui::{Banner, Divider, Severity, SwitchField, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::SettingsWindow;
use crate::components::{SettingsInputField, SettingsSectionHeader};

const DOMAINS_DESCRIPTION: &str = i18n::msg!(
    "Each entry is an exact domain (github.com) or a leading-*. subdomain wildcard (*.npmjs.org). IP addresses and local domains are not allowed. An entry here only takes effect within the editor-wide network allowlist: a host that is not on that list stays unreachable even if you add it here."
);

/// The "allow every domain" switch lifts the *sandbox's* own host list, not the
/// editor-wide egress allowlist that list sits inside — see ADR-F17. Saying so
/// here keeps the switch from reading as an escape hatch from that allowlist,
/// which is exactly what it is not.
const ALLOW_ALL_HOSTS_DESCRIPTION: &str = i18n::msg!(
    "Let sandboxed commands reach any domain over the network without prompting. This only lifts the sandbox's own restriction, so commands can still reach only the hosts within the editor-wide network allowlist (localhost is always reachable)."
);

/// ADR-F92: the switch at the top of the page. Named after running outside
/// the sandbox, not after any one tool, so that it is not read as switching
/// some agent feature on or off.
const ALLOW_OUTSIDE_SANDBOX_DESCRIPTION: &str = i18n::msg!(
    "When off (the default), nothing that the agent features newly start or run goes outside the sandbox: terminal commands that can't run in the sandbox, starting an external agent or Claude Code outside the sandbox, and resuming a thread that was started there are all refused without asking, and the settings that always start them outside are ignored. A connection already running outside the sandbox keeps running. MCP servers (context servers) are not covered by this setting. When on, each of those asks first or follows its own setting."
);

/// ADR-F96 stage 2: the standing permission behind the chat input's network
/// button. Saying that the editor-wide allowlist still applies keeps it from
/// reading as a way past that allowlist, as for "Allow All Domains" below.
const ALLOW_UNRESTRICTED_NETWORK_DESCRIPTION: &str = i18n::msg!(
    "When on, a thread of the built-in agent can lift its network restriction with the network button next to the message input: its sandboxed terminal commands and fetches then reach any host without asking. Commands still reach only the hosts within the editor-wide network allowlist, and fetches still never reach local or private addresses. When off (the default), that button asks to turn this on first. Each command still asks for confirmation as before."
);

/// ADR-F96: the per-folder defaults are a map keyed by folder path, which this
/// page does not edit row by row; the agent panel's buttons write them.
const PROJECT_THREAD_DEFAULTS_DESCRIPTION: &str = i18n::msg!(
    "What new agent threads start with in each project folder: whether terminal commands run in the sandbox and whether the network is restricted. Set it by right-clicking the sandbox or network button next to the agent panel's message input, or edit agent.project_thread_defaults in settings.json."
);

const ENABLE_SANDBOX_DESCRIPTION: &str = i18n::msg!(
    "Wrap agent-run terminal commands in an OS-level sandbox. When off, commands run with Zed's own permissions, so auto-approval does not apply and each command asks for confirmation. This does not affect external agents themselves or how Claude Code is launched (their settings are on the AI page)."
);

const ENABLE_SANDBOX_LOCKED_DESCRIPTION: &str = i18n::msg!(
    "Wrap agent-run terminal commands in an OS-level sandbox. It can only be turned off while running outside the sandbox is allowed (above)."
);

const WRITE_PATHS_DESCRIPTION: &str = i18n::msg!(
    "Each entry must be an absolute path and grants write access to the whole subtree, except protected Git metadata."
);

pub(crate) fn render_sandbox_settings_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    // Sandbox permissions are a user-level setting; they aren't configurable
    // per-project, so always operate against the global value here.
    let permissions = AgentSettings::get_global(cx).sandbox_permissions.clone();
    let validation_error = settings_window.sandbox_host_validation_error.clone();

    // Read the list values from the raw user settings content rather than the
    // compiled `AgentSettings`. The compiled `write_paths` are lexically
    // normalized (see `compile_sandbox_permissions`), so editing or removing a
    // row by the normalized value would fail to match the literal entry stored
    // in settings.json and silently leave the permission in place.
    let (network_hosts, write_paths) = raw_sandbox_lists(cx);

    let host_rows: Vec<AnyElement> = network_hosts
        .into_iter()
        .enumerate()
        .map(|(index, host)| render_host_row(index, host, cx))
        .collect();
    let add_host_input = render_add_host_input(cx);

    let path_rows: Vec<AnyElement> = write_paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| render_path_row(index, path, cx))
        .collect();
    let add_path_input = render_add_path_input(cx);

    let empty_border = cx.theme().colors().border_variant;
    // ADR-F92: `allow_unsandboxed` already reads as off while running outside
    // the sandbox is not allowed, so the switch below shows what applies.
    let outside_allowed = project::agent_sandbox::outside_sandbox_allowed(cx);
    let sandbox_enabled = !permissions.allow_unsandboxed;

    v_flex()
        .id("sandbox-settings-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_6()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(
            SwitchField::new(
                "sandbox-allow-outside",
                Some(i18n::t!("Allow Running Outside the Sandbox")),
                Some(i18n::tr(ALLOW_OUTSIDE_SANDBOX_DESCRIPTION).into()),
                outside_allowed,
                move |state, _window, cx| {
                    set_allow_outside_sandbox(*state == ToggleState::Selected, cx);
                },
            )
            .tab_index(0),
        )
        .child(
            SwitchField::new(
                "sandbox-enabled",
                Some(i18n::t!("Enable Sandbox")),
                Some(
                    i18n::tr(if outside_allowed {
                        ENABLE_SANDBOX_DESCRIPTION
                    } else {
                        ENABLE_SANDBOX_LOCKED_DESCRIPTION
                    })
                    .into(),
                ),
                sandbox_enabled,
                move |state, _window, cx| {
                    set_sandbox_enabled(*state == ToggleState::Selected, cx);
                },
            )
            .disabled(!outside_allowed)
            .tab_index(0),
        )
        .child(
            SwitchField::new(
                "sandbox-allow-unrestricted-network",
                Some(i18n::t!("Allow Lifting the Network Restriction for a Thread")),
                Some(i18n::tr(ALLOW_UNRESTRICTED_NETWORK_DESCRIPTION).into()),
                project::agent_sandbox::unrestricted_network_allowed(cx),
                move |state, _window, cx| {
                    set_allow_unrestricted_network(*state == ToggleState::Selected, cx);
                },
            )
            .tab_index(0),
        )
        .child(render_project_thread_defaults(cx))
        .child(Divider::horizontal())
        .child(
            SwitchField::new(
                "pause-worktree-code-after-agent-writes",
                Some(i18n::t!("Pause Folder Code After Agent Writes Until Resumed")),
                Some(
                    i18n::t!("In a project folder where the agent has written files, language servers and formatters do not run that folder's code (the project's Prettier and ESLint configuration, build scripts, and so on) until you press Resume. This setting is saved only in your user settings and cannot be changed by project settings.")
                        .into(),
                ),
                project::worktree_code_execution::pause_after_agent_writes_enabled(cx),
                move |state, _window, cx| {
                    set_pause_worktree_code_after_agent_writes(
                        *state == ToggleState::Selected,
                        cx,
                    );
                },
            )
            .tab_index(0),
        )
        .child({
            let docs_url =
                client::zed_urls::sandboxing_docs(Some("persistent-sandbox-permissions"), cx);
            let tooltip = i18n::t!("Opens {docs_url}");
            // Wrap in a row so the button shrinks to its content width instead
            // of stretching across the settings page.
            h_flex().child(
                Button::new("sandbox-docs-link", i18n::t!("Learn more about sandboxing"))
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .end_icon(
                        Icon::new(IconName::ArrowUpRight)
                            .color(Color::Muted)
                            .size(IconSize::XSmall),
                    )
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(move |_, _, cx| cx.open_url(&docs_url)),
            )
        })
        .when(sandbox_enabled, |this| this
        .when_some(validation_error, |this, error| {
            this.child(
                Banner::new()
                    .severity(Severity::Warning)
                    .child(Label::new(error).size(LabelSize::Small))
                    .action_slot(
                        Button::new("dismiss-sandbox-host-error", i18n::t!("Dismiss"))
                            .style(ButtonStyle::Tinted(ui::TintColor::Warning))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sandbox_host_validation_error = None;
                                cx.notify();
                            })),
                    ),
            )
        })
        .child(
            v_flex()
                .gap_4()
                .child(SettingsSectionHeader::new(i18n::t!("Network")).no_padding(true))
                .child(
                    SwitchField::new(
                        "sandbox-allow-all-hosts",
                        Some(i18n::t!("Allow All Domains")),
                        Some(i18n::tr(ALLOW_ALL_HOSTS_DESCRIPTION).into()),
                        permissions.allow_all_hosts,
                        move |state, _window, cx| {
                            set_allow_all_hosts(*state == ToggleState::Selected, cx);
                        },
                    )
                    .tab_index(0),
                )
                .child(render_list_section(
                    i18n::t!("Allowed Domains"),
                    i18n::tr(DOMAINS_DESCRIPTION),
                    host_rows,
                    add_host_input,
                    empty_border,
                )),
        )

        .child(Divider::horizontal())
        .child(
            v_flex()
                .gap_4()
                .child(SettingsSectionHeader::new(i18n::t!("File System")).no_padding(true))
                .child(
                    SwitchField::new(
                        "sandbox-allow-fs-write-all",
                        Some(i18n::t!("Allow All File System Writes")),
                        Some(
                            i18n::t!("Let sandboxed commands write anywhere except protected paths (such as Git metadata). This is almost the same as running outside the sandbox, so while it is on, auto-approval does not apply and each command asks for confirmation.")
                                .into(),
                        ),
                        permissions.allow_fs_write_all,
                        move |state, _window, cx| {
                            set_allow_fs_write_all(*state == ToggleState::Selected, cx);
                        },
                    )
                    .tab_index(0),
                )
                .child(render_list_section(
                    i18n::t!("Writable Paths"),
                    i18n::tr(WRITE_PATHS_DESCRIPTION),
                    path_rows,
                    add_path_input,
                    empty_border,
                )),
        )
        .child(Divider::horizontal())
        .child(
            v_flex()
                .gap_4()
                .child(SettingsSectionHeader::new(i18n::t!("Escalation Prompts")).no_padding(true))
                .child(
                    SwitchField::new(
                        "sandbox-warn-confusable-unicode",
                        Some(i18n::t!("Warn About Confusable Unicode")),
                        Some(
                            i18n::t!("Warn when an approval prompt requests a domain or write path that contains potentially confusable Unicode characters, such as homoglyphs (i.e. two symbols that look similar, such as a Cyrillic `а`)")
                                .into(),
                        ),
                        permissions.warn_confusable_unicode,
                        move |state, _window, cx| {
                            set_warn_confusable_unicode(*state == ToggleState::Selected, cx);
                        },
                    )
                    .tab_index(0),
                )
                .child(
                    SwitchField::new(
                        "sandbox-warn-ntfs-grants",
                        Some(i18n::t!("Warn About Windows-Drive Grants")),
                        Some(
                            i18n::t!("Windows only: warn when a sandbox grant targets a file on a Windows drive (accessed inside WSL via DrvFs). Such grants are enforced through a translated path and their sandbox-integrity guarantees are weaker than files on the Linux distro's own filesystem.")
                                .into(),
                        ),
                        permissions.warn_ntfs_grants,
                        move |state, _window, cx| {
                            set_warn_ntfs_grants(*state == ToggleState::Selected, cx);
                        },
                    )
                    .tab_index(0),
                ),
        )
        )
        .into_any_element()
}

fn render_project_thread_defaults(cx: &mut Context<SettingsWindow>) -> AnyElement {
    h_flex()
        .w_full()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .gap_0p5()
                .child(Label::new(i18n::t!("Project Defaults for New Threads")))
                .child(
                    Label::new(i18n::tr(PROJECT_THREAD_DEFAULTS_DESCRIPTION))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .child(
            Button::new(
                "sandbox-project-thread-defaults-in-settings-file",
                i18n::t!("Edit in settings.json"),
            )
            .style(ButtonStyle::Outlined)
            .size(ButtonSize::Medium)
            .tab_index(0_isize)
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_current_settings_file(window, cx);
            })),
        )
        .into_any_element()
}

fn render_list_section(
    title: &'static str,
    description: &'static str,
    rows: Vec<AnyElement>,
    add_input: AnyElement,
    empty_border: gpui::Hsla,
) -> impl IntoElement {
    let is_empty = rows.is_empty();

    v_flex()
        .gap_0p5()
        .child(Label::new(title))
        .child(
            Label::new(description)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            v_flex()
                .mt_2()
                .w_full()
                .gap_1p5()
                .when(is_empty, |this| {
                    this.child(render_empty_state(empty_border))
                })
                .when(!is_empty, |this| {
                    this.child(v_flex().gap_1p5().children(rows))
                })
                .child(add_input),
        )
}

fn render_empty_state(border_color: gpui::Hsla) -> AnyElement {
    h_flex()
        .p_2()
        .rounded_md()
        .border_1()
        .border_dashed()
        .border_color(border_color)
        .child(
            Label::new(i18n::t!("Nothing configured"))
                .size(LabelSize::Small)
                .color(Color::Disabled),
        )
        .into_any_element()
}

fn render_host_row(index: usize, host: String, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let host_for_delete = host.clone();
    let host_for_update = host.clone();
    let settings_window = cx.entity().downgrade();

    SettingsInputField::new(format!("sandbox-host-{}", index))
        .with_initial_text(host)
        .tab_index(0)
        .with_buffer_font()
        .color(Color::Default)
        .action_slot(
            IconButton::new(format!("sandbox-host-delete-{}", index), IconName::Trash)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text(i18n::t!("Remove Domain")))
                .on_click(cx.listener(move |_, _, _, cx| {
                    remove_network_host(host_for_delete.clone(), cx);
                })),
        )
        .on_confirm(move |new_host, _window, cx| {
            let Some(new_host) = new_host else {
                return;
            };
            let new_host = new_host.trim().to_string();
            if new_host.is_empty() || new_host == host_for_update {
                return;
            }
            let result = canonicalize_host(&new_host);
            settings_window
                .update(cx, |this, cx| {
                    match result {
                        Ok(canonical) => {
                            this.sandbox_host_validation_error = None;
                            update_network_host(host_for_update.clone(), canonical, cx);
                        }
                        Err(error) => {
                            this.sandbox_host_validation_error = Some(error);
                        }
                    }
                    cx.notify();
                })
                .log_err();
        })
        .into_any_element()
}

fn render_add_host_input(cx: &mut Context<SettingsWindow>) -> AnyElement {
    let settings_window = cx.entity().downgrade();

    SettingsInputField::new("sandbox-host-new")
        .with_placeholder(i18n::t!("Add domain (e.g. github.com or *.npmjs.org)…"))
        .tab_index(0)
        .with_buffer_font()
        .display_clear_button()
        .display_confirm_button()
        .clear_on_confirm()
        .on_confirm(move |host, _window, cx| {
            let Some(host) = host else {
                return;
            };
            let host = host.trim().to_string();
            if host.is_empty() {
                return;
            }
            let result = canonicalize_host(&host);
            settings_window
                .update(cx, |this, cx| {
                    match result {
                        Ok(canonical) => {
                            this.sandbox_host_validation_error = None;
                            add_network_host(canonical, cx);
                        }
                        Err(error) => {
                            this.sandbox_host_validation_error = Some(error);
                        }
                    }
                    cx.notify();
                })
                .log_err();
        })
        .into_any_element()
}

fn render_path_row(index: usize, path: PathBuf, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let path_for_delete = path.clone();
    let path_for_update = path.clone();
    let settings_window = cx.entity().downgrade();

    SettingsInputField::new(format!("sandbox-path-{}", index))
        .with_initial_text(path.to_string_lossy().into_owned())
        .tab_index(0)
        .with_buffer_font()
        .color(Color::Default)
        .action_slot(
            IconButton::new(format!("sandbox-path-delete-{}", index), IconName::Trash)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text(i18n::t!("Remove Path")))
                .on_click(cx.listener(move |_, _, _, cx| {
                    remove_write_path(path_for_delete.clone(), cx);
                })),
        )
        .on_confirm(move |new_path, _window, cx| {
            let Some(new_path) = new_path else {
                return;
            };
            let new_path = new_path.trim();
            if new_path.is_empty() {
                return;
            }
            let new_path = PathBuf::from(new_path);
            if new_path == path_for_update {
                return;
            }
            update_write_path(path_for_update.clone(), new_path, cx);
            settings_window.update(cx, |_, cx| cx.notify()).log_err();
        })
        .into_any_element()
}

fn render_add_path_input(cx: &mut Context<SettingsWindow>) -> AnyElement {
    let settings_window = cx.entity().downgrade();

    SettingsInputField::new("sandbox-path-new")
        .with_placeholder(i18n::t!("Add an absolute path (e.g. /path/to/directory)…"))
        .tab_index(0)
        .with_buffer_font()
        .display_clear_button()
        .display_confirm_button()
        .clear_on_confirm()
        .on_confirm(move |path, _window, cx| {
            let Some(path) = path else {
                return;
            };
            let path = path.trim();
            if path.is_empty() {
                return;
            }
            add_write_path(PathBuf::from(path), cx);
            settings_window.update(cx, |_, cx| cx.notify()).log_err();
        })
        .into_any_element()
}

/// The literal host and write-path lists as stored in user settings.json. These
/// are the exact strings/paths that edits and removals must match against.
fn raw_sandbox_lists(cx: &App) -> (Vec<String>, Vec<PathBuf>) {
    let store = SettingsStore::global(cx);
    let permissions = store
        .raw_user_settings()
        .and_then(|user| user.content.agent.as_ref())
        .and_then(|agent| agent.sandbox_permissions.as_ref());

    let network_hosts = permissions
        .and_then(|permissions| permissions.network_hosts.as_ref())
        .map(|hosts| hosts.0.clone())
        .unwrap_or_default();
    // Display and match on the requested path of each entry (the literal a
    // hand-edit types); Zed-written "allow always" grants also carry a resolved
    // canonical, but the settings row still keys off the requested path.
    let write_paths = permissions
        .and_then(|permissions| permissions.write_paths.as_ref())
        .map(|paths| {
            paths
                .0
                .iter()
                .map(|entry| entry.requested.clone())
                .collect()
        })
        .unwrap_or_default();

    (network_hosts, write_paths)
}

/// Validate and canonicalize a user-provided domain, returning either the
/// canonical form to persist or a domain-friendly error to surface.
fn canonicalize_host(host: &str) -> Result<String, String> {
    use http_proxy::HostPatternError;

    HostPattern::parse(host)
        .map(|pattern| pattern.to_string())
        .map_err(|error| match error {
            HostPatternError::Empty => i18n::t!("Domain cannot be empty.").to_string(),
            HostPatternError::IpLiteral(_) => i18n::t!(
                "IP addresses and local domains aren't allowed; enter a domain like github.com."
            )
            .to_string(),
            HostPatternError::InvalidWildcard(_) => {
                i18n::t!("Wildcards are only allowed as a leading label, e.g. *.github.com.")
                    .to_string()
            }
            HostPatternError::Invalid { .. } => {
                i18n::t!("Not a valid domain. Use a domain like github.com or *.npmjs.org.")
                    .to_string()
            }
        })
}

fn update_sandbox_permissions(
    cx: &mut App,
    update: impl 'static + Send + FnOnce(&mut settings::SandboxPermissionsContent),
) {
    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        update(
            settings
                .agent
                .get_or_insert_default()
                .sandbox_permissions
                .get_or_insert_default(),
        );
    });
}

/// ADR-F50: written to the user settings file, the only one it is read from.
fn set_pause_worktree_code_after_agent_writes(value: bool, cx: &mut App) {
    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        settings
            .agent
            .get_or_insert_default()
            .pause_worktree_code_after_agent_writes = Some(value);
    });
}

/// ADR-F92: written to the user settings file, the only one it is read from.
fn set_allow_outside_sandbox(value: bool, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        permissions.allow_outside_sandbox = Some(value);
    });
}

/// ADR-F96 stage 2: written to the user settings file, the only one it is read
/// from.
fn set_allow_unrestricted_network(value: bool, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        permissions.allow_unrestricted_network = Some(value);
    });
}

fn set_sandbox_enabled(value: bool, cx: &mut App) {
    // The UI presents an "enabled" switch, but the stored setting is the
    // inverse (`allow_unsandboxed`).
    update_sandbox_permissions(cx, move |permissions| {
        permissions.allow_unsandboxed = Some(!value);
    });
}

fn set_allow_all_hosts(value: bool, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        permissions.allow_all_hosts = Some(value);
    });
}

fn set_allow_fs_write_all(value: bool, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        permissions.allow_fs_write_all = Some(value);
    });
}

fn set_warn_confusable_unicode(value: bool, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        permissions.warn_confusable_unicode = Some(value);
    });
}

fn set_warn_ntfs_grants(value: bool, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        permissions.warn_ntfs_grants = Some(value);
    });
}

fn add_network_host(host: String, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        let hosts = &mut permissions.network_hosts.get_or_insert_default().0;
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    });
}

fn update_network_host(old_host: String, new_host: String, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        let hosts = &mut permissions.network_hosts.get_or_insert_default().0;
        if hosts.contains(&new_host) {
            return;
        }
        if let Some(entry) = hosts.iter_mut().find(|host| **host == old_host) {
            *entry = new_host;
        }
    });
}

fn remove_network_host(host: String, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        if let Some(hosts) = permissions.network_hosts.as_mut() {
            hosts.0.retain(|entry| *entry != host);
        }
    });
}

/// Insert a hand-authored write-path entry (no resolved canonical) into the
/// settings list as a minimal subtree, mirroring `util::paths::insert_subtree`
/// but over [`settings::GrantedWritePathContent`] keyed on the requested path.
///
/// Containment is judged on the folded spelling (ADR-F49 decision 34), so a
/// folder the file already grants under an older spelling of its WSL
/// distribution is not granted a second time under the new one. What is
/// stored is still what the user typed.
fn insert_write_path_subtree(paths: &mut Vec<settings::GrantedWritePathContent>, path: PathBuf) {
    let folded = fold_grant_spelling(&path);
    if paths
        .iter()
        .any(|entry| folded.starts_with(fold_grant_spelling(&entry.requested)))
    {
        return;
    }
    paths.retain(|entry| !fold_grant_spelling(&entry.requested).starts_with(&folded));
    paths.push(settings::GrantedWritePathContent {
        requested: path,
        resolved: None,
        on_windows_fs: false,
    });
}

/// ADR-F49 decision 34: a grant that names a folder in a WSL distribution, in
/// the one spelling those are stored and compared in; anything else unchanged.
fn fold_grant_spelling(path: &Path) -> PathBuf {
    path.to_str()
        .and_then(util::path_mapping::canonical_wsl_grant_spelling)
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// ADR-F49 decision 34: whether a row names the folder the user acted on.
///
/// The rows here are the file's own (see [`raw_sandbox_lists`]), and one
/// folder in a WSL distribution can be written several ways — a row approved
/// before that decision names the distribution as `wsl.exe` reported it, one
/// approved after names it in lower case. Matching by location is what makes
/// removing or editing the row the user clicked reach every row that grants
/// that same folder, rather than leaving a stale twin behind.
fn names_the_same_grant(row: &Path, acted_on: &Path) -> bool {
    fold_grant_spelling(row) == fold_grant_spelling(acted_on)
}

fn add_write_path(path: PathBuf, cx: &mut App) {
    // Normalize away `.`/`..` so the stored entry matches the form the runtime
    // uses for coverage checks (see `compile_sandbox_permissions`) and the form
    // persisted by the in-thread "Allow always" grant. A hand-authored entry
    // records no resolved canonical, so enforcement resolves it fresh (see
    // `granted_write_path_to_location`).
    let Ok(path) = util::paths::normalize_lexically(&path) else {
        return;
    };
    update_sandbox_permissions(cx, move |permissions| {
        let paths = &mut permissions.write_paths.get_or_insert_default().0;
        insert_write_path_subtree(paths, path);
    });
}

fn update_write_path(old_path: PathBuf, new_path: PathBuf, cx: &mut App) {
    let Ok(new_path) = util::paths::normalize_lexically(&new_path) else {
        return;
    };
    update_sandbox_permissions(cx, move |permissions| {
        if let Some(paths) = permissions.write_paths.as_mut() {
            paths
                .0
                .retain(|entry| !names_the_same_grant(&entry.requested, &old_path));
            insert_write_path_subtree(&mut paths.0, new_path);
        }
    });
}

fn remove_write_path(path: PathBuf, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        if let Some(paths) = permissions.write_paths.as_mut() {
            paths
                .0
                .retain(|entry| !names_the_same_grant(&entry.requested, &path));
        }
    });
}

#[cfg(test)]
mod tests {
    use fs::Fs as _;
    use gpui::TestAppContext;

    use super::*;

    /// ADR-F96 stage 2: the page's switch writes `allow_unrestricted_network` to the user's own
    /// settings file, the one it is read from, so the switch then reads as what it wrote; the
    /// neighbouring "any host" permission is left as it was.
    #[gpui::test]
    async fn test_unrestricted_network_switch_writes_the_user_settings_file(
        cx: &mut TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.executor());
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });
        fs.create_dir(
            paths::settings_file()
                .parent()
                .expect("settings file has a parent directory"),
        )
        .await
        .expect("settings directory can be created");
        fs.save(
            paths::settings_file(),
            &r#"{ "agent": { "sandbox_permissions": { "allow_all_hosts": false } } }"#.into(),
            language::LineEnding::Unix,
        )
        .await
        .expect("settings file can be written");

        for value in [true, false, true] {
            cx.update(|cx| set_allow_unrestricted_network(value, cx));
            cx.run_until_parked();
            let text = fs
                .load(paths::settings_file())
                .await
                .expect("the settings file");
            let written: serde_json::Value =
                serde_json::from_str(&text).expect("the settings file is JSON");
            let permissions = &written["agent"]["sandbox_permissions"];
            assert_eq!(
                permissions["allow_unrestricted_network"],
                serde_json::Value::Bool(value),
                "{text}"
            );
            assert_eq!(
                permissions["allow_all_hosts"],
                serde_json::Value::Bool(false),
                "{text}"
            );
            assert_eq!(
                cx.update(|cx| project::agent_sandbox::unrestricted_network_allowed(cx)),
                value
            );
        }
    }
}
