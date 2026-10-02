[CmdletBinding()]
Param(
    [Parameter()][Alias('i')][switch]$Install,
    [Parameter()][Alias('h')][switch]$Help,
    [Parameter()][Alias('a')][string]$Architecture,
    [Parameter()][string]$Name,
    # Source of the clang-format.exe shipped in {app}\bin (see CopyClangFormat).
    # Defaults to this fork's reference copy; falls back to VS 2022's bundled LLVM.
    [Parameter()][string]$ClangFormatPath = "I:\CustomFormatter\bin\win32\clang-format.exe"
)

. "$PSScriptRoot/lib/workspace.ps1"

# https://stackoverflow.com/questions/57949031/powershell-script-stops-if-program-fails-like-bash-set-o-errexit
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$buildSuccess = $false
$canCodeSign = $false

$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture" }
}

$Architecture = if ($Architecture) {
    $Architecture
} else {
    $OSArchitecture
}

$CargoOutDir = "./target/$Architecture-pc-windows-msvc/release"

function Get-VSArch {
    param(
        [string]$Arch
    )

    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

function Find-VsDevShell {
    # This fork is developed on VS 2022 Enterprise, while CI and other machines
    # may only have Professional, Community, or the standalone Build Tools.
    # Probe the known editions in descending order, then fall back to vswhere
    # so a non-default install location is still found.
    $base = "C:\Program Files\Microsoft Visual Studio\2022"
    foreach ($edition in @("Enterprise", "Professional", "Community", "BuildTools")) {
        $candidate = Join-Path $base "$edition\Common7\Tools\Launch-VsDevShell.ps1"
        if (Test-Path $candidate) {
            return $candidate
        }
    }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $installPath = & $vswhere -latest -products * -property installationPath 2>$null | Select-Object -First 1
        if ($installPath) {
            $candidate = Join-Path $installPath "Common7\Tools\Launch-VsDevShell.ps1"
            if (Test-Path $candidate) {
                return $candidate
            }
        }
    }
    throw "Could not find Launch-VsDevShell.ps1. Install Visual Studio 2022 (Enterprise/Professional/Community) or the Build Tools with the C++ workload."
}

Push-Location
$vsDevShell = Find-VsDevShell
& $vsDevShell -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture)
Pop-Location

$target = "$Architecture-pc-windows-msvc"

if ($Help) {
    Write-Output "Usage: test.ps1 [-Install] [-Help]"
    Write-Output "Build the installer for Windows.\n"
    Write-Output "Options:"
    Write-Output "  -Architecture, -a Which architecture to build (x86_64 or aarch64)"
    Write-Output "  -Install, -i      Run the installer after building."
    Write-Output "  -Help, -h         Show this help message."
    exit 0
}

Push-Location -Path crates/zed
$channel = Get-Content "RELEASE_CHANNEL"
$env:ZED_RELEASE_CHANNEL = $channel
$env:RELEASE_CHANNEL = $channel
Pop-Location

function CheckEnvironmentVariables {
    if(-not $env:CI) {
        return
    }

    $requiredVars = @('ZED_WORKSPACE', 'RELEASE_VERSION', 'ZED_RELEASE_CHANNEL')

    foreach ($var in $requiredVars) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($var))) {
            Write-Error "$var is not set"
            exit 1
        }
    }

    # On PRs from forks the signing secrets are not populated,
    # so skip code signing instead of failing, like bundle-mac does.
    $signingVars = @(
        'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET',
        'ACCOUNT_NAME', 'CERT_PROFILE_NAME', 'ENDPOINT',
        'FILE_DIGEST', 'TIMESTAMP_DIGEST', 'TIMESTAMP_SERVER'
    )

    $missingVars = @($signingVars | Where-Object { [string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($_)) })
    if ($missingVars.Count -eq 0) {
        $script:canCodeSign = $true
    } else {
        Write-Host "====== WARNING ======"
        Write-Host "One or more of the following variables are missing: $($missingVars -join ', ')"
        Write-Host "This bundle will not be code signed"
        Write-Host "====== WARNING ======"
    }
}

function PrepareForBundle {
    if (Test-Path "$innoDir") {
        Remove-Item -Path "$innoDir" -Recurse -Force
    }
    New-Item -Path "$innoDir" -ItemType Directory -Force
    Copy-Item -Path "$env:ZED_WORKSPACE\crates\zed\resources\windows\*" -Destination "$innoDir" -Recurse -Force
    New-Item -Path "$innoDir\make_appx" -ItemType Directory -Force
    New-Item -Path "$innoDir\appx" -ItemType Directory -Force
    New-Item -Path "$innoDir\bin" -ItemType Directory -Force
    New-Item -Path "$innoDir\tools" -ItemType Directory -Force

    rustup target add $target
}

function GenerateLicenses {
    . $PSScriptRoot/generate-licenses.ps1
}

function BuildZedAndItsFriends {
    Write-Output "Building Zed and its friends, for channel: $channel"
    # Build zed.exe, cli.exe and auto_update_helper.exe
    cargo --config .cargo/bundle-config.toml build --release --package zed --package cli --package auto_update_helper --target $target
    # The cargo binary stays "zed"; the installed executable is named after
    # branding::CURRENT.executable_stem.
    Copy-Item -Path ".\$CargoOutDir\zed.exe" -Destination "$innoDir\ForgeKairos.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\cli.exe" -Destination "$innoDir\cli.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\auto_update_helper.exe" -Destination "$innoDir\auto_update_helper.exe" -Force
    # Build explorer_command_injector.dll
    switch ($channel) {
        "stable" {
            cargo --config .cargo/bundle-config.toml build --release --features stable --no-default-features --package explorer_command_injector --target $target
        }
        "preview" {
            cargo --config .cargo/bundle-config.toml build --release --features preview --no-default-features --package explorer_command_injector --target $target
        }
        # No channel feature selects the dev CLSID and "{regValueName}ContextMenu"
        # key, which match the dev installer and AppxManifest-Dev.xml.
        "dev" {
            cargo --config .cargo/bundle-config.toml build --release --no-default-features --package explorer_command_injector --target $target
        }
        default {
            cargo --config .cargo/bundle-config.toml build --release --package explorer_command_injector --target $target
        }
    }
    Copy-Item -Path ".\$CargoOutDir\explorer_command_injector.dll" -Destination "$innoDir\zed_explorer_command_injector.dll" -Force
}

function BuildRemoteServer {
    Write-Output "Building remote_server for $target"
    cargo --config .cargo/bundle-config.toml build --release --package remote_server --target $target

    # Create zipped remote server binary
    $remoteServerSrc = (Resolve-Path ".\$CargoOutDir\remote_server.exe").Path

    if ($canCodeSign) {
        Write-Output "Code signing remote_server.exe"
        & "$innoDir\sign.ps1" $remoteServerSrc
    }

    $remoteServerDst = "$env:ZED_WORKSPACE\target\zed-remote-server-windows-$Architecture.zip"
    Write-Output "Compressing remote_server to $remoteServerDst"
    Compress-Archive -Path $remoteServerSrc -DestinationPath $remoteServerDst -Force

    # SSH-to-Windows remoting uploads this zip from {app}\bin (crates/remote's
    # bundled_server_search_dirs); without it the connection falls back to a
    # download, which this fork's egress policy blocks by default. The copy in
    # target\ stays for release uploads.
    Copy-Item -Path $remoteServerDst -Destination "$innoDir\bin\zed-remote-server-windows-$Architecture.zip" -Force

    Write-Output "Remote server compressed successfully"
}

function Get-ZigPath {
    $zig = Get-Command zig -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($zig) { return $zig.Source }

    # winget-installed zig is not always on PATH; probe its known locations.
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
        throw "zig not found and winget is unavailable; install zig from https://ziglang.org/ and put it on PATH."
    }
    Write-Output "zig not found; installing via winget (winget install -e --id zig.zig) ..."
    # winget returns non-zero even when already installed, so we re-probe for the
    # binary afterwards instead of trusting the exit code.
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
        # GZipStream writes its footer on close, so dispose it before destination.
        if ($gzipStream) { $gzipStream.Dispose() }
        if ($destination) { $destination.Dispose() }
        if ($source) { $source.Dispose() }
    }
}

function BuildLinuxRemoteServer {
    # SSH-to-Linux remoting needs a Linux remote_server binary shipped alongside
    # ForgeKairos.exe. This fork blocks egress by default, so the server cannot be fetched
    # on demand; crates/remote/src/transport.rs uploads a bundled artifact named
    # "zed-remote-server-linux-x86_64.gz" found in {app} or {app}\bin.
    #
    # This mirrors BuildRemoteServer (a plain --release build) but cross-compiles
    # to musl with cargo-zigbuild for a single static binary independent of the
    # target host's glibc. We deliberately do NOT pass --features debug-embed:
    # that feature only forces rust-embed to embed assets in DEBUG builds (used by
    # script/build-dev.ps1); a --release build embeds already, exactly like the
    # canonical Windows BuildRemoteServer above.
    $linuxTarget = "x86_64-unknown-linux-musl"
    $linuxTargetDir = "$env:ZED_WORKSPACE\target\remote_server_linux"
    $binaryPath = "$linuxTargetDir\$linuxTarget\release\remote_server"
    $artifactPath = "$innoDir\bin\zed-remote-server-linux-x86_64.gz"

    $zigPath = Get-ZigPath
    if (-not $zigPath) {
        Install-Zig
        $zigPath = Get-ZigPath
    }
    if (-not $zigPath) {
        throw "zig still not found after install; open a new shell or put zig on PATH."
    }
    $zigDir = Split-Path -Parent $zigPath
    if (($env:PATH -split ';') -notcontains $zigDir) {
        $env:PATH = "$zigDir;$env:PATH"
        Write-Output "Added zig to PATH: $zigDir"
    }

    Write-Output "rustup target add $linuxTarget ..."
    rustup target add $linuxTarget
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $linuxTarget failed (exit $LASTEXITCODE)."
    }

    if (-not (Get-Command cargo-zigbuild -CommandType Application -ErrorAction SilentlyContinue)) {
        Write-Output "Installing cargo-zigbuild (cargo install cargo-zigbuild --locked) ..."
        cargo install cargo-zigbuild --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo install cargo-zigbuild failed (exit $LASTEXITCODE)."
        }
    }

    # musl static link; strip=debuginfo drops DWARF but keeps symbol names (so
    # backtraces still resolve). Scoped to this build and restored afterwards so
    # it cannot leak into any other cargo invocation.
    $previousRustFlags = $env:RUSTFLAGS
    $rustFlags = "-C target-feature=+crt-static -C strip=debuginfo"
    if ($previousRustFlags) { $rustFlags = "$previousRustFlags $rustFlags" }
    if ($env:ZED_ZSTD_MUSL_LIB) { $rustFlags = "$rustFlags -C link-arg=-L$env:ZED_ZSTD_MUSL_LIB" }

    $env:RUSTFLAGS = $rustFlags
    try {
        Write-Output "cargo zigbuild -p remote_server --release --target $linuxTarget ..."
        cargo zigbuild -p remote_server --release --target $linuxTarget --target-dir $linuxTargetDir
        $buildExitCode = $LASTEXITCODE
    } finally {
        if ($null -eq $previousRustFlags) {
            if (Test-Path Env:\RUSTFLAGS) { Remove-Item Env:\RUSTFLAGS }
        } else {
            $env:RUSTFLAGS = $previousRustFlags
        }
    }
    if ($buildExitCode -ne 0) {
        throw "cargo zigbuild -p remote_server --release failed (exit $buildExitCode)."
    }
    if (-not (Test-Path $binaryPath)) {
        throw "Linux remote_server build reported success but the artifact is missing: $binaryPath"
    }

    Write-Output "Compressing Linux remote_server to $artifactPath"
    Compress-ToGzip -SourcePath $binaryPath -DestinationPath $artifactPath
    Write-Output ("Linux remote_server: {0} ({1:N1} MB compressed / {2:N1} MB uncompressed)" -f `
        $artifactPath, `
        ((Get-Item $artifactPath).Length / 1MB), `
        ((Get-Item $binaryPath).Length / 1MB))
}

function DownloadSlirp4netns {
    # ADR-F18 remote sandbox: on a Linux remote, agent commands run in a netns and
    # slirp4netns provides their (proxied, egress-restricted) outbound path. Shipped
    # next to ForgeKairos.exe as "slirp4netns-linux-x86_64"; crates/remote uploads it on
    # connect, mirroring the remote_server artifact. Static single binary, so it is
    # glibc-independent. License: slirp4netns is GPLv2+ (compatible with this GPL-3
    # fork); its bundled libslirp is MIT/BSD.
    $slirp4netnsVersion = "v1.2.3"
    $artifactPath = "$innoDir\bin\slirp4netns-linux-x86_64"
    $uri = "https://github.com/rootless-containers/slirp4netns/releases/download/$slirp4netnsVersion/slirp4netns-x86_64"
    Write-Output ("Downloading slirp4netns {0} from {1}" -f $slirp4netnsVersion, $uri)
    $previousProgress = $ProgressPreference
    $ProgressPreference = "SilentlyContinue"  # progress rendering is slow for large downloads
    try {
        Invoke-WebRequest -Uri $uri -OutFile $artifactPath -UseBasicParsing
    } finally {
        $ProgressPreference = $previousProgress
    }
    if (-not (Test-Path $artifactPath) -or (Get-Item $artifactPath).Length -lt 100000) {
        throw "slirp4netns download failed or the file is invalid: $artifactPath"
    }
    Write-Output ("slirp4netns: {0} ({1:N1} MB)" -f $artifactPath, ((Get-Item $artifactPath).Length / 1MB))
}

function CopyClangFormat {
    # The C / C++ defaults in assets/settings/default.json format through a bare
    # "clang-format" external command, which crates/util resolves as {app} ->
    # {app}\bin -> PATH. Nothing installs clang-format on a user's machine, so
    # without this copy the default formatter silently does nothing.
    #
    # License: clang-format is part of LLVM (Apache-2.0 with LLVM Exceptions).
    # crates\zed\resources\windows\licenses\LLVM-LICENSE.txt ships alongside it,
    # copied into $innoDir by PrepareForBundle and installed by zed.iss.
    $destination = "$innoDir\bin\clang-format.exe"

    if ($ClangFormatPath -and (Test-Path -PathType Leaf $ClangFormatPath)) {
        $source = (Resolve-Path $ClangFormatPath).Path
    } else {
        # LLVM shipped with the VS 2022 C++ workload; the edition varies per machine.
        $vsCandidates = @("Enterprise", "Professional", "Community", "BuildTools") |
            ForEach-Object { "C:\Program Files\Microsoft Visual Studio\2022\$_\VC\Tools\Llvm\bin\clang-format.exe" }
        $source = $vsCandidates | Where-Object { Test-Path -PathType Leaf $_ } | Select-Object -First 1
        if (-not $source) {
            throw "clang-format.exe not found (tried -ClangFormatPath '$ClangFormatPath' and VS 2022's VC\Tools\Llvm\bin). Pass -ClangFormatPath to point at one."
        }
    }

    Copy-Item -Path $source -Destination $destination -Force
    Write-Output ("clang-format: {0} <- {1} ({2:N1} MB)" -f $destination, $source, ((Get-Item $destination).Length / 1MB))

    $licenseFile = "$innoDir\licenses\LLVM-LICENSE.txt"
    if (-not (Test-Path -PathType Leaf $licenseFile)) {
        throw "Missing $licenseFile; clang-format may not be shipped without LLVM's license."
    }
}

function ZipZedAndItsFriendsDebug {
    $items = @(
        ".\$CargoOutDir\zed.pdb",
        ".\$CargoOutDir\cli.pdb",
        ".\$CargoOutDir\auto_update_helper.pdb",
        ".\$CargoOutDir\explorer_command_injector.pdb",
        ".\$CargoOutDir\remote_server.pdb"
    )

    Compress-Archive -Path $items -DestinationPath ".\$CargoOutDir\zed-$env:RELEASE_VERSION-$env:ZED_RELEASE_CHANNEL.dbg.zip" -Force
}


function UploadToSentry {
    if (-not (Get-Command "sentry-cli" -ErrorAction SilentlyContinue)) {
        Write-Output "sentry-cli not found. skipping sentry upload."
        Write-Output "install with: 'winget install -e --id=Sentry.sentry-cli'"
        return
    }
    if ([string]::IsNullOrWhiteSpace($env:SENTRY_AUTH_TOKEN)) {
        Write-Output "missing SENTRY_AUTH_TOKEN. skipping sentry upload."
        return
    }
    Write-Output "Uploading zed debug symbols to sentry..."
    for ($i = 1; $i -le 3; $i++) {
        try {
            sentry-cli debug-files upload --include-sources --wait -p zed -o zed-dev $CargoOutDir
            break
        }
        catch {
            Write-Output "Sentry upload attempt $i failed: $_"
            if ($i -eq 3) {
                Write-Output "All sentry upload attempts failed"
                throw
            }
            Start-Sleep -Seconds 2
        }
    }
}

function MakeAppx {
    switch ($channel) {
        "stable" {
            $manifestFile = "$env:ZED_WORKSPACE\crates\explorer_command_injector\AppxManifest.xml"
        }
        "preview" {
            $manifestFile = "$env:ZED_WORKSPACE\crates\explorer_command_injector\AppxManifest-Preview.xml"
        }
        "dev" {
            $manifestFile = "$env:ZED_WORKSPACE\crates\explorer_command_injector\AppxManifest-Dev.xml"
        }
        default {
            $manifestFile = "$env:ZED_WORKSPACE\crates\explorer_command_injector\AppxManifest-Nightly.xml"
        }
    }
    Copy-Item -Path "$manifestFile" -Destination "$innoDir\make_appx\AppxManifest.xml"
    # Add makeAppx.exe to Path
    $sdk = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64"
    $env:Path += ';' + $sdk
    makeAppx.exe pack /d "$innoDir\make_appx" /p "$innoDir\zed_explorer_command_injector.appx" /nv
}

function SignZedAndItsFriends {
    if (-not $canCodeSign) {
        return
    }

    $files = "$innoDir\ForgeKairos.exe,$innoDir\cli.exe,$innoDir\auto_update_helper.exe,$innoDir\zed_explorer_command_injector.dll,$innoDir\zed_explorer_command_injector.appx"
    & "$innoDir\sign.ps1" $files
}

function DownloadAMDGpuServices {
    # If you update the AGS SDK version, please also update the version in `crates/gpui/src/platform/windows/directx_renderer.rs`
    $url = "https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0"
    $zipPath = ".\AGS_SDK_v6.3.0.zip"
    # Download the AGS SDK zip file
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    # Extract the AGS SDK zip file
    Expand-Archive -Path $zipPath -DestinationPath "." -Force
}

function DownloadConpty {
    $url = "https://github.com/microsoft/terminal/releases/download/v1.23.13503.0/Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg"
    $zipPath = ".\Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg"
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    Expand-Archive -Path $zipPath -DestinationPath ".\conpty" -Force
}

function CollectFiles {
    Move-Item -Path "$innoDir\zed_explorer_command_injector.appx" -Destination "$innoDir\appx\zed_explorer_command_injector.appx" -Force
    Move-Item -Path "$innoDir\zed_explorer_command_injector.dll" -Destination "$innoDir\appx\zed_explorer_command_injector.dll" -Force
    # branding::CURRENT.cli_name, so it does not shadow an official Zed's
    # bin\zed.exe on PATH.
    Move-Item -Path "$innoDir\cli.exe" -Destination "$innoDir\bin\forgekairos.exe" -Force
    Move-Item -Path "$innoDir\zed.sh" -Destination "$innoDir\bin\forgekairos" -Force
    Move-Item -Path "$innoDir\auto_update_helper.exe" -Destination "$innoDir\tools\auto_update_helper.exe" -Force
    if($Architecture -eq "aarch64") {
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-arm64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
    else {
        New-Item -Type Directory -Path "$innoDir\x64" -Force
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\AGS_SDK-6.3.0\ags_lib\lib\amd_ags_x64.dll" -Destination "$innoDir\amd_ags_x64.dll" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\x64\OpenConsole.exe" -Destination "$innoDir\x64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-x64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
}

function AssertBundledRemoteArtifacts {
    # Each of these is what crates/remote looks for in {app}\bin when connecting
    # without network access; a missing one only shows up on the user's machine as
    # a blocked download, so refuse to build an installer without it.
    $required = @(
        "$innoDir\bin\zed-remote-server-windows-$Architecture.zip",
        "$innoDir\bin\zed-remote-server-linux-x86_64.gz",
        "$innoDir\bin\slirp4netns-linux-x86_64"
    )
    $missing = @($required | Where-Object { -not (Test-Path $_ -PathType Leaf) -or (Get-Item $_).Length -eq 0 })
    if ($missing.Count -gt 0) {
        throw "Bundled remote artifacts missing or empty: $($missing -join ', ')"
    }
}

function BuildInstaller {
    $issFilePath = "$innoDir\zed.iss"
    switch ($channel) {
        "stable" {
            # Fork-only AppId, distinct from upstream Zed's {2DB0DA96-...} so this
            # installer never overwrites or uninstalls an official Zed install on
            # the same machine. Once shipped this GUID must never change (it keys
            # the uninstall entry and in-place upgrades).
            $appId = "{{9924BCC9-2563-4389-B8F4-37F62D0954C7}"
            $appIconName = "app-icon"
            $appName = "ForgeKairos"
            $appDisplayName = "ForgeKairos"
            # Must match auto_update's asset_name() exactly (all lowercase):
            # forgekairos-windows-x86_64-setup.exe. OutputBaseFilename in zed.iss uses this.
            $appSetupName = "forgekairos-windows-$Architecture-setup"
            # Must match the runtime mutex in crates\zed\src\zed\windows_only_instance.rs,
            # which is "{app_identifier()}-Instance-Mutex"; app_identifier() comes from
            # branding::CURRENT.app_identifiers.
            $appMutex = "ForgeKairos-Editor-Stable-Instance-Mutex"
            # The mutex a build from before the rename holds, so a manual install
            # over it still waits for it to close.
            $appLegacyMutex = "Zed-JP-Enhance-Editor-Stable-Instance-Mutex"
            # branding::CURRENT.executable_stem; auto_update_helper moves this file.
            $appExeName = "ForgeKairos"
            # Fork-specific ProgId prefix so file associations do not clobber an
            # official Zed install's HKCU entries. Must also match REG_PATH in
            # crates\explorer_command_injector, which reads "{regValueName}ContextMenu".
            $regValueName = "ForgeKairos"
            # The ProgId prefix the build from before the rename registered. Only
            # the stable channel had one of its own: the other channels used
            # upstream's names, which an official Zed install may still own.
            $legacyRegValueName = "ZedJPEnhance"
            # The Start menu folder and desktop shortcut name of that build.
            $legacyAppName = "Zed JP Enhance"
            # branding::CURRENT.app_user_model_ids, which the app also sets at startup.
            $appUserId = "SeiyaFunaokaJP.ForgeKairos"
            $appShellNameShort = "&ForgeKairos"
            # Identity Name, Version and the publisher ID derived from Publisher
            # in crates\explorer_command_injector\AppxManifest.xml.
            $appAppxFullName = "SeiyaFunaokaJP.ForgeKairos_1.0.0.0_neutral__w9cgtc1wq3tc4"
        }
        "preview" {
            # Fork-only AppId (upstream preview was {F70E4811-...}). Not built by
            # this stable-only fork, but kept off upstream's GUID space for safety.
            $appId = "{{7C80B45F-ADD1-4E31-AD8E-43A6005A64F4}"
            $appIconName = "app-icon-preview"
            $appName = "ForgeKairos Preview"
            $appDisplayName = "ForgeKairos Preview"
            $appSetupName = "forgekairos-windows-$Architecture-setup"
            # Must match "{app_identifier()}-Instance-Mutex" for the preview channel.
            $appMutex = "ForgeKairos-Editor-Preview-Instance-Mutex"
            $appLegacyMutex = "Zed-JP-Enhance-Editor-Preview-Instance-Mutex"
            $appExeName = "ForgeKairos"
            $regValueName = "ForgeKairosPreview"
            $legacyRegValueName = ""
            $legacyAppName = ""
            $appUserId = "SeiyaFunaokaJP.ForgeKairos.Preview"
            $appShellNameShort = "&ForgeKairos Preview"
            $appAppxFullName = "SeiyaFunaokaJP.ForgeKairos.Preview_1.0.0.0_neutral__w9cgtc1wq3tc4"
        }
        "nightly" {
            # Fork-only AppId (upstream nightly was {1BDB21D3-...}).
            $appId = "{{EF65D849-2DAF-46EF-9B85-016677EEDFEB}"
            $appIconName = "app-icon-nightly"
            $appName = "ForgeKairos Nightly"
            $appDisplayName = "ForgeKairos Nightly"
            $appSetupName = "forgekairos-windows-$Architecture-setup"
            # Must match "{app_identifier()}-Instance-Mutex" for the nightly channel.
            $appMutex = "ForgeKairos-Editor-Nightly-Instance-Mutex"
            $appLegacyMutex = "Zed-JP-Enhance-Editor-Nightly-Instance-Mutex"
            $appExeName = "ForgeKairos"
            $regValueName = "ForgeKairosNightly"
            $legacyRegValueName = ""
            $legacyAppName = ""
            $appUserId = "SeiyaFunaokaJP.ForgeKairos.Nightly"
            $appShellNameShort = "&ForgeKairos Nightly"
            $appAppxFullName = "SeiyaFunaokaJP.ForgeKairos.Nightly_1.0.0.0_neutral__w9cgtc1wq3tc4"
        }
        "dev" {
            # Fork-only AppId (upstream dev was {8357632E-...}).
            $appId = "{{FBF63EC8-33D5-4339-8E91-58FD6DAE5653}"
            $appIconName = "app-icon-dev"
            $appName = "ForgeKairos Dev"
            $appDisplayName = "ForgeKairos Dev"
            $appSetupName = "forgekairos-windows-$Architecture-setup"
            # Must match "{app_identifier()}-Instance-Mutex" for the dev channel.
            $appMutex = "ForgeKairos-Editor-Dev-Instance-Mutex"
            $appLegacyMutex = "Zed-JP-Enhance-Editor-Dev-Instance-Mutex"
            $appExeName = "ForgeKairos"
            $regValueName = "ForgeKairosDev"
            $legacyRegValueName = ""
            $legacyAppName = ""
            $appUserId = "SeiyaFunaokaJP.ForgeKairos.Dev"
            $appShellNameShort = "&ForgeKairos Dev"
            $appAppxFullName = "SeiyaFunaokaJP.ForgeKairos.Dev_1.0.0.0_neutral__w9cgtc1wq3tc4"
        }
        default {
            Write-Error "can't bundle installer for $channel."
            exit 1
        }
    }

    # Locate ISCC.exe (Inno Setup 6). It may sit under Program Files, Program
    # Files (x86), or -- when installed per-user via winget, as on this fork's
    # build machine -- under %LOCALAPPDATA%\Programs. Fall back to PATH last.
    $isccCandidates = @(
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        "C:\Program Files\Inno Setup 6\ISCC.exe",
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    )
    $innoSetupPath = $isccCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $innoSetupPath) {
        $isccOnPath = Get-Command ISCC.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($isccOnPath) {
            $innoSetupPath = $isccOnPath.Source
        }
    }
    if (-not $innoSetupPath) {
        throw "ISCC.exe (Inno Setup 6) not found. Install it (e.g. winget install -e --id JRSoftware.InnoSetup) or put ISCC.exe on PATH."
    }
    Write-Host "Using Inno Setup compiler: $innoSetupPath"

    $definitions = @{
        "AppId"          = $appId
        "AppIconName"    = $appIconName
        "OutputDir"      = "$env:ZED_WORKSPACE\target"
        "AppSetupName"   = $appSetupName
        "AppName"        = $appName
        "AppDisplayName" = $appDisplayName
        "RegValueName"   = $regValueName
        "LegacyRegValueName" = $legacyRegValueName
        "LegacyAppName" = $legacyAppName
        "AppMutex"       = $appMutex
        "AppLegacyMutex" = $appLegacyMutex
        "AppExeName"     = $appExeName
        "ResourcesDir"   = "$innoDir"
        "ShellNameShort" = $appShellNameShort
        "AppUserId"      = $appUserId
        "Version"        = "$env:RELEASE_VERSION"
        "SourceDir"      = "$env:ZED_WORKSPACE"
        "AppxFullName"   = $appAppxFullName
    }

    $defs = @()
    foreach ($key in $definitions.Keys) {
        $defs += "/d$key=`"$($definitions[$key])`""
    }

    $innoArgs = @($issFilePath) + $defs
    if($canCodeSign) {
        # Checked by zed.iss to decide whether to sign the installer.
        $env:ZED_SIGN_BUNDLE = "1"
        $signTool = "powershell.exe -ExecutionPolicy Bypass -File $innoDir\sign.ps1 `$f"
        $innoArgs += "/sDefaultsign=`"$signTool`""
    }

    # Execute Inno Setup
    Write-Host "🚀 Running Inno Setup: $innoSetupPath $innoArgs"
    $process = Start-Process -FilePath $innoSetupPath -ArgumentList $innoArgs -NoNewWindow -Wait -PassThru

    if ($process.ExitCode -eq 0) {
        Write-Host "✅ Inno Setup successfully compiled the installer"
        Write-Output "SETUP_PATH=target/$appSetupName.exe" >> $env:GITHUB_ENV
        $script:buildSuccess = $true
    }
    else {
        Write-Host "❌ Inno Setup failed: $($process.ExitCode)"
        $script:buildSuccess = $false
    }
}

ParseZedWorkspace
$innoDir = "$env:ZED_WORKSPACE\inno\$Architecture"
$debugArchive = "$CargoOutDir\zed-$env:RELEASE_VERSION-$env:ZED_RELEASE_CHANNEL.dbg.zip"
$debugStoreKey = "$env:ZED_RELEASE_CHANNEL/zed-$env:RELEASE_VERSION-$env:ZED_RELEASE_CHANNEL.dbg.zip"

CheckEnvironmentVariables
PrepareForBundle
GenerateLicenses
BuildZedAndItsFriends
BuildRemoteServer
MakeAppx
SignZedAndItsFriends
ZipZedAndItsFriendsDebug
DownloadAMDGpuServices
DownloadConpty
CollectFiles
# Ship the Linux SSH-remote artifacts inside {app}\bin so remoting works with no
# network access (this fork blocks egress by default). Must run after CollectFiles
# (which creates and populates bin\) and before BuildInstaller (whose bin\* glob
# picks them up), and after all MSVC cargo builds so the musl RUSTFLAGS stay isolated.
BuildLinuxRemoteServer
DownloadSlirp4netns
# Ships the C/C++ default formatter's binary in {app}\bin, resolved there at
# runtime by crates/util's resolve_command_next_to_current_exe.
CopyClangFormat
AssertBundledRemoteArtifacts
BuildInstaller

if($env:CI) {
    UploadToSentry
}

if ($buildSuccess) {
    Write-Output "Build successful"
    if ($Install) {
        Write-Output "Installing ForgeKairos..."
        Start-Process -FilePath "$env:ZED_WORKSPACE/target/forgekairos-windows-$Architecture-setup.exe"
    }
    exit 0
}
else {
    Write-Output "Build failed"
    exit 1
}
