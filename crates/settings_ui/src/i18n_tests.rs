//! The settings table keeps upstream's English and is translated when drawn,
//! from `locales/ja/_settings.po`. These tests pin that the Japanese on screen is
//! the Japanese the table used to hold, that the English is complete, and that
//! search and page lookups work in both languages.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
        "ForgeKairos のメニュー、設定、メッセージを表示する言語です。タブ名など一部の表示は再起動後に切り替わります。",
    ),
    // Added by upstream v1.19.2. The folder indicators replace the folder icon
    // switches, whose rows left the table.
    (
        "p0.i11[reveal_if_open].title",
        "開いているファイルはそのペインで表示",
    ),
    (
        "p0.i11[reveal_if_open].description",
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
        "p6.i64[close_panel_on_toggle].title",
        "切り替え時にパネルを閉じる",
    ),
    (
        "p6.i64[close_panel_on_toggle].description",
        "フォーカスのあるパネルで ToggleFocus アクションを実行したときに、フォーカスをエディターに戻す代わりに、パネルを閉じるかどうかを設定します。",
    ),
    (
        "p7.i7[project_panel.folder_indicator].title",
        "フォルダーの表示",
    ),
    (
        "p7.i7[project_panel.folder_indicator].description",
        "プロジェクトパネルのディレクトリに何を表示するかを設定します。",
    ),
    (
        "p7.i40[outline_panel.folder_indicator].title",
        "フォルダーの表示",
    ),
    (
        "p7.i40[outline_panel.folder_indicator].description",
        "アウトラインパネルのディレクトリに何を表示するかを設定します。",
    ),
    (
        "p7.i46[outline_panel.multi_buffer_hide_symbols].title",
        "マルチバッファーではシンボルを隠す",
    ),
    (
        "p7.i46[outline_panel.multi_buffer_hide_symbols].description",
        "マルチバッファーを表示しているときに、アウトラインパネルのシンボル・抜粋・検索結果を隠すかどうかを設定します。",
    ),
    (
        "p7.i59[git_panel.folder_indicator].title",
        "フォルダーの表示",
    ),
    (
        "p7.i59[git_panel.folder_indicator].description",
        "Git パネルのディレクトリに何を表示するかを設定します。",
    ),
    // Added by upstream v1.20.2.
    ("p0.i4[on_new_window].title", "新しいウィンドウを開いたとき"),
    (
        "p0.i4[on_new_window].description",
        "新しいウィンドウを開いたときに表示する内容です。",
    ),
    (
        "p1.i30[cursor_animation.enabled].title",
        "カーソルのアニメーション",
    ),
    (
        "p1.i30[cursor_animation.enabled].description",
        "エディター内でカーソルが移動するときに、滑らかにアニメーションさせるかどうかを設定します。",
    ),
    (
        "p3.i148[markdown_preview.open_markdown_files_in_preview].title",
        "Markdown ファイルをプレビューで開く",
    ),
    (
        "p3.i148[markdown_preview.open_markdown_files_in_preview].description",
        "Markdown ファイルを自動的にプレビューで開くかどうかを設定します。",
    ),
    ("p5.i10.header", "コマンドパレット"),
    (
        "p5.i11[command_palette.use_command_history].title",
        "コマンドの履歴を使う",
    ),
    (
        "p5.i11[command_palette.use_command_history].description",
        "コマンドパレットの並べ替えに、コマンドの使用履歴による順位を使うかどうかを設定します。",
    ),
    (
        "p6.i6[status_bar.pending_keystrokes_indicator].title",
        "入力中のキーストロークのインジケーター",
    ),
    (
        "p6.i6[status_bar.pending_keystrokes_indicator].description",
        "複数ストロークのキーバインドの入力中に、カウントダウン付きのインジケーターを表示します。Which-key メニューが有効なときは、そのキーバインドのプレビューのポップオーバーは無効になります。",
    ),
    ("p6.i55[window_title_format].title", "タイトルの書式"),
    (
        "p6.i55[window_title_format].description",
        "ウィンドウのタイトルのテンプレートです。使える変数は `${projectName}`、`${fileName}`、`${filePath}`、`${relativePath}`、`${fileStem}`、`${remoteName}`、`${remoteHost}`、`${appName}`、`${branch}`、`${separator}` です。`${separator}` は隣の変数が空のときは省かれますが、文字どおりに書いたテキストは残ります。共同作業のインジケーターがある場合は、展開したテンプレートの後ろに付きます。テンプレートを展開した結果が空のときは、既定のテンプレートを使います。",
    ),
    (
        "p6.i55[window_title_format].placeholder",
        "${projectName}${separator}${fileName}",
    ),
    (
        "p6.i56[window_title_separator].title",
        "タイトルの区切り文字",
    ),
    (
        "p6.i56[window_title_separator].description",
        "ウィンドウのタイトルの書式で `${separator}` に入る文字列です。前後の空白も値に含めてください。",
    ),
    ("p6.i56[window_title_separator].placeholder", " — "),
    (
        "p7.i3[project_panel.title_tooltip_delay$].title",
        "プロジェクトパネルの項目名のツールチップの遅延",
    ),
    (
        "p7.i3[project_panel.title_tooltip_delay$].description",
        "プロジェクトパネルの項目名のツールチップが表示されるまでの遅延 (ミリ秒) です。",
    ),
    (
        "p7.i3.v1.f0[project_panel.title_tooltip_delay].title",
        "カスタムの遅延",
    ),
    (
        "p7.i3.v1.f0[project_panel.title_tooltip_delay].description",
        "プロジェクトパネルの項目名のツールチップの遅延 (ミリ秒) です。",
    ),
    (
        "language.i67[markdown_preview.open_markdown_files_in_preview].title",
        "Markdown ファイルをプレビューで開く",
    ),
    (
        "language.i67[markdown_preview.open_markdown_files_in_preview].description",
        "Markdown ファイルを自動的にプレビューで開くかどうかを設定します。",
    ),
    // Added by upstream v1.21.0.
    (
        "p12.i5[agent.threads_sidebar.default_width].title",
        "スレッドサイドバーの既定の幅",
    ),
    (
        "p12.i5[agent.threads_sidebar.default_width].description",
        "スレッドサイドバーの既定の幅です。この設定を変えると、手動でサイズを変えたサイドバーにも反映されます。区切り線をダブルクリックするとこの幅に戻ります。",
    ),
    (
        "p12.i6[agent.threads_sidebar.auto_open].title",
        "スレッドサイドバーを自動で開く",
    ),
    (
        "p12.i6[agent.threads_sidebar.auto_open].description",
        "既存のウィンドウでフォルダーを開いたときに、スレッドサイドバーを自動で開くかどうかを設定します。",
    ),
    (
        "p12.i25[agent.prevent_idle_sleep].title",
        "アイドル時のスリープを防ぐ",
    ),
    (
        "p12.i25[agent.prevent_idle_sleep].description",
        "エージェントのスレッドの実行中に、システムがスリープしないようにするかどうかを設定します。",
    ),
    // Added by zed-jp-enhance with the call graph panel, at the end of the
    // Panels page so that the rows above keep their keys.
    ("p7.i79.header", "呼び出しグラフパネル"),
    (
        "p7.i80[call_graph_panel.button].title",
        "呼び出しグラフパネルのボタン",
    ),
    (
        "p7.i80[call_graph_panel.button].description",
        "ステータスバーに呼び出しグラフパネルのボタンを表示します。",
    ),
    (
        "p7.i81[call_graph_panel.dock].title",
        "呼び出しグラフパネルのドック位置",
    ),
    (
        "p7.i81[call_graph_panel.dock].description",
        "呼び出しグラフパネルをドッキングする位置です。",
    ),
    (
        "p7.i82[call_graph_panel.default_width].title",
        "呼び出しグラフパネルの既定の幅",
    ),
    (
        "p7.i82[call_graph_panel.default_width].description",
        "呼び出しグラフパネルの既定の幅 (ピクセル) です。",
    ),
    (
        "p7.i83[call_graph_panel.max_children].title",
        "一覧する呼び出しの上限",
    ),
    (
        "p7.i83[call_graph_panel.max_children].description",
        "1 つの関数の呼び出し元 / 呼び出し先を、「さらに表示」の行の前にいくつまで並べるかです。",
    ),
    (
        "p7.i84[call_graph_panel.follow_cursor].title",
        "カーソルに追従",
    ),
    (
        "p7.i84[call_graph_panel.follow_cursor].description",
        "カーソルが動くたびに、カーソル位置の関数を起点にします。",
    ),
    (
        "p7.i85[call_graph_panel.sources].title",
        "呼び出しグラフの情報源",
    ),
    (
        "p7.i85[call_graph_panel.sources].description",
        "呼び出しをどこから取得するか。正確な言語サーバー、C と C++ のファイルを名前で照合する索引、またはその両方です。",
    ),
    (
        "p7.i86[call_graph_panel.excluded_callees].title",
        "除外する呼び出し先",
    ),
    (
        "p7.i86[call_graph_panel.excluded_callees].description",
        "関数の呼び出し先の一覧から外す呼び出し先 (ログやアサーションのマクロなど)。末尾の行に件数を出し、押すと表示します。",
    ),
    // zed-jp-enhance: clangd's memory settings, whose header moves the
    // languages' one down.
    ("p4.i36.header", "clangd"),
    ("p4.i37[clangd.index_mode].title", "索引のモード"),
    (
        "p4.i37[clangd.index_mode].description",
        "C / C++ の言語サーバー clangd がプロジェクトのどこまでを索引にするか。完全 (Full) は clangd の既定どおり、プロジェクト全体を背景で索引にします。軽量 (Light) は開いているファイルだけを索引にし、メモリを最も節約できます。その代わり、プロジェクト全体の参照とリネームが、開いているファイルの範囲になります (呼び出し階層と、開いていないファイルにある定義への移動も同じです)。`lsp.clangd.binary.arguments` に `--background-index` があるときは無視されます。変更すると clangd を再起動します。",
    ),
    ("p4.i38[clangd.max_workers].title", "最大ワーカー数"),
    (
        "p4.i38[clangd.max_workers].description",
        "clangd がファイルの解析と索引作りに使うスレッド数 (`-j`)。少ないほど同時に使うメモリは減りますが、索引作りは遅くなります。0 のときは clangd の既定のままです。`lsp.clangd.binary.arguments` に `-j` があるときは無視されます。変更すると clangd を再起動します。この項目はユーザー設定でのみ有効です。",
    ),
    ("p4.i39[clangd.memory_limit_mb].title", "メモリの上限 (MB)"),
    (
        "p4.i39[clangd.memory_limit_mb].description",
        "clangd のメモリ使用量 (30 秒ごとに確認) がこのメガバイト数を超えたら、clangd を再起動します。再起動は 10 分に 1 回までで、それ以上は通知だけを出します。リモートのプロジェクトでは、接続先のホストで測ります。0 のときは再起動しません。この項目はユーザー設定でのみ有効です。",
    ),
    // zed-jp-enhance: the symbol index's settings (ADR-F68), right after
    // clangd's, which move the languages' header down again.
    ("p4.i40.header", "シンボル索引"),
    ("p4.i41[symbol_index.enabled].title", "索引を作る"),
    (
        "p4.i41[symbol_index.enabled].description",
        "プロジェクトの C / C++ のファイルを名前で引く索引です。コンパイル情報が無くても (compile_commands.json は要りません)、定義への移動、関数の呼び出し元 / 呼び出し先、型の中身を、名前の一致で調べます。そのため、同じ名前の別のシンボルが結果に混ざることがあります。言語サーバーの答えがあれば、そちらが先です。自動 (Auto) は C / C++ のファイルがあるフォルダーで索引を作り、オン (On) は常に作り、オフ (Off) は作りません。既定は自動です。",
    ),
    (
        "p4.i42[symbol_index.include_ignored].title",
        "無視されたファイルも索引に入れる",
    ),
    (
        "p4.i42[symbol_index.include_ignored].description",
        "`.gitignore` で除外されたファイルも索引に入れるかどうか。入るのは、プロジェクトパネルが読み込んだ無視ファイルだけです (無視されたフォルダーは、展開したときに読み込まれます)。既定はオフです。",
    ),
    (
        "p4.i43[symbol_index.language_server_timeout_ms].title",
        "言語サーバーを待つ時間 (ミリ秒)",
    ),
    (
        "p4.i43[symbol_index.language_server_timeout_ms].description",
        "定義への移動で、言語サーバーの答えをどれだけ待ってから、索引の名前一致も候補に出すか (ミリ秒、100〜10000)。既定は 1500 です。",
    ),
    (
        "p4.i44[symbol_index.hover_type_contents].title",
        "ホバーで型の中身を表示",
    ),
    (
        "p4.i44[symbol_index.hover_type_contents].description",
        "C / C++ で名前にホバーしたとき、その型の中身 (メンバー、列挙子、マクロの定義) も、索引から名前で探して表示するかどうか。既定はオンです。",
    ),
    ("p4.i45.header", "言語"),
    (
        "p0.i26[auto_update_include_prereleases].title",
        "プレリリースを含める",
    ),
    (
        "p0.i26[auto_update_include_prereleases].description",
        "更新の確認でプレリリースも対象にします。既定ではリリースだけを対象にします。",
    ),
    // ForgeKairos: AI control of the built-in browser (ADR-F89).
    ("p14.i12.header", "AI による操作"),
    (
        "p14.i13[browser.ai_control].title",
        "AI エージェントに内蔵ブラウザーを使わせる",
    ),
    (
        "p14.i13[browser.ai_control].description",
        "オンにすると、AI エージェントが内蔵ブラウザーの AI タブを開いて操作し、Web アプリをデバッグできます。AI タブは別のブラウザープロファイルを使い、通信は許可リストの範囲に限られます (localhost は常に許可)。あなた自身のブラウザータブが AI エージェントに見えることはありません。",
    ),
];

