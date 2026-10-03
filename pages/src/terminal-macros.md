# Terminal macros and sessions

[日本語](ja/terminal-macros.md)

ForgeKairos adds terminal tools in the style of Tera Term: macros that send text and wait for
output, quick connections over SSH and Telnet, session logs, and file transfer.

| Action | Command | Windows / Linux | macOS |
|---|---|---|---|
| Run a macro | `terminal: run macro` | `ctrl-shift-alt-m` | `cmd-ctrl-alt-m` |
| Edit macros | `terminal: edit macros` | | |
| Quick connect | `terminal: quick connect` | `ctrl-shift-alt-k` | `cmd-ctrl-alt-k` |

## Macros

A macro is a list of steps run against the active terminal.

| Step | What it does |
|---|---|
| Send | Sends text to the terminal. |
| Wait for Text | Waits for a substring or a regular expression in the output, with a timeout. |
| Sleep | Waits for a fixed time. |
| Clear Screen | Clears the terminal. |
| Repeat | Repeats the steps inside it. |
| Branch | Branches on the output, the screen or a variable. |
| Set Variable | Sets a variable. |

### Variables

`${name}` is replaced in the text to send and in patterns. An undefined variable stays as
written, so a typo stays visible; write `$${` for a literal `${`. A value inserted into a
regular expression is escaped.

A variable gets its value from:

- **Capture Into** on a Wait for Text step, or on a Branch step (on its "Then" side)
- a named group of a regular expression, `(?P<name>…)`
- a Set Variable step

The editor warns about a `${name}` that has no value yet at that point of the macro, and
shows regular expression errors in place. **How Branches and Variables Work** in the editor
explains them with examples.

### Branches

- **Branch** matches against the output since the last Send (default), the whole screen, or a
  variable. An unset variable takes the "Else" side. Branch does not wait; put a Wait for
  Text step in front of it when needed.
- **Wait for Text** can have an **On Timeout** branch instead of stopping the macro.
- Branches can be nested.

### The macro editor

The macro editor opens as a tab in the center pane. Opening a macro that is already open
switches to its tab.

- Each step is a card. Click the header to fold or unfold it; **Expand All** and
  **Collapse All** act on every card.
- The **+** on the line between cards inserts a step. Drag a card's header onto a **+** to
  move it, also into or out of a branch.
- A branch shows its two sides as columns ("Then" and "Else"), stacked when the tab is
  narrow.
- Move up, move down and delete are in each card's **…** menu and its right-click menu.
- A folded card with an error shows an icon.

Unsaved changes show a dot on the tab, and closing asks whether to save. Tabs of macros that
were open come back after a restart, without unsaved changes. When the macro file changes
outside the editor, a tab without unsaved changes follows it; a tab with unsaved changes asks
before overwriting.

### Folders

Macros are kept in folders. Folders can be empty, have an order, and two folders can hold
macros of the same name. Add a folder with **New Folder…**; a folder's right-click menu creates,
renames, moves and deletes. When a folder that is not empty is deleted, you choose whether
its macros move to Unfiled or are deleted. Folders do not nest. The **Run Macro…** picker
groups macros by folder (`terminal.macro_picker_folders_collapsed` opens it with every
folder collapsed).

### Where macros are saved

Macros, folders and quick connections are saved in `terminal_sessions.json` in the config
directory (`%APPDATA%\ForgeKairos` on Windows, `~/.config/forgekairos` on macOS and Linux).
Writes are atomic, and edits to the file are picked up while ForgeKairos runs.

## Quick connect

Quick connect opens an SSH or Telnet session in a new terminal tab, with a host, port, user,
an optional macro to run after connecting, and logging. If Windows's `telnet.exe` is not
installed, ForgeKairos tells you how to enable it.

Passwords can be remembered (**Remember password**, off by default). They are kept in the
operating system's credential store (on Windows, Credential Manager), never in the JSON file.
SSH receives the password through askpass on the first prompt only; Telnet watches for a
password prompt once.

## Session logs

A terminal's output can be saved to a log file (toggle, start, stop, reveal the file, or
open it in the editor following its end). Logging pauses while a remembered password is
entered. See the `terminal.session_log` settings in the [Settings reference](settings.md).

## File transfer

**File Transfer…** in the terminal's right-click menu (or `terminal: open file transfer`)
opens a two-pane window for SFTP, SCP, FTP and FTPS. It runs the operating system's own
`sftp`, `scp` and `curl`. The target is taken from the active terminal. In an SSH remote
project, the local side can be the remote host. Transfers are your actions; agents cannot
start them.
