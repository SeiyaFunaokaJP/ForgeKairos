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

## Other operating systems

`script/bundle.ps1` (PowerShell 7; on Linux and macOS also `script/bundle`, which runs it with
`pwsh`) makes the packages for every operating system from one command. `-Os` chooses which:
`windows`, `linux`, `macos` or `all`; without it, the system it runs on.

```powershell
pwsh -NoProfile -File script/bundle.ps1 -Os windows    # script\bundle-windows.ps1, see above
pwsh -NoProfile -File script/bundle.ps1 -Os linux      # on Windows: in Docker inside WSL
pwsh -NoProfile -File script/bundle.ps1 -Os macos -Wait
pwsh -NoProfile -File script/bundle.ps1 -Os all -DryRun
```

| OS | On Windows | On Linux | On macOS |
|---|---|---|---|
| Windows | `script\bundle-windows.ps1` | the workflow (only with `-Os all` or `-Ci`) | the workflow (only with `-Os all` or `-Ci`) |
| Linux | `script/bundle-linux` in a Docker container in WSL | `script/bundle-linux` (in a container with `-Container`) | the workflow, or a local Docker container with `-Container` |
| macOS | the workflow | the workflow | `script/bundle-mac` |

The packages are left in `target` under the names the update check looks for:
`forgekairos-windows-x86_64-setup.exe`, `forgekairos-linux-x86_64.tar.gz`,
`forgekairos-macos-aarch64.dmg` and `forgekairos-macos-x86_64.dmg`, plus the remote servers
`forgekairos-remote-server-linux-x86_64.gz` and `forgekairos-remote-server-macos-<arch>.gz`.

Like the Windows installer, the Linux and macOS packages carry what SSH, WSL and Docker
connections install on a Linux host: the Linux x86_64 remote server
(`forgekairos-remote-server-linux-x86_64.gz`) and `slirp4netns-linux-x86_64`. Without them the
editor would have to download the server, which the default network policy blocks. They sit
where the editor looks for them: in `libexec/` next to the editor in the Linux package, and in
`Contents/Resources` of the macOS app. clang-format is not included on Linux and macOS: the
editor runs the `clang-format` on `PATH`, from the distribution's package manager, Homebrew
(`brew install clang-format`) or Xcode.

Options: `-Architecture` (`x86_64` or `aarch64` of what is built on this machine; by default
this machine's), `-MacosArchitecture` (`both`, `aarch64` or `x86_64`: what the workflow builds
for macOS; it builds Windows and Linux for x86_64), `-ReleaseTag`
(as for the installer), `-Jobs <n>` (cargo's `-j`), `-Ci` (make everything through the
workflow), `-Wait` (wait for the workflow and download what it made into
`target/ci-<run id>/`), `-Ref` (the branch or tag the workflow builds; by default the current
branch), `-DryRun` (print the commands instead of running them), and for Windows `-Fast`,
`-Install` and `-ClangFormatPath`, which it hands to `script\bundle-windows.ps1`.

### Linux

On Linux, install the build dependencies with `script/linux` first; the package then links
against the host's glibc and runs only on systems with that version or newer. On Windows the
script builds in an `ubuntu:22.04` container (glibc 2.35) of the Docker in WSL
(`wsl.exe -e docker`), so the package runs on Ubuntu 22.04, Debian 12 and newer. The image
installs the dependencies with `script/linux` and the toolchain of `rust-toolchain.toml`.
`-LinuxImage` chooses another base image, `-Container` uses a container on a Linux host too.

The checkout is mounted into the container, so the build uses your working tree (including
uncommitted changes). The cargo registry and the target folder live in the Docker volumes
`forgekairos-bundle-cargo-ubuntu-22.04` and `forgekairos-bundle-target-ubuntu-22.04` inside
WSL, not under `target`; remove them with `wsl.exe -e docker volume rm` to free the space
(the target folder takes some tens of GB). The first run builds the image and every
dependency; later runs only rebuild what changed.

### macOS

