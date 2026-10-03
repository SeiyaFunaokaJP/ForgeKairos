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

## 開発ビルド

```powershell
powershell -NoProfile -File script\build-dev.ps1        # エディター、CLI、同梱のリモートサーバー
powershell -NoProfile -File script\build-dev.ps1 -Run   # ビルドしてから起動
```

スクリプトが行うこと:

- CMake が無ければ、Visual Studio 同梱の CMake を `PATH` に足す
- ディスクの空きが少なければ警告する
- `-j 8` でビルドする (並列を制限しないと rustc がメモリ不足で落ちることがある)
- Linux 用のリモートサーバー (`x86_64-unknown-linux-musl`、静的リンク) を zig と `cargo-zigbuild` で
  ビルドし (無ければ入れる。zig は `winget` で)、エディターの隣に置く
- リモートのホストでエージェントのサンドボックスが使う `slirp4netns` を取ってくる
- `clang-format.exe` を `target\debug\bin\` に置く

| オプション | 効果 |
|---|---|
| `-Run` | ビルドに成功したらエディターを起動する。 |
| `-FreeDisk` | 先に `target\debug\incremental` と、`target\debug\deps` の `.exe` と `.pdb` を消す。 |
| `-SkipCli` | コマンドラインの道具をビルドしない。 |
| `-SkipRemoteServer` | リモートサーバーをビルドしない。 |
| `-SkipSlirp4netns` | `slirp4netns` を取ってこない。 |
| `-SkipClangFormat` | clang-format を置かない。 |
| `-ClangFormatPath <ファイル>` | 置く `clang-format.exe`。指定が無いか、そのファイルが無ければ Visual Studio 2022 同梱の LLVM のものを使う。 |

エディターは `target\debug\zed.exe`。コンソールから使うには、隣に `cli.exe` が要る (スクリプトは両方を
ビルドする)。

## インストーラー

`script\bundle-windows.ps1` は、リリースビルド、Linux 用リモートサーバーのクロスビルド、clang-format と
そのライセンスと `slirp4netns` の同梱を行い、Inno Setup 6 (`winget install JRSoftware.InnoSetup`) を実行する。
オプションには `-Architecture`、`-Install` (できたものを入れる)、`-ClangFormatPath` などがある。

## 確認

upstream の規約は引き続き守る: `cargo clippy` ではなく `./script/clippy` を使う。翻訳の確認は
[`locales/README.md`](https://github.com/SeiyaFunaokaJP/ForgeKairos/blob/main/locales/README.md#checking-translations)
(英語) を見る。
