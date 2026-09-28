//! The settings table keeps upstream's English and is translated when drawn,
//! from `locales/ja/_settings.po`. These tests pin that the Japanese on screen is
//! the Japanese the table used to hold, that the English is complete, and that
//! search and page lookups work in both languages.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use gpui::{App, AppContext as _, BorrowAppContext as _, TestAppContext};
use heck::ToTitleCase as _;
use i18n::test_support::LocaleOverride;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::{
    DynamicItem, SettingItem, SettingsPage, SettingsPageItem, SettingsWindow, SubPageType,
    page_data, test::register_settings,
};

const SETTINGS_PO: &str = include_str!("../../../locales/ja/_settings.po");

/// What every table string displayed with the Japanese catalog before the table
/// went back to English (`page_data.rs` at a97e20b), keyed like [`table_strings`].
const JAPANESE_BEFORE: &str = include_str!("../test_data/settings_table_ja.tsv");

/// Table strings added after the table moved to the catalog, keyed like
/// [`table_strings`], with the Japanese they show.
const ADDED_AFTER_THE_MIGRATION: &[(&str, &str)] = &[
    ("p0.i1[ui_language].title", "表示言語"),
    (
        "p0.i1[ui_language].description",
        "Zed のメニュー、設定、メッセージを表示する言語です。タブ名など一部の表示は再起動後に切り替わります。",
    ),
    // Added by upstream v1.19.2. The folder indicators replace the folder icon
    // switches, whose rows left the table.
    (
        "p0.i10[reveal_if_open].title",
        "開いているファイルはそのペインで表示",
    ),
    (
        "p0.i10[reveal_if_open].description",
        "有効にすると、既に開いているバッファーを優先して表示します。",
    ),
    ("p3.i140[language_detection].title", "言語の自動検出"),
    (
        "p3.i140[language_detection].description",
        "保存されていないバッファーの言語を自動で検出するかどうかを設定します。",
    ),
    ("language.i59[language_detection].title", "言語の自動検出"),
    (
        "language.i59[language_detection].description",
        "保存されていないバッファーの言語を自動で検出するかどうかを設定します。",
    ),
    ("p5.i8[editor.search.search_on_type].title", "入力中に検索"),
    (
        "p5.i8[editor.search.search_on_type].description",
        "プロジェクト内検索で、Enter を押さなくても入力に合わせて検索を始めます。",
    ),
    (
        "p6.i61[close_panel_on_toggle].title",
        "切り替え時にパネルを閉じる",
    ),
    (
        "p6.i61[close_panel_on_toggle].description",
        "フォーカスのあるパネルで ToggleFocus アクションを実行したときに、フォーカスをエディターに戻す代わりに、パネルを閉じるかどうかを設定します。",
    ),
    (
        "p7.i6[project_panel.folder_indicator].title",
        "フォルダーの表示",
    ),
    (
        "p7.i6[project_panel.folder_indicator].description",
        "プロジェクトパネルのディレクトリに何を表示するかを設定します。",
    ),
    (
        "p7.i39[outline_panel.folder_indicator].title",
        "フォルダーの表示",
    ),
    (
        "p7.i39[outline_panel.folder_indicator].description",
        "アウトラインパネルのディレクトリに何を表示するかを設定します。",
    ),
    (
        "p7.i45[outline_panel.multi_buffer_hide_symbols].title",
        "マルチバッファーではシンボルを隠す",
    ),
    (
        "p7.i45[outline_panel.multi_buffer_hide_symbols].description",
        "マルチバッファーを表示しているときに、アウトラインパネルのシンボル・抜粋・検索結果を隠すかどうかを設定します。",
    ),
    (
        "p7.i58[git_panel.folder_indicator].title",
        "フォルダーの表示",
    ),
    (
        "p7.i58[git_panel.folder_indicator].description",
        "Git パネルのディレクトリに何を表示するかを設定します。",
    ),
];

/// Strings whose Japanese changed on purpose when the table moved to the catalog.
const CHANGED_ON_PURPOSE: &[(&str, &str)] = &[
    // The title said 音声のテスト and the button 音声をテスト for the same
    // English; GLOSSARY.md has Test Audio = 音声のテスト.
    ("Test Audio", "音声のテスト"),
];

