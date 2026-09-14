//! DESIGN.md §3 (ADR-F02) 方式 2: コマンドパレットのラベルは
//! [`crate::humanize_action_name`] による自動生成でリテラルが存在しないため、
//! アクション名 → 日本語ラベルの静的対応表を humanize の前段に挟む。
//! 未登録のアクションは従来どおり英語の humanize 出力にフォールバックする。
//! 訳語の規約は `docs/jp-enhance/GLOSSARY.md`。

use std::collections::HashMap;
use std::sync::LazyLock;

static LABELS: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| LABEL_TABLE.iter().copied().collect());

/// Japanese label registered for an action name such as `"editor::GoToDefinition"`.
pub fn label_for_action(action_name: &str) -> Option<&'static str> {
    LABELS.get(action_name).copied()
}

/// 対応表に載っているアクション名。`crates/zed` のテストが実在確認に使う (F1-3)
pub fn labeled_action_names() -> impl Iterator<Item = &'static str> {
    LABEL_TABLE.iter().map(|(action_name, _)| *action_name)
}

const LABEL_TABLE: &[(&str, &str)] = &[
    // zed
    ("zed::About", "Zed について"),
    ("zed::Quit", "Zed を終了"),
    ("zed::Extensions", "拡張機能"),
    ("zed::ShowWelcome", "ようこそページを開く"),
    ("zed::OpenOnboarding", "初期設定を開く"),
    ("zed::OpenSettings", "環境設定: 設定を開く"),
    ("zed::OpenSettingsFile", "環境設定: settings.json を開く"),
    ("zed::OpenSettingsPage", "環境設定: 設定ページを開く"),
    ("zed::OpenDefaultSettings", "環境設定: 既定の設定を開く"),
    (
        "zed::OpenProjectSettings",
        "環境設定: プロジェクト設定を開く",
    ),
    (
        "zed::OpenProjectSettingsFile",
        "環境設定: プロジェクトの settings.json を開く",
    ),
    ("zed::OpenServerSettings", "環境設定: サーバー設定を開く"),
    ("zed::OpenAccountSettings", "環境設定: アカウント設定を開く"),
    ("zed::OpenKeymap", "環境設定: キーマップを開く"),
    ("zed::OpenKeymapFile", "環境設定: keymap.json を開く"),
    ("zed::OpenDefaultKeymap", "環境設定: 既定のキーマップを開く"),
    (
        "zed::ToggleBaseKeymapSelector",
        "環境設定: ベースキーマップを選択",
    ),
    ("zed::ChangeKeybinding", "環境設定: キーバインドを変更"),
    ("zed::ImportVsCodeSettings", "VS Code の設定をインポート"),
    ("zed::ImportCursorSettings", "Cursor の設定をインポート"),
    ("zed::OpenTasks", "タスク: tasks.json を開く"),
    (
        "zed::OpenProjectTasks",
        "タスク: プロジェクトのタスクを開く",
    ),
    ("zed::OpenDebugTasks", "タスク: デバッグタスクを開く"),
    ("zed::OpenLog", "ログを開く"),
    ("zed::OpenTelemetryLog", "テレメトリログを開く"),
    (
        "zed::RevealLogInFileManager",
        "ログをファイルマネージャーで表示",
    ),
    ("zed::OpenLicenses", "ライセンスを開く"),
    ("zed::OpenDocs", "ドキュメントを開く"),
    ("zed::OpenZedRepo", "Zed のリポジトリを開く"),
    ("zed::OpenStatusPage", "ステータスページを開く"),
    (
        "zed::CopySystemSpecsIntoClipboard",
        "システム情報をクリップボードにコピー",
    ),
    (
        "zed::CopyInstalledExtensionsIntoClipboard",
        "インストール済み拡張機能をクリップボードにコピー",
    ),
    ("zed::InstallDevExtension", "開発中の拡張機能をインストール"),
    ("zed::ReloadExtensions", "拡張機能を再読み込み"),
    (
        "zed::OpenPerformanceProfiler",
        "パフォーマンスプロファイラーを開く",
    ),
    (
        "zed::IncreaseBufferFontSize",
        "エディター: フォントサイズを拡大",
    ),
    (
        "zed::DecreaseBufferFontSize",
        "エディター: フォントサイズを縮小",
    ),
    (
        "zed::ResetBufferFontSize",
        "エディター: フォントサイズをリセット",
    ),
    ("zed::IncreaseUiFontSize", "表示: UI フォントサイズを拡大"),
    ("zed::DecreaseUiFontSize", "表示: UI フォントサイズを縮小"),
    ("zed::ResetUiFontSize", "表示: UI フォントサイズをリセット"),
    ("zed::ResetAllZoom", "表示: すべてのズームをリセット"),
    ("zed::ToggleFullScreen", "表示: 全画面表示の切り替え"),
    ("zed::Zoom", "ウィンドウ: ズーム"),
    ("zed::Minimize", "ウィンドウ: 最小化"),
    ("zed::Hide", "ウィンドウ: 非表示"),
    ("zed::HideOthers", "ウィンドウ: ほかを非表示"),
    ("zed::ShowAll", "ウィンドウ: すべて表示"),
    // workspace
    (
        "workspace::Open",
        "ファイル: フォルダーまたはファイルを開く",
    ),
    ("workspace::OpenFiles", "ファイル: ファイルを開く"),
    (
        "workspace::AddFolderToProject",
        "プロジェクトにフォルダーを追加",
    ),
    ("workspace::ChangeProjectFolder", "フォルダーを変更"),
    ("workspace::Save", "ファイル: 保存"),
    ("workspace::SaveAs", "ファイル: 名前を付けて保存"),
    ("workspace::SaveAll", "ファイル: すべて保存"),
    (
        "workspace::SaveWithoutFormat",
        "ファイル: フォーマットせずに保存",
    ),
    ("workspace::FormatAndSave", "ファイル: フォーマットして保存"),
    ("workspace::NewFile", "ファイル: 新規ファイル"),
    (
        "workspace::NewFileSplitHorizontal",
        "ファイル: 新規ファイル (水平分割)",
    ),
    (
        "workspace::NewFileSplitVertical",
        "ファイル: 新規ファイル (垂直分割)",
    ),
    (
        "workspace::ReloadActiveItem",
        "ファイル: このファイルを再読み込み",
    ),
    ("workspace::OpenWithSystem", "ファイル: 既定のアプリで開く"),
    (
        "workspace::ToggleReadOnlyFile",
        "ファイル: 読み取り専用の切り替え",
    ),
    ("workspace::CopyPath", "パスをコピー"),
    ("workspace::CopyRelativePath", "相対パスをコピー"),
    ("workspace::CloseProject", "プロジェクトを閉じる"),
    ("workspace::NewWindow", "ウィンドウ: 新規ウィンドウ"),
    ("workspace::CloseWindow", "ウィンドウ: ウィンドウを閉じる"),
    (
        "workspace::ActivateNextWindow",
        "ウィンドウ: 次のウィンドウ",
    ),
    (
        "workspace::ActivatePreviousWindow",
        "ウィンドウ: 前のウィンドウ",
    ),
    ("workspace::Reload", "ウィンドウ: 再読み込み"),
    ("workspace::NewTerminal", "ターミナル: 新規ターミナル"),
    (
        "workspace::NewCenterTerminal",
        "ターミナル: 中央ペインに新規ターミナル",
    ),
    ("workspace::OpenTerminal", "ターミナル: ターミナルを開く"),
    (
        "workspace::OpenInTerminal",
        "ターミナル: ここでターミナルを開く",
    ),
    ("workspace::ToggleLeftDock", "表示: 左ドックの表示切り替え"),
    ("workspace::ToggleRightDock", "表示: 右ドックの表示切り替え"),
    (
        "workspace::ToggleBottomDock",
        "表示: 下ドックの表示切り替え",
    ),
    (
        "workspace::ToggleAllDocks",
        "表示: すべてのドックの表示切り替え",
    ),
    ("workspace::CloseAllDocks", "表示: すべてのドックを閉じる"),
    (
        "workspace::CloseActiveDock",
        "表示: アクティブなドックを閉じる",
    ),
    ("workspace::IncreaseActiveDockSize", "表示: ドックを広げる"),
    ("workspace::DecreaseActiveDockSize", "表示: ドックを狭める"),
    (
        "workspace::ResetActiveDockSize",
        "表示: ドックのサイズをリセット",
    ),
    (
        "workspace::MoveFocusedPanelToNextPosition",
        "表示: パネルを次の位置へ移動",
    ),
    (
        "workspace::ToggleCenteredLayout",
        "表示: 中央寄せレイアウトの切り替え",
    ),
    (
        "workspace::UseAgenticLayout",
        "表示: エージェントレイアウト",
    ),
    ("workspace::UseClassicLayout", "表示: クラシックレイアウト"),
    ("workspace::ToggleZoom", "表示: パネルのズーム切り替え"),
    (
        "workspace::ToggleEditorZoom",
        "表示: エディターのズーム切り替え",
    ),
    ("workspace::ZoomIn", "表示: 拡大"),
    ("workspace::ZoomOut", "表示: 縮小"),
    ("workspace::ActivateNextPane", "ペイン: 次のペインへ"),
    ("workspace::ActivatePreviousPane", "ペイン: 前のペインへ"),
    ("workspace::ActivateLastPane", "ペイン: 最後のペインへ"),
    ("workspace::ActivatePaneLeft", "ペイン: 左のペインへ"),
    ("workspace::ActivatePaneRight", "ペイン: 右のペインへ"),
    ("workspace::ActivatePaneUp", "ペイン: 上のペインへ"),
    ("workspace::ActivatePaneDown", "ペイン: 下のペインへ"),
    (
        "workspace::FocusCenterPane",
        "ペイン: 中央ペインにフォーカス",
    ),
    ("workspace::SwapPaneLeft", "ペイン: 左のペインと入れ替え"),
    ("workspace::SwapPaneRight", "ペイン: 右のペインと入れ替え"),
    ("workspace::SwapPaneUp", "ペイン: 上のペインと入れ替え"),
    ("workspace::SwapPaneDown", "ペイン: 下のペインと入れ替え"),
    ("workspace::MovePaneLeft", "ペイン: 左へ移動"),
    ("workspace::MovePaneRight", "ペイン: 右へ移動"),
    ("workspace::MovePaneUp", "ペイン: 上へ移動"),
    ("workspace::MovePaneDown", "ペイン: 下へ移動"),
    ("workspace::ResetPaneSizes", "ペイン: サイズをリセット"),
    (
        "workspace::CloseAllItemsAndPanes",
        "タブ: すべてのタブとペインを閉じる",
    ),
    (
        "workspace::CloseInactiveTabsAndPanes",
        "タブ: 非アクティブなタブとペインを閉じる",
    ),
    (
        "workspace::CloseItemInAllPanes",
        "タブ: すべてのペインでこの項目を閉じる",
    ),
    ("workspace::NewSearch", "検索: プロジェクト内検索"),
    ("workspace::ClearNavigationHistory", "移動: 履歴を消去"),
    ("workspace::ClearBookmarks", "ブックマーク: すべて消去"),
    ("workspace::ClearAllNotifications", "通知をすべて消去"),
    (
        "workspace::SuppressNotification",
        "この通知を今後表示しない",
    ),
    ("workspace::ToggleVimMode", "Vim モードの切り替え"),
    ("workspace::ToggleHelixMode", "Helix モードの切り替え"),
    ("workspace::ToggleEditPrediction", "編集予測の切り替え"),
    ("workspace::Feedback", "フィードバックを送る"),
    ("workspace::ReopenLastPicker", "最後のピッカーを再表示"),
    (
        "workspace::ClearTrustedWorktrees",
        "信頼済みワークツリーを消去",
    ),
    (
        "workspace::ToggleWorktreeSecurity",
        "ワークツリーの信頼設定を切り替え",
    ),
    (
        "workspace::ShutdownDebugAdapters",
        "デバッグ: デバッグアダプターを終了",
    ),
    (
        "multi_workspace::ToggleWorkspaceSidebar",
        "表示: ワークスペースサイドバーの切り替え",
    ),
    ("multi_workspace::NextProject", "次のプロジェクトへ"),
    ("multi_workspace::PreviousProject", "前のプロジェクトへ"),
    (
        "multi_workspace::MoveProjectToNewWindow",
        "プロジェクトを新しいウィンドウへ移動",
    ),
    (
        "window::MoveTabToNewWindow",
        "ウィンドウ: システムタブを新しいウィンドウへ移動",
    ),
    (
        "window::MergeAllWindows",
        "ウィンドウ: すべてのウィンドウを統合",
    ),
    // pane / tabs
    ("pane::CloseActiveItem", "タブ: タブを閉じる"),
    (
        "workspace::MoveItemToNewWindow",
        "タブ: 新しいウィンドウへ移動",
    ),
    ("pane::CloseAllItems", "タブ: すべてのタブを閉じる"),
    ("pane::CloseCleanItems", "タブ: 未編集のタブを閉じる"),
    ("pane::CloseItemsToTheLeft", "タブ: 左側のタブを閉じる"),
    ("pane::CloseItemsToTheRight", "タブ: 右側のタブを閉じる"),
    (
        "pane::CloseMultibufferItems",
        "タブ: マルチバッファーのタブを閉じる",
    ),
    ("pane::ActivateNextItem", "タブ: 次のタブ"),
    ("pane::ActivatePreviousItem", "タブ: 前のタブ"),
    ("pane::ActivateLastItem", "タブ: 最後のタブ"),
    ("pane::ReopenClosedItem", "タブ: 閉じたタブを開き直す"),
    ("pane::TogglePinTab", "タブ: ピン留めの切り替え"),
    ("pane::UnpinAllTabs", "タブ: すべてのピン留めを解除"),
    ("pane::TogglePreviewTab", "タブ: プレビュータブの切り替え"),
    ("pane::SwapItemLeft", "タブ: 左へ移動"),
    ("pane::SwapItemRight", "タブ: 右へ移動"),
    ("pane::GoBack", "移動: 戻る"),
    ("pane::GoForward", "移動: 進む"),
    ("pane::SplitAndMoveLeft", "ペイン: 左へ分割して移動"),
    ("pane::SplitAndMoveRight", "ペイン: 右へ分割して移動"),
    ("pane::SplitAndMoveUp", "ペイン: 上へ分割して移動"),
    ("pane::SplitAndMoveDown", "ペイン: 下へ分割して移動"),
    ("pane::JoinAll", "ペイン: すべて結合"),
    ("pane::JoinIntoNext", "ペイン: 次のペインへ結合"),
    (
        "pane::AlternateFile",
        "ファイル: 対応するファイルへ切り替え",
    ),
    ("pane::DeploySearch", "検索: プロジェクト内検索を開く"),
    ("pane::RevealInProjectPanel", "プロジェクトパネルで表示"),
    // editor: 編集
    ("editor::Undo", "編集: 元に戻す"),
    ("editor::Redo", "編集: やり直し"),
    ("editor::Copy", "編集: コピー"),
    ("editor::CopyAndTrim", "編集: 余白を削ってコピー"),
    ("editor::Cut", "編集: 切り取り"),
    ("editor::Paste", "編集: 貼り付け"),
    ("editor::DuplicateLineDown", "編集: 行を下に複製"),
    ("editor::DuplicateLineUp", "編集: 行を上に複製"),
    ("editor::DuplicateSelection", "編集: 選択範囲を複製"),
    ("editor::DeleteLine", "編集: 行を削除"),
    ("editor::MoveLineUp", "編集: 行を上へ移動"),
    ("editor::MoveLineDown", "編集: 行を下へ移動"),
    ("editor::JoinLines", "編集: 行を結合"),
    ("editor::ToggleComments", "編集: 行コメントの切り替え"),
    (
        "editor::ToggleBlockComments",
        "編集: ブロックコメントの切り替え",
    ),
    ("editor::Indent", "編集: インデント"),
    ("editor::Outdent", "編集: インデント解除"),
    ("editor::AutoIndent", "編集: 自動インデント"),
    ("editor::Rewrap", "編集: 段落を折り返し直す"),
    ("editor::Transpose", "編集: 文字を入れ替え"),
    ("editor::InsertSnippet", "編集: スニペットを挿入"),
    ("editor::InsertUuidV4", "編集: UUID v4 を挿入"),
    ("editor::InsertUuidV7", "編集: UUID v7 を挿入"),
    ("editor::ShowCharacterPalette", "編集: 絵文字と記号を表示"),
    ("editor::WrapSelectionsInTag", "編集: 選択範囲をタグで囲む"),
    ("editor::UnwrapSyntaxNode", "編集: 構文ノードの囲みを外す"),
    ("editor::AlignSelections", "編集: 選択範囲を揃える"),
    ("editor::ConvertToUpperCase", "編集: 大文字に変換"),
    ("editor::ConvertToLowerCase", "編集: 小文字に変換"),
    ("editor::ConvertToTitleCase", "編集: タイトルケースに変換"),
    (
        "editor::ConvertToSentenceCase",
        "編集: センテンスケースに変換",
    ),
    ("editor::ConvertToSnakeCase", "編集: スネークケースに変換"),
    ("editor::ConvertToKebabCase", "編集: ケバブケースに変換"),
    (
        "editor::ConvertToUpperCamelCase",
        "編集: アッパーキャメルケースに変換",
    ),
    (
        "editor::ConvertToLowerCamelCase",
        "編集: ローワーキャメルケースに変換",
    ),
    (
        "editor::ConvertToOppositeCase",
        "編集: 大文字と小文字を反転",
    ),
    ("editor::ToggleCase", "編集: 大文字と小文字を切り替え"),
    ("editor::ConvertToBase64", "編集: Base64 にエンコード"),
    ("editor::ConvertFromBase64", "編集: Base64 をデコード"),
    (
        "editor::ConvertIndentationToSpaces",
        "編集: インデントをスペースに変換",
    ),
    (
        "editor::ConvertIndentationToTabs",
        "編集: インデントをタブに変換",
    ),
    (
        "editor::SortLinesCaseSensitive",
        "編集: 行を並べ替え (大文字と小文字を区別)",
    ),
    (
        "editor::SortLinesCaseInsensitive",
        "編集: 行を並べ替え (大文字と小文字を区別しない)",
    ),
    ("editor::SortLinesByLength", "編集: 行を長さ順に並べ替え"),
    ("editor::ReverseLines", "編集: 行を逆順にする"),
    ("editor::ShuffleLines", "編集: 行をシャッフル"),
    (
        "editor::UniqueLinesCaseSensitive",
        "編集: 重複行を削除 (大文字と小文字を区別)",
    ),
    (
        "editor::UniqueLinesCaseInsensitive",
        "編集: 重複行を削除 (大文字と小文字を区別しない)",
    ),
    (
        "editor::DiffClipboardWithSelection",
        "編集: クリップボードと選択範囲を比較",
    ),
    ("editor::ReloadFile", "ファイルを再読み込み"),
    ("editor::RevealInFileManager", "ファイルマネージャーで表示"),
    ("editor::CopyFileName", "ファイル名をコピー"),
    (
        "editor::CopyFileNameWithoutExtension",
        "拡張子なしのファイル名をコピー",
    ),
    ("editor::CopyFileLocation", "ファイルの場所をコピー"),
    (
        "editor::CopyPermalinkToLine",
        "この行への永続リンクをコピー",
    ),
    ("editor::OpenPermalinkToLine", "この行への永続リンクを開く"),
    ("editor::OpenUrl", "URL を開く"),
    ("editor::OpenSelectedFilename", "選択したファイル名を開く"),
    // editor: 選択
    ("editor::SelectAll", "選択: すべて選択"),
    ("editor::SelectLine", "選択: 行を選択"),
    ("editor::SelectAllMatches", "選択: 一致箇所をすべて選択"),
    ("editor::SelectNext", "選択: 次の一致を選択に追加"),
    ("editor::SelectPrevious", "選択: 前の一致を選択に追加"),
    ("editor::AddSelectionAbove", "選択: 上にカーソルを追加"),
    ("editor::AddSelectionBelow", "選択: 下にカーソルを追加"),
    ("editor::SelectLargerSyntaxNode", "選択: 選択範囲を拡張"),
    ("editor::SelectSmallerSyntaxNode", "選択: 選択範囲を縮小"),
    ("editor::SelectEnclosingSymbol", "選択: 囲むシンボルを選択"),
    (
        "editor::SelectInsideDelimiters",
        "選択: 区切り文字の内側を選択",
    ),
    (
        "editor::SelectAroundDelimiters",
        "選択: 区切り文字を含めて選択",
    ),
    (
        "editor::SplitSelectionIntoLines",
        "選択: 選択範囲を行ごとに分割",
    ),
    ("editor::SwapSelectionEnds", "選択: 選択範囲の端を入れ替え"),
    ("editor::UndoSelection", "選択: 選択を元に戻す"),
    ("editor::RedoSelection", "選択: 選択をやり直し"),
    // editor: 言語機能・移動
    ("editor::GoToDefinition", "定義へ移動"),
    ("editor::GoToDefinitionSplit", "定義へ移動 (分割して開く)"),
    ("editor::GoToDeclaration", "宣言へ移動"),
    ("editor::GoToDeclarationSplit", "宣言へ移動 (分割して開く)"),
    ("editor::GoToTypeDefinition", "型定義へ移動"),
    (
        "editor::GoToTypeDefinitionSplit",
        "型定義へ移動 (分割して開く)",
    ),
    ("editor::GoToImplementation", "実装へ移動"),
    (
        "editor::GoToImplementationSplit",
        "実装へ移動 (分割して開く)",
    ),
    ("editor::FindAllReferences", "すべての参照を検索"),
    ("editor::Rename", "シンボルの名前変更"),
    ("editor::Format", "フォーマット: ドキュメントをフォーマット"),
    (
        "editor::FormatSelections",
        "フォーマット: 選択範囲をフォーマット",
    ),
    ("editor::OrganizeImports", "インポートを整理"),
    ("editor::Hover", "ホバー情報を表示"),
    ("editor::ShowCompletions", "補完候補を表示"),
    ("editor::ShowWordCompletions", "単語の補完候補を表示"),
    ("editor::ShowSignatureHelp", "シグネチャヘルプを表示"),
    ("editor::ToggleCodeActions", "コードアクションを表示"),
    ("editor::OpenDocs", "ドキュメントを表示"),
    ("editor::ExpandMacroRecursively", "マクロを再帰的に展開"),
    ("editor::SwitchSourceHeader", "ソース/ヘッダーを切り替え"),
    ("editor::RestartLanguageServer", "言語サーバーを再起動"),
    ("editor::StopLanguageServer", "言語サーバーを停止"),
    (
        "lsp_tool::ReviewPausedWorktreeCode",
        "止めているフォルダーのコードを確認",
    ),
    (
        "editor::CancelLanguageServerWork",
        "言語サーバーの処理を中止",
    ),
    ("editor::GoToDiagnostic", "移動: 次の問題へ"),
    ("editor::GoToPreviousDiagnostic", "移動: 前の問題へ"),
    ("editor::GoToHunk", "移動: 次の変更箇所へ"),
    ("editor::GoToPreviousHunk", "移動: 前の変更箇所へ"),
    ("editor::GoToNextChange", "移動: 次の変更へ"),
    ("editor::GoToPreviousChange", "移動: 前の変更へ"),
    ("editor::GoToNextSymbol", "移動: 次のシンボルへ"),
    ("editor::GoToPreviousSymbol", "移動: 前のシンボルへ"),
    ("editor::GoToNextReference", "移動: 次の参照へ"),
    ("editor::GoToPreviousReference", "移動: 前の参照へ"),
    ("editor::GoToParentModule", "移動: 親モジュールへ"),
    ("editor::MoveToBeginning", "移動: 先頭へ"),
    ("editor::MoveToEnd", "移動: 末尾へ"),
    ("editor::MoveToEnclosingBracket", "移動: 対応する括弧へ"),
    ("editor::ScrollCursorTop", "スクロール: カーソルを上端へ"),
    ("editor::ScrollCursorCenter", "スクロール: カーソルを中央へ"),
    ("editor::ScrollCursorBottom", "スクロール: カーソルを下端へ"),
    // editor: 折りたたみ・表示
    ("editor::Fold", "折りたたみ"),
    ("editor::UnfoldLines", "展開"),
    ("editor::ToggleFold", "折りたたみの切り替え"),
    ("editor::FoldAll", "すべて折りたたむ"),
    ("editor::UnfoldAll", "すべて展開"),
    ("editor::ToggleFoldAll", "すべての折りたたみを切り替え"),
    ("editor::FoldRecursive", "再帰的に折りたたむ"),
    ("editor::UnfoldRecursive", "再帰的に展開"),
    ("editor::FoldSelectedRanges", "選択範囲を折りたたむ"),
    ("editor::FoldFunctionBodies", "関数本体を折りたたむ"),
    ("editor::ToggleInlayHints", "表示: インレイヒントの切り替え"),
    ("editor::ToggleLineNumbers", "表示: 行番号の切り替え"),
    (
        "editor::ToggleRelativeLineNumbers",
        "表示: 相対行番号の切り替え",
    ),
    ("editor::ToggleSoftWrap", "表示: 折り返しの切り替え"),
    (
        "editor::ToggleIndentGuides",
        "表示: インデントガイドの切り替え",
    ),
    ("editor::ToggleMinimap", "表示: ミニマップの切り替え"),
    ("editor::ToggleTabBar", "表示: タブバーの切り替え"),
    ("editor::ToggleBreadcrumb", "表示: パンくずリストの切り替え"),
    ("editor::ToggleDiagnostics", "表示: 診断の切り替え"),
    (
        "editor::ToggleInlineDiagnostics",
        "表示: インライン診断の切り替え",
    ),
    ("editor::ToggleCodeLens", "表示: コードレンズの切り替え"),
    (
        "editor::ToggleSemanticHighlights",
        "表示: セマンティックハイライトの切り替え",
    ),
    ("editor::DisplayCursorNames", "表示: カーソル名を表示"),
    // editor: 差分・抜粋・ブックマーク
    ("editor::OpenExcerpts", "抜粋を開く"),
    ("editor::OpenExcerptsSplit", "抜粋を分割して開く"),
    ("editor::ExpandExcerpts", "抜粋を広げる"),
    ("editor::ExpandExcerptsUp", "抜粋を上に広げる"),
    ("editor::ExpandExcerptsDown", "抜粋を下に広げる"),
    ("editor::ExpandAllDiffHunks", "差分をすべて展開"),
    ("editor::CollapseAllDiffHunks", "差分をすべて折りたたむ"),
    ("editor::ToggleAllDiffHunks", "すべての差分の展開切り替え"),
    (
        "editor::ToggleSelectedDiffHunks",
        "選択した差分の展開切り替え",
    ),
    ("editor::ToggleSplitDiff", "差分の分割表示を切り替え"),
    ("editor::ApplyDiffHunk", "差分を適用"),
    ("editor::ApplyAllDiffHunks", "すべての差分を適用"),
    ("editor::ToggleBookmark", "ブックマーク: 切り替え"),
    (
        "editor::ToggleBookmarkWithLabel",
        "ブックマーク: ラベルを付けて切り替え",
    ),
    ("editor::ViewBookmarks", "ブックマーク: 一覧を表示"),
    (
        "editor::GoToNextBookmark",
        "ブックマーク: 次のブックマークへ",
    ),
    (
        "editor::GoToPreviousBookmark",
        "ブックマーク: 前のブックマークへ",
    ),
    (
        "editor::ToggleGitBlameInline",
        "Git: インライン blame の切り替え",
    ),
    ("editor::OpenGitBlameCommit", "Git: この行のコミットを開く"),
    ("editor::AcceptEditPrediction", "編集予測を採用"),
    ("editor::ShowEditPrediction", "編集予測を表示"),
    ("editor::ToggleEditPrediction", "編集予測の切り替え"),
    ("editor::SpawnNearestTask", "タスク: 最寄りのタスクを実行"),
    (
        "editor::ToggleBreakpoint",
        "デバッグ: ブレークポイントの切り替え",
    ),
    (
        "editor::EnableBreakpoint",
        "デバッグ: ブレークポイントを有効化",
    ),
    (
        "editor::DisableBreakpoint",
        "デバッグ: ブレークポイントを無効化",
    ),
    // project_panel
    ("project_panel::Toggle", "プロジェクトパネル: 表示切り替え"),
    (
        "project_panel::ToggleFocus",
        "プロジェクトパネル: フォーカス切り替え",
    ),
    ("project_panel::NewFile", "プロジェクトパネル: 新規ファイル"),
    (
        "project_panel::NewDirectory",
        "プロジェクトパネル: 新規フォルダー",
    ),
    ("project_panel::Rename", "プロジェクトパネル: 名前の変更"),
    ("project_panel::Delete", "プロジェクトパネル: 完全に削除"),
    ("project_panel::Trash", "プロジェクトパネル: ゴミ箱へ移動"),
    ("project_panel::Duplicate", "プロジェクトパネル: 複製"),
    ("project_panel::Copy", "プロジェクトパネル: コピー"),
    ("project_panel::Cut", "プロジェクトパネル: 切り取り"),
    ("project_panel::Paste", "プロジェクトパネル: 貼り付け"),
    (
        "project_panel::CollapseAllEntries",
        "プロジェクトパネル: すべて折りたたむ",
    ),
    (
        "project_panel::ExpandAllEntries",
        "プロジェクトパネル: すべて展開",
    ),
    (
        "project_panel::Refresh",
        "プロジェクトパネル: 最新の情報に更新",
    ),
    (
        "project_panel::RevealInFileManager",
        "プロジェクトパネル: ファイルマネージャーで表示",
    ),
    (
        "project_panel::OpenSplitVertical",
        "プロジェクトパネル: 垂直分割で開く",
    ),
    (
        "project_panel::OpenSplitHorizontal",
        "プロジェクトパネル: 水平分割で開く",
    ),
    (
        "project_panel::OpenPermanent",
        "プロジェクトパネル: 固定タブで開く",
    ),
    (
        "project_panel::OpenMarkdownPreview",
        "プロジェクトパネル: Markdown プレビューを開く",
    ),
    (
        "project_panel::NewSearchInDirectory",
        "プロジェクトパネル: このフォルダー内を検索",
    ),
    (
        "project_panel::RemoveFromProject",
        "プロジェクトパネル: プロジェクトから削除",
    ),
    (
        "project_panel::ToggleHideGitIgnore",
        "プロジェクトパネル: Git 無視ファイルの表示切り替え",
    ),
    (
        "project_panel::ToggleHideHidden",
        "プロジェクトパネル: 隠しファイルの表示切り替え",
    ),
    (
        "project_panel::CompareMarkedFiles",
        "プロジェクトパネル: マークしたファイルを比較",
    ),
    (
        "project_panel::SelectNextGitEntry",
        "プロジェクトパネル: 次の Git 変更へ",
    ),
    (
        "project_panel::SelectPrevGitEntry",
        "プロジェクトパネル: 前の Git 変更へ",
    ),
    (
        "project_panel::SelectNextDiagnostic",
        "プロジェクトパネル: 次の問題へ",
    ),
    (
        "project_panel::SelectPrevDiagnostic",
        "プロジェクトパネル: 前の問題へ",
    ),
    // outline
    ("outline::Toggle", "アウトライン: シンボルへ移動"),
    ("outline_panel::Toggle", "アウトラインパネル: 表示切り替え"),
    (
        "outline_panel::ToggleFocus",
        "アウトラインパネル: フォーカス切り替え",
    ),
    (
        "outline_panel::CollapseAllEntries",
        "アウトラインパネル: すべて折りたたむ",
    ),
    (
        "outline_panel::ExpandAllEntries",
        "アウトラインパネル: すべて展開",
    ),
    (
        "outline_panel::RevealInFileManager",
        "アウトラインパネル: ファイルマネージャーで表示",
    ),
    (
        "outline_panel::ToggleActiveEditorPin",
        "アウトラインパネル: エディターの固定切り替え",
    ),
    // 検索
    ("buffer_search::Deploy", "検索: ファイル内を検索"),
    ("buffer_search::DeployReplace", "検索: ファイル内を置換"),
    ("buffer_search::Dismiss", "検索: 検索バーを閉じる"),
    ("buffer_search::FocusEditor", "検索: エディターにフォーカス"),
    (
        "buffer_search::UseSelectionForFind",
        "検索: 選択範囲を検索語にする",
    ),
    ("search::ToggleCaseSensitive", "検索: 大文字と小文字を区別"),
    ("search::ToggleWholeWord", "検索: 単語単位で検索"),
    ("search::ToggleRegex", "検索: 正規表現を使用"),
    ("search::ToggleReplace", "検索: 置換の表示切り替え"),
    ("search::ToggleSelection", "検索: 選択範囲内を検索"),
    ("search::ToggleIncludeIgnored", "検索: 無視ファイルを含める"),
    ("search::SelectNextMatch", "検索: 次の一致へ"),
    ("search::SelectPreviousMatch", "検索: 前の一致へ"),
    ("search::SelectAllMatches", "検索: すべての一致を選択"),
    ("search::ReplaceNext", "検索: 次を置換"),
    ("search::ReplaceAll", "検索: すべて置換"),
    ("search::FocusSearch", "検索: 検索ボックスにフォーカス"),
    ("search::NextHistoryQuery", "検索: 次の検索履歴"),
    ("search::PreviousHistoryQuery", "検索: 前の検索履歴"),
    ("search::CycleMode", "検索: 検索モードを切り替え"),
    (
        "search::NewSearchInDirectory",
        "検索: このフォルダー内を検索",
    ),
    (
        "project_search::ToggleFilters",
        "検索: フィルターの表示切り替え",
    ),
    (
        "project_search::ToggleFocus",
        "検索: プロジェクト内検索にフォーカス",
    ),
    ("project_search::SearchInNew", "検索: 新しいタブで検索"),
    (
        "project_search::ToggleAllSearchResults",
        "検索: 検索結果の展開切り替え",
    ),
    (
        "project_search::OpenTextFinder",
        "検索: テキストファインダーを開く",
    ),
    // diagnostics
    ("diagnostics::Deploy", "問題: プロジェクトの診断を開く"),
    (
        "diagnostics::DeployCurrentFile",
        "問題: このファイルの診断を開く",
    ),
    ("diagnostics::ToggleWarnings", "問題: 警告の表示切り替え"),
    (
        "diagnostics::ToggleDiagnosticsRefresh",
        "問題: 自動更新の切り替え",
    ),
    // terminal
    ("terminal_panel::Toggle", "ターミナル: 表示切り替え"),
    (
        "terminal_panel::ToggleFocus",
        "ターミナル: フォーカス切り替え",
    ),
    ("terminal::Clear", "ターミナル: 画面を消去"),
    ("terminal::Copy", "ターミナル: コピー"),
    ("terminal::Paste", "ターミナル: 貼り付け"),
    ("terminal::SelectAll", "ターミナル: すべて選択"),
    ("terminal::RenameTerminal", "ターミナル: 名前の変更"),
    ("terminal::RestartTerminal", "ターミナル: 再起動"),
    ("terminal::RerunTask", "ターミナル: タスクを再実行"),
    (
        "terminal::ToggleSessionLog",
        "ターミナル: ログの記録を開始/停止",
    ),
    (
        "terminal::StartSessionLog",
        "ターミナル: ログの記録を開始 (保存先を選ぶ)…",
    ),
    ("terminal::StopSessionLog", "ターミナル: ログの記録を停止"),
    (
        "terminal::OpenSessionLog",
        "ターミナル: ログファイルの場所を開く",
    ),
    (
        "terminal::TailSessionLog",
        "ターミナル: ログをエディターで開く",
    ),
    (
        "terminal::ToggleBroadcastInput",
        "ターミナル: 全ペインに同時入力",
    ),
    ("terminal::SplitGrid", "ターミナル: グリッドに分割"),
    ("terminal::RunMacro", "ターミナル: マクロを実行…"),
    ("terminal::EditMacros", "ターミナル: マクロの編集…"),
    ("terminal::CancelMacro", "ターミナル: マクロを中止"),
    ("terminal::QuickConnect", "ターミナル: クイック接続…"),
    ("terminal::OpenFileTransfer", "ターミナル: ファイル転送…"),
    (
        "file_transfer::FocusNextField",
        "ファイル転送: 次の入力欄へ",
    ),
    (
        "file_transfer::FocusPreviousField",
        "ファイル転送: 前の入力欄へ",
    ),
    (
        "terminal::InstallClaudeCode",
        "ターミナル: Claude Code をインストール (公式コマンド)",
    ),
    (
        "terminal::LaunchClaudeCode",
        "ターミナル: Claude Code を起動",
    ),
    ("terminal::ScrollToTop", "ターミナル: 先頭までスクロール"),
    ("terminal::ScrollToBottom", "ターミナル: 末尾までスクロール"),
    (
        "terminal::ScrollPageUp",
        "ターミナル: 1 ページ上へスクロール",
    ),
    (
        "terminal::ScrollPageDown",
        "ターミナル: 1 ページ下へスクロール",
    ),
    ("terminal::ScrollLineUp", "ターミナル: 1 行上へスクロール"),
    ("terminal::ScrollLineDown", "ターミナル: 1 行下へスクロール"),
    ("terminal::ToggleViMode", "ターミナル: Vim モードの切り替え"),
    (
        "terminal::ShowCharacterPalette",
        "ターミナル: 絵文字と記号を表示",
    ),
    // git
    ("git_panel::Toggle", "Git: パネルの表示切り替え"),
    ("git_panel::ToggleFocus", "Git: パネルのフォーカス切り替え"),
    ("git_panel::ActivateChangesTab", "Git: 変更タブを表示"),
    ("git_panel::ActivateHistoryTab", "Git: 履歴タブを表示"),
    ("git_panel::ToggleTreeView", "Git: ツリー表示の切り替え"),
    (
        "git_panel::ToggleFillCoAuthors",
        "Git: Co-authored-by を自動入力",
    ),
    ("git::StageAll", "Git: すべてステージ"),
    ("git::UnstageAll", "Git: すべてアンステージ"),
    ("git::StageFile", "Git: ファイルをステージ"),
    ("git::UnstageFile", "Git: ファイルをアンステージ"),
    ("git::ToggleStaged", "Git: ステージ状態を切り替え"),
    ("git::StageAndNext", "Git: ステージして次へ"),
    ("git::UnstageAndNext", "Git: アンステージして次へ"),
    ("git::StageSection", "Git: このセクションをステージ"),
    ("git::UnstageSection", "Git: このセクションをアンステージ"),
    ("git::StageRange", "Git: 選択範囲をステージ"),
    ("git::Commit", "Git: コミット"),
    ("git::Amend", "Git: 直前のコミットを修正"),
    ("git::Uncommit", "Git: 直前のコミットを取り消し"),
    (
        "git::GenerateCommitMessage",
        "Git: コミットメッセージを生成",
    ),
    ("git::ExpandCommitEditor", "Git: コミットエディターを広げる"),
    ("git::Signoff", "Git: Signed-off-by を付ける"),
    ("git::SkipHooks", "Git: フックをスキップ"),
    ("git::Push", "Git: プッシュ"),
    ("git::ForcePush", "Git: 強制プッシュ"),
    ("git::PushTo", "Git: プッシュ先を指定してプッシュ"),
    ("git::Pull", "Git: プル"),
    ("git::PullRebase", "Git: リベースしてプル"),
    ("git::Fetch", "Git: フェッチ"),
    ("git::FetchFrom", "Git: 取得元を指定してフェッチ"),
    ("git::Merge", "Git: ブランチをマージ"),
    (
        "git::ResetToRemote",
        "Git: リモートの状態に強制的に合わせる",
    ),
    ("git::CreateRemote", "Git: リモートを追加"),
    ("git::RemoveRemote", "Git: リモートを削除"),
    ("git::CreatePatch", "Git: パッチファイルを作成"),
    ("git::ApplyPatch", "Git: パッチファイルを適用"),
    ("git::Init", "Git: リポジトリを初期化"),
    ("git::Clone", "Git: リポジトリをクローン"),
    ("git::SelectRepo", "Git: リポジトリを選択"),
    ("git::Branch", "Git: ブランチ一覧"),
    ("git::Switch", "Git: ブランチを切り替え"),
    ("git::CheckoutBranch", "Git: ブランチをチェックアウト"),
    ("git::RenameBranch", "Git: ブランチ名を変更"),
    ("git::CopyBranchName", "Git: ブランチ名をコピー"),
    ("branch_picker::DeleteBranch", "Git: ブランチを削除"),
    ("git::Diff", "Git: 差分を表示"),
    ("git::DiffHead", "Git: HEAD との差分を表示"),
    ("git::DiffBranch", "Git: ブランチとの差分を表示"),
    ("git::CompareWithBranch", "Git: ブランチと比較"),
    ("git::Blame", "Git: 注釈 (blame) を表示"),
    ("git::FileHistory", "Git: ファイルの履歴"),
    ("git::ViewCommit", "Git: コミットを表示"),
    ("git::ViewStagedChanges", "Git: ステージ済みの変更を表示"),
    ("git::ViewUnstagedChanges", "Git: 未ステージの変更を表示"),
    ("git::ViewUncommittedChanges", "Git: 未コミットの変更を表示"),
    ("git::OpenModifiedFiles", "Git: 変更されたファイルを開く"),
    ("git::OpenFileDiff", "Git: ファイルの差分を開く"),
    ("git::OpenFileAtHead", "Git: HEAD のファイルを開く"),
    ("git::Restore", "Git: 変更を元に戻す"),
    ("git::RestoreFile", "Git: ファイルの変更を元に戻す"),
    ("git::RestoreAndNext", "Git: 変更を元に戻して次へ"),
    (
        "git::RestoreTrackedFiles",
        "Git: 追跡中ファイルの変更を元に戻す",
    ),
    (
        "git::TrashUntrackedFiles",
        "Git: 未追跡ファイルをゴミ箱へ移動",
    ),
    ("git::StashAll", "Git: すべてスタッシュ"),
    ("git::StashStaged", "Git: ステージ済みをスタッシュ"),
    ("git::StashTracked", "Git: 追跡中ファイルをスタッシュ"),
    ("git::StashPop", "Git: スタッシュを取り出す"),
    ("git::StashApply", "Git: スタッシュを適用"),
    ("git::ViewStash", "Git: スタッシュを表示"),
    ("git::AddToGitignore", "Git: .gitignore に追加"),
    ("git::AddToGitInfoExclude", "Git: .git/info/exclude に追加"),
    ("git::CreatePullRequest", "Git: プルリクエストを作成"),
    ("git::CreateWorktree", "Git: ワークツリーを作成"),
    ("git::SwitchWorktree", "Git: ワークツリーを切り替え"),
    (
        "git::OpenWorktreeInNewWindow",
        "Git: ワークツリーを新しいウィンドウで開く",
    ),
    ("git_graph::Open", "Git: コミットグラフを開く"),
    // Markdown
    ("markdown::OpenPreview", "Markdown: プレビューを開く"),
    (
        "markdown::OpenPreviewToTheSide",
        "Markdown: プレビューを横に開く",
    ),
    (
        "markdown::OpenFollowingPreview",
        "Markdown: 追従プレビューを開く",
    ),
    (
        "markdown::CopyAsMarkdown",
        "Markdown: Markdown としてコピー",
    ),
    ("markdown::ToggleBlockQuote", "Markdown: 引用の切り替え"),
    ("markdown::ToggleBold", "Markdown: 太字の切り替え"),
    ("markdown::ToggleItalic", "Markdown: 斜体の切り替え"),
    (
        "markdown::ToggleStrikethrough",
        "Markdown: 取り消し線の切り替え",
    ),
    (
        "markdown::ToggleInlineCode",
        "Markdown: インラインコードの切り替え",
    ),
    ("markdown::ToggleHeading1", "Markdown: 見出し 1 の切り替え"),
    ("markdown::ToggleHeading2", "Markdown: 見出し 2 の切り替え"),
    ("markdown::ToggleHeading3", "Markdown: 見出し 3 の切り替え"),
    (
        "markdown::ToggleBulletList",
        "Markdown: 箇条書きリストの切り替え",
    ),
    (
        "markdown::ToggleNumberedList",
        "Markdown: 番号付きリストの切り替え",
    ),
    (
        "markdown::ToggleTaskList",
        "Markdown: タスクリストの切り替え",
    ),
    ("markdown::InsertLink", "Markdown: リンクを挿入"),
    ("markdown::InsertHorizontalRule", "Markdown: 水平線を挿入"),
    (
        "markdown::WrapInCodeBlock",
        "Markdown: コードブロックで囲む",
    ),
    (
        "markdown::NewlineInvertHardBreak",
        "Markdown: 改行 (行末スペース補完を反転)",
    ),
    (
        "markdown::InsertMermaidDiagram",
        "Markdown: Mermaid 図を挿入",
    ),
    (
        "markdown::InsertMermaidDiagramPicker",
        "Markdown: Mermaid 図を挿入 (種類を選ぶ)",
    ),
    ("markdown::EditTable", "Markdown: 表を編集…"),
    ("markdown::ExportToPdf", "Markdown: PDF に書き出す"),
    // IDE サーバー (ADR-F26)
    (
        "ide_server::ToggleIdeServer",
        "IDE サーバー: 有効・無効を切り替える",
    ),
    (
        "ide_server::CopyMcpConfiguration",
        "IDE サーバー: MCP 設定をコピー",
    ),
    (
        "ide_server::RevealLockFile",
        "IDE サーバー: ロックファイルの場所を開く",
    ),
    (
        "ide_server::SendSelectionToClaudeCode",
        "IDE サーバー: 選択範囲を Claude Code に送る",
    ),
    // ピッカー・セレクター
    ("command_palette::Toggle", "コマンドパレット"),
    ("file_finder::Toggle", "ファイルへ移動"),
    ("tab_switcher::Toggle", "タブを切り替え"),
    ("tab_switcher::ToggleAll", "すべてのタブを切り替え"),
    ("tab_switcher::CloseSelectedItem", "選択したタブを閉じる"),
    ("go_to_line::Toggle", "行または列へ移動"),
    ("theme_selector::Toggle", "テーマを選択"),
    ("icon_theme_selector::Toggle", "アイコンテーマを選択"),
    ("language_selector::Toggle", "言語モードを選択"),
    ("toolchain::Select", "ツールチェーンを選択"),
    ("toolchain::AddToolchain", "ツールチェーンを追加"),
    ("projects::OpenRecent", "最近のプロジェクトを開く"),
    ("projects::OpenRemote", "リモートプロジェクトを開く"),
    ("projects::OpenWsl", "WSL のプロジェクトを開く"),
    ("projects::OpenFolderInWsl", "WSL のフォルダーを開く"),
    ("projects::OpenDevContainer", "開発コンテナーを開く"),
    ("projects::InitializeDevContainer", "開発コンテナーを初期化"),
    ("recent_projects::AddToWorkspace", "ワークスペースに追加"),
    ("journal::NewJournalEntry", "新規ジャーナルエントリ"),
    ("snippets::ConfigureSnippets", "スニペットを設定"),
    ("snippets::OpenFolder", "スニペットフォルダーを開く"),
    ("image_viewer::ZoomIn", "画像: 拡大"),
    ("image_viewer::ZoomOut", "画像: 縮小"),
    ("image_viewer::ResetZoom", "画像: ズームをリセット"),
    ("image_viewer::FitToView", "画像: 画面に合わせる"),
    ("image_viewer::ZoomToActualSize", "画像: 実際のサイズ"),
    // タスク・デバッグ
    ("task::Spawn", "タスク: タスクを実行"),
    ("task::Rerun", "タスク: 直前のタスクを再実行"),
    ("debug_panel::Toggle", "デバッグ: パネルの表示切り替え"),
    (
        "debug_panel::ToggleFocus",
        "デバッグ: パネルのフォーカス切り替え",
    ),
    ("debugger::Start", "デバッグ: 開始"),
    ("debugger::Continue", "デバッグ: 続行"),
    ("debugger::Pause", "デバッグ: 一時停止"),
    ("debugger::Stop", "デバッグ: 停止"),
    ("debugger::Restart", "デバッグ: 再起動"),
    ("debugger::Detach", "デバッグ: デタッチ"),
    ("debugger::StepOver", "デバッグ: ステップオーバー"),
    ("debugger::StepInto", "デバッグ: ステップイン"),
    ("debugger::StepOut", "デバッグ: ステップアウト"),
    ("debugger::StepBack", "デバッグ: ステップバック"),
    ("debugger::RunToCursor", "デバッグ: カーソル位置まで実行"),
    ("debugger::RerunSession", "デバッグ: セッションを再実行"),
    ("debugger::EvaluateSelectedText", "デバッグ: 選択範囲を評価"),
    (
        "debugger::ClearAllBreakpoints",
        "デバッグ: すべてのブレークポイントを削除",
    ),
    (
        "debugger::ToggleEnableBreakpoint",
        "デバッグ: ブレークポイントの有効/無効を切り替え",
    ),
    (
        "debugger::ToggleIgnoreBreakpoints",
        "デバッグ: ブレークポイントを一時的に無視",
    ),
    (
        "debugger::OpenProjectDebugTasks",
        "デバッグ: プロジェクトのデバッグタスクを開く",
    ),
    // エージェント
    ("agent::Toggle", "エージェント: パネルの表示切り替え"),
    (
        "agent::ToggleFocus",
        "エージェント: パネルのフォーカス切り替え",
    ),
    ("agent::NewThread", "エージェント: 新規スレッド"),
    (
        "agent::OpenThreadInTab",
        "エージェント: スレッドをタブで開く",
    ),
    (
        "agent::OpenThreadInSplit",
        "エージェント: スレッドを右に分割して開く",
    ),
    (
        "agent::MoveThreadToPanel",
        "エージェント: スレッドをパネルに戻す",
    ),
    (
        "agent::NewTerminalThread",
        "エージェント: 新規ターミナルスレッド",
    ),
    (
        "agent::NewExternalAgentThread",
        "エージェント: 外部エージェントの新規スレッド",
    ),
    ("agent::OpenSettings", "エージェント: 設定を開く"),
    ("agent::SelectAgent", "エージェント: エージェントを選択"),
    ("agent::ToggleModelSelector", "エージェント: モデルを選択"),
    (
        "agent::ToggleProfileSelector",
        "エージェント: プロファイルを選択",
    ),
    (
        "agent::ToggleOptionsMenu",
        "エージェント: オプションメニュー",
    ),
    (
        "agent::TogglePanelThreadHistory",
        "エージェント: パネル内のスレッド履歴の切り替え",
    ),
    (
        "agent::ToggleThinkingMode",
        "エージェント: 思考モードの切り替え",
    ),
    (
        "agent::ToggleFastMode",
        "エージェント: 高速モードの切り替え",
    ),
    (
        "agent::ToggleAutoApproveToolActions",
        "エージェント: ツールの自動承認を切り替え",
    ),
    (
        "agent::ToggleToolPermissionSelector",
        "エージェント: ツールの権限セレクターを開く",
    ),
    ("agent::ToggleSearch", "エージェント: スレッドを検索"),
    ("agent::Keep", "エージェント: 変更を採用"),
    ("agent::KeepAll", "エージェント: すべての変更を採用"),
    ("agent::Reject", "エージェント: 変更を拒否"),
    ("agent::RejectAll", "エージェント: すべての変更を拒否"),
    ("agent::AllowOnce", "エージェント: 今回だけ許可"),
    ("agent::AllowAlways", "エージェント: 常に許可"),
    ("agent::OpenAgentDiff", "エージェント: 差分を開く"),
    (
        "agent::OpenActiveThreadAsMarkdown",
        "エージェント: スレッドを Markdown で開く",
    ),
    (
        "agent::CopyThreadToClipboard",
        "エージェント: スレッドをクリップボードにコピー",
    ),
    (
        "agent::ExpandMessageEditor",
        "エージェント: メッセージ入力欄を広げる",
    ),
    (
        "agent::AddSelectionToThread",
        "エージェント: 選択範囲をスレッドに追加",
    ),
    ("agent::ManageProfiles", "エージェント: プロファイルを管理"),
    ("agent::ManageSkills", "エージェント: スキルを管理"),
    (
        "agent::RemoveSelectedThread",
        "エージェント: 選択したスレッドを削除",
    ),
    (
        "agent::RenameSelectedThread",
        "エージェント: 選択したスレッドの名前を変更",
    ),
    (
        "agent::ArchiveSelectedThread",
        "エージェント: 選択したスレッドをアーカイブ",
    ),
    ("agent::Follow", "エージェント: エージェントをフォロー"),
    (
        "assistant::InlineAssist",
        "エージェント: インラインアシスト",
    ),
    ("edit_prediction::ToggleMenu", "編集予測: メニューを表示"),
    ("edit_prediction::RatePredictions", "編集予測: 予測を評価"),
    // REPL・更新・フィードバック
    ("repl::Run", "REPL: 実行"),
    ("repl::RunInPlace", "REPL: その場で実行"),
    ("repl::Restart", "REPL: 再起動"),
    ("repl::Interrupt", "REPL: 中断"),
    ("repl::ClearOutputs", "REPL: 出力を消去"),
    ("repl::Sessions", "REPL: セッション一覧"),
    ("repl::Shutdown", "REPL: 終了"),
    ("auto_update::Check", "更新を確認"),
    ("auto_update::ViewReleaseNotes", "リリースノートを表示"),
    ("feedback::FileBugReport", "バグを報告"),
    ("feedback::RequestFeature", "機能をリクエスト"),
    ("feedback::EmailZed", "Zed にメールを送る"),
    // F1-2: 第 2 波で日本語化したクレートのアクション
    // editor: 入力・削除
    ("editor::Backspace", "編集: 前の文字を削除"),
    ("editor::Delete", "編集: 次の文字を削除"),
    ("editor::DeleteToEndOfLine", "編集: 行末まで削除"),
    ("editor::Newline", "編集: 改行を挿入"),
    ("editor::NewlineAbove", "編集: 上に行を挿入"),
    ("editor::NewlineBelow", "編集: 下に行を挿入"),
    ("editor::Tab", "編集: タブを挿入"),
    ("editor::Backtab", "編集: インデントを 1 段戻す"),
    ("editor::ConvertToRot13", "編集: ROT13 に変換"),
    ("editor::ConvertToRot47", "編集: ROT47 に変換"),
    ("editor::KillRingCut", "編集: キルリングに切り取り"),
    ("editor::KillRingYank", "編集: キルリングから貼り付け"),
    ("editor::SetMark", "編集: マークを設定"),
    // editor: 補完・メニュー
    ("editor::ConfirmCompletionInsert", "補完候補を挿入して確定"),
    (
        "editor::ConfirmCompletionReplace",
        "補完候補で置き換えて確定",
    ),
    ("editor::NextSnippetTabstop", "スニペット: 次のタブストップ"),
    (
        "editor::PreviousSnippetTabstop",
        "スニペット: 前のタブストップ",
    ),
    ("editor::ContextMenuFirst", "メニュー: 先頭の項目を選択"),
    ("editor::ContextMenuLast", "メニュー: 末尾の項目を選択"),
    ("editor::ContextMenuNext", "メニュー: 次の項目を選択"),
    ("editor::ContextMenuPrevious", "メニュー: 前の項目を選択"),
    ("editor::OpenContextMenu", "コンテキストメニューを開く"),
    ("editor::Cancel", "キャンセル"),
    ("editor::ConfirmRename", "シンボルの名前変更を確定"),
    (
        "editor::SignatureHelpNext",
        "シグネチャヘルプ: 次のシグネチャ",
    ),
    (
        "editor::SignatureHelpPrevious",
        "シグネチャヘルプ: 前のシグネチャ",
    ),
    (
        "editor::ToggleAutoSignatureHelp",
        "シグネチャヘルプの自動表示を切り替え",
    ),
    // editor: 編集予測
    (
        "editor::AcceptNextLineEditPrediction",
        "編集予測: 次の 1 行を採用",
    ),
    (
        "editor::AcceptNextWordEditPrediction",
        "編集予測: 次の 1 単語を採用",
    ),
    (
        "editor::AcceptPartialCopilotSuggestion",
        "編集予測: 候補の一部を採用",
    ),
    ("editor::NextEditPrediction", "編集予測: 次の候補"),
    ("editor::PreviousEditPrediction", "編集予測: 前の候補"),
    // editor: Git・差分レビュー
    ("editor::BlameHover", "Git: 注釈 (blame) をホバー表示"),
    ("editor::BlameRevision", "Git: このリビジョンを注釈 (blame)"),
    (
        "editor::BlamePreviousRevision",
        "Git: 前のリビジョンを注釈 (blame)",
    ),
    (
        "editor::SubmitDiffReviewComment",
        "差分レビュー: コメントを送信",
    ),
    (
        "editor::ToggleReviewCommentsExpanded",
        "差分レビュー: コメントの開閉",
    ),
    (
        "editor::SendReviewToAgent",
        "差分レビュー: エージェントへ送る",
    ),
    // editor: 診断・折りたたみ
    ("editor::RunFlycheck", "flycheck を実行"),
    ("editor::CancelFlycheck", "flycheck を中止"),
    ("editor::ClearFlycheck", "flycheck の結果を消去"),
    (
        "editor::ToggleFoldRecursive",
        "折りたたみ: 再帰的に切り替え",
    ),
    (
        "editor::FoldAtLevel_1",
        "折りたたみ: レベル 1 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_2",
        "折りたたみ: レベル 2 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_3",
        "折りたたみ: レベル 3 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_4",
        "折りたたみ: レベル 4 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_5",
        "折りたたみ: レベル 5 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_6",
        "折りたたみ: レベル 6 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_7",
        "折りたたみ: レベル 7 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_8",
        "折りたたみ: レベル 8 まで折りたたむ",
    ),
    (
        "editor::FoldAtLevel_9",
        "折りたたみ: レベル 9 まで折りたたむ",
    ),
    // editor: カーソル移動・スクロール
    ("editor::MoveLeft", "移動: 左へ"),
    ("editor::MoveRight", "移動: 右へ"),
    ("editor::MoveUp", "移動: 上へ"),
    ("editor::MoveDown", "移動: 下へ"),
    ("editor::PageUp", "移動: 1 ページ上へ"),
    ("editor::PageDown", "移動: 1 ページ下へ"),
    ("editor::LineUp", "スクロール: 1 行上へ"),
    ("editor::LineDown", "スクロール: 1 行下へ"),
    ("editor::HalfPageUp", "スクロール: 半ページ上へ"),
    ("editor::HalfPageDown", "スクロール: 半ページ下へ"),
    ("editor::NextScreen", "スクロール: 次の画面へ"),
    (
        "editor::ScrollCursorCenterTopBottom",
        "スクロール: カーソルを中央/上/下へ",
    ),
    ("editor::MoveToNextWordEnd", "移動: 次の単語の末尾へ"),
    ("editor::MoveToPreviousWordStart", "移動: 前の単語の先頭へ"),
    (
        "editor::MoveToNextSubwordEnd",
        "移動: 次のサブワードの末尾へ",
    ),
    (
        "editor::MoveToPreviousSubwordStart",
        "移動: 前のサブワードの先頭へ",
    ),
    ("editor::MoveToStartOfParagraph", "移動: 段落の先頭へ"),
    ("editor::MoveToEndOfParagraph", "移動: 段落の末尾へ"),
    (
        "editor::MoveToNextCommentParagraph",
        "移動: 次のコメント段落へ",
    ),
    (
        "editor::MoveToPreviousCommentParagraph",
        "移動: 前のコメント段落へ",
    ),
    ("editor::MoveToStartOfExcerpt", "移動: 抜粋の先頭へ"),
    ("editor::MoveToEndOfExcerpt", "移動: 抜粋の末尾へ"),
    ("editor::MoveToStartOfNextExcerpt", "移動: 次の抜粋の先頭へ"),
    (
        "editor::MoveToEndOfPreviousExcerpt",
        "移動: 前の抜粋の末尾へ",
    ),
    (
        "editor::MoveToStartOfLargerSyntaxNode",
        "移動: 外側の構文ノードの先頭へ",
    ),
    (
        "editor::MoveToEndOfLargerSyntaxNode",
        "移動: 外側の構文ノードの末尾へ",
    ),
    (
        "editor::GoToNextDocumentHighlight",
        "移動: 次のシンボル出現箇所へ",
    ),
    (
        "editor::GoToPreviousDocumentHighlight",
        "移動: 前のシンボル出現箇所へ",
    ),
    ("editor::FindNextMatch", "検索: 次の一致へ"),
    ("editor::FindPreviousMatch", "検索: 前の一致へ"),
    ("editor::SaveLocation", "現在位置を記録"),
    // editor: 選択の拡張
    ("editor::SelectLeft", "選択: 左へ拡張"),
    ("editor::SelectRight", "選択: 右へ拡張"),
    ("editor::SelectUp", "選択: 上へ拡張"),
    ("editor::SelectDown", "選択: 下へ拡張"),
    ("editor::SelectPageUp", "選択: 1 ページ上へ拡張"),
    ("editor::SelectPageDown", "選択: 1 ページ下へ拡張"),
    ("editor::SelectToBeginning", "選択: 先頭まで拡張"),
    ("editor::SelectToEnd", "選択: 末尾まで拡張"),
    (
        "editor::SelectToNextWordEnd",
        "選択: 次の単語の末尾まで拡張",
    ),
    (
        "editor::SelectToPreviousWordStart",
        "選択: 前の単語の先頭まで拡張",
    ),
    (
        "editor::SelectToNextSubwordEnd",
        "選択: 次のサブワードの末尾まで拡張",
    ),
    (
        "editor::SelectToPreviousSubwordStart",
        "選択: 前のサブワードの先頭まで拡張",
    ),
    (
        "editor::SelectToStartOfParagraph",
        "選択: 段落の先頭まで拡張",
    ),
    ("editor::SelectToEndOfParagraph", "選択: 段落の末尾まで拡張"),
    ("editor::SelectToStartOfExcerpt", "選択: 抜粋の先頭まで拡張"),
    ("editor::SelectToEndOfExcerpt", "選択: 抜粋の末尾まで拡張"),
    (
        "editor::SelectToStartOfNextExcerpt",
        "選択: 次の抜粋の先頭まで拡張",
    ),
    (
        "editor::SelectToEndOfPreviousExcerpt",
        "選択: 前の抜粋の末尾まで拡張",
    ),
    (
        "editor::SelectToStartOfLargerSyntaxNode",
        "選択: 外側の構文ノードの先頭まで拡張",
    ),
    (
        "editor::SelectToEndOfLargerSyntaxNode",
        "選択: 外側の構文ノードの末尾まで拡張",
    ),
    ("editor::SelectNextSyntaxNode", "選択: 次の構文ノードを選択"),
    (
        "editor::SelectPreviousSyntaxNode",
        "選択: 前の構文ノードを選択",
    ),
    (
        "editor::RotateSelectionsForward",
        "選択: 選択範囲の内容を後方へ回転",
    ),
    (
        "editor::RotateSelectionsBackward",
        "選択: 選択範囲の内容を前方へ回転",
    ),
    ("editor::ToggleSelectionMenu", "選択メニューの表示切り替え"),
    (
        "editor::OpenSelectionsInMultibuffer",
        "選択範囲をマルチバッファーで開く",
    ),
    // editor: その他
    (
        "editor::CopyHighlightJson",
        "ハイライト情報を JSON でコピー",
    ),
    ("editor::EditBookmark", "ブックマークを編集"),
    ("editor::EditLogBreakpoint", "ログポイントを編集"),
    ("editor::ToggleInlineValues", "インライン値の表示切り替え"),
    ("editor::ToggleFocus", "エディターにフォーカス"),
    ("editor::OpenProposedChangesEditor", "提案された変更を開く"),
    ("text_finder::Toggle", "テキストへ移動"),
    // プレビュー・セレクター
    ("svg::OpenPreview", "SVG: プレビューを開く"),
    ("svg::OpenPreviewToTheSide", "SVG: プレビューを横に開く"),
    ("svg::OpenFollowingPreview", "SVG: 追従プレビューを開く"),
    ("markdown::Copy", "Markdown: コピー"),
    ("markdown::ScrollUp", "Markdown プレビュー: 上へスクロール"),
    (
        "markdown::ScrollDown",
        "Markdown プレビュー: 下へスクロール",
    ),
    (
        "markdown::ScrollPageUp",
        "Markdown プレビュー: 1 ページ上へスクロール",
    ),
    (
        "markdown::ScrollPageDown",
        "Markdown プレビュー: 1 ページ下へスクロール",
    ),
    (
        "markdown::ScrollUpByItem",
        "Markdown プレビュー: 1 要素上へスクロール",
    ),
    (
        "markdown::ScrollDownByItem",
        "Markdown プレビュー: 1 要素下へスクロール",
    ),
    (
        "markdown::ScrollToTop",
        "Markdown プレビュー: 先頭へスクロール",
    ),
    (
        "markdown::ScrollToBottom",
        "Markdown プレビュー: 末尾へスクロール",
    ),
    (
        "markdown::CloseAndReturnToEditor",
        "Markdown プレビュー: 閉じてエディターに戻る",
    ),
    ("theme_selector::Reload", "テーマを再読み込み"),
    ("line_ending_selector::Toggle", "改行コードを選択"),
    ("encoding_selector::Toggle", "文字エンコードを選択"),
    (
        "activity_indicator::ShowErrorMessage",
        "エラーメッセージを表示",
    ),
    (
        "auto_update::ViewReleaseNotesLocally",
        "リリースノートをエディターで表示",
    ),
    (
        "recent_projects::RemoveSelected",
        "最近のプロジェクト: 一覧から削除",
    ),
    (
        "recent_projects::ToggleActionsMenu",
        "最近のプロジェクト: 操作メニューを開く",
    ),
    ("vim::OpenDefaultKeymap", "Vim: 既定のキーマップを開く"),
    // ピッカー共通
    ("picker::ConfirmCompletion", "ピッカー: 補完を確定"),
    (
        "picker::TogglePreview",
        "ピッカー: プレビューの表示切り替え",
    ),
    ("picker::SetPreviewRight", "ピッカー: プレビューを右に表示"),
    ("picker::SetPreviewBelow", "ピッカー: プレビューを下に表示"),
    ("picker::SetPreviewHidden", "ピッカー: プレビューを隠す"),
    ("picker::ToggleActionsMenu", "ピッカー: 操作メニューを開く"),
    (
        "picker::ToggleMultiSelect",
        "ピッカー: 複数選択モードを切り替え",
    ),
    ("picker::MultiSelectNext", "ピッカー: 選択して次へ"),
    // キーマップエディター
    (
        "keymap_editor::EditBinding",
        "キーマップ: キーバインドを編集",
    ),
    (
        "keymap_editor::CreateBinding",
        "キーマップ: キーバインドを追加",
    ),
    (
        "keymap_editor::OpenCreateKeybindingModal",
        "キーマップ: キーバインド追加ダイアログを開く",
    ),
    (
        "keymap_editor::DeleteBinding",
        "キーマップ: キーバインドを削除",
    ),
    (
        "keymap_editor::CopyAction",
        "キーマップ: アクション名をコピー",
    ),
    (
        "keymap_editor::CopyContext",
        "キーマップ: コンテキストをコピー",
    ),
    (
        "keymap_editor::ToggleConflictFilter",
        "キーマップ: 競合のみ表示を切り替え",
    ),
    (
        "keymap_editor::ToggleNoActionBindings",
        "キーマップ: 無効化された割り当ての表示を切り替え",
    ),
    (
        "keymap_editor::ToggleKeystrokeSearch",
        "キーマップ: キーストローク検索を切り替え",
    ),
    (
        "keymap_editor::ToggleExactKeystrokeMatching",
        "キーマップ: キーストロークの完全一致を切り替え",
    ),
    (
        "keymap_editor::ShowMatchingKeybinds",
        "キーマップ: 一致するキーバインドを表示",
    ),
    ("keystroke_input::StartRecording", "キー入力: 記録を開始"),
    ("keystroke_input::StopRecording", "キー入力: 記録を停止"),
    ("keystroke_input::ClearKeystrokes", "キー入力: 記録を消去"),
    // デバッグ
    ("debugger::Rerun", "デバッグ: 再実行"),
    (
        "debugger::UnsetBreakpoint",
        "デバッグ: ブレークポイントを解除",
    ),
    ("debugger::ToggleExpandItem", "デバッグ: 項目の開閉"),
    (
        "debugger::ToggleUserFrames",
        "デバッグ: 自分のコードのフレームのみ表示",
    ),
    ("debugger::FocusConsole", "デバッグ: コンソールにフォーカス"),
    ("debugger::FocusVariables", "デバッグ: 変数にフォーカス"),
    (
        "debugger::FocusFrames",
        "デバッグ: コールスタックにフォーカス",
    ),
    ("debugger::FocusModules", "デバッグ: モジュールにフォーカス"),
    (
        "debugger::FocusLoadedSources",
        "デバッグ: 読み込み済みソースにフォーカス",
    ),
    (
        "debugger::FocusBreakpointList",
        "デバッグ: ブレークポイント一覧にフォーカス",
    ),
    (
        "debugger::FocusTerminal",
        "デバッグ: ターミナルにフォーカス",
    ),
    (
        "debugger::GoToSelectedAddress",
        "デバッグ: 選択したアドレスへ移動",
    ),
    (
        "debugger::NextBreakpointProperty",
        "デバッグ: 次のブレークポイント設定項目",
    ),
    (
        "debugger::PreviousBreakpointProperty",
        "デバッグ: 前のブレークポイント設定項目",
    ),
    (
        "debugger::ToggleSessionPicker",
        "デバッグ: セッションを選択",
    ),
    ("debugger::ToggleThreadPicker", "デバッグ: スレッドを選択"),
    ("console::WatchExpression", "デバッグ: 式をウォッチに追加"),
    ("variable_list::AddWatch", "デバッグ: 変数をウォッチに追加"),
    ("variable_list::RemoveWatch", "デバッグ: ウォッチを削除"),
    ("variable_list::EditVariable", "デバッグ: 変数を編集"),
    (
        "variable_list::CopyVariableName",
        "デバッグ: 変数名をコピー",
    ),
    (
        "variable_list::CopyVariableValue",
        "デバッグ: 変数の値をコピー",
    ),
    (
        "variable_list::ExpandSelectedEntry",
        "デバッグ: 選択項目を展開",
    ),
    (
        "variable_list::CollapseSelectedEntry",
        "デバッグ: 選択項目を折りたたむ",
    ),
    ("variable_list::GoToMemory", "デバッグ: メモリを表示"),
    (
        "new_process_modal::ActivateDebugTab",
        "デバッグ: 新規プロセス - デバッグタブ",
    ),
    (
        "new_process_modal::ActivateLaunchTab",
        "デバッグ: 新規プロセス - 起動タブ",
    ),
    (
        "new_process_modal::ActivateAttachTab",
        "デバッグ: 新規プロセス - アタッチタブ",
    ),
    (
        "new_process_modal::ActivateTaskTab",
        "デバッグ: 新規プロセス - タスクタブ",
    ),
    // REPL・編集予測 (zeta)・開発者向け
    ("repl::ClearCurrentOutput", "REPL: 現在の出力を消去"),
    ("repl::RefreshKernelspecs", "REPL: カーネル一覧を更新"),
    ("zeta::FocusPredictions", "編集予測: 予測にフォーカス"),
    ("zeta::NextEdit", "編集予測: 次の編集へ"),
    ("zeta::PreviousEdit", "編集予測: 前の編集へ"),
    ("zeta::PreviewPrediction", "編集予測: 予測をプレビュー"),
    ("zeta::ThumbsUpActivePrediction", "編集予測: 予測を高評価"),
    ("zeta::ThumbsDownActivePrediction", "編集予測: 予測を低評価"),
    (
        "dev::OpenDebugAdapterLogs",
        "開発: デバッグアダプターのログを開く",
    ),
    (
        "dev::CopyDebugAdapterArguments",
        "開発: デバッグアダプターの引数をコピー",
    ),
    (
        "dev::OpenEditPredictionContextView",
        "開発: 編集予測のコンテキストを表示",
    ),
    (
        "dev::EditPredictionContextGoBack",
        "開発: 編集予測コンテキストを戻る",
    ),
    (
        "dev::EditPredictionContextGoForward",
        "開発: 編集予測コンテキストを進む",
    ),
    // F1-4: 設定ウィンドウ (settings_ui)
    ("settings_editor::Minimize", "設定: ウィンドウを最小化"),
    (
        "settings_editor::ToggleFocusNav",
        "設定: ナビゲーションとコンテンツのフォーカスを切り替え",
    ),
    (
        "settings_editor::ExpandNavEntry",
        "設定: ナビゲーション項目を展開",
    ),
    (
        "settings_editor::CollapseNavEntry",
        "設定: ナビゲーション項目を折りたたむ",
    ),
    ("settings_editor::FocusNextFile", "設定: 次の設定ファイルへ"),
    (
        "settings_editor::FocusPreviousFile",
        "設定: 前の設定ファイルへ",
    ),
    ("settings_editor::FocusFile", "設定: 指定した設定ファイルへ"),
    (
        "settings_editor::OpenCurrentFile",
        "設定: 現在の設定ファイルを開く",
    ),
    (
        "settings_editor::FocusPreviousRootNavEntry",
        "設定: 前のトップレベル項目へ",
    ),
    (
        "settings_editor::FocusNextRootNavEntry",
        "設定: 次のトップレベル項目へ",
    ),
    (
        "settings_editor::FocusFirstNavEntry",
        "設定: 最初のナビゲーション項目へ",
    ),
    (
        "settings_editor::FocusLastNavEntry",
        "設定: 最後のナビゲーション項目へ",
    ),
    (
        "settings_editor::FocusNextNavEntry",
        "設定: 次のナビゲーション項目を開く",
    ),
    (
        "settings_editor::FocusPreviousNavEntry",
        "設定: 前のナビゲーション項目を開く",
    ),
    ("skill_creator::SaveSkill", "スキル作成: スキルを保存"),
    ("skill_creator::Cancel", "スキル作成: キャンセル"),
    ("skill_creator::FocusNextField", "スキル作成: 次の入力欄へ"),
    (
        "skill_creator::FocusPreviousField",
        "スキル作成: 前の入力欄へ",
    ),
    // browser_panel (ADR-F19)
    ("browser_panel::Toggle", "ブラウザー: パネルの表示切り替え"),
    (
        "browser_panel::ToggleFocus",
        "ブラウザー: パネルのフォーカス切り替え",
    ),
    ("browser_panel::NewBrowserTab", "ブラウザー: 新しいタブ"),
    (
        "browser_panel::OpenInCenter",
        "ブラウザー: エディター領域へ移す",
    ),
    (
        "browser_panel::NewPanelTab",
        "ブラウザー: パネルに新しいタブ",
    ),
    (
        "browser_panel::ClosePanelTab",
        "ブラウザー: パネルのタブを閉じる",
    ),
    (
        "browser_panel::SetAsHomePage",
        "ブラウザー: このページを起動時に開く",
    ),
    ("browser_panel::NavigateBack", "ブラウザー: 戻る"),
    ("browser_panel::NavigateForward", "ブラウザー: 進む"),
    ("browser_panel::Reload", "ブラウザー: 再読み込み"),
    (
        "browser_panel::FocusAddressBar",
        "ブラウザー: アドレスバーにフォーカス",
    ),
    (
        "browser_panel::SubmitAddress",
        "ブラウザー: 入力した URL を開く",
    ),
    ("browser_panel::GoHome", "ブラウザー: ホームを開く"),
    (
        "browser_panel::OpenInExternalBrowser",
        "ブラウザー: 外部ブラウザーで開く",
    ),
    (
        "browser_panel::SearchSelection",
        "ブラウザー: 選択テキストを Web で検索",
    ),
    (
        "browser_panel::AskCopilot",
        "ブラウザー: 選択テキストを Copilot に聞く",
    ),
    (
        "browser_panel::SplitPanelRight",
        "ブラウザー: パネルを右に分割",
    ),
    (
        "browser_panel::FocusNextPaneGroup",
        "ブラウザー: 次のペインにフォーカス",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_no_duplicate_action_names() {
        let mut seen = HashSet::new();
        for (action_name, _) in LABEL_TABLE {
            assert!(
                seen.insert(*action_name),
                "duplicate action name in LABEL_TABLE: {action_name}"
            );
        }
    }

    #[test]
    fn test_labels_are_non_empty_and_trimmed() {
        for (action_name, label) in LABEL_TABLE {
            assert!(!label.is_empty(), "empty label for {action_name}");
            assert_eq!(
                *label,
                label.trim(),
                "label for {action_name} has surrounding whitespace"
            );
        }
    }

    #[test]
    fn test_lookup() {
        assert_eq!(
            label_for_action("editor::GoToDefinition"),
            Some("定義へ移動")
        );
        assert_eq!(label_for_action("editor::NoSuchActionExists"), None);
    }
}
