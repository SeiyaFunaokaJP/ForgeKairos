# Introduction

[日本語](ja/introduction.md)

ForgeKairos is a fork of the [Zed](https://zed.dev) code editor. It takes in Zed's releases one
at a time (it is currently based on Zed v1.21.0) and adds:

- a multilingual UI, starting with English and Japanese ([Display language and translations](language.md))
- control over outbound traffic started by AI features, denied by default ([Outbound network control](network.md))
- AI that is off until you turn it on, with local LLMs first and Claude Code integration ([AI](ai.md))
- a symbol index and a call graph for large C and C++ projects ([C and C++ code navigation](editor.md))
- terminal macros, quick connections and session logs ([Terminal macros and sessions](terminal-macros.md))
- a commit graph with history editing and a merge request box ([Git](git.md))
- Markdown input helpers and a structured editor for Mermaid diagrams ([Markdown and Mermaid](markdown.md))
- a spreadsheet-like grid for CSV and TSV files ([CSV and TSV grid](tabular-data.md))
- SSH remoting to old Linux hosts and terminals that survive disconnects ([Remote development](remote.md))

This site describes only what ForgeKairos adds or changes. For everything else, see
[Zed's documentation](https://zed.dev/docs/).

## Supported systems

ForgeKairos is developed for Windows and distributed as a Windows installer. For macOS and
Linux, see [Installation and updates](install.md#other-operating-systems). Remote
development works with Linux hosts.

## Not affiliated with Zed Industries

"Zed" is a trademark of Zed Industries, Inc. ForgeKairos is an independent fork; it is not
affiliated with, endorsed by or supported by Zed Industries. Names of Zed's own services,
such as Zed AI and Zed Pro, are used only to refer to those services.

## License

ForgeKairos keeps the licenses of Zed: mostly GPL-3.0-or-later, with some crates under
Apache-2.0, as each crate states (see `LICENSE-GPL` and `LICENSE-APACHE`). Zed is copyright
Zed Industries, Inc. The clang-format binary included in the Windows installer is part of the
LLVM project and is licensed under Apache-2.0 with LLVM Exceptions. Other third-party
material is listed in `THIRD_PARTY_NOTICES.md`.

## Repository

<https://github.com/SeiyaFunaokaJP/ForgeKairos>

The repository's history is connected to Zed's, and each Zed release is merged in turn.