/// Strings whose Japanese changed on purpose when the table moved to the catalog.
const CHANGED_ON_PURPOSE: &[(&str, &str)] = &[
    // Upstream reworded these, so their Japanese follows the new English.
    (
        "Show an indicator while a multi-stroke key binding is pending. If the input has a timeout, a countdown is shown and hovering pauses it. Its binding preview popover is disabled when the which-key menu is enabled.",
        "複数ストロークのキーバインドの入力中にインジケーターを表示します。入力にタイムアウトがあるときはカウントダウンを表示し、ポインターを重ねると一時停止します。Which-key メニューが有効なときは、そのキーバインドのプレビューのポップオーバーは無効になります。",
    ),
    (
        "Disable edit predictions in these language scopes, such as \"comment\" and \"string\". Use \"...\" to add scopes without repeating the inherited list.",
        "これらの言語スコープ (\"comment\" や \"string\" など) では編集予測を無効にします。\"...\" を使うと、継承した一覧を繰り返さずにスコープを追加できます。",
    ),
    // The title said 音声のテスト and the button 音声をテスト for the same
    // English; GLOSSARY.md has Test Audio = 音声のテスト.
    ("Test Audio", "音声のテスト"),
    // These name the values of their dropdown, which now show in Japanese.
    (
        "Cursor shape for insert mode. Inherit uses the editor's cursor shape.",
        "挿入モードのカーソルの形状です。「エディターと同じ」はエディターのカーソルの形状を使います。",
    ),
    (
        "Default cursor shape for the terminal (bar, block, underline, or hollow).",
        "ターミナルの既定のカーソルの形状です (縦線・ブロック・下線・中抜きのブロック)。",
    ),
    (
        "Controls line number display in the editor's gutter. \"disabled\" shows absolute line numbers, \"enabled\" shows relative line numbers for each absolute line, and \"wrapped\" shows relative line numbers for every line, absolute or wrapped.",
        "エディターのガターの行番号表示を制御します。「無効」は絶対行番号を、「有効」は各絶対行に対する相対行番号を、「折り返し行も数える」は折り返し行も含めたすべての行に対する相対行番号を表示します。",
    ),
    (
        "How thinking blocks should be displayed by default. 'Auto' fully expands during streaming, then auto-collapses when done. 'Preview' auto-expands with a height constraint during streaming. 'Always Expanded' shows full content. 'Always Collapsed' keeps them collapsed.",
        "思考ブロックを既定でどのように表示するかを設定します。「自動」はストリーミング中は完全に展開し、完了後に自動的に折りたたみます。「プレビュー」は高さを制限した状態でストリーミング中に自動展開します。「常に展開」は常に全体を表示します。「常に折りたたむ」は常に折りたたんだままにします。",
    ),
    (
        "Off: Do not request semantic tokens from language servers.\nCombined: Use LSP semantic tokens together with tree-sitter highlighting.\nFull: Use LSP semantic tokens exclusively, replacing tree-sitter highlighting.",
        "オフ: 言語サーバーにセマンティックトークンを要求しません。\n併用: LSP のセマンティックトークンを tree-sitter のハイライトと併せて使います。\nLSP のみ: tree-sitter のハイライトの代わりに、LSP のセマンティックトークンだけを使います。",
    ),
    // Says 索引 for the index, as the settings schema's description of the same setting does.
    (
        "Maximum directory depth to eagerly index outside of git repositories; contents of directories at this depth or deeper are indexed on demand. Repositories rooted shallower than this depth are always indexed fully. In projects that are not rooted at a git repository, repositories directly inside a root folder activate their git features immediately; deeper ones activate on first use. 0 means no limit and activates all git repositories immediately",
        "git リポジトリの外側で先読みの索引を作るディレクトリの最大の深さです。この深さ以上のディレクトリの内容は、必要になった時点で索引に入ります。この深さより浅い位置にあるリポジトリは常に完全に索引に入ります。git リポジトリを起点としないプロジェクトでは、ルートフォルダーの直下にあるリポジトリは即座に git 機能が有効になり、それより深いものは最初に使われた時点で有効になります。0 は無制限を意味し、すべての git リポジトリを即座に有効にします",
    ),
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

/// This build sends nothing to Zed Industries, so the window offers no switch
/// for its telemetry, crash reports or agent ratings.
#[gpui::test]
fn the_table_offers_no_switch_for_sending_data_to_zed(cx: &mut TestAppContext) {
    let strings = table_strings_in_test_app(cx);
    let offered: Vec<&str> = strings
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| {
            [
                "[telemetry.metrics]",
                "[telemetry.diagnostics]",
                "[agent.enable_feedback]",
            ]
            .iter()
            .any(|json_path| key.contains(json_path))
        })
        .collect();
    assert!(offered.is_empty(), "still offered: {offered:?}");
    assert!(
        strings
            .iter()
            .any(|(key, _)| key.contains("[telemetry.anthropic_retention]")),
        "the rest of the Privacy section stays"
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
            "ClangdIndexMode",
            settings::ClangdIndexMode::VARIANTS,
            true,
            vec!["完全", "軽量"],
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
                "ForgeKairos (既定)",
                "VS Code",
                "JetBrains",
                "Sublime Text",
                "Atom",
                "TextMate",
                "Emacs (ベータ)",
                "Cursor",
                "なし",
            ],
        ),
        // zed-jp-enhance: the call graph panel's sources (ADR-F68).
        (
            "CallGraphSources",
            settings::CallGraphSources::VARIANTS,
            true,
            vec!["すべて", "言語サーバーのみ", "名前一致のみ"],
        ),
        // zed-jp-enhance: when the symbol index is built (ADR-F68).
        (
            "SymbolIndexEnabled",
            settings::SymbolIndexEnabled::VARIANTS,
            true,
            vec!["自動", "オン", "オフ"],
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
        "Cursor"
    );
}