fn settings_po() -> &'static i18n_catalog::PoFile {
    static FILE: OnceLock<i18n_catalog::PoFile> = OnceLock::new();
    FILE.get_or_init(|| {
        let (file, errors) = i18n_catalog::parse_po("_settings.po", SETTINGS_PO);
        assert!(errors.is_empty(), "_settings.po should parse: {errors:?}");
        file
    })
}

fn japanese() -> &'static dyn i18n::Catalog {
    i18n_catalog::test_support::bundled_catalog("ja")
}

/// Every string of the settings table that is shown on screen, keyed by where
/// it sits: page and item index, plus the JSON path when there is one.
fn table_strings(cx: &App) -> Vec<(String, String)> {
    fn setting_item(prefix: &str, item: &SettingItem, output: &mut Vec<(String, String)>) {
        let prefix = match item.field.json_path() {
            Some(json_path) => format!("{prefix}[{json_path}]"),
            None => prefix.to_string(),
        };
        output.push((format!("{prefix}.title"), item.title.to_string()));
        if !item.description.is_empty() {
            output.push((
                format!("{prefix}.description"),
                item.description.to_string(),
            ));
        }
        if let Some(placeholder) = item
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.placeholder)
        {
            output.push((format!("{prefix}.placeholder"), placeholder.to_string()));
        }
    }

    fn items(prefix: &str, items: &[SettingsPageItem], output: &mut Vec<(String, String)>) {
        for (item_index, item) in items.iter().enumerate() {
            let prefix = format!("{prefix}.i{item_index}");
            match item {
                SettingsPageItem::SectionHeader(header) => {
                    output.push((format!("{prefix}.header"), header.to_string()));
                }
                SettingsPageItem::SettingItem(item) => setting_item(&prefix, item, output),
                SettingsPageItem::DynamicItem(DynamicItem {
                    discriminant,
                    fields,
                    ..
                }) => {
                    setting_item(&prefix, discriminant, output);
                    for (variant_index, variant) in fields.iter().enumerate() {
                        for (field_index, field) in variant.iter().enumerate() {
                            setting_item(
                                &format!("{prefix}.v{variant_index}.f{field_index}"),
                                field,
                                output,
                            );
                        }
                    }
                }
                SettingsPageItem::SubPageLink(link) => {
                    // Language names come from the language registry.
                    if link.r#type == SubPageType::Language {
                        continue;
                    }
                    let prefix = match link.json_path {
                        Some(json_path) => format!("{prefix}[{json_path}]"),
                        None => prefix,
                    };
                    output.push((format!("{prefix}.link"), link.title.to_string()));
                    if let Some(description) = &link.description {
                        output.push((
                            format!("{prefix}.link_description"),
                            description.to_string(),
                        ));
                    }
                }
                SettingsPageItem::ActionLink(link) => {
                    output.push((format!("{prefix}.action"), link.title.to_string()));
                    if let Some(description) = &link.description {
                        output.push((
                            format!("{prefix}.action_description"),
                            description.to_string(),
                        ));
                    }
                    output.push((format!("{prefix}.button"), link.button_text.to_string()));
                }
            }
        }
    }

    let mut output = Vec::new();
    let pages: Vec<SettingsPage> = page_data::settings_data(cx);
    for (page_index, page) in pages.iter().enumerate() {
        let prefix = format!("p{page_index}");
        output.push((format!("{prefix}.title"), page.title.to_string()));
        items(&prefix, &page.items, &mut output);
    }
    items(
        "language",
        &page_data::language_sub_page_items(),
        &mut output,
    );
    output
}

fn table_strings_in_test_app(cx: &mut TestAppContext) -> Vec<(String, String)> {
    cx.update(|cx| {
        register_settings(cx);
        let app_state = AppState::test(cx);
        AppState::set_global(app_state, cx);
        table_strings(cx)
    })
}

fn escape_tsv(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape_tsv(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => output.push('\t'),
            Some('n') => output.push('\n'),
            Some(other) => output.push(other),
            None => output.push('\\'),
        }
    }
    output
}

fn japanese_before() -> BTreeMap<String, String> {
    JAPANESE_BEFORE
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (key, value) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("fixture line without a tab: {line:?}"));
            (key.to_string(), unescape_tsv(value))
        })
        .chain(
            ADDED_AFTER_THE_MIGRATION
                .iter()
                .map(|(key, japanese)| (key.to_string(), japanese.to_string())),
        )
        .collect()
}

fn has_japanese(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(character, '\u{3000}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
    })
}

