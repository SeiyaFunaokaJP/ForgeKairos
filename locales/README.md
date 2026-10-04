# Translations

The UI text in the Rust sources is English. Translations live here, one directory per
language, as gettext PO files, and are built into Zed. The same files can also be put
in your config directory to try a translation without rebuilding Zed.

## Layout

```
locales/
  <tag>/                 a BCP 47 language tag: ja, pt-BR, zh-Hant
    <crate>.po           messages a crate marks with t!, tn!, tc!, tcn! and msg!
    _actions.po          command palette names, msgctxt "action", msgid = action name
    _menus.po            application menu names and items, msgctxt "menu"
    _settings.po         the settings window's table, msgctxt "settings";
                         dropdown labels use the enum's name as msgctxt
    _action_docs.po      descriptions of actions (their doc comments), shown in the
                         keymap editor and the keymap and settings files' hovers,
                         msgctxt "action documentation", msgid = action name
    _settings_schema.po  descriptions in the settings file's JSON schema and of the
                         arguments of actions in the keymap file's, shown in their
                         hovers and completions, msgctxt "settings schema",
                         msgid = the English description
  en/_brand.po           the product's own name in place of upstream's (generated;
                         see "The product name" below)
  _brand_keep.txt        where upstream's name "Zed" stays
```

Every file of a directory goes into one catalog. A message without a msgctxt is one key
across all of them, so two files must not translate the same msgid differently (the
tests fail if they do). The `_` files keep every entry under their context, so
their short labels ("Go", "Selection") never translate the same English met anywhere
else. Crate files must not use the contexts `action`, `menu`, `settings`,
`action documentation` or `settings schema`.

Languages fall back from the most specific tag to the base language, then to English:
`pt-BR` → `pt` → `en/_brand.po` → the English msgid. A message that is missing, empty or marked
`#, fuzzy` shows the next language in that chain.

## Starting a language

Copy the header and entries of an existing language (`ja/`) into `locales/<tag>/` and
replace every `msgstr`. Each file starts with this header:

```po
msgid ""
msgstr ""
"Language: pt-BR\n"
"X-Language-Name: Português (Brasil)\n"
"Content-Type: text/plain; charset=UTF-8\n"
"Plural-Forms: nplurals=2; plural=(n > 1);\n"
```

- `X-Language-Name` is the language's own name. The settings window lists the language
  under it (Settings → General → Display Language).
