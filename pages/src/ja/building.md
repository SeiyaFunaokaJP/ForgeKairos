# Windows でのビルド

[English](../building.md)

Windows で ForgeKairos をビルドする手順の概要。Zed 自身のビルドの手引きは
[Zed の開発の文書](https://zed.dev/docs/development/windows) にある。

## 必要なもの

- Visual Studio 2022 (C++ のワークロード、MSVC v143、Spectre の緩和ライブラリ、Windows 11 SDK 10.0.26100、
  CMake。CMake は Visual Studio 同梱のものでよい)
- rustup (`rust-toolchain.toml` が指定するツールチェーンとターゲットは最初のビルドで入る)
- 長いパスを Git (`core.longpaths`) と Windows (`LongPathsEnabled`) で有効にする
- `RUSTFLAGS` を設定しない
- 十分なディスクの空き (`target` フォルダーは 100 GB を超えることがある)

## ビルド

```powershell
powershell -NoProfile -File script\build.ps1            # debug ビルド (target\debug)
powershell -NoProfile -File script\build.ps1 -Run       # ビルドしてから起動
powershell -NoProfile -File script\build.ps1 -Release   # release ビルド (target\x86_64-pc-windows-msvc\release)
```

スクリプトが行うこと:

- CMake が無ければ、Visual Studio 同梱の CMake を `PATH` に足す
- ディスクの空きが少なければ警告する
- `-j 8` でビルドする (`-Jobs` で変えられる。並列を制限しないと rustc がメモリ不足で落ちることがある)
- Linux 用のリモートサーバー (`x86_64-unknown-linux-musl`、静的リンク) を zig と `cargo-zigbuild` で
  ビルドし (無ければ入れる。zig は `winget` で)、`forgekairos-remote-server-linux-x86_64.gz` としてエディターの隣に置く
- リモートのホストでエージェントのサンドボックスが使う `slirp4netns` を取ってくる
- `clang-format.exe` をエディターの隣の `bin\` に置く

出力先は `target\debug` (`-Release` では `target\x86_64-pc-windows-msvc\release`)。`CARGO_TARGET_DIR` があれば `target` の代わりにそこを使う。

| オプション | 効果 |
|---|---|
| `-Release` | release のプロファイルで `target\x86_64-pc-windows-msvc\release` に、インストーラーのスクリプトと同じ形でビルドする (下を参照)。 |
| `-Run` | ビルドに成功したらエディターを起動する。 |
| `-Jobs <n>` | `-j 8` の代わりに `-j <n>` でビルドする。 |
| `-FreeDisk` | 先に、ビルドするプロファイルの `incremental` フォルダーと、`deps` の `.exe` と `.pdb` を消す。 |
| `-SkipCli` | コマンドラインの道具をビルドしない。 |
| `-SkipRemoteServer` | リモートサーバーをビルドしない。 |
| `-SkipSlirp4netns` | `slirp4netns` を取ってこない。 |
| `-SkipClangFormat` | clang-format を置かない。 |
| `-ClangFormatPath <ファイル>` | 置く `clang-format.exe`。指定が無いか、そのファイルが無ければ Visual Studio 2022 同梱の LLVM のものを使う。 |

エディターは `ForgeKairos.exe`、コマンドラインの道具は `forgekairos-cli.exe` で、どちらも `target\debug`
(または `target\x86_64-pc-windows-msvc\release`) にできる。コマンドラインの道具は隣のエディターを探す。インストーラーはコマンドラインの
道具を `bin\forgekairos.exe` として入れる。ビルドのフォルダーで名前が違うのは、Windows では `forgekairos.exe` と
`ForgeKairos.exe` が同じファイルになるため。

release ビルドは stable チャンネルのデータフォルダー (インストールした ForgeKairos と同じ) を使う。分けるには
`--user-data-dir <フォルダー>` を付けて起動する。

## インストーラー

`script\build.ps1` はインストーラーを作らない。作るのは `script\bundle-windows.ps1`: release ビルド
(`x86_64-pc-windows-msvc` 向け。出力は `target\x86_64-pc-windows-msvc\release`) をし、Windows 用のリモートサーバーの
ビルド、Linux 用のクロスビルド、clang-format とそのライセンスと `slirp4netns` の同梱を行い、Inno Setup 6
(`winget install JRSoftware.InnoSetup`) を実行する。インストーラーは `target\forgekairos-windows-x86_64-setup.exe` に
できる。オプションには `-Architecture`、`-Install` (できたものを入れる)、`-ReleaseTag <タグ>` (公開する GitHub
リリースのタグ。更新の確認が比べる)、`-ClangFormatPath`、`-Fast` (下を参照) などがある。Visual Studio 2022 とネットワーク (ConPTY、
AMD GPU Services のライブラリ、`slirp4netns`、無ければ zig を取ってくる) が要る。

```powershell
pwsh -NoProfile -File script\bundle-windows.ps1
```

インストーラーは `script\build.ps1 -Release` の release ビルドをそのまま使う。両方とも同じパッケージ
(`zed`、`cli`、`auto_update_helper` を 1 回の cargo で)、同じターゲット、Visual Studio の開発者環境、リリース
チャンネルの設定でビルドするため。どれかが違うと (素の `cargo build --release`、別のパッケージの組み合わせ、開発者
環境の外でのビルド)、cargo は C の依存とエディターをビルドし直す。何も変えていなければ数秒で終わる。コミットの後は
エディターに埋め込むコミットのハッシュが変わるので、release のプロファイル (thin LTO) でのエディターのコンパイルと
リンクに 30 分ほどかかる。

`-Fast` は手元で試す用のインストーラーを作る (公開用ではない)。`release-fast` のプロファイル (LTO なし、デバッグ
情報は limited) で `target\x86_64-pc-windows-msvc\release-fast` にビルドし、`lzma2/fast` で圧縮する。
初回はこのプロファイル用にすべての依存をビルドし直すので、時間とディスクを使う。

PowerShell 7 (`pwsh`。`winget install Microsoft.PowerShell`) が要る。Windows PowerShell 5.1 (`powershell`)
から起動すると同じオプションのまま `pwsh` で起動し直し、`pwsh` が入っていなければ説明を出して止まる。

## ほかの OS

`script/bundle.ps1` (PowerShell 7。Linux と macOS では `pwsh` で起動する `script/bundle` も使える) は、どの OS の
パッケージも 1 つのコマンドで作る。`-Os` で選ぶ: `windows`、`linux`、`macos`、`all`。省くと動いている OS。

```powershell
pwsh -NoProfile -File script/bundle.ps1 -Os windows    # script\bundle-windows.ps1 (上を参照)
pwsh -NoProfile -File script/bundle.ps1 -Os linux      # Windows では WSL の Docker の中で
pwsh -NoProfile -File script/bundle.ps1 -Os macos -Wait
pwsh -NoProfile -File script/bundle.ps1 -Os all -DryRun
```

| 作る OS | Windows で | Linux で | macOS で |
|---|---|---|---|
| Windows | `script\bundle-windows.ps1` | ワークフロー (`-Os all` か `-Ci` のときだけ) | ワークフロー (`-Os all` か `-Ci` のときだけ) |
| Linux | WSL の Docker のコンテナーで `script/bundle-linux` | `script/bundle-linux` (`-Container` でコンテナー) | ワークフロー。`-Container` なら手元の Docker のコンテナー |
| macOS | ワークフロー | ワークフロー | `script/bundle-mac` |

パッケージは `target` に、更新の確認が探す名前でできる: `forgekairos-windows-x86_64-setup.exe`、
`forgekairos-linux-x86_64.tar.gz`、`forgekairos-macos-aarch64.dmg`、`forgekairos-macos-x86_64.dmg`。リモート
サーバーは `forgekairos-remote-server-linux-x86_64.gz` と `forgekairos-remote-server-macos-<arch>.gz`。

Linux と macOS のパッケージにも、Windows のインストーラーと同じく、SSH・WSL・Docker の接続が Linux のホストに
入れるもの (Linux x86_64 のリモートサーバー `forgekairos-remote-server-linux-x86_64.gz` と
`slirp4netns-linux-x86_64`) が入る。無いとエディターはサーバーをダウンロードすることになり、既定のネットワークの
方針がそれを止める。置き場所はエディターが探す所: Linux のパッケージではエディターと同じ `libexec/`、macOS では
アプリの `Contents/Resources`。clang-format は Linux と macOS には入れない: エディターは `PATH` の
`clang-format` (ディストリビューションのパッケージ、Homebrew の `brew install clang-format`、Xcode) を使う。

オプション: `-Architecture` (この機械で作るものの `x86_64` か `aarch64`。既定はこの機械)、`-MacosArchitecture`
(`both`、`aarch64`、`x86_64`: ワークフローが作る macOS。Windows と Linux はワークフローでは x86_64)、
`-ReleaseTag` (インストーラーと同じ)、
`-Jobs <n>` (cargo の `-j`)、`-Ci` (すべてワークフローで作る)、`-Wait` (ワークフローの終わりを待ち、できたものを
`target/ci-<実行の ID>/` に落とす)、`-Ref` (ワークフローがビルドするブランチかタグ。既定は今のブランチ)、
`-DryRun` (実行せずにコマンドを表示する)。Windows 用の `-Fast`、`-Install`、`-ClangFormatPath` は
`script\bundle-windows.ps1` に渡す。

### Linux

Linux では先に `script/linux` でビルドに要るものを入れる。パッケージはその機械の glibc にリンクし、同じか新しい
版の glibc でだけ動く。Windows では WSL の Docker (`wsl.exe -e docker`) の `ubuntu:22.04` のコンテナー
(glibc 2.35) でビルドするので、Ubuntu 22.04、Debian 12 とそれ以降で動く。イメージは `script/linux` で依存を、
`rust-toolchain.toml` のツールチェーンを入れる。`-LinuxImage` で別のベースイメージを、`-Container` で Linux の
機械でもコンテナーを使う。

作業ツリーをコンテナーにマウントするので、ビルドは手元の作業ツリー (コミットしていない変更も含む) から作る。cargo
のレジストリと target フォルダーは `target` の下ではなく、WSL の中の Docker のボリューム
`forgekairos-bundle-cargo-ubuntu-22.04` と `forgekairos-bundle-target-ubuntu-22.04` に置く (target は数十 GB に
なる)。空けるには `wsl.exe -e docker volume rm` で消す。初回はイメージとすべての依存をビルドする。2 回目からは変わった
ところだけをビルドし直す。

### macOS

macOS のパッケージは Mac (Xcode、`script/bundle-mac`。Linux のリモートサーバーを `cargo-zigbuild` で
クロスビルドするので zig も要る: `brew install zig`。`cargo-zigbuild` は無ければスクリプトが入れる) で作るか、下の GitHub Actions のワークフローで作る。
ほかの OS では `script/bundle.ps1 -Os macos` が GitHub CLI (`gh workflow run`。先に `gh auth login`) で
ワークフローを起動する。ワークフローは GitHub にある状態のブランチをビルドするので、先にプッシュする。署名の
シークレットが無ければアプリはアドホック署名になり、ダウンロードしたアプリを macOS は初回は開かない: システム設定の
「プライバシーとセキュリティ」の「このまま開く」で許可するか、
`xattr -dr com.apple.quarantine /Applications/ForgeKairos.app` で隔離の印を外す。リポジトリにシークレット
`MACOS_SIGNING_IDENTITY`、`MACOS_CERTIFICATE`、`MACOS_CERTIFICATE_PASSWORD`、`APPLE_NOTARIZATION_KEY`、
`APPLE_NOTARIZATION_KEY_ID`、`APPLE_NOTARIZATION_ISSUER_ID` を設定すると、その Developer ID で署名し、
公証 (notarization) も行う。

### リリースのワークフロー

`.github/workflows/forgekairos_release.yml` は GitHub のホストするランナーでビルドする: Windows x86_64 は
`windows-2022`、Linux x86_64 は `ubuntu-22.04`、macOS の aarch64 と x86_64 は `macos-14` (x86_64 はクロス
コンパイル)。各ジョブは `script/bundle.ps1 -Os <OS>` を実行し、パッケージをファイル名と同じ名前の
アーティファクトとして上げる。動くのは、起動したとき (Actions のタブから入力 `os`、`macos_arch`、
`release_tag` を付けて、または `script/bundle.ps1` から) と、`v*` のタグをプッシュしたときだけ。タグのときは
最後のジョブがすべてのパッケージを付けた下書きの GitHub リリースを作る (確かめてから公開する)。

リポジトリは非公開なので、ワークフローはアカウントの課金される Actions の時間を使い、macOS の時間は 10 倍に
数える。標準のランナーでは 1 ジョブに数時間かかる (thin LTO の release ビルド) ので、要る OS だけを起動する
(`-Os macos`、入力 `macos_arch`)。ジョブは `Swatinem/rust-cache` で cargo のレジストリとビルドをキャッシュし、
依存が変わらない再実行を安くする。GitHub が保つキャッシュはリポジトリあたり 10 GB までで、古いものから消える。

## リリースの作り方

1. タグを決める: `v` と `crates/zed/Cargo.toml` の版 (`v1.22.0`)。プレリリースは後ろに付ける
   (`v1.22.0-pre`、`v1.22.0-pre.2`)。`-` を含むタグはプレリリースとして公開し、更新の確認は
   `auto_update_include_prereleases` を有効にした人にだけ出す。タグはどのビルドにも `-ReleaseTag` で渡す:
   ビルドに埋め込まれ、更新の確認は GitHub のリリースをこれと比べる。
2. Windows と Linux は手元で作る:

   ```powershell
   pwsh -NoProfile -File script/bundle.ps1 -Os windows -ReleaseTag v1.22.0
   pwsh -NoProfile -File script/bundle.ps1 -Os linux -ReleaseTag v1.22.0
   ```

3. macOS はワークフローで作る (先にコミットをプッシュする。課金される):
   `pwsh -NoProfile -File script/bundle.ps1 -Os macos -ReleaseTag v1.22.0 -Wait` がディスクイメージと macOS の
   リモートサーバーを `target/ci-<実行の ID>/` に落とす。
4. 下書きのリリースを作り、パッケージをその名前のまま上げる (更新の確認はこの名前で探す):

   ```powershell
   gh release create v1.22.0 --repo SeiyaFunaokaJP/ForgeKairos --draft --target (git rev-parse HEAD) --title "ForgeKairos v1.22.0" --generate-notes
   gh release upload v1.22.0 --repo SeiyaFunaokaJP/ForgeKairos `
       target/forgekairos-windows-x86_64-setup.exe `
       target/forgekairos-linux-x86_64.tar.gz target/forgekairos-remote-server-linux-x86_64.gz `
       (Get-ChildItem -Recurse -File target/ci-<実行の ID>).FullName
   ```

   プレリリースなら `--prerelease` を足す。下書きを確かめてから GitHub で公開する。公開でタグができる。

代わりにタグをプッシュする (`git push origin v1.22.0`) と、ワークフローがすべての OS をビルドし、下書きの
リリースも作る。時間はいちばんかかる。下書きの公開でできたタグでもワークフローが動くことがある: パッケージを
もう上げてあるなら、その実行は `gh run cancel` で止める。

更新の確認はサインインせずにリリースを読むので、リポジトリが非公開のあいだは何も見えない。

## 確認

upstream の規約は引き続き守る: `cargo clippy` ではなく `./script/clippy` を使う。翻訳の確認は
[`locales/README.md`](https://github.com/SeiyaFunaokaJP/ForgeKairos/blob/main/locales/README.md#checking-translations)
(英語) を見る。
