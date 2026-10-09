# The files shipped next to the editor that script/build.ps1 and
# script/bundle-windows.ps1 both prepare: the Linux remote server and
# slirp4netns, which crates/remote uploads to SSH and WSL hosts, and
# clang-format, the default C/C++ formatter.
#
# Every function reports failure by throwing. Returning success as a value does
# not work: the output of cargo and rustup inside the function would join the
# returned value.

$LinuxRemoteServerTarget = 'x86_64-unknown-linux-musl'
$Slirp4netnsVersion = 'v1.2.3'
# From that release's SHA256SUMS, the same sum script/lib/remote-artifacts.sh checks.
$Slirp4netnsX86_64Sha256 = '2051ae25baba59c96f42bf2b7b7efa2eabb022e497b6c4b692b1428d2b2d1f5b'

function Get-ZigPath {
    $zig = Get-Command zig -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($zig) { return $zig.Source }

    # zig installed through winget is not always on PATH; look where winget puts it.
    $wingetCandidates = @(
        (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages\zig.zig_*\zig-*\zig.exe'),
        (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links\zig.exe')
    )
    foreach ($pattern in $wingetCandidates) {
        $found = Get-ChildItem -Path $pattern -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

function Install-Zig {
    if (-not (Get-Command winget.exe -ErrorAction SilentlyContinue)) {
        throw 'zig is not installed and winget is not available to install it. Install zig from https://ziglang.org/ and put it on PATH.'
    }
    Write-Host 'zig not found; installing it with winget (winget install -e --id zig.zig) ...'
    # winget exits non-zero when zig is already installed too, so the caller
    # looks for zig again instead of trusting the exit code.
    winget install -e --id zig.zig --accept-source-agreements --accept-package-agreements --silent | Out-Host
}

function Compress-ToGzip {
    param([string]$SourcePath, [string]$DestinationPath)

    $source = $null
    $destination = $null
    $gzipStream = $null
    try {
        $source = [IO.File]::OpenRead($SourcePath)
        $destination = [IO.File]::Create($DestinationPath)
        $gzipStream = New-Object System.IO.Compression.GZipStream($destination, [System.IO.Compression.CompressionMode]::Compress)
        $source.CopyTo($gzipStream)
    } finally {
        # GZipStream writes its footer when it is closed, so it goes before the file.
        if ($gzipStream) { $gzipStream.Dispose() }
        if ($destination) { $destination.Dispose() }
        if ($source) { $source.Dispose() }
    }
}

function Initialize-ZigBuild {
    $zigPath = Get-ZigPath
    if (-not $zigPath) {
        Install-Zig
        $zigPath = Get-ZigPath
    }
    if (-not $zigPath) {
        throw 'zig is still not found after installing it. Open a new shell or put zig on PATH.'
    }
    $zigDir = Split-Path -Parent $zigPath
    if (($env:PATH -split ';') -notcontains $zigDir) {
        $env:PATH = "$zigDir;$env:PATH"
        Write-Host "Added zig to PATH: $zigDir"
    }

    if (-not (Get-Command rustup -CommandType Application -ErrorAction SilentlyContinue)) {
        throw 'rustup is not installed (https://rustup.rs/).'
    }
    Write-Host "rustup target add $LinuxRemoteServerTarget ..."
    rustup target add $LinuxRemoteServerTarget
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $LinuxRemoteServerTarget failed (exit $LASTEXITCODE)."
    }

    if (-not (Get-Command cargo-zigbuild -CommandType Application -ErrorAction SilentlyContinue)) {
        Write-Host 'Installing cargo-zigbuild (cargo install cargo-zigbuild --locked) ...'
        cargo install cargo-zigbuild --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo install cargo-zigbuild failed (exit $LASTEXITCODE)."
        }
    }
}

# Cross-compiles remote_server for Linux and gzips it to $ArtifactPath, the
# name crates/remote looks for next to the editor and in its bin folder.
function Build-LinuxRemoteServer {
    param(
        # The cargo target directory of this build (cargo's --target-dir).
        [Parameter(Mandatory)][string]$TargetDir,
        [Parameter(Mandatory)][string]$ArtifactPath,
        [switch]$Release,
        # cargo's -j; 0 leaves it to cargo.
        [int]$Jobs = 0
    )

    Initialize-ZigBuild

    # A static musl binary runs whatever glibc the host has. strip=debuginfo
    # drops DWARF but keeps the symbol names, so backtraces still resolve: a
    # debug remote_server is 582 MB otherwise, and a development build installs
    # a new one on the host for every build (a 31 GB host ran out of space on
    # 2026-08-30). RUSTFLAGS is restored afterwards so that it cannot reach any
    # other cargo invocation.
    $previousRustFlags = $env:RUSTFLAGS
    $rustFlags = '-C target-feature=+crt-static -C strip=debuginfo'
    if ($previousRustFlags) { $rustFlags = "$previousRustFlags $rustFlags" }
    if ($env:ZED_ZSTD_MUSL_LIB) { $rustFlags = "$rustFlags -C link-arg=-L$env:ZED_ZSTD_MUSL_LIB" }

    # A release build embeds its assets already; debug-embed makes a debug
    # build embed them too, since the host has no checkout to read them from.
    $cargoArguments = @('zigbuild', '-p', 'remote_server', '--target', $LinuxRemoteServerTarget, '--target-dir', $TargetDir)
    if ($Release) {
        $cargoArguments += '--release'
        $cargoProfile = 'release'
    } else {
        $cargoArguments += @('--features', 'debug-embed')
        $cargoProfile = 'debug'
    }
    if ($Jobs -gt 0) { $cargoArguments += @('-j', $Jobs) }

    $env:RUSTFLAGS = $rustFlags
    try {
        Write-Host "cargo $($cargoArguments -join ' ') ..."
        & cargo @cargoArguments
        $buildExitCode = $LASTEXITCODE
    } finally {
        if ($null -eq $previousRustFlags) {
            if (Test-Path Env:\RUSTFLAGS) { Remove-Item Env:\RUSTFLAGS }
        } else {
            $env:RUSTFLAGS = $previousRustFlags
        }
    }
    if ($buildExitCode -ne 0) {
        throw "cargo zigbuild -p remote_server failed (exit $buildExitCode)."
    }
    $binaryPath = Join-Path $TargetDir "$LinuxRemoteServerTarget\$cargoProfile\remote_server"
    if (-not (Test-Path $binaryPath)) {
        throw "The remote_server build succeeded but its binary is missing: $binaryPath"
    }

    $artifactDir = Split-Path -Parent $ArtifactPath
    if (-not (Test-Path $artifactDir)) {
        New-Item -ItemType Directory -Force $artifactDir | Out-Null
    }
    Write-Host "Compressing remote_server to $ArtifactPath"
    Compress-ToGzip -SourcePath $binaryPath -DestinationPath $ArtifactPath
    # The uncompressed size is what the remote host's disk pays for.
    Write-Host ("Linux remote server: {0} ({1:N1} MB, {2:N1} MB uncompressed)" -f `
        $ArtifactPath, `
        ((Get-Item $ArtifactPath).Length / 1MB), `
        ((Get-Item $binaryPath).Length / 1MB))
}

# Downloads the static slirp4netns binary that ADR-F18's remote sandbox uses
# on a Linux host: agent commands run in their own network namespace, and
# slirp4netns gives them their only (proxied) way out. It is fetched once; the
# version is fixed. License: slirp4netns is GPLv2+ (compatible with this
# GPL-3 fork), its bundled libslirp MIT/BSD.
function Get-Slirp4netns {
    param([Parameter(Mandatory)][string]$Destination)

    # The binary is uploaded to remote hosts and runs there, so only the
    # published build is accepted.
    if ((Test-Path $Destination) -and (Get-FileHash -Algorithm SHA256 $Destination).Hash -eq $Slirp4netnsX86_64Sha256) {
        Write-Host "slirp4netns is already downloaded: $Destination"
        return
    }
    $destinationDir = Split-Path -Parent $Destination
    if (-not (Test-Path $destinationDir)) {
        New-Item -ItemType Directory -Force $destinationDir | Out-Null
    }
    $uri = "https://github.com/rootless-containers/slirp4netns/releases/download/$Slirp4netnsVersion/slirp4netns-x86_64"
    Write-Host ("Downloading slirp4netns {0} from {1}" -f $Slirp4netnsVersion, $uri)
    $previousProgress = $ProgressPreference
    # Drawing Invoke-WebRequest's progress makes a large download slow.
    $ProgressPreference = 'SilentlyContinue'
    $download = "$Destination.download"
    try {
        Invoke-WebRequest -Uri $uri -OutFile $download -UseBasicParsing
    } finally {
        $ProgressPreference = $previousProgress
    }
    $actual = (Get-FileHash -Algorithm SHA256 $download).Hash
    if ($actual -ne $Slirp4netnsX86_64Sha256) {
        Remove-Item -Force $download
        throw "The slirp4netns download from $uri has SHA-256 $actual, expected $Slirp4netnsX86_64Sha256."
    }
    Move-Item -Force -Path $download -Destination $Destination
    Write-Host ("slirp4netns: {0} ({1:N1} MB)" -f $Destination, ((Get-Item $Destination).Length / 1MB))
}

# The C / C++ defaults in assets/settings/default.json format through a bare
# "clang-format", which crates/util resolves next to the editor, then in its
# bin folder, then on PATH. Nothing else installs clang-format on a user's
# machine.
function Copy-ClangFormat {
    param(
        [Parameter(Mandatory)][string]$Destination,
        # Where to copy it from. When empty or missing, the LLVM that comes
        # with Visual Studio 2022's C++ workload is used.
        [string]$SourcePath = ''
    )

    if ($SourcePath -and (Test-Path -PathType Leaf $SourcePath)) {
        $source = (Resolve-Path $SourcePath).Path
    } else {
        $vsCandidates = @('Enterprise', 'Professional', 'Community', 'BuildTools') |
            ForEach-Object { "C:\Program Files\Microsoft Visual Studio\2022\$_\VC\Tools\Llvm\bin\clang-format.exe" }
        $source = $vsCandidates | Where-Object { Test-Path -PathType Leaf $_ } | Select-Object -First 1
        if (-not $source) {
            throw "clang-format.exe not found (tried -ClangFormatPath '$SourcePath' and Visual Studio 2022's VC\Tools\Llvm\bin). Pass -ClangFormatPath to point at one."
        }
    }

    $destinationDir = Split-Path -Parent $Destination
    if (-not (Test-Path $destinationDir)) {
        New-Item -ItemType Directory -Force $destinationDir | Out-Null
    }
    Copy-Item -Path $source -Destination $Destination -Force
    Write-Host ("clang-format: {0} <- {1} ({2:N1} MB)" -f $Destination, $source, ((Get-Item $Destination).Length / 1MB))
}
