<#
.SYNOPSIS
Lists what changed between an upstream snapshot and HEAD, for porting the fork again (see PORTING.md).

.EXAMPLE
powershell -NoProfile -File script\fork-diff.ps1
powershell -NoProfile -File script\fork-diff.ps1 -Detail
#>
param(
    # What to compare with (by default the snapshot the fork started from).
    [string]$Base = 'c8e44cf',
    # List every modified file.
    [switch]$Detail
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

$diff = git -C $repoRoot diff --name-status $Base HEAD
if ($LASTEXITCODE -ne 0) {
    Write-Error "git diff failed (base=$Base)"
}

$added = @(); $modified = @(); $deleted = @(); $other = @()
foreach ($line in $diff) {
    $parts = $line -split "`t"
    switch ($parts[0].Substring(0, 1)) {
        'A' { $added += $parts[1] }
        'M' { $modified += $parts[1] }
        'D' { $deleted += $parts[1] }
        default { $other += $line }
    }
}

"base: $Base -> HEAD"
"added {0} / modified {1} / deleted {2}" -f $added.Count, $modified.Count, $deleted.Count
""
"== Added files (the fork's own; a port can mostly take them as they are) =="
$added | ForEach-Object { "  A $_" }
""
"== Deleted files =="
$deleted | ForEach-Object { "  D $_" }
if ($other.Count) {
    ""
    "== Other (renames and the like) =="
    $other | ForEach-Object { "  $_" }
}
""
"== Modified files per crate (a measure of what porting them again costs) =="
$modified | ForEach-Object { ($_ -split '/')[0..1] -join '/' } |
    Group-Object | Sort-Object Count -Descending |
    ForEach-Object { "  {0,4}  {1}" -f $_.Count, $_.Name }

if ($Detail) {
    ""
    "== Every modified file =="
    $modified | ForEach-Object { "  M $_" }
}
