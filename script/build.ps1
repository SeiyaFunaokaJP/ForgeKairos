<#
.SYNOPSIS
Builds ForgeKairos on Windows: the editor (ForgeKairos.exe), the command-line
launcher (forgekairos-cli.exe) and the files shipped next to them. Debug by
default, release with -Release.

.DESCRIPTION
Works around the pitfalls described in .forgekairos-docs/BUILD-WINDOWS.md:
- Puts Visual Studio 2022's CMake on PATH when cmake.exe is not there.
- Warns when the disk is nearly full. -FreeDisk first removes what is safe to
  remove (the incremental cache, and the test executables and PDBs in deps).
- Builds with -j 8 unless -Jobs says otherwise: unlimited parallelism makes
  rustc run out of memory.
- Cross-builds the Linux remote server (static musl) for SSH and WSL remoting
  and puts it next to the editor as forgekairos-remote-server-linux-x86_64.gz,
  which the editor uploads to the host. Installs zig, cargo-zigbuild and the
  rustup target when they are missing (-SkipRemoteServer skips this).
- Downloads the static slirp4netns binary for the remote sandbox (ADR-F18) and
  puts it next to the editor (-SkipSlirp4netns skips this).
- Copies clang-format.exe, which the default C formatter runs, to bin\ next to
  the editor (-SkipClangFormat skips this).

Everything goes to <target>\debug, or <target>\release with -Release, where
<target> is $env:CARGO_TARGET_DIR or the repository's target folder.

A release build reads its release channel from crates\zed\RELEASE_CHANNEL
(stable), so running it from target\release uses the same data folder as an
installed ForgeKairos. Pass --user-data-dir <folder> to keep them apart.

This script does not make an installer. script\bundle-windows.ps1 does: it
makes its own release build for an explicit target triple, adds the Windows
remote server, the Explorer context menu package and ConPTY, and runs Inno
Setup 6 (ISCC.exe). Both scripts prepare the remote server, slirp4netns and
clang-format with the functions in script\lib\remote-artifacts.ps1.

.EXAMPLE
powershell -NoProfile -File script\build.ps1
powershell -NoProfile -File script\build.ps1 -Run
powershell -NoProfile -File script\build.ps1 -Release
powershell -NoProfile -File script\build.ps1 -Jobs 4 -FreeDisk
powershell -NoProfile -File script\build.ps1 -SkipRemoteServer -SkipSlirp4netns
powershell -NoProfile -File script\build.ps1 -ClangFormatPath C:\LLVM\bin\clang-format.exe
#>
param(
    # Build with the release profile into <target>\release.
    [switch]$Release,
    # Start the editor after a successful build.
    [switch]$Run,
    # Before building, remove the incremental cache and the test executables
    # and PDBs in deps.
    [switch]$FreeDisk,
    # Do not build the command-line launcher.
    [switch]$SkipCli,
    # Do not build the Linux remote server.
    [switch]$SkipRemoteServer,
    # Do not download slirp4netns for the remote sandbox (ADR-F18).
    [switch]$SkipSlirp4netns,
    # Do not copy clang-format.exe for the default C formatter.
    [switch]$SkipClangFormat,
    # Where to copy clang-format.exe from. When empty or missing, the LLVM that
    # comes with Visual Studio 2022 is used.
    [string]$ClangFormatPath = '',
    # cargo's -j.
    [ValidateRange(1, 256)]
    [int]$Jobs = 8
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

. "$PSScriptRoot\lib\remote-artifacts.ps1"

if ($env:CARGO_TARGET_DIR) {
    $targetRoot = if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path $repoRoot $env:CARGO_TARGET_DIR
    }
} else {
    $targetRoot = Join-Path $repoRoot 'target'
}
$cargoProfile = if ($Release) { 'release' } else { 'debug' }
$profileDir = Join-Path $targetRoot $cargoProfile
$cargoProfileArguments = if ($Release) { @('--release') } else { @() }

# These are the names crates/branding gives the cargo binaries
# (EDITOR_CARGO_BIN_NAME, CLI_CARGO_BIN_NAME) and the bundled remote server
# (remote_server_binary_prefix), which crates/remote looks for.
$editorExe = Join-Path $profileDir 'ForgeKairos.exe'
$remoteServerArtifact = Join-Path $profileDir 'forgekairos-remote-server-linux-x86_64.gz'

# --- CMake from Visual Studio ---------------------------------------------
# The same editions crates/remote/src/transport.rs (ensure_cmake_available)
# searches. Without CMake the build of wasmtime's dependencies fails, and so
# does the remote server build a development build runs on an SSH connection.
if (-not (Get-Command cmake.exe -ErrorAction SilentlyContinue)) {
    $vsCmakeCandidates = @('Enterprise', 'Professional', 'Community', 'BuildTools') | ForEach-Object {
        $edition = $_
        @($env:ProgramFiles, ${env:ProgramFiles(x86)}) | Where-Object { $_ } | ForEach-Object {
            Join-Path $_ "Microsoft Visual Studio\2022\$edition\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
        }
    }
    $found = $vsCmakeCandidates | Where-Object { Test-Path (Join-Path $_ 'cmake.exe') } | Select-Object -First 1
    if ($found) {
        $env:PATH = "$found;$env:PATH"
        Write-Host "Added CMake to PATH: $found"
    } else {
        Write-Warning 'cmake.exe not found. The build of the wasmtime dependencies will fail. See the prerequisites in .forgekairos-docs/BUILD-WINDOWS.md.'
    }
}

