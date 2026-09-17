<#
    register-explorer-menu.ps1

    開発ビルドの zed.exe を、エクスプローラーの右クリックメニューに「Zed で開く」として
    手元だけで登録/削除するスクリプト。

    背景: 配布用インストーラー (script/bundle-windows.ps1 が作る) は Windows 11 では
    Appx (crates/explorer_command_injector) 経由でしか新しい右クリックメニューに項目を
    出せず、この Appx はコード署名 (Azure Trusted Signing) が無いと Add-AppxPackage が
    失敗する。zed-jp-enhance フォークのビルド環境には Zed Industries 名義の署名鍵が無いため、
    今のところこの経路は機能しない可能性が高い。本スクリプトは、その代わりに旧来の
    レジストリ (HKCU\Software\Classes\...\shell) へ直接登録する、開発機だけの回避策。

    使い方 (PowerShell から実行。実行ポリシーで止まる場合は
    `powershell -ExecutionPolicy Bypass -File .\script\register-explorer-menu.ps1` のように):
        .\script\register-explorer-menu.ps1                 既定の ..\target\debug\zed.exe を登録
        .\script\register-explorer-menu.ps1 -ExePath <path>  指定した zed.exe を登録
        .\script\register-explorer-menu.ps1 -Remove          登録を削除
        .\script\register-explorer-menu.ps1 -DryRun          レジストリを変更せず、内容だけ表示

    登録後の見え方 (Windows 11):
        この方式はレジストリ直接登録 (旧来の右クリックメニュー) のため、右クリック →
        「その他のオプションを表示」の中に「Zed で開く」が出る。トップレベルの新しい
        メニューに直接出すには、インストーラーが Appx を正しく (署名済みで) 登録できて
        いる必要がある。
        Windows 10 以前ではトップレベルの右クリックメニューにそのまま出る。
#>

[CmdletBinding()]
param(
    [string]$ExePath,
    [switch]$Remove,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# $PSScriptRoot は [CmdletBinding()] を付けた param() ブロックのデフォルト値の中では
# 空文字列になってしまう (Advanced Function 化されたスクリプトが既定値を評価する段階では
# まだスクリプトのファイルコンテキストが束縛されていないため)。そのため既定値はここ、
# 本体の先頭で解決する。
if ([string]::IsNullOrEmpty($ExePath)) {
    $ExePath = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\target\debug\zed.exe"))
}

# インストーラー (zed.iss) 側の {#RegValueName} ("ZedJPEnhance" 等) とはあえて別名にして、
# 正規インストール時のレジストリキーと衝突・上書きしないようにする。
$RegValueName = "ZedJpEnhance"
$MenuLabel = "Zed で開く"

if (-not $Remove -and -not (Test-Path -LiteralPath $ExePath)) {
    throw "zed.exe が見つかりません: $ExePath`n-ExePath でビルド済みの zed.exe の場所を指定してください。"
}

$ResolvedExePath = $null
if (-not $Remove) {
    $ResolvedExePath = (Resolve-Path -LiteralPath $ExePath).ProviderPath
}

# 対象: ファイル全般 (*)、フォルダー自体 (Directory)、フォルダーの何もない場所
# (Directory\Background)、ドライブのルート (Drive)。%1 はクリックした項目自身のパス、
# %V はフォルダー/ドライブ自身のパス (Background と Drive では %1 は渡らない)。
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
            Write-Host "[DryRun] 削除予定: $shellKey (存在すればサブキーごと)"
            continue
        }
        if (Test-Path -LiteralPath $shellKey) {
            Remove-Item -LiteralPath $shellKey -Recurse -Force
            Write-Host "削除しました: $shellKey"
        } else {
            Write-Host "存在しないためスキップ: $shellKey"
        }
        continue
    }

    $commandLine = "`"$ResolvedExePath`" $($target.Argument)"

    if ($DryRun) {
        Write-Host "[DryRun] 作成/更新予定: $shellKey"
        Write-Host "[DryRun]   (既定値) = $MenuLabel"
        Write-Host "[DryRun]   Icon     = $ResolvedExePath"
        Write-Host "[DryRun] 作成/更新予定: $commandKey"
        Write-Host "[DryRun]   (既定値) = $commandLine"
        continue
    }

    New-Item -Path $shellKey -Force -Value $MenuLabel | Out-Null
    New-ItemProperty -LiteralPath $shellKey -Name "Icon" -Value $ResolvedExePath -PropertyType String -Force | Out-Null
    New-Item -Path $commandKey -Force -Value $commandLine | Out-Null
    Write-Host "登録しました: $shellKey -> $commandLine"
}
