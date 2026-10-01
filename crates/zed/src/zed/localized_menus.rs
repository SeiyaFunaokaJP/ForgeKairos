use gpui::{App, Global, Menu, MenuItem, OsMenu, SharedString};

use super::app_menus;

/// Translates the display names of application menus into the current UI language.
///
/// The menus are a static table written in English, so they are translated as a
/// whole right before they are handed to the platform instead of wrapping every
/// label in a macro. Actions, separators and flags are left untouched.
pub fn localize_menus(menus: Vec<Menu>) -> Vec<Menu> {
    menus.into_iter().map(localize_menu).collect()
}

fn localize_menu(menu: Menu) -> Menu {
    Menu {
        name: localize_name(menu.name),
        items: menu.items.into_iter().map(localize_item).collect(),
        disabled: menu.disabled,
    }
}

fn localize_item(item: MenuItem) -> MenuItem {
    match item {
        MenuItem::Separator => MenuItem::Separator,
        MenuItem::Submenu(menu) => MenuItem::Submenu(localize_menu(menu)),
        MenuItem::SystemMenu(os_menu) => MenuItem::SystemMenu(OsMenu {
            name: localize_name(os_menu.name),
            menu_type: os_menu.menu_type,
        }),
        MenuItem::Action {
            name,
            action,
            os_action,
            checked,
            disabled,
        } => MenuItem::Action {
            name: localize_name(name),
            action,
            os_action,
            checked,
            disabled,
        },
    }
}

fn localize_name(name: SharedString) -> SharedString {
    let translated = i18n::tr_ctx(i18n::context::MENU, &name);
    if translated == name.as_ref() {
        name
    } else {
        SharedString::from(translated.to_string())
    }
}

struct LocalizedMenusGeneration(u64);

impl Global for LocalizedMenusGeneration {}

/// Rebuilds the application menus in the current UI language and hands them to the
/// platform, unless they were already rebuilt since the catalog was last replaced.
///
/// Call this whenever the UI language may have changed. Returns whether the menus
/// were rebuilt.
pub fn refresh_localized_menus(cx: &mut App) -> bool {
    let generation = i18n::generation();
    if cx
        .try_global::<LocalizedMenusGeneration>()
        .is_some_and(|localized| localized.0 == generation)
    {
        return false;
    }
    let menus = app_menus(cx);
    cx.set_menus(localize_menus(menus));
    cx.set_global(LocalizedMenusGeneration(generation));
    true
}