# --- Free disk space --------------------------------------------------------
function Get-FreeGB {
    $probe = if (Test-Path $targetRoot) { $targetRoot } else { $repoRoot }
    $drive = (Get-Item $probe).PSDrive.Name
    [math]::Round((Get-PSDrive $drive).Free / 1GB, 1)
}

if ($FreeDisk) {
    $incremental = Join-Path $profileDir 'incremental'
    if (Test-Path $incremental) {
        Write-Host "Removing $incremental ..."
        Remove-Item -Recurse -Force $incremental
    }
    $deps = Join-Path $profileDir 'deps'
    if (Test-Path $deps) {
        Write-Host "Removing the executables and PDBs in $deps (old test binaries) ..."
        Get-ChildItem $deps -File |
            Where-Object { $_.Extension -in '.exe', '.pdb' } |
            Remove-Item -Force
    }
    Write-Host ("Free space after cleaning: {0} GB" -f (Get-FreeGB))
}

$freeGB = Get-FreeGB
if ($freeGB -lt 20) {
    Write-Warning ("Only {0} GB of disk space is free. If the build runs out, run again with -FreeDisk." -f $freeGB)
}

# --- Linux remote server ----------------------------------------------------
# With the remote server next to the editor, connecting to an SSH or WSL host
# needs neither a step from the user nor network access (egress is blocked by
# default).
function Update-RemoteServerArtifact {
    $newestSource = Get-ChildItem -Path (Join-Path $repoRoot 'crates') -Recurse -File -Include '*.rs', 'Cargo.toml' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1 -ExpandProperty LastWriteTimeUtc
    $lockFile = Join-Path $repoRoot 'Cargo.lock'
    if ((Test-Path $lockFile) -and ((-not $newestSource) -or (Get-Item $lockFile).LastWriteTimeUtc -gt $newestSource)) {
        $newestSource = (Get-Item $lockFile).LastWriteTimeUtc
    }
    if ((Test-Path $remoteServerArtifact) -and $newestSource -and ((Get-Item $remoteServerArtifact).LastWriteTimeUtc -ge $newestSource)) {
        Write-Host "The Linux remote server is up to date: $remoteServerArtifact"
        return
    }
    Build-LinuxRemoteServer `
        -TargetDir (Join-Path $targetRoot 'remote_server') `
        -ArtifactPath $remoteServerArtifact `
        -Release:$Release `
        -Jobs $Jobs
}

# --- Build ------------------------------------------------------------------
Write-Host "cargo build$($cargoProfileArguments | ForEach-Object { " $_" }) -p zed -j $Jobs ..."
cargo build @cargoProfileArguments -p zed -j $Jobs
if ($LASTEXITCODE -ne 0) {
    Write-Error "Building zed failed (exit $LASTEXITCODE). See the pitfalls in .forgekairos-docs/BUILD-WINDOWS.md."
}

if (-not $SkipCli) {
    # `forgekairos <path>` in a console runs the launcher, which looks for the
    # editor next to itself.
    Write-Host "cargo build$($cargoProfileArguments | ForEach-Object { " $_" }) -p cli -j $Jobs ..."
    cargo build @cargoProfileArguments -p cli -j $Jobs
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Building cli failed (exit $LASTEXITCODE)."
    }
}

# The editor itself is built at this point, so the files shipped next to it
# only warn when they cannot be prepared.
if (-not $SkipRemoteServer) {
    try {
        Update-RemoteServerArtifact
    } catch {
        Write-Warning ("Could not prepare the Linux remote server: {0}" -f $_.Exception.Message)
        Write-Warning 'Connecting to an SSH or WSL host cannot install the server. Pass -SkipRemoteServer to skip it on purpose.'
    }
}

if (-not $SkipSlirp4netns) {
    try {
        Get-Slirp4netns -Destination (Join-Path $profileDir 'slirp4netns-linux-x86_64')
    } catch {
        Write-Warning ("Could not prepare slirp4netns: {0}" -f $_.Exception.Message)
        Write-Warning 'The remote sandbox (ADR-F18) cannot isolate agent commands in a network namespace. Pass -SkipSlirp4netns to skip it on purpose.'
    }
}

if (-not $SkipClangFormat) {
    try {
        Copy-ClangFormat -Destination (Join-Path $profileDir 'bin\clang-format.exe') -SourcePath $ClangFormatPath
    } catch {
        Write-Warning ("Could not prepare clang-format: {0}" -f $_.Exception.Message)
        Write-Warning 'Formatting C files does nothing without it (-ClangFormatPath tells where to copy it from).'
    }
}

# A build from before the rename left these here. They are not removed: the
# old zed.exe may be the one running. The new editor looks for the remote
# server under its own name first and takes the old archive only without it.
$legacyFiles = @('zed.exe', 'cli.exe', 'zed-remote-server-linux-x86_64.gz', 'zed-remote-server-linux-x86_64') |
    Where-Object { Test-Path -PathType Leaf (Join-Path $profileDir $_) }
if ($legacyFiles) {
    Write-Warning ("Left from a build before the rename in {0}: {1}. The editor is ForgeKairos.exe and the launcher forgekairos-cli.exe now; delete the old files once nothing runs zed.exe." -f $profileDir, ($legacyFiles -join ', '))
}

Write-Host ''
Write-Host ("Built: {0} ({1:N0} MB)" -f $editorExe, ((Get-Item $editorExe).Length / 1MB))

if ($Run) {
    Write-Host 'Starting it ...'
    Start-Process $editorExe
}