macOS packages are made on a Mac (Xcode, `script/bundle-mac`; zig, `brew install zig`, to
cross-build the Linux remote server with `cargo-zigbuild`, which the script installs when
missing), or by the GitHub Actions workflow below, which `script/bundle.ps1 -Os macos` starts on any other system with the
GitHub CLI (`gh workflow run`, after `gh auth login`). The workflow builds the branch as it
is on GitHub, so push first. Without signing secrets the app is signed ad hoc and macOS
refuses to open the downloaded app the first time: allow it under System Settings, Privacy &
Security, "Open Anyway", or remove the quarantine flag with
`xattr -dr com.apple.quarantine /Applications/ForgeKairos.app`. With the secrets
`MACOS_SIGNING_IDENTITY`, `MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`,
`APPLE_NOTARIZATION_KEY`, `APPLE_NOTARIZATION_KEY_ID` and `APPLE_NOTARIZATION_ISSUER_ID`
set in the repository, it is signed with that Developer ID and notarized.

### The release workflow

`.github/workflows/forgekairos_release.yml` builds on GitHub-hosted runners: Windows x86_64
on `windows-2022`, Linux x86_64 on `ubuntu-22.04`, and macOS aarch64 and x86_64 on `macos-14`
(x86_64 cross-compiled). Each job runs `script/bundle.ps1 -Os <os>` and uploads its package
as an artifact named after the file. It runs only when started (from the Actions tab with
the inputs `os`, `macos_arch` and `release_tag`, or by `script/bundle.ps1`) and when a `v*`
tag is pushed; for a tag, a last job creates a draft GitHub release with all the packages
for you to check and publish.

The repository is private, so the workflow uses the account's billed Actions minutes, and
macOS minutes count ten times. A full run takes hours per job on the standard runners (a
release build with thin LTO), so start only the systems you need (`-Os macos`, the
`macos_arch` input). The jobs cache the cargo registry and the build with
`Swatinem/rust-cache`, which makes a rerun without dependency changes cheaper; GitHub keeps
at most 10 GB of caches per repository and drops the oldest first.

## Making a release

1. Choose the tag: `v` and the version in `crates/zed/Cargo.toml` (`v1.22.0`). A pre-release
   adds a suffix (`v1.22.0-pre`, `v1.22.0-pre.2`); a tag with a `-` is published as a
   pre-release, which the update check offers only to those who turned on
   `auto_update_include_prereleases`. Pass the tag as `-ReleaseTag` to every build: it is
   compiled in, and the update check compares the releases on GitHub with it.
2. Build Windows and Linux here:

   ```powershell
   pwsh -NoProfile -File script/bundle.ps1 -Os windows -ReleaseTag v1.22.0
   pwsh -NoProfile -File script/bundle.ps1 -Os linux -ReleaseTag v1.22.0
   ```

3. Build macOS through the workflow (push the commit first; billed minutes):
   `pwsh -NoProfile -File script/bundle.ps1 -Os macos -ReleaseTag v1.22.0 -Wait` downloads
   the disk images and the macOS remote servers into `target/ci-<run id>/`.
4. Create a draft release and upload the packages under their names (the update check finds
   them by these names):

   ```powershell
   gh release create v1.22.0 --repo SeiyaFunaokaJP/ForgeKairos --draft --target (git rev-parse HEAD) --title "ForgeKairos v1.22.0" --generate-notes
   gh release upload v1.22.0 --repo SeiyaFunaokaJP/ForgeKairos `
       target/forgekairos-windows-x86_64-setup.exe `
       target/forgekairos-linux-x86_64.tar.gz target/forgekairos-remote-server-linux-x86_64.gz `
       (Get-ChildItem -Recurse -File target/ci-<run id>).FullName
   ```

   Add `--prerelease` for a pre-release. Check the draft, then publish it on GitHub, which
   creates the tag.

Pushing the tag instead (`git push origin v1.22.0`) makes the workflow build every system and
create the draft release itself; that costs the most minutes. Publishing a draft creates the
tag, which can start the workflow too: cancel that run (`gh run cancel`) when the packages
are already uploaded.

The update check reads the releases without signing in, so it sees nothing while the
repository is private.

## Checks

The upstream rules still apply: use `./script/clippy` rather than `cargo clippy`. For
translation checks, see
[`locales/README.md`](https://github.com/SeiyaFunaokaJP/ForgeKairos/blob/main/locales/README.md#checking-translations).
