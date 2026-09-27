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
```

Every file of a directory goes into one catalog. A message without a msgctxt is one key
across all of them, so two files must not translate the same msgid differently (the
tests fail if they do). The three `_` files keep every entry under their context, so
their short labels ("Go", "Selection") never translate the same English met anywhere
else. Crate files must not use the contexts `action`, `menu` or `settings`.

Languages fall back from the most specific tag to the base language, then to English:
`pt-BR` → `pt` → the English msgid. A message that is missing, empty or marked
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
- Keep entries sorted by msgctxt, then msgid, and leave out `#:` location comments, so
  that merges stay small. Escape `\`, `"`, newlines (`\n`) and tabs (`\t`).

There is no template (`.pot`) yet. Until a tool generates one, the complete list of
messages is the union of the existing language's files, and the code (`t!(...)` and
friends, the menu table in `crates/zed/src/zed/app_menus.rs`, the settings table in
`crates/settings_ui/src/page_data.rs`).

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

## Trying a translation without rebuilding

Put the PO files in `<config dir>/locales/<tag>/`, where the config directory is
`~/.config/zed` on macOS and Linux (`$XDG_CONFIG_HOME/zed` when it is set) and
`%APPDATA%\Zed` on Windows. Then choose the language in Settings → General → Display
Language, or set it in `settings.json`:

```json
{ "ui_language": "pt-BR" }
```

Entries there override the bundled ones message by message, and a directory there can
add a language Zed does not ship. Zed reads the files when it switches to the language
and keeps them for the rest of the session: restart Zed after editing them. Menus,
settings and the command palette switch at once; other text switches after a restart.

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
- `i18n_catalog::lint` compares a translation with a template (placeholders, format
  specs, plural forms, duplicates, obsolete and untranslated entries). It has no
  command yet, since there is no template to compare with.
