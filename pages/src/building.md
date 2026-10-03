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

## Development build

```powershell
powershell -NoProfile -File script\build-dev.ps1        # the editor, the CLI and the bundled remote server
powershell -NoProfile -File script\build-dev.ps1 -Run   # build, then start it
```

The script:

- adds the CMake that comes with Visual Studio to `PATH` when CMake is missing
- warns when disk space is low
- builds with `-j 8`; unlimited parallelism can make rustc run out of memory
- builds the remote server for Linux (`x86_64-unknown-linux-musl`, statically linked) with
  zig and `cargo-zigbuild`, installing them when missing (zig through `winget`), and places
  it next to the editor
- downloads `slirp4netns`, used by the agent sandbox on remote hosts
- places `clang-format.exe` in `target\debug\bin\`

| Option | Effect |
|---|---|
| `-Run` | Start the editor after a successful build. |
| `-FreeDisk` | First delete `target\debug\incremental` and the `.exe` and `.pdb` files in `target\debug\deps`. |
| `-SkipCli` | Do not build the command-line tool. |
| `-SkipRemoteServer` | Do not build the remote server. |
| `-SkipSlirp4netns` | Do not download `slirp4netns`. |
| `-SkipClangFormat` | Do not place clang-format. |
| `-ClangFormatPath <file>` | The `clang-format.exe` to place. When it is not given or does not exist, the LLVM that comes with Visual Studio 2022 is used. |

The editor is `target\debug\zed.exe`. To use it from a console, `cli.exe` must be next to it
(the script builds both).

## Installer

`script\bundle-windows.ps1` makes a release build, cross-builds the Linux remote server,
adds clang-format with its license and `slirp4netns`, and runs Inno Setup 6
(`winget install JRSoftware.InnoSetup`). Options include `-Architecture`, `-Install` (install
the result) and `-ClangFormatPath`.

## Checks

The upstream rules still apply: use `./script/clippy` rather than `cargo clippy`. For
translation checks, see
[`locales/README.md`](https://github.com/SeiyaFunaokaJP/ForgeKairos/blob/main/locales/README.md#checking-translations).
