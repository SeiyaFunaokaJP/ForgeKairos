use gpui::{App, Menu, MenuItem, OsAction};
use release_channel::ReleaseChannel;
use terminal_view::terminal_panel;
use zed_actions::{Quit, assistant, debug_panel, dev, git_panel, project_panel};

pub fn app_menus(cx: &mut App) -> Vec<Menu> {
    let mut view_items = vec![
        MenuItem::action(
            "拡大",
            zed_actions::IncreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "縮小",
            zed_actions::DecreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "ズームをリセット",
            zed_actions::ResetBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "すべてのズームをリセット",
            zed_actions::ResetAllZoom { persist: false },
        ),
        MenuItem::separator(),
        MenuItem::action("左ドックの表示切り替え", workspace::ToggleLeftDock),
        MenuItem::action("右ドックの表示切り替え", workspace::ToggleRightDock),
        MenuItem::action("下ドックの表示切り替え", workspace::ToggleBottomDock),
        MenuItem::action("すべてのドックの表示切り替え", workspace::ToggleAllDocks),
        MenuItem::submenu(Menu {
            name: "エディターのレイアウト".into(),
            disabled: false,
            items: vec![
                MenuItem::action("上に分割", workspace::SplitUp::default()),
                MenuItem::action("下に分割", workspace::SplitDown::default()),
                MenuItem::action("左に分割", workspace::SplitLeft::default()),
                MenuItem::action("右に分割", workspace::SplitRight::default()),
            ],
        }),
        MenuItem::separator(),
        MenuItem::action("プロジェクトパネル", project_panel::ToggleFocus),
        MenuItem::action("アウトラインパネル", outline_panel::ToggleFocus),
        MenuItem::action("ターミナルパネル", terminal_panel::Toggle),
        MenuItem::action("デバッガーパネル", debug_panel::ToggleFocus),
        MenuItem::action("エージェントパネル", assistant::ToggleFocus),
        MenuItem::action("Git パネル", git_panel::ToggleFocus),
        MenuItem::separator(),
        MenuItem::action("診断", diagnostics::Deploy),
        MenuItem::separator(),
    ];

    if ReleaseChannel::try_global(cx) == Some(ReleaseChannel::Dev) {
        view_items.push(MenuItem::action(
            "GPUI インスペクターの表示切り替え",
            dev::ToggleInspector,
        ));
        view_items.push(MenuItem::separator());
    }

    let zed_menu = Menu {
        name: "Zed".into(),
        disabled: false,
        items: vec![
            MenuItem::action("Zed について", zed_actions::About),
            MenuItem::action("更新を確認", auto_update::Check),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new("設定").items([
                MenuItem::action("設定を開く", zed_actions::OpenSettings),
                MenuItem::action("設定ファイルを開く", super::OpenSettingsFile),
                MenuItem::action("プロジェクト設定を開く", zed_actions::OpenProjectSettings),
                MenuItem::action(
                    "プロジェクト設定ファイルを開く",
                    super::OpenProjectSettingsFile,
                ),
                MenuItem::action("既定の設定を開く", super::OpenDefaultSettings),
                MenuItem::separator(),
                MenuItem::action("キーマップを開く", zed_actions::OpenKeymap),
                MenuItem::action("キーマップファイルを開く", zed_actions::OpenKeymapFile),
                MenuItem::action("既定のキーバインドを開く", zed_actions::OpenDefaultKeymap),
                MenuItem::separator(),
                MenuItem::action(
                    "テーマを選択...",
                    zed_actions::theme_selector::Toggle::default(),
                ),
                MenuItem::action(
                    "アイコンテーマを選択...",
                    zed_actions::icon_theme_selector::Toggle::default(),
                ),
            ])),
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::os_submenu("サービス", gpui::SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("拡張機能", zed_actions::Extensions::default()),
            #[cfg(not(target_os = "windows"))]
            MenuItem::action("CLI をインストール", install_cli::InstallCliBinary),
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::action("Zed を非表示", super::Hide),
            #[cfg(target_os = "macos")]
            MenuItem::action("ほかを非表示", super::HideOthers),
            #[cfg(target_os = "macos")]
            MenuItem::action("すべて表示", super::ShowAll),
            MenuItem::separator(),
            MenuItem::action("Zed を終了", Quit),
        ],
    };

    let mut menus = vec![
        Menu {
            name: "ファイル".into(),
            disabled: false,
            items: vec![
                MenuItem::action("新規ファイル", workspace::NewFile),
                MenuItem::action("新規ウィンドウ", workspace::NewWindow),
                MenuItem::separator(),
                #[cfg(not(target_os = "macos"))]
                MenuItem::action("ファイルを開く...", workspace::OpenFiles),
                MenuItem::action(
                    if cfg!(not(target_os = "macos")) {
                        "フォルダーを開く..."
                    } else {
                        "開く…"
                    },
                    workspace::Open::default(),
                ),
                MenuItem::action(
                    "最近のプロジェクトを開く…",
                    zed_actions::OpenRecent::default(),
                ),
                MenuItem::action(
                    "リモートプロジェクトを開く…",
                    zed_actions::OpenRemote::default(),
                ),
                MenuItem::separator(),
                MenuItem::action(
                    "プロジェクトにフォルダーを追加…",
                    workspace::AddFolderToProject,
                ),
                MenuItem::separator(),
                MenuItem::action("保存", workspace::Save { save_intent: None }),
                MenuItem::action("名前を付けて保存…", workspace::SaveAs),
                MenuItem::action("すべて保存", workspace::SaveAll { save_intent: None }),
                MenuItem::separator(),
                MenuItem::action(
                    "エディターを閉じる",
                    workspace::CloseActiveItem {
                        save_intent: None,
                        close_pinned: true,
                    },
                ),
                MenuItem::action("プロジェクトを閉じる", workspace::CloseProject),
                MenuItem::action("ウィンドウを閉じる", workspace::CloseWindow),
            ],
        },
        Menu {
            name: "編集".into(),
            disabled: false,
            items: vec![
                MenuItem::os_action("元に戻す", editor::actions::Undo, OsAction::Undo),
                MenuItem::os_action("やり直し", editor::actions::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("切り取り", editor::actions::Cut, OsAction::Cut),
                MenuItem::os_action("コピー", editor::actions::Copy, OsAction::Copy),
                MenuItem::action("余白を削ってコピー", editor::actions::CopyAndTrim),
                MenuItem::os_action("貼り付け", editor::actions::Paste, OsAction::Paste),
                MenuItem::separator(),
                MenuItem::action("検索", search::buffer_search::Deploy::find()),
                MenuItem::action("プロジェクト内を検索", workspace::DeploySearch::default()),
                MenuItem::separator(),
                MenuItem::action(
                    "行コメントの切り替え",
                    editor::actions::ToggleComments::default(),
                ),
            ],
        },
        Menu {
            name: "選択".into(),
            disabled: false,
            items: vec![
                MenuItem::os_action(
                    "すべて選択",
                    editor::actions::SelectAll,
                    OsAction::SelectAll,
                ),
                MenuItem::action("選択範囲を拡張", editor::actions::SelectLargerSyntaxNode),
                MenuItem::action("選択範囲を縮小", editor::actions::SelectSmallerSyntaxNode),
                MenuItem::action(
                    "次の兄弟ノードを選択",
                    editor::actions::SelectNextSyntaxNode,
                ),
                MenuItem::action(
                    "前の兄弟ノードを選択",
                    editor::actions::SelectPreviousSyntaxNode,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    "上にカーソルを追加",
                    editor::actions::AddSelectionAbove {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    "下にカーソルを追加",
                    editor::actions::AddSelectionBelow {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    "次の出現箇所を選択",
                    editor::actions::SelectNext {
                        replace_newest: false,
                    },
                ),
                MenuItem::action(
                    "前の出現箇所を選択",
                    editor::actions::SelectPrevious {
                        replace_newest: false,
                    },
                ),
                MenuItem::action("すべての出現箇所を選択", editor::actions::SelectAllMatches),
                MenuItem::separator(),
                MenuItem::action("行を上へ移動", editor::actions::MoveLineUp),
                MenuItem::action("行を下へ移動", editor::actions::MoveLineDown),
                MenuItem::action("選択範囲を複製", editor::actions::DuplicateLineDown),
            ],
        },
        Menu {
            name: "表示".into(),
            disabled: false,
            items: view_items,
        },
        Menu {
            name: "移動".into(),
            disabled: false,
            items: vec![
                MenuItem::action("戻る", workspace::GoBack),
                MenuItem::action("進む", workspace::GoForward),
                MenuItem::separator(),
                MenuItem::action("コマンドパレット...", zed_actions::command_palette::Toggle),
                MenuItem::separator(),
                MenuItem::action("ファイルへ移動...", workspace::ToggleFileFinder::default()),
                // MenuItem::action("Go to Symbol in Project", project_symbols::Toggle),
                MenuItem::action(
                    "エディター内のシンボルへ移動...",
                    zed_actions::outline::ToggleOutline,
                ),
                MenuItem::action("行/列へ移動...", editor::actions::ToggleGoToLine),
                MenuItem::separator(),
                MenuItem::action("定義へ移動", editor::actions::GoToDefinition::default()),
                MenuItem::action("宣言へ移動", editor::actions::GoToDeclaration),
                MenuItem::action("型定義へ移動", editor::actions::GoToTypeDefinition),
                MenuItem::action(
                    "すべての参照を検索",
                    editor::actions::FindAllReferences::default(),
                ),
                MenuItem::separator(),
                MenuItem::action("次の問題へ", editor::actions::GoToDiagnostic::default()),
                MenuItem::action(
                    "前の問題へ",
                    editor::actions::GoToPreviousDiagnostic::default(),
                ),
            ],
        },
        Menu {
            name: "実行".into(),
            disabled: false,
            items: vec![
                MenuItem::action(
                    "タスクを実行",
                    zed_actions::Spawn::ViaModal {
                        reveal_target: None,
                    },
                ),
                MenuItem::action("デバッガーを開始", debugger_ui::Start),
                MenuItem::separator(),
                MenuItem::action("tasks.json を編集…", zed_actions::OpenProjectTasks),
                MenuItem::action("debug.json を編集…", zed_actions::OpenProjectDebugTasks),
                MenuItem::separator(),
                MenuItem::action("続行", debugger_ui::Continue),
                MenuItem::action("ステップオーバー", debugger_ui::StepOver),
                MenuItem::action("ステップイン", debugger_ui::StepInto),
                MenuItem::action("ステップアウト", debugger_ui::StepOut),
                MenuItem::separator(),
                MenuItem::action(
                    "ブレークポイントの切り替え",
                    editor::actions::ToggleBreakpoint,
                ),
                MenuItem::action("ブレークポイントを編集", editor::actions::EditLogBreakpoint),
                MenuItem::action(
                    "すべてのブレークポイントを削除",
                    debugger_ui::ClearAllBreakpoints,
                ),
            ],
        },
        Menu {
            // gpui_macos の `Platform::create_menu_bar` はこの名前で
            // NSApplication の windowsMenu を判定する (DEVLOG F1-3)
            name: "ウィンドウ".into(),
            disabled: false,
            items: vec![
                MenuItem::action("最小化", super::Minimize),
                MenuItem::action("ズーム", super::Zoom),
                MenuItem::separator(),
            ],
        },
        Menu {
            name: "ヘルプ".into(),
            disabled: false,
            items: vec![
                MenuItem::action(
                    "リリースノートをエディターで表示",
                    auto_update_ui::ViewReleaseNotesLocally,
                ),
                MenuItem::action("テレメトリログを表示", zed_actions::OpenTelemetryLog),
                MenuItem::action("依存ライブラリのライセンス", zed_actions::OpenLicenses),
                MenuItem::action("ようこそページを表示", onboarding::ShowWelcome),
                MenuItem::separator(),
                MenuItem::action("バグを報告...", zed_actions::feedback::FileBugReport),
                MenuItem::action("機能をリクエスト...", zed_actions::feedback::RequestFeature),
                MenuItem::action("メールを送る...", zed_actions::feedback::EmailZed),
                MenuItem::separator(),
                MenuItem::action(
                    "ドキュメント",
                    super::OpenBrowser {
                        url: "https://zed.dev/docs".into(),
                    },
                ),
                MenuItem::action("Zed のリポジトリ", feedback::OpenZedRepo),
                MenuItem::action(
                    "Zed の Twitter",
                    super::OpenBrowser {
                        url: "https://twitter.com/zeddotdev".into(),
                    },
                ),
                MenuItem::action(
                    "採用情報",
                    super::OpenBrowser {
                        url: "https://zed.dev/jobs".into(),
                    },
                ),
            ],
        },
    ];

    // macOS treats the first menu as the application menu — it is drawn bold
    // next to the Apple logo whatever it contains — so only Windows and Linux
    // honour the request to park "Zed" at the far right, after "ヘルプ".
    if cfg!(target_os = "macos") {
        menus.insert(0, zed_menu);
    } else {
        menus.push(zed_menu);
    }

    menus
}
