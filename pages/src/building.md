# Building on Windows

[日本語](ja/building.md)

This page summarizes how to build ForgeKairos on Windows. Zed's own build notes are in
[Zed's development documentation](https://zed.dev/docs/development/windows).

## Requirements

- Visual Studio 2022 with the C++ workload, MSVC v143, the Spectre-mitigated libraries,
  the Windows 11 SDK 10.0.26100, and CMake (the copy that comes with Visual Studio works)
- rustup; the toolchain pinned by `rust-toolchain.toml` and its targets are installed on the
  first build
- Long paths enabled in Git (`core.longpaths`) and in Windows (`LongPathsEnabled`)
- `RUSTFLAGS` not set
- Plenty of free disk space: a full `target` folder can exceed 100 GB

## Building

```powershell
powershell -NoProfile -File script\build.ps1            # debug build in target\debug
powershell -NoProfile -File script\build.ps1 -Run       # build, then start it
powershell -NoProfile -File script\build.ps1 -Release   # release build in target\x86_64-pc-windows-msvc\release
```

The script:

- adds the CMake that comes with Visual Studio to `PATH` when CMake is missing
- warns when disk space is low
- builds with `-j 8` (`-Jobs` changes it); unlimited parallelism can make rustc run out of memory
- builds the remote server for Linux (`x86_64-unknown-linux-musl`, statically linked) with
  zig and `cargo-zigbuild`, installing them when missing (zig through `winget`), and places
  it next to the editor as `forgekairos-remote-server-linux-x86_64.gz`
- downloads `slirp4netns`, used by the agent sandbox on remote hosts
- places `clang-format.exe` in `bin\` next to the editor

It writes to `target\debug`, or `target\x86_64-pc-windows-msvc\release` with `-Release`; when `CARGO_TARGET_DIR`
is set, to that folder instead of `target`.

| Option | Effect |
|---|---|
| `-Release` | Build with the release profile into `target\x86_64-pc-windows-msvc\release`, exactly as the installer script does (see below). |
| `-Run` | Start the editor after a successful build. |
| `-Jobs <n>` | Build with `-j <n>` instead of `-j 8`. |
| `-FreeDisk` | First delete the `incremental` folder and the `.exe` and `.pdb` files in `deps` of the profile being built. |
| `-SkipCli` | Do not build the command-line tool. |
| `-SkipRemoteServer` | Do not build the remote server. |
| `-SkipSlirp4netns` | Do not download `slirp4netns`. |
| `-SkipClangFormat` | Do not place clang-format. |
| `-ClangFormatPath <file>` | The `clang-format.exe` to place. When it is not given or does not exist, the LLVM that comes with Visual Studio 2022 is used. |

The editor is `ForgeKairos.exe` and the command-line tool `forgekairos-cli.exe`, both in
`target\debug` (or `target\x86_64-pc-windows-msvc\release`); the command-line tool looks for the editor next to
itself. The installer installs the command-line tool as `bin\forgekairos.exe`; in the
build folder it has another name, because Windows does not tell `forgekairos.exe` from
`ForgeKairos.exe`.

A release build uses the stable channel's data folder, the same one an installed
ForgeKairos uses. To keep them apart, start it with `--user-data-dir <folder>`.

## Installer

`script\build.ps1` does not make an installer. `script\bundle-windows.ps1` does: it makes a
release build (for `x86_64-pc-windows-msvc`, into `target\x86_64-pc-windows-msvc\release`),
builds the Windows remote server, cross-builds the Linux one, adds clang-format with its
license and `slirp4netns`, and runs Inno Setup 6 (`winget install JRSoftware.InnoSetup`).
The installer is written to `target\forgekairos-windows-x86_64-setup.exe`. Options include
`-Architecture`, `-Install` (install the result), `-ReleaseTag <tag>` (the GitHub release tag
the build is published under, which the update check compares with), `-ClangFormatPath` and
`-Fast` (see below).
It needs Visual Studio 2022 and network access (it downloads ConPTY, the AMD GPU Services
library, `slirp4netns` and, when missing, zig).

```powershell
pwsh -NoProfile -File script\bundle-windows.ps1
```

The installer reuses a release build made by `script\build.ps1 -Release`: both build the
same packages (`zed`, `cli` and `auto_update_helper` in one cargo run) for the same target, in
the Visual Studio developer environment, with the release channel set. Anything that differs
(a plain `cargo build --release`, another set of packages, a build outside the developer
environment) makes cargo rebuild the C dependencies and the editor. With nothing changed
cargo takes seconds; after a commit, which changes the commit hash compiled into the
editor, recompiling and linking the editor with the release profile's thin LTO takes about
half an hour.

`-Fast` makes an installer to try out on this machine, not to publish: it builds with the
`release-fast` profile (no LTO, limited debug info) into
`target\x86_64-pc-windows-msvc\release-fast` and compresses with `lzma2/fast`. Its first
run builds every dependency again for that profile, which also takes disk space.

It needs PowerShell 7 (`pwsh`, `winget install Microsoft.PowerShell`). Started from Windows
PowerShell 5.1 (`powershell`), it runs itself again under `pwsh` with the same options, and
stops with an explanation when `pwsh` is not installed.

## Checks

The upstream rules still apply: use `./script/clippy` rather than `cargo clippy`. For
translation checks, see
[`locales/README.md`](https://github.com/SeiyaFunaokaJP/ForgeKairos/blob/main/locales/README.md#checking-translations).
