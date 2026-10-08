# Changes from Zed

[日本語](ja/changes.md)

This page lists, in one place, how ForgeKairos behaves differently from Zed. Renaming and
other cosmetic changes are left out. Each section links to the page with the details.

## Language

- The UI can be shown in other languages, starting with English and Japanese. The sources
  keep the English text; translations are gettext PO files in `locales/`. The language is
  chosen with `ui_language` and applies without a restart; action descriptions and the
  settings schema are translated too. ([Display language and translations](language.md))
- Translations can be tried or added without rebuilding, and a remote server shows its
  messages in the client's language.

## Network and privacy

- Outbound traffic started by AI features is denied by default and allowed per host
  (Settings → Network). Redirects are checked hop by hop, and a consent prompt can add a
  host. ([Outbound network control](network.md))
- ACP agents, MCP servers and the npm that installs agents get a loopback proxy checked
  against the same allowlist, also on SSH remotes.
- Nothing is sent to Zed: telemetry, crash reports and Zed's hosted edit prediction are off,
  and collaboration (calls, screen sharing, LiveKit) is not built.
- Updates are checked only against this repository's GitHub Releases, and automatic updates
  are off by default. ([Installation and updates](install.md))
- Links that a language server or Copilot asks to open are shown for confirmation first,
  and only `http` and `https` addresses are opened.

## AI

- Every AI feature is off until you turn it on (`agent.enabled`), and no default model is
  set: a running local LLM is picked up. ([AI](ai.md))
- A cloud provider can be used only after you accept a warning that your data may be stored
  or used for training. Providers you added can be edited and removed in the settings.
- The Claude Code CLI can run in a tab of the agent panel and connect to ForgeKairos as its
  IDE (a loopback IDE server with highlight, annotation, tour, definition, reference and
  symbol search tools).
- Agent commands run in a sandbox, also on SSH remotes (network namespace and slirp4netns on
  Linux). Running outside it is an explicit choice by you, never by the agent.
- Tool permissions gain an **AI Judge** mode in which a separate model judges whether a command
  is safe, and an **Edit Only** profile.
- After an agent writes into a project folder, that folder's own code (formatters, linters,
  build scripts) is paused until you review and resume it.
- MCP servers, language servers and formatters defined in a project's settings do not start
  until you approve that exact content.
- Private files and files outside the project are hidden from agents, also under other
  spellings of the same path.
- Large attachments are sent as outlines within a size budget; the model selector collapses
  non-favorite groups; untrusted folders get no tools, with an explanation.
- Agent threads can be opened as tabs, split, moved to another panel or another window.

## Workspace

- Tabs and panels can be dragged out of the window to open in a window of their own, and
  panels can be dragged between docks.
- A dock can show two panels together (by default the agent and the browser on the right).
- A built-in browser panel (Microsoft Edge WebView2, Windows only), whose tabs can move to and
  from the editor and terminal panes.
- Docks and defaults closer to VS Code: project, outline and Git panels on the left, the
  agent on the right, menus in the title bar, a terminal open at start, whitespace shown,
  Japanese font fallbacks, and status bar buttons for Vim mode, SSH remotes and formatting.
  ([Settings reference](settings.md#zed-settings-with-a-different-default))
- The Windows installer adds **Open with ForgeKairos** to Explorer's right-click menu.
- Missing language servers and formatters are offered for installation into your data
  folder from a notification.

## C and C++

- A name-based symbol index covers the whole project without compilation information and
  backs up the language server for go to definition and symbol search.
  ([C and C++ code navigation](editor.md))
- A call graph panel shows callers and callees.
- The hover shows what a type holds (struct members, enum constants, macro definitions).
- clangd's indexing, thread count and memory are configurable, and it can be restarted when
  it uses too much memory.
- C and C++ are formatted with clang-format (bundled on Windows) in a GNU-based style.
- After a language server restart, buffers that are still open are reopened correctly.

## Terminal

- Macros with variables, branches and waits for output, edited as cards in a tab and kept
  in folders. ([Terminal macros and sessions](terminal-macros.md))
- Quick connections for SSH and Telnet, with passwords optionally kept in the OS credential
  store.
- Session logs, and file transfer (SFTP, SCP, FTP) from the terminal.

## Git

- A commit graph with history rewriting (amend, fix up, squash, reorder, drop, reword) by
  menus and drag and drop, refused up front when it cannot work and offering a way to undo. ([Git](git.md))
- A merge request box to assemble a branch from chosen commits, and comparison of two refs.
- Explanations of each operation in the UI, and a Git guide.
- Older git versions (such as the one on Ubuntu 18.04) are supported, and failures are
  shown in the Git panel.

## Markdown and Mermaid

- Formatting actions and a formatting bar, image insertion (including pasting), text color,
  highlight and size, and export to PDF. ([Markdown and Mermaid](markdown.md))
- A structured editor for Mermaid diagrams in a tab, and label editing in the preview.
- The preview renders `<details>`, `<br>`, a single newline as a line break, and uses the
  full width by default.

## CSV and TSV

- CSV and TSV files open as a spreadsheet-like grid: editing, rows and columns, filters, cell
  colors and comparison. Edits go to the text buffer, so undo and saving work as usual.
  ([CSV and TSV grid](tabular-data.md))

## Remote development

- A statically linked remote server is bundled, so Linux hosts with an old glibc (tested on
  Ubuntu 18.04) work without downloads. ([Remote development](remote.md))
- One server process per host serves all projects on it, and connections recover
  automatically after a network drop or sleep.
- SSH terminals keep running across disconnects and restarts and replay their output when
  you reconnect.
- Hosts without internet access download language servers and other tools through your
  machine.
- On Linux, file watching leaves inotify capacity for other programs and polls the rest; the
  project panel has a manual refresh.
