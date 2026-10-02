> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# ForgeKairos

English | [日本語](README.ja.md)

ForgeKairos is a fork of the [Zed](https://zed.dev) code editor. It follows upstream's
releases one at a time with `git merge` (currently based on Zed v1.21.0) and adds a
multilingual UI (Japanese and English first), control over outbound network traffic
(deny by default), a local-first approach to AI, and SSH remoting to old Linux hosts.
It is developed mainly for Windows.

Repository: <https://github.com/SeiyaFunaokaJP/ForgeKairos>

## Not affiliated with Zed Industries

"Zed" is a trademark of Zed Industries, Inc. ForgeKairos is an independent fork; it is not
affiliated with, endorsed by or supported by Zed Industries. Names of Zed's own services
(Zed AI, Zed Pro and so on) are used only to refer to those services.

## Highlights

- **Multilingual UI** - the UI text is English in the sources and translated through
  gettext PO files in `locales/` (see `locales/README.md`); Japanese ships with the app.
- **Outbound traffic is denied by default** - the editor talks only to the hosts you allow
  (Settings -> Network, or `%APPDATA%\ForgeKairos\network_allowlist.json`). Redirects are
  checked hop by hop. ACP agents, MCP servers and the npm that installs agents get a
  loopback proxy, so their traffic is checked against the same list, also on SSH remotes.
- **AI is off until you turn it on** - cloud AI providers require accepting a warning about
  training risks first; local LLMs (Ollama, LM Studio, OpenAI-compatible servers) work
  without it. External agents such as Claude Code can be used through ACP.
- **SSH remoting to old Linux** - a statically linked (musl) remote server is bundled, so
  hosts whose glibc is too old for VS Code (tested on Ubuntu 18.04) work.
- **Editing additions** - Markdown input helpers and PDF export, a call graph panel, a name
  index for C and C++, and VS Code-like defaults.

Telemetry and collaboration (calls and screen sharing) are disabled; the collaboration
and LiveKit crates are not built. Updates are checked only against this repository's
GitHub Releases.

## Building (Windows)

Requirements: Visual Studio 2022 (C++ workload, Spectre-mitigated libraries, Windows 11
SDK 26100) and rustup.

```powershell
powershell -NoProfile -File script\build-dev.ps1        # the editor, the CLI and the bundled remote server
powershell -NoProfile -File script\build-dev.ps1 -Run   # build, then start it
```

See [README.ja.md](README.ja.md) for details (in Japanese).

## Relationship with upstream

- Forked from [zed-industries/zed](https://github.com/zed-industries/zed) v1.17.2; each
  upstream release tag is merged in turn (no rebasing).
- Upstream's development guidelines (`.rules`, `./script/clippy`, GPUI idioms) still apply.
- Upstream's documentation (`docs/src/`, `CONTRIBUTING.md` and so on) is kept as it is;
  for what this fork adds, this README is the reference.

## License

ForgeKairos keeps the licenses of Zed: mostly **GPL-3.0-or-later**, with some crates under
Apache-2.0 (as each crate states; see `LICENSE-GPL` and `LICENSE-APACHE`). Zed is
Copyright Zed Industries, Inc. Third-party material outside the Rust dependency graph is
listed in `THIRD_PARTY_NOTICES.md`.
