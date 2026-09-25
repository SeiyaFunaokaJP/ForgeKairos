use gpui::{Action as _, App};
use itertools::Itertools as _;
use settings::{
    AudioInputDeviceName, AudioOutputDeviceName, EditPredictionDataCollectionChoice,
    LanguageSettingsContent, SemanticTokens, SettingsContent,
};
use std::sync::{Arc, OnceLock};
use strum::{EnumMessage, IntoDiscriminant as _, VariantArray};
use theme::SystemAppearance;
use ui::IntoElement;

use crate::{
    ActionLink, DynamicItem, PROJECT, SettingField, SettingItem, SettingsFieldMetadata,
    SettingsPage, SettingsPageItem, SubPageLink, SubPageType, USER, active_language,
    all_language_names,
    pages::{
        open_audio_test_window, render_acknowledged_ai_providers_page,
        render_edit_prediction_setup_page, render_external_agents_page,
        render_launch_approvals_page, render_llm_providers_page, render_mcp_servers_page,
        render_network_allowlist_page, render_sandbox_settings_page, render_skills_setup_page,
        render_tool_permissions_setup_page,
    },
};

const DEFAULT_STRING: String = String::new();
/// A default empty string reference. Useful in `pick` functions for cases either in dynamic item fields, or when dealing with `settings::Maybe`
/// to avoid the "NO DEFAULT" case.
const DEFAULT_EMPTY_STRING: Option<&String> = Some(&DEFAULT_STRING);

const DEFAULT_AUDIO_OUTPUT: AudioOutputDeviceName = AudioOutputDeviceName(None);
const DEFAULT_EMPTY_AUDIO_OUTPUT: Option<&AudioOutputDeviceName> = Some(&DEFAULT_AUDIO_OUTPUT);
const DEFAULT_AUDIO_INPUT: AudioInputDeviceName = AudioInputDeviceName(None);
const DEFAULT_EMPTY_AUDIO_INPUT: Option<&AudioInputDeviceName> = Some(&DEFAULT_AUDIO_INPUT);

macro_rules! concat_sections {
    (@vec, $($arr:expr),+ $(,)?) => {{
        let total_len = 0_usize $(+ $arr.len())+;
        let mut out = Vec::with_capacity(total_len);

        $(
            out.extend($arr);
        )+

        out
    }};

    ($($arr:expr),+ $(,)?) => {{
        let total_len = 0_usize $(+ $arr.len())+;

        let mut out: Box<[std::mem::MaybeUninit<_>]> = Box::new_uninit_slice(total_len);

        let mut index = 0usize;
        $(
            let array = $arr;
            for item in array {
                out[index].write(item);
                index += 1;
            }
        )+

        debug_assert_eq!(index, total_len);

        // SAFETY: we wrote exactly `total_len` elements.
        unsafe { out.assume_init() }
    }};
}

pub(crate) fn settings_data(cx: &App) -> Vec<SettingsPage> {
    vec![
        general_page(cx),
        appearance_page(),
        keymap_page(),
        editor_page(),
        languages_and_tools_page(cx),
        search_and_files_page(),
        window_and_layout_page(),
        panels_page(),
        debugger_page(),
        terminal_page(),
        version_control_page(),
        collaboration_page(),
        ai_page(cx),
        network_page(),
        browser_page(),
        developer_page(cx),
    ]
}

fn developer_page(cx: &App) -> SettingsPage {
    use feature_flags::FeatureFlagAppExt as _;

    let mut items: Vec<SettingsPageItem> = Vec::new();

    // Feature flag overrides are a staff-only affordance, so only surface the section when the overrides are enabled.
    if cx.feature_flag_overrides_enabled() {
        items.push(SettingsPageItem::SectionHeader("機能フラグ"));
        items.push(SettingsPageItem::SubPageLink(SubPageLink {
            title: "機能フラグ".into(),
            r#type: Default::default(),
            description: None,
            search_aliases: &[],
            json_path: Some("feature_flags"),
            in_json: true,
            files: USER,
            render: crate::pages::render_feature_flags_page,
        }));
    }

    items.push(SettingsPageItem::SectionHeader("計測"));
    items.push(SettingsPageItem::SettingItem(SettingItem {
        title: "パフォーマンスプロファイラー",
        description: "フォアグラウンドとバックグラウンドの実行タスクのタイミングデータを収集し、`zed: open performance profiler` で確認できるようにします。メモリ使用量が増えることがあります。",
        field: Box::new(SettingField {
            organization_override: None,
            json_path: Some("instrumentation.performance_profiler.enabled"),
            pick: |settings_content| {
                settings_content
                    .instrumentation
                    .as_ref()
                    .and_then(|i| i.performance_profiler.as_ref())
                    .and_then(|p| p.enabled.as_ref())
            },
            write: |settings_content, value, _| {
                settings_content
                    .instrumentation
                    .get_or_insert_default()
                    .performance_profiler
                    .get_or_insert_default()
                    .enabled = value;
            },
        }),
        metadata: None,
        files: USER,
    }));

    SettingsPage {
        title: "開発者",
        items: items.into_boxed_slice(),
    }
}

