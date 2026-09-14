use std::path::PathBuf;

use agent_settings::AgentSettings;
use gpui::{ReadGlobal as _, ScrollHandle, prelude::*};
use http_proxy::HostPattern;
use settings::{Settings as _, SettingsStore};
use ui::{Banner, Divider, Severity, SwitchField, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::SettingsWindow;
use crate::components::{SettingsInputField, SettingsSectionHeader};

const DOMAINS_DESCRIPTION: &str = "各項目は完全一致のドメイン (github.com)、または先頭の *. によるサブドメインのワイルドカード (*.npmjs.org) です。IP アドレスとローカルドメインは指定できません。ここでの許可が効くのは IDE 全体のネットワーク許可リストの範囲内だけで、そちらに無いホストは、ここに登録しても接続できません。";

/// The "allow every domain" switch lifts the *sandbox's* own host list, not the
/// editor-wide egress allowlist that list sits inside — see ADR-F17. Saying so
/// here keeps the switch from reading as an escape hatch from that allowlist,
/// which is exactly what it is not.
const ALLOW_ALL_HOSTS_DESCRIPTION: &str = "サンドボックス内のコマンドが、確認なしで任意のドメインへネットワーク接続できるようにします。外すのはサンドボックス側の制限だけなので、実際に接続できるのは IDE 全体のネットワーク許可リストの範囲内です (ローカルホストは常に接続できます)。";

const WRITE_PATHS_DESCRIPTION: &str = "各項目は絶対パスで指定します。保護された Git のメタデータを除き、その配下すべてに書き込みアクセスを許可します。";

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
                "sandbox-enabled",
                Some("サンドボックスを有効にする"),
                Some(
                    "エージェントが実行するターミナルコマンドを OS レベルのサンドボックスで包みます。無効の間は、コマンドが Zed 本体の権限で実行されるため自動承認が効かず、コマンドごとに確認します。"
                        .into(),
                ),
                sandbox_enabled,
                move |state, _window, cx| {
                    set_sandbox_enabled(*state == ToggleState::Selected, cx);
                },
            )
            .tab_index(0),
        )
        .child(
            SwitchField::new(
                "pause-worktree-code-after-agent-writes",
                Some("エージェントが書き込んだフォルダーのコードを再開まで止める"),
                Some(
                    "エージェントがファイルを書き込んだプロジェクトのフォルダーでは、[再開] を押すまで、言語サーバーとフォーマッターがそのフォルダーのコード (プロジェクトの prettier・ESLint の設定、ビルドスクリプトなど) を実行しません。この設定はユーザー設定にだけ保存され、プロジェクトの設定では変えられません。"
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
            let tooltip = format!("{docs_url} を開きます");
            // Wrap in a row so the button shrinks to its content width instead
            // of stretching across the settings page.
            h_flex().child(
                Button::new("sandbox-docs-link", "サンドボックスについて詳しく見る")
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
                        Button::new("dismiss-sandbox-host-error", "閉じる")
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
                .child(SettingsSectionHeader::new("ネットワーク").no_padding(true))
                .child(
                    SwitchField::new(
                        "sandbox-allow-all-hosts",
                        Some("すべてのドメインを許可"),
                        Some(ALLOW_ALL_HOSTS_DESCRIPTION.into()),
                        permissions.allow_all_hosts,
                        move |state, _window, cx| {
                            set_allow_all_hosts(*state == ToggleState::Selected, cx);
                        },
                    )
                    .tab_index(0),
                )
                .child(render_list_section(
                    "許可するドメイン",
                    DOMAINS_DESCRIPTION,
                    host_rows,
                    add_host_input,
                    empty_border,
                )),
        )

        .child(Divider::horizontal())
        .child(
            v_flex()
                .gap_4()
                .child(SettingsSectionHeader::new("ファイルシステム").no_padding(true))
                .child(
                    SwitchField::new(
                        "sandbox-allow-fs-write-all",
                        Some("ファイルシステムへの書き込みをすべて許可"),
                        Some(
                            "サンドボックス内のコマンドが、保護されたパス (Git のメタデータなど) を除きどこにでも書き込めるようにします。サンドボックスの外で実行するのとほぼ同じになるため、有効の間は自動承認が効かず、コマンドごとに確認します。"
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
                    "書き込みを許可するパス",
                    WRITE_PATHS_DESCRIPTION,
                    path_rows,
                    add_path_input,
                    empty_border,
                )),
        )
        .child(Divider::horizontal())
        .child(
            v_flex()
                .gap_4()
                .child(SettingsSectionHeader::new("権限昇格の確認").no_padding(true))
                .child(
                    SwitchField::new(
                        "sandbox-warn-confusable-unicode",
                        Some("紛らわしい Unicode 文字を警告"),
                        Some(
                            "許可を求めるダイアログで、紛らわしい Unicode 文字 (キリル文字の `а` のように見た目がよく似た同形異字など) を含むドメインや書き込みパスが要求されたときに警告します"
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
                        Some("Windows ドライブへの許可を警告"),
                        Some(
                            "Windows のみ: サンドボックスの許可対象が Windows ドライブ上のファイル (WSL 内から DrvFs 経由でアクセスされるもの) のときに警告します。この場合の許可は変換されたパスを通じて適用されるため、Linux ディストリビューション自身のファイルシステム上のファイルに比べてサンドボックスの保証が弱くなります。"
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
            Label::new("設定されていません")
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
                .tooltip(Tooltip::text("ドメインを削除"))
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
        .with_placeholder("ドメインを追加 (例: github.com、*.npmjs.org)…")
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
                .tooltip(Tooltip::text("パスを削除"))
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
        .with_placeholder("絶対パスを追加 (例: /path/to/directory)…")
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
            HostPatternError::Empty => "ドメインを入力してください。".to_string(),
            HostPatternError::IpLiteral(_) => {
                "IP アドレスとローカルドメインは指定できません。github.com のようなドメインを入力してください。"
                    .to_string()
            }
            HostPatternError::InvalidWildcard(_) => {
                "ワイルドカードは先頭のラベルにのみ指定できます (例: *.github.com)。".to_string()
            }
            HostPatternError::Invalid { .. } => {
                "有効なドメインではありません。github.com や *.npmjs.org のような形式で指定してください。".to_string()
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
fn insert_write_path_subtree(paths: &mut Vec<settings::GrantedWritePathContent>, path: PathBuf) {
    if paths.iter().any(|entry| path.starts_with(&entry.requested)) {
        return;
    }
    paths.retain(|entry| !entry.requested.starts_with(&path));
    paths.push(settings::GrantedWritePathContent {
        requested: path,
        resolved: None,
        on_windows_fs: false,
    });
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
            paths.0.retain(|entry| entry.requested != old_path);
            insert_write_path_subtree(&mut paths.0, new_path);
        }
    });
}

fn remove_write_path(path: PathBuf, cx: &mut App) {
    update_sandbox_permissions(cx, move |permissions| {
        if let Some(paths) = permissions.write_paths.as_mut() {
            paths.0.retain(|entry| entry.requested != path);
        }
    });
}
