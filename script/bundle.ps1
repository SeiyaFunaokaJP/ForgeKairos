#!/usr/bin/env pwsh
<#
.SYNOPSIS
Makes ForgeKairos's release packages for Windows, Linux and macOS from one entry
point, on whichever of the three this runs on (PowerShell 7).

.DESCRIPTION
-Os chooses what to make (default: the operating system this runs on):

  windows  script\bundle-windows.ps1 (the Inno Setup installer). Only on Windows.
  linux    script/bundle-linux. Natively on Linux. On Windows it runs in a Docker
           container inside WSL, and with -Container on Linux or macOS in a local
           Docker container; the result then links against the container's glibc
           (-LinuxImage) rather than the host's. On macOS without -Container it
           goes to the workflow, like macos below.
  macos    script/bundle-mac. Natively on macOS. Elsewhere it starts the
           forgekairos_release.yml GitHub Actions workflow with `gh workflow run`;
           -Wait waits for it and downloads the result with `gh run download`.
  all      Each of the above that this machine can make, and the workflow for
           the rest (started first, so it runs while the local builds do).

-Ci makes every requested package through the workflow instead. The workflow
builds the branch as it is on GitHub, so push first. The repository is private:
its macOS minutes are billed at ten times the Linux rate.

The packages are left in target/ under the names the updater looks for
(asset_name in crates/auto_update): forgekairos-windows-<arch>-setup.exe,
forgekairos-linux-<arch>.tar.gz, forgekairos-macos-<arch>.dmg, and the
forgekairos-remote-server-<os>-<arch>.gz files. Downloads from the workflow go to
target/ci-<run id>/<asset>/. The workflow builds Windows and Linux for x86_64 and
macOS for -MacosArchitecture.

Every package carries what remote connections upload to a Linux host (the Linux
remote server and slirp4netns), since this fork blocks their download. Only the
Windows installer carries clang-format; Linux and macOS run the one on PATH.

The Linux container keeps its cargo registry and target folder in the Docker
volumes forgekairos-bundle-cargo-<image> and forgekairos-bundle-target-<image>,
so later runs only rebuild what changed. `docker volume rm` them to start over.

.EXAMPLE
pwsh -NoProfile -File script/bundle.ps1                      # this OS
pwsh -NoProfile -File script/bundle.ps1 -Os linux            # on Windows: in Docker in WSL
pwsh -NoProfile -File script/bundle.ps1 -Os macos -Wait      # through GitHub Actions
pwsh -NoProfile -File script/bundle.ps1 -Os macos -MacosArchitecture aarch64 -Wait
pwsh -NoProfile -File script/bundle.ps1 -Os all -DryRun      # show what would run
pwsh -NoProfile -File script/bundle.ps1 -Os windows -Fast -Install
#>
[CmdletBinding()]
Param(
    [ValidateSet('windows', 'linux', 'macos', 'all')][string]$Os,
    # x86_64 or aarch64 of the packages built here. Default: this machine's.
    # It does not reach the workflow, which builds Windows and Linux for
    # x86_64 and macOS for -MacosArchitecture.
    [Alias('a')][string]$Architecture,
    # The workflow: the macOS architectures to build.
    [ValidateSet('both', 'aarch64', 'x86_64')][string]$MacosArchitecture = 'both',
    # The GitHub release tag the build is published under (vX.Y.Z or vX.Y.Z-pre).
    [string]$ReleaseTag,
    # Windows only: script\bundle-windows.ps1's quicker, unpublishable installer.
    [switch]$Fast,
    # Windows only: run the installer afterwards.
    [switch]$Install,
    # Windows only: the clang-format.exe to ship.
    [string]$ClangFormatPath,
    # cargo's -j for local builds (CARGO_BUILD_JOBS); 0 leaves cargo's default.
    [int]$Jobs = 0,
    # Linux: build in a container also on a Linux host.
    [switch]$Container,
    # Linux: the base image of the container. Its glibc is the oldest the
    # package runs on.
    [string]$LinuxImage = 'ubuntu:22.04',
    # Make every requested package through the GitHub Actions workflow.
    [switch]$Ci,
    # The workflow: the branch or tag to build (default: the current branch).
    [string]$Ref,
    # The workflow: the repository it runs in.
    [string]$Repository = 'SeiyaFunaokaJP/ForgeKairos',
    # The workflow: wait for the run and download its packages.
    [switch]$Wait,
    # Print the commands that make packages or start the workflow instead of
    # running them.
    [switch]$DryRun,
    [Alias('h')][switch]$Help
)

if ($PSVersionTable.PSVersion.Major -lt 7) {
    # Windows PowerShell 5.1 has no $IsWindows and cannot run bundle-windows.ps1;
    # hand the same arguments to PowerShell 7.
    $pwsh = Get-Command pwsh -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $pwsh) {
        Write-Error "This script needs PowerShell 7 (pwsh). Install it with 'winget install Microsoft.PowerShell' and run 'pwsh -NoProfile -File script\bundle.ps1'."
        exit 1
    }
    $forwardedArguments = @('-NoProfile', '-File', $PSCommandPath)
    foreach ($parameter in $PSBoundParameters.GetEnumerator()) {
        if ($parameter.Value -is [System.Management.Automation.SwitchParameter]) {
            if ($parameter.Value.IsPresent) {
                $forwardedArguments += "-$($parameter.Key)"
            }
        } elseif ([string]$parameter.Value -ne '') {
            $forwardedArguments += "-$($parameter.Key)"
            $forwardedArguments += [string]$parameter.Value
        }
    }
    & $pwsh.Source @forwardedArguments
    exit $LASTEXITCODE
}

$ErrorActionPreference = 'Stop'
$MacosArchitectureGiven = $PSBoundParameters.ContainsKey('MacosArchitecture')

if ($Help) {
    Get-Help $PSCommandPath -Detailed
    exit 0
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
$OutputDir = Join-Path $RepoRoot 'target'
$WorkflowFile = 'forgekairos_release.yml'

$HostOs = if ($IsWindows) { 'windows' } elseif ($IsMacOS) { 'macos' } else { 'linux' }

function Get-HostArchitecture {
    switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
        'X64' { 'x86_64' }
        'Arm64' { 'aarch64' }
        default { throw "Unsupported architecture $_" }
    }
}

function Format-Command {
    param([string]$File, [string[]]$Arguments)
    $quoted = foreach ($argument in @($File) + $Arguments) {
        if ($argument -match '[\s"''$]') { "'$($argument -replace "'", "''")'" } else { $argument }
    }
    $quoted -join ' '
}

# Runs a command that makes something, or only shows it with -DryRun.
function Invoke-Step {
    param([string]$File, [string[]]$Arguments)
    Write-Host "> $(Format-Command $File $Arguments)" -ForegroundColor Cyan
    if ($DryRun) {
        return
    }
    # Out-Host keeps the command's output out of the caller's return value.
    & $File @Arguments | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "$File exited with $LASTEXITCODE"
    }
}

# Runs a command that only reads, also with -DryRun, and returns its output.
function Invoke-Query {
    param([string]$File, [string[]]$Arguments)
    $output = & $File @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$(Format-Command $File $Arguments) exited with $LASTEXITCODE"
    }
    $output
}

function Assert-ReleaseTag {
    if ($ReleaseTag -and $ReleaseTag -notmatch '^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') {
        throw "-ReleaseTag must look like v1.2.3 or v1.2.3-pre (got '$ReleaseTag')."
    }
}

function Copy-Output {
    param([string]$Source)
    if (-not (Test-Path -PathType Leaf $Source)) {
        throw "Expected output not found: $Source"
    }
    $destination = Join-Path $OutputDir (Split-Path -Leaf $Source)
    if ([IO.Path]::GetFullPath($Source) -ne [IO.Path]::GetFullPath($destination)) {
        Copy-Item -Path $Source -Destination $destination -Force
    }
    Write-Host "Output: $destination"
}

function Set-BuildEnvironment {
    if ($Jobs -gt 0) {
        $env:CARGO_BUILD_JOBS = "$Jobs"
    }
    if ($ReleaseTag) {
        $env:FORGEKAIROS_RELEASE_TAG = $ReleaseTag
    }
}

# --- Windows -----------------------------------------------------------------

function Invoke-WindowsBundle {
    $arguments = @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'bundle-windows.ps1'))
    if ($Architecture) { $arguments += @('-Architecture', $Architecture) }
    if ($ReleaseTag) { $arguments += @('-ReleaseTag', $ReleaseTag) }
    if ($ClangFormatPath) { $arguments += @('-ClangFormatPath', $ClangFormatPath) }
    if ($Fast) { $arguments += '-Fast' }
    if ($Install) { $arguments += '-Install' }
    # A separate process, because bundle-windows.ps1 enters the Visual Studio
    # developer shell, which would leak into the Linux and macOS steps.
    Set-BuildEnvironment
    Invoke-Step (Get-Process -Id $PID).Path $arguments
}

# --- Linux -------------------------------------------------------------------

function Invoke-LinuxNativeBundle {
    $arch = Get-HostArchitecture
    if ($Architecture -and $Architecture -ne $arch) {
        throw "script/bundle-linux builds for this machine ($arch), not $Architecture."
    }
    Set-BuildEnvironment
    Push-Location $RepoRoot
    try {
        Invoke-Step 'bash' @('script/bundle-linux')
    } finally {
        Pop-Location
    }
    if ($DryRun) { return }
    $cargoTarget = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { $OutputDir }
    Copy-Output (Join-Path $cargoTarget "release/forgekairos-linux-$arch.tar.gz")
    Copy-Output (Join-Path $cargoTarget "forgekairos-remote-server-linux-$arch.gz")
}

# Docker runs in WSL on Windows, where it sees the Windows drives under /mnt.
$DockerCommand = if ($IsWindows) { @('wsl.exe', '-e', 'docker') } else { @('docker') }

function Invoke-Docker {
    param([string[]]$Arguments, [switch]$Query)
    $all = @($DockerCommand | Select-Object -Skip 1) + $Arguments
    if ($Query) {
        Invoke-Query $DockerCommand[0] $all
    } else {
        Invoke-Step $DockerCommand[0] $all
    }
}

function ConvertTo-DockerPath {
    param([string]$Path)
    if (-not $IsWindows) {
        return $Path
    }
    # wslpath wants forward slashes; backslashes would be eaten as escapes.
    Invoke-Query 'wsl.exe' @('-e', 'wslpath', '-a', ($Path -replace '\\', '/'))
}

function Get-ContainerGitArguments {
    # In a git worktree, .git is a file naming the real git dir by a host path
    # the container cannot read; mount the shared git dir and point git at it.
    $gitDir = (Invoke-Query 'git' @('-C', $RepoRoot, 'rev-parse', '--absolute-git-dir')).Trim()
    $commonDir = (Invoke-Query 'git' @('-C', $RepoRoot, 'rev-parse', '--path-format=absolute', '--git-common-dir')).Trim()
    $repoFull = [IO.Path]::GetFullPath($RepoRoot).TrimEnd('\', '/')
    $gitFull = [IO.Path]::GetFullPath($gitDir)
    if ($gitFull.StartsWith($repoFull + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        return @()
    }
    $commonFull = [IO.Path]::GetFullPath($commonDir).TrimEnd('\', '/')
    $relative = [IO.Path]::GetRelativePath($commonFull, $gitFull) -replace '\\', '/'
    return @(
        '-v', "$(ConvertTo-DockerPath $commonFull):/forgekairos-git:ro",
        '-e', "GIT_DIR=/forgekairos-git/$relative",
        '-e', 'GIT_WORK_TREE=/src'
    )
}

function Invoke-LinuxContainerBundle {
    if ($DryRun) {
        # A dry run touches neither Docker nor the build context; the
        # architecture check waits for the real run.
        $dockerArch = if ($Architecture) { $Architecture } else { Get-HostArchitecture }
    } else {
        try {
            $dockerArch = (Invoke-Docker -Query @('info', '--format', '{{.Architecture}}') | Select-Object -First 1).Trim()
        } catch {
            $where = if ($IsWindows) { 'in WSL (wsl.exe -e docker info)' } else { '(docker info)' }
            throw "Docker is not reachable $where. Install and start Docker there, or use -Ci. ($_)"
        }
    }
    $arch = switch ($dockerArch) {
        'x86_64' { 'x86_64' }
        'amd64' { 'x86_64' }
        'aarch64' { 'aarch64' }
        'arm64' { 'aarch64' }
        default { throw "Unsupported Docker architecture $dockerArch" }
    }
    if ($Architecture -and $Architecture -ne $arch) {
        throw "Docker here builds for $arch, not $Architecture."
    }

    $imageSlug = ($LinuxImage -replace '[^A-Za-z0-9_.-]', '-').ToLowerInvariant()
    $image = "forgekairos-bundle-linux:$imageSlug"
    $cargoVolume = "forgekairos-bundle-cargo-$imageSlug"
    $targetVolume = "forgekairos-bundle-target-$imageSlug"

    $context = Join-Path $OutputDir 'linux-bundle-image'
    $contextFiles = [ordered]@{
        'Dockerfile' = Join-Path $PSScriptRoot 'lib/linux-bundle.Dockerfile'
        'linux' = Join-Path $PSScriptRoot 'linux'
        'rust-toolchain.toml' = Join-Path $RepoRoot 'rust-toolchain.toml'
    }
    if ($DryRun) {
        Write-Host "> (copy $($contextFiles.Values -join ', ') to $context)" -ForegroundColor Cyan
    } else {
        New-Item -ItemType Directory -Force $context | Out-Null
        foreach ($file in $contextFiles.GetEnumerator()) {
            Copy-Item $file.Value (Join-Path $context $file.Key) -Force
        }
    }
    $dockerContext = ConvertTo-DockerPath $context
    Invoke-Docker @('build', '--build-arg', "BASE_IMAGE=$LinuxImage", '-t', $image, $dockerContext)

    $runArguments = @(
        'run', '--rm', '--init',
        '-v', "$(ConvertTo-DockerPath $RepoRoot):/src",
        # An empty named volume starts as a copy of the image's /opt/cargo
        # (rustup's proxies), then keeps the registry and cargo-about.
        '-v', "${cargoVolume}:/opt/cargo",
        '-v', "${targetVolume}:/target",
        '-e', 'CARGO_TARGET_DIR=/target',
        '-e', 'CARGO_INCREMENTAL=0',
        '-w', '/src'
    )
    $runArguments += Get-ContainerGitArguments
    if ($Jobs -gt 0) { $runArguments += @('-e', "CARGO_BUILD_JOBS=$Jobs") }
    if ($ReleaseTag) { $runArguments += @('-e', "FORGEKAIROS_RELEASE_TAG=$ReleaseTag") }
    $runArguments += @($image, 'bash', '/src/script/lib/linux-bundle-in-container.sh')
    Invoke-Docker $runArguments

    if ($DryRun) { return }
    Copy-Output (Join-Path $OutputDir "forgekairos-linux-$arch.tar.gz")
    Copy-Output (Join-Path $OutputDir "forgekairos-remote-server-linux-$arch.gz")
}

# --- macOS -------------------------------------------------------------------

function Invoke-MacBundle {
    $arch = if ($Architecture) { $Architecture } else { Get-HostArchitecture }
    if ($arch -notin @('x86_64', 'aarch64')) {
        throw "-Architecture must be x86_64 or aarch64 (got '$arch')."
    }
    if ($env:CARGO_TARGET_DIR) {
        throw 'script/bundle-mac reads its build from target/; unset CARGO_TARGET_DIR.'
    }
    $triple = "$arch-apple-darwin"
    Set-BuildEnvironment
    Push-Location $RepoRoot
    try {
        Invoke-Step 'bash' @('script/bundle-mac', $triple)
    } finally {
        Pop-Location
    }
    if ($DryRun) { return }
    Copy-Output (Join-Path $OutputDir "$triple/release/forgekairos-macos-$arch.dmg")
    Copy-Output (Join-Path $OutputDir "forgekairos-remote-server-macos-$arch.gz")
}

# --- GitHub Actions ----------------------------------------------------------

function Get-CiRef {
    if ($Ref) { return $Ref }
    $branch = (Invoke-Query 'git' @('-C', $RepoRoot, 'branch', '--show-current')).Trim()
    if (-not $branch) {
        throw 'HEAD is not on a branch; pass -Ref <branch or tag> for the workflow to build.'
    }
    $branch
}

function Start-CiBundle {
    param([string[]]$OsList)
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        throw "Building $($OsList -join ', ') needs the GitHub Actions workflow, started with the GitHub CLI (gh), which is not installed: https://cli.github.com/"
    }
    $ciRef = Get-CiRef
    $head = (Invoke-Query 'git' @('-C', $RepoRoot, 'rev-parse', 'HEAD')).Trim()
    $upstream = & git -C $RepoRoot rev-parse --verify --quiet '@{upstream}' 2>$null
    if (-not $Ref -and $upstream -ne $head) {
        Write-Warning "The workflow builds '$ciRef' as it is on GitHub, but this checkout's HEAD ($($head.Substring(0, 10))) is not what its upstream branch has. Push first, or the result will not contain your latest commits."
    }

    # -Architecture is for the packages built here; the workflow takes its
    # architectures from -MacosArchitecture and builds the rest for x86_64.
    $x86Only = @($OsList | Where-Object { $_ -in @('windows', 'linux') })
    if ($Architecture -and $Architecture -ne 'x86_64' -and $x86Only.Count -gt 0) {
        throw "The workflow builds $($x86Only -join ' and ') for x86_64 only, not $Architecture."
    }
    if ($Architecture -and $OsList -contains 'macos' -and -not $MacosArchitectureGiven) {
        Write-Warning "-Architecture does not reach the workflow; it builds macOS for '$MacosArchitecture' (-MacosArchitecture)."
    }
    $osInput = if ($OsList.Count -eq 3) { 'all' } else { $OsList -join ',' }
    $arguments = @('workflow', 'run', $WorkflowFile, '--repo', $Repository, '--ref', $ciRef,
        '-f', "os=$osInput", '-f', "macos_arch=$MacosArchitecture")
    if ($ReleaseTag) { $arguments += @('-f', "release_tag=$ReleaseTag") }

    # gh workflow run does not print the id of the run it starts. The run is
    # told apart by the commit it builds (the ref's head on GitHub) and by not
    # being one of that commit's runs from before.
    $headSha = $null
    $earlierRuns = @()
    if (-not $DryRun) {
        $escapedRef = [uri]::EscapeDataString($ciRef)
        $headSha = (Invoke-Query 'gh' @('api', "repos/$Repository/commits/$escapedRef", '--jq', '.sha')).Trim()
        $earlierRuns = @(Get-CiRuns $headSha | ForEach-Object { $_.databaseId })
    }
    Write-Host "Starting $WorkflowFile in $Repository for $osInput at $ciRef (billed GitHub Actions minutes)."
    Invoke-Step 'gh' $arguments
    return [pscustomobject]@{ Ref = $ciRef; HeadSha = $headSha; EarlierRuns = $earlierRuns }
}

function Get-CiRunListArguments {
    param([string]$HeadSha)
    @('run', 'list', '--repo', $Repository, '--workflow', $WorkflowFile,
        '--commit', $HeadSha, '--event', 'workflow_dispatch', '--limit', '20',
        '--json', 'databaseId,createdAt,headSha,url')
}

function Get-CiRuns {
    param([string]$HeadSha)
    @(Invoke-Query 'gh' (Get-CiRunListArguments $HeadSha) | ConvertFrom-Json) |
        Where-Object { $_.headSha -eq $HeadSha }
}

function Wait-CiBundle {
    param($Started)
    if ($DryRun) {
        Write-Host "> $(Format-Command 'gh' (Get-CiRunListArguments '<head of the ref>'))" -ForegroundColor Cyan
        Write-Host "> gh run watch <run id> --repo $Repository --exit-status" -ForegroundColor Cyan
        Write-Host "> gh run download <run id> --repo $Repository --dir target/ci-<run id>" -ForegroundColor Cyan
        return
    }
    $run = $null
    for ($attempt = 0; $attempt -lt 30 -and -not $run; $attempt++) {
        Start-Sleep -Seconds 4
        $run = Get-CiRuns $Started.HeadSha |
            Where-Object { $_.databaseId -notin $Started.EarlierRuns } |
            Sort-Object { [DateTimeOffset]$_.createdAt } |
            Select-Object -First 1
    }
    if (-not $run) {
        throw "Could not find the workflow run just started; see https://github.com/$Repository/actions/workflows/$WorkflowFile"
    }
    Write-Host "Waiting for $($run.url)"
    Invoke-Step 'gh' @('run', 'watch', "$($run.databaseId)", '--repo', $Repository, '--exit-status', '--interval', '60')
    $downloadDir = Join-Path $OutputDir "ci-$($run.databaseId)"
    Invoke-Step 'gh' @('run', 'download', "$($run.databaseId)", '--repo', $Repository, '--dir', $downloadDir)
    Get-ChildItem -Recurse -File $downloadDir | ForEach-Object { Write-Host "Output: $($_.FullName)" }
}

# --- Plan --------------------------------------------------------------------

Assert-ReleaseTag
if (-not $Os) { $Os = $HostOs }
$requested = if ($Os -eq 'all') { @('windows', 'linux', 'macos') } else { @($Os) }

$local = @()
$remote = @()
foreach ($target in $requested) {
    $canBuildHere = switch ($target) {
        'windows' { $IsWindows }
        'macos' { $IsMacOS }
        # macOS needs -Container (a local Docker) to build it here.
        'linux' { -not $IsMacOS -or $Container }
    }
    if ($Ci -or -not $canBuildHere) {
        if ($target -eq 'windows' -and $Os -eq 'windows' -and -not $Ci) {
            throw 'The Windows installer can only be made on Windows (script\bundle-windows.ps1 needs Visual Studio and Inno Setup). Use -Ci to make it with the GitHub Actions workflow.'
        }
        $remote += $target
    } else {
        $local += $target
    }
}
if (($Fast -or $Install -or $ClangFormatPath) -and $local -notcontains 'windows') {
    Write-Warning '-Fast, -Install and -ClangFormatPath only apply to the Windows installer built here.'
}

New-Item -ItemType Directory -Force $OutputDir | Out-Null
$started = $null
if ($remote.Count -gt 0) {
    $started = Start-CiBundle $remote
}
foreach ($target in $local) {
    Write-Host "=== $target ===" -ForegroundColor Green
    switch ($target) {
        'windows' { Invoke-WindowsBundle }
        'macos' { Invoke-MacBundle }
        'linux' {
            if ($IsLinux -and -not $Container) { Invoke-LinuxNativeBundle } else { Invoke-LinuxContainerBundle }
        }
    }
}
if ($started) {
    if ($Wait) {
        Wait-CiBundle $started
    } elseif (-not $DryRun) {
        Write-Host "The workflow is running: https://github.com/$Repository/actions/workflows/$WorkflowFile (download its artifacts there, or rerun with -Wait)."
    }
}
