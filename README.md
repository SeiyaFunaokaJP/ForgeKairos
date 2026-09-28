# zed-jp-enhance

[Zed](https://zed.dev) のフォーク (upstream のリリースを `git merge` で 1 版ずつ取り込む。現在 v1.18.1 ベース)。
日本語 UI、ネットワーク統制 (egress deny-by-default)、ローカル LLM 第一の AI 方針、
そして VSCode が切り捨てた古い Linux への SSH リモート対応を自前改造で足した Windows 向け IDE。

## 特徴

### 日本語化

- **UI の全面日本語化** — メニューバー / 設定 GUI / コマンドパレット (日英どちらのクエリでもヒット) /
  git・エージェント・診断など全出荷パネル / エディタ周辺 / 相対日時 / UI に出るエラー文言まで

### ネットワーク統制

- **外部通信は既定で全遮断** (deny-by-default) — 許可したホストとだけ通信する。
  許可リストは設定 GUI (設定 → ネットワーク) か `%APPDATA%\Zed\network_allowlist.json` で管理。
  リダイレクトも 1 hop ずつ再検証。組み込み許可は拡張マーケットプレイス /
  ACP レジストリの更新確認 / 本体の更新確認の 3 つ (いずれもパス単位) のみ
- **AI エージェントの子プロセスも統制内** — ACP エージェント / MCP サーバー / npm
  (エージェントのインストール経路) に loopback プロキシを注入し、その外部通信も
  許可リストで判定する。SSH リモートで実行されるエージェント / MCP にも同じ統制が
  リモート側 loopback で効く (Linux / WSL リモート)
- **サンドボックスのネットワークも許可リストの入れ子** — コマンド実行サンドボックスの
  許可ドメインは、IDE 全体の許可リストの範囲内でのみ有効
- **テレメトリ / コラボ (通話・画面共有) は無効** — collab / LiveKit はビルドグラフごと除去済み。
  **本体の更新はこのリポジトリの GitHub Releases だけ**を見る (更新確認のみ組み込み許可、
  ダウンロードは明示同意で許可リストに追加)

### AI (ローカル第一)

- **AI 機能は既定ですべてオフ** — 使う人が明示的に有効化する。外部 AI プロバイダー
  (クラウド API) は**学習リスクの警告 + 承諾チェック**を経ないと設定できず、
  承諾フローから必要ホストを許可リストへワンクリック追加できる。
  ローカル LLM (Ollama / LM Studio / OpenAI 互換。モデル一覧の自動取得・API キーなし接続対応) は警告なしで使える
- **MCP 接続の GUI** — 設定 → AI → MCP サーバーで、状態のライブ表示 / 接続テスト /
  クライアントシークレット入力 / プロキシ適用バッジ / 実行ホスト
  (この PC / SSH 接続先) の選択まで完結
- **サブスクリプションで使う外部エージェント (ACP)** — Claude Code (Claude Pro/Max) や
  Google Antigravity をエージェントパネルから利用可能。ACP レジストリからの
  インストール / 更新も IDE 内から統制付き (必要ホストの同意 → 許可リスト追加) で行える

### SSH リモート開発

- **musl 静的リンクの remote_server を同梱** — 接続先の glibc に依存しないため、
  **VSCode (glibc 2.28+ 必須) が接続できない古い Linux (Ubuntu 18.04 で実機確認済み) にも入れる**。
  ビルド時に成果物を `zed.exe` の隣へ自動生成・配置し、接続時のユーザー操作はゼロ

### 編集機能の追加

- **Markdown 入力補助** — 引用/リスト/番号/タスクの Enter 自動継続、太字・斜体などの
  整形アクション 14 本 (`Ctrl+B` / `Ctrl+I` / `Ctrl+Shift+X` は Markdown 内のみ)、
  専用整形バー (タブ行の下)、Enter で hard break (行末スペース 2 つ) を補うオプション
- **Markdown → PDF** 書き出し (Edge headless)。ツールバー / プレビュー / タブ右クリックから
- プレビューは**分割表示が既定** (alt+クリックで同一ペイン)
- エージェント用 **repo map** ツール、**C / C++ フォーマッタ既定** (clang-format を同梱)
- **VSCode ライクな既定** — プロジェクトパネル左 / AI チャット右 / メニューバー常時表示 /
  ターミナル Ctrl+F 検索 / タイトルバーに AI チャット開閉ボタン

## ビルド (Windows)

前提: VS 2022 (C++ ワークロード + Spectre 緩和ライブラリ + Windows 11 SDK 26100)、rustup。

```powershell
powershell -NoProfile -File script\build-dev.ps1        # zed.exe / cli.exe / remote_server 同梱物
powershell -NoProfile -File script\build-dev.ps1 -Run   # ビルド後そのまま起動
```

スクリプトは CMake (VS 同梱) の PATH 補完、ディスク残量チェック、SSH リモート用
remote_server (musl) のクロスビルドと同梱配置 (zig を winget で自動調達) まで行う。
**必ず `-j 8` を付ける** (無制限並列は rustc がメモリ枯渇で落ちる)。
ディスクが埋まったら `-FreeDisk` スイッチ。

## 起動

- 成果物は `target\debug\zed.exe`。**エクスプローラーからのダブルクリック
  (または `Start-Process`) で起動**する
- コンソールから `zed <パス>` のように使うには `cli.exe` が `zed.exe` の隣に必要
  (`build-dev.ps1` が一緒にビルドする)
- C / C++ のフォーマットに使う `clang-format.exe` は `build-dev.ps1` が
  `target\debug\bin\` へ置く (`-ClangFormatPath` で入手元を指定 / `-SkipClangFormat` で省略)。
  手動で `zed.exe` の隣 (または `bin\`) に置くか PATH に通してもよい

### 初回起動時の状態 (意図した既定)

| 項目 | 既定 | 変えたいとき |
|---|---|---|
| UI | 日本語 (固定) | — |
| 外部通信 | 全遮断 (組み込み許可 3 つのみ) | 設定 → ネットワーク |
| AI (エージェント / インライン補助) | 無効 | 設定 → AI → 「エージェントを有効にする」 |
| 既定モデル | 未設定 (有効化後、認証済みプロバイダーから自動選択) | 設定 → AI → LLM プロバイダー |
| 編集予測 (インライン補完) | 無効 (`none`) | 設定 → AI → 編集予測 |
| 外部 AI プロバイダー | 警告 + 承諾チェックを経て有効化 | 各プロバイダーの設定画面 |
| テレメトリ | 無効 | (推奨: そのまま) |
| 自動更新 | 無効 (手動確認は可。接続先はこのリポジトリの Releases のみ) | 設定 → 一般 |

## upstream との関係

- fork 元: [zed-industries/zed](https://github.com/zed-industries/zed) v1.17.2。
  upstream のリリースタグを `git merge` で 1 版ずつ取り込む (rebase はしない。ADR-F63)。現在 v1.18.1 ベース
- upstream の開発規約 (`.rules`、`./script/clippy`、GPUI イディオム) は引き続き遵守する
- upstream 由来のドキュメント (`docs/src/`、`CONTRIBUTING.md` 等) はそのまま残っているが、
  フォーク独自機能の記述は本 README が正

## ライセンス

upstream (Zed) のライセンスを継承する: 主に **GPL-3.0-or-later**、一部 Apache-2.0
(各クレートの表記に従う。`LICENSE-GPL` / `LICENSE-APACHE` 参照)。
C / C++ の既定フォーマッタが使う **clang-format** バイナリは LLVM プロジェクトの一部で、
**Apache-2.0 with LLVM Exceptions**。Windows インストーラーはこれを `{app}\bin\clang-format.exe`
として同梱し、ライセンス全文を `{app}\licenses\LLVM-LICENSE.txt` に配置する
(リポジトリ上の実体は `crates/zed/resources/windows/licenses/LLVM-LICENSE.txt`)。
リモートサンドボックス (ADR-F18) の `slirp4netns` バイナリを同梱して配布する場合は、
その **GPLv2+** ライセンス (本フォークの GPL-3 と両立) を、内部の **libslirp** (MIT/BSD)
の表記とともに配布物に含めること。