/// Writes the table the way [`JAPANESE_BEFORE`] stores it; run with
/// `ZED_I18N_DUMP_SETTINGS_TABLE=<path>` against the old table to regenerate the
/// fixture.
#[gpui::test]
fn dump_table_strings_when_asked(cx: &mut TestAppContext) {
    let Some(path) = std::env::var_os("ZED_I18N_DUMP_SETTINGS_TABLE") else {
        return;
    };
    let lines: String = table_strings_in_test_app(cx)
        .into_iter()
        .map(|(key, text)| format!("{key}\t{}\n", escape_tsv(&text)))
        .collect();
    std::fs::write(&path, lines).expect("the dump should be writable");
}

#[test]
fn po_file_is_sorted_without_duplicates() {
    let file = settings_po();
    assert_eq!(file.header.language.as_deref(), Some("ja"));
    assert_eq!(
        file.header.plural_forms.as_deref(),
        Some("nplurals=1; plural=0;")
    );

    let keys: Vec<(Option<&str>, &str)> = file
        .entries
        .iter()
        .map(|entry| (entry.context.as_deref(), entry.msgid.as_str()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "entries are sorted by msgctxt, then msgid");
    let unique: HashSet<_> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        keys.len(),
        "no (msgctxt, msgid) appears twice"
    );
    assert!(
        file.entries.iter().all(|entry| entry.is_translated()),
        "every entry is translated"
    );
}

