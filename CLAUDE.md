@.rules

---

# zed-jp-enhance (このリポジトリについて)

Zed のフォーク (基点 v1.17.2)。履歴は upstream に繋がっており、upstream のリリースタグを `git merge` で
1 版ずつ取り込む (ADR-F63。rebase はしない。upstream のタグは `refs/upstream/tags/*`)。
多言語 UI (まず日本語と英語) + ローカル LLM 統制 (egress deny-by-default) を自前改造で足す。

UI の文字列は英語の原文をソースに書き、`t!` などのマクロで包む。訳は `locales/<言語>/*.po` に置く。
upstream のファイルへの変更は最小に保つ (包むだけ、または台帳にある結線点)。

必読 (この順): `docs/jp-enhance/TODO.md` (今の状態と次にやること) → `docs/jp-enhance/DESIGN.md` (ADR-F0x) →
`docs/jp-enhance/DEVLOG.md` (履歴)。ビルドは `docs/jp-enhance/BUILD-WINDOWS.md`。

upstream の開発規約 (上記 .rules、`./script/clippy`、GPUI イディオム) は遵守する。
git commit はマイルストーン単位。ドキュメント更新 (TODO/DEVLOG/DESIGN) をコミットに含めること。