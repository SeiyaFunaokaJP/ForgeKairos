# C and C++ code navigation

[日本語](ja/editor.md)

ForgeKairos adds a name-based symbol index for C and C++, a call graph panel, the contents of
types in the hover, and settings that limit clangd's memory. For language servers in
general, see [Zed's C and C++ documentation](https://zed.dev/docs/languages/c).

## Symbol index

In a project with C or C++ files, ForgeKairos builds an index of names
(`symbol_index.enabled` = `"auto"`). The index needs no compilation information and runs
nothing; it is also built in projects you have not trusted. It holds functions and
prototypes, `#define`s, structs, unions, enums and typedefs, enum constants, globals,
direct and member calls, function pointer assignments and calls inside macro bodies.

- The index is stored in the app's data folder, one per project folder. In measurements it
  took roughly half the size of the sources.
- Indexes not used for 90 days are deleted.
- Results that come from the index are marked as name matches, because a name can match an
  unrelated symbol of the same name.

### Go to definition

In C and C++, when the language server returns nothing, fails, or takes longer than
`symbol_index.language_server_timeout_ms` (1500 ms), go to definition uses the index. One
match jumps there directly, with a short notice; several matches are listed. The fallback
does not apply inside comments and strings. It works for F12, Ctrl+click and hover links.

### Symbol search

Project symbol search (`ctrl-t`) shows the index's answers first and adds the language
server's results when they arrive, without duplicates. Qualified C++ names such as
`ns::f` can be searched.

## Call graph panel

The call graph panel shows a tree of the callers or callees of a function. It asks one
level at a time, only when you expand a node. By default the root does not follow the
cursor; you choose it.

| Action | Windows / Linux | macOS |
|---|---|---|
| Toggle the panel's focus | `ctrl-k ctrl-g` | `cmd-k cmd-g` |
| Show the callers of the function at the cursor | `ctrl-k ctrl-shift-g` | `cmd-k cmd-shift-g` |

In the panel, Left collapses and Right expands a row, Space previews the call site,
`ctrl-c` (`cmd-c`) copies the name, and `alt-left` / `alt-right` move through the history of
roots. Pressing Enter again on a row with several call sites moves to the next one.

- Index answers appear first; the language server's answers are merged in when they arrive.
- Recursion is marked. Indirect calls through function pointers list their candidates.
- When a name has several definitions, the panel lists the candidates for the root.
- Callees listed in `call_graph_panel.excluded_callees` (such as `printk` and `printf`) are
  counted in a final row and shown on request.

See the `call_graph_panel` settings in the [Settings reference](settings.md).

## Type contents in the hover

Hovering a name in C or C++ also shows what its type holds, below the language server's
hover:

- the name, its kind and where it is defined
- the chain of typedefs (for example `alias_t → outer_t → struct outer`)
- the members of a struct, union or class, expandable level by level
- the constants of an enum, and the text of a macro
- a switch between definitions of the same name

The contents are found by name in the index, not resolved by the language server. Turn
them off with `symbol_index.hover_type_contents`.

## clangd memory

| Setting | Effect |
|---|---|
| `clangd.index_mode` | `"full"` (default) indexes the whole project in the background. `"light"` indexes only open files and uses the least memory; references, call hierarchy and renames then cover only open files. |
| `clangd.max_workers` | clangd's thread count (`-j`). `0` keeps clangd's default. |
| `clangd.memory_limit_mb` | Restart clangd when its memory, checked every 30 seconds, goes over this many MB. A second time within 10 minutes only notifies. `0` never restarts. |

- The defaults add no arguments. If your `lsp.clangd.binary.arguments` already has the same
  kind of flag, nothing is added. Changing a setting restarts clangd.
- `memory_limit_mb` and `max_workers` are read only from your user settings.
- Memory is measured over clangd and all its child processes. The language server menu
  shows it.
- The notification for a memory limit offers to switch to light mode.

## AI tools

The built-in agent has `call_hierarchy` and `type_layout`; agents connected through the IDE
server have `getCallHierarchy` and `getTypeLayout` (see [AI](ai.md)). Both are read-only.
Names from files hidden from agents are withheld.

## C and C++ formatting

C and C++ files (including `.h`) are formatted with clang-format and a style based on GNU:
a space before parentheses, 120 columns, no single-line blocks or functions, indented case
labels, and includes left unsorted. "Format Selections" formats only the selected lines.
clang-format is looked up in the app's folder, its `bin` folder, the copy installed from a
missing-tool notification, and then `PATH`. The Windows installer includes clang-format.
