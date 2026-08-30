<#
.SYNOPSIS
upstream スナップショットと現在の HEAD の差分台帳を出力する (再移植用。PORTING.md 参照)。

.EXAMPLE
powershell -NoProfile -File script\fork-diff.ps1
powershell -NoProfile -File script\fork-diff.ps1 -Detail
#>
param(
    # 比較の基点 (既定: fork 元スナップショット)
    [string]$Base = 'c8e44cf',
    # 変更ファイルを全列挙する
    [switch]$Detail
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

$diff = git -C $repoRoot diff --name-status $Base HEAD
if ($LASTEXITCODE -ne 0) {
    Write-Error "git diff が失敗しました (base=$Base)"
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
"追加 {0} / 変更 {1} / 削除 {2}" -f $added.Count, $modified.Count, $deleted.Count
""
"== 追加ファイル (フォーク専有。再移植ではほぼそのまま持っていける) =="
$added | ForEach-Object { "  A $_" }
""
"== 削除ファイル =="
$deleted | ForEach-Object { "  D $_" }
if ($other.Count) {
    ""
    "== その他 (rename 等) =="
    $other | ForEach-Object { "  $_" }
}
""
"== 変更ファイルのクレート別集計 (再適用コストの目安) =="
$modified | ForEach-Object { ($_ -split '/')[0..1] -join '/' } |
    Group-Object | Sort-Object Count -Descending |
    ForEach-Object { "  {0,4}  {1}" -f $_.Count, $_.Name }

if ($Detail) {
    ""
    "== 変更ファイル全列挙 =="
    $modified | ForEach-Object { "  M $_" }
}
