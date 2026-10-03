# はじめに

[English](../introduction.md)

ForgeKairos は [Zed](https://zed.dev) のコードエディターのフォークである。Zed のリリースを 1 版ずつ取り込み
(現在は Zed v1.22.0 を基にしている)、次のものを足している。

- 多言語の UI (まず英語と日本語) ([表示言語と翻訳](language.md))
- AI の機能が始める外部への通信の統制 (既定で拒否) ([外部通信の統制](network.md))
- 有効にするまでオフの AI (ローカル LLM を優先、Claude Code との連携) ([AI](ai.md))
- 大きな C と C++ のプロジェクトのためのシンボル索引と呼び出しグラフ ([C と C++ のコードの移動](editor.md))
- ターミナルのマクロ、クイック接続、セッションのログ ([ターミナルのマクロとセッション](terminal-macros.md))
- 履歴の編集とマージリクエストの箱を持つコミットグラフ ([Git](git.md))
- Markdown の入力補助と Mermaid の図の構造化エディター ([Markdown と Mermaid](markdown.md))
- CSV と TSV の表計算ソフトのようなグリッド ([CSV と TSV のグリッド](tabular-data.md))
- 古い Linux のホストへの SSH リモートと、切断をまたいで生きるターミナル ([リモート開発](remote.md))

このサイトは ForgeKairos が足した点・変えた点だけを書く。それ以外は [Zed の文書](https://zed.dev/docs/) を見る。

## 対応するシステム

ForgeKairos は Windows 向けに開発し、Windows のインストーラーで配布する。macOS と Linux は
[インストールと更新](install.md#ほかの-os) を見る。リモート開発は Linux のホストで使える。

## Zed Industries との関係

「Zed」は Zed Industries, Inc. の商標である。ForgeKairos は独立したフォークで、Zed Industries とは関係が
なく、Zed Industries が承認・支援しているものではない。Zed AI、Zed Pro など Zed のサービスの名前は、その
サービスを指すときにだけ使う。

## ライセンス

ForgeKairos は Zed のライセンスを引き継ぐ: 主に GPL-3.0-or-later、一部のクレートは Apache-2.0 (各クレートの
表記に従う。`LICENSE-GPL` と `LICENSE-APACHE` を参照)。Zed の著作権は Zed Industries, Inc. にある。Windows の
インストーラーが同梱する clang-format は LLVM プロジェクトの一部で、Apache-2.0 with LLVM Exceptions で
ある。そのほかの第三者のものは `THIRD_PARTY_NOTICES.md` に載せている。

## リポジトリ

<https://github.com/SeiyaFunaokaJP/ForgeKairos>

リポジトリの履歴は Zed の履歴に繋がっており、Zed のリリースを順に merge で取り込む。
