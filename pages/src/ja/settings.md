# 設定の一覧

[English](../settings.md)

ForgeKairos が Zed に足した設定と、既定値を変えた Zed の設定の一覧。値は同梱の
`assets/settings/default.json` のもの。ほかの設定は [Zed の設定の文書](https://zed.dev/docs/configuring-zed) を見る。

外部通信の許可リストは `settings.json` の設定ではなく、`network_allowlist.json` に置く
([外部通信の統制](network.md) を参照)。

## ForgeKairos が足した設定

### 全般とレイアウト

| キー | 既定値 | 説明 |
|---|---|---|
| `ui_language` | `"system"` | 表示言語。`"system"`、`"en"`、または `"ja"` などの BCP 47 タグ。 |
| `default_stacked_dock` | `"right"` | プロジェクトを初めて開いたときに 2 つのパネルを並べるドック (`left`、`right`、`bottom`)。 |
| `default_stacked_panel` | `"agent"` | そのドックの 1 枚目のパネル (`none`、`agent`、`browser`、`terminal`、`project`、`git`、`outline`、`debug`)。 |
| `default_stacked_secondary_panel` | `"browser"` | そのドックの 2 枚目のパネル (選択肢は同じ)。 |
| `tab_bar.show_buttons_on_inactive_panes` | `true` | フォーカスの無いペインにもタブ行のボタンを出す。 |
| `acknowledged_external_ai_providers` | `[]` | 学習リスクの警告を承諾した AI プロバイダー。設定 UI が書く。 |

### エディターとファイル

| キー | 既定値 | 説明 |
|---|---|---|
| `toolbar.markdown_format_bar` | `true` | Markdown を開いている間、エディターのツールバーの下に整形の行を出す。 |
| `hard_break_on_newline` | `false` (Markdown は `true`) | Enter を押したとき、先に Markdown の強制改行 (行末のスペース 2 つ) を足す。 |
| `markdown_preview.soft_break_as_hard_break` | `true` | Markdown のプレビューで 1 つの改行を改行として表示する。 |
| `tabular_data.open_as_grid` | `true` | CSV と TSV をグリッドで開く。 |
| `tabular_data.multiline_cells` | `true` | 改行を含むセルを、行の高さを伸ばして全部表示する。 |
| `git.status_untracked_files` | `"all"` | git status が追跡していないファイルをどこまで報告するか (`all`、`normal`、`no`)。SSH のリモートでも使う。 |

### C と C++

| キー | 既定値 | 説明 |
|---|---|---|
| `symbol_index.enabled` | `"auto"` | 名前ベースのシンボル索引を作る条件: `auto` (C か C++ のファイルがあるとき)、`on`、`off`。 |
| `symbol_index.include_ignored` | `false` | `.gitignore` で除外したファイルも索引に入れる (プロジェクトパネルが読み込んだものだけ)。 |
| `symbol_index.language_server_timeout_ms` | `1500` | 「定義へ移動」が言語サーバーを待つ時間。過ぎると名前一致も出す。 |
| `symbol_index.hover_type_contents` | `true` | ホバーに型の中身 (構造体のメンバー、列挙の定数、マクロの定義) を出す。 |
| `clangd.index_mode` | `"full"` | `full`: clangd がプロジェクト全体を裏で索引する。`light`: 開いたファイルだけ。 |
| `clangd.max_workers` | `0` | clangd のスレッド数 (`-j`)。`0` は clangd の既定。 |
| `clangd.memory_limit_mb` | `0` | clangd のメモリがこの MB を超えたら再起動する (30 秒ごとに確認、10 分に 1 回まで)。`0` は再起動しない。 |
| `call_graph_panel.button` | `true` | ステータスバーに呼び出しグラフのボタンを出す。 |
| `call_graph_panel.dock` | `"right"` | 呼び出しグラフのパネルのドック (`left`、`right`)。 |
| `call_graph_panel.default_width` | `320` | 呼び出しグラフのパネルの既定の幅。 |
| `call_graph_panel.max_children` | `200` | 「さらに表示」の行の前に並べる呼び出し元・呼び出し先の数。 |
| `call_graph_panel.follow_cursor` | `false` | カーソルが動くたびに、カーソル位置の関数を根にする。 |
| `call_graph_panel.sources` | `"all"` | 呼び出しの出どころ: `all`、`language_server`、`name_match`。 |
| `call_graph_panel.excluded_callees` | `["printk", "pr_info", …]` | 呼び出し先から外す関数 (最後の行で件数を出す)。 |

### AI とエージェント

| キー | 既定値 | 説明 |
|---|---|---|
| `agent.pause_worktree_code_after_agent_writes` | `true` | エージェントが書き込んだプロジェクトのフォルダーは、変更を確認して再開するまで、そのフォルダーのコード (フォーマッター、リンター、インタープリター、ビルドスクリプト) を実行しない。ユーザー設定のファイルからだけ読む。 |
| `agent.language_servers_allowed_while_paused` | `[]` | フォルダーの停止中も動かしてよい拡張機能の言語サーバー。ユーザー設定のファイルからだけ読む。 |
| `agent.starts_open` | `false` | 起動時にエージェントパネルを開く。 |
| `agent.sidebar_enabled` | `false` | ウィンドウ全体のスレッドのサイドバーを使えるようにする。 |
| `agent.panel_thread_history` | `true` | エージェントパネルの中にスレッドの履歴の一覧を出す。 |
| `agent.panel_tab_bar` | `"auto"` | エージェントパネル自身のタブ行を出す条件 (`auto`、`always`、`never`)。 |
| `agent.auto_compact_min_context_window` | `80000` | 自動の要約を行うモデルの文脈長 (トークン) の下限。8000 未満は 8000 になる。 |
| `agent.attach_active_file` | `false` | 送るメッセージごとに、開いているエディター (選択範囲、無ければファイル全体) を添付する。 |
| `agent.context_attachments.inline_max_bytes` | `16384` | これより大きいファイルはアウトラインで添付する。 |
| `agent.context_attachments.message_budget_bytes` | `65536` | 1 メッセージの添付の合計がこれを超えると、大きいものからアウトラインか参照に減らす。 |
| `agent.context_attachments.outline_max_bytes` | `8192` | 1 ファイルのアウトラインの上限。 |
| `agent.context_attachments.fetch_max_bytes` | `32768` | 取得した Web ページを添付する量の上限。 |
| `agent.context_attachments.diagnostics_max_files` | `50` | 診断を添付するファイル数の上限。 |
| `agent.system_prompt_repo_map_bytes` | `0` | エージェントのシステムプロンプトに入れるリポジトリの地図のバイト数。`0` で無効、上限 32768。 |
| `agent.idle_disconnect_minutes` | `5` | 外部エージェントのスレッドが無くなってから接続を保つ分数。`0` はウィンドウを閉じるまで保つ。 |
| `agent.claude_code_run_outside_sandbox` | `false` | 「Claude Code を起動」で、確認せずに常にサンドボックスの外で起動する。ユーザー設定のファイルからだけ読む。 |
| `ide_server.enabled` | `false` | ForgeKairos を Claude Code CLI 向けの loopback の IDE サーバーとして公開する。 |
| `ide_server.assist_tools` | `true` | エージェントに IDE のツール (ハイライト、注釈、ツアー、定義、参照、シンボル検索) を MCP サーバー名 `zed` で渡す。 |
| `ide_server.share_active_file` | `true` | 何も選択していなくても、開いているファイルを接続中の Claude Code CLI に知らせる。 |
| `ide_server.share_task_output` | `true` | 最後に失敗したタスクの出力の末尾 (4 KB まで) をエージェントが読めるようにする。 |

エージェントのプロファイルには **編集のみ** (`agent.profiles.edit`) が加わる。ファイルを読み、編集するが、
コマンドを実行せず、ネットワークにも出ない。パスの削除・移動・コピーも含めない。
`agent.tool_permissions.default` には 4 つ目の値 `"auto"` がある ([AI](ai.md) を参照)。

### ターミナル

| キー | 既定値 | 説明 |
|---|---|---|
| `terminal.macro_picker_folders_collapsed` | `false` | マクロのピッカー (「マクロを実行…」) をすべてのフォルダーを畳んで開く。 |
| `terminal.session_log.auto` | `false` | 新しく開いたターミナルのログを自動で取る。 |
| `terminal.session_log.directory` | `""` | ログの保存先 (`~` を展開する)。空ならログフォルダーの下の `terminal-sessions`。 |
| `terminal.session_log.file_name_template` | `"{date}_{time}_{title}.log"` | ログのファイル名。`{date}` `{time}` `{title}` `{host}` `{pid}` を展開する。 |
| `terminal.session_log.strip_control_sequences` | `true` | ANSI / VT のエスケープシーケンスを除いて、平文で保存する。 |
| `terminal.session_log.timestamps` | `false` | 各行の先頭に `[YYYY-MM-DD HH:MM:SS] ` を付ける。 |
| `terminal.remote_persistence.enabled` | `true` | SSH のターミナルを切断をまたいで生かす (Windows 以外のリモート)。 |
| `terminal.remote_persistence.detached_timeout_hours` | `24` | どのターミナルも繋がっていない状態がこの時間続いたら、リモートのシェルを終える。`0` は終えない。 |

### 内蔵ブラウザー

| キー | 既定値 | 説明 |
|---|---|---|
| `browser.button` | `true` | ステータスバーにブラウザーのボタンを出す。 |
| `browser.dock` | `"right"` | ブラウザーのパネルのドック (`left`、`right`、`bottom`)。 |
| `browser.starts_open` | `false` | 起動時にブラウザーのパネルを開く。 |
| `browser.default_width` | `480` | 左右のドックでの既定の幅。 |
| `browser.default_height` | `320` | 下のドックでの既定の高さ。 |
| `browser.home_url` | `"https://m365.cloud.microsoft/chat"` | 開始ページ。 |
| `browser.search_url` | `"https://www.google.com/search?q={query}"` | アドレスバーの入力が URL でないときに使う URL。 |
| `browser.copilot_url` | `"https://github.com/copilot"` | 選択した文字列について「Copilot に聞く」で開く URL。 |
| `browser.use_egress_proxy` | `false` | 内蔵ブラウザーの通信にも許可リストを適用する。 |

ブラウザーのパネルは Windows では Microsoft Edge WebView2 を使う。ほかの OS では「対応していない」旨と、
外部のブラウザーで開く手段だけを出す。

### リモートと開発ツール

| キー | 既定値 | 説明 |
|---|---|---|
| `remote.relay_http_through_client` | `true` | リモートのダウンロード (言語サーバー、node、拡張機能、エージェント) をこの PC 経由で行う。 |
| `dev_tools.suggest_missing` | `true` | 足りない言語サーバーとフォーマッターを通知し、データフォルダーに入れるボタンを出す。 |
| `dev_tools.ignored` | `[]` | 通知しないツール (例: `"clangd"`)。 |

## 既定値を変えた Zed の設定

| キー | ForgeKairos | Zed |
|---|---|---|
| `agent.enabled` | `false` | `true` |
| `agent.dock` | `"right"` | `"left"` |
| `agent.default_model` | 未設定 (認証済みの最初のプロバイダーを使う) | `zed.dev` / `claude-sonnet-4` |
| `edit_predictions.provider` | `"none"` | `"zed"` |
| `edit_predictions.allow_data_collection` | `"no"` | `"default"` |
| `telemetry.diagnostics`、`telemetry.metrics` | `false` | `true` |
| `auto_update` | `false` | `true` |
| `auto_install_extensions` | `{}` | `{ "html": true }` |
| `project_panel.dock`、`outline_panel.dock`、`git_panel.dock`、`collaboration_panel.dock` | `"left"` | `"right"` |
| `project_panel.git_status_indicator` | `true` | `false` |
| `title_bar.show_menus` | `true` | `false` |
| `show_whitespaces` | `"all"` | `"selection"` |
| `terminal.starts_open` | `true` | `false` |
| `markdown_preview.limit_content_width` | `false` | `true` |
| `buffer_font_fallbacks` | `["BIZ UDGothic", "MS Gothic", "Yu Gothic UI"]` | `null` |
| `ui_font_fallbacks` | `["Yu Gothic UI", "Meiryo"]` | `null` |
| `languages.C.formatter`、`languages.C++.formatter` | GNU を基にした書式の clang-format | 未設定 |
