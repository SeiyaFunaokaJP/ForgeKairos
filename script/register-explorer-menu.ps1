<#
.SYNOPSIS
Adds "Open with ForgeKairos" for a development build to the Explorer context
menu of this machine, or removes it.

.DESCRIPTION
The installer (made by script/bundle-windows.ps1) puts its item in the
Windows 11 context menu through an Appx package (crates/explorer_command_injector),
and Add-AppxPackage fails unless that package is code signed (Azure Trusted
Signing). This fork's build machine has no signing key, so that route most
likely does not work. This script registers the item directly in the registry
(HKCU\Software\Classes\...\shell) instead, for development machines only.

On Windows 11 an item registered this way is in the classic menu: right-click,
then "Show more options". Only the signed Appx puts it in the top-level menu.
Windows 10 and earlier show it in the top-level menu.

If the execution policy stops the script, run it as
`powershell -ExecutionPolicy Bypass -File .\script\register-explorer-menu.ps1`.

.EXAMPLE
.\script\register-explorer-menu.ps1
Registers ..\target\debug\ForgeKairos.exe.

.EXAMPLE
.\script\register-explorer-menu.ps1 -ExePath <path>
Registers the given editor executable.

.EXAMPLE
.\script\register-explorer-menu.ps1 -Remove
Removes the item.

.EXAMPLE
.\script\register-explorer-menu.ps1 -DryRun
Shows what would be written without changing the registry.
#>

[CmdletBinding()]
param(
    [string]$ExePath,
    [switch]$Remove,
    [switch]$DryRun,
    # The text of the menu item.
    [string]$MenuLabel = "Open with ForgeKairos"
)

$ErrorActionPreference = "Stop"

# $PSScriptRoot is empty while the default values of a [CmdletBinding()]
# param() block are evaluated: the script's file context is not bound yet at
# that point. So the default is resolved here, at the start of the body.
if ([string]::IsNullOrEmpty($ExePath)) {
    $ExePath = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\target\debug\ForgeKairos.exe"))
}

# Differs, ignoring case, from the installer's {#RegValueName} in zed.iss
# ("ForgeKairos", "ForgeKairosDev" and so on, and "ZedJPEnhance" from before the
# rename), so that it never overwrites an installed build's registry keys
# (registry key names are case-insensitive).
$RegValueName = "ForgeKairosLocalBuild"

if (-not $Remove -and -not (Test-Path -LiteralPath $ExePath)) {
    throw "The editor executable was not found: $ExePath`nPass -ExePath with the location of a built ForgeKairos.exe."
}

$ResolvedExePath = $null
if (-not $Remove) {
    $ResolvedExePath = (Resolve-Path -LiteralPath $ExePath).ProviderPath
}

# Any file (*), a folder (Directory), the empty area of a folder
# (Directory\Background) and a drive's root (Drive). %1 is the clicked item's
# path; %V is the folder's or drive's own path (Background and Drive get no %1).
$Targets = @(
    @{ ClassesKey = "*"; Argument = '"%1"' },
    @{ ClassesKey = "Directory"; Argument = '"%V"' },
    @{ ClassesKey = "Directory\Background"; Argument = '"%V"' },
    @{ ClassesKey = "Drive"; Argument = '"%V"' }
)

foreach ($target in $Targets) {
    $shellKey = "HKCU:\Software\Classes\$($target.ClassesKey)\shell\$RegValueName"
    $commandKey = "$shellKey\command"

    if ($Remove) {
        if ($DryRun) {
            Write-Host "[DryRun] Would remove: $shellKey (with its subkeys, if it exists)"
            continue
        }
        if (Test-Path -LiteralPath $shellKey) {
            Remove-Item -LiteralPath $shellKey -Recurse -Force
            Write-Host "Removed: $shellKey"
        } else {
            Write-Host "Not present, skipped: $shellKey"
        }
        continue
    }

    $commandLine = "`"$ResolvedExePath`" $($target.Argument)"

    if ($DryRun) {
        Write-Host "[DryRun] Would create or update: $shellKey"
        Write-Host "[DryRun]   (Default) = $MenuLabel"
        Write-Host "[DryRun]   Icon      = $ResolvedExePath"
        Write-Host "[DryRun] Would create or update: $commandKey"
        Write-Host "[DryRun]   (Default) = $commandLine"
        continue
    }

    New-Item -Path $shellKey -Force -Value $MenuLabel | Out-Null
    New-ItemProperty -LiteralPath $shellKey -Name "Icon" -Value $ResolvedExePath -PropertyType String -Force | Out-Null
    New-Item -Path $commandKey -Force -Value $commandLine | Out-Null
    Write-Host "Registered: $shellKey -> $commandLine"
}