- `Plural-Forms` is the rule gettext uses for the language; copy it from the
  [gettext manual](https://www.gnu.org/software/gettext/manual/html_node/Plural-forms.html)
  or from another project's PO files for the same language.
- Files are UTF-8 with LF line endings.
- Keep entries sorted by msgctxt (entries without one first), then msgid, byte by byte,
  and leave out `#:` location comments, so that merges stay small. Write every string on
  one line and escape `\`, `"`, newlines (`\n`) and tabs (`\t`). `po-format` (below)
  does all of this.

## Templates and tools

`tooling/i18n` is a command-line tool for these files. Run it from the repository root:

```sh
cargo run -p i18n_tool -- <command> ...
```

- `extract --out <dir>` writes one template per crate, `<dir>/<crate>.pot`, with the
  messages of its `t!`, `tn!`, `tc!`, `tcn!` and `msg!` calls (`--crate <name>` to limit
  it, `--include-tests` to add messages only tests use). Messages under the contexts
  `action`, `menu` and `settings` are left out, and so are the menu table in
  `crates/zed/src/zed/app_menus.rs`, the settings table in
  `crates/settings_ui/src/page_data.rs` and the action names: they have no template yet,
  and the existing language's `_` files are their list.
- `check` compares the code with `locales/<tag>/*.po` (`--lang <tag>`, `--crate <name>`)
  and lists messages a crate's file lacks, entries its code no longer uses, placeholder
  and plural mistakes, messages two files translate differently, entries out of order and
  CR line endings. All but the unused entries (and the changed ones below) make it exit
  with 1, so `check --crate <name>` can gate a change to one crate; with `--strict`, those
  do too. The `_` files are only checked for mistakes, not for missing entries,
  except for the two below when their input is given. Messages are the literals of the
  macros and of direct calls of `i18n::tr`, `i18n::tr_ctx` and `i18n::tr_ctx_opt`.
  - `--actions <file>` takes the output of `zed --dump-all-actions` and compares
    `_action_docs.po` with it: actions with a doc comment and no translation, entries for
    actions that are gone or have no doc comment, and entries whose doc comment has
    changed since they were translated (the `#.` comments of an entry hold the English
    it was translated from).
  - `--settings-schema <file>` takes the output of `zed --dump-settings-schema` and
    compares `_settings_schema.po` with its descriptions: descriptions without a
    translation, and entries the schema no longer has. An entry whose description was
    reworded is reported as changed, with the new English, when a description without a
    translation shares most of its words. With `--actions` too, the descriptions of the
    actions' arguments count as in use (they are not required).

  Descriptions without a translation fail like other missing messages; unused and changed
  entries fail only with `--strict`. Both dumps come from a build of Zed
  (`cargo run -p zed -- --dump-all-actions > actions.json`). A Windows release build
  prints them only with `--foreground` added. The settings schema dump leaves out the
  descriptions of feature flags, so their entries are not compared.
- `po-format <files or directories>` rewrites files in the layout above; `--check` only
  reports files that would change.
- `po-union <a.po> <b.po> ... --out <file>` merges files, for instance both sides of a
  merge conflict (`git show :2:<path>` and `git show :3:<path>`). An entry that is empty on
  one side takes the other side's translation; two different translations are listed and
  nothing is written.
- `scan` counts string literals that contain Japanese in `crates/`, split into test and
  other code (`--list` to see them, `--fail-on-prod` to fail when any is outside tests).
  Test code is an item under a `cfg` that holds only in tests (`test`, `all(test, …)`,
  `feature = "test-support"`; not `any(test, unix)`), a `#[test]`-like function, a file
  such an item declares, and files under `tests/` or named `tests.rs` / `*_tests.rs`.

## Writing translations

### Placeholders

Messages use the placeholders of Rust's `format!`, without format specs:

| In the msgid | In the translation |
|---|---|
| `{}` | `{}` takes the arguments in order of appearance |
| `{0}`, `{1}` | positional; reorder freely: `"{1} の {0} を選択"` |
| `{name}` | named; keep the name exactly |
| `{{`, `}}` | a literal `{` or `}` |

Use every placeholder of the msgid and no others. Do not write specs such as `{x:.1}`
in a translation: the source applies them before the translation sees the value. A
translation with a placeholder Zed cannot fill is not used, and the English is shown
instead (a debug build logs a warning under the `i18n` target).

### Plurals

```po
msgid "{count} Comment"
msgid_plural "{count} Comments"
msgstr[0] "{count} comentário"
msgstr[1] "{count} comentários"
```

Write as many `msgstr[n]` as `nplurals` in the header. `{count}` is the number the form
was chosen for.

### Contexts

`msgctxt` separates the same English with different meanings. Besides the reserved
contexts above, crates choose their own with `tc!("context", "...")`; translate each
context's entry separately. Settings dropdown labels are translated only under the
enum's name, such as:

```po
msgctxt "ShowWhitespaceSetting"
msgid "Selection"
msgstr "選択範囲"
```

A dropdown label without such an entry stays English.

To see the contexts in use, list them from the existing language, with how often each
occurs:

```sh
grep -h '^msgctxt' locales/ja/*.po | sort | uniq -c
```

A context says where the text appears (`search bar`, `keybinding`) or what it is
(`list separator`, `Mermaid diagram kind`). The same English under two contexts is two
entries, and each needs its translation. `list separator` is the `", "` placed between
items of a list; translate it with your language's separator.

### Descriptions of actions and settings

`_action_docs.po` translates an action's doc comment under the action's name, so that
upstream rewording the comment keeps the old translation until someone updates it.
Write the English it translates in `#.` comments above the entry, one per line of the
doc comment (`#.` alone for an empty line); `check --actions` compares them with the
current doc comments:

```po
#. Copy the selection to the clipboard.
msgctxt "action documentation"
msgid "editor::Copy"
msgstr "選択範囲をクリップボードにコピーします。"
```

`_settings_schema.po` translates the schema's descriptions by their English text, as
the schema has it: the lines of the doc comment joined with `\n`. The same English is
the same entry wherever it appears in the schema. The descriptions of the arguments of
actions, which the keymap file's schema shows, are translated the same way in the same
file. Zed translates the schema when it
gives it to the JSON language server, and gives it again when the display language
changes.

Both files are plain text: braces in them are not placeholders. Most of the Japanese
entries started from the translations of the [zed-i18n](https://github.com/LI-NA/zed-i18n)
project (GPL-3.0, the `ja-JP` translations at commit `c45ba0f`, made for Zed v1.21.0),
joined per description and changed to the terms and style of the other files; see
`THIRD_PARTY_NOTICES.md`.

### Text that is not translated

Text Zed sends to a language model (tool descriptions, system prompts, and the results
and errors of tools) is English in the source and is not in the catalog, so the display
language does not change what a model is told. Log messages are not translated either.

## The product name

Upstream's English names the application "Zed". The msgids keep upstream's text, so that
merging upstream stays simple, and the name is changed in what is shown instead:

- `en/_brand.po` gives every message that names the application its English text with the
  product's own name. It is installed when the UI is shown in English (English is still
  the language of the source text: it is not listed as a translation), and it is the last
  layer of every other language. Text a model reads (`i18n::in_english`) does not go
  through it.
- The translations name the product themselves.
- Not every "Zed" is the application: Zed's online services (Zed AI, Zed Pro), statements
  about upstream (the official Zed) and stored values keep the name. `_brand_keep.txt`
  lists them, each with its reason.

`cargo run -p i18n_tool -- brand --write` writes `en/_brand.po` and renames the
application in the translations; it changes nothing when run again, so run it after adding
translations. `brand --check` (also part of `check`) fails when a file is out of date,
when a translation names "Zed" where its English does not and the keep list does not say
why, and when a line of the keep list matches nothing.

Text outside the catalogs is checked too: `cargo run -p i18n_tool -- scan --brand` lists
the string literals outside tests that still say "Zed" and fails unless
`tooling/i18n/brand_sources.txt` gives each its reason. A message of the i18n macros is
left out only once a translation has it (`brand` renames only what the catalogs hold), and
the bare name "Zed" is always listed, as it is also an identifier in many places.

## Built-in snippets

The snippets that ship with Zed, such as the Mermaid diagrams offered in Markdown, are
not in the PO files: their bodies are inserted into the user's document, so each language
has its own copy of the whole file.

```
assets/snippets/
  <language>.json        the English original, such as markdown.json
  <tag>/<language>.json  its translation, such as ja/markdown.json
```

A translation is a snippet file in the same format as the English one. Keep its prefixes
(the tests fail if a translation offers a different set) and its tab stops (`${1:…}`),
and translate the names, descriptions and the text in the bodies. A Mermaid body must
stay valid Mermaid (check it with `tooling/mermaid-conformance`) and insert the same
diagram as the editor's Insert Mermaid Diagram menu for that language.

Zed offers the file of the display language, falling back per file the same way as the
catalogs: `pt-BR/`, then `pt/`, then the English file. A new display language takes
effect from the next completion. These files are built into Zed; the config directory
has no counterpart for them. The user's own snippets in `<config dir>/snippets/` are a
separate mechanism and do not depend on the display language; a user snippet with the
same prefix as a built-in one replaces it.

## Trying a translation without rebuilding

Put the PO files in `<config dir>/locales/<tag>/`, where the config directory is
`~/.config/forgekairos` on macOS and Linux (`$XDG_CONFIG_HOME/forgekairos` when it is
set) and `%APPDATA%\ForgeKairos` on Windows. Then choose the language in Settings → General → Display
Language, or set it in `settings.json`:

```json
{ "ui_language": "pt-BR" }
```

Entries there override the bundled ones message by message, and a directory there can
add a language Zed does not ship. Zed reads the files when it switches to the language
and keeps them for the rest of the session: restart Zed after editing them. Menus,
settings and the command palette switch at once; other text switches after a restart.

On a remote host (SSH, WSL, Docker), the text the remote server writes, such as some git
errors and terminal notices, is translated there with the catalogs built into the
server, in the language Zed sends when it connects. The files in your config directory
do not reach the server, and a language the server does not ship stays English there.
The server takes a new language when Zed reconnects, and it uses one language for all
the clients connected to it: the one that connected last.

Problems in the files (syntax errors, a broken `Plural-Forms`, a file that is not UTF-8)
are shown in a notification and written to the log; the readable entries are still
used. Files larger than 16 MiB, and files beyond the first 256 of a directory, are
skipped.

## Checking translations

- `cargo test -p i18n_catalog` checks that every bundled file parses, that the files of
  a language agree on each message and that the `_` files keep to their contexts.
- `cargo test -p settings_ui`, `cargo test -p command_palette` and
  `cargo test -p zed --bin zed -- localized_menus` check the Japanese catalog against
  the Japanese the UI showed before these files existed. They do not check other
  languages.
- To find untranslated messages while using Zed, run a debug build with
  `ZED_LOG=i18n=debug`: each message looked up without a translation is logged once.
- `cargo run -p i18n_tool -- check` compares the files with the code (see
  [Templates and tools](#templates-and-tools)).
