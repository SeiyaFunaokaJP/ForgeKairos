# The release build that script\build.ps1 -Release and script\bundle-windows.ps1
# share. Both must build with the same environment, packages and target triple,
# or cargo treats the other script's output as stale: build scripts that use cc
# rerun when VCINSTALLDIR differs (and with them every crate above), zed reruns
# when RELEASE_CHANNEL differs, and building a different set of packages in one
# cargo run unifies the dependencies' features differently. A relink of the
# editor after that alone takes about 30 minutes with the release profile's
# thin LTO.

# Built in one cargo run so that their dependencies' features stay the same.
$WindowsReleasePackages = @('zed', 'cli', 'auto_update_helper')

function Get-VSArch {
    param(
        [string]$Arch
    )

    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

function Get-WindowsHostArchitecture {
    switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
        "X64" { "x86_64" }
        "Arm64" { "aarch64" }
        default { throw "Unsupported architecture" }
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

# Enters the Visual Studio developer shell for $Architecture and sets the
# release channel variables from crates\zed\RELEASE_CHANNEL. Returns the channel.
function Enter-WindowsReleaseEnvironment {
    param(
        [Parameter(Mandatory)][string]$Architecture,
        [Parameter(Mandatory)][string]$RepoRoot
    )

    Push-Location
    $vsDevShell = Find-VsDevShell
    & $vsDevShell -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch (Get-WindowsHostArchitecture)) | Out-Host
    Pop-Location

    $channel = (Get-Content (Join-Path $RepoRoot 'crates\zed\RELEASE_CHANNEL') -TotalCount 1).Trim()
    $env:ZED_RELEASE_CHANNEL = $channel
    $env:RELEASE_CHANNEL = $channel
    # The CLI compiles RELEASE_VERSION in (option_env!) and bundle-windows.ps1
    # sets it, so a build without it recompiled the CLI on every switch
    # between build.ps1 -Release and the installer.
    $env:RELEASE_VERSION = Get-ZedCrateVersion -RepoRoot $RepoRoot
    return $channel
}

# The version in crates\zed\Cargo.toml's [package], what bundle-windows.ps1's
# ParseZedWorkspace reads with cargo metadata.
function Get-ZedCrateVersion {
    param([Parameter(Mandatory)][string]$RepoRoot)

    $manifest = Get-Content (Join-Path $RepoRoot 'crates\zed\Cargo.toml')
    $inPackage = $false
    foreach ($line in $manifest) {
        if ($line -match '^\s*\[(.+)\]\s*$') {
            $inPackage = $Matches[1] -eq 'package'
        } elseif ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw "No version in the [package] of crates\zed\Cargo.toml."
}

# The cargo arguments of the shared release build, without -j.
function Get-WindowsReleaseCargoArguments {
    param(
        [Parameter(Mandatory)][string]$Target,
        [Parameter(Mandatory)][string]$CargoProfile
    )

    $arguments = @('--config', '.cargo/bundle-config.toml', 'build', '--profile', $CargoProfile, '--target', $Target)
    foreach ($package in $WindowsReleasePackages) {
        $arguments += @('--package', $package)
    }
    return , $arguments
}

# The profile folder name cargo uses for $CargoProfile.
function Get-CargoProfileDirectory {
    param(
        [Parameter(Mandatory)][string]$CargoProfile
    )

    switch ($CargoProfile) {
        'dev' { 'debug' }
        default { $CargoProfile }
    }
}
