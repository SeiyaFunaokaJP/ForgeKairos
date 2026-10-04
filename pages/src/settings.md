# Settings reference

[日本語](ja/settings.md)

This page lists the settings that ForgeKairos adds to Zed, and the Zed settings whose
default it changes. The values are those of the bundled `assets/settings/default.json`. For
every other setting, see [Zed's configuration documentation](https://zed.dev/docs/configuring-zed).

The outbound network allowlist is not a setting in `settings.json`; it is kept in
`network_allowlist.json` (see [Outbound network control](network.md)).

## Settings added by ForgeKairos

### General and layout

| Key | Default | Description |
|---|---|---|
| `ui_language` | `"system"` | Display language: `"system"`, `"en"` or a BCP 47 tag such as `"ja"`. |
| `default_stacked_dock` | `"right"` | Dock in which two panels are arranged together when a project is opened for the first time (`left`, `right`, `bottom`). |
| `default_stacked_panel` | `"agent"` | First panel of that dock (`none`, `agent`, `browser`, `terminal`, `project`, `git`, `outline`, `debug`). |
| `default_stacked_secondary_panel` | `"browser"` | Second panel of that dock (same choices). |
| `tab_bar.show_buttons_on_inactive_panes` | `true` | Show the tab bar buttons on panes that are not focused as well. |
| `acknowledged_external_ai_providers` | `[]` | AI providers whose training-risk warning you have accepted. Written by the settings UI. |
| `auto_update_include_prereleases` | `false` | Whether update checks also consider pre-releases on GitHub (see [Installation and updates](install.md#updates)). |

### Editor and files

| Key | Default | Description |
|---|---|---|
| `toolbar.markdown_format_bar` | `true` | Show the Markdown formatting row below the editor toolbar while a Markdown file is open. |
| `hard_break_on_newline` | `false` (`true` for Markdown) | Pressing Enter first appends the two trailing spaces of a Markdown hard line break. |
| `markdown_preview.soft_break_as_hard_break` | `true` | Render a single newline in the Markdown preview as a line break. |
| `tabular_data.open_as_grid` | `true` | Open CSV and TSV files as a grid. |
| `tabular_data.multiline_cells` | `true` | Show cells that contain newlines in full by growing the row height. |
| `git.status_untracked_files` | `"all"` | How much git status reports about untracked files (`all`, `normal`, `no`). Also used on SSH remotes. |

### C and C++

| Key | Default | Description |
|---|---|---|
| `symbol_index.enabled` | `"auto"` | When to build the name-based symbol index: `auto` (once the project has C or C++ files), `on`, `off`. |
| `symbol_index.include_ignored` | `false` | Also index files excluded by `.gitignore` (only those the project panel has loaded). |
| `symbol_index.language_server_timeout_ms` | `1500` | How long "go to definition" waits for the language server before it also offers name matches. |
| `symbol_index.hover_type_contents` | `true` | Show what a type holds (struct members, enum constants, a macro's definition) in the hover. |
| `clangd.index_mode` | `"full"` | `full`: clangd indexes the whole project in the background. `light`: only open files. |
| `clangd.max_workers` | `0` | clangd's thread count (`-j`). `0` keeps clangd's default. |
| `clangd.memory_limit_mb` | `0` | Restart clangd when its memory goes over this many MB (checked every 30 s, at most once per 10 min). `0` never restarts. |
| `call_graph_panel.button` | `true` | Show the call graph panel button in the status bar. |
| `call_graph_panel.dock` | `"right"` | Dock of the call graph panel (`left`, `right`). |
| `call_graph_panel.default_width` | `320` | Default width of the call graph panel. |
| `call_graph_panel.max_children` | `200` | Callers or callees listed before a "Show more" row. |
| `call_graph_panel.follow_cursor` | `false` | Make the function under the cursor the root whenever the cursor moves. |
| `call_graph_panel.sources` | `"all"` | Where calls come from: `all`, `language_server`, `name_match`. |
| `call_graph_panel.excluded_callees` | `["printk", "pr_info", …]` | Callees left out of a function's callees (counted in a final row). |

### AI and agents

| Key | Default | Description |
|---|---|---|
| `agent.pause_worktree_code_after_agent_writes` | `true` | A project folder an agent wrote into stops running its own code (formatters, linters, interpreters, build scripts) until you review and resume it. Read only from the user settings file. |
| `agent.language_servers_allowed_while_paused` | `[]` | Extension language servers that keep running while a folder is paused. Read only from the user settings file. |
| `agent.starts_open` | `false` | Open the agent panel on startup. |
| `agent.sidebar_enabled` | `false` | Make the window-wide threads sidebar available. |
| `agent.panel_thread_history` | `true` | Show the thread history as a list inside the agent panel. |
| `agent.panel_tab_bar` | `"auto"` | When the agent panel shows its own tab bar (`auto`, `always`, `never`). |
| `agent.auto_compact_min_context_window` | `80000` | Smallest model context window (tokens) for which auto-compaction runs. Values below 8000 become 8000. |
| `agent.attach_active_file` | `false` | Attach the active editor (selection, or the whole file) to every message. |
| `agent.context_attachments.inline_max_bytes` | `16384` | Files larger than this are attached as an outline. |
| `agent.context_attachments.message_budget_bytes` | `65536` | When a message's attachments exceed this, the largest are reduced to an outline or a reference. |
| `agent.context_attachments.outline_max_bytes` | `8192` | Largest outline sent for one file. |
| `agent.context_attachments.fetch_max_bytes` | `32768` | Largest part of a fetched web page that is attached. |
| `agent.context_attachments.diagnostics_max_files` | `50` | Most files whose diagnostics are attached. |
| `agent.system_prompt_repo_map_bytes` | `0` | Byte budget for a repository map in the agent's system prompt. `0` disables it; at most 32768. |
| `agent.idle_disconnect_minutes` | `5` | Minutes an external agent stays connected after its last thread closes. `0` keeps it until the window closes. |
| `agent.claude_code_run_outside_sandbox` | `false` | "Launch Claude Code" always starts the CLI outside the sandbox, without asking. Read only from the user settings file. |
| `ide_server.enabled` | `false` | Expose ForgeKairos as a loopback IDE server for the Claude Code CLI. |
| `ide_server.assist_tools` | `true` | Give agents the IDE tools (highlights, annotations, tours, definitions, references, symbol search) under the MCP server name `zed`. |
| `ide_server.share_active_file` | `true` | Tell the connected Claude Code CLI about the open file even when nothing is selected. |
| `ide_server.share_task_output` | `true` | Let agents read the tail (up to 4 KB) of the output of the last failed task. |

The agent profiles also gain an **Edit Only** profile (`agent.profiles.edit`): it reads and
edits files but never runs commands or reaches the network, and leaves out deleting, moving
and copying paths. `agent.tool_permissions.default` accepts a fourth value, `"auto"` (see
[AI](ai.md)).

### Terminal

| Key | Default | Description |
|---|---|---|
| `terminal.macro_picker_folders_collapsed` | `false` | Open the macro picker ("Run Macro…") with every folder collapsed. |
| `terminal.session_log.auto` | `false` | Start logging newly opened terminals automatically. |
| `terminal.session_log.directory` | `""` | Folder for logs (`~` is expanded). Empty: `terminal-sessions` under the log directory. |
| `terminal.session_log.file_name_template` | `"{date}_{time}_{title}.log"` | Log file name. `{date}` `{time}` `{title}` `{host}` `{pid}` are expanded. |
| `terminal.session_log.strip_control_sequences` | `true` | Strip ANSI / VT escape sequences and save plain text. |
| `terminal.session_log.timestamps` | `false` | Prefix each line with `[YYYY-MM-DD HH:MM:SS] `. |
| `terminal.remote_persistence.enabled` | `true` | Keep SSH terminals alive across disconnects (remote hosts other than Windows). |
| `terminal.remote_persistence.detached_timeout_hours` | `24` | End a remote shell once no terminal has been attached to it for this many hours. `0` means never. |

### Built-in browser

| Key | Default | Description |
|---|---|---|
| `browser.button` | `true` | Show the browser panel button in the status bar. |
| `browser.dock` | `"right"` | Dock of the browser panel (`left`, `right`, `bottom`). |
| `browser.starts_open` | `false` | Open the browser panel on startup. |
| `browser.default_width` | `480` | Default width when docked left or right. |
| `browser.default_height` | `320` | Default height when docked at the bottom. |
| `browser.home_url` | `"https://m365.cloud.microsoft/chat"` | Start page. |
| `browser.search_url` | `"https://www.google.com/search?q={query}"` | URL used when the address bar input is not a URL. |
| `browser.copilot_url` | `"https://github.com/copilot"` | URL opened by "Ask Copilot" about the selected text. |
| `browser.use_egress_proxy` | `false` | Also restrict the built-in browser's traffic by the network allowlist. |

The browser panel uses Microsoft Edge WebView2 on Windows. On other platforms it shows a
"not supported" notice and a way to open an external browser.

### Remote and development tools

| Key | Default | Description |
|---|---|---|
| `remote.relay_http_through_client` | `true` | Route the remote host's downloads (language servers, node, extensions, agent binaries) through this machine. |
| `dev_tools.suggest_missing` | `true` | Notify about missing language servers and formatters, with a button to install them into your data folder. |
| `dev_tools.ignored` | `[]` | Tools not to suggest (for example `"clangd"`). |

## Zed settings with a different default

| Key | ForgeKairos | Zed |
|---|---|---|
| `agent.enabled` | `false` | `true` |
| `agent.dock` | `"right"` | `"left"` |
| `agent.default_model` | not set (the first authenticated provider is used) | `zed.dev` / `claude-sonnet-4` |
| `edit_predictions.provider` | `"none"` | `"zed"` |
| `edit_predictions.allow_data_collection` | `"no"` | `"default"` |
| `telemetry.diagnostics`, `telemetry.metrics` | `false`, and not used: nothing is sent to Zed whatever they say | `true` |
| `auto_update` | `false` | `true` |
| `auto_install_extensions` | `{}` | `{ "html": true }` |
| `project_panel.dock`, `outline_panel.dock`, `git_panel.dock`, `collaboration_panel.dock` | `"left"` | `"right"` |
| `project_panel.git_status_indicator` | `true` | `false` |
| `title_bar.show_menus` | `true` | `false` |
| `show_whitespaces` | `"all"` | `"selection"` |
| `terminal.starts_open` | `true` | `false` |
| `markdown_preview.limit_content_width` | `false` | `true` |
| `buffer_font_fallbacks` | `["BIZ UDGothic", "MS Gothic", "Yu Gothic UI"]` | `null` |
| `ui_font_fallbacks` | `["Yu Gothic UI", "Meiryo"]` | `null` |
| `languages.C.formatter`, `languages.C++.formatter` | clang-format with a GNU-based style | not set |
