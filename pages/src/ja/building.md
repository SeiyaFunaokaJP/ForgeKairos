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
powershell -NoProfile -File script\build.ps1 -Release   # release ビルド (target\release)
```

スクリプトが行うこと:

- CMake が無ければ、Visual Studio 同梱の CMake を `PATH` に足す
- ディスクの空きが少なければ警告する
- `-j 8` でビルドする (`-Jobs` で変えられる。並列を制限しないと rustc がメモリ不足で落ちることがある)
- Linux 用のリモートサーバー (`x86_64-unknown-linux-musl`、静的リンク) を zig と `cargo-zigbuild` で
  ビルドし (無ければ入れる。zig は `winget` で)、`forgekairos-remote-server-linux-x86_64.gz` としてエディターの隣に置く
- リモートのホストでエージェントのサンドボックスが使う `slirp4netns` を取ってくる
- `clang-format.exe` をエディターの隣の `bin\` に置く

出力先は `target\debug` (`-Release` では `target\release`)。`CARGO_TARGET_DIR` があれば `target` の代わりにそこを使う。

| オプション | 効果 |
|---|---|
| `-Release` | release のプロファイルで `target\release` にビルドする。 |
| `-Run` | ビルドに成功したらエディターを起動する。 |
| `-Jobs <n>` | `-j 8` の代わりに `-j <n>` でビルドする。 |
| `-FreeDisk` | 先に、ビルドするプロファイルの `incremental` フォルダーと、`deps` の `.exe` と `.pdb` を消す。 |
| `-SkipCli` | コマンドラインの道具をビルドしない。 |
| `-SkipRemoteServer` | リモートサーバーをビルドしない。 |
| `-SkipSlirp4netns` | `slirp4netns` を取ってこない。 |
| `-SkipClangFormat` | clang-format を置かない。 |
| `-ClangFormatPath <ファイル>` | 置く `clang-format.exe`。指定が無いか、そのファイルが無ければ Visual Studio 2022 同梱の LLVM のものを使う。 |

エディターは `ForgeKairos.exe`、コマンドラインの道具は `forgekairos-cli.exe` で、どちらも `target\debug`
(または `target\release`) にできる。コマンドラインの道具は隣のエディターを探す。インストーラーはコマンドラインの
道具を `bin\forgekairos.exe` として入れる。ビルドのフォルダーで名前が違うのは、Windows では `forgekairos.exe` と
`ForgeKairos.exe` が同じファイルになるため。

release ビルドは stable チャンネルのデータフォルダー (インストールした ForgeKairos と同じ) を使う。分けるには
`--user-data-dir <フォルダー>` を付けて起動する。

## インストーラー

`script\build.ps1` はインストーラーを作らない。作るのは `script\bundle-windows.ps1`: 自前で release ビルド
(`x86_64-pc-windows-msvc` 向け。出力は `target\x86_64-pc-windows-msvc\release`) をし、Windows 用のリモートサーバーの
ビルド、Linux 用のクロスビルド、clang-format とそのライセンスと `slirp4netns` の同梱を行い、Inno Setup 6
(`winget install JRSoftware.InnoSetup`) を実行する。インストーラーは `target\forgekairos-windows-x86_64-setup.exe` に
できる。オプションには `-Architecture`、`-Install` (できたものを入れる)、`-ReleaseTag <タグ>` (公開する GitHub
リリースのタグ。更新の確認が比べる)、`-ClangFormatPath` などがある。Visual Studio 2022 とネットワーク (ConPTY、
AMD GPU Services のライブラリ、`slirp4netns`、無ければ zig を取ってくる) が要る。

```powershell
pwsh -NoProfile -File script\bundle-windows.ps1
```

PowerShell 7 (`pwsh`。`winget install Microsoft.PowerShell`) が要る。Windows PowerShell 5.1 (`powershell`)
から起動すると同じオプションのまま `pwsh` で起動し直し、`pwsh` が入っていなければ説明を出して止まる。

## 確認

upstream の規約は引き続き守る: `cargo clippy` ではなく `./script/clippy` を使う。翻訳の確認は
[`locales/README.md`](https://github.com/SeiyaFunaokaJP/ForgeKairos/blob/main/locales/README.md#checking-translations)
(英語) を見る。
