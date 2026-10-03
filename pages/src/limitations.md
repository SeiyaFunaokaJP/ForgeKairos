# Known limitations

[日本語](ja/limitations.md)

## Platforms

- ForgeKairos is developed and tested on Windows. The macOS and Linux packages have not been
  built or tested.
- The built-in browser panel needs Microsoft Edge WebView2 and works only on Windows.
- The bundled remote server is for Linux x86_64 hosts.
- On Windows 11, **Open with ForgeKairos** may appear only under "Show more options".
- Collaboration (calls and screen sharing) is not included.

## Network control

- The proxy given to agents and MCP servers works only for programs that read the proxy
  environment variables.
- The allowlist does not restrict the editor's own tools (language servers, extensions,
  updates, Git) or, by default, the built-in browser.
- Outside the sandbox, an agent's network is restrained only as far as it honors the proxy
  settings.

## Translation

- Text sent to language models and log messages stay English.
- Some labels, such as tab titles, switch language only after a restart.
- A remote server uses one display language for all clients connected to it.

## C and C++

- The symbol index matches by name: a result can be an unrelated symbol of the same name.
  Results from the index are marked as name matches.
- After a language server restart, diagnostics for files that no one has open stay cleared.
  "Restart All Servers" with no file open leaves the servers stopped.
- With `clangd.index_mode` = `"light"`, references, call hierarchy and renames cover only open
  files.

## Remote development

- Terminals that survive disconnects work only on SSH hosts other than Windows, and not for
  tasks or agent shells.
- Folders watched by polling on Linux show external changes a few seconds late.

## Markdown, Mermaid and CSV

- Text colors and sizes use HTML `style` attributes, which GitHub does not render.
- Editing Mermaid labels by double-clicking in the preview works only for flowcharts, ER,
  state, class, sequence, pie and Gantt diagrams. The structured editor refuses to open a
  diagram whose syntax it cannot reproduce.
- The CSV grid has no formulas, number or date formatting, merged cells, or column
  reordering by dragging.