fn general_page(cx: &App) -> SettingsPage {
    fn general_settings_section(_cx: &App) -> Vec<SettingsPageItem> {
        vec![
            SettingsPageItem::SectionHeader("全般設定"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アクセシビリティモード",
                description: "スクリーンリーダーなどの支援技術向けに Zed のインターフェースを最適化します。有効にすると、通常は折りたたまれるコントロールが展開されたままになり、キーボードで操作できます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("accessible_mode"),
                    pick: |settings_content| settings_content.workspace.accessible_mode.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.accessible_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブがない状態で閉じたとき",
                description: "タブがない状態で「アクティブな項目を閉じる」アクションを使ったときの動作です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("when_closing_with_no_tabs"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .when_closing_with_no_tabs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.when_closing_with_no_tabs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "最後のウィンドウを閉じたとき",
                description: "最後のウィンドウが閉じられたときの動作です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("on_last_window_closed"),
                    pick: |settings_content| {
                        settings_content.workspace.on_last_window_closed.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.on_last_window_closed = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "システムのファイルダイアログを使用",
                description: "「開く」と「名前を付けて保存」に OS ネイティブのダイアログを使用します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_system_path_prompts"),
                    pick: |settings_content| {
                        settings_content.workspace.use_system_path_prompts.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.use_system_path_prompts = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "システムの確認ダイアログを使用",
                description: "確認ダイアログに OS ネイティブのダイアログを使用します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_system_prompts"),
                    pick: |settings_content| settings_content.workspace.use_system_prompts.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.use_system_prompts = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プライベートな値を伏せる",
                description: "プライベートファイル内の変数の値を隠します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("redact_private_values"),
                    pick: |settings_content| settings_content.editor.redact_private_values.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.redact_private_values = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プライベートファイル",
                description: "ファイルがプライベートかどうかを判定するために、ファイルパスと照合する glob です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("worktree.private_files"),
                        pick: |settings_content| {
                            settings_content.project.worktree.private_files.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.private_files = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "CLI の既定の開き方",
                description: "フラグを指定しなかったときに `zed <path>` がディレクトリをどう開くかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("cli_default_open_behavior"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .cli_default_open_behavior
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.cli_default_open_behavior = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定の開き方",
                description: "UI からプロジェクトを開くときの既定の動作です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("default_open_behavior"),
                    pick: |settings_content| {
                        settings_content.workspace.default_open_behavior.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.default_open_behavior = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]
    }
    fn security_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("セキュリティ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "すべてのプロジェクトを既定で信頼",
                description: "Zed を開くときにすべてのプロジェクトを自動的に信頼して制限モードを回避し、プロジェクトごとに許可を与えなくてもすべての機能を使えるようにします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("session.trust_all_projects"),
                    pick: |settings_content| {
                        settings_content
                            .session
                            .as_ref()
                            .and_then(|session| session.trust_all_worktrees.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .session
                            .get_or_insert_default()
                            .trust_all_worktrees = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // The decisions ADR-F42 stores for MCP servers, language server
            // binaries and external formatters, with the only place to take
            // one back (`docs/jp-enhance/DESIGN.md` §4.42, §4.50).
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "起動の許可・不許可の記録".into(),
                r#type: SubPageType::LaunchApprovals,
                json_path: None,
                description: Some(
                    "プロジェクトの設定が起動しようとした MCP サーバー・言語サーバー・外部フォーマッターについて、許可・不許可の記録を確認して取り消します。"
                        .into(),
                ),
                search_aliases: &[
                    "approval",
                    "formatter",
                    "language server",
                    "launch",
                    "lsp",
                    "mcp",
                    "revoke",
                ],
                in_json: false,
                files: USER,
                render: render_launch_approvals_page,
            }),
        ]
    }

    fn workspace_restoration_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("ワークスペースの復元"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "未保存のバッファーを復元",
                description: "再起動時に未保存のバッファーを復元するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("session.restore_unsaved_buffers"),
                    pick: |settings_content| {
                        settings_content
                            .session
                            .as_ref()
                            .and_then(|session| session.restore_unsaved_buffers.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .session
                            .get_or_insert_default()
                            .restore_unsaved_buffers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "起動時に復元する内容",
                description: "Zed を開いたときに前回のセッションから何を復元するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("restore_on_startup"),
                    pick: |settings_content| settings_content.workspace.restore_on_startup.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.restore_on_startup = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scoped_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("適用範囲付きの設定"),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "Preview チャンネル",
                description: "Zed の Preview ビルドでのみ有効にする設定です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("preview_channel_settings"),
                        pick: |settings_content| Some(settings_content),
                        write: |_settings_content, _value, _| {},
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "設定プロファイル",
                description: "既存のユーザー設定に一時的に上書き適用する設定プロファイルを、任意の数だけ定義できます。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("settings_profiles"),
                        pick: |settings_content| Some(settings_content),
                        write: |_settings_content, _value, _| {},
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
        ]
    }

    fn privacy_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("プライバシー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "診断テレメトリ",
                description: "クラッシュレポートなどのデバッグ情報を送信します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("telemetry.diagnostics"),
                    pick: |settings_content| {
                        settings_content
                            .telemetry
                            .as_ref()
                            .and_then(|telemetry| telemetry.diagnostics.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .telemetry
                            .get_or_insert_default()
                            .diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "利用状況テレメトリ",
                description: "Zed でどの言語を使っているかなど、匿名化された利用データを送信します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("telemetry.metrics"),
                    pick: |settings_content| {
                        settings_content
                            .telemetry
                            .as_ref()
                            .and_then(|telemetry| telemetry.metrics.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.telemetry.get_or_insert_default().metrics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Anthropic のデータ保持",
                description: "Zero Data Retention では提供できない Anthropic モデルへのリクエスト送信を許可します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("telemetry.anthropic_retention"),
                    pick: |settings_content| {
                        settings_content
                            .telemetry
                            .as_ref()
                            .and_then(|telemetry| telemetry.anthropic_retention.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .telemetry
                            .get_or_insert_default()
                            .anthropic_retention = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn auto_update_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("自動更新"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "自動更新",
                description: "更新を自動的に確認するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("auto_update"),
                    pick: |settings_content| settings_content.auto_update.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.auto_update = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "全般",
        items: concat_sections!(
            @vec,
            general_settings_section(cx),
            security_section(),
            workspace_restoration_section(),
            scoped_settings_section(),
            privacy_section(),
            auto_update_section(),
        )
        .into(),
    }
}

fn appearance_page() -> SettingsPage {
    fn theme_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("テーマ"),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "テーマモード",
                    description: "固定のテーマを使うか、外観とライト/ダークモードに応じてテーマを動的に切り替えるかを選択します。",
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("theme$"),
                        pick: |settings_content| {
                            Some(&dynamic_variants::<settings::ThemeSelection>()[
                                settings_content
                                    .theme
                                    .theme
                                    .as_ref()?
                                    .discriminant() as usize])
                        },
                        write: |settings_content, value, app: &App| {
                            let Some(value) = value else {
                                settings_content.theme.theme = None;
                                return;
                            };
                            let settings_value = settings_content.theme.theme.get_or_insert_default();
                            *settings_value = match value {
                                settings::ThemeSelectionDiscriminants::Static => {
                                    let name = match settings_value {
                                        settings::ThemeSelection::Static(_) => return,
                                        settings::ThemeSelection::Dynamic { mode, light, dark } => {
                                            match mode {
                                                theme_settings::ThemeAppearanceMode::Light => light.clone(),
                                                theme_settings::ThemeAppearanceMode::Dark => dark.clone(),
                                                theme_settings::ThemeAppearanceMode::System => {
                                                    if SystemAppearance::global(app).is_light() {
                                                        light.clone()
                                                    } else {
                                                        dark.clone()
                                                    }
                                                }
                                            }
                                        },
                                    };
                                    settings::ThemeSelection::Static(name)
                                },
                                settings::ThemeSelectionDiscriminants::Dynamic => {
                                    let static_name = match settings_value {
                                        settings::ThemeSelection::Static(theme_name) => theme_name.clone(),
                                        settings::ThemeSelection::Dynamic {..} => return,
                                    };

                                    settings::ThemeSelection::Dynamic {
                                        mode: settings::ThemeAppearanceMode::System,
                                        light: static_name.clone(),
                                        dark: static_name,
                                    }
                                },
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.theme.theme.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::ThemeSelection>().into_iter().map(|variant| {
                    match variant {
                        settings::ThemeSelectionDiscriminants::Static => vec![
                            SettingItem {
                                files: USER,
                                title: "テーマ名",
                                description: "選択したテーマの名前です。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("theme"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Static(name)) => Some(name),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Static(theme_name) => *theme_name = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                        settings::ThemeSelectionDiscriminants::Dynamic => vec![
                            SettingItem {
                                files: USER,
                                title: "モード",
                                description: "選択したライト/ダークテーマのどちらを使うか、または OS の外観設定に従うかを選択します。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("theme.mode"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Dynamic { mode, ..}) => Some(mode),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Dynamic{ mode, ..} => *mode = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "ライトテーマ",
                                description: "モードがライトのとき、またはモードがシステムで OS がライトモードのときに使うテーマです。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("theme.light"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Dynamic { light, ..}) => Some(light),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Dynamic{ light, ..} => *light = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "ダークテーマ",
                                description: "モードがダークのとき、またはモードがシステムで OS がダークモードのときに使うテーマです。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("theme.dark"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Dynamic { dark, ..}) => Some(dark),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Dynamic{ dark, ..} => *dark = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                    }
                }).collect(),
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "アイコンテーマ",
                    description: "Zed がファイルとディレクトリに関連付けるアイコンのセットです。",
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("icon_theme$"),
                        pick: |settings_content| {
                            Some(&dynamic_variants::<settings::IconThemeSelection>()[
                                settings_content
                                    .theme
                                    .icon_theme
                                    .as_ref()?
                                    .discriminant() as usize])
                        },
                        write: |settings_content, value, app| {
                            let Some(value) = value else {
                                settings_content.theme.icon_theme = None;
                                return;
                            };
                            let settings_value = settings_content.theme.icon_theme.get_or_insert_with(|| {
                                settings::IconThemeSelection::Static(settings::IconThemeName(theme::default_icon_theme().name.clone().into()))
                            });
                            *settings_value = match value {
                                settings::IconThemeSelectionDiscriminants::Static => {
                                    let name = match settings_value {
                                        settings::IconThemeSelection::Static(_) => return,
                                        settings::IconThemeSelection::Dynamic { mode, light, dark } => {
                                            match mode {
                                                theme_settings::ThemeAppearanceMode::Light => light.clone(),
                                                theme_settings::ThemeAppearanceMode::Dark => dark.clone(),
                                                theme_settings::ThemeAppearanceMode::System => {
                                                    if SystemAppearance::global(app).is_light() {
                                                        light.clone()
                                                    } else {
                                                        dark.clone()
                                                    }
                                                }
                                            }
                                        },
                                    };
                                    settings::IconThemeSelection::Static(name)
                                },
                                settings::IconThemeSelectionDiscriminants::Dynamic => {
                                    let static_name = match settings_value {
                                        settings::IconThemeSelection::Static(theme_name) => theme_name.clone(),
                                        settings::IconThemeSelection::Dynamic {..} => return,
                                    };

                                    settings::IconThemeSelection::Dynamic {
                                        mode: settings::ThemeAppearanceMode::System,
                                        light: static_name.clone(),
                                        dark: static_name,
                                    }
                                },
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.theme.icon_theme.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::IconThemeSelection>().into_iter().map(|variant| {
                    match variant {
                        settings::IconThemeSelectionDiscriminants::Static => vec![
                            SettingItem {
                                files: USER,
                                title: "アイコンテーマ名",
                                description: "選択したアイコンテーマの名前です。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("icon_theme$string"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Static(name)) => Some(name),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Static(theme_name)) => *theme_name = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                        settings::IconThemeSelectionDiscriminants::Dynamic => vec![
                            SettingItem {
                                files: USER,
                                title: "モード",
                                description: "選択したライト/ダークのアイコンテーマのどちらを使うか、または OS の外観設定に従うかを選択します。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("icon_theme"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Dynamic { mode, ..}) => Some(mode),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Dynamic{ mode, ..}) => *mode = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "ライトアイコンテーマ",
                                description: "モードがライトのとき、またはモードがシステムで OS がライトモードのときに使うアイコンテーマです。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("icon_theme.light"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Dynamic { light, ..}) => Some(light),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Dynamic{ light, ..}) => *light = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "ダークアイコンテーマ",
                                description: "モードがダークのとき、またはモードがシステムで OS がダークモードのときに使うアイコンテーマです。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("icon_theme.dark"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Dynamic { dark, ..}) => Some(dark),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Dynamic{ dark, ..}) => *dark = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                    }
                }).collect(),
            }),
        ]
    }

    fn buffer_font_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader("バッファーのフォント"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントファミリー",
                description: "エディターのテキストのフォントファミリーです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("buffer_font_family"),
                    pick: |settings_content| settings_content.theme.buffer_font_family.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントサイズ",
                description: "エディターのテキストのフォントサイズです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("buffer_font_size"),
                    pick: |settings_content| settings_content.theme.buffer_font_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントの太さ",
                description: "エディターのテキストのフォントの太さです (100〜900)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("buffer_font_weight"),
                    pick: |settings_content| settings_content.theme.buffer_font_weight.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_weight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "行の高さ",
                    description: "エディターのテキストの行の高さです。",
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("buffer_line_height$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::BufferLineHeight>()[settings_content
                                    .theme
                                    .buffer_line_height
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                settings_content.theme.buffer_line_height = None;
                                return;
                            };
                            let settings_value = settings_content
                                .theme
                                .buffer_line_height
                                .get_or_insert_with(|| settings::BufferLineHeight::default());
                            *settings_value = match value {
                                settings::BufferLineHeightDiscriminants::Comfortable => {
                                    settings::BufferLineHeight::Comfortable
                                }
                                settings::BufferLineHeightDiscriminants::Standard => {
                                    settings::BufferLineHeight::Standard
                                }
                                settings::BufferLineHeightDiscriminants::Custom => {
                                    let custom_value =
                                        theme_settings::buffer_line_height_from_settings(
                                            *settings_value,
                                        )
                                        .value();
                                    settings::BufferLineHeight::Custom(custom_value)
                                }
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .theme
                            .buffer_line_height
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::BufferLineHeight>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::BufferLineHeightDiscriminants::Comfortable => vec![],
                        settings::BufferLineHeightDiscriminants::Standard => vec![],
                        settings::BufferLineHeightDiscriminants::Custom => vec![SettingItem {
                            files: USER,
                            title: "カスタムの行の高さ",
                            description: "行の高さのカスタム値です (1.0 以上)。",
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("buffer_line_height"),
                                pick: |settings_content| match settings_content
                                    .theme
                                    .buffer_line_height
                                    .as_ref()
                                {
                                    Some(settings::BufferLineHeight::Custom(value)) => Some(value),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        return;
                                    };
                                    match settings_content.theme.buffer_line_height.as_mut() {
                                        Some(settings::BufferLineHeight::Custom(line_height)) => {
                                            *line_height = f32::max(value, 1.0)
                                        }
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                    })
                    .collect(),
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "フォント機能",
                description: "テキストバッファーの描画で有効にする OpenType 機能です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("buffer_font_features"),
                        pick: |settings_content| {
                            settings_content.theme.buffer_font_features.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.theme.buffer_font_features = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "フォールバックフォント",
                description: "テキストバッファーの描画に使うフォールバックフォントです。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("buffer_font_fallbacks"),
                        pick: |settings_content| {
                            settings_content.theme.buffer_font_fallbacks.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.theme.buffer_font_fallbacks = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
        ]
    }

    fn ui_font_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("UI のフォント"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントファミリー",
                description: "UI 要素のフォントファミリーです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ui_font_family"),
                    pick: |settings_content| settings_content.theme.ui_font_family.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントサイズ",
                description: "UI 要素のフォントサイズです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ui_font_size"),
                    pick: |settings_content| settings_content.theme.ui_font_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントの太さ",
                description: "UI 要素のフォントの太さです (100〜900)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ui_font_weight"),
                    pick: |settings_content| settings_content.theme.ui_font_weight.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_weight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "フォント機能",
                description: "UI 要素の描画で有効にする OpenType 機能です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("ui_font_features"),
                        pick: |settings_content| settings_content.theme.ui_font_features.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.theme.ui_font_features = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "フォールバックフォント",
                description: "UI の描画に使うフォールバックフォントです。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("ui_font_fallbacks"),
                        pick: |settings_content| settings_content.theme.ui_font_fallbacks.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.theme.ui_font_fallbacks = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
        ]
    }

    fn agent_panel_font_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("エージェントパネルのフォント"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "UI フォントファミリー",
                description: "エージェントパネルの応答テキストのフォントファミリーです。未設定の場合は通常の UI フォントファミリーを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_ui_font_family"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_ui_font_family
                            .as_ref()
                            .or(settings_content.theme.ui_font_family.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_ui_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "UI フォントサイズ",
                description: "エージェントパネルの応答テキストのフォントサイズです。未設定の場合は通常の UI フォントサイズを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_ui_font_size"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_ui_font_size
                            .as_ref()
                            .or(settings_content.theme.ui_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_ui_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "バッファーのフォントファミリー",
                description: "エージェントパネルのユーザーメッセージのフォントファミリーです。未設定の場合は通常のバッファーのフォントファミリーを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_buffer_font_family"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_buffer_font_family
                            .as_ref()
                            .or(settings_content.theme.buffer_font_family.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_buffer_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "バッファーのフォントサイズ",
                description: "エージェントパネルのユーザーメッセージのフォントサイズです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_buffer_font_size"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_buffer_font_size
                            .as_ref()
                            .or(settings_content.theme.buffer_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_buffer_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn markdown_preview_font_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("Markdown プレビューのフォント"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントファミリー",
                description: "Markdown プレビューのフォントファミリーです。未設定の場合は UI フォントファミリーを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("markdown_preview_font_family"),
                    pick: |settings_content| {
                        settings_content.theme.markdown_preview_font_family.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.markdown_preview_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コードのフォントファミリー",
                description: "Markdown プレビュー内のコードブロックのフォントファミリーです。未設定の場合はエディターのフォントファミリーを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("markdown_preview_code_font_family"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .markdown_preview_code_font_family
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.markdown_preview_code_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントサイズ",
                description: "Markdown プレビューのフォントサイズです。未設定の場合はエディターのフォントサイズを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("markdown_preview_font_size"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .markdown_preview_font_size
                            .as_ref()
                            .or(settings_content.theme.buffer_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.markdown_preview_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn text_rendering_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("テキストの描画"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "テキストの描画モード",
                description: "使用するテキストの描画モードです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("text_rendering_mode"),
                    pick: |settings_content| {
                        settings_content.workspace.text_rendering_mode.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.text_rendering_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn cursor_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("カーソル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "複数カーソルの修飾キー",
                description: "カーソルを追加するときの修飾キーです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("multi_cursor_modifier"),
                    pick: |settings_content| settings_content.editor.multi_cursor_modifier.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.multi_cursor_modifier = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの点滅",
                description: "エディターでカーソルを点滅させるかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("cursor_blink"),
                    pick: |settings_content| settings_content.editor.cursor_blink.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.cursor_blink = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの形状",
                description: "エディターのカーソルの形状です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("cursor_shape"),
                    pick: |settings_content| settings_content.editor.cursor_shape.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.cursor_shape = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マウスカーソルを隠す",
                description: "マウスカーソルを隠すタイミングです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hide_mouse"),
                    pick: |settings_content| settings_content.hide_mouse.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.hide_mouse = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "モーションを減らす",
                description: "読み込み中のスピナーなど、必須ではないアニメーションを静止状態で描画して減らすかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("reduce_motion"),
                    pick: |settings_content| settings_content.reduce_motion.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.reduce_motion = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn highlighting_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("ハイライト"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "未使用コードの淡色化",
                description: "未使用のコードをどの程度淡く表示するかを設定します (0.0〜0.9)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("unnecessary_code_fade"),
                    pick: |settings_content| settings_content.theme.unnecessary_code_fade.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.unnecessary_code_fade = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "現在の行のハイライト",
                description: "現在の行をどのようにハイライトするかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("current_line_highlight"),
                    pick: |settings_content| {
                        settings_content.editor.current_line_highlight.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.current_line_highlight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "選択テキストのハイライト",
                description: "選択したテキストと一致するすべての箇所をハイライトします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("selection_highlight"),
                    pick: |settings_content| settings_content.editor.selection_highlight.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.selection_highlight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "選択範囲の角丸",
                description: "テキストの選択範囲の角を丸くするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("rounded_selection"),
                    pick: |settings_content| settings_content.editor.rounded_selection.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.rounded_selection = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ハイライトの最小コントラスト",
                description: "ハイライトの背景の上にテキストを描画するときに保つ、APCA 知覚コントラストの最小値です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimum_contrast_for_highlights"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimum_contrast_for_highlights
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.minimum_contrast_for_highlights = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn guides_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("ガイド"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "折り返しガイドを表示",
                description: "折り返しガイド (縦のルーラー) を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("show_wrap_guides"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .show_wrap_guides
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .show_wrap_guides = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            // todo(settings_ui): This needs a custom component
            SettingsPageItem::SettingItem(SettingItem {
                title: "折り返しガイド",
                description: "折り返しガイドを表示する文字数です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("wrap_guides"),
                        pick: |settings_content| {
                            settings_content
                                .project
                                .all_languages
                                .defaults
                                .wrap_guides
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.all_languages.defaults.wrap_guides = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    let items: Box<[SettingsPageItem]> = concat_sections!(
        theme_section(),
        buffer_font_section(),
        ui_font_section(),
        agent_panel_font_section(),
        markdown_preview_font_section(),
        text_rendering_section(),
        cursor_section(),
        highlighting_section(),
        guides_section(),
    );

    SettingsPage {
        title: "外観",
        items,
    }
}

fn keymap_page() -> SettingsPage {
    fn keybindings_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("キーバインド"),
            SettingsPageItem::ActionLink(ActionLink {
                title: "キーバインドの編集".into(),
                description: Some(
                    "キーマップエディターでキーバインドをカスタマイズします。".into(),
                ),
                button_text: "キーマップを開く".into(),
                on_click: Arc::new(|settings_window, window, cx| {
                    let Some(original_window) = settings_window.original_window else {
                        return;
                    };
                    original_window
                        .update(cx, |_workspace, original_window, cx| {
                            original_window
                                .dispatch_action(zed_actions::OpenKeymap.boxed_clone(), cx);
                            original_window.activate_window();
                        })
                        .ok();
                    window.remove_window();
                }),
                files: USER,
            }),
        ]
    }

    fn base_keymap_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("ベースキーマップ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ベースキーマップ",
                description: "基準にするキーバインドのセットの名前です。Emacs (ベータ) では C-x C-s (保存) / C-x C-f (ファイルを開く) / C-k (行をキル) / C-y (ヤンク) / M-y (直前のヤンクを 1 つ古い世代に差し替え) / C-space (マーク) などが使えます。C-x などのプレフィックスキーの入力中に候補を出したいときは、「エディター」ページの「Which-key メニューを表示」を有効にしてください。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("base_keymap"),
                    pick: |settings_content| settings_content.base_keymap.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.base_keymap = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]
    }

    fn modal_editing_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("モーダル編集"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Vim モード",
                description: "Vim モードとそのキーバインドを有効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim_mode"),
                    pick: |settings_content| settings_content.vim_mode.as_ref(),
                    write: write_vim_mode,
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Helix モード",
                description: "Helix モードとそのキーバインドを有効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("helix_mode"),
                    pick: |settings_content| settings_content.helix_mode.as_ref(),
                    write: write_helix_mode,
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    let items: Box<[SettingsPageItem]> = concat_sections!(
        keybindings_section(),
        base_keymap_section(),
        modal_editing_section(),
    );

    SettingsPage {
        title: "キーマップ",
        items,
    }
}

fn editor_page() -> SettingsPage {
    fn auto_save_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("自動保存"),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "自動保存のモード",
                    description: "バッファーの変更を自動保存するタイミングです。",
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("autosave$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::AutosaveSetting>()[settings_content
                                    .workspace
                                    .autosave
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                settings_content.workspace.autosave = None;
                                return;
                            };
                            let settings_value = settings_content
                                .workspace
                                .autosave
                                .get_or_insert_with(|| settings::AutosaveSetting::Off);
                            *settings_value = match value {
                                settings::AutosaveSettingDiscriminants::Off => {
                                    settings::AutosaveSetting::Off
                                }
                                settings::AutosaveSettingDiscriminants::AfterDelay => {
                                    let milliseconds = match settings_value {
                                        settings::AutosaveSetting::AfterDelay { milliseconds } => {
                                            *milliseconds
                                        }
                                        _ => settings::DelayMs(1000),
                                    };
                                    settings::AutosaveSetting::AfterDelay { milliseconds }
                                }
                                settings::AutosaveSettingDiscriminants::OnFocusChange => {
                                    settings::AutosaveSetting::OnFocusChange
                                }
                                settings::AutosaveSettingDiscriminants::OnWindowChange => {
                                    settings::AutosaveSetting::OnWindowChange
                                }
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.workspace.autosave.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::AutosaveSetting>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::AutosaveSettingDiscriminants::Off => vec![],
                        settings::AutosaveSettingDiscriminants::AfterDelay => vec![SettingItem {
                            files: USER,
                            title: "遅延 (ミリ秒)",
                            description: "操作がない状態が続いた後に保存します (ミリ秒)。",
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("autosave.after_delay.milliseconds"),
                                pick: |settings_content| match settings_content
                                    .workspace
                                    .autosave
                                    .as_ref()
                                {
                                    Some(settings::AutosaveSetting::AfterDelay {
                                        milliseconds,
                                    }) => Some(milliseconds),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        settings_content.workspace.autosave = None;
                                        return;
                                    };
                                    match settings_content.workspace.autosave.as_mut() {
                                        Some(settings::AutosaveSetting::AfterDelay {
                                            milliseconds,
                                        }) => *milliseconds = value,
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                        settings::AutosaveSettingDiscriminants::OnFocusChange => vec![],
                        settings::AutosaveSettingDiscriminants::OnWindowChange => vec![],
                    })
                    .collect(),
            }),
        ]
    }

    fn which_key_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Which-key メニュー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Which-key メニューを表示",
                description: "複数ストロークのキーバインドの入力中に、一致するキーバインドを Which-key メニューとして表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("which_key.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .which_key
                            .as_ref()
                            .and_then(|settings| settings.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.which_key.get_or_insert_default().enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "メニューの遅延",
                description: "Which-key メニューが表示されるまでの遅延 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("which_key.delay_ms"),
                    pick: |settings_content| {
                        settings_content
                            .which_key
                            .as_ref()
                            .and_then(|settings| settings.delay_ms.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.which_key.get_or_insert_default().delay_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn multibuffer_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader("マルチバッファー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マルチバッファーでのダブルクリック",
                description: "マルチバッファーの抜粋内でダブルクリックしたときの動作です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("double_click_in_multibuffer"),
                    pick: |settings_content| {
                        settings_content.editor.double_click_in_multibuffer.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.double_click_in_multibuffer = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "抜粋を展開する行数",
                description: "マルチバッファーの抜粋を既定で何行分展開するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("expand_excerpt_lines"),
                    pick: |settings_content| settings_content.editor.expand_excerpt_lines.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.expand_excerpt_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "抜粋の前後の行数",
                description: "マルチバッファーの抜粋に既定で何行分の前後関係を含めるかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("excerpt_context_lines"),
                    pick: |settings_content| settings_content.editor.excerpt_context_lines.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.excerpt_context_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アウトラインを展開する深さ",
                description: "現在のファイルのアウトライン項目を既定で展開する深さです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.expand_outlines_with_depth"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()
                            .and_then(|outline_panel| {
                                outline_panel.expand_outlines_with_depth.as_ref()
                            })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .expand_outlines_with_depth = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "差分の表示スタイル",
                description: "エディターで差分をどのように表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diff_view_style"),
                    pick: |settings_content| settings_content.editor.diff_view_style.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.diff_view_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "左右分割の差分の最小幅",
                description: "左右分割の差分表示を使う最小の幅 (列数) です。エディターがこれより狭い場合は自動的にインライン表示に切り替わります。0 で無効になります。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimum_split_diff_width"),
                    pick: |settings_content| {
                        settings_content.editor.minimum_split_diff_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.minimum_split_diff_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scrolling_section() -> [SettingsPageItem; 9] {
        [
            SettingsPageItem::SectionHeader("スクロール"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "最終行を越えてスクロール",
                description: "エディターで最終行より先までスクロールできるようにします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scroll_beyond_last_line"),
                    pick: |settings_content| {
                        settings_content.editor.scroll_beyond_last_line.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.scroll_beyond_last_line = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "垂直スクロールの余白",
                description: "自動スクロール時にカーソルの上下に残す行数です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vertical_scroll_margin"),
                    pick: |settings_content| {
                        settings_content.editor.vertical_scroll_margin.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.vertical_scroll_margin = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "水平スクロールの余白",
                description: "マウスでスクロールするときに左右に残す文字数です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("horizontal_scroll_margin"),
                    pick: |settings_content| {
                        settings_content.editor.horizontal_scroll_margin.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.horizontal_scroll_margin = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スクロールの感度",
                description: "水平・垂直スクロールの感度の倍率です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scroll_sensitivity"),
                    pick: |settings_content| settings_content.editor.scroll_sensitivity.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.scroll_sensitivity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マウスホイールでズーム",
                description: "主修飾キーを押しながらマウスホイールを回したときに、エディターのフォントサイズを拡大縮小するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("mouse_wheel_zoom"),
                    pick: |settings_content| settings_content.editor.mouse_wheel_zoom.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.mouse_wheel_zoom = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "高速スクロールの感度",
                description: "水平・垂直の高速スクロールの感度の倍率です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("fast_scroll_sensitivity"),
                    pick: |settings_content| {
                        settings_content.editor.fast_scroll_sensitivity.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.fast_scroll_sensitivity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "クリック時の自動スクロール",
                description: "表示領域の端の近くをクリックしたときにスクロールするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("autoscroll_on_clicks"),
                    pick: |settings_content| settings_content.editor.autoscroll_on_clicks.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.autoscroll_on_clicks = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スティッキースクロール",
                description: "スコープをエディターの上部に固定表示するかどうかを設定します",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("sticky_scroll.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .sticky_scroll
                            .as_ref()
                            .and_then(|sticky_scroll| sticky_scroll.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .sticky_scroll
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn signature_help_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("シグネチャヘルプ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "シグネチャヘルプの自動表示",
                description: "シグネチャヘルプのポップアップを自動的に表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("auto_signature_help"),
                    pick: |settings_content| settings_content.editor.auto_signature_help.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.auto_signature_help = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "編集後にシグネチャヘルプを表示",
                description: "補完や括弧の対を挿入した後にシグネチャヘルプのポップアップを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("show_signature_help_after_edits"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .show_signature_help_after_edits
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.show_signature_help_after_edits = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スニペットの並び順",
                description: "スニペットを他の補完候補に対してどう並べるかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("snippet_sort_order"),
                    pick: |settings_content| settings_content.editor.snippet_sort_order.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.snippet_sort_order = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn hover_popover_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("ホバーポップアップ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "エディター内のシンボルにマウスを重ねたときに情報ポップアップを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_enabled"),
                    pick: |settings_content| settings_content.editor.hover_popover_enabled.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings ui): add units to this number input
            SettingsPageItem::SettingItem(SettingItem {
                title: "遅延",
                description: "情報ポップアップを表示するまでの待ち時間 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_delay"),
                    pick: |settings_content| settings_content.editor.hover_popover_delay.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_delay = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "固定表示",
                description: "マウスをポップアップの方へ動かしたときにホバーポップアップを残し、内容を操作できるようにするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_sticky"),
                    pick: |settings_content| settings_content.editor.hover_popover_sticky.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_sticky = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings ui): add units to this number input
            SettingsPageItem::SettingItem(SettingItem {
                title: "非表示までの遅延",
                description: "マウスが離れてからホバーポップアップを隠すまでの待ち時間 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_hiding_delay"),
                    pick: |settings_content| {
                        settings_content.editor.hover_popover_hiding_delay.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_hiding_delay = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn drag_and_drop_selection_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("ドラッグ&ドロップによる選択"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "ドラッグ&ドロップによる選択を有効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("drag_and_drop_selection.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .as_ref()
                            .and_then(|drag_and_drop| drag_and_drop.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "遅延",
                description: "ドラッグ&ドロップによる選択が始まるまでの遅延 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("drag_and_drop_selection.delay"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .as_ref()
                            .and_then(|drag_and_drop| drag_and_drop.delay.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .get_or_insert_default()
                            .delay = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn gutter_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader("ガター"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "行番号を表示",
                description: "ガターに行番号を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.line_numbers"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.line_numbers.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .line_numbers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "相対行番号",
                description: "エディターのガターの行番号表示を制御します。\"disabled\" は絶対行番号を、\"enabled\" は各絶対行に対する相対行番号を、\"wrapped\" は折り返し行も含めたすべての行に対する相対行番号を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("relative_line_numbers"),
                    pick: |settings_content| settings_content.editor.relative_line_numbers.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.relative_line_numbers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "実行ボタンを表示",
                description: "ガターに実行ボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.runnables"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.runnables.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .runnables = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブレークポイントを表示",
                description: "ガターにブレークポイントを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.breakpoints"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.breakpoints.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .breakpoints = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブックマークを表示",
                description: "ガターにブックマークを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.bookmarks"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.bookmarks.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .bookmarks = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "折りたたみコントロールを表示",
                description: "ガターにコードの折りたたみコントロールを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.folds"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.folds.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.gutter.get_or_insert_default().folds = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "行番号の最小桁数",
                description: "ガターに領域を確保しておく最小の文字数です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.min_line_number_digits"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.min_line_number_digits.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .min_line_number_digits = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    title: "Git ガターの幅",
                    description: "ガターに表示する Git 差分インジケーターの幅です。既定ではバッファーのフォントサイズに合わせて拡大縮小します。",
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("gutter.git_gutter_width$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::GitGutterWidth>()[settings_content
                                    .editor
                                    .gutter
                                    .as_ref()?
                                    .git_gutter_width
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let gutter = settings_content.editor.gutter.get_or_insert_default();
                            gutter.git_gutter_width = value.map(|value| match value {
                                settings::GitGutterWidthDiscriminants::Default => {
                                    settings::GitGutterWidth::Default
                                }
                                settings::GitGutterWidthDiscriminants::Custom => {
                                    let width = match gutter.git_gutter_width {
                                        Some(settings::GitGutterWidth::Custom(width)) => {
                                            settings::PixelSetting(*width)
                                        }
                                        _ => settings::PixelSetting(3.0),
                                    };
                                    settings::GitGutterWidth::Custom(width)
                                }
                            });
                        },
                    }),
                    metadata: None,
                    files: USER,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .editor
                            .gutter
                            .as_ref()?
                            .git_gutter_width
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::GitGutterWidth>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::GitGutterWidthDiscriminants::Default => vec![],
                        settings::GitGutterWidthDiscriminants::Custom => vec![SettingItem {
                            files: USER,
                            title: "カスタム幅",
                            description: "Git 差分インジケーターの幅 (ピクセル) です。",
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("gutter.git_gutter_width"),
                                pick: |settings_content| match settings_content
                                    .editor
                                    .gutter
                                    .as_ref()
                                    .and_then(|gutter| gutter.git_gutter_width.as_ref())
                                {
                                    Some(settings::GitGutterWidth::Custom(value)) => Some(value),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        return;
                                    };
                                    if let Some(settings::GitGutterWidth::Custom(width)) =
                                        settings_content
                                            .editor
                                            .gutter
                                            .as_mut()
                                            .and_then(|gutter| gutter.git_gutter_width.as_mut())
                                    {
                                        *width = value;
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                    })
                    .collect(),
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "インラインのコードアクション",
                description: "バッファーの行頭にコードアクションのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("inline_code_actions"),
                    pick: |settings_content| settings_content.editor.inline_code_actions.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.inline_code_actions = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scrollbar_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader("スクロールバー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "表示",
                description: "エディターにスクロールバーを表示するタイミングです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar"),
                    pick: |settings_content| {
                        settings_content.editor.scrollbar.as_ref()?.show.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソル",
                description: "スクロールバーにカーソル位置を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.cursors"),
                    pick: |settings_content| {
                        settings_content.editor.scrollbar.as_ref()?.cursors.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .cursors = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git の差分",
                description: "スクロールバーに Git の差分インジケーターを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.git_diff"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .git_diff
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .git_diff = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "検索結果",
                description: "スクロールバーにバッファー内検索の結果インジケーターを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.search_results"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .search_results
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .search_results = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "選択テキスト",
                description: "スクロールバーに選択テキストと一致する箇所を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.selected_text"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .selected_text
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .selected_text = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "選択シンボル",
                description: "スクロールバーに選択シンボルと一致する箇所を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.selected_symbol"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .selected_symbol
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .selected_symbol = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "診断",
                description: "スクロールバーに表示する診断インジケーターの種類です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.diagnostics"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .diagnostics
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "水平スクロールバー",
                description: "false にすると水平スクロールバーを強制的に無効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.axes.horizontal"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .axes
                            .as_ref()?
                            .horizontal
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .axes
                            .get_or_insert_default()
                            .horizontal = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "垂直スクロールバー",
                description: "false にすると垂直スクロールバーを強制的に無効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.axes.vertical"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .axes
                            .as_ref()?
                            .vertical
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .axes
                            .get_or_insert_default()
                            .vertical = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn minimap_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader("ミニマップ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "表示",
                description: "エディターにミニマップを表示するタイミングです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.show"),
                    pick: |settings_content| {
                        settings_content.editor.minimap.as_ref()?.show.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.minimap.get_or_insert_default().show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "表示する場所",
                description: "エディターのどこにミニマップを表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.display_in"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()?
                            .display_in
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .display_in = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "つまみ",
                description: "ミニマップのつまみを表示するタイミングです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.thumb"),
                    pick: |settings_content| {
                        settings_content.editor.minimap.as_ref()?.thumb.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .thumb = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "つまみの境界線",
                description: "ミニマップのスクロールバーのつまみの境界線のスタイルです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.thumb_border"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()?
                            .thumb_border
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .thumb_border = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "現在の行のハイライト",
                description: "ミニマップで現在の行をどのようにハイライトするかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.current_line_highlight"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()
                            .and_then(|minimap| minimap.current_line_highlight.as_ref())
                            .or(settings_content.editor.current_line_highlight.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .current_line_highlight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "最大幅 (列数)",
                description: "ミニマップに表示する最大の列数です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.max_width_columns"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()?
                            .max_width_columns
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .max_width_columns = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn toolbar_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader("ツールバー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "パンくずリスト",
                description: "パンくずリストを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.breadcrumbs"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .breadcrumbs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .breadcrumbs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "クイックアクション",
                description: "クイックアクションのボタン (検索・選択・エディター操作など) を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.quick_actions"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .quick_actions
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .quick_actions = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Markdown 整形バー",
                description: "Markdown ファイルを開いているときに、ツールバーの下へ整形ボタンの行を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.markdown_format_bar"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .markdown_format_bar
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .markdown_format_bar = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "選択範囲メニュー",
                description: "エディターのツールバーに選択範囲メニューを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.selections_menu"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .selections_menu
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .selections_menu = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントのレビュー",
                description: "エディターのツールバーにエージェントのレビューボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.agent_review"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .agent_review
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .agent_review = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コードアクション",
                description: "エディターのツールバーにコードアクションのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.code_actions"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .code_actions
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .code_actions = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn vim_settings_section() -> [SettingsPageItem; 14] {
        [
            SettingsPageItem::SectionHeader("Vim"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定のモード",
                description: "Vim の開始時のモードです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.default_mode"),
                    pick: |settings_content| settings_content.vim.as_ref()?.default_mode.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.vim.get_or_insert_default().default_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "相対行番号の切り替え",
                description: "Vim モードで相対行番号を切り替えます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.toggle_relative_line_numbers"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .toggle_relative_line_numbers
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .toggle_relative_line_numbers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "システムのクリップボードを使用",
                description: "Vim モードでシステムのクリップボードを使うタイミングを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.use_system_clipboard"),
                    pick: |settings_content| {
                        settings_content.vim.as_ref()?.use_system_clipboard.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .use_system_clipboard = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "smartcase 検索を使用",
                description: "Vim モードで smartcase 検索を有効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.use_smartcase_find"),
                    pick: |settings_content| {
                        settings_content.vim.as_ref()?.use_smartcase_find.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .use_smartcase_find = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "置換を既定で行全体に適用",
                description: ":substitute コマンドが既定で行内のすべての一致を置換するようにします。この場合、'g' フラグは逆に 1 件だけの置換に切り替えます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.gdefault"),
                    pick: |settings_content| settings_content.vim.as_ref()?.gdefault.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.vim.get_or_insert_default().gdefault = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ヤンク時のハイライト時間",
                description: "Vim モードでヤンクしたテキストをハイライトする時間 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.highlight_on_yank_duration"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .highlight_on_yank_duration
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .highlight_on_yank_duration = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "正規表現検索",
                description: "Vim の検索で既定で正規表現を使用します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.use_regex_search"),
                    pick: |settings_content| {
                        settings_content.vim.as_ref()?.use_regex_search.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .use_regex_search = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ノーマルモードで編集予測を表示",
                description: "ノーマルモードで編集予測を表示するかどうかを設定します。既定では挿入モードと置換モードでのみ表示されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.show_edit_predictions_in_normal_mode"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .show_edit_predictions_in_normal_mode
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .show_edit_predictions_in_normal_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの形状 - ノーマルモード",
                description: "ノーマルモードのカーソルの形状です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.normal"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .normal
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .normal = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの形状 - 挿入モード",
                description: "挿入モードのカーソルの形状です。Inherit はエディターのカーソルの形状を使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.insert"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .insert
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .insert = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの形状 - 置換モード",
                description: "置換モードのカーソルの形状です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.replace"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .replace
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .replace = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの形状 - ビジュアルモード",
                description: "ビジュアルモードのカーソルの形状です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.visual"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .visual
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .visual = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カスタムダイグラフ",
                description: "Vim モードのカスタムダイグラフの定義です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("vim.custom_digraphs"),
                        pick: |settings_content| {
                            settings_content.vim.as_ref()?.custom_digraphs.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.vim.get_or_insert_default().custom_digraphs = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    let items = concat_sections!(
        auto_save_section(),
        which_key_section(),
        multibuffer_section(),
        scrolling_section(),
        signature_help_section(),
        hover_popover_section(),
        drag_and_drop_selection_section(),
        gutter_section(),
        scrollbar_section(),
        minimap_section(),
        toolbar_section(),
        vim_settings_section(),
        language_settings_data(),
    );

    SettingsPage {
        title: "エディター",
        items: items,
    }
}

fn languages_and_tools_page(cx: &App) -> SettingsPage {
    fn file_types_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("ファイルの種類"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルの関連付け",
                description: "言語と、その言語として扱うファイル名・拡張子の対応です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("file_type_associations"),
                        pick: |settings_content| {
                            settings_content.project.all_languages.file_types.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.all_languages.file_types = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn diagnostics_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("診断"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "最大の重大度",
                description: "エディターに表示する診断を絞り込む重大度です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics_max_severity"),
                    pick: |settings_content| {
                        settings_content.editor.diagnostics_max_severity.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.diagnostics_max_severity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "警告を含める",
                description: "既定で警告を表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.include_warnings"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .include_warnings
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .include_warnings = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn inline_diagnostics_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("インライン診断"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "診断をインラインで表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "更新のデバウンス",
                description: "最後の診断の更新からインライン診断を表示するまでの遅延 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.update_debounce_ms"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .update_debounce_ms
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .update_debounce_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "余白",
                description: "ソース行の末尾からインライン診断の開始位置までの余白です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.padding"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .padding = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "最小の列",
                description: "インライン診断を表示する最小の列位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.min_column"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .min_column
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .min_column = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn lsp_pull_diagnostics_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("LSP のプル型診断"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "言語サーバーからプル型で診断を取得するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.lsp_pull_diagnostics.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .lsp_pull_diagnostics
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .lsp_pull_diagnostics
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings_ui): Needs unit
            SettingsPageItem::SettingItem(SettingItem {
                title: "デバウンス",
                description: "言語サーバーから診断を取得するまでの最小の待ち時間です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.lsp_pull_diagnostics.debounce_ms"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .lsp_pull_diagnostics
                            .as_ref()?
                            .debounce_ms
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .lsp_pull_diagnostics
                            .get_or_insert_default()
                            .debounce_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn lsp_highlights_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("LSP のハイライト"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "デバウンス",
                description: "言語サーバーにハイライトを問い合わせるまでのデバウンスの遅延です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("lsp_highlight_debounce"),
                    pick: |settings_content| {
                        settings_content.editor.lsp_highlight_debounce.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.lsp_highlight_debounce = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn languages_list_section(cx: &App) -> Box<[SettingsPageItem]> {
        // todo(settings_ui): Refresh on extension (un)/installed
        // Note that `crates/json_schema_store` solves the same problem, there is probably a way to unify the two
        std::iter::once(SettingsPageItem::SectionHeader("言語"))
            .chain(all_language_names(cx).into_iter().map(|language_name| {
                let link = format!("languages.{language_name}");
                SettingsPageItem::SubPageLink(SubPageLink {
                    title: language_name,
                    r#type: crate::SubPageType::Language,
                    description: None,
                    search_aliases: &[],
                    json_path: Some(link.leak()),
                    in_json: true,
                    files: USER | PROJECT,
                    render: |this, scroll_handle, window, cx| {
                        let items: Box<[SettingsPageItem]> = concat_sections!(
                            language_settings_data(),
                            non_editor_language_settings_data(),
                            edit_prediction_language_settings_section()
                        );
                        this.render_sub_page_items(
                            items.iter().enumerate(),
                            scroll_handle,
                            window,
                            cx,
                        )
                        .into_any_element()
                    },
                })
            }))
            .collect()
    }

    SettingsPage {
        title: "言語とツール",
        items: {
            concat_sections!(
                non_editor_language_settings_data(),
                file_types_section(),
                diagnostics_section(),
                inline_diagnostics_section(),
                lsp_pull_diagnostics_section(),
                lsp_highlights_section(),
                languages_list_section(cx),
            )
        },
    }
}

fn search_and_files_page() -> SettingsPage {
    fn search_section() -> [SettingsPageItem; 9] {
        [
            SettingsPageItem::SectionHeader("検索"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "単語単位で検索",
                description: "既定で単語単位で検索します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.whole_word"),
                    pick: |settings_content| {
                        settings_content.editor.search.as_ref()?.whole_word.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .whole_word = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "大文字と小文字を区別",
                description: "既定で大文字と小文字を区別して検索します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.case_sensitive"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .search
                            .as_ref()?
                            .case_sensitive
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .case_sensitive = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "smartcase 検索を使用",
                description: "検索クエリの内容に応じて、大文字と小文字の区別を自動的に有効にするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_smartcase_search"),
                    pick: |settings_content| settings_content.editor.use_smartcase_search.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.use_smartcase_search = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "無視されたファイルを含める",
                description: "既定で検索結果に無視されたファイルを含めます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.include_ignored"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .search
                            .as_ref()?
                            .include_ignored
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .include_ignored = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "正規表現",
                description: "既定で正規表現検索を使用します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.regex"),
                    pick: |settings_content| {
                        settings_content.editor.search.as_ref()?.regex.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.search.get_or_insert_default().regex = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "検索の折り返し",
                description: "エディターの検索結果を末尾から先頭へ循環させるかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search_wrap"),
                    pick: |settings_content| settings_content.editor.search_wrap.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.search_wrap = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "一致箇所を中央に表示",
                description: "現在の一致箇所をエディターの中央に表示するかどうかを設定します",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.search.center_on_match"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .search
                            .as_ref()
                            .and_then(|search| search.center_on_match.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .center_on_match = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソル位置から検索クエリを初期化",
                description: "新しい検索のクエリをカーソル位置のテキストで埋めるタイミングを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("seed_search_query_from_cursor"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .seed_search_query_from_cursor
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.seed_search_query_from_cursor = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn file_finder_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("ファイル検索"),
            // todo: null by default
            SettingsPageItem::SettingItem(SettingItem {
                title: "検索に無視されたファイルを含める",
                description: "検索時に gitignore されたファイルも対象にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_finder.include_ignored"),
                    pick: |settings_content| {
                        settings_content
                            .file_finder
                            .as_ref()?
                            .include_ignored
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .file_finder
                            .get_or_insert_default()
                            .include_ignored = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルアイコン",
                description: "ファイル検索にファイルアイコンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_finder.file_icons"),
                    pick: |settings_content| {
                        settings_content.file_finder.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .file_finder
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "検索結果でアクティブなファイルを飛ばす",
                description: "ファイル検索の結果で、アクティブなファイルにはフォーカスを合わせないようにするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_finder.skip_focus_for_active_in_search"),
                    pick: |settings_content| {
                        settings_content
                            .file_finder
                            .as_ref()?
                            .skip_focus_for_active_in_search
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .file_finder
                            .get_or_insert_default()
                            .skip_focus_for_active_in_search = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn file_scan_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader("ファイルスキャン"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルスキャンの除外",
                description: "Zed が完全に除外するファイルまたは glob です。ファイルスキャンとファイル検索から除外され、プロジェクトのファイルツリーにも表示されません。「ファイルスキャンの対象」より優先されます",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("file_scan_exclusions"),
                        pick: |settings_content| {
                            settings_content
                                .project
                                .worktree
                                .file_scan_exclusions
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.file_scan_exclusions = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルスキャンの対象",
                description: "git に無視されていても Zed が対象に含めるファイルまたは glob です。git の管理外でもプロジェクトにとって重要なファイルに便利です。範囲が広すぎる glob は Zed のファイルスキャンを遅くする点に注意してください。「ファイルスキャンの除外」がこの設定より優先されます",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("file_scan_inclusions"),
                        pick: |settings_content| {
                            settings_content
                                .project
                                .worktree
                                .file_scan_inclusions
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.file_scan_inclusions = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルスキャンの深さ",
                description: "git リポジトリの外側で先読みインデックスを作成するディレクトリの最大の深さです。この深さ以上のディレクトリの内容は、必要になった時点でインデックスされます。この深さより浅い位置にあるリポジトリは常に完全にインデックスされます。git リポジトリを起点としないプロジェクトでは、ルートフォルダーの直下にあるリポジトリは即座に git 機能が有効になり、それより深いものは最初に使われた時点で有効になります。0 は無制限を意味し、すべての git リポジトリを即座に有効にします",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_scan_depth"),
                    pick: |settings_content| {
                        settings_content.project.worktree.file_scan_depth.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.project.worktree.file_scan_depth = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "シンボリックリンクをスキャン",
                description: "リンク先のディレクトリの内容をスキャンするタイミングです",
                field: Box::new(SettingField {
                    json_path: Some("scan_symlinks"),
                    organization_override: None,
                    pick: |settings_content| {
                        settings_content.project.worktree.scan_symlinks.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.project.worktree.scan_symlinks = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルの状態を復元",
                description: "ファイルを再度開いたときに前回の状態を復元します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("restore_on_file_reopen"),
                    pick: |settings_content| {
                        settings_content.workspace.restore_on_file_reopen.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.restore_on_file_reopen = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイル削除時に閉じる",
                description: "削除されたファイルを自動的に閉じます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("close_on_file_delete"),
                    pick: |settings_content| {
                        settings_content.workspace.close_on_file_delete.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.close_on_file_delete = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "検索とファイル",
        items: concat_sections![search_section(), file_finder_section(), file_scan_section()],
    }
}

fn window_and_layout_page() -> SettingsPage {
    fn status_bar_section() -> [SettingsPageItem; 11] {
        [
            SettingsPageItem::SectionHeader("ステータスバー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プロジェクトパネルのボタン",
                description: "ステータスバーにプロジェクトパネルのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.button"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "現在の言語のボタン",
                description: "ステータスバーに現在の言語のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.active_language_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .active_language_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .active_language_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "現在のエンコーディングのボタン",
                description: "ステータスバーに現在のエンコーディングを表示するタイミングを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.active_encoding_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .active_encoding_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .active_encoding_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソル位置のボタン",
                description: "ステータスバーにカーソル位置のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.cursor_position_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .cursor_position_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .cursor_position_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "改行コードのボタン",
                description: "ステータスバーに現在の改行コードのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.line_endings_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .line_endings_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .line_endings_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ターミナルのボタン",
                description: "ステータスバーにターミナルのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.button"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "診断のボタン",
                description: "ステータスバーにプロジェクトの診断のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.button"),
                    pick: |settings_content| settings_content.diagnostics.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.diagnostics.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プロジェクト内検索のボタン",
                description: "ステータスバーにプロジェクト内検索のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.button"),
                    pick: |settings_content| {
                        settings_content.editor.search.as_ref()?.button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "デバッガーのボタン",
                description: "ステータスバーにデバッガーのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.button"),
                    pick: |settings_content| settings_content.debugger.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.debugger.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "現在のファイル名",
                description: "ステータスバーに現在のファイル名を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.show_active_file"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .show_active_file
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .show_active_file = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn title_bar_section() -> [SettingsPageItem; 11] {
        [
            SettingsPageItem::SectionHeader("タイトルバー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブランチの状態アイコンを表示",
                description: "タイトルバーのブランチアイコンに git の状態インジケーターを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_branch_status_icon"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_branch_status_icon
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_branch_status_icon = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブランチ名を表示",
                description: "タイトルバーにブランチ名のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_branch_name"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_branch_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_branch_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ワークツリー名を表示",
                description: "タイトルバーにワークツリー名のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_worktree_name"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_worktree_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_worktree_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プロジェクト情報を表示",
                description: "タイトルバーにプロジェクトのホストと名前を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_project_items"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_project_items
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_project_items = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "お知らせバナーを表示",
                description: "タイトルバーに新機能を知らせるバナーを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_onboarding_banner"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_onboarding_banner
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_onboarding_banner = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "サインインを表示",
                description: "タイトルバーにサインインのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_sign_in"),
                    pick: |settings_content| {
                        settings_content.title_bar.as_ref()?.show_sign_in.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_sign_in = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ユーザーメニューを表示",
                description: "タイトルバーにユーザーメニューのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_user_menu"),
                    pick: |settings_content| {
                        settings_content.title_bar.as_ref()?.show_user_menu.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_user_menu = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ユーザーの画像を表示",
                description: "タイトルバーにユーザーの画像を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_user_picture"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_user_picture
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_user_picture = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "メニューを表示",
                description: "タイトルバーにメニューを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_menus"),
                    pick: |settings_content| {
                        settings_content.title_bar.as_ref()?.show_menus.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_menus = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "ボタンの配置",
                    description:
                        "(Linux のみ) タイトルバーのウィンドウ操作ボタンの配置を選択します。",
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("title_bar.button_layout$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::WindowButtonLayoutContent>()[settings_content
                                    .title_bar
                                    .as_ref()?
                                    .button_layout
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                settings_content
                                    .title_bar
                                    .get_or_insert_default()
                                    .button_layout = None;
                                return;
                            };

                            let current_custom_layout = settings_content
                                .title_bar
                                .as_ref()
                                .and_then(|title_bar| title_bar.button_layout.as_ref())
                                .and_then(|button_layout| match button_layout {
                                    settings::WindowButtonLayoutContent::Custom(layout) => {
                                        Some(layout.clone())
                                    }
                                    _ => None,
                                });

                            let button_layout = match value {
                                settings::WindowButtonLayoutContentDiscriminants::PlatformDefault => {
                                    settings::WindowButtonLayoutContent::PlatformDefault
                                }
                                settings::WindowButtonLayoutContentDiscriminants::Standard => {
                                    settings::WindowButtonLayoutContent::Standard
                                }
                                settings::WindowButtonLayoutContentDiscriminants::Custom => {
                                    settings::WindowButtonLayoutContent::Custom(
                                        current_custom_layout.unwrap_or_else(|| {
                                            "close:minimize,maximize".to_string()
                                        }),
                                    )
                                }
                            };

                            settings_content
                                .title_bar
                                .get_or_insert_default()
                                .button_layout = Some(button_layout);
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .title_bar
                            .as_ref()?
                            .button_layout
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::WindowButtonLayoutContent>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::WindowButtonLayoutContentDiscriminants::PlatformDefault => {
                            vec![]
                        }
                        settings::WindowButtonLayoutContentDiscriminants::Standard => vec![],
                        settings::WindowButtonLayoutContentDiscriminants::Custom => vec![
                            SettingItem {
                                files: USER,
                                title: "カスタムのボタン配置",
                                description:
                                    "\"close:minimize,maximize\" のような GNOME 形式の配置文字列です。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("title_bar.button_layout"),
                                    pick: |settings_content| match settings_content
                                        .title_bar
                                        .as_ref()?
                                        .button_layout
                                        .as_ref()?
                                    {
                                        settings::WindowButtonLayoutContent::Custom(layout) => {
                                            Some(layout)
                                        }
                                        _ => DEFAULT_EMPTY_STRING,
                                    },
                                    write: |settings_content, value, _| {
                                        settings_content
                                            .title_bar
                                            .get_or_insert_default()
                                            .button_layout = value
                                            .map(settings::WindowButtonLayoutContent::Custom);
                                    },
                                }),
                                metadata: Some(Box::new(SettingsFieldMetadata {
                                    placeholder: Some("close:minimize,maximize"),
                                    ..Default::default()
                                })),
                            },
                        ],
                    })
                    .collect(),
            }),
        ]
    }

    fn tab_bar_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader("タブバー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブバーを表示",
                description: "エディターにタブバーを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show"),
                    pick: |settings_content| settings_content.tab_bar.as_ref()?.show.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.tab_bar.get_or_insert_default().show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブに Git の状態を表示",
                description: "タブの項目に Git のファイルの状態を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.git_status"),
                    pick: |settings_content| settings_content.tabs.as_ref()?.git_status.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.tabs.get_or_insert_default().git_status = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブにファイルアイコンを表示",
                description: "タブにファイルのアイコンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.file_icons"),
                    pick: |settings_content| settings_content.tabs.as_ref()?.file_icons.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.tabs.get_or_insert_default().file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブの閉じるボタンの位置",
                description: "タブ内の閉じるボタンの位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.close_position"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.close_position.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.tabs.get_or_insert_default().close_position = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "タブの最大数",
                description: "1 つのペインで開けるタブの最大数です。未保存のタブは閉じられません。",
                // todo(settings_ui): The default for this value is null and it's use in code
                // is complex, so I'm going to come back to this later
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("max_tabs"),
                        pick: |settings_content| settings_content.workspace.max_tabs.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.workspace.max_tabs = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ナビゲーション履歴のボタンを表示",
                description: "タブバーにナビゲーション履歴のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show_nav_history_buttons"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()?
                            .show_nav_history_buttons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .show_nav_history_buttons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブバーのボタンを表示",
                description: "タブバーのボタン (新規・ペインの分割・ズーム) を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show_tab_bar_buttons"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()?
                            .show_tab_bar_buttons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .show_tab_bar_buttons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "非アクティブなペインにもボタンを表示",
                description: "フォーカスされていないペインのタブバーにも、新規・ペインの分割・ズームなどのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show_buttons_on_inactive_panes"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()?
                            .show_buttons_on_inactive_panes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .show_buttons_on_inactive_panes = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ピン留めタブの配置",
                description: "ピン留めしたタブを、ピン留めしていないタブの上の別の行に表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show_pinned_tabs_in_separate_row"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()?
                            .show_pinned_tabs_in_separate_row
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .show_pinned_tabs_in_separate_row = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn tab_settings_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("タブの設定"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "閉じた後にアクティブにするタブ",
                description: "現在のタブを閉じた後の動作です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.activate_on_close"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.activate_on_close.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabs
                            .get_or_insert_default()
                            .activate_on_close = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブに診断を表示",
                description: "診断のエラー・警告を含むファイルのうち、どれをタブ上で示すかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.show_diagnostics"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.show_diagnostics.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabs
                            .get_or_insert_default()
                            .show_diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "閉じるボタンを表示",
                description: "タブの閉じるボタンの表示動作を設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.show_close_button"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.show_close_button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabs
                            .get_or_insert_default()
                            .show_close_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn preview_tabs_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader("プレビュータブ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プレビュータブを有効にする",
                description: "開いたエディターをプレビュータブとして表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enabled"),
                    pick: |settings_content| {
                        settings_content.preview_tabs.as_ref()?.enabled.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プロジェクトパネルからのプレビュー",
                description: "プロジェクトパネルからシングルクリックで開いたときに、プレビューモードのタブで開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_from_project_panel"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_from_project_panel
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_from_project_panel = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイル検索からのプレビュー",
                description: "ファイル検索から選んだときに、プレビューモードのタブで開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_from_file_finder"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_from_file_finder
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_from_file_finder = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マルチバッファーからのプレビュー",
                description: "マルチバッファーから開いたときに、プレビューモードのタブで開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_from_multibuffer"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_from_multibuffer
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_from_multibuffer = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コードナビゲーションからのマルチバッファーのプレビュー",
                description: "コードナビゲーションでマルチバッファーを開いたときに、プレビューモードのタブで開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_multibuffer_from_code_navigation"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_multibuffer_from_code_navigation
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_multibuffer_from_code_navigation = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コードナビゲーションからのファイルのプレビュー",
                description: "コードナビゲーションで単一のファイルを開いたときに、プレビューモードのタブで開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_file_from_code_navigation"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_file_from_code_navigation
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_file_from_code_navigation = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コードナビゲーション時にプレビューを維持",
                description: "コードナビゲーションでタブから移動したときに、そのタブをプレビューモードのまま維持するかどうかを設定します。`enable_preview_file_from_code_navigation` または `enable_preview_multibuffer_from_code_navigation` も true の場合、新しいタブが既存のタブを置き換えることがあります。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_keep_preview_on_code_navigation"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_keep_preview_on_code_navigation
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_keep_preview_on_code_navigation = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn layout_section() -> [SettingsPageItem; 9] {
        [
            SettingsPageItem::SectionHeader("レイアウト"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定でパネルを 2 枚並べるドック",
                description: "プロジェクトを初めて開いたときに、パネルを 2 枚 (左右のドックは縦、下ドックは横) 並べておくドックです。ドック自体は開きません (起動時に開くかどうかは各パネルの「起動時に開く」で設定します)。以降はプロジェクトごとに保存されたレイアウトが優先されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("default_stacked_dock"),
                    pick: |settings_content| {
                        settings_content.workspace.default_stacked_dock.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.default_stacked_dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定で並べる 1 枚目のパネル",
                description: "そのドックを埋めるパネルです。「並べない」にすると、初回のレイアウトを組みません。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("default_stacked_panel"),
                    pick: |settings_content| {
                        settings_content.workspace.default_stacked_panel.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.default_stacked_panel = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定で並べる 2 枚目のパネル",
                description: "1 枚目の隣に並べて表示するパネルです。「並べない」にすると、上流の Zed と同じく 1 枚だけを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("default_stacked_secondary_panel"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .default_stacked_secondary_panel
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.default_stacked_secondary_panel = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "下ドックのレイアウト",
                description: "下ドックのレイアウトモードです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("bottom_dock_layout"),
                    pick: |settings_content| settings_content.workspace.bottom_dock_layout.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.bottom_dock_layout = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "中央寄せレイアウトの左余白",
                description: "中央寄せレイアウトの左側の余白です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("centered_layout.left_padding"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .centered_layout
                            .as_ref()?
                            .left_padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .centered_layout
                            .get_or_insert_default()
                            .left_padding = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "中央寄せレイアウトの右余白",
                description: "中央寄せレイアウトの右側の余白です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("centered_layout.right_padding"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .centered_layout
                            .as_ref()?
                            .right_padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .centered_layout
                            .get_or_insert_default()
                            .right_padding = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マウスでフォーカスを移動",
                description: "マウスがペインの上に来たときに、そのペインへフォーカスを移すかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("focus_follows_mouse.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .as_ref()
                            .and_then(|s| s.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォーカス移動のデバウンス (ミリ秒)",
                description: "フォーカスを移すまでの待ち時間です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("focus_follows_mouse.debounce_ms"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .as_ref()
                            .and_then(|s| s.debounce_ms.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .get_or_insert_default()
                            .debounce_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn window_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("ウィンドウ"),
            // todo(settings_ui): Should we filter by platform.as_ref()?
            SettingsPageItem::SettingItem(SettingItem {
                title: "システムのウィンドウタブを使用",
                description: "(macOS のみ) ウィンドウをタブとしてまとめられるようにするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_system_window_tabs"),
                    pick: |settings_content| {
                        settings_content.workspace.use_system_window_tabs.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.use_system_window_tabs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "全画面表示のモード",
                description: "(macOS のみ) 全画面表示の切り替えアクションで入る全画面モードを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("fullscreen_mode"),
                    pick: |settings_content| settings_content.workspace.fullscreen_mode.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.fullscreen_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ウィンドウの装飾",
                description: "(Linux のみ) ウィンドウの装飾を Zed が描画するか、コンポジターに任せるかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("window_decorations"),
                    pick: |settings_content| settings_content.workspace.window_decorations.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.window_decorations = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn pane_modifiers_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("ペインの表示調整"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "非アクティブ時の不透明度",
                description: "非アクティブなパネルの不透明度です (0.0〜1.0)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("active_pane_modifiers.inactive_opacity"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .as_ref()?
                            .inactive_opacity
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .get_or_insert_default()
                            .inactive_opacity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "境界線の太さ",
                description: "アクティブなペインを囲む境界線の太さです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("active_pane_modifiers.border_size"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .as_ref()?
                            .border_size
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .get_or_insert_default()
                            .border_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ズーム時の余白",
                description: "ズームしたペインに余白を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("zoomed_padding"),
                    pick: |settings_content| settings_content.workspace.zoomed_padding.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.zoomed_padding = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn pane_split_direction_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("ペインの分割方向"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "垂直分割の方向",
                description: "垂直に分割するときの方向です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("pane_split_direction_vertical"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .pane_split_direction_vertical
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.pane_split_direction_vertical = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "水平分割の方向",
                description: "水平に分割するときの方向です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("pane_split_direction_horizontal"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .pane_split_direction_horizontal
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.pane_split_direction_horizontal = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "ウィンドウとレイアウト",
        items: concat_sections![
            status_bar_section(),
            title_bar_section(),
            tab_bar_section(),
            tab_settings_section(),
            preview_tabs_section(),
            layout_section(),
            window_section(),
            pane_modifiers_section(),
            pane_split_direction_section(),
        ],
    }
}

fn panels_page() -> SettingsPage {
    fn project_panel_section() -> [SettingsPageItem; 29] {
        [
            SettingsPageItem::SectionHeader("プロジェクトパネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プロジェクトパネルのドック位置",
                description: "プロジェクトパネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.dock"),
                    pick: |settings_content| settings_content.project_panel.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.project_panel.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プロジェクトパネルの既定の幅",
                description: "プロジェクトパネルの既定の幅 (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.default_width"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .default_width
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: ".gitignore の項目を隠す",
                description: "プロジェクトパネルで gitignore の対象項目を隠すかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.hide_gitignore"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .hide_gitignore
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .hide_gitignore = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "項目の間隔",
                description: "プロジェクトパネルのワークツリー項目の間隔です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.entry_spacing"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .entry_spacing
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .entry_spacing = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルアイコン",
                description: "プロジェクトパネルにファイルアイコンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.file_icons"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォルダーアイコン",
                description: "プロジェクトパネルのディレクトリに、フォルダーアイコンと山形記号のどちらを表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.folder_icons"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .folder_icons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .folder_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git の状態",
                description: "プロジェクトパネルに Git の状態を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.git_status"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.git_status.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .git_status = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "インデント幅",
                description: "入れ子になった項目のインデント幅です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.indent_size"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .indent_size
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .indent_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "項目を自動的に表示",
                description: "対応するプロジェクトの項目がアクティブになったときに、プロジェクトパネルでその項目を自動的に表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_reveal_entries"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_reveal_entries
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_reveal_entries = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "起動時に開く",
                description: "起動時にプロジェクトパネルを開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.starts_open"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .starts_open
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ディレクトリを自動的にまとめる",
                description: "ディレクトリの中にサブディレクトリが 1 つだけあるときに、自動的にまとめてコンパクトに表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_fold_dirs"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_fold_dirs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_fold_dirs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォルダー名を太字にする",
                description: "プロジェクトパネルでフォルダー名を太字で表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.bold_folder_labels"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .bold_folder_labels
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .bold_folder_labels = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スクロールバーを表示",
                description: "プロジェクトパネルにスクロールバーを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.scrollbar.show"),
                    pick: |settings_content| {
                        show_scrollbar_or_editor(settings_content, |settings_content| {
                            settings_content
                                .project_panel
                                .as_ref()?
                                .scrollbar
                                .as_ref()?
                                .show
                                .as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "水平スクロール",
                description: "プロジェクトパネルで水平スクロールを許可するかどうかを設定します。無効にすると表示は常に左端に固定され、長いファイル名は切り詰められます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.scrollbar.horizontal_scroll"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .scrollbar
                            .as_ref()?
                            .horizontal_scroll
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .horizontal_scroll = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "診断を表示",
                description: "診断のエラー・警告を含むファイルのうち、どれをプロジェクトパネルで示すかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.show_diagnostics"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .show_diagnostics
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .show_diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "診断のバッジ",
                description: "プロジェクトパネルのファイル名の横に、エラーと警告の件数バッジを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.diagnostic_badges"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .diagnostic_badges
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .diagnostic_badges = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git の状態インジケーター",
                description: "プロジェクトパネルのファイル名の横に git の状態インジケーターを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.git_status_indicator"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .git_status_indicator
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .git_status_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スティッキースクロール",
                description: "親ディレクトリをプロジェクトパネルの上部に固定表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.sticky_scroll"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .sticky_scroll
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .sticky_scroll = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "インデントガイドを表示",
                description: "プロジェクトパネルにインデントガイドを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.indent_guides.show"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .indent_guides
                            .as_ref()?
                            .show
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .indent_guides
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ドラッグ&ドロップ",
                description: "プロジェクトパネルでドラッグ&ドロップ操作を有効にするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.drag_and_drop"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .drag_and_drop
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .drag_and_drop = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ルート項目を隠す",
                description: "ウィンドウで 1 つのフォルダーだけを開いているときに、ルート項目を隠すかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.hide_root"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.hide_root.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .hide_root = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "隠し項目を隠す",
                description: "プロジェクトパネルで隠し項目を表示しないかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.hide_hidden"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .hide_hidden
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .hide_hidden = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "並べ替えの方法",
                description: "プロジェクトパネルの項目の並び順です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.sort_mode"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.sort_mode.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .sort_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "並べ替えの基準",
                description: "プロジェクトパネルで、ファイル名とフォルダー名を大文字と小文字を区別して並べ替えるかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.sort_order.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .sort_order = value;
                    },
                    json_path: Some("project_panel.sort_order"),
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "作成時にファイルを自動的に開く",
                description: "新しく作成したファイルをエディターで自動的に開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_open.on_create"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_open
                            .as_ref()?
                            .on_create
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_open
                            .get_or_insert_default()
                            .on_create = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "貼り付け時にファイルを自動的に開く",
                description: "貼り付けまたは複製したファイルを自動的に開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_open.on_paste"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_open
                            .as_ref()?
                            .on_paste
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_open
                            .get_or_insert_default()
                            .on_paste = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ドロップ時にファイルを自動的に開く",
                description: "外部からドロップされたファイルを自動的に開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_open.on_drop"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_open
                            .as_ref()?
                            .on_drop
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_open
                            .get_or_insert_default()
                            .on_drop = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "隠しファイル",
                description: "「隠しファイル」として扱い、プロジェクトパネルから隠せるようにするファイルを照合する glob です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("worktree.hidden_files"),
                        pick: |settings_content| {
                            settings_content.project.worktree.hidden_files.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.hidden_files = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn terminal_panel_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("ターミナルパネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ターミナルのドック位置",
                description: "ターミナルパネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.dock"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "起動時に開く",
                description: "起動時にターミナルパネルを開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.starts_open"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.starts_open.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ターミナルパネルの可変サイズ",
                description: "左右にドッキングしたときに、ターミナルパネルで可変 (比率) サイズを使うかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.flexible"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.flexible.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().flexible = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "件数バッジを表示",
                description: "ターミナルパネルのアイコンに、開いているターミナルの数のバッジを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.show_count_badge"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .show_count_badge
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .show_count_badge = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn outline_panel_section() -> [SettingsPageItem; 11] {
        [
            SettingsPageItem::SectionHeader("アウトラインパネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アウトラインパネルのボタン",
                description: "ステータスバーにアウトラインパネルのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.button"),
                    pick: |settings_content| {
                        settings_content.outline_panel.as_ref()?.button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アウトラインパネルのドック位置",
                description: "アウトラインパネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.dock"),
                    pick: |settings_content| settings_content.outline_panel.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.outline_panel.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アウトラインパネルの既定の幅",
                description: "アウトラインパネルの既定の幅 (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.default_width"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .default_width
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルアイコン",
                description: "アウトラインパネルにファイルアイコンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.file_icons"),
                    pick: |settings_content| {
                        settings_content.outline_panel.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォルダーアイコン",
                description: "アウトラインパネルのディレクトリに、フォルダーアイコンと山形記号のどちらを表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.folder_icons"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .folder_icons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .folder_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git の状態",
                description: "アウトラインパネルに Git の状態を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.git_status"),
                    pick: |settings_content| {
                        settings_content.outline_panel.as_ref()?.git_status.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .git_status = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "インデント幅",
                description: "入れ子になった項目のインデント幅です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.indent_size"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .indent_size
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .indent_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "項目を自動的に表示",
                description: "対応するアウトラインの項目がアクティブになったときに、自動的に表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.auto_reveal_entries"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .auto_reveal_entries
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .auto_reveal_entries = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ディレクトリを自動的にまとめる",
                description: "ディレクトリの中にサブディレクトリが 1 つだけあるときに、自動的にまとめるかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.auto_fold_dirs"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .auto_fold_dirs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .auto_fold_dirs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "インデントガイドを表示",
                description: "アウトラインパネルにインデントガイドを表示するタイミングです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.indent_guides.show"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .indent_guides
                            .as_ref()?
                            .show
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .indent_guides
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
            }),
        ]
    }

    fn git_panel_section() -> [SettingsPageItem; 18] {
        [
            SettingsPageItem::SectionHeader("Git パネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git パネルのボタン",
                description: "ステータスバーに Git パネルのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.button"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git パネルのドック位置",
                description: "Git パネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.dock"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "起動時に開く",
                description: "起動時に git パネルを開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.starts_open"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.starts_open.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git パネルの既定の幅",
                description: "Git パネルの既定の幅 (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.default_width"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.default_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Git パネルの状態の表示スタイル",
                description: "項目の状態をどのように表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.status_style"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.status_style.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .status_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定のブランチ名",
                description: "Git で init.defaultbranch が設定されていないときに使うブランチ名です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.fallback_branch_name"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .fallback_branch_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .fallback_branch_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "並べ替え",
                description: "git パネルの項目の並び順です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.sort_by"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.sort_by.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().sort_by = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "グループ化",
                description: "git パネルの項目のグループ化の方法です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.group_by"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.group_by.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().group_by = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "未追跡ファイルの差分を折りたたむ",
                description: "差分パネルで未追跡のファイルを折りたたむかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.collapse_untracked_diff"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .collapse_untracked_diff
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .collapse_untracked_diff = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ツリー表示",
                description: "有効にすると項目をツリー表示で、無効にするとフラットな一覧で表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.tree_view"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.tree_view.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().tree_view = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイルアイコン",
                description: "Git の状態アイコンの横にファイルアイコンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.file_icons"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォルダーアイコン",
                description: "git パネルのディレクトリに、フォルダーアイコンと山形記号のどちらを表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.folder_icons"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.folder_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .folder_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "差分の統計",
                description: "Git パネルの各ファイルの横に、追加・削除の行数を表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.diff_stats"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.diff_stats.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .diff_stats = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "クリック時の動作",
                description: "Git パネルで変更されたファイルをクリックしたときの既定の動作です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.entry_primary_click_action"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .entry_primary_click_action
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .entry_primary_click_action = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "件数バッジを表示",
                description: "git パネルのアイコンに、未コミットの変更数のバッジを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.show_count_badge"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .show_count_badge
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .show_count_badge = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コミットタイトルの最大長",
                description: "警告を表示するまでのコミットメッセージのタイトルの最大長です。0 で無効になります。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.commit_title_max_length"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .commit_title_max_length
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .commit_title_max_length = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スクロールバー",
                description: "スクロールバーをどのように、いつ表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.scrollbar.show"),
                    pick: |settings_content| {
                        show_scrollbar_or_editor(settings_content, |settings_content| {
                            settings_content
                                .git_panel
                                .as_ref()?
                                .scrollbar
                                .as_ref()?
                                .show
                                .as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn debugger_panel_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("デバッガーパネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "デバッガーパネルのドック位置",
                description: "デバッグパネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.dock"),
                    pick: |settings_content| settings_content.debugger.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.debugger.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn collaboration_panel_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("コラボレーションパネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コラボレーションパネルのボタン",
                description: "ステータスバーにコラボレーションパネルのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("collaboration_panel.button"),
                    pick: |settings_content| {
                        settings_content
                            .collaboration_panel
                            .as_ref()?
                            .button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .collaboration_panel
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コラボレーションパネルのドック位置",
                description: "コラボレーションパネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("collaboration_panel.dock"),
                    pick: |settings_content| {
                        settings_content.collaboration_panel.as_ref()?.dock.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .collaboration_panel
                            .get_or_insert_default()
                            .dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コラボレーションパネルの既定の幅",
                description: "コラボレーションパネルの既定の幅 (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("collaboration_panel.dock"),
                    pick: |settings_content| {
                        settings_content
                            .collaboration_panel
                            .as_ref()?
                            .default_width
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .collaboration_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn agent_panel_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader("エージェントパネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントパネルのボタン",
                description: "ステータスバーにエージェントパネルのボタンを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.button"),
                    pick: |settings_content| settings_content.agent.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントパネルのドック位置",
                description: "エージェントパネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.dock"),
                    pick: |settings_content| settings_content.agent.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "起動時に開く",
                description: "起動時にエージェントパネルを開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.starts_open"),
                    pick: |settings_content| settings_content.agent.as_ref()?.starts_open.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントパネルの可変サイズ",
                description: "左右にドッキングしたときに、エージェントパネルで可変 (比率) サイズを使うかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.flexible"),
                    pick: |settings_content| settings_content.agent.as_ref()?.flexible.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().flexible = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントパネルの既定の幅",
                description: "エージェントパネルを左右にドッキングしたときの既定の幅です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.default_width"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.default_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントパネルの既定の高さ",
                description: "エージェントパネルを下にドッキングしたときの既定の高さです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.default_height"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.default_height.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .default_height = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "内容の幅を制限",
                    description: "読みやすさのために、エージェントパネルの内容を最大幅に制限し、パネルがそれより広いときは中央に寄せるかどうかを設定します。",
                    field: Box::new(SettingField::<bool> {
                        organization_override: None,
                        json_path: Some("agent.limit_content_width"),
                        pick: |settings_content| {
                            settings_content
                                .agent
                                .as_ref()?
                                .limit_content_width
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .agent
                                .get_or_insert_default()
                                .limit_content_width = value;
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    let enabled = settings_content
                        .agent
                        .as_ref()?
                        .limit_content_width
                        .unwrap_or(true);
                    Some(if enabled { 1 } else { 0 })
                },
                fields: vec![
                    vec![],
                    vec![SettingItem {
                        files: USER,
                        title: "内容の最大幅",
                        description: "内容の最大幅 (ピクセル) です。パネルがこの値より広いときは内容を中央に寄せます。",
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("agent.max_content_width"),
                            pick: |settings_content| {
                                settings_content.agent.as_ref()?.max_content_width.as_ref()
                            },
                            write: |settings_content, value, _| {
                                settings_content
                                    .agent
                                    .get_or_insert_default()
                                    .max_content_width = value;
                            },
                        }),
                        metadata: None,
                    }],
                ],
            }),
        ]
    }

    SettingsPage {
        title: "パネル",
        items: concat_sections![
            project_panel_section(),
            terminal_panel_section(),
            outline_panel_section(),
            git_panel_section(),
            debugger_panel_section(),
            collaboration_panel_section(),
            agent_panel_section(),
        ],
    }
}

fn debugger_page() -> SettingsPage {
    fn general_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("全般"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ステップ実行の粒度",
                description: "デバッグ操作のステップ実行の粒度を設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.stepping_granularity"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .stepping_granularity
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .stepping_granularity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブレークポイントを保存",
                description: "Zed のセッションをまたいでブレークポイントを引き継ぐかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.save_breakpoints"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .save_breakpoints
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .save_breakpoints = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タイムアウト",
                description: "TCP のデバッグアダプターに接続するときに、タイムアウトエラーとするまでの時間 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.timeout"),
                    pick: |settings_content| settings_content.debugger.as_ref()?.timeout.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.debugger.get_or_insert_default().timeout = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "DAP の通信をログに記録",
                description: "アクティブなデバッグアダプターと Zed の間のメッセージをログに記録するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.log_dap_communications"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .log_dap_communications
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .log_dap_communications = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "DAP のログメッセージを整形",
                description: "デバッグアダプターのロガーに追加するときに、DAP のメッセージを整形するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.format_dap_log_messages"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .format_dap_log_messages
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .format_dap_log_messages = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "デバッガー",
        items: concat_sections![general_section()],
    }
}

fn terminal_page() -> SettingsPage {
    fn environment_section() -> [SettingsPageItem; 5] {
        [
                SettingsPageItem::SectionHeader("環境"),
                SettingsPageItem::DynamicItem(DynamicItem {
                    discriminant: SettingItem {
                        files: USER | PROJECT,
                        title: "シェル",
                        description: "ターミナルを開くときに使うシェルです。",
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("terminal.shell$"),
                            pick: |settings_content| {
                                Some(&dynamic_variants::<settings::Shell>()[
                                    settings_content
                                        .terminal
                                        .as_ref()?
                                        .project
                                        .shell
                                        .as_ref()?
                                        .discriminant() as usize
                                ])
                            },
                            write: |settings_content, value, _| {
                                let Some(value) = value else {
                                    if let Some(terminal) = settings_content.terminal.as_mut() {
                                        terminal.project.shell = None;
                                    }
                                    return;
                                };
                                let settings_value = settings_content
                                    .terminal
                                    .get_or_insert_default()
                                    .project
                                    .shell
                                    .get_or_insert_with(|| settings::Shell::default());
                                let default_shell = if cfg!(target_os = "windows") {
                                    "powershell.exe"
                                } else {
                                    "sh"
                                };
                                *settings_value = match value {
                                    settings::ShellDiscriminants::System => settings::Shell::System,
                                    settings::ShellDiscriminants::Program => {
                                        let program = match settings_value {
                                            settings::Shell::Program(program) => program.clone(),
                                            settings::Shell::WithArguments { program, .. } => program.clone(),
                                            _ => String::from(default_shell),
                                        };
                                        settings::Shell::Program(program)
                                    }
                                    settings::ShellDiscriminants::WithArguments => {
                                        let (program, args, title_override) = match settings_value {
                                            settings::Shell::Program(program) => (program.clone(), vec![], None),
                                            settings::Shell::WithArguments {
                                                program,
                                                args,
                                                title_override,
                                            } => (program.clone(), args.clone(), title_override.clone()),
                                            _ => (String::from(default_shell), vec![], None),
                                        };
                                        settings::Shell::WithArguments {
                                            program,
                                            args,
                                            title_override,
                                        }
                                    }
                                };
                            },
                        }),
                        metadata: None,
                    },
                    pick_discriminant: |settings_content| {
                        Some(
                            settings_content
                                .terminal
                                .as_ref()?
                                .project
                                .shell
                                .as_ref()?
                                .discriminant() as usize,
                        )
                    },
                    fields: dynamic_variants::<settings::Shell>()
                        .into_iter()
                        .map(|variant| match variant {
                            settings::ShellDiscriminants::System => vec![],
                            settings::ShellDiscriminants::Program => vec![SettingItem {
                                files: USER | PROJECT,
                                title: "プログラム",
                                description: "使用するシェルのプログラムです。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("terminal.shell"),
                                    pick: |settings_content| match settings_content.terminal.as_ref()?.project.shell.as_ref()
                                    {
                                        Some(settings::Shell::Program(program)) => Some(program),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .terminal
                                            .get_or_insert_default()
                                            .project
                                            .shell
                                            .as_mut()
                                        {
                                            Some(settings::Shell::Program(program)) => *program = value,
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            }],
                            settings::ShellDiscriminants::WithArguments => vec![
                                SettingItem {
                                    files: USER | PROJECT,
                                    title: "プログラム",
                                    description: "実行するシェルのプログラムです。",
                                    field: Box::new(SettingField {
                                        organization_override: None,
                                        json_path: Some("terminal.shell.program"),
                                        pick: |settings_content| {
                                            match settings_content.terminal.as_ref()?.project.shell.as_ref() {
                                                Some(settings::Shell::WithArguments { program, .. }) => Some(program),
                                                _ => None,
                                            }
                                        },
                                        write: |settings_content, value, _| {
                                            let Some(value) = value else {
                                                return;
                                            };
                                            match settings_content
                                                .terminal
                                                .get_or_insert_default()
                                                .project
                                                .shell
                                                .as_mut()
                                            {
                                                Some(settings::Shell::WithArguments { program, .. }) => {
                                                    *program = value
                                                }
                                                _ => return,
                                            }
                                        },
                                    }),
                                    metadata: None,
                                },
                                SettingItem {
                                    files: USER | PROJECT,
                                    title: "引数",
                                    description: "シェルのプログラムに渡す引数です。",
                                    field: Box::new(
                                        SettingField {
                                            organization_override: None,
                                            json_path: Some("terminal.shell.args"),
                                            pick: |settings_content| {
                                                match settings_content.terminal.as_ref()?.project.shell.as_ref() {
                                                    Some(settings::Shell::WithArguments { args, .. }) => Some(args),
                                                    _ => None,
                                                }
                                            },
                                            write: |settings_content, value, _| {
                                                let Some(value) = value else {
                                                    return;
                                                };
                                                match settings_content
                                                    .terminal
                                                    .get_or_insert_default()
                                                    .project
                                                    .shell
                                                    .as_mut()
                                                {
                                                    Some(settings::Shell::WithArguments { args, .. }) => *args = value,
                                                    _ => return,
                                                }
                                            },
                                        }
                                        .unimplemented(),
                                    ),
                                    metadata: None,
                                },
                                SettingItem {
                                    files: USER | PROJECT,
                                    title: "タイトルの上書き",
                                    description: "ターミナルのタブのタイトルを上書きする文字列です (省略可)。",
                                    field: Box::new(SettingField {
                                        organization_override: None,
                                        json_path: Some("terminal.shell.title_override"),
                                        pick: |settings_content| {
                                            match settings_content.terminal.as_ref()?.project.shell.as_ref() {
                                                Some(settings::Shell::WithArguments { title_override, .. }) => {
                                                    title_override.as_ref().or(DEFAULT_EMPTY_STRING)
                                                }
                                                _ => None,
                                            }
                                        },
                                        write: |settings_content, value, _| {
                                            match settings_content
                                                .terminal
                                                .get_or_insert_default()
                                                .project
                                                .shell
                                                .as_mut()
                                            {
                                                Some(settings::Shell::WithArguments { title_override, .. }) => {
                                                    *title_override = value.filter(|s| !s.is_empty())
                                                }
                                                _ => return,
                                            }
                                        },
                                    }),
                                    metadata: None,
                                },
                            ],
                        })
                        .collect(),
                }),
                SettingsPageItem::DynamicItem(DynamicItem {
                    discriminant: SettingItem {
                        files: USER | PROJECT,
                        title: "作業ディレクトリ",
                        description: "ターミナルを起動するときに使う作業ディレクトリです。",
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("terminal.working_directory$"),
                            pick: |settings_content| {
                                Some(&dynamic_variants::<settings::WorkingDirectory>()[
                                    settings_content
                                        .terminal
                                        .as_ref()?
                                        .project
                                        .working_directory
                                        .as_ref()?
                                        .discriminant() as usize
                                ])
                            },
                            write: |settings_content, value, _| {
                                let Some(value) = value else {
                                    if let Some(terminal) = settings_content.terminal.as_mut() {
                                        terminal.project.working_directory = None;
                                    }
                                    return;
                                };
                                let settings_value = settings_content
                                    .terminal
                                    .get_or_insert_default()
                                    .project
                                    .working_directory
                                    .get_or_insert_with(|| settings::WorkingDirectory::CurrentProjectDirectory);
                                *settings_value = match value {
                                    settings::WorkingDirectoryDiscriminants::CurrentFileDirectory => {
                                        settings::WorkingDirectory::CurrentFileDirectory
                                    },
                                    settings::WorkingDirectoryDiscriminants::CurrentProjectDirectory => {
                                        settings::WorkingDirectory::CurrentProjectDirectory
                                    }
                                    settings::WorkingDirectoryDiscriminants::FirstProjectDirectory => {
                                        settings::WorkingDirectory::FirstProjectDirectory
                                    }
                                    settings::WorkingDirectoryDiscriminants::AlwaysHome => {
                                        settings::WorkingDirectory::AlwaysHome
                                    }
                                    settings::WorkingDirectoryDiscriminants::Always => {
                                        let directory = match settings_value {
                                            settings::WorkingDirectory::Always { .. } => return,
                                            _ => String::new(),
                                        };
                                        settings::WorkingDirectory::Always { directory }
                                    }
                                };
                            },
                        }),
                        metadata: None,
                    },
                    pick_discriminant: |settings_content| {
                        Some(
                            settings_content
                                .terminal
                                .as_ref()?
                                .project
                                .working_directory
                                .as_ref()?
                                .discriminant() as usize,
                        )
                    },
                    fields: dynamic_variants::<settings::WorkingDirectory>()
                        .into_iter()
                        .map(|variant| match variant {
                            settings::WorkingDirectoryDiscriminants::CurrentFileDirectory => vec![],
                            settings::WorkingDirectoryDiscriminants::CurrentProjectDirectory => vec![],
                            settings::WorkingDirectoryDiscriminants::FirstProjectDirectory => vec![],
                            settings::WorkingDirectoryDiscriminants::AlwaysHome => vec![],
                            settings::WorkingDirectoryDiscriminants::Always => vec![SettingItem {
                                files: USER | PROJECT,
                                title: "ディレクトリ",
                                description: "使用するディレクトリのパスです (シェルによる展開が行われます)。",
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("terminal.working_directory.always"),
                                    pick: |settings_content| {
                                        match settings_content.terminal.as_ref()?.project.working_directory.as_ref() {
                                            Some(settings::WorkingDirectory::Always { directory }) => Some(directory),
                                            _ => None,
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let value = value.unwrap_or_default();
                                        match settings_content
                                            .terminal
                                            .get_or_insert_default()
                                            .project
                                            .working_directory
                                            .as_mut()
                                        {
                                            Some(settings::WorkingDirectory::Always { directory }) => *directory = value,
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            }],
                        })
                        .collect(),
                }),
                SettingsPageItem::SettingItem(SettingItem {
                    title: "環境変数",
                    description: "ターミナルの環境に追加するキーと値の組です。",
                    field: Box::new(
                        SettingField {
                            organization_override: None,
                            json_path: Some("terminal.env"),
                            pick: |settings_content| settings_content.terminal.as_ref()?.project.env.as_ref(),
                            write: |settings_content, value, _| {
                                settings_content.terminal.get_or_insert_default().project.env = value;
                            },
                        }
                        .unimplemented(),
                    ),
                    metadata: None,
                    files: USER | PROJECT,
                }),
                SettingsPageItem::SettingItem(SettingItem {
                    title: "仮想環境を検出",
                    description: "ターミナルの作業ディレクトリに Python の仮想環境があれば有効化します。",
                    field: Box::new(
                        SettingField {
                            organization_override: None,
                            json_path: Some("terminal.detect_venv"),
                            pick: |settings_content| settings_content.terminal.as_ref()?.project.detect_venv.as_ref(),
                            write: |settings_content, value, _| {
                                settings_content
                                    .terminal
                                    .get_or_insert_default()
                                    .project
                                    .detect_venv = value;
                            },
                        }
                        .unimplemented(),
                    ),
                    metadata: None,
                    files: USER | PROJECT,
                }),
            ]
    }

    fn font_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("フォント"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントサイズ",
                description: "ターミナルのテキストのフォントサイズです。未設定の場合はバッファーのフォントサイズを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.font_size"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|terminal| terminal.font_size.as_ref())
                            .or(settings_content.theme.buffer_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントファミリー",
                description: "ターミナルのテキストのフォントファミリーです。未設定の場合はバッファーのフォントファミリーを使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.font_family"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|terminal| terminal.font_family.as_ref())
                            .or(settings_content.theme.buffer_font_family.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォールバックフォント",
                description: "ターミナルのテキストのフォールバックフォントです。未設定の場合はバッファーのフォールバックフォントを使います。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.font_fallbacks"),
                        pick: |settings_content| {
                            settings_content
                                .terminal
                                .as_ref()
                                .and_then(|terminal| terminal.font_fallbacks.as_ref())
                                .or(settings_content.theme.buffer_font_fallbacks.as_ref())
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .font_fallbacks = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォントの太さ",
                description: "ターミナルのテキストのフォントの太さです (CSS の重み単位で 100〜900)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.font_weight"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.font_weight.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .font_weight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォント機能",
                description: "ターミナルのテキストのフォント機能です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.font_features"),
                        pick: |settings_content| {
                            settings_content
                                .terminal
                                .as_ref()
                                .and_then(|terminal| terminal.font_features.as_ref())
                                .or(settings_content.theme.buffer_font_features.as_ref())
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .font_features = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn display_settings_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("表示の設定"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "行の高さ",
                description: "ターミナルのテキストの行の高さです。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.line_height"),
                        pick: |settings_content| {
                            settings_content.terminal.as_ref()?.line_height.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .line_height = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの形状",
                description: "ターミナルの既定のカーソルの形状です (bar・block・underline・hollow)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.cursor_shape"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.cursor_shape.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .cursor_shape = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "カーソルの点滅",
                description: "ターミナルのカーソルの点滅動作を設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.blinking"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.blinking.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().blinking = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "代替スクロール",
                description: "代替スクロールモードを既定で有効にするかどうかを設定します (Vim などのアプリでマウスのスクロールを矢印キーに変換します)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.alternate_scroll"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .alternate_scroll
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .alternate_scroll = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "最小コントラスト",
                description: "前景色と背景色の間で保つ APCA 知覚コントラストの最小値です (0〜106)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.minimum_contrast"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .minimum_contrast
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .minimum_contrast = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn behavior_settings_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader("動作の設定"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Option キーを Meta キーとして扱う",
                description: "Option キーを Meta キーとして扱うかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.option_as_meta"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.option_as_meta.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .option_as_meta = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "選択時にコピー",
                description: "ターミナルでテキストを選択したときに、自動的にシステムのクリップボードへコピーするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.copy_on_select"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.copy_on_select.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .copy_on_select = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コピー後も選択を維持",
                description: "クリップボードへコピーした後もテキストの選択を維持するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.keep_selection_on_copy"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .keep_selection_on_copy
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .keep_selection_on_copy = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マウスモードでもリンクを開く",
                description: "ターミナルのアプリケーションがマウスレポートを有効にしている場合でも、cmd + クリック (Linux と Windows では ctrl + クリック) でハイパーリンクを開くかどうかを設定します。無効にするとこれらのクリックはアプリケーションへ転送されますが、shift + cmd + クリックでリンクを開けます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.open_links_in_mouse_mode"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .open_links_in_mouse_mode
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .open_links_in_mouse_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マクロのフォルダーを閉じて開く",
                description: "「マクロを実行」を開いたとき、フォルダーをすべて閉じた状態で表示します。絞り込みを入力すると一致するマクロはすべて表示されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.macro_picker_folders_collapsed"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .macro_picker_folders_collapsed
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .macro_picker_folders_collapsed = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ベル音",
                description: "BEL 文字 (`\\a`、`0x07`) が出力されたときに音を鳴らすかどうかを設定します",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.bell"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.bell.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().bell = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn layout_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("レイアウトの設定"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定の幅",
                description: "ターミナルを左右にドッキングしたときの既定の幅 (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.default_width"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.default_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定の高さ",
                description: "ターミナルを下にドッキングしたときの既定の高さ (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.default_height"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.default_height.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .default_height = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn advanced_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("詳細設定"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スクロール履歴の最大行数",
                description: "スクロールバックの履歴に保持する最大の行数です (最大 100,000。0 でスクロールを無効化)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.max_scroll_history_lines"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .max_scroll_history_lines
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .max_scroll_history_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スクロールの倍率",
                description: "ターミナルをマウスホイールでスクロールするときの倍率です",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.scroll_multiplier"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .scroll_multiplier
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .scroll_multiplier = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn toolbar_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("ツールバー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "パンくずリスト",
                description: "ターミナルペイン内のパンくずリストに、ターミナルのタイトルを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.toolbar.breadcrumbs"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .toolbar
                            .as_ref()?
                            .breadcrumbs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .toolbar
                            .get_or_insert_default()
                            .breadcrumbs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scrollbar_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("スクロールバー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スクロールバーを表示",
                description: "ターミナルにスクロールバーを表示するタイミングです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.scrollbar.show"),
                    pick: |settings_content| {
                        show_scrollbar_or_editor(settings_content, |settings_content| {
                            settings_content
                                .terminal
                                .as_ref()?
                                .scrollbar
                                .as_ref()?
                                .show
                                .as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn session_log_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("セッションログ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "自動でログを記録する",
                description: "新しく開いたターミナルが、出力の記録を自動で始めます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.session_log.auto"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .session_log
                            .as_ref()?
                            .auto
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .session_log
                            .get_or_insert_default()
                            .auto = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "保存先フォルダー",
                description: "ログファイルを書き出すフォルダーです (`~` は展開されます)。空欄のときは Zed のログフォルダー配下の `terminal-sessions` に保存します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.session_log.directory"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .session_log
                            .as_ref()?
                            .directory
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .session_log
                            .get_or_insert_default()
                            .directory = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ファイル名のテンプレート",
                description: "新しいログのファイル名です。`{date}` `{time}` `{title}` `{host}` `{pid}` を展開します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.session_log.file_name_template"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .session_log
                            .as_ref()?
                            .file_name_template
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .session_log
                            .get_or_insert_default()
                            .file_name_template = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "制御シーケンスを除去する",
                description: "ANSI / VT のエスケープシーケンスを取り除いて、素のテキストとして保存します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.session_log.strip_control_sequences"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .session_log
                            .as_ref()?
                            .strip_control_sequences
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .session_log
                            .get_or_insert_default()
                            .strip_control_sequences = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "行頭にタイムスタンプを付ける",
                description: "各行の先頭に `[YYYY-MM-DD HH:MM:SS] ` を付けます。記録中のターミナルでは、記録中のバーからも切り替えられます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.session_log.timestamps"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .session_log
                            .as_ref()?
                            .timestamps
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .session_log
                            .get_or_insert_default()
                            .timestamps = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn remote_persistence_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("リモートターミナルの切断対策"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "切断してもシェルを動かし続ける",
                description: "SSH のターミナルのシェルをリモートの Zed サーバーに預けます。接続が切れても実行中のコマンドは止まらず、再接続するとそれまでの出力を表示し直して続きから使えます。タブを閉じるとシェルも終了します。Windows のリモートホストでは使えません。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.remote_persistence.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .remote_persistence
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .remote_persistence
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "未接続のシェルを終了するまでの時間",
                description: "どのターミナルからも接続されていない状態がこの時間 (時間単位) 続いたら、リモートのシェルを終了します。0 のときは終了しません。新しく起動するシェルから反映されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.remote_persistence.detached_timeout_hours"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .remote_persistence
                            .as_ref()?
                            .detached_timeout_hours
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .remote_persistence
                            .get_or_insert_default()
                            .detached_timeout_hours = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "ターミナル",
        items: concat_sections![
            environment_section(),
            font_section(),
            display_settings_section(),
            behavior_settings_section(),
            layout_settings_section(),
            advanced_settings_section(),
            session_log_section(),
            remote_persistence_section(),
            toolbar_section(),
            scrollbar_section(),
        ],
    }
}

fn version_control_page() -> SettingsPage {
    fn git_integration_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Git 連携"),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "Git 連携を無効にする",
                    description: "Zed の Git 連携機能をすべて無効にします。表示だけでなく、ワークツリーの Git リポジトリ追跡と git status / git diff の実行自体を止めます。リモート SSH ではサーバー側の実行も止まります。切り替えは再起動なしで反映されます。",
                    field: Box::new(SettingField::<bool> {
                        organization_override: None,
                        json_path: Some("git.disable_git"),
                        pick: |settings_content| {
                            settings_content
                                .git
                                .as_ref()?
                                .enabled
                                .as_ref()?
                                .disable_git
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .git
                                .get_or_insert_default()
                                .enabled
                                .get_or_insert_default()
                                .disable_git = value;
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    let disabled = settings_content
                        .git
                        .as_ref()?
                        .enabled
                        .as_ref()?
                        .disable_git
                        .unwrap_or(false);
                    Some(if disabled { 0 } else { 1 })
                },
                fields: vec![
                    vec![],
                    vec![
                        SettingItem {
                            files: USER,
                            title: "Git の状態を有効にする",
                            description: "エディターに Git の状態情報を表示します。",
                            field: Box::new(SettingField::<bool> {
                                organization_override: None,
                                json_path: Some("git.enable_status"),
                                pick: |settings_content| {
                                    settings_content
                                        .git
                                        .as_ref()?
                                        .enabled
                                        .as_ref()?
                                        .enable_status
                                        .as_ref()
                                },
                                write: |settings_content, value, _| {
                                    settings_content
                                        .git
                                        .get_or_insert_default()
                                        .enabled
                                        .get_or_insert_default()
                                        .enable_status = value;
                                },
                            }),
                            metadata: None,
                        },
                        SettingItem {
                            files: USER,
                            title: "Git の差分を有効にする",
                            description: "エディターに Git の差分情報を表示します。",
                            field: Box::new(SettingField::<bool> {
                                organization_override: None,
                                json_path: Some("git.enable_diff"),
                                pick: |settings_content| {
                                    settings_content
                                        .git
                                        .as_ref()?
                                        .enabled
                                        .as_ref()?
                                        .enable_diff
                                        .as_ref()
                                },
                                write: |settings_content, value, _| {
                                    settings_content
                                        .git
                                        .get_or_insert_default()
                                        .enabled
                                        .get_or_insert_default()
                                        .enable_diff = value;
                                },
                            }),
                            metadata: None,
                        },
                    ],
                ],
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "追跡されていないファイルの検出範囲",
                description: "git status に追跡されていないファイルをどこまで報告させるかを設定します。All は追跡されていないフォルダーの中身も 1 件ずつ報告するため、プロジェクトパネルの色分けが正確になりますが、巨大なリポジトリでは最も重くなります。Normal はフォルダーだけ、No は追跡されていないファイルを一切扱いません。リモート SSH でも同じ値がサーバー側で使われます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.status_untracked_files"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .status_untracked_files
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .status_untracked_files = value;
                    },
                }),
                metadata: None,
            }),
        ]
    }

    fn git_gutter_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Git ガター"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "表示",
                description: "エディターのガターに Git の状態を表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.git_gutter"),
                    pick: |settings_content| settings_content.git.as_ref()?.git_gutter.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().git_gutter = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings_ui): Figure out the right default for this value in default.json
            SettingsPageItem::SettingItem(SettingItem {
                title: "デバウンス",
                description: "変更が Git ガターに反映されるまでのデバウンスのしきい値 (ミリ秒) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.gutter_debounce"),
                    pick: |settings_content| {
                        settings_content.git.as_ref()?.gutter_debounce.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().gutter_debounce = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn inline_git_blame_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("インライン Git blame"),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    title: "有効",
                    description: "現在フォーカスしている行の Git blame 情報を表示するかどうかを設定します。",
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("git.inline_blame.enabled"),
                        pick: |settings_content| {
                            settings_content
                                .git
                                .as_ref()?
                                .inline_blame
                                .as_ref()?
                                .enabled
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .git
                                .get_or_insert_default()
                                .inline_blame
                                .get_or_insert_default()
                                .enabled = value;
                        },
                    }),
                    metadata: None,
                    files: USER,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        *settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .enabled
                            .as_ref()? as usize,
                    )
                },
                fields: vec![
                    vec![],
                    vec![SettingItem {
                        title: "表示位置",
                        description: "Git blame を有効にしたときの表示位置です。",
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("git.inline_blame.location"),
                            pick: |settings_content| {
                                settings_content
                                    .git
                                    .as_ref()?
                                    .inline_blame
                                    .as_ref()?
                                    .location
                                    .as_ref()
                            },
                            write: |settings_content, value, _| {
                                settings_content
                                    .git
                                    .get_or_insert_default()
                                    .inline_blame
                                    .get_or_insert_default()
                                    .location = value;
                            },
                        }),
                        metadata: None,
                        files: USER,
                    }],
                ],
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "遅延",
                description: "インライン blame の情報を表示するまでの遅延です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.delay_ms"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .delay_ms
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .delay_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "余白",
                description: "ソース行の末尾からインライン blame の開始位置までの余白 (列数) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.padding"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .padding = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "最小の列",
                description: "インライン blame の情報を表示する最小の列位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.min_column"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .min_column
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .min_column = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "コミットの概要を表示",
                description: "インライン blame にコミットの概要を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.show_commit_summary"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .show_commit_summary
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .show_commit_summary = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn git_blame_view_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Git blame ビュー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アバターを表示",
                description: "コミットの作成者のアバターを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.blame.show_avatar"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .blame
                            .as_ref()?
                            .show_avatar
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .blame
                            .get_or_insert_default()
                            .show_avatar = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn branch_picker_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("ブランチの選択"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "作成者名を表示",
                description: "ブランチの選択で、コミット情報の一部として作成者名を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.branch_picker.show_author_name"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .branch_picker
                            .as_ref()?
                            .show_author_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .branch_picker
                            .get_or_insert_default()
                            .show_author_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn git_hunks_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Git の変更箇所"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "変更箇所のスタイル",
                description: "エディターで Git の変更箇所をどのように表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.hunk_style"),
                    pick: |settings_content| settings_content.git.as_ref()?.hunk_style.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().hunk_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "差分の基準",
                description: "git の機能が HEAD との差分 (未コミットの変更) を示すか、既定のブランチとの差分 (現在のブランチのすべての変更) を示すかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.diff_base"),
                    pick: |settings_content| settings_content.git.as_ref()?.diff_base.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().diff_base = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "パスの表示スタイル",
                description: "git のビューで、名前とパスのどちらを先に表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.path_style"),
                    pick: |settings_content| settings_content.git.as_ref()?.path_style.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().path_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ステージ/元に戻すボタンを表示",
                description: "差分の変更箇所に、ステージと元に戻すのボタンを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.show_stage_restore_buttons"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .show_stage_restore_buttons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .show_stage_restore_buttons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn file_diff_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("ファイルの差分"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "既定でファイル全体を表示",
                description: "新しく開いたファイルの差分で、変更箇所だけでなくファイル全体を表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.file_diff.show_full_file"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .file_diff
                            .as_ref()?
                            .show_full_file
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .file_diff
                            .get_or_insert_default()
                            .show_full_file = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "バージョン管理",
        items: concat_sections![
            git_integration_section(),
            git_gutter_section(),
            inline_git_blame_section(),
            git_blame_view_section(),
            branch_picker_section(),
            file_diff_section(),
            git_hunks_section(),
        ],
    }
}

fn collaboration_page() -> SettingsPage {
    fn calls_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("通話"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "参加時にミュート",
                description: "チャンネルや通話に参加するときにマイクをミュートするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("calls.mute_on_join"),
                    pick: |settings_content| settings_content.calls.as_ref()?.mute_on_join.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.calls.get_or_insert_default().mute_on_join = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "参加時に共有",
                description: "空のチャンネルに参加するときに、現在のプロジェクトを共有するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("calls.share_on_join"),
                    pick: |settings_content| {
                        settings_content.calls.as_ref()?.share_on_join.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.calls.get_or_insert_default().share_on_join = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn audio_settings() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::ActionLink(ActionLink {
                title: "音声のテスト".into(),
                description: Some("マイクとスピーカーの設定をテストします".into()),
                button_text: "音声をテスト".into(),
                on_click: Arc::new(|_settings_window, window, cx| {
                    open_audio_test_window(window, cx);
                }),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "出力オーディオデバイス",
                description: "出力オーディオデバイスを選択します",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("audio.experimental.output_audio_device"),
                    pick: |settings_content| {
                        settings_content
                            .audio
                            .as_ref()?
                            .output_audio_device
                            .as_ref()
                            .or(DEFAULT_EMPTY_AUDIO_OUTPUT)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .audio
                            .get_or_insert_default()
                            .output_audio_device = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "入力オーディオデバイス",
                description: "入力オーディオデバイスを選択します",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("audio.experimental.input_audio_device"),
                    pick: |settings_content| {
                        settings_content
                            .audio
                            .as_ref()?
                            .input_audio_device
                            .as_ref()
                            .or(DEFAULT_EMPTY_AUDIO_INPUT)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .audio
                            .get_or_insert_default()
                            .input_audio_device = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "コラボレーション",
        items: concat_sections![calls_section(), audio_settings()],
    }
}

fn ai_page(cx: &App) -> SettingsPage {
    fn general_section() -> [SettingsPageItem; 16] {
        [
            SettingsPageItem::SectionHeader("全般"),
            // The only UI writer of `agent.enabled` (ADR-F13, `docs/jp-enhance/DESIGN.md`):
            // keeping it unique is what makes the enablement audit meaningful.
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントを有効にする",
                description: "エージェントパネル・インラインアシスタント・コミットメッセージ生成などのエージェント機能を有効にするかどうかを設定します。既定では無効です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.enabled"),
                    pick: |settings_content| settings_content.agent.as_ref()?.enabled.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "AI を無効にする",
                description: "Zed の AI 機能をすべて無効にするかどうかを設定します。一度有効にすると設定ファイルから消すまで元に戻せません。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("disable_ai"),
                    pick: |settings_content| settings_content.project.disable_ai.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.project.disable_ai = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スレッドサイドバー",
                description: "ウィンドウ幅のスレッドサイドバーを表示します。オフのときはエージェントパネル内のスレッド履歴を使います。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.sidebar_enabled"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.sidebar_enabled.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().sidebar_enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スレッドサイドバーの位置",
                description: "スレッドサイドバーをウィンドウのどちら側に表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.sidebar_side"),
                    pick: |settings_content| settings_content.agent.as_ref()?.sidebar_side.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().sidebar_side = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "パネル内にスレッド履歴を表示",
                description: "エージェントパネルの中に、現在のスレッドの左側へスレッド履歴を分割表示します。パネルの幅が足りないときは自動的に隠れます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.panel_thread_history"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.panel_thread_history.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .panel_thread_history = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "パネルのタブバー",
                description: "エージェントパネル内のタブ列をいつ表示するかを設定します。「自動」はパネル内のスレッド履歴が出ていて分割していないとき (履歴側に開いているタブが並ぶため) 隠します。「表示しない」でも、パネルが狭くて履歴もその帯も出せないときは、開いているタブへ届かなくなるので表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.panel_tab_bar"),
                    pick: |settings_content| settings_content.agent.as_ref()?.panel_tab_bar.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().panel_tab_bar = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "LLM プロバイダー".into(),
                r#type: Default::default(),
                json_path: Some("llm_providers"),
                description: Some("標準で同梱されているモデルプロバイダーを設定します。".into()),
                search_aliases: &[
                    "ai",
                    "amazon",
                    "anthropic",
                    "api key",
                    "azure",
                    "bedrock",
                    "chat",
                    "claude",
                    // The page answers "where is the Claude subscription?": it
                    // is the official Claude Code CLI run in the integrated
                    // terminal, not an entry in this list (ADR-F20).
                    "claude code",
                    "claude max",
                    "claude pro",
                    "copilot",
                    "gemini",
                    "github",
                    "google",
                    "gpt",
                    "grok",
                    // The Claude Code install / launch buttons live on this page.
                    "install",
                    "llama",
                    "llm",
                    "lm studio",
                    "mistral",
                    "npm",
                    "ollama",
                    "openai",
                    "opencode",
                    "provider",
                    "subscription",
                    "vercel",
                    "xai",
                ],
                in_json: false,
                files: USER,
                render: render_llm_providers_page,
            }),
            // The provider names listed here are the same ones warned about in
            // `llm_providers_page` / `edit_prediction_provider_setup`; this link
            // only shows what has already been acknowledged there and lets it be
            // withdrawn (ADR-F10, `docs/jp-enhance/DESIGN.md` §4.10).
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "承諾済みの外部 AI プロバイダー".into(),
                r#type: Default::default(),
                json_path: Some("acknowledged_external_ai_providers"),
                description: Some(
                    "学習リスクの警告に同意して有効化した外部 AI プロバイダーの一覧です。承諾を取り消すと、次にそのプロバイダーを使うときに再び警告が表示されます。"
                        .into(),
                ),
                search_aliases: &[
                    "acknowledge",
                    "acknowledged",
                    "ai",
                    "consent",
                    "external ai",
                    "provider",
                    "revoke",
                ],
                in_json: false,
                files: USER,
                render: render_acknowledged_ai_providers_page,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "外部エージェント".into(),
                r#type: Default::default(),
                json_path: Some("agent_servers"),
                description: Some(
                    "Agent Client Protocol で接続するエージェントの確認・追加・削除を行います。"
                        .into(),
                ),
                search_aliases: &[
                    "acp",
                    "agent client protocol",
                    "amp",
                    "claude agent",
                    "claude code",
                    "codex",
                    "copilot cli",
                    "cursor",
                    "external agent",
                    "factory droid",
                    "github copilot",
                    "grok build",
                    "junie",
                    "opencode",
                ],
                in_json: false,
                files: USER,
                render: render_external_agents_page,
            }),
            SettingsPageItem::ActionLink(ActionLink {
                title: "サンドボックスの外での起動の同意".into(),
                description: Some(
                    "スレッドごとに「サンドボックスの外で起動」に同意した記録を取り消し、次の起動の前にもう一度確認するようにします。動いているエージェントは止めません。「常にサンドボックスの外で起動」の設定は変わりません。"
                        .into(),
                ),
                button_text: "同意を取り消す".into(),
                on_click: Arc::new(|_settings_window, window, cx| {
                    window.dispatch_action(
                        zed_actions::agent::RevokeOutsideSandboxConsents.boxed_clone(),
                        cx,
                    );
                }),
                files: USER,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "MCP サーバー".into(),
                r#type: SubPageType::McpServers,
                json_path: Some("context_servers"),
                description: Some(
                    "Model Context Protocol サーバーの確認・追加・設定・削除を行います。".into(),
                ),
                search_aliases: &["context server", "mcp", "model context protocol"],
                in_json: false,
                files: USER,
                render: render_mcp_servers_page,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "IDE サーバーを有効にする",
                description: "Zed 自身を 127.0.0.1 の IDE サーバーとして公開し、統合ターミナルの Claude Code CLI や外部のエージェントから、開いているファイル・選択範囲・診断を読めるようにします。差分はエディターのタブで「採用 / 拒否」を選べます。外向きの通信はしません。既定では無効です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ide_server.enabled"),
                    pick: |settings_content| settings_content.ide_server.as_ref()?.enabled.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.ide_server.get_or_insert_default().enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントに IDE ツールを渡す",
                description: "Claude Code のタブや外部のエージェントに、ハイライト・注釈・ツアー・定義・参照・シンボル検索のツールを渡します。「IDE サーバーを有効にする」とは別の設定で、ロックファイルは書きません。外向きの通信はしません。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ide_server.assist_tools"),
                    pick: |settings_content| {
                        settings_content.ide_server.as_ref()?.assist_tools.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.ide_server.get_or_insert_default().assist_tools = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "開いているファイルを Claude Code に伝える",
                description: "何も選んでいないときも、開いているファイルを接続中の Claude Code に伝えます (入力欄の「In <ファイル名>」)。オフにすると、行を選んだときだけ送ります。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ide_server.share_active_file"),
                    pick: |settings_content| {
                        settings_content.ide_server.as_ref()?.share_active_file.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .ide_server
                            .get_or_insert_default()
                            .share_active_file = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "失敗したタスクの出力をエージェントに渡す",
                description: "最後に失敗したタスク (0 以外で終了) の出力の末尾 (最大 4 KB) を、IDE サーバーのツール getLastTaskOutput でエージェントが読めるようにします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ide_server.share_task_output"),
                    pick: |settings_content| {
                        settings_content.ide_server.as_ref()?.share_task_output.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .ide_server
                            .get_or_insert_default()
                            .share_task_output = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn agent_configuration_section(_cx: &App) -> Box<[SettingsPageItem]> {
        let mut items = vec![SettingsPageItem::SectionHeader("エージェントの設定")];

        items.extend([
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "スキル".into(),
                r#type: Default::default(),
                json_path: Some(zed_actions::AGENT_SKILLS_SETTINGS_PATH),
                description: Some("グローバル、またはプロジェクトのワークツリーにインストールされたエージェントのスキルを確認・管理します。".into()),
                search_aliases: &["agent skill", "agent skills", "custom instructions", "skill", "skills"],
                in_json: false,
                files: USER | PROJECT,
                render: render_skills_setup_page,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "サンドボックス".into(),
                r#type: Default::default(),
                json_path: Some(zed_actions::AGENT_SANDBOX_SETTINGS_PATH),
                description: Some(
                    "エージェントのターミナルコマンドを OS のサンドボックスで実行するかどうかを切り替え、サンドボックス内の許可 (書き込み先・ネットワーク) を設定します。自動承認はサンドボックス内で実行するときだけ効きます。"
                        .into(),
                ),
                search_aliases: &[
                    "allow",
                    "domain",
                    "filesystem",
                    "network",
                    "sandbox",
                    "unsandboxed",
                    "permissions",
                ],
                in_json: true,
                files: USER,
                render: render_sandbox_settings_page,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "ツールの権限".into(),
                r#type: Default::default(),
                json_path: Some("agent.tool_permissions"),
                description: Some("特定のツール入力に対して自動的に許可・拒否する、あるいは常に確認を求める正規表現パターンを設定します。".into()),
                search_aliases: &[],
                in_json: true,
                files: USER,
                render: render_tool_permissions_setup_page,
            }),
        ]);

        items.extend([
            SettingsPageItem::SettingItem(SettingItem {
                title: "単一ファイルでのレビュー",
                description: "有効にすると、エージェントの編集を単一ファイルのバッファーにも表示してレビューできるようにします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.single_file_review"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.single_file_review.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .single_file_review = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フィードバックを有効にする",
                description: "エージェントの編集に対するフィードバック用の、高評価/低評価のボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: Some(|org_config| if org_config.is_agent_thread_feedback_enabled {
                        None
                    } else {
                        Some(&false)
                    }),
                    json_path: Some("agent.enable_feedback"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.enable_feedback.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .enable_feedback = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントの待機時に通知",
                description: "エージェントが応答を終えたとき、またはツールの実行前に確認が必要なときに、通知をどこに表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.notify_when_agent_waiting"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .notify_when_agent_waiting
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .notify_when_agent_waiting = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "エージェントの完了時に音を鳴らす",
                description: "エージェントが応答を終えたとき、またはユーザーの入力が必要なときに、音を鳴らすタイミングを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.play_sound_when_agent_done"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .play_sound_when_agent_done
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .play_sound_when_agent_done = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "編集カードを展開",
                description: "エージェントパネルの編集カードを展開し、差分のプレビューを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.expand_edit_card"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.expand_edit_card.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .expand_edit_card = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ターミナルカードを展開",
                description: "エージェントパネルのターミナルカードを展開し、コマンドの出力全体を表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.expand_terminal_card"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .expand_terminal_card
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .expand_terminal_card = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ターミナルスレッドの初期化コマンド",
                description: "エージェントパネルでターミナルスレッドのシェルを作成したときに、自動的に実行するコマンドです。設定済みのシェルで実行されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.terminal_init_command"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .terminal_init_command
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .terminal_init_command = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("例: claude"),
                    display_confirm_button: true,
                    display_clear_button: true,
                    confirm_on_focus_out: true,
                    treat_missing_text_as_empty: true,
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "思考の表示",
                description: "思考ブロックを既定でどのように表示するかを設定します。'Auto' はストリーミング中は完全に展開し、完了後に自動的に折りたたみます。'Preview' は高さを制限した状態でストリーミング中に自動展開します。'Always Expanded' は常に全体を表示します。'Always Collapsed' は常に折りたたんだままにします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.thinking_display"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .thinking_display
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .thinking_display = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ターミナルの停止時に生成を中止",
                description: "実行中のターミナルツールの停止ボタンを押したときに、エージェントの生成も中止するかどうかを設定します。これは停止ボタンにのみ適用され、ターミナル内の ctrl+c には適用されません。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.cancel_generation_on_terminal_stop"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .cancel_generation_on_terminal_stop
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .cancel_generation_on_terminal_stop = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "修飾キーで送信",
                description: "メッセージの送信に常に cmd + Enter (Linux と Windows では ctrl + Enter) を使うかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.use_modifier_to_send"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .use_modifier_to_send
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .use_modifier_to_send = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "メッセージエディターの最小行数",
                description: "エージェントのメッセージエディターに表示する最小の行数です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.message_editor_min_lines"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .message_editor_min_lines
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .message_editor_min_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ターンの統計を表示",
                description: "生成中の経過時間やターンの所要時間などの統計を表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.show_turn_stats"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.show_turn_stats.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .show_turn_stats = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "マージコンフリクトのインジケーターを表示",
                description: "エージェントでコンフリクトを解決できることを示すインジケーターを、ステータスバーに表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.show_merge_conflict_indicator"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.show_merge_conflict_indicator.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .show_merge_conflict_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]);

        items.extend([
            SettingsPageItem::SettingItem(SettingItem {
                title: "自動圧縮",
                description: "エージェントのコンテキストが大きくなりすぎたときに自動的に圧縮し、以前のメッセージを要約してモデルのコンテキストウィンドウに空きを作ります。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.auto_compact.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .auto_compact
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .auto_compact
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "自動圧縮のしきい値",
                description: "自動圧縮を実行する条件です。\"90%\" のようなパーセント文字列は、コンテキストウィンドウに対する割合として扱われます。正の整数は、使用トークン数がその値を超えたら圧縮することを意味します。負の整数は、コンテキストウィンドウの残りトークン数がその値を下回ったら圧縮することを意味します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.auto_compact.threshold"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .auto_compact
                            .as_ref()?
                            .threshold
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .auto_compact
                            .get_or_insert_default()
                            .threshold = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("90%"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "自動圧縮を行う最小コンテキストウィンドウ",
                description: "自動圧縮を実行するモデルのコンテキストウィンドウの下限 (トークン数) です。これを下回るモデルでは圧縮を行わず、トークン上限の警告だけを表示します。ローカル LLM の小さいコンテキストでも自動圧縮を効かせたいときに下げてください。8000 未満を指定しても 8000 として扱われます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.auto_compact_min_context_window"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .auto_compact_min_context_window
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .auto_compact_min_context_window = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("80000"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アクティブなファイルを自動で添付",
                description: "メッセージを送信するときに、アクティブなエディターを自動で添付します。選択範囲があればその行だけを、なければファイル全体を添付します。同じものを手で @ メンションしている場合は二重に添付しません。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.attach_active_file"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.attach_active_file.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .attach_active_file = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "添付を全文で送る上限 (バイト)",
                description: "これより大きいファイルは全文ではなくアウトライン (シンボルと行番号の一覧) で送り、必要な範囲だけエージェントに読ませます。1024 未満は 1024 として扱われます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.context_attachments.inline_max_bytes"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .context_attachments
                            .as_ref()?
                            .inline_max_bytes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .context_attachments
                            .get_or_insert_default()
                            .inline_max_bytes = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("16384"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "1 メッセージの添付の合計上限 (バイト)",
                description: "1 つのメッセージの添付の合計がこれを超えたら、大きいものから順にアウトラインや参照だけに減らして送ります。「全文で送る」にした添付は減らしません。4096 未満は 4096 として扱われます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.context_attachments.message_budget_bytes"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .context_attachments
                            .as_ref()?
                            .message_budget_bytes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .context_attachments
                            .get_or_insert_default()
                            .message_budget_bytes = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("65536"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アウトラインの上限 (バイト)",
                description: "大きいファイルを添付したときに送るアウトラインの大きさの上限です。収まらないときは深い階層のシンボルから省きます。512 未満は 512 として扱われます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.context_attachments.outline_max_bytes"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .context_attachments
                            .as_ref()?
                            .outline_max_bytes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .context_attachments
                            .get_or_insert_default()
                            .outline_max_bytes = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("8192"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Web ページの添付の上限 (バイト)",
                description: "@fetch で添付した Web ページの本文のうち、送る部分の上限です。1024 未満は 1024 として扱われます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.context_attachments.fetch_max_bytes"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .context_attachments
                            .as_ref()?
                            .fetch_max_bytes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .context_attachments
                            .get_or_insert_default()
                            .fetch_max_bytes = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("32768"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "診断を添付するファイル数の上限",
                description: "@diagnostics で添付する診断を、この数のファイルまでに絞ります。1 未満は 1 として扱われます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.context_attachments.diagnostics_max_files"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .context_attachments
                            .as_ref()?
                            .diagnostics_max_files
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .context_attachments
                            .get_or_insert_default()
                            .diagnostics_max_files = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("50"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "システムプロンプトに含めるリポジトリマップのバイト数",
                description: "システムプロンプトのプロジェクトのルートフォルダー一覧の直後に、リポジトリマップ (ファイルとそこで宣言されているシンボルの一覧) を差し込みます。0 で無効です。上限は 32768 バイトで、それより大きい値は 32768 として扱われます。プロジェクトの内容が変わったときだけ作り直します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.system_prompt_repo_map_bytes"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .system_prompt_repo_map_bytes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .system_prompt_repo_map_bytes = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("0"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "使わなくなったエージェントを止めるまでの分数",
                description: "外部エージェントのスレッドが 1 つも開かれていない状態がこの分数だけ続いたら、そのエージェントとの接続を終了します (エージェントとそのサンドボックスも止まります)。スレッドを開き直すと接続し直します。0 でウィンドウを閉じるまで接続を保ちます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.idle_disconnect_minutes"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .idle_disconnect_minutes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .idle_disconnect_minutes = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("5"),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]);

        items.into_boxed_slice()
    }

    fn edit_prediction_display_sub_section() -> [SettingsPageItem; 1] {
        [SettingsPageItem::SettingItem(SettingItem {
            title: "表示モード",
            description: "バッファーで編集予測のプレビューを表示するタイミングです。eager モードはインラインで表示し、subtle モードは修飾キーを押している間だけ表示します。",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("edit_prediction.display_mode"),
                pick: |settings_content| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .mode
                        .as_ref()
                },
                write: |settings_content, value, _| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .mode = value;
                },
            }),
            metadata: None,
            files: USER,
        })]
    }

    SettingsPage {
        title: "AI",
        items: concat_sections!(
            @vec,
            general_section(),
            agent_configuration_section(cx),
            edit_prediction_language_settings_section(),
            edit_prediction_display_sub_section(),
        )
        .into(),
    }
}

fn network_page() -> SettingsPage {
    fn network_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("ネットワーク"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プロキシ",
                description: "ネットワークリクエストに使うプロキシです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("proxy"),
                    pick: |settings_content| settings_content.proxy.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.proxy = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("socks5h://localhost:10808"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "サーバー URL",
                description: "接続先の Zed サーバーの URL です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("server_url"),
                    pick: |settings_content| settings_content.server_url.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.server_url = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("https://zed.dev"),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]
    }

    /// The egress allowlist is this fork's own (`docs/jp-enhance/DESIGN.md` §4), and
    /// it does not live in settings.json, so it gets a section of its own rather
    /// than sitting next to the proxy and server-URL settings.
    fn remote_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("リモート"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "リモートの通信をこの PC 経由にする",
                description: "オンにすると、SSH などのリモートホストが行う外部通信 (言語サーバーや node のダウンロード、拡張機能のアーカイブ、エージェントのバイナリ) をこの PC が代行します。インターネットに出られないリモートでも言語サーバーを自動取得できます。許可リストと同意ダイアログの対象になるのはエージェント関連の取得だけで、言語サーバーなど IDE の道具立ての取得は制限されません。オフにするとリモートが自分で外部に接続します。切り替えは再接続なしで反映されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("remote.relay_http_through_client"),
                    pick: |settings_content| {
                        settings_content
                            .remote_host
                            .as_ref()?
                            .relay_http_through_client
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .remote_host
                            .get_or_insert_default()
                            .relay_http_through_client = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    /// ADR-F39. On this page because what makes it worth having is the remote
    /// case: the tool is installed on the host, with the bytes coming through
    /// this PC's network.
    fn dev_tools_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("開発ツール"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "足りない開発ツールを知らせる",
                description: "オンにすると、開いたファイルの言語に必要な言語サーバーやフォーマッタ (C なら clangd と clang-format、Python なら Ruff) が見つからないときに通知を出し、その場で導入できます。導入先はユーザーのデータディレクトリ内だけで、管理者権限は要らず、他のユーザーにも影響しません。リモート接続中は接続先を調べて接続先に導入し、取得はこの PC のネットワーク経由で行うので、接続先がインターネットに出られなくても入ります。個別に知らせないツールは settings.json の `dev_tools.ignored` に ID を並べます (通知の「今後表示しない」でも追加されます)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("dev_tools.suggest_missing"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .dev_tools
                            .as_ref()?
                            .suggest_missing
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .dev_tools
                            .get_or_insert_default()
                            .suggest_missing = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn egress_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("外部通信の統制"),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: "ネットワーク許可リスト".into(),
                r#type: SubPageType::NetworkAllowlist,
                json_path: None,
                description: Some(
                    "AI が起点になる外部通信 (AI プロバイダー・エージェントのツール・MCP サーバー・外部エージェントのプロセス) を既定ですべて拒否する、このフォーク独自の機能です。言語サーバーや拡張機能など IDE 自身の道具立ての通信は対象外です。通信を許可するホストを確認・追加・削除します。"
                        .into(),
                ),
                search_aliases: &[
                    "allowed hosts",
                    "allowlist",
                    "egress",
                    "firewall",
                    "network allowlist",
                    "whitelist",
                ],
                in_json: false,
                files: USER,
                render: render_network_allowlist_page,
            }),
        ]
    }

    SettingsPage {
        title: "ネットワーク",
        items: concat_sections![
            network_section(),
            egress_section(),
            remote_section(),
            dev_tools_section()
        ],
    }
}

fn language_settings_field<T>(
    settings_content: &SettingsContent,
    get_language_setting_field: fn(&LanguageSettingsContent) -> Option<&T>,
) -> Option<&T> {
    let all_languages = &settings_content.project.all_languages;

    active_language()
        .and_then(|current_language_name| {
            all_languages
                .languages
                .0
                .get(current_language_name.as_ref())
        })
        .and_then(get_language_setting_field)
        .or_else(|| get_language_setting_field(&all_languages.defaults))
}

fn language_settings_field_mut<T>(
    settings_content: &mut SettingsContent,
    value: Option<T>,
    write: fn(&mut LanguageSettingsContent, Option<T>),
) {
    let all_languages = &mut settings_content.project.all_languages;
    let language_content = if let Some(current_language) = active_language() {
        all_languages
            .languages
            .0
            .entry(current_language.to_string())
            .or_default()
    } else {
        &mut all_languages.defaults
    };
    write(language_content, value);
}

fn language_settings_data() -> Box<[SettingsPageItem]> {
    fn indentation_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("インデント"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブのサイズ",
                description: "タブが占める列数です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).tab_size"), // TODO(cameron): not JQ syntax because not URL-safe
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.tab_size.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.tab_size = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブ文字を使用",
                description: "複数のスペースではなくタブ文字でインデントするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).hard_tabs"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.hard_tabs.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.hard_tabs = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "自動インデント",
                description: "入力時の自動インデントの動作を設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).auto_indent"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.auto_indent.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.auto_indent = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "貼り付け時の自動インデント",
                description: "貼り付けた内容のインデントを、文脈に応じて調整するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).auto_indent_on_paste"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.auto_indent_on_paste.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.auto_indent_on_paste = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn wrapping_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("折り返し"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "折り返し",
                description: "長い行をどのように折り返すかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).soft_wrap"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.soft_wrap.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.soft_wrap = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "折り返しガイドを表示",
                description: "エディターに折り返しガイドを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_wrap_guides"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_wrap_guides.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_wrap_guides = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "推奨の行の長さ",
                description: "折り返しが有効なバッファーで、行を折り返す列位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).preferred_line_length"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.preferred_line_length.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.preferred_line_length = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "折り返しガイド",
                description: "エディターで折り返しガイドを表示する文字数です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).wrap_guides"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.wrap_guides.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.wrap_guides = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "再折り返しを許可",
                description: "この言語で `editor::rewrap` アクションを許可する範囲を設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).allow_rewrap"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.allow_rewrap.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.allow_rewrap = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn indent_guides_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("インデントガイド"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "エディターにインデントガイドを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.enabled.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.indent_guides.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "線の太さ",
                description: "インデントガイドの太さ (ピクセル) です。1 から 10 の範囲で指定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.line_width"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.line_width.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.indent_guides.get_or_insert_default().line_width = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "アクティブな線の太さ",
                description: "アクティブなインデントガイドの太さ (ピクセル) です。1 から 10 の範囲で指定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.active_line_width"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.active_line_width.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .indent_guides
                                .get_or_insert_default()
                                .active_line_width = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "配色",
                description: "インデントガイドの配色方法を設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.coloring"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.coloring.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.indent_guides.get_or_insert_default().coloring = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "背景の配色",
                description: "インデントガイドの背景の配色方法を設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.background_coloring"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.indent_guides.as_ref().and_then(|indent_guides| {
                                indent_guides.background_coloring.as_ref()
                            })
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .indent_guides
                                .get_or_insert_default()
                                .background_coloring = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn formatting_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader("フォーマット"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "保存時にフォーマット",
                description: "On: バッファー全体をフォーマットします。\nOff: フォーマットしません。\nModifications: 未ステージの変更がある行だけをフォーマットします。git の差分または LSP の範囲フォーマットが使えない場合はフォーマットしません。\nModifications If Available: 同様ですが、使えない場合はバッファー全体のフォーマットにフォールバックします。",
                field: Box::new(
                    // TODO(settings_ui): this setting should just be a bool
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).format_on_save"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.format_on_save.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.format_on_save = value;
                                },
                            )
                        },
                    },
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "保存時に行末の空白を削除",
                description: "保存する前に、バッファーの各行から行末の空白を削除するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).remove_trailing_whitespace_on_save"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.remove_trailing_whitespace_on_save.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.remove_trailing_whitespace_on_save = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "保存時に末尾へ改行を確保",
                description: "保存時にバッファーの末尾に改行を 1 つだけ確保するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).ensure_final_newline_on_save"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.ensure_final_newline_on_save.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.ensure_final_newline_on_save = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "改行コード",
                description: "新規ファイル、およびフォーマットと保存の操作で、改行コードをどう扱うかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).line_ending"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.line_ending.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.line_ending = value;
                        })
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォーマッタ",
                description: "バッファーのフォーマット方法を設定します。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).formatter"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.formatter.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.formatter = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "入力時フォーマットを使用",
                description: "LSP サーバーの機能で定義された「トリガー」文字を入力するたびに、追加の LSP 問い合わせでコードを整形・修正するかどうかを設定します",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).use_on_type_format"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.use_on_type_format.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.use_on_type_format = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "フォーマット時のコードアクション",
                description: "フォーマット時に追加で実行するコードアクションです。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).code_actions_on_format"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.code_actions_on_format.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.code_actions_on_format = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn autoclose_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("自動閉じ"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "閉じ文字を自動的に挿入",
                description: "閉じ文字を自動的に入力するかどうかを設定します。たとえば '(' を入力すると、Zed が正しい位置に ')' を自動的に追加します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).use_autoclose"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.use_autoclose.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.use_autoclose = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "囲み文字を自動的に挿入",
                description: "テキストを自動的に文字で囲むかどうかを設定します。たとえばテキストを選択して '(' を入力すると、Zed がそのテキストを () で囲みます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).use_auto_surround"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.use_auto_surround.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.use_auto_surround = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "括弧を常に自動閉じとして扱う",
                description: "閉じ文字がどのように挿入されたかにかかわらず、常に読み飛ばして自動的に削除するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).always_treat_brackets_as_autoclosed"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.always_treat_brackets_as_autoclosed.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.always_treat_brackets_as_autoclosed = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "JSX タグの自動閉じ",
                description: "JSX のタグを自動的に閉じるかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).jsx_tag_auto_close"),
                    // TODO(settings_ui): this setting should just be a bool
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.jsx_tag_auto_close.as_ref()?.enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.jsx_tag_auto_close.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn whitespace_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("空白文字"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "空白文字を表示",
                description: "エディターにタブとスペースを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_whitespaces"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_whitespaces.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_whitespaces = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スペースの表示文字",
                description: "show_whitespaces を有効にしたときに、スペースの描画に使う文字です (既定: \"•\")",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).whitespace_map.space"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.whitespace_map.as_ref()?.space.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.whitespace_map.get_or_insert_default().space = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "タブの表示文字",
                description: "show_whitespaces を有効にしたときに、タブの描画に使う文字です (既定: \"→\")",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).whitespace_map.tab"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.whitespace_map.as_ref()?.tab.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.whitespace_map.get_or_insert_default().tab = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn completions_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader("補完候補"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "入力時に補完候補を表示",
                description: "明示的に要求しなくても、エディターでの入力中に補完メニューを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_completions_on_input"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_completions_on_input.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_completions_on_input = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "補完候補のドキュメントを表示",
                description: "補完メニューの候補について、インラインおよび隣接する領域にドキュメントを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_completion_documentation"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_completion_documentation.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_completion_documentation = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "単語",
                description: "単語をどのように補完するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.words"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.words.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.completions.get_or_insert_default().words = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "単語補完の最小文字数",
                description: "単語ベースの補完を自動的に表示するために必要な、補完クエリの文字数です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.words_min_length"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.words_min_length.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .completions
                                .get_or_insert_default()
                                .words_min_length = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "補完メニューのスクロールバー",
                description: "補完メニューにスクロールバーを表示するタイミングです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.completion_menu_scrollbar"),
                    pick: |settings_content| {
                        settings_content.editor.completion_menu_scrollbar.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.completion_menu_scrollbar = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "補完の詳細の配置",
                description: "コード補完のメニューで、詳細テキストを左右どちらに寄せるかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.completion_detail_alignment"),
                    pick: |settings_content| {
                        settings_content.editor.completion_detail_alignment.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.completion_detail_alignment = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "補完メニューの項目の種類",
                description: "補完メニューの各項目について、LSP の項目の種類 (関数・メソッド・変数など) をどのように表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.completion_menu_item_kind"),
                    pick: |settings_content| {
                        settings_content.editor.completion_menu_item_kind.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.completion_menu_item_kind = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn inlay_hints_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader("インレイヒント"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "ヒントの表示を切り替える全体スイッチです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.inlay_hints.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "値のヒントを表示",
                description: "デバッグ中のインライン値の表示を切り替える全体スイッチです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_value_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_value_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .show_value_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "型のヒントを表示",
                description: "型のヒントを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_type_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_type_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.inlay_hints.get_or_insert_default().show_type_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "引数のヒントを表示",
                description: "引数のヒントを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_parameter_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_parameter_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .show_parameter_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "その他のヒントを表示",
                description: "その他のヒントを表示するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_other_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_other_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .show_other_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "背景を表示",
                description: "インレイヒントに背景を表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_background"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_background.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.inlay_hints.get_or_insert_default().show_background = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "編集後のデバウンス (ミリ秒)",
                description: "バッファーの編集後にインレイヒントの更新をデバウンスするかどうかを設定します (0 でデバウンスを無効化)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.edit_debounce_ms"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.edit_debounce_ms.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .edit_debounce_ms = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "スクロール後のデバウンス (ミリ秒)",
                description: "バッファーのスクロール後にインレイヒントの更新をデバウンスするかどうかを設定します (0 でデバウンスを無効化)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.scroll_debounce_ms"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.scroll_debounce_ms.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .scroll_debounce_ms = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "修飾キーで切り替え",
                description: "指定した修飾キーを押したときに、インレイヒントの表示/非表示を切り替えます。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some(
                            "languages.$(language).inlay_hints.toggle_on_modifiers_press",
                        ),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language
                                    .inlay_hints
                                    .as_ref()?
                                    .toggle_on_modifiers_press
                                    .as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language
                                        .inlay_hints
                                        .get_or_insert_default()
                                        .toggle_on_modifiers_press = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn tasks_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("タスク"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "この言語でタスクを有効にするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).tasks.enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.tasks.as_ref()?.enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.tasks.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "変数",
                description: "特定の言語に対して設定する、追加のタスク変数です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).tasks.variables"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.tasks.as_ref()?.variables.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.tasks.get_or_insert_default().variables = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "LSP を優先",
                description: "Zed の言語拡張のタスクより、LSP のタスクを優先します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).tasks.prefer_lsp"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.tasks.as_ref()?.prefer_lsp.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.tasks.get_or_insert_default().prefer_lsp = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn miscellaneous_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader("その他"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "単語単位の差分を有効にする",
                description: "エディターで単語単位の差分ハイライトを有効にするかどうかを設定します。有効にすると、変更された行の中で実際に変わった単語がハイライトされます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).word_diff_enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.word_diff_enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.word_diff_enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "デバッガー",
                description: "この言語で優先的に使うデバッガーです。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).debuggers"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.debuggers.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.debuggers = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "中クリックで貼り付け",
                description: "Linux で中クリックによる貼り付けを有効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).editor.middle_click_paste"),
                    pick: |settings_content| settings_content.editor.middle_click_paste.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.middle_click_paste = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "改行時にコメントを継続",
                description: "直前の行がコメントの場合に、新しい行もコメントで始めるかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).extend_comment_on_newline"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.extend_comment_on_newline.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.extend_comment_on_newline = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "括弧の色分け",
                description: "エディターで括弧を色分けするかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).colorize_brackets"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.colorize_brackets.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.colorize_brackets = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Vim/Emacs の modeline のサポート",
                description: "modeline を探す行数です (0 で無効)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("modeline_lines"),
                    pick: |settings_content| settings_content.modeline_lines.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.modeline_lines = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn global_only_miscellaneous_sub_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SettingItem(SettingItem {
                title: "画像ビューアー",
                description: "画像ファイルのサイズの単位です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("image_viewer.unit"),
                    pick: |settings_content| {
                        settings_content
                            .image_viewer
                            .as_ref()
                            .and_then(|image_viewer| image_viewer.unit.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.image_viewer.get_or_insert_default().unit = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "Markdown プレビューの幅を制限",
                    description: "読みやすさのために、Markdown プレビューの内容を最大幅に制限し、ペインがそれより広いときは中央に寄せるかどうかを設定します。",
                    field: Box::new(SettingField::<bool> {
                        organization_override: None,
                        json_path: Some("markdown_preview.limit_content_width"),
                        pick: |settings_content| {
                            settings_content
                                .markdown_preview
                                .as_ref()?
                                .limit_content_width
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .markdown_preview
                                .get_or_insert_default()
                                .limit_content_width = value;
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    let enabled = settings_content
                        .markdown_preview
                        .as_ref()?
                        .limit_content_width
                        .unwrap_or(false);
                    Some(if enabled { 1 } else { 0 })
                },
                fields: vec![
                    vec![],
                    vec![SettingItem {
                        files: USER,
                        title: "最大幅",
                        description: "内容の最大幅 (ピクセル) です。ペインがこの値より広いときは内容を中央に寄せます。",
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("markdown_preview.max_width"),
                            pick: |settings_content| {
                                settings_content
                                    .markdown_preview
                                    .as_ref()?
                                    .max_width
                                    .as_ref()
                            },
                            write: |settings_content, value, _| {
                                settings_content
                                    .markdown_preview
                                    .get_or_insert_default()
                                    .max_width = value;
                            },
                        }),
                        metadata: None,
                    }],
                ],
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "改行をそのまま改行として表示",
                description: "オフにすると CommonMark 準拠になり、行末の半角スペース 2 つ / `\\` / `<br>` だけが改行になります。PDF の書き出しにも同じ設定が効きます。",
                field: Box::new(SettingField::<bool> {
                    organization_override: None,
                    json_path: Some("markdown_preview.soft_break_as_hard_break"),
                    pick: |settings_content| {
                        settings_content
                            .markdown_preview
                            .as_ref()?
                            .soft_break_as_hard_break
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .markdown_preview
                            .get_or_insert_default()
                            .soft_break_as_hard_break = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "CSV / TSV を表として開く",
                description: "オフにするとテキストエディターで開きます。どちらからでも横に切り替えられます。",
                field: Box::new(SettingField::<bool> {
                    organization_override: None,
                    json_path: Some("tabular_data.open_as_grid"),
                    pick: |settings_content| {
                        settings_content
                            .tabular_data
                            .as_ref()?
                            .open_as_grid
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabular_data
                            .get_or_insert_default()
                            .open_as_grid = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "複数行のセルをすべて表示",
                description: "表 (グリッド) で、改行を含むセルの行の高さを広げてすべて表示します。オフにすると 1 行に切り詰めます。",
                field: Box::new(SettingField::<bool> {
                    organization_override: None,
                    json_path: Some("tabular_data.multiline_cells"),
                    pick: |settings_content| {
                        settings_content
                            .tabular_data
                            .as_ref()?
                            .multiline_cells
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabular_data
                            .get_or_insert_default()
                            .multiline_cells = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ドロップ領域のサイズ",
                description: "ドロップしたファイルを分割ペインで開くための、エディター内のドロップ領域の相対サイズです。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("drop_target_size"),
                    pick: |settings_content| settings_content.workspace.drop_target_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.drop_target_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    let is_global = active_language().is_none();

    let code_lens_item = [SettingsPageItem::SettingItem(SettingItem {
        title: "コードレンズ",
        description: "言語サーバーのコードレンズを表示するかどうか、またどのように表示するかを設定します。",
        field: Box::new(SettingField {
            organization_override: None,
            json_path: Some("code_lens"),
            pick: |settings_content| settings_content.editor.code_lens.as_ref(),
            write: |settings_content, value, _| {
                settings_content.editor.code_lens = value;
            },
        }),
        metadata: None,
        files: USER,
    })];

    let lsp_document_colors_item = [SettingsPageItem::SettingItem(SettingItem {
        title: "LSP の色プレビュー",
        description: "エディターで LSP の色プレビューをどのように描画するかを設定します。",
        field: Box::new(SettingField {
            organization_override: None,
            json_path: Some("lsp_document_colors"),
            pick: |settings_content| settings_content.editor.lsp_document_colors.as_ref(),
            write: |settings_content, value, _| {
                settings_content.editor.lsp_document_colors = value;
            },
        }),
        metadata: None,
        files: USER,
    })];

    if is_global {
        concat_sections!(
            indentation_section(),
            wrapping_section(),
            indent_guides_section(),
            formatting_section(),
            autoclose_section(),
            whitespace_section(),
            completions_section(),
            inlay_hints_section(),
            code_lens_item,
            lsp_document_colors_item,
            tasks_section(),
            miscellaneous_section(),
            global_only_miscellaneous_sub_section(),
        )
    } else {
        concat_sections!(
            indentation_section(),
            wrapping_section(),
            indent_guides_section(),
            formatting_section(),
            autoclose_section(),
            whitespace_section(),
            completions_section(),
            inlay_hints_section(),
            code_lens_item,
            tasks_section(),
            miscellaneous_section(),
        )
    }
}

/// LanguageSettings items that should be included in the "Languages & Tools" page
/// not the "Editor" page
fn non_editor_language_settings_data() -> Box<[SettingsPageItem]> {
    fn lsp_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader("LSP"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "言語サーバーを有効にする",
                description: "コードインテリジェンスに言語サーバーを使うかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).enable_language_server"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.enable_language_server.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.enable_language_server = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "言語サーバー",
                description: "この言語で使用する (または無効にする) 言語サーバーの一覧です。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).language_servers"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.language_servers.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.language_servers = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "連動編集",
                description: "言語サーバーが対応している場合に、関連する範囲を連動して編集するかどうかを設定します。たとえば開始タグ <html> を編集すると、対応する終了タグ </html> の内容も一緒に編集されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).linked_edits"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.linked_edits.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.linked_edits = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "定義へ移動のフォールバック",
                description: "言語サーバーからの「定義へ移動」の結果が空だったときに、追加の問い合わせを行うかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("go_to_definition_fallback"),
                    pick: |settings_content| {
                        settings_content.editor.go_to_definition_fallback.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.go_to_definition_fallback = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "定義へ移動したときのスクロール方法",
                description: "定義や参照へ移動したときに、移動先をどのようにスクロールして表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("go_to_definition_scroll_strategy"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .go_to_definition_scroll_strategy
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.go_to_definition_scroll_strategy = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "LSP の結果の表示先",
                description: "複数の位置を含みうる LSP の結果 (定義へ移動・実装へ移動・すべての参照を検索) をどこに表示するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("lsp_results_location"),
                    pick: |settings_content| settings_content.editor.lsp_results_location.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.lsp_results_location = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "セマンティックトークン",
                description: {
                    static DESCRIPTION: OnceLock<&'static str> = OnceLock::new();
                    DESCRIPTION.get_or_init(|| {
                        SemanticTokens::VARIANTS
                            .iter()
                            .filter_map(|v| {
                                v.get_documentation().map(|doc| format!("{v:?}: {doc}"))
                            })
                            .join("\n")
                            .leak()
                    })
                },
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).semantic_tokens"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .semantic_tokens
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .semantic_tokens = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "LSP の折りたたみ範囲",
                description: "有効にすると、インデントに基づく折りたたみの代わりに、言語サーバーの折りたたみ範囲を使用します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).document_folding_ranges"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.document_folding_ranges.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.document_folding_ranges = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "LSP のドキュメントシンボル",
                description: "有効にすると、アウトラインとパンくずリストに tree-sitter ではなく、言語サーバーのドキュメントシンボルを使用します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).document_symbols"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.document_symbols.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.document_symbols = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn lsp_completions_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("LSP の補完"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "有効",
                description: "LSP の補完候補を取得するかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.lsp"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.lsp.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.completions.get_or_insert_default().lsp = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "取得のタイムアウト (ミリ秒)",
                description: "LSP の補完候補を取得するときに、個々のサーバーの応答をどれだけ待つかを設定します (0 で無期限に待機)。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.lsp_fetch_timeout_ms"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.lsp_fetch_timeout_ms.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .completions
                                .get_or_insert_default()
                                .lsp_fetch_timeout_ms = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "挿入方法",
                description: "LSP の補完候補をどのように挿入するかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.lsp_insert_mode"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.lsp_insert_mode.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.completions.get_or_insert_default().lsp_insert_mode = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn debugger_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("デバッガー"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "デバッガー",
                description: "この言語で優先的に使うデバッガーです。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).debuggers"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.debuggers.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.debuggers = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn prettier_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Prettier"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "使用を許可",
                description: "指定した言語で、Prettier によるフォーマットを有効または無効にします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).prettier.allowed"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.prettier.as_ref()?.allowed.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.prettier.get_or_insert_default().allowed = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "パーサー",
                description: "この言語のファイルをフォーマットするときに、Prettier 連携が使用するパーサー名を強制します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).prettier.parser"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.prettier.as_ref()?.parser.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.prettier.get_or_insert_default().parser = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "プラグイン",
                description: "この言語のファイルをフォーマットするときに、Prettier 連携が使用するプラグインを強制します。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).prettier.plugins"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.prettier.as_ref()?.plugins.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.prettier.get_or_insert_default().plugins = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "オプション",
                description: "Prettier の既定のオプションです。package.json の Prettier セクションと同じ形式で指定します。",
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).prettier.options"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.prettier.as_ref()?.options.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.prettier.get_or_insert_default().options = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    concat_sections!(
        lsp_section(),
        lsp_completions_section(),
        debugger_section(),
        prettier_section(),
    )
}

fn edit_prediction_language_settings_section() -> [SettingsPageItem; 5] {
    [
        SettingsPageItem::SectionHeader("編集予測"),
        SettingsPageItem::SubPageLink(SubPageLink {
            title: "プロバイダーの設定".into(),
            r#type: Default::default(),
            json_path: Some("edit_predictions.providers"),
            description: Some(
                "編集予測に使用するプロバイダーを選択します。Zed 組み込みの Zeta モデルに加えて、別の編集予測プロバイダーの認証情報や接続先もここで設定します。"
                    .into(),
            ),
            search_aliases: &[],
            in_json: false,
            files: USER,
            render: render_edit_prediction_setup_page,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "データの収集",
            description: "Zed の編集予測を使用するときに、Zed が学習データを収集してよいかどうかを設定します。データは、オープンソースと判定されたプロジェクトのファイルについてのみ収集されます。このフォークの既定値は「いいえ」です。「既定 (過去の切替に従う)」はステータスバーの切り替えで以前に設定した内容を使用し、設定がない場合は収集しません。",
            field: Box::new(SettingField {
                organization_override: Some(|org_settings| {
                    const DATA_COLLECTION_DISABLED: EditPredictionDataCollectionChoice =
                        EditPredictionDataCollectionChoice::No;

                    if !org_settings.edit_prediction.is_feedback_enabled {
                        Some(&DATA_COLLECTION_DISABLED)
                    } else {
                        None
                    }
                }),
                json_path: Some("edit_predictions.allow_data_collection"),
                pick: |settings_content| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .allow_data_collection
                        .as_ref()
                },
                write: |settings_content, value, _app| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .allow_data_collection = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "編集予測を表示",
            description: "編集予測をすぐに表示するか、手動で表示するかを設定します。",
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("languages.$(language).show_edit_predictions"),
                pick: |settings_content| {
                    language_settings_field(settings_content, |language| {
                        language.show_edit_predictions.as_ref()
                    })
                },
                write: |settings_content, value, _| {
                    language_settings_field_mut(settings_content, value, |language, value| {
                        language.show_edit_predictions = value;
                    })
                },
            }),
            metadata: None,
            files: USER | PROJECT,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "無効にする言語スコープ",
            description: "指定した言語スコープで編集予測を表示するかどうかを設定します。",
            field: Box::new(
                SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).edit_predictions_disabled_in"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.edit_predictions_disabled_in.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.edit_predictions_disabled_in = value;
                        })
                    },
                }
                .unimplemented(),
            ),
            metadata: None,
            files: USER | PROJECT,
        }),
    ]
}

fn show_scrollbar_or_editor(
    settings_content: &SettingsContent,
    show: fn(&SettingsContent) -> Option<&settings::ShowScrollbar>,
) -> Option<&settings::ShowScrollbar> {
    show(settings_content).or(settings_content
        .editor
        .scrollbar
        .as_ref()
        .and_then(|scrollbar| scrollbar.show.as_ref()))
}

fn dynamic_variants<T>() -> &'static [T::Discriminant]
where
    T: strum::IntoDiscriminant,
    T::Discriminant: strum::VariantArray,
{
    <<T as strum::IntoDiscriminant>::Discriminant as strum::VariantArray>::VARIANTS
}

/// Updates the `vim_mode` setting, disabling `helix_mode` if present and
/// `vim_mode` is being enabled.
fn write_vim_mode(settings: &mut SettingsContent, value: Option<bool>, _: &App) {
    write_vim_mode_inner(settings, value);
}

fn write_vim_mode_inner(settings: &mut SettingsContent, value: Option<bool>) {
    if value == Some(true) && settings.helix_mode == Some(true) {
        settings.helix_mode = Some(false);
    }
    settings.vim_mode = value;
}

/// Updates the `helix_mode` setting, disabling `vim_mode` if present and
/// `helix_mode` is being enabled.
fn write_helix_mode(settings: &mut SettingsContent, value: Option<bool>, _: &App) {
    write_helix_mode_inner(settings, value);
}

fn write_helix_mode_inner(settings: &mut SettingsContent, value: Option<bool>) {
    if value == Some(true) && settings.vim_mode == Some(true) {
        settings.vim_mode = Some(false);
    }
    settings.helix_mode = value;
}

/// 内蔵ブラウザー (ADR-F19)。ブックマークは設定ではなく `browser_bookmarks.json`
/// にあり、ブラウザーパネル内のブックマークペインで編集するのでここには出さない。
fn browser_page() -> SettingsPage {
    fn browsing_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("ブラウジング"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "起動時に開くページ",
                description: "内蔵ブラウザーのパネルやタブを開いたときに最初に表示するページです。ブラウザーのツールバーのピンボタンからも設定できます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.home_url"),
                    pick: |settings_content| settings_content.browser.as_ref()?.home_url.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.browser.get_or_insert_default().home_url = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("https://m365.cloud.microsoft/chat"),
                    display_confirm_button: true,
                    display_clear_button: true,
                    confirm_on_focus_out: true,
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "検索 URL",
                description: "アドレスバーの入力が URL でなかったときと、「選択テキストを Web で検索」で使う URL です。{query} が検索語に置き換わります。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.search_url"),
                    pick: |settings_content| settings_content.browser.as_ref()?.search_url.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.browser.get_or_insert_default().search_url = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("https://www.google.com/search?q={query}"),
                    display_confirm_button: true,
                    display_clear_button: true,
                    confirm_on_focus_out: true,
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Copilot URL",
                description: "「選択テキストを Copilot に聞く」で開く URL です。{query} を書くとそこに質問が埋め込まれます。プレースホルダーが無い場合は選択テキストをクリップボードにコピーします。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.copilot_url"),
                    pick: |settings_content| {
                        settings_content.browser.as_ref()?.copilot_url.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.browser.get_or_insert_default().copilot_url = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("https://github.com/copilot"),
                    display_confirm_button: true,
                    display_clear_button: true,
                    confirm_on_focus_out: true,
                    ..Default::default()
                })),
                files: USER,
            }),
        ]
    }

    fn panel_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("ブラウザーパネル"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブラウザーパネルのボタン",
                description: "ステータスバーにブラウザーパネルのボタンを表示します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.button"),
                    pick: |settings_content| settings_content.browser.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.browser.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブラウザーパネルのドック位置",
                description: "ブラウザーパネルをドッキングする位置です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.dock"),
                    pick: |settings_content| settings_content.browser.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.browser.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "起動時に開く",
                description: "起動時にブラウザーパネルを開くかどうかを設定します。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.starts_open"),
                    pick: |settings_content| {
                        settings_content.browser.as_ref()?.starts_open.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.browser.get_or_insert_default().starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブラウザーパネルの既定の幅",
                description: "ブラウザーパネルを左右にドッキングしたときの既定の幅 (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.default_width"),
                    pick: |settings_content| {
                        settings_content.browser.as_ref()?.default_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .browser
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "ブラウザーパネルの既定の高さ",
                description: "ブラウザーパネルを下にドッキングしたときの既定の高さ (ピクセル) です。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.default_height"),
                    pick: |settings_content| {
                        settings_content.browser.as_ref()?.default_height.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .browser
                            .get_or_insert_default()
                            .default_height = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn egress_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("外部通信"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "内蔵ブラウザーを許可リストの対象にする",
                description: "オンにすると内蔵ブラウザーの通信も外部通信の許可リストで制限されます。既定では人間操作のターミナルと同様に許可リストの対象外です。切り替えは内蔵ブラウザーの次回起動から反映されます。",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("browser.use_egress_proxy"),
                    pick: |settings_content| {
                        settings_content.browser.as_ref()?.use_egress_proxy.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .browser
                            .get_or_insert_default()
                            .use_egress_proxy = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "ブラウザー",
        items: concat_sections![browsing_section(), panel_section(), egress_section()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_vim_helix_mode() {
        // Enabling vim mode while `vim_mode` and `helix_mode` are not yet set
        // should only update the `vim_mode` setting.
        let mut settings = SettingsContent::default();
        write_vim_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.vim_mode, Some(true));
        assert_eq!(settings.helix_mode, None);

        // Enabling helix mode while `vim_mode` and `helix_mode` are not yet set
        // should only update the `helix_mode` setting.
        let mut settings = SettingsContent::default();
        write_helix_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.helix_mode, Some(true));
        assert_eq!(settings.vim_mode, None);

        // Disabling helix mode should only touch `helix_mode` setting when
        // `vim_mode` is not set.
        write_helix_mode_inner(&mut settings, Some(false));
        assert_eq!(settings.helix_mode, Some(false));
        assert_eq!(settings.vim_mode, None);

        // Enabling vim mode should update `vim_mode` but leave `helix_mode`
        // untouched.
        write_vim_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.vim_mode, Some(true));
        assert_eq!(settings.helix_mode, Some(false));

        // Enabling helix mode should update `helix_mode` and disable
        // `vim_mode`.
        write_helix_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.helix_mode, Some(true));
        assert_eq!(settings.vim_mode, Some(false));

        // Enabling vim mode should update `vim_mode` and disable
        // `helix_mode`.
        write_vim_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.vim_mode, Some(true));
        assert_eq!(settings.helix_mode, Some(false));
    }
}