/// Rebuilds the application menus in the new language whenever `ui_language` swaps the
/// catalog. The Windows and Linux title bars read the menus back from the platform.
pub fn refresh_localized_menus_on_language_change(cx: &mut App) {
    cx.observe_global::<i18n_settings::ActiveUiLanguage>(|cx| {
        refresh_localized_menus(cx);
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, OsAction, SystemMenuType, TestAppContext};
    use i18n::test_support::LocaleOverride;
    use release_channel::ReleaseChannel;
    use std::collections::{BTreeSet, HashMap};

    struct TestCatalog {
        translations: HashMap<String, &'static str>,
    }

    impl i18n::Catalog for TestCatalog {
        fn locale(&self) -> &str {
            "ja"
        }

        fn lookup(&self, context: Option<&str>, msgid: &str) -> Option<&'static str> {
            if context != Some(i18n::context::MENU) {
                return None;
            }
            self.translations.get(msgid).copied()
        }

        fn lookup_plural(
            &self,
            _context: Option<&str>,
            _msgid: &str,
            _msgid_plural: &str,
            _count: u64,
        ) -> Option<&'static str> {
            None
        }
    }

    fn leak_catalog(pairs: Vec<(String, String)>) -> &'static dyn i18n::Catalog {
        let translations = pairs
            .into_iter()
            .map(|(msgid, msgstr)| (msgid, &*Box::leak(msgstr.into_boxed_str())))
            .collect();
        Box::leak(Box::new(TestCatalog { translations }))
    }

    const MENUS_PO: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../locales/ja/_menus.po"
    ));

    fn menus_po_msgids() -> Vec<String> {
        let (file, errors) = i18n_catalog::parse_po("_menus.po", MENUS_PO);
        assert!(errors.is_empty(), "{errors:?}");
        file.entries.into_iter().map(|entry| entry.msgid).collect()
    }

    fn collect_labels(menus: &[Menu]) -> Vec<String> {
        fn collect_items(items: &[MenuItem], labels: &mut Vec<String>) {
            for item in items {
                match item {
                    MenuItem::Separator => {}
                    MenuItem::Submenu(menu) => {
                        labels.push(menu.name.to_string());
                        collect_items(&menu.items, labels);
                    }
                    MenuItem::SystemMenu(os_menu) => labels.push(os_menu.name.to_string()),
                    MenuItem::Action { name, .. } => labels.push(name.to_string()),
                }
            }
        }

        let mut labels = Vec::new();
        for menu in menus {
            labels.push(menu.name.to_string());
            collect_items(&menu.items, &mut labels);
        }
        labels
    }

    fn assert_same_shape(original: &[Menu], localized: &[Menu]) {
        fn assert_same_items(original: &[MenuItem], localized: &[MenuItem], path: &str) {
            assert_eq!(original.len(), localized.len(), "item count of {path}");
            for (index, (original, localized)) in original.iter().zip(localized).enumerate() {
                let path = format!("{path}/{index}");
                match (original, localized) {
                    (MenuItem::Separator, MenuItem::Separator) => {}
                    (MenuItem::Submenu(original), MenuItem::Submenu(localized)) => {
                        assert_eq!(original.disabled, localized.disabled, "{path}");
                        assert_same_items(&original.items, &localized.items, &path);
                    }
                    (MenuItem::SystemMenu(original), MenuItem::SystemMenu(localized)) => {
                        assert!(original.menu_type == localized.menu_type, "{path}");
                    }
                    (
                        MenuItem::Action {
                            action: original_action,
                            os_action: original_os_action,
                            checked: original_checked,
                            disabled: original_disabled,
                            ..
                        },
                        MenuItem::Action {
                            action: localized_action,
                            os_action: localized_os_action,
                            checked: localized_checked,
                            disabled: localized_disabled,
                            ..
                        },
                    ) => {
                        assert!(
                            original_action.partial_eq(localized_action.as_ref()),
                            "action of {path}: {} became {}",
                            original_action.name(),
                            localized_action.name()
                        );
                        assert!(original_os_action == localized_os_action, "{path}");
                        assert_eq!(original_checked, localized_checked, "{path}");
                        assert_eq!(original_disabled, localized_disabled, "{path}");
                    }
                    _ => panic!("the kind of item {path} changed"),
                }
            }
        }

        assert_eq!(original.len(), localized.len(), "menu count");
        for (index, (original, localized)) in original.iter().zip(localized).enumerate() {
            assert_eq!(original.disabled, localized.disabled, "menu {index}");
            assert_same_items(&original.items, &localized.items, &index.to_string());
        }
    }

    fn build_app_menus(dev_channel: bool, cx: &mut TestAppContext) -> Vec<Menu> {
        cx.update(|cx| {
            let channel = if dev_channel {
                ReleaseChannel::Dev
            } else {
                ReleaseChannel::Stable
            };
            release_channel::init_test(semver::Version::new(0, 0, 0), channel, cx);
            init_menu_settings(cx);
            app_menus(cx)
        })
    }

    /// `app_menus` reads `DisableAiSettings` (upstream since v1.20) and
    /// `AgentSettings`, so the menus can only be built once the settings are registered.
    fn init_menu_settings(cx: &mut App) {
        if !cx.has_global::<settings::SettingsStore>() {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            <project::DisableAiSettings as settings::Settings>::register(cx);
            <agent_settings::AgentSettings as settings::Settings>::register(cx);
        }
    }

    fn sample_menus() -> Vec<Menu> {
        vec![
            Menu::new("File").items([
                MenuItem::action("New", workspace::NewFile),
                MenuItem::separator(),
                MenuItem::submenu(
                    Menu::new("Recent")
                        .items([
                            MenuItem::action("About Zed", zed_actions::About).checked(true),
                            MenuItem::submenu(Menu::new("Nested").items([
                                MenuItem::action("Innermost", workspace::NewWindow).disabled(true),
                            ])),
                        ])
                        .disabled(true),
                ),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
            ]),
            Menu::new("Edit").items([
                MenuItem::os_action("Copy", editor::actions::Copy, OsAction::Copy),
                MenuItem::action("Untranslated", editor::actions::Paste),
            ]),
        ]
    }

    #[test]
    fn test_localize_menus_changes_nothing_without_a_catalog() {
        let _english = LocaleOverride::new(None);
        let localized = localize_menus(sample_menus());
        assert_eq!(collect_labels(&localized), collect_labels(&sample_menus()));
        assert_same_shape(&sample_menus(), &localized);
    }

    #[gpui::test]
    fn test_localize_menus_leaves_app_menus_english_without_a_catalog(cx: &mut TestAppContext) {
        let _english = LocaleOverride::new(None);
        let original = build_app_menus(false, cx);
        let localized = localize_menus(build_app_menus(false, cx));
        assert_eq!(collect_labels(&localized), collect_labels(&original));
        assert_same_shape(&original, &localized);
    }

    #[test]
    fn test_localize_menus_translates_every_level() {
        let catalog = leak_catalog(
            [
                ("File", "ファイル"),
                ("New", "新規"),
                ("Recent", "最近"),
                ("About Zed", "Zed について"),
                ("Nested", "入れ子"),
                ("Innermost", "最奥"),
                ("Services", "サービス"),
                ("Edit", "編集"),
                ("Copy", "コピー"),
            ]
            .into_iter()
            .map(|(msgid, msgstr)| (msgid.to_string(), msgstr.to_string()))
            .collect(),
        );
        let _japanese = LocaleOverride::new(Some(catalog));

        let localized = localize_menus(sample_menus());

        assert_eq!(
            collect_labels(&localized),
            [
                "ファイル",
                "新規",
                "最近",
                "Zed について",
                "入れ子",
                "最奥",
                "サービス",
                "編集",
                "コピー",
                "Untranslated",
            ]
        );
        assert_same_shape(&sample_menus(), &localized);
    }

    #[gpui::test]
    fn test_every_app_menu_label_is_in_menus_po(cx: &mut TestAppContext) {
        let msgids = menus_po_msgids();
        let unique_msgids = msgids.iter().collect::<BTreeSet<_>>();
        assert_eq!(
            unique_msgids.len(),
            msgids.len(),
            "duplicate msgid in _menus.po"
        );

        for dev_channel in [false, true] {
            let missing = collect_labels(&build_app_menus(dev_channel, cx))
                .into_iter()
                .filter(|label| !msgids.contains(label))
                .collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "menu labels missing from locales/ja/_menus.po (dev channel: {dev_channel}): {missing:?}"
            );
        }
    }

    #[gpui::test]
    fn test_translated_app_menus_match_the_japanese_menus_before_the_po_migration(
        cx: &mut TestAppContext,
    ) {
        let _japanese =
            LocaleOverride::new(Some(i18n_catalog::test_support::bundled_catalog("ja")));

        for dev_channel in [false, true] {
            let actual = collect_labels(&localize_menus(build_app_menus(dev_channel, cx)))
                .into_iter()
                .collect::<BTreeSet<_>>();
            let expected = labels_before_po_migration(dev_channel)
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>();
            pretty_assertions::assert_eq!(actual, expected, "dev channel: {dev_channel}");
        }
    }

    /// Label by label, not only as a set: each English label shows the Japanese that
    /// stood in its place in `app_menus.rs` before the migration.
    #[gpui::test]
    fn test_each_app_menu_label_shows_the_japanese_it_replaced(cx: &mut TestAppContext) {
        let _japanese =
            LocaleOverride::new(Some(i18n_catalog::test_support::bundled_catalog("ja")));
        let before: HashMap<&str, &str> = MENU_LABELS_BEFORE_PO_MIGRATION.iter().copied().collect();

        for dev_channel in [false, true] {
            let english = collect_labels(&build_app_menus(dev_channel, cx));
            let shown = collect_labels(&localize_menus(build_app_menus(dev_channel, cx)));
            assert_eq!(english.len(), shown.len());
            let mismatches = english
                .iter()
                .zip(&shown)
                .filter_map(|(english, shown)| {
                    let expected = before.get(english.as_str()).copied();
                    (expected != Some(shown.as_str()))
                        .then(|| format!("{english:?} shows {shown:?}, expected {expected:?}"))
                })
                .collect::<Vec<_>>();
            assert!(
                mismatches.is_empty(),
                "dev channel {dev_channel}:
{}",
                mismatches.join(
                    "
"
                )
            );
        }
    }

    #[gpui::test]
    fn test_refresh_localized_menus_rebuilds_only_after_the_catalog_changes(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            init_menu_settings(cx);
            assert!(refresh_localized_menus(cx));
            assert!(!refresh_localized_menus(cx));

            cx.set_global(LocalizedMenusGeneration(i18n::generation().wrapping_add(1)));
            assert!(refresh_localized_menus(cx));
            assert!(!refresh_localized_menus(cx));
        });
    }

    fn set_ui_language(language: &str, cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.ui_language = Some(settings::UiLanguageName(language.to_string()));
                });
            });
        });
        cx.run_until_parked();
    }

    fn platform_menu_names(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| cx.get_menus())
            .expect("the menus were set")
            .into_iter()
            .map(|menu| menu.name.to_string())
            .collect()
    }

    #[gpui::test]
    async fn test_switching_the_ui_language_retranslates_the_menus(cx: &mut TestAppContext) {
        let _installs = i18n::test_support::ThreadLocalInstalls::new();
        let app_state = super::super::tests::init_test(cx);
        cx.update(|cx| {
            title_bar::init(cx);
            i18n_settings::init_for_tests(None, cx);
            refresh_localized_menus_on_language_change(cx);
            refresh_localized_menus(cx);
        });
        cx.update(|cx| {
            workspace::open_new(Default::default(), app_state.clone(), cx, |_, _, _| {})
        })
        .await
        .expect("the workspace window opens");
        cx.run_until_parked();
        let multi_workspace = cx
            .update(|cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<workspace::MultiWorkspace>())
            })
            .expect("a workspace window is open");

        for (language, file_menu, help_menu) in [
            ("ja", "ファイル", "ヘルプ"),
            ("en", "File", "Help"),
            ("ja", "ファイル", "ヘルプ"),
        ] {
            set_ui_language(language, cx);
            assert_eq!(i18n::current_locale(), language);

            let names = platform_menu_names(cx);
            assert!(
                names.iter().any(|name| name == file_menu)
                    && names.iter().any(|name| name == help_menu),
                "platform menus in {language}: {names:?}"
            );

            multi_workspace
                .update(cx, |_, window, _| window.refresh())
                .expect("the window is open");
            cx.run_until_parked();
            let title_bar_names = multi_workspace
                .update(cx, |multi_workspace, _, cx| {
                    multi_workspace
                        .workspace()
                        .read(cx)
                        .titlebar_item()
                        .and_then(|item| item.downcast::<title_bar::TitleBar>().ok())
                        .map(|title_bar| title_bar.read(cx).application_menu_names(cx))
                })
                .expect("the window is open")
                .expect("the workspace has a title bar");
            if cfg!(target_os = "macos") {
                assert_eq!(title_bar_names, None, "macOS draws the menus itself");
            } else {
                let title_bar_names = title_bar_names
                    .expect("Windows and Linux draw the menu bar")
                    .into_iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>();
                assert_eq!(title_bar_names, names, "title bar menus in {language}");
            }
        }
    }

    /// `(English, Japanese)` for every label of `app_menus`: the string literals of
    /// `app_menus.rs` at a97e20b (Japanese) and after the migration (English),
    /// paired by their order in the file, which the migration left unchanged.
    const MENU_LABELS_BEFORE_PO_MIGRATION: &[(&str, &str)] = &[
        ("About Zed", "Zed について"),
        ("Add Cursor Above", "上にカーソルを追加"),
        ("Add Cursor Below", "下にカーソルを追加"),
        ("Add Folder to Project…", "プロジェクトにフォルダーを追加…"),
        ("Agent Panel", "エージェントパネル"),
        ("Amend Last Commit", "直前のコミットを修正 (amend)"),
        ("Back", "戻る"),
        ("Browser Panel", "ブラウザーパネル"),
        // Added by zed-jp-enhance with the call graph panel.
        ("Call Graph Panel", "呼び出しグラフパネル"),
        ("Change Folder…", "フォルダーを変更…"),
        ("Check for Updates", "更新を確認"),
        ("Clear All Breakpoints", "すべてのブレークポイントを削除"),
        ("Close Editor", "エディターを閉じる"),
        ("Close Project", "プロジェクトを閉じる"),
        ("Close Window", "ウィンドウを閉じる"),
        ("Command Palette...", "コマンドパレット..."),
        ("Compare Two Branches…", "2 つのブランチを比較…"),
        ("Continue", "続行"),
        ("Copy", "コピー"),
        ("Copy and Trim", "余白を削ってコピー"),
        ("Create Branch…", "ブランチを作成…"),
        ("Create Merge Request", "MR を作る"),
        ("Cut", "切り取り"),
        ("Debugger Panel", "デバッガーパネル"),
        ("Diagnostics", "診断"),
        ("Documentation", "ドキュメント"),
        ("Duplicate Selection", "選択範囲を複製"),
        ("Edit", "編集"),
        ("Edit Breakpoint", "ブレークポイントを編集"),
        ("Edit debug.json…", "debug.json を編集…"),
        ("Edit tasks.json…", "tasks.json を編集…"),
        ("Editor Layout", "エディターのレイアウト"),
        ("Email Us...", "メールを送る..."),
        ("Expand Selection", "選択範囲を拡張"),
        ("Extensions", "拡張機能"),
        ("Fetch", "フェッチ"),
        ("File", "ファイル"),
        ("File Bug Report...", "バグを報告..."),
        ("Find", "検索"),
        ("Find All References", "すべての参照を検索"),
        ("Find in Project", "プロジェクト内を検索"),
        (
            "Fixup into an Earlier Commit…",
            "今の変更を昔のコミットに入れる (fixup)…",
        ),
        ("Force Push with Lease…", "強制プッシュ (lease 付き)…"),
        ("Forward", "進む"),
        ("Git", "Git"),
        ("Git Guide", "Git の操作ガイド"),
        ("Git Panel", "Git パネル"),
        ("Go", "移動"),
        ("Go to Declaration", "宣言へ移動"),
        ("Go to Definition", "定義へ移動"),
        ("Go to File...", "ファイルへ移動..."),
        ("Go to Line/Column...", "行/列へ移動..."),
        (
            "Go to Symbol in Editor...",
            "エディター内のシンボルへ移動...",
        ),
        ("Go to Type Definition", "型定義へ移動"),
        ("Help", "ヘルプ"),
        ("Hide Others", "ほかを非表示"),
        ("Hide Zed", "Zed を非表示"),
        ("Install CLI", "CLI をインストール"),
        ("Join the Team", "採用情報"),
        ("Minimize", "最小化"),
        ("Move Line Down", "行を下へ移動"),
        ("Move Line Up", "行を上へ移動"),
        ("New", "新規ファイル"),
        ("New Browser Tab", "新しいブラウザータブ"),
        ("New Window", "新規ウィンドウ"),
        ("Next Problem", "次の問題へ"),
        ("Open Default Key Bindings", "既定のキーバインドを開く"),
        ("Open Default Settings", "既定の設定を開く"),
        ("Open File...", "ファイルを開く..."),
        ("Open Folder...", "フォルダーを開く..."),
        ("Open Git Graph", "コミットグラフを開く"),
        ("Open Keymap", "キーマップを開く"),
        ("Open Keymap File", "キーマップファイルを開く"),
        ("Open Project Settings", "プロジェクト設定を開く"),
        (
            "Open Project Settings File",
            "プロジェクト設定ファイルを開く",
        ),
        ("Open Recent…", "最近のプロジェクトを開く…"),
        ("Open Remote…", "リモートプロジェクトを開く…"),
        ("Open Settings", "設定を開く"),
        ("Open Settings File", "設定ファイルを開く"),
        ("Open…", "開く…"),
        ("Outline Panel", "アウトラインパネル"),
        ("Paste", "貼り付け"),
        ("Previous Problem", "前の問題へ"),
        ("Project Panel", "プロジェクトパネル"),
        ("Pull", "プル"),
        ("Push", "プッシュ"),
        ("Quit Zed", "Zed を終了"),
        ("Redo", "やり直し"),
        ("Request Feature...", "機能をリクエスト..."),
        ("Reset All Zoom", "すべてのズームをリセット"),
        ("Reset Zoom", "ズームをリセット"),
        ("Run", "実行"),
        ("Save", "保存"),
        ("Save All", "すべて保存"),
        ("Save As…", "名前を付けて保存…"),
        ("Select All", "すべて選択"),
        ("Select All Occurrences", "すべての出現箇所を選択"),
        ("Select Icon Theme...", "アイコンテーマを選択..."),
        ("Select Next Occurrence", "次の出現箇所を選択"),
        ("Select Next Sibling", "次の兄弟ノードを選択"),
        ("Select Previous Occurrence", "前の出現箇所を選択"),
        ("Select Previous Sibling", "前の兄弟ノードを選択"),
        ("Select Theme...", "テーマを選択..."),
        ("Selection", "選択"),
        ("Services", "サービス"),
        ("Settings", "設定"),
        ("Show All", "すべて表示"),
        // Added by upstream v1.19.2.
        ("Show Incoming Calls", "呼び出し元を表示"),
        ("Show Outgoing Calls", "呼び出し先を表示"),
        // Added by zed-jp-enhance with the call graph panel.
        (
            "Show Callees in Call Graph",
            "呼び出しグラフで呼び出し先を表示",
        ),
        (
            "Show Callers in Call Graph",
            "呼び出しグラフで呼び出し元を表示",
        ),
        ("Show Welcome", "ようこそページを表示"),
        ("Shrink Selection", "選択範囲を縮小"),
        ("Spawn Task", "タスクを実行"),
        ("Split Down", "下に分割"),
        ("Split Left", "左に分割"),
        ("Split Right", "右に分割"),
        ("Split Up", "上に分割"),
        ("Start Debugger", "デバッガーを開始"),
        ("Stash…", "stash…"),
        ("Step Into", "ステップイン"),
        ("Step Out", "ステップアウト"),
        ("Step Over", "ステップオーバー"),
        ("Switch Branch…", "ブランチを切り替え…"),
        ("Terminal Panel", "ターミナルパネル"),
        ("Toggle All Docks", "すべてのドックの表示切り替え"),
        ("Toggle Bottom Dock", "下ドックの表示切り替え"),
        ("Toggle Breakpoint", "ブレークポイントの切り替え"),
        ("Toggle GPUI Inspector", "GPUI インスペクターの表示切り替え"),
        ("Toggle Left Dock", "左ドックの表示切り替え"),
        ("Toggle Line Comment", "行コメントの切り替え"),
        ("Toggle Right Dock", "右ドックの表示切り替え"),
        ("Undo", "元に戻す"),
        ("View", "表示"),
        ("View Dependency Licenses", "依存ライブラリのライセンス"),
        (
            "View Release Notes Locally",
            "リリースノートをエディターで表示",
        ),
        ("View Telemetry", "テレメトリログを表示"),
        ("Window", "ウィンドウ"),
        ("Zed", "Zed"),
        ("Zed Repository", "Zed のリポジトリ"),
        ("Zed Twitter", "Zed の Twitter"),
        ("Zoom", "ズーム"),
        ("Zoom In", "拡大"),
        ("Zoom Out", "縮小"),
    ];

    /// The Japanese labels `app_menus` showed at commit a97e20b, when they were still
    /// written directly in the source, extracted from that revision by a script.
    fn labels_before_po_migration(dev_channel: bool) -> Vec<&'static str> {
        let mut labels = SHOWN_EVERYWHERE.to_vec();
        if cfg!(target_os = "macos") {
            labels.extend_from_slice(SHOWN_ON_MACOS);
        } else {
            labels.extend_from_slice(SHOWN_EXCEPT_ON_MACOS);
        }
        if !cfg!(target_os = "windows") {
            labels.extend_from_slice(SHOWN_EXCEPT_ON_WINDOWS);
        }
        if dev_channel {
            labels.extend_from_slice(SHOWN_ON_DEV_CHANNEL);
        }
        labels
    }

    const SHOWN_EVERYWHERE: &[&str] = &[
        "拡大",
        "縮小",
        "ズームをリセット",
        "すべてのズームをリセット",
        "左ドックの表示切り替え",
        "右ドックの表示切り替え",
        "下ドックの表示切り替え",
        "すべてのドックの表示切り替え",
        "エディターのレイアウト",
        "上に分割",
        "下に分割",
        "左に分割",
        "右に分割",
        "プロジェクトパネル",
        "アウトラインパネル",
        "呼び出しグラフパネル",
        "ターミナルパネル",
        "デバッガーパネル",
        "エージェントパネル",
        "Git パネル",
        "ブラウザーパネル",
        "新しいブラウザータブ",
        "診断",
        "Zed",
        "Zed について",
        "更新を確認",
        "設定",
        "設定を開く",
        "設定ファイルを開く",
        "プロジェクト設定を開く",
        "プロジェクト設定ファイルを開く",
        "既定の設定を開く",
        "キーマップを開く",
        "キーマップファイルを開く",
        "既定のキーバインドを開く",
        "テーマを選択...",
        "アイコンテーマを選択...",
        "拡張機能",
        "Zed を終了",
        "ファイル",
        "新規ファイル",
        "新規ウィンドウ",
        "最近のプロジェクトを開く…",
        "リモートプロジェクトを開く…",
        "プロジェクトにフォルダーを追加…",
        "フォルダーを変更…",
        "保存",
        "名前を付けて保存…",
        "すべて保存",
        "エディターを閉じる",
        "プロジェクトを閉じる",
        "ウィンドウを閉じる",
        "編集",
        "元に戻す",
        "やり直し",
        "切り取り",
        "コピー",
        "余白を削ってコピー",
        "貼り付け",
        "検索",
        "プロジェクト内を検索",
        "行コメントの切り替え",
        "選択",
        "すべて選択",
        "選択範囲を拡張",
        "選択範囲を縮小",
        "次の兄弟ノードを選択",
        "前の兄弟ノードを選択",
        "上にカーソルを追加",
        "下にカーソルを追加",
        "次の出現箇所を選択",
        "前の出現箇所を選択",
        "すべての出現箇所を選択",
        "行を上へ移動",
        "行を下へ移動",
        "選択範囲を複製",
        "表示",
        "移動",
        "戻る",
        "進む",
        "コマンドパレット...",
        "ファイルへ移動...",
        "エディター内のシンボルへ移動...",
        "行/列へ移動...",
        "定義へ移動",
        "宣言へ移動",
        "型定義へ移動",
        "すべての参照を検索",
        "呼び出し元を表示",
        "呼び出し先を表示",
        "呼び出しグラフで呼び出し元を表示",
        "呼び出しグラフで呼び出し先を表示",
        "次の問題へ",
        "前の問題へ",
        "実行",
        "タスクを実行",
        "デバッガーを開始",
        "tasks.json を編集…",
        "debug.json を編集…",
        "続行",
        "ステップオーバー",
        "ステップイン",
        "ステップアウト",
        "ブレークポイントの切り替え",
        "ブレークポイントを編集",
        "すべてのブレークポイントを削除",
        "ウィンドウ",
        "最小化",
        "ズーム",
        "ヘルプ",
        "リリースノートをエディターで表示",
        "テレメトリログを表示",
        "依存ライブラリのライセンス",
        "ようこそページを表示",
        "バグを報告...",
        "機能をリクエスト...",
        "メールを送る...",
        "ドキュメント",
        "Zed のリポジトリ",
        "Zed の Twitter",
        "採用情報",
        "Git",
        "コミットグラフを開く",
        "ブランチを作成…",
        "ブランチを切り替え…",
        "直前のコミットを修正 (amend)",
        "今の変更を昔のコミットに入れる (fixup)…",
        "MR を作る",
        "2 つのブランチを比較…",
        "フェッチ",
        "プル",
        "プッシュ",
        "強制プッシュ (lease 付き)…",
        "stash…",
        "Git の操作ガイド",
    ];

    const SHOWN_ON_DEV_CHANNEL: &[&str] = &["GPUI インスペクターの表示切り替え"];

    const SHOWN_ON_MACOS: &[&str] = &[
        "サービス",
        "Zed を非表示",
        "ほかを非表示",
        "すべて表示",
        "開く…",
    ];

    const SHOWN_EXCEPT_ON_WINDOWS: &[&str] = &["CLI をインストール"];

    const SHOWN_EXCEPT_ON_MACOS: &[&str] = &["ファイルを開く...", "フォルダーを開く..."];
}
