@.rules

---

# ForgeKairos (このリポジトリについて。旧名 zed-jp-enhance)

Zed のフォーク (基点 v1.17.2、現在 v1.23.3 ベース)。製品名と識別子は `crates/branding` に集約してある (画面の文は `locales/README.md` の「製品名」の節)。履歴は upstream に繋がっており、upstream のリリースタグを `git merge` で
1 版ずつ取り込む (ADR-F63。rebase はしない。upstream のタグは `refs/upstream/tags/*`)。
多言語 UI (まず日本語と英語) + ローカル LLM 統制 (egress deny-by-default) を自前改造で足す。

UI の文字列は英語の原文をソースに書き、`t!` などのマクロで包む。訳は `locales/<言語>/*.po` に置く。
upstream のファイルへの変更は最小に保つ (包むだけ、または台帳にある結線点)。

必読 (この順): `.forgekairos-docs/TODO.md` (今の状態と次にやること) → `.forgekairos-docs/DESIGN.md` (ADR-F0x) →
`.forgekairos-docs/DEVLOG.md` (履歴)。ビルドは `.forgekairos-docs/BUILD-WINDOWS.md`。

upstream の開発規約 (上記 .rules、`./script/clippy`、GPUI イディオム) は遵守する。
git commit はマイルストーン単位。ドキュメント更新 (TODO/DEVLOG/DESIGN) をコミットに含めること。