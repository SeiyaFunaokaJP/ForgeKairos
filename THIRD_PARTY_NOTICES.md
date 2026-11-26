# Third-party notices

This file lists third-party material used by this fork that the generated
license list does not cover. Rust crates linked into the application are listed
in `assets/licenses.md`, which `script/generate-licenses` builds from the
dependency graph at release time; upstream Zed's own licenses are in
`LICENSE-GPL` and `LICENSE-APACHE`.

ForgeKairos is a fork of Zed, Copyright Zed Industries, Inc., under the licenses in
`LICENSE-GPL` and `LICENSE-APACHE`. "Zed" is a trademark of Zed Industries, Inc.;
ForgeKairos is not affiliated with Zed Industries.

Add an entry here whenever the fork starts carrying, modifying or relying on
third-party material outside that graph. Record the exact version, the license,
the copyright notice, whether it is distributed with the application, and what
the fork changed.

## Distributed with the application

### zed-i18n translations (modified)

| | |
|---|---|
| Used for | The first drafts of the Japanese descriptions of actions and of the settings schema |
| Version | The `ja-JP` translations at commit `c45ba0f6c3f63ffea40ab4431a9f00c7895d9d50` (2026-09-25), made for Zed v1.21.0 |
| Source | <https://github.com/LI-NA/zed-i18n> (`translations/ja-JP.json`, keyed by `catalog/en-US.json` and `manifest/ui-strings.json`) |
| License | GPL-3.0 (the project's catalogs and translations; its tooling is MIT) |
| Copyright | Copyright (c) 2026 LI-NA and the zed-i18n contributors |
| Location | `locales/ja/_action_docs.po`, `locales/ja/_settings_schema.po` (bundled into the application) |
| License text | `LICENSE-GPL` (GPL-3.0, the same license the application is distributed under) |

Changed by this fork (October 2026): zed-i18n translates each line of a doc comment on its
own; the fork joins the lines of one description into one translation, changes
the terms and the style to those of the fork's other Japanese translations
(`locales/ja/*.po`), and corrects mistranslations it found. Descriptions zed-i18n
does not have, such as those of the actions and settings the fork adds, are the
fork's own translations. Nothing was taken from zed-i18n's other files.

### zed-i18n translations for other languages (modified)

| | |
|---|---|
| Used for | The first drafts of about 44% of the entries of the Chinese (Simplified and Traditional), Korean, German, French, Spanish and Brazilian Portuguese catalogs |
| Version | The `zh-CN`, `zh-TW`, `ko-KR`, `de-DE`, `fr-FR`, `es-ES` and `pt-BR` translations at commit `955f1f7421a9104456f99bff8ac112a7ea897ee7` (2026-10-13), made for Zed v1.23.3 |
| Source | <https://github.com/LI-NA/zed-i18n> (`translations/<language>.json`) |
| License | GPL-3.0 (the project's catalogs and translations; its tooling is MIT) |
| Copyright | Copyright (c) 2026 LI-NA and the zed-i18n contributors |
| Location | `locales/zh-Hans/`, `locales/zh-Hant/`, `locales/ko/`, `locales/de/`, `locales/fr/`, `locales/es/`, `locales/pt-BR/` (bundled into the application) |
| License text | `LICENSE-GPL` (GPL-3.0, the same license the application is distributed under) |

Changed by this fork (October 2026): an entry was taken only when its English matched
the fork's message exactly and its placeholders agreed; the taken entries were then
reviewed against the English and changed to each language's terms, style and product
name. The other entries are the fork's own translations.

## Used for development only, not distributed

### Mermaid

| | |
|---|---|
| Used for | Checking that the Mermaid text this fork writes out is valid for the official parser (`tooling/mermaid-conformance/`) |
| Version | 12.1.0 (the version merman 0.9.3 targets) |
| Source | <https://github.com/mermaid-js/mermaid>, npm package `mermaid` |
| License | MIT |
| Copyright | Copyright (c) 2014 - 2022 Knut Sveidqvist |
| In the repository | No. Installed by `npm ci` into `tooling/mermaid-conformance/node_modules/`, which is ignored |

### jsdom

| | |
|---|---|
| Used for | The DOM that Mermaid's parser needs when run under Node.js |
| Version | 26.1.0 |
| Source | <https://github.com/jsdom/jsdom>, npm package `jsdom` |
| License | MIT |
| Copyright | Copyright (c) 2010 Elijah Insua |
| In the repository | No. Installed the same way |

Both bring further packages, pinned in
`tooling/mermaid-conformance/package-lock.json` (154 in total). By the license
each package declares: MIT 96 (including `khroma`, which declares it in its
license file only), ISC 34, Apache-2.0 7, BSD-3-Clause 4, BSD-2-Clause 2,
MIT-0 1, Unlicense 1 (`robust-predicates`), MPL-2.0 OR Apache-2.0 1
(`dompurify`), and EPL-2.0 1 (`elkjs`). None of them is copied into the repository or into a release.