#[gpui::test]
fn table_strings_display_the_japanese_they_used_to_hold(cx: &mut TestAppContext) {
    let strings = table_strings_in_test_app(cx);
    let before = japanese_before();

    let keys: HashSet<&str> = strings.iter().map(|(key, _)| key.as_str()).collect();
    let before_keys: HashSet<&str> = before.keys().map(String::as_str).collect();
    let mut missing: Vec<_> = before_keys.difference(&keys).collect();
    let mut added: Vec<_> = keys.difference(&before_keys).collect();
    missing.sort();
    added.sort();
    assert!(
        missing.is_empty() && added.is_empty(),
        "the table's structure changed: missing {missing:?}, added {added:?}"
    );

    let _japanese = LocaleOverride::new(Some(japanese()));
    let mut mismatches = Vec::new();
    for (key, english) in &strings {
        let displayed = crate::tr_table(english);
        let expected = CHANGED_ON_PURPOSE
            .iter()
            .find(|(changed_english, _)| changed_english == english)
            .map(|(_, japanese)| (*japanese).to_string())
            .unwrap_or_else(|| before[key].clone());
        if displayed != expected {
            mismatches.push(format!(
                "{key}: {english:?} shows {displayed:?}, expected {expected:?}"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} strings differ:\n{}",
        mismatches.len(),
        strings.len(),
        mismatches.join("\n")
    );
}

#[gpui::test]
fn every_table_string_has_a_po_entry(cx: &mut TestAppContext) {
    let strings = table_strings_in_test_app(cx);
    let before = japanese_before();
    let msgids: HashSet<&str> = settings_po()
        .entries
        .iter()
        .filter(|entry| entry.context.as_deref() == Some(i18n::context::SETTINGS))
        .map(|entry| entry.msgid.as_str())
        .collect();
    let mut missing: Vec<&str> = strings
        .iter()
        // Placeholders are example values ("90%", URLs); they only need an
        // entry once someone translates them.
        .filter(|(key, english)| {
            !(key.ends_with(".placeholder") && before.get(key) == Some(english))
        })
        .map(|(_, english)| english.as_str())
        .filter(|english| !msgids.contains(english))
        .collect();
    missing.sort();
    missing.dedup();
    assert!(missing.is_empty(), "not in _settings.po: {missing:#?}");
}

#[gpui::test]
fn table_is_english_without_a_catalog(cx: &mut TestAppContext) {
    let strings = table_strings_in_test_app(cx);
    let _english = LocaleOverride::new(None);
    let japanese_left: Vec<_> = strings
        .iter()
        .filter(|(_, english)| has_japanese(english) || has_japanese(crate::tr_table(english)))
        .collect();
    assert!(japanese_left.is_empty(), "{japanese_left:#?}");
}

/// Display text of a dropdown label of the enum named `enum_name`, as
/// `EnumVariantDropdown` draws it.
fn dropdown_display(enum_name: &str, label: &str, title_case: bool) -> String {
    crate::components::display_label(enum_name, label, title_case)
}

/// The dropdowns whose labels the fork translated, with the labels they had
/// (the strum names at a97e20b) and whether the setting title-cases them.
fn translated_dropdowns() -> Vec<(
    &'static str,
    &'static [&'static str],
    bool,
    Vec<&'static str>,
)> {
    use strum::VariantNames as _;
    vec![
        (
            "PanelTabBar",
            settings::PanelTabBar::VARIANTS,
            true,
            vec!["自動", "常に表示", "表示しない"],
        ),
        (
            "StatusUntrackedFilesSetting",
            settings::StatusUntrackedFilesSetting::VARIANTS,
            true,
            vec!["すべて", "通常", "報告しない"],
        ),
        (
            "DefaultDockPanel",
            settings::DefaultDockPanel::VARIANTS,
            false,
            vec![
                "並べない",
                "エージェントパネル",
                "ブラウザーパネル",
                "ターミナルパネル",
                "プロジェクトパネル",
                "Git パネル",
                "アウトラインパネル",
                "デバッグパネル",
            ],
        ),
        (
            "EditPredictionDataCollectionChoice",
            settings::EditPredictionDataCollectionChoice::VARIANTS,
            true,
            vec!["既定 (過去の切替に従う)", "はい", "いいえ"],
        ),
        (
            "BaseKeymapContent",
            settings::BaseKeymapContent::VARIANTS,
            false,
            vec![
                "Zed",
                "VSCode",
                "JetBrains",
                "Sublime Text",
                "Atom",
                "TextMate",
                "Emacs (ベータ)",
                "Cursor (ベータ)",
                "なし",
            ],
        ),
    ]
}

#[test]
fn translated_dropdown_labels_show_the_japanese_they_used_to_have() {
    let _japanese = LocaleOverride::new(Some(japanese()));
    let mut mismatches = Vec::new();
    for (name, labels, title_case, before) in translated_dropdowns() {
        assert_eq!(labels.len(), before.len(), "{name} gained or lost a label");
        for (label, expected) in labels.iter().zip(before) {
            let displayed = dropdown_display(name, label, title_case);
            if displayed != expected {
                mismatches.push(format!(
                    "{name}::{label} shows {displayed:?}, expected {expected:?}"
                ));
            }
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    // The onboarding picker (`BaseKeymap::OPTIONS`) spells the beta keymaps
    // this way too.
    assert_eq!(
        dropdown_display("BaseKeymapContent", "Emacs", false),
        "Emacs (ベータ)"
    );
    // The same English reads differently where the enum gives it a context of
    // its own: "Cursor" is also the Editor page's section header.
    assert_eq!(crate::tr_table("Cursor"), "カーソル");
    assert_eq!(
        dropdown_display("BaseKeymapContent", "Cursor", false),
        "Cursor (ベータ)"
    );
}

#[test]
fn dropdown_labels_are_translated_in_the_context_of_their_enum() {
    use crate::components::enum_context;
    assert_eq!(
        enum_context::<settings::BaseKeymapContent>(),
        "BaseKeymapContent"
    );
    assert_eq!(enum_context::<settings::PanelTabBar>(), "PanelTabBar");
    assert_eq!(
        enum_context::<settings::EditPredictionDataCollectionChoice>(),
        "EditPredictionDataCollectionChoice"
    );
    for (name, _) in all_dropdowns() {
        assert!(!name.contains(':'), "{name}");
    }

    let _japanese = LocaleOverride::new(Some(japanese()));
    // Plain "Default" and "None" stay English in the enums that give them no
    // context of their own.
    assert_eq!(dropdown_display("ShowScrollbar", "Never", true), "Never");
    assert_eq!(dropdown_display("PanelTabBar", "Never", true), "表示しない");
    assert_eq!(dropdown_display("SoftWrap", "None", true), "None");
    assert_eq!(dropdown_display("BaseKeymapContent", "None", false), "なし");
}

macro_rules! dropdown_types {
    ($($type:ident),* $(,)?) => {{
        use strum::VariantNames as _;
        vec![$((stringify!($type), settings::$type::VARIANTS as &[&str])),*]
    }};
}

/// Every type `init_renderers` draws with `render_dropdown`.
fn all_dropdowns() -> Vec<(&'static str, &'static [&'static str])> {
    dropdown_types![
        ActivateOnClose,
        AlternateScroll,
        AutoIndentMode,
        AutosaveSettingDiscriminants,
        BaseKeymapContent,
        BottomDockLayout,
        BufferLineHeightDiscriminants,
        CliDefaultOpenBehavior,
        ClosePosition,
        CloseWindowWhenNoItems,
        CodeLens,
        CompletionDetailAlignment,
        CompletionMenuItemKind,
        CurrentLineHighlight,
        CursorShape,
        CursorShapeContent,
        DefaultDockPanel,
        DefaultOpenBehavior,
        DiagnosticSeverityContent,
        DiffViewStyle,
        DisplayIn,
        DockPosition,
        DockSide,
        DocumentColorsRenderMode,
        DocumentFoldingRanges,
        DocumentSymbols,
        DoubleClickInMultibuffer,
        EditPredictionDataCollectionChoice,
        EditPredictionPromptFormatContent,
        EditPredictionsMode,
        EncodingDisplayOptions,
        FolderIndicator,
        FormatOnSave,
        FullscreenMode,
        GitDiffBaseSetting,
        GitGutterSetting,
        GitGutterWidthDiscriminants,
        GitHunkStyleSetting,
        GitPanelClickBehavior,
        GitPanelGroupBy,
        GitPanelSortBy,
        GitPathStyle,
        GoToDefinitionFallback,
        GoToDefinitionScrollStrategy,
        HideMouseMode,
        IconThemeSelectionDiscriminants,
        ImageFileSizeUnit,
        IncludeIgnoredContent,
        IndentGuideBackgroundColoring,
        IndentGuideColoring,
        InlineBlameLocation,
        LineEndingSetting,
        LspInsertMode,
        MinimapThumb,
        MinimapThumbBorder,
        ModeContent,
        MultiCursorModifier,
        NotifyWhenAgentWaiting,
        OnLastWindowClosed,
        OpenResultsIn,
        PaneSplitDirectionHorizontal,
        PaneSplitDirectionVertical,
        PanelTabBar,
        PlaySoundWhenAgentDone,
        ProjectPanelEntrySpacing,
        ProjectPanelSortMode,
        ProjectPanelSortOrder,
        ReduceMotionMode,
        RelativeLineNumbers,
        RestoreOnStartupBehavior,
        RewrapBehavior,
        ScanSymlinksSetting,
        ScrollBeyondLastLine,
        ScrollbarDiagnostics,
        SeedQuerySetting,
        SemanticTokens,
        ShellDiscriminants,
        ShowCloseButton,
        ShowDiagnostics,
        ShowIndentGuides,
        ShowMinimap,
        ShowScrollbar,
        ShowWhitespaceSetting,
        SidebarDockPosition,
        SnippetSortOrder,
        SoftWrap,
        StatusStyle,
        StatusUntrackedFilesSetting,
        SteppingGranularity,
        TerminalBell,
        TerminalBlink,
        TerminalDockPosition,
        TextRenderingMode,
        ThemeAppearanceMode,
        ThemeSelectionDiscriminants,
        ThinkingBlockDisplay,
        UseSystemClipboard,
        VimInsertModeCursorShape,
        WindowButtonLayoutContentDiscriminants,
        WindowDecorations,
        WordsCompletionMode,
        WorkingDirectoryDiscriminants,
    ]
}

#[test]
fn dropdown_list_matches_init_renderers() {
    let source = include_str!("settings_ui.rs");
    let registered: HashSet<&str> = source
        .match_indices("(render_dropdown)")
        .filter_map(|(index, _)| {
            let before = &source[..index];
            let start = before.rfind("add_basic_renderer::<settings::")?;
            let name = &before[start + "add_basic_renderer::<settings::".len()..];
            name.strip_suffix('>')
        })
        .collect();
    let listed: HashSet<&str> = all_dropdowns().into_iter().map(|(name, _)| name).collect();
    assert_eq!(listed, registered);
}

/// Labels of upstream dropdowns whose Japanese changed on purpose: they were
/// shown in English before, as `(enum, English label, Japanese)`.
const DROPDOWN_LABELS_CHANGED_ON_PURPOSE: &[(&str, &str, &str)] = &[
    // "Selection" alone reads as the menu's 選択; the setting shows whitespace
    // in the selected text.
    ("ShowWhitespaceSetting", "Selection", "選択範囲"),
];

/// A new table string or dropdown label must not quietly translate a label that
/// is meant to stay English.
#[test]
fn untranslated_dropdown_labels_stay_english() {
    let translated: HashSet<&str> = translated_dropdowns()
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    let _japanese = LocaleOverride::new(Some(japanese()));
    let mut translated_by_accident = Vec::new();
    for (name, labels) in all_dropdowns() {
        if translated.contains(name) {
            continue;
        }
        for label in labels {
            for title_case in [false, true] {
                let english = if title_case {
                    label.to_title_case()
                } else {
                    label.to_string()
                };
                let displayed = dropdown_display(name, label, title_case);
                let expected = DROPDOWN_LABELS_CHANGED_ON_PURPOSE
                    .iter()
                    .find(|(changed_enum, changed, _)| *changed_enum == name && *changed == english)
                    .map_or(english.as_str(), |(_, _, japanese)| japanese);
                if displayed != expected {
                    translated_by_accident.push(format!("{name}::{label} shows {displayed:?}"));
                }
            }
        }
    }
    assert!(
        translated_by_accident.is_empty(),
        "{}",
        translated_by_accident.join("\n")
    );
}

async fn open_settings_window(
    cx: &mut TestAppContext,
    action: Box<dyn gpui::Action>,
) -> (
    gpui::WindowHandle<SettingsWindow>,
    &mut gpui::VisualTestContext,
) {
    use project::Project;

    cx.update(|cx| {
        register_settings(cx);
        release_channel::init("0.0.0".parse().expect("valid version"), cx);
        crate::init(cx);
    });
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        AppState::set_global(app_state.clone(), cx);
        app_state
    });
    app_state
        .fs
        .as_fake()
        .insert_tree("/project", serde_json::json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = cx.update(|cx| {
        Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            project::LocalProjectFlags::default(),
            cx,
        )
    });
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project", true, cx)
        })
        .await
        .expect("the worktree should be created");
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        let workspace = cx.new(|cx| {
            Workspace::new(
                Default::default(),
                project.clone(),
                app_state.clone(),
                window,
                cx,
            )
        });
        MultiWorkspace::new(workspace, window, cx)
    });
    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
    let settings_window = cx
        .update(|_, cx| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<SettingsWindow>())
        })
        .expect("the action should open the settings window");
    (settings_window, cx)
}

#[gpui::test]
async fn open_settings_page_finds_pages_by_their_english_name(cx: &mut TestAppContext) {
    let _japanese = LocaleOverride::new(Some(japanese()));
    let (settings_window, cx) = open_settings_window(
        cx,
        Box::new(zed_actions::OpenSettingsPage {
            page: "Network".into(),
            target: None,
        }),
    )
    .await;
    settings_window
        .read_with(cx, |settings_window, _| {
            assert_eq!(settings_window.current_page().title, "Network");
        })
        .expect("the settings window should be open");

    // `OpenSettingsPage` is registered on workspaces, so it is dispatched in
    // the workspace window; the page name is matched ignoring ASCII case.
    cx.update(|window, cx| {
        window.dispatch_action(
            Box::new(zed_actions::OpenSettingsPage {
                page: "general".into(),
                target: None,
            }),
            cx,
        )
    });
    cx.run_until_parked();
    settings_window
        .read_with(cx, |settings_window, _| {
            assert_eq!(settings_window.current_page().title, "General");
        })
        .expect("the settings window should be open");
}

fn item_position(settings_window: &SettingsWindow, json_path: &str) -> (usize, usize) {
    settings_window
        .pages
        .iter()
        .enumerate()
        .find_map(|(page_index, page)| {
            page.items
                .iter()
                .enumerate()
                .find_map(|(item_index, item)| {
                    let item_path = match item {
                        SettingsPageItem::SettingItem(item)
                        | SettingsPageItem::DynamicItem(DynamicItem {
                            discriminant: item, ..
                        }) => item.field.json_path(),
                        _ => None,
                    };
                    (item_path == Some(json_path)).then_some((page_index, item_index))
                })
        })
        .unwrap_or_else(|| panic!("no setting at {json_path}"))
}

fn search(
    settings_window: gpui::WindowHandle<SettingsWindow>,
    query: &str,
    cx: &mut gpui::VisualTestContext,
) -> HashMap<&'static str, bool> {
    settings_window
        .update(cx, |settings_window, window, cx| {
            settings_window
                .search_bar
                .update(cx, |editor, cx| editor.set_text(query, window, cx));
        })
        .expect("the settings window should be open");
    cx.run_until_parked();
    settings_window
        .read_with(cx, |settings_window, _| {
            ["accessible_mode", "vim_mode"]
                .into_iter()
                .map(|json_path| {
                    let (page_index, item_index) = item_position(settings_window, json_path);
                    (
                        json_path,
                        settings_window.filter_table[page_index][item_index],
                    )
                })
                .collect()
        })
        .expect("the settings window should be open")
}

#[gpui::test]
async fn search_finds_settings_by_english_and_japanese_names(cx: &mut TestAppContext) {
    let japanese_override = LocaleOverride::new(Some(japanese()));
    let (settings_window, cx) = open_settings_window(cx, Box::new(zed_actions::OpenSettings)).await;

    let found = search(settings_window, "accessible", cx);
    assert!(
        found["accessible_mode"],
        "English finds the setting: {found:?}"
    );
    assert!(!found["vim_mode"], "the search filters: {found:?}");

    let found = search(settings_window, "アクセシビリティ", cx);
    assert!(
        found["accessible_mode"],
        "Japanese finds the setting: {found:?}"
    );
    assert!(!found["vim_mode"], "the search filters: {found:?}");

    // Back to English. `LocaleOverride` does not advance the generation the way
    // `i18n::install` does, so mark the Japanese index as stale by hand; the
    // next frame should rebuild it, after which the Japanese name no longer
    // matches.
    settings_window
        .update(cx, |settings_window, _, _| {
            settings_window.build_search_index();
            let search_index = settings_window
                .search_index
                .as_mut()
                .and_then(std::sync::Arc::get_mut)
                .expect("a freshly built index is not shared");
            search_index.i18n_generation = i18n::generation().wrapping_add(1);
        })
        .expect("the settings window should be open");
    drop(japanese_override);
    let _english = LocaleOverride::new(None);
    settings_window
        .update(cx, |_, window, _| window.refresh())
        .expect("the settings window should be open");
    cx.run_until_parked();
    settings_window
        .read_with(cx, |settings_window, _| {
            assert_eq!(
                settings_window
                    .search_index
                    .as_ref()
                    .map(|search_index| search_index.i18n_generation),
                Some(i18n::generation()),
                "drawing the window rebuilt the index"
            );
        })
        .expect("the settings window should be open");

    let found = search(settings_window, "アクセシビリティ", cx);
    assert!(!found["accessible_mode"], "{found:?}");
    let found = search(settings_window, "accessible", cx);
    assert!(found["accessible_mode"], "{found:?}");
}

fn set_ui_language(language: &str, cx: &mut gpui::VisualTestContext) {
    cx.update(|_, cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.ui_language = Some(settings::UiLanguageName(language.to_string()));
            });
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn switching_the_ui_language_rebuilds_the_settings_window(cx: &mut TestAppContext) {
    let _installs = i18n::test_support::ThreadLocalInstalls::new();
    let (settings_window, cx) = open_settings_window(cx, Box::new(zed_actions::OpenSettings)).await;
    cx.update(|_, cx| i18n_settings::init_for_tests(None, cx));

    for (language, general_page, japanese_finds) in [
        ("ja", "全般", true),
        ("en", "General", false),
        ("ja", "全般", true),
    ] {
        set_ui_language(language, cx);
        assert_eq!(i18n::current_locale(), language);
        settings_window
            .read_with(cx, |settings_window, _| {
                assert_eq!(
                    settings_window
                        .search_index
                        .as_ref()
                        .map(|search_index| search_index.i18n_generation),
                    Some(i18n::generation()),
                    "the language switch redrew the window, which rebuilt the index"
                );
                let first_page = settings_window
                    .navbar_entries
                    .iter()
                    .find(|entry| entry.is_root && entry.page_index == 0)
                    .expect("the navigation lists the first page");
                assert_eq!(crate::tr_table(first_page.title), general_page);
            })
            .expect("the settings window should be open");
        let found = search(settings_window, "アクセシビリティ", cx);
        assert_eq!(
            found["accessible_mode"], japanese_finds,
            "{language}: {found:?}"
        );
    }
}

