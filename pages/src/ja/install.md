# インストールと更新

[English](../install.md)

## Windows のインストーラー

ForgeKairos は Windows のインストーラー `forgekairos-windows-<arch>-setup.exe` で配布する。入手先は
リポジトリの [GitHub Releases](https://github.com/SeiyaFunaokaJP/ForgeKairos/releases)。

- 今のユーザー向けに、管理者の権限なしで、ユーザーのプログラムのフォルダーの下の `ForgeKairos` に入れる。
- 64 ビットの Windows 10 バージョン 1709 以降が要る。
- インストーラーで選べるもの: デスクトップのアイコン (既定でオフ)、エクスプローラーの右クリックのメニューの
  **ForgeKairos で開く** (ファイルとフォルダー)、ファイルの関連付け、コマンドラインの道具を `PATH` に足す。
- アプリは `ForgeKairos.exe`、コマンドラインの道具は `bin\forgekairos.exe`。
- インストーラーは clang-format (C と C++ の整形に使う) と、Linux のホスト向けのリモートサーバーを同梱する。

Windows 11 では、**ForgeKairos で開く** が「その他のオプションを確認」の下に出ることがある。

## 更新

自動更新はオフ (`auto_update` は `false`)。更新を確かめるには、アプリのメニューの **更新を確認**
(またはコマンドパレットの `auto update: check`) を使う。ForgeKairos はこのリポジトリの GitHub Releases
だけを見て、Zed の更新は見ない。リリースノートは GitHub のリリースのページを開く。

既定では正式なリリースだけを対象にする。プレリリースも対象にするには、設定 → 全般 → 自動更新 の
**プレリリースを含める** をオンにする (`"auto_update_include_prereleases": true`)。対象のリリースのうち、
公開日が最も新しいものではなく、タグの版が最も大きいものを選ぶ。

更新はこのリポジトリのリリースのダウンロードからだけ取得する。GitHub がインストーラーの SHA-256 の
ダイジェストを示しているときは、それと合わないインストーラーは入れない。確認に失敗したとき (GitHub に
繋がらない、ダウンロードしたものが合わない) は、「最新です」とせずにその旨を知らせる。この OS 向けの
インストーラーが無いリリースは、新しい版が無いのと同じに扱って飛ばす。

## Zed-JP-Enhance からの移行

ForgeKairos の以前の名前は Zed-JP-Enhance である。

- **ForgeKairos は手でインストールする。** Zed-JP-Enhance は自動更新で ForgeKairos に移れない。
  ForgeKairos のインストーラーは旧版の上に入れ、旧版の登録を消す。
- **Zed-JP-Enhance はアンインストールし、併用しない。** 旧版のサンドボックスは ForgeKairos のフォルダーを
  守らない。
- **データは初回の通常の起動で移る。** 旧のフォルダー (Windows では `%APPDATA%` と `%LOCALAPPDATA%` の
  `Zed-JP-Enhance`) を ForgeKairos の名前に変える (rename)。コピーはしない。設定、ネットワーク許可リスト、
  同意、データベース、スレッドが一緒に移る。次のときは移さない。
  - 新しいフォルダーに中身がある
  - Zed-JP-Enhance が起動中 (今回の起動は旧のフォルダーを使い、その旨を知らせる)
  - rename に失敗した (移した分を戻し、知らせる)
  - `--user-data-dir` 付き、補助のプロセス、コマンドラインの道具として起動した

  後で Zed-JP-Enhance が旧のフォルダーを作り直したら、1 回だけ知らせる。
- C と C++ のシンボル索引は新しい形式で作り直す。
- 公式の Zed のフォルダーは読まず、移さない。

## データフォルダー

| OS | 設定 | データ |
|---|---|---|
| Windows | `%APPDATA%\ForgeKairos` | `%LOCALAPPDATA%\ForgeKairos` |
| Linux | `$XDG_CONFIG_HOME/forgekairos` | `$XDG_DATA_HOME/forgekairos` |
| macOS | `~/.config/forgekairos` | `~/Library/Application Support/ForgeKairos` |

プロジェクトの設定は各プロジェクトの `.zed/` のまま、環境変数も `ZED_` の名前のまま。

## ほかの OS

ForgeKairos は Windows で開発し、確かめている。macOS と Linux のパッケージを作るスクリプトは残しているが、
それらのパッケージはまだ作っても確かめてもいない。
