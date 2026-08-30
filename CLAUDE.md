@.rules

---

# zed-jp-enhance (このリポジトリについて)

Zed v1.17.2 のハードフォーク (upstream 追従なし。remote `upstream-snapshot` からの rebase/merge 禁止)。
日本語 UI 化 + ローカル LLM 統制 (egress deny-by-default) を自前改造で足す。

必読 (この順): `docs/jp-enhance/TODO.md` (今の状態と次にやること) → `docs/jp-enhance/DESIGN.md` (ADR-F0x) →
`docs/jp-enhance/DEVLOG.md` (履歴)。ビルドは `docs/jp-enhance/BUILD-WINDOWS.md`。

upstream の開発規約 (上記 .rules、`./script/clippy`、GPUI イディオム) は遵守する。
git commit はマイルストーン単位。ドキュメント更新 (TODO/DEVLOG/DESIGN) をコミットに含めること。