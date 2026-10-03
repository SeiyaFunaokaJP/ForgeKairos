# Display language and translations

[日本語](ja/language.md)

ForgeKairos shows its UI in the language you choose. English and Japanese ship with the
app. The UI text in the sources is English; translations are gettext PO files in the
repository's `locales/` folder.

## Choosing the language

Open Settings → General → Display Language, or set `ui_language` in `settings.json`:

```json
{ "ui_language": "ja" }
```

| Value | Meaning |
|---|---|
| `"system"` (default) | Follow the operating system's preferred languages. English when there is no translation for them. |
| `"en"` | English (no translation). |
| a BCP 47 tag such as `"ja"` or `"pt-BR"` | That language. |

Menus, settings and the command palette switch at once. Some other text, such as tab
titles, switches after a restart.

## What is translated

- Menus, the settings window, the command palette and panel text.
- Command palette entries are found by their translated name and by their English name.
- Descriptions of actions (shown in the keymap editor and in the hovers of the keymap and
  settings files) and descriptions in the settings file's JSON schema.
- Built-in snippets, such as the Mermaid diagrams offered in Markdown, have a copy per
  language.

Text that ForgeKairos sends to a language model (tool descriptions, system prompts, tool
results) stays English, so the display language does not change what a model is told.
Log messages are not translated.

## Language fallback

A message falls back from the most specific tag to the base language, then to English:
`pt-BR` → `pt` → English. A message that is missing, empty or marked `#, fuzzy` shows the
next language in the chain.

## Trying or adding a translation without rebuilding

Put PO files in `<config directory>/locales/<tag>/`. The config directory is:

| OS | Config directory |
|---|---|
| Windows | `%APPDATA%\ForgeKairos` |
| Linux | `$XDG_CONFIG_HOME/forgekairos` (`~/.config/forgekairos` when it is not set) |
| macOS | `~/.config/forgekairos` |

Entries there override the bundled ones message by message, and a folder there can add a
language that is not bundled. The files are read when ForgeKairos switches to the language;
restart after editing them. Problems in the files are shown in a notification and written
to the log; the readable entries are still used.

## Remote hosts

On SSH, WSL and Docker remotes, text written by the remote server (such as some git errors
and terminal notices) is translated on the remote with the catalogs built into the server,
in the language sent when ForgeKairos connects. Files in your config directory do not reach
the server. The server uses one language for all clients connected to it: the one that
connected last.

## Contributing a translation

The file layout, the PO header, placeholders, plurals, contexts and the `i18n_tool`
command-line tool are described in
[`locales/README.md`](https://github.com/SeiyaFunaokaJP/ForgeKairos/blob/main/locales/README.md).
