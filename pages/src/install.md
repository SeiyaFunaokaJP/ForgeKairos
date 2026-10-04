# Installation and updates

[日本語](ja/install.md)

## Windows installer

ForgeKairos is distributed as a Windows installer, `forgekairos-windows-<arch>-setup.exe`,
from the [GitHub Releases](https://github.com/SeiyaFunaokaJP/ForgeKairos/releases) of the
repository.

- It installs for the current user, without administrator rights, into a `ForgeKairos`
  folder under the user's programs folder.
- It requires 64-bit Windows 10 version 1709 or later.
- The installer offers: a desktop icon (off by default), **Open with ForgeKairos** in
  Explorer's right-click menu for files and for folders, file associations, and adding the
  command-line tool to `PATH`.
- The application is `ForgeKairos.exe`; the command-line tool is `bin\forgekairos.exe`.
- The installer includes clang-format (used to format C and C++) and the bundled remote
  server for Linux hosts.

On Windows 11, the **Open with ForgeKairos** entry may appear under "Show more options".

## Updates

Automatic updates are off (`auto_update` is `false`). To check for an update, use
**Check for Updates** in the application menu (or `auto update: check` in the command
palette). ForgeKairos looks for updates only in this repository's GitHub Releases, never in
Zed's. Release notes open the release page on GitHub.

Only releases are considered by default. To be offered pre-releases too, turn on
**Include Pre-releases** under Settings → General → Auto Update
(`"auto_update_include_prereleases": true`). Among the releases it considers, ForgeKairos
picks the one with the highest version in its tag, not the most recently published one.

An update is downloaded only from this repository's release downloads. When GitHub lists
a SHA-256 digest for the installer, an installer that does not match it is not installed.
When the check fails (GitHub cannot be reached, or the download does not match),
ForgeKairos says so instead of reporting that it is up to date. A release without an
installer for this system is skipped, as if there were no newer version.

## Moving from Zed-JP-Enhance

ForgeKairos was previously named Zed-JP-Enhance.

- **Install ForgeKairos by hand.** Zed-JP-Enhance cannot update itself to ForgeKairos.
  The ForgeKairos installer installs over it and removes its registrations.
- **Uninstall Zed-JP-Enhance and do not run both.** The older version's sandbox does not
  protect ForgeKairos's folders.
- **Your data moves on the first normal start.** ForgeKairos renames the old folders
  (`Zed-JP-Enhance` under `%APPDATA%` and `%LOCALAPPDATA%` on Windows) to its own; nothing
  is copied. Settings, the network allowlist, consents, databases and threads come along.
  The data is not moved when:
  - the new folder already has content,
  - Zed-JP-Enhance is running (this start keeps using the old folder and tells you so),
  - a rename fails (anything already moved is moved back, with a notice),
  - ForgeKairos is started with `--user-data-dir`, or as a helper process or the
    command-line tool.

  If Zed-JP-Enhance later creates its folder again, ForgeKairos tells you once.
- The C and C++ symbol index is rebuilt in a new format.
- The official Zed's folders are never read or moved.

## Data folders

| OS | Settings | Data |
|---|---|---|
| Windows | `%APPDATA%\ForgeKairos` | `%LOCALAPPDATA%\ForgeKairos` |
| Linux | `$XDG_CONFIG_HOME/forgekairos` | `$XDG_DATA_HOME/forgekairos` |
| macOS | `~/.config/forgekairos` | `~/Library/Application Support/ForgeKairos` |

Project settings stay in `.zed/` in each project, and environment variables keep their
`ZED_` names.

## Other operating systems

ForgeKairos is developed and tested on Windows. The packaging scripts for macOS and Linux
are kept, but those packages have not been built or tested.