#[gpui::test]
fn every_setting_item_has_a_renderer(cx: &mut TestAppContext) {
    cx.update(|cx| {
        register_settings(cx);
        let app_state = AppState::test(cx);
        AppState::set_global(app_state, cx);
        crate::init_renderers(cx);
        let renderers = cx.default_global::<crate::SettingFieldRenderer>().clone();
        let renderers = renderers.renderers.borrow();
        let mut missing = Vec::new();
        let mut check = |item: &SettingItem| {
            if !renderers.contains_key(&crate::AnySettingField::type_id(item.field.as_ref())) {
                missing.push(format!(
                    "{} ({})",
                    item.title,
                    crate::AnySettingField::type_name(item.field.as_ref())
                ));
            }
        };
        for page in page_data::settings_data(cx) {
            for item in page.items.iter() {
                match item {
                    SettingsPageItem::SettingItem(item) => check(item),
                    SettingsPageItem::DynamicItem(DynamicItem {
                        discriminant,
                        fields,
                        ..
                    }) => {
                        check(discriminant);
                        fields.iter().flatten().for_each(&mut check);
                    }
                    _ => {}
                }
            }
        }
        assert!(missing.is_empty(), "NO RENDERER for {missing:#?}");
    });
}

#[gpui::test]
fn display_language_offers_the_system_english_and_every_translation(cx: &mut TestAppContext) {
    use crate::pages::ui_language::{current_label, language_choices};

    let _japanese = LocaleOverride::new(Some(japanese()));
    let choices = cx.update(|cx| language_choices(cx));
    let values: Vec<&str> = choices.iter().map(|choice| choice.value.as_ref()).collect();
    assert_eq!(values[..2], ["system", "en"]);
    assert!(values.contains(&"ja"), "{values:?}");
    assert_eq!(current_label(&choices, "system"), "システムの言語に従う");
    assert_eq!(current_label(&choices, ""), "システムの言語に従う");
    assert_eq!(current_label(&choices, "System"), "システムの言語に従う");
    assert_eq!(current_label(&choices, "en"), "English");
    assert_eq!(current_label(&choices, "ja"), "日本語");
    assert_eq!(current_label(&choices, "JA"), "日本語");
    assert_eq!(current_label(&choices, "xx-YY"), "xx-YY");

    drop(_japanese);
    let _english = LocaleOverride::new(None);
    let choices = cx.update(|cx| language_choices(cx));
    assert_eq!(
        current_label(&choices, "system"),
        "Follow the System Language"
    );
    assert_eq!(current_label(&choices, "ja"), "日本語");
}

#[gpui::test]
fn display_language_is_near_the_top_of_the_general_page(cx: &mut TestAppContext) {
    cx.update(|cx| {
        register_settings(cx);
        let app_state = AppState::test(cx);
        AppState::set_global(app_state, cx);
        let pages = page_data::settings_data(cx);
        let general = pages.first().expect("the General page comes first");
        assert_eq!(general.title, "General");
        let position = general
            .items
            .iter()
            .position(|item| {
                matches!(item, SettingsPageItem::SettingItem(item)
                    if item.field.json_path() == Some("ui_language"))
            })
            .expect("the General page has the display language");
        assert_eq!(position, 1, "right under the page's first header");
        let SettingsPageItem::SettingItem(item) = &general.items[position] else {
            unreachable!("matched above");
        };
        let _japanese = LocaleOverride::new(Some(japanese()));
        assert_eq!(crate::tr_table(item.title), "表示言語");
    });
}

/// Sub page titles that are data (a language, a provider's name) or already in
/// the UI language must not be translated as if they were the table's English.
#[test]
fn only_the_tables_own_sub_page_titles_are_translated() {
    fn link(title: &'static str, r#type: SubPageType) -> crate::SubPageLink {
        crate::SubPageLink {
            title: title.into(),
            r#type,
            description: None,
            search_aliases: &[],
            json_path: None,
            in_json: false,
            files: crate::USER,
            render: |_, _, _, _| gpui::IntoElement::into_any_element(gpui::Empty),
        }
    }

    let _japanese = LocaleOverride::new(Some(japanese()));
    for (title, r#type, kind, shown) in [
        ("Go", SubPageType::Language, "language", "Go"),
        ("Regex", SubPageType::Language, "language", "Regex"),
        ("Settings", SubPageType::Dynamic, "dynamic", "Settings"),
        ("Settings", SubPageType::Other, "table", "設定"),
        ("Skills", SubPageType::Other, "table", "スキル"),
    ] {
        assert_eq!(
            crate::shown_link_title(&link(title, r#type)),
            shown,
            "{title} ({kind})"
        );
    }
}