#[test]
fn the_base_keymap_dropdown_names_the_keymaps_as_onboarding_does() {
    let english = i18n_catalog::test_support::bundled_catalog("en");
    {
        let _english = LocaleOverride::new(Some(english));
        assert_eq!(
            dropdown_display("BaseKeymapContent", "Zed", false),
            "ForgeKairos (Default)"
        );
        assert_eq!(
            dropdown_display("BaseKeymapContent", "VSCode", false),
            "VS Code"
        );
        assert_eq!(
            dropdown_display("BaseKeymapContent", "Emacs", false),
            "Emacs (beta)"
        );
        assert_eq!(dropdown_display("BaseKeymapContent", "None", false), "None");
    }
    for catalog in [english, japanese()] {
        let _locale = LocaleOverride::new(Some(catalog));
        let onboarding: Vec<&str> = settings::BaseKeymap::names().collect();
        let labels = <settings::BaseKeymapContent as strum::VariantNames>::VARIANTS;
        let variants = <settings::BaseKeymapContent as strum::VariantArray>::VARIANTS;
        let mut offered_by_onboarding = 0;
        for (label, variant) in labels.iter().zip(variants) {
            let keymap = settings::BaseKeymap::from(*variant);
            if settings::BaseKeymap::OPTIONS
                .iter()
                .all(|(_, value)| *value != keymap)
            {
                continue;
            }
            offered_by_onboarding += 1;
            let displayed = dropdown_display("BaseKeymapContent", label, false);
            assert!(
                onboarding.contains(&displayed.as_str()),
                "{label}: the window shows {displayed:?}, onboarding {onboarding:?}"
            );
        }
        assert_eq!(offered_by_onboarding, settings::BaseKeymap::OPTIONS.len());
    }
    // The label changes, the value written to settings.json does not.
    assert_eq!(
        serde_json::to_value(settings::BaseKeymapContent::Zed).ok(),
        Some(serde_json::json!("Zed"))
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
    // The same English reads as each enum's own entry says.
    assert_eq!(dropdown_display("PanelTabBar", "Never", true), "表示しない");
    assert_eq!(
        dropdown_display("UseSystemClipboard", "Never", true),
        "使わない"
    );
    assert_eq!(dropdown_display("SoftWrap", "None", true), "折り返さない");
    assert_eq!(dropdown_display("BaseKeymapContent", "None", false), "なし");
    // A label no entry of its enum translates stays English, even where
    // another enum translates the same English.
    assert_eq!(dropdown_display("DockSide", "Never", true), "Never");
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
        CallGraphSources,
        ClangdIndexMode,
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
        OnNewWindow,
        OpenResultsIn,
        PaneSplitDirectionHorizontal,
        PaneSplitDirectionVertical,
        PanelTabBar,
        PlaySoundWhenAgentDone,
        ProjectPanelEntrySpacing,
        ProjectPanelSortMode,
        ProjectPanelSortOrder,
        ProjectPanelTitleTooltipDelayDiscriminants,
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
        SymbolIndexEnabled,
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

/// Dropdown labels that read the same in Japanese (names and notations), as
/// `(enum, English label as displayed)`.
const DROPDOWN_LABELS_KEPT_IN_ENGLISH: &[(&str, &str)] = &[
    ("BaseKeymapContent", "Atom"),
    ("BaseKeymapContent", "Cursor"),
    ("BaseKeymapContent", "JetBrains"),
    ("BaseKeymapContent", "Sublime Text"),
    ("BaseKeymapContent", "TextMate"),
    ("BaseKeymapContent", "VSCode"),
    ("MultiCursorModifier", "Alt"),
];

/// Every setting drawn as a dropdown, as the enum's name and whether the
/// setting title-cases its labels. One enum can be drawn both ways.
fn dropdowns_on_screen(cx: &App) -> BTreeSet<(&'static str, bool)> {
    fn add(item: &SettingItem, output: &mut BTreeSet<(&'static str, bool)>) {
        let type_name = crate::AnySettingField::type_name(item.field.as_ref());
        let enum_name = type_name.rsplit("::").next().unwrap_or(type_name);
        let title_case = item
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.should_do_titlecase)
            .unwrap_or(true);
        output.insert((enum_name, title_case));
    }

    fn walk(items: &[SettingsPageItem], output: &mut BTreeSet<(&'static str, bool)>) {
        for item in items {
            match item {
                SettingsPageItem::SettingItem(item) => add(item, output),
                SettingsPageItem::DynamicItem(DynamicItem {
                    discriminant,
                    fields,
                    ..
                }) => {
                    add(discriminant, output);
                    for field in fields.iter().flatten() {
                        add(field, output);
                    }
                }
                _ => {}
            }
        }
    }

    let dropdown_names: HashSet<&str> = all_dropdowns().into_iter().map(|(name, _)| name).collect();
    let mut output = BTreeSet::new();
    for page in page_data::settings_data(cx) {
        walk(&page.items, &mut output);
    }
    walk(&page_data::language_sub_page_items(), &mut output);
    output.retain(|(name, _)| dropdown_names.contains(name));
    output
}

/// The English of each label of each dropdown on screen, as `(enum, English
/// as displayed)`.
fn dropdown_labels_on_screen(cx: &mut TestAppContext) -> Vec<(&'static str, String)> {
    let on_screen = cx.update(|cx| {
        register_settings(cx);
        let app_state = AppState::test(cx);
        AppState::set_global(app_state, cx);
        dropdowns_on_screen(cx)
    });
    let labels_by_name: HashMap<&str, &[&str]> = all_dropdowns().into_iter().collect();
    let mut output = Vec::new();
    for (name, title_case) in on_screen {
        for label in labels_by_name[name] {
            let english = if title_case {
                label.to_title_case()
            } else {
                label.to_string()
            };
            output.push((name, english));
        }
    }
    output
}

/// A dropdown in the settings window shows its value in the display language:
/// every label has a translation under its enum's context.
#[gpui::test]
fn every_dropdown_label_on_screen_is_translated(cx: &mut TestAppContext) {
    let labels = dropdown_labels_on_screen(cx);
    assert!(labels.len() > 300, "only {} labels found", labels.len());
    // `english` is already cased as the setting cases it. The English UI is
    // set explicitly: under a catalog another test left installed, a label
    // kept in English would be compared with its own translation.
    let english_displayed: Vec<String> = {
        let _english = LocaleOverride::new(Some(i18n_catalog::test_support::bundled_catalog("en")));
        labels
            .iter()
            .map(|(name, english)| dropdown_display(name, english, false))
            .collect()
    };
    let _japanese = LocaleOverride::new(Some(japanese()));
    let mut untranslated = Vec::new();
    for ((name, english), english_displayed) in labels.iter().zip(&english_displayed) {
        let displayed = dropdown_display(name, english, false);
        let kept_in_english = DROPDOWN_LABELS_KEPT_IN_ENGLISH.contains(&(name, english.as_str()));
        let as_expected = if kept_in_english {
            displayed == *english_displayed
        } else {
            has_japanese(&displayed)
        };
        if !as_expected {
            untranslated.push(format!(
                "msgctxt \"{name}\"\nmsgid \"{english}\"\nshows \"{displayed}\"\n"
            ));
        }
    }
    assert!(
        untranslated.is_empty(),
        "{} dropdown labels are not translated:\n{}",
        untranslated.len(),
        untranslated.join("\n")
    );

    let on_screen: HashSet<(&str, &str)> = labels
        .iter()
        .map(|(name, english)| (*name, english.as_str()))
        .collect();
    let mut unused_kept: Vec<_> = DROPDOWN_LABELS_KEPT_IN_ENGLISH
        .iter()
        .filter(|entry| !on_screen.contains(entry))
        .collect();
    unused_kept.sort();
    assert!(unused_kept.is_empty(), "not on screen: {unused_kept:?}");
}

/// Every dropdown entry of `_settings.po` translates a label some dropdown shows.
#[gpui::test]
fn every_dropdown_entry_translates_a_label_on_screen(cx: &mut TestAppContext) {
    let labels = dropdown_labels_on_screen(cx);
    let on_screen: HashSet<(&str, &str)> = labels
        .iter()
        .map(|(name, english)| (*name, english.as_str()))
        .collect();
    let mut stale: Vec<String> = settings_po()
        .entries
        .iter()
        .filter_map(|entry| {
            let context = entry.context.as_deref()?;
            (context != i18n::context::SETTINGS
                && !on_screen.contains(&(context, entry.msgid.as_str())))
            .then(|| format!("{context}: {:?}", entry.msgid))
        })
        .collect();
    stale.sort();
    assert!(stale.is_empty(), "no dropdown shows {stale:#?}");
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

/// Every row the search leaves on screen, as `page/header` for section headers
/// and `page/json path` (or `page/title`) for the rest, and the navigation entry
/// it picked as the best match.
fn search_results(
    settings_window: gpui::WindowHandle<SettingsWindow>,
    query: &str,
    cx: &mut gpui::VisualTestContext,
) -> (BTreeSet<String>, &'static str) {
    search(settings_window, query, cx);
    settings_window
        .read_with(cx, |settings_window, _| {
            let mut rows = BTreeSet::new();
            for (page, shown) in settings_window
                .pages
                .iter()
                .zip(&settings_window.filter_table)
            {
                for (item, shown) in page.items.iter().zip(shown) {
                    if !shown {
                        continue;
                    }
                    let name = match item {
                        SettingsPageItem::SectionHeader(header) => format!("# {header}"),
                        SettingsPageItem::SettingItem(item)
                        | SettingsPageItem::DynamicItem(DynamicItem {
                            discriminant: item, ..
                        }) => item.field.json_path().unwrap_or(item.title).to_string(),
                        SettingsPageItem::SubPageLink(link) => link.title.to_string(),
                        SettingsPageItem::ActionLink(link) => link.title.to_string(),
                    };
                    rows.insert(format!("{}/{name}", page.title));
                }
            }
            let best_match = settings_window
                .navbar_entries
                .get(settings_window.navbar_entry)
                .map_or("", |entry| entry.title);
            (rows, best_match)
        })
        .expect("the settings window should be open")
}

#[gpui::test]
async fn japanese_search_finds_words_in_the_middle_of_titles_and_descriptions(
    cx: &mut TestAppContext,
) {
    let _japanese = LocaleOverride::new(Some(japanese()));
    let (settings_window, cx) = open_settings_window(cx, Box::new(zed_actions::OpenSettings)).await;

    // Japanese is not written with spaces, so each of these is in the middle of
    // a run the index holds as one word.
    for (query, expected_rows, expected_best_match) in [
        (
            // The header "リモートターミナルの切断対策" also finds the rows below it.
            "切断",
            &[
                "Terminal/# Remote Terminal Persistence",
                "Terminal/terminal.remote_persistence.detached_timeout_hours",
            ][..],
            "Remote Terminal Persistence",
        ),
        (
            // "無視されたファイルも索引に入れる"
            "索引",
            &["Languages & Tools/symbol_index.include_ignored"][..],
            "Symbol Index",
        ),
        (
            // "保存時に末尾へ改行を確保"
            "改行",
            &["Editor/languages.$(language).ensure_final_newline_on_save"][..],
            // No section header says it, so the first page that has a match.
            "Editor",
        ),
        (
            // "...のシェルをリモートの Zed サーバーに預けます。" in a description.
            "預け",
            &["Terminal/terminal.remote_persistence.enabled"][..],
            "Terminal",
        ),
    ] {
        let (rows, best_match) = search_results(settings_window, query, cx);
        for expected in expected_rows {
            assert!(rows.contains(*expected), "{query}: {expected} in {rows:#?}");
        }
        assert!(
            !rows.iter().any(|row| row.ends_with("/vim_mode")),
            "{query}: the search still filters: {rows:#?}"
        );
        assert_eq!(best_match, expected_best_match, "{query}");
    }
}

/// Matching inside words is only for queries that are not ASCII: what English
/// finds, and the page it picks, stay as they were.
#[gpui::test]
async fn english_search_still_matches_the_start_of_words(cx: &mut TestAppContext) {
    let _japanese = LocaleOverride::new(Some(japanese()));
    let (settings_window, cx) = open_settings_window(cx, Box::new(zed_actions::OpenSettings)).await;
    // Taken from the search before it matched inside words; the counts move
    // with the table, but "ine" and "mat" would find more if ASCII matched
    // inside words ("line", "format").
    for (query, expected_count, expected_best_match) in [
        ("line", 57, "Appearance"),
        ("font size", 15, "Font"),
        ("tab", 43, "Preview Tabs"),
        ("index", 12, "Symbol Index"),
        ("save", 18, "Auto Save"),
        ("vim", 20, "Vim"),
        ("mat", 21, "General"),
        ("ine", 0, "General"),
        ("new", 25, "General"),
    ] {
        let (rows, best_match) = search_results(settings_window, query, cx);
        assert_eq!(
            (rows.len(), best_match),
            (expected_count, expected_best_match),
            "{query}: {rows:#?}"
        );
    }
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

    for (language, general_page, japanese_finds, search_placeholder, window_title) in [
        ("ja", "全般", true, "設定を検索…", "ForgeKairos — 設定"),
        (
            "en",
            "General",
            false,
            "Search settings…",
            "ForgeKairos — Settings",
        ),
        ("ja", "全般", true, "設定を検索…", "ForgeKairos — 設定"),
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
        let placeholder = settings_window
            .update(cx, |settings_window, _, cx| {
                settings_window
                    .search_bar
                    .update(cx, |editor, cx| editor.placeholder_text(cx))
            })
            .expect("the settings window should be open");
        assert_eq!(
            placeholder.as_deref(),
            Some(search_placeholder),
            "{language}: the search bar's placeholder"
        );
        assert_eq!(
            gpui::VisualTestContext::from_window(settings_window.into(), cx)
                .window_title()
                .as_deref(),
            Some(window_title),
            "{language}: the window title"
        );
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
