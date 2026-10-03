<#
.SYNOPSIS
upstream を git merge で取り込んだ途中に残った Cargo.toml / Cargo.lock の衝突を機械的に解く。

.DESCRIPTION
衝突の印 (<<<<<<< / ||||||| / ======= / >>>>>>>) が残っている Cargo.toml と Cargo.lock だけを
書き換え、何をしたかを表示する。git add も commit もしない。

Cargo.toml (ルートと各クレート):
  衝突の塊ごとに、基準 / フォーク / upstream の 3 つを「1 行 = 1 項目」(依存の行、members の
  要素、features の要素、コメント行) として 3-way で合わせる。
    - 片側だけが変えた項目はその側を採る。両側が足した行は両方残す
    - 両側の並びがアルファベット順なら、結果も順に並べ直す
    - フォークが外したクレート (ルートの Cargo.toml の `exclude` + LiveKit のネイティブ依存) の行は、
      フォークが消したものは消したまま、upstream が新しく足したものも入れない
      (`[workspace.dependencies]` は除く。.forgekairos-docs/DESIGN.md §4.6 ADR-F06)
    - upstream の行から外したクレートの feature を除くとフォークの行と同じになる場合はフォークを採る
      (例: `git_ui = { workspace = true, features = ["call"] }` と `git_ui.workspace = true`)
    - 両側が変えた行でも、フォークの行が「フォークが最後に取り込んだ upstream のリリース」
      (-ForkPoint。既定は HEAD から辿れる最新の refs/upstream/tags/*) の行そのままなら upstream を
      採る。merge の基準は upstream の main からリリースブランチが分かれた点なので、リリース
      ブランチの cherry-pick (`[package]` の version、wasmtime の版上げなど) が毎回ここに来る
    - `[package]` の `version` は上に当てはまらなくても upstream を採る
  それ以外 (同じ項目を両側が違うふうに変えた、複数行にまたがる値、表の見出し) は触らずに残して
  一覧に出す。1 つの塊の中に解けない項目が 1 つでもあれば、その塊は丸ごと残す。

Cargo.lock:
  upstream の側 (:3:Cargo.lock) を置いてから `cargo update --workspace` に解き直させる。フォークが足した
  依存は Cargo.toml から入る。Cargo.toml に衝突が残っているうちは何もしない (先に解いてから
  もう一度実行する)。解き直しの後、フォークの Cargo.lock が -ForkPoint の Cargo.lock から版を
  変えていたパッケージ (= Cargo.lock にしか現れない固定) が保たれているかを検査して、崩れたものを
  表示する。

衝突の塊に基準の側 (|||||||) が無い場合は、index の :1: / :2: / :3: から
`git merge-file --diff3` で作り直してから解く (ファイルを手で触っていない場合に限る)。
merge の前に `git config merge.conflictStyle zdiff3` (または diff3) にしておくのが確実。

.EXAMPLE
git merge refs/upstream/tags/v1.18.1
powershell -NoProfile -File script\resolve-cargo-conflicts.ps1
powershell -NoProfile -File script\resolve-cargo-conflicts.ps1 -DryRun

.EXAMPLE
# merge の外で、取り出した衝突ファイルの写しに対して試す (git はリポジトリ I:\zed-jp-llm に聞く)
powershell -NoProfile -File script\resolve-cargo-conflicts.ps1 -RepoRoot C:\tmp\merged -GitRepo I:\zed-jp-llm `
    -Path "Cargo.toml,crates\zed\Cargo.toml,Cargo.lock" -ForkPoint refs/upstream/tags/v1.17.2 `
    -TheirsLock C:\tmp\theirs.lock -OursLock C:\tmp\ours.lock
#>
param(
    # 対象の Cargo.toml / Cargo.lock (RepoRoot からの相対パス。カンマ区切り可)。既定: git が衝突と見なしているファイル
    [string[]]$Path,
    # 作業ツリーの根 (既定: このスクリプトの親)
    [string]$RepoRoot,
    # git に問い合わせるリポジトリ (既定: RepoRoot)
    [string]$GitRepo,
    # フォークが最後に取り込んだ upstream のリリース (既定: HEAD から辿れる最新の refs/upstream/tags/*)
    [string]$ForkPoint,
    # Cargo.lock の upstream / フォークの側を index (:3: / :2:) ではなくファイルから読む (merge の外での確認用)
    [string]$TheirsLock,
    [string]$OursLock,
    # Cargo.lock を解き直さない
    [switch]$SkipLock,
    # 何も書き換えずに、するはずのことだけを表示する
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
if (-not $RepoRoot) {
    $RepoRoot = Split-Path -Parent $PSScriptRoot
}
$RepoRoot = (Resolve-Path $RepoRoot).Path
if (-not $GitRepo) {
    $GitRepo = $RepoRoot
}
$GitRepo = (Resolve-Path $GitRepo).Path
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

# フォークが LiveKit を外したことで一緒に消えたサードパーティの依存。
# クレートそのものはルートの Cargo.toml の `exclude` から読む。
$ExcludedThirdParty = @('livekit', 'libwebrtc', 'webrtc-sys')

function Read-Text([string]$File) {
    [System.IO.File]::ReadAllText($File, $Utf8NoBom)
}

function Write-Text([string]$File, [string]$Text) {
    if (-not $DryRun) {
        [System.IO.File]::WriteAllText($File, $Text, $Utf8NoBom)
    }
}

function Invoke-Git([string[]]$Arguments) {
    # PowerShell のパイプを通すと末尾の改行と文字コードが変わるので、出力を UTF-8 のまま受け取る
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = 'git'
    $info.Arguments = ($Arguments | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join ' '
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $Utf8NoBom
    $info.WorkingDirectory = $GitRepo
    $process = [System.Diagnostics.Process]::Start($info)
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    return @{ ExitCode = $process.ExitCode; Output = $stdout.Result; Error = $stderr.Result }
}

function Invoke-Cargo([string[]]$Arguments) {
    # cargo は進み具合を stderr に書く。Windows PowerShell は `2>&1` の stderr を
    # $ErrorActionPreference = 'Stop' のもとでは例外にするので、ここだけ緩める
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    Push-Location $RepoRoot
    try {
        $output = @(& cargo @Arguments 2>&1 | ForEach-Object { "$_" })
        return @{ ExitCode = $LASTEXITCODE; Output = $output }
    } finally {
        Pop-Location
        $ErrorActionPreference = $previous
    }
}

function Test-InGitRepo {
    $result = Invoke-Git @('rev-parse', '--is-inside-work-tree')
    return ($result.ExitCode -eq 0 -and $result.Output.Trim() -eq 'true')
}

function Get-GitStage([string]$RelativePath, [int]$Stage) {
    $gitPath = $RelativePath -replace '\\', '/'
    $result = Invoke-Git @('show', ":${Stage}:$gitPath")
    if ($result.ExitCode -ne 0) {
        return $null
    }
    return $result.Output
}

function Resolve-ForkPoint {
    if ($ForkPoint) {
        return $ForkPoint
    }
    if (-not (Test-InGitRepo)) {
        return $null
    }
    $tags = (Invoke-Git @('for-each-ref', '--sort=-v:refname', '--format=%(refname)', 'refs/upstream/tags')).Output -split "`n"
    foreach ($tag in $tags) {
        $tag = $tag.Trim()
        if ($tag -eq '') { continue }
        if ((Invoke-Git @('merge-base', '--is-ancestor', $tag, 'HEAD')).ExitCode -eq 0) {
            return $tag
        }
    }
    return $null
}

$ForkPointFiles = @{}
function Get-ForkPointText([string]$RelativePath) {
    if (-not $script:ResolvedForkPoint) {
        return $null
    }
    if (-not $ForkPointFiles.ContainsKey($RelativePath)) {
        $result = Invoke-Git @('show', "$($script:ResolvedForkPoint):$($RelativePath -replace '\\', '/')")
        $ForkPointFiles[$RelativePath] = if ($result.ExitCode -eq 0) { $result.Output } else { $null }
    }
    return $ForkPointFiles[$RelativePath]
}

function Get-ExcludedCrates {
    $names = New-Object System.Collections.Generic.HashSet[string]
    foreach ($name in $ExcludedThirdParty) { [void]$names.Add($name) }
    $rootManifest = Join-Path $RepoRoot 'Cargo.toml'
    $text = Read-Text $rootManifest
    $match = [regex]::Match($text, '(?ms)^exclude\s*=\s*\[(.*?)\]')
    if ($match.Success) {
        foreach ($item in [regex]::Matches($match.Groups[1].Value, '"([^"]+)"')) {
            [void]$names.Add((Split-Path -Leaf $item.Groups[1].Value))
        }
    } else {
        Write-Warning 'ルートの Cargo.toml に exclude が見つかりません (外したクレートの規則は LiveKit の依存だけに効きます)'
    }
    return , $names
}

# --- Cargo.toml -------------------------------------------------------------------------------

function ConvertTo-ManifestItem([string]$Line, [string]$Context) {
    # 1 行を「項目」に分類する。解けない形は $null を返す。
    $trimmed = $Line.Trim()
    if ($trimmed -eq '') {
        return @{ Kind = 'text'; Key = 'blank'; Line = $Line }
    }
    if ($trimmed.StartsWith('#')) {
        return @{ Kind = 'text'; Key = "comment:$trimmed"; Line = $Line }
    }
    if ($trimmed.StartsWith('[')) {
        return $null
    }
    $withoutComment = ($trimmed -replace '\s+#[^"]*$', '')
    $opens = ([regex]::Matches($withoutComment, '[\[{]')).Count
    $closes = ([regex]::Matches($withoutComment, '[\]}]')).Count
    if ($Context -like 'array:*') {
        $itemMatch = [regex]::Match($trimmed, '^"([^"]+)"\s*,?\s*(#.*)?$')
        if ($itemMatch.Success) {
            return @{ Kind = 'item'; Key = "item:" + $itemMatch.Groups[1].Value; Name = $itemMatch.Groups[1].Value; Line = $Line }
        }
        return $null
    }
    if ($opens -ne $closes) {
        return $null
    }
    $entryMatch = [regex]::Match($trimmed, '^("?)([A-Za-z0-9_.-]+)\1\s*=')
    if ($entryMatch.Success) {
        $fullKey = $entryMatch.Groups[2].Value
        $name = ($fullKey -split '\.')[0]
        if ($fullKey -match '\.workspace$' -or $fullKey -notmatch '\.') {
            return @{ Kind = 'entry'; Key = "entry:$name"; Name = $name; FullKey = $fullKey; Line = $Line }
        }
        return @{ Kind = 'entry'; Key = "entry:$fullKey"; Name = $name; FullKey = $fullKey; Line = $Line }
    }
    return $null
}

function Test-ExcludedItem($Item, $Excluded) {
    if ($null -eq $Item -or $Item.Kind -eq 'text') {
        return $false
    }
    $name = $Item.Name
    if ($Item.Kind -eq 'item') {
        # members の "crates/call"、features の "call/test-support" や "dep:call"
        $name = $name -replace '^dep:', ''
        $name = ($name -split '/')[0]
        if ($Item.Name -like 'crates/*') {
            $name = Split-Path -Leaf $Item.Name
        }
    }
    return $Excluded.Contains($name)
}

function Remove-ExcludedFeatures([string]$Line, $Excluded) {
    # `x = { workspace = true, features = ["call"] }` から外したクレートの feature を取り除いて、
    # 残りが workspace だけなら `x.workspace = true` の形にする。
    $match = [regex]::Match($Line, '^(\s*)([A-Za-z0-9_-]+)\s*=\s*\{(.*)\}\s*$')
    if (-not $match.Success) {
        return $Line
    }
    $indent = $match.Groups[1].Value
    $name = $match.Groups[2].Value
    $body = $match.Groups[3].Value
    $featuresMatch = [regex]::Match($body, 'features\s*=\s*\[([^\]]*)\]')
    if (-not $featuresMatch.Success) {
        return $Line
    }
    $kept = @()
    foreach ($feature in [regex]::Matches($featuresMatch.Groups[1].Value, '"([^"]+)"')) {
        $featureName = $feature.Groups[1].Value
        $crate = (($featureName -replace '^dep:', '') -split '/')[0]
        if (-not $Excluded.Contains($crate)) {
            $kept += '"' + $featureName + '"'
        }
    }
    if ($kept.Count -gt 0) {
        $newFeatures = 'features = [' + ($kept -join ', ') + ']'
        $body = $body.Substring(0, $featuresMatch.Index) + $newFeatures + $body.Substring($featuresMatch.Index + $featuresMatch.Length)
    } else {
        $body = $body.Substring(0, $featuresMatch.Index) + $body.Substring($featuresMatch.Index + $featuresMatch.Length)
    }
    $parts = @($body -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
    if ($parts.Count -eq 1 -and $parts[0] -match '^workspace\s*=\s*true$') {
        return "$indent$name.workspace = true"
    }
    return "$indent$name = { " + ($parts -join ', ') + ' }'
}

function Get-SortKey($Item) {
    if ($Item.Kind -eq 'item') { return $Item.Name }
    if ($Item.Kind -eq 'entry') { return ($Item.FullKey -replace '\.workspace$', '') }
    return $null
}

function Test-Sorted($Items) {
    $keys = @($Items | Where-Object { $_.Kind -ne 'text' } | ForEach-Object { Get-SortKey $_ })
    for ($index = 1; $index -lt $keys.Count; $index++) {
        if ([string]::CompareOrdinal($keys[$index - 1], $keys[$index]) -gt 0) {
            return $false
        }
    }
    return $true
}

function ConvertTo-Items([string[]]$Lines, [string]$Context) {
    # 同じ行 (空行やコメント) が複数あっても区別できるように出現回数を鍵に足す
    $seen = @{}
    $items = @()
    foreach ($line in $Lines) {
        $item = ConvertTo-ManifestItem $line $Context
        if ($null -eq $item) {
            return $null
        }
        $count = 0
        if ($seen.ContainsKey($item.Key)) { $count = $seen[$item.Key] }
        $seen[$item.Key] = $count + 1
        $item.Key = "$($item.Key)#$count"
        $items += $item
    }
    return , $items
}

function Resolve-Hunk($Hunk, [string]$Section, [string]$Context, $Excluded, $ForkPointLines, $Notes) {
    # 戻り値: 解けたら結果の行の配列、解けなければ理由の文字列
    if ($null -eq $Hunk.Base) {
        return '基準の側 (|||||||) がありません'
    }
    $ours = ConvertTo-Items $Hunk.Ours $Context
    $base = ConvertTo-Items $Hunk.Base $Context
    $theirs = ConvertTo-Items $Hunk.Theirs $Context
    if ($null -eq $ours -or $null -eq $base -or $null -eq $theirs) {
        return '1 行 1 項目の形ではありません (複数行の値・表の見出しなど)'
    }
    $oursByKey = @{}; foreach ($item in $ours) { $oursByKey[$item.Key] = $item }
    $baseByKey = @{}; foreach ($item in $base) { $baseByKey[$item.Key] = $item }
    $theirsByKey = @{}; foreach ($item in $theirs) { $theirsByKey[$item.Key] = $item }
    $keepExcludedAdditions = ($Section -eq 'workspace.dependencies')

    $decided = @{}
    $allKeys = @($ours + $base + $theirs | ForEach-Object { $_.Key } | Select-Object -Unique)
    foreach ($key in $allKeys) {
        $o = $oursByKey[$key]; $b = $baseByKey[$key]; $t = $theirsByKey[$key]
        $oLine = if ($o) { $o.Line } else { $null }
        $bLine = if ($b) { $b.Line } else { $null }
        $tLine = if ($t) { $t.Line } else { $null }
        $any = if ($o) { $o } elseif ($t) { $t } else { $b }
        if ($oLine -ceq $tLine) {
            $decided[$key] = $o
        } elseif ($oLine -ceq $bLine) {
            if ($t -and -not $b -and -not $keepExcludedAdditions -and (Test-ExcludedItem $t $Excluded)) {
                $Notes.Add("  upstream が足した外したクレートの行を入れない: $($tLine.Trim())")
                $decided[$key] = $null
            } else {
                $decided[$key] = $t
            }
        } elseif ($tLine -ceq $bLine) {
            $decided[$key] = $o
        } elseif ($null -eq $o -and $b -and (Test-ExcludedItem $any $Excluded)) {
            $Notes.Add("  フォークが消した外したクレートの行を消したままにする (upstream の変更: $($tLine.Trim()))")
            $decided[$key] = $null
        } elseif ($o -and $t -and (Remove-ExcludedFeatures $tLine $Excluded) -ceq $oLine) {
            $Notes.Add("  外したクレートの feature を除けば同じなのでフォークの行を採る: $($oLine.Trim())")
            $decided[$key] = $o
        } elseif ($o -and $t -and $any.Kind -ne 'text' -and $ForkPointLines.Contains($oLine.Trim())) {
            $Notes.Add("  フォークの行は $($script:ResolvedForkPoint) のまま (リリースブランチの変更) なので upstream を採る: $($tLine.Trim())")
            $decided[$key] = $t
        } elseif ($o -and $t -and $Section -eq 'package' -and $any.Kind -eq 'entry' -and $any.FullKey -eq 'version') {
            $Notes.Add("  [package] の version は upstream を採る: $($tLine.Trim())")
            $decided[$key] = $t
        } else {
            return "両側が違うふうに変えた項目: ours=[$oLine] theirs=[$tLine]"
        }
    }

    # フォークの並びを土台に、upstream だけにある行を upstream での直前の行の後ろへ差し込む
    $result = New-Object System.Collections.Generic.List[object]
    foreach ($item in $ours) {
        if ($decided[$item.Key]) { $result.Add($decided[$item.Key]) }
    }
    $placed = @{}
    foreach ($item in $result) { $placed[$item.Key] = $true }
    for ($index = 0; $index -lt $theirs.Count; $index++) {
        $item = $theirs[$index]
        if (-not $decided[$item.Key] -or $placed.ContainsKey($item.Key)) {
            continue
        }
        $insertAt = 0
        for ($previous = $index - 1; $previous -ge 0; $previous--) {
            $previousKey = $theirs[$previous].Key
            if ($placed.ContainsKey($previousKey)) {
                for ($position = 0; $position -lt $result.Count; $position++) {
                    if ($result[$position].Key -eq $previousKey) { $insertAt = $position + 1 }
                }
                break
            }
        }
        $result.Insert($insertAt, $decided[$item.Key])
        $placed[$item.Key] = $true
    }

    $hasText = @($result | Where-Object { $_.Kind -eq 'text' }).Count -gt 0
    $sortable = (@($ours | Where-Object { $_.Kind -ne 'text' }).Count + @($theirs | Where-Object { $_.Kind -ne 'text' }).Count) -gt 0
    if (-not $hasText -and $sortable -and (Test-Sorted $ours) -and (Test-Sorted $theirs)) {
        # Sort-Object はカルチャ依存なので、Test-Sorted と同じ序数の比較で挿入ソートする
        $sorted = New-Object System.Collections.Generic.List[object]
        foreach ($item in $result) {
            $position = $sorted.Count
            while ($position -gt 0 -and [string]::CompareOrdinal((Get-SortKey $sorted[$position - 1]), (Get-SortKey $item)) -gt 0) {
                $position--
            }
            $sorted.Insert($position, $item)
        }
        return , @($sorted | ForEach-Object { $_.Line })
    }
    return , @($result | ForEach-Object { $_.Line })
}

function Split-Conflicts([string]$Text) {
    # 行と衝突の塊に分ける。印の形が崩れていたら $null
    $lines = $Text -split "`n", -1
    $segments = New-Object System.Collections.Generic.List[object]
    $state = 'outside'
    $hunk = $null
    foreach ($rawLine in $lines) {
        $line = $rawLine.TrimEnd("`r")
        if ($line -match '^<{7}( |$)') {
            if ($state -ne 'outside') { return $null }
            $hunk = @{ Ours = New-Object System.Collections.Generic.List[string]; Base = $null; Theirs = New-Object System.Collections.Generic.List[string]; Start = $rawLine; End = $null; Raw = New-Object System.Collections.Generic.List[string] }
            $hunk.Raw.Add($rawLine)
            $state = 'ours'
            continue
        }
        if ($state -ne 'outside') { $hunk.Raw.Add($rawLine) }
        if ($line -match '^\|{7}( |$)') {
            if ($state -ne 'ours') { return $null }
            $hunk.Base = New-Object System.Collections.Generic.List[string]
            $state = 'base'
            continue
        }
        if ($line -match '^={7}$') {
            if ($state -ne 'ours' -and $state -ne 'base') { return $null }
            $state = 'theirs'
            continue
        }
        if ($line -match '^>{7}( |$)') {
            if ($state -ne 'theirs') { return $null }
            $segments.Add(@{ Hunk = $hunk })
            $hunk = $null
            $state = 'outside'
            continue
        }
        switch ($state) {
            'outside' { $segments.Add(@{ Line = $rawLine }) }
            'ours' { $hunk.Ours.Add($line) }
            'base' { $hunk.Base.Add($line) }
            'theirs' { $hunk.Theirs.Add($line) }
        }
    }
    if ($state -ne 'outside') { return $null }
    return , $segments
}

function Get-Context([string[]]$Before) {
    # 塊の直前までの行から、今いる表 ([section]) と開いている配列 (members = [ など) を求める
    $section = ''
    $array = ''
    foreach ($line in $Before) {
        $trimmed = $line.Trim()
        if ($array) {
            if ($trimmed -match '^\]') { $array = '' }
            continue
        }
        $header = [regex]::Match($trimmed, '^\[\[?([^\]]+)\]\]?')
        if ($header.Success) {
            $section = $header.Groups[1].Value.Trim()
            continue
        }
        $open = [regex]::Match($trimmed, '^([A-Za-z0-9_."-]+)\s*=\s*\[\s*(#.*)?$')
        if ($open.Success) {
            $array = $open.Groups[1].Value
        }
    }
    $context = if ($array) { "array:$array" } else { 'table' }
    return @{ Section = $section; Context = $context }
}

function Repair-MissingBase([string]$RelativePath, [string]$Text) {
    # 基準の側の無い印 (merge 形式) を index の 3 つの段から diff3 形式で作り直す。
    # 手で触ったファイルを壊さないように、merge 形式で作り直した結果が今のファイルと
    # 印の行を除いて一致するときだけ置き換える。
    if (-not (Test-InGitRepo)) { return $null }
    $stages = @{}
    foreach ($stage in 1, 2, 3) {
        $content = Get-GitStage $RelativePath $stage
        if ($null -eq $content) { return $null }
        $file = [System.IO.Path]::GetTempFileName()
        [System.IO.File]::WriteAllText($file, $content, $Utf8NoBom)
        $stages[$stage] = $file
    }
    try {
        # merge-file の終了コードは衝突の数なので、失敗の判定には使わない
        $merged = Invoke-Git @('merge-file', '-p', $stages[2], $stages[1], $stages[3])
        $normalize = { param($value) (($value -split "`n") | Where-Object { $_ -notmatch '^(<{7}|>{7}|\|{7}|={7})' }) -join "`n" }
        if ((& $normalize $merged.Output) -cne (& $normalize $Text)) {
            return $null
        }
        $diff3 = Invoke-Git @('merge-file', '-p', '--diff3', '-L', 'HEAD', '-L', 'base', '-L', 'upstream', $stages[2], $stages[1], $stages[3])
        return $diff3.Output
    } finally {
        foreach ($file in $stages.Values) { Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue }
    }
}

function Resolve-Manifest([string]$RelativePath, $Excluded) {
    $file = Join-Path $RepoRoot $RelativePath
    $text = Read-Text $file
    $segments = Split-Conflicts $text
    if ($null -eq $segments) {
        return @{ Resolved = 0; Left = @("$RelativePath : 衝突の印の形が崩れているので触りません") }
    }
    $hunks = @($segments | Where-Object { $_.Hunk })
    if ($hunks.Count -eq 0) {
        return @{ Resolved = 0; Left = @() }
    }
    if (@($hunks | Where-Object { $null -eq $_.Hunk.Base }).Count -gt 0) {
        $repaired = Repair-MissingBase $RelativePath $text
        if ($null -ne $repaired) {
            Write-Host "$RelativePath : 基準の側が無いので index から diff3 形式で作り直しました"
            $text = $repaired
            $segments = Split-Conflicts $text
        }
    }

    $forkPointLines = New-Object System.Collections.Generic.HashSet[string]
    $forkPointText = Get-ForkPointText $RelativePath
    if ($null -ne $forkPointText) {
        foreach ($line in ($forkPointText -split "`n")) {
            if ($line.Trim() -ne '') { [void]$forkPointLines.Add($line.Trim()) }
        }
    }

    $output = New-Object System.Collections.Generic.List[string]
    $left = @()
    $resolvedCount = 0
    $hunkIndex = 0
    foreach ($segment in $segments) {
        if (-not $segment.Hunk) {
            $output.Add($segment.Line)
            continue
        }
        $hunkIndex++
        $hunk = $segment.Hunk
        $contextInfo = Get-Context $output.ToArray()
        $notes = New-Object System.Collections.Generic.List[string]
        $resolution = Resolve-Hunk $hunk $contextInfo.Section $contextInfo.Context $Excluded $forkPointLines $notes
        $firstLine = ($output.Count + 1)
        if ($resolution -is [string]) {
            $left += "$RelativePath : 塊 $hunkIndex (結果の $firstLine 行目付近, [$($contextInfo.Section)]) を残しました: $resolution"
            foreach ($rawLine in $hunk.Raw) { $output.Add($rawLine) }
            continue
        }
        $resolvedCount++
        Write-Host "$RelativePath : 塊 $hunkIndex ([$($contextInfo.Section)] $($contextInfo.Context)) を解きました ($($hunk.Ours.Count)/$($hunk.Theirs.Count) 行 -> $(@($resolution).Count) 行)"
        foreach ($note in $notes) { Write-Host $note }
        foreach ($line in $resolution) { $output.Add($line) }
    }
    if ($resolvedCount -gt 0) {
        Write-Text $file ($output -join "`n")
    }
    return @{ Resolved = $resolvedCount; Left = $left }
}

# --- Cargo.lock -------------------------------------------------------------------------------

function Get-LockPackages([string]$Text) {
    $packages = @{}
    foreach ($block in ($Text -split '\[\[package\]\]') | Select-Object -Skip 1) {
        $name = [regex]::Match($block, '(?m)^name = "([^"]+)"').Groups[1].Value
        $version = [regex]::Match($block, '(?m)^version = "([^"]+)"').Groups[1].Value
        $sourceMatch = [regex]::Match($block, '(?m)^source = "([^"]+)"')
        if (-not $sourceMatch.Success) {
            continue
        }
        if (-not $packages.ContainsKey($name)) {
            $packages[$name] = New-Object System.Collections.Generic.HashSet[string]
        }
        [void]$packages[$name].Add("$version $($sourceMatch.Groups[1].Value)")
    }
    return $packages
}

function Get-LockSide([string]$FromFile, [int]$Stage) {
    if ($FromFile) {
        return Read-Text (Resolve-Path $FromFile).Path
    }
    if (Test-InGitRepo) {
        return Get-GitStage 'Cargo.lock' $Stage
    }
    return $null
}

function Resolve-Lock {
    $theirs = Get-LockSide $TheirsLock 3
    if ($null -eq $theirs) {
        Write-Host 'Cargo.lock : upstream の側 (:3:Cargo.lock / -TheirsLock) が取れないので触りません'
        return $false
    }
    $ours = Get-LockSide $OursLock 2
    # 固定の検査の基準は merge の基準 (:1:) ではなくフォークが取り込んだリリース。:1: はリリース
    # ブランチが分かれる前の main なので、リリースブランチの版上げまで「固定」に見えてしまう
    $base = Get-ForkPointText 'Cargo.lock'
    $lockFile = Join-Path $RepoRoot 'Cargo.lock'
    if ($DryRun) {
        Write-Host 'Cargo.lock : upstream の側を置いて cargo update --workspace で解き直すはずです (-DryRun なので何もしません)'
        return $true
    }
    Write-Text $lockFile $theirs
    # `--workspace` はワークスペースのクレートだけを更新し、それ以外は Cargo.lock に無いものだけを
    # 解く (= upstream が選んだ版を保ったまま、フォークが足した依存を入れて使われない依存を落とす)。
    # cargo metadata と違ってパッケージのソースを落とさずに済む。
    Write-Host 'Cargo.lock : upstream の側を置いて cargo update --workspace で解き直します'
    $result = Invoke-Cargo @('update', '--workspace', '--offline')
    if ($result.ExitCode -ne 0) {
        Write-Host 'Cargo.lock : --offline で解けなかったのでネットワークありでもう一度解きます'
        $result = Invoke-Cargo @('update', '--workspace')
        if ($result.ExitCode -ne 0) {
            Write-Host 'Cargo.lock : cargo update が失敗しました。Cargo.lock は upstream の側のままです'
            $result.Output | ForEach-Object { Write-Host "  $_" }
            return $false
        }
    }
    $changes = @($result.Output | Where-Object { $_ -match '^\s*(Adding|Removing|Updating|Downgrading)\s' } | ForEach-Object { $_.Trim() })
    $summary = @($changes | Group-Object { ($_ -split '\s+')[0] } | ForEach-Object { "$($_.Name) $($_.Count)" }) -join ' / '
    Write-Host "Cargo.lock : upstream の側からの変化: $summary"
    $changes | Where-Object { $_ -like 'Adding*' } | ForEach-Object { Write-Host "  $_" }

    if ($null -eq $ours -or $null -eq $base) {
        Write-Host 'Cargo.lock : フォークの側か -ForkPoint の Cargo.lock が取れないので固定の検査は省きます'
        return $true
    }
    $resolved = Get-LockPackages (Read-Text $lockFile)
    $oursPackages = Get-LockPackages $ours
    $basePackages = Get-LockPackages $base
    $theirsPackages = Get-LockPackages $theirs
    $pins = 0
    $broken = @()
    foreach ($name in $oursPackages.Keys) {
        foreach ($entry in $oursPackages[$name]) {
            $inBase = $basePackages.ContainsKey($name) -and $basePackages[$name].Contains($entry)
            if ($inBase) { continue }
            # -ForkPoint に同名のパッケージが無いものはフォークが足した依存で、Cargo.toml から
            # 入り直すはずなので、固定としては見ない
            if (-not $basePackages.ContainsKey($name)) { continue }
            $pins++
            $inTheirs = $theirsPackages.ContainsKey($name) -and $theirsPackages[$name].Contains($entry)
            $kept = $resolved.ContainsKey($name) -and $resolved[$name].Contains($entry)
            if (-not $kept -and -not $inTheirs) {
                $now = if ($resolved.ContainsKey($name)) { ($resolved[$name] -join ', ') } else { '(無し)' }
                $broken += "  $name : フォーク $entry / 解き直し後 $now  (戻すなら: cargo update -p $name --precise $(($entry -split ' ')[0]))"
            }
        }
    }
    if ($broken.Count -gt 0) {
        Write-Host "Cargo.lock : フォークが Cargo.lock で固定していた版のうち $($broken.Count) 件が変わりました"
        $broken | ForEach-Object { Write-Host $_ }
    } else {
        Write-Host "Cargo.lock : 解き直しました (フォークが Cargo.lock だけで固定していた版: $pins 件、崩れたもの無し)"
    }
    return $true
}

# --- main -------------------------------------------------------------------------------------

if (-not $Path) {
    if (-not (Test-InGitRepo)) {
        Write-Error '-Path を指定するか、merge の途中の作業ツリーで実行してください'
    }
    $Path = @((Invoke-Git @('diff', '--name-only', '--diff-filter=U')).Output -split "`n" | Where-Object { $_ -ne '' })
}
# `powershell -File` は配列を 1 つの文字列で渡すので、カンマ区切りも受け付ける
$Path = @($Path | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() -replace '\\', '/' } | Where-Object { $_ -ne '' })
$manifests = @($Path | Where-Object { $_ -eq 'Cargo.toml' -or $_ -like '*/Cargo.toml' })
$lockConflicted = @($Path | Where-Object { $_ -eq 'Cargo.lock' }).Count -gt 0
$others = @($Path | Where-Object { $_ -ne 'Cargo.lock' -and $_ -ne 'Cargo.toml' -and $_ -notlike '*/Cargo.toml' })

$excluded = Get-ExcludedCrates
Write-Host ("外したクレートとして扱う名前: " + (($excluded | Sort-Object) -join ', '))
$script:ResolvedForkPoint = Resolve-ForkPoint
if ($script:ResolvedForkPoint) {
    Write-Host "フォークが取り込んだ upstream のリリース: $($script:ResolvedForkPoint)"
} else {
    Write-Host 'フォークが取り込んだ upstream のリリースが分からないので、リリースブランチの規則と固定の検査は省きます (-ForkPoint で指定できます)'
}

$left = @()
$resolvedTotal = 0
foreach ($manifest in $manifests) {
    $file = Join-Path $RepoRoot $manifest
    if (-not (Test-Path -LiteralPath $file)) {
        $left += "$manifest : 作業ツリーにありません (削除と変更の衝突など)"
        continue
    }
    $result = Resolve-Manifest $manifest $excluded
    $resolvedTotal += $result.Resolved
    $left += $result.Left
}

$manifestsStillConflicted = @($manifests | Where-Object {
        $file = Join-Path $RepoRoot $_
        (Test-Path -LiteralPath $file) -and ((Read-Text $file) -match '(?m)^<{7}( |$)')
    })

if ($lockConflicted -and -not $SkipLock) {
    if ($manifestsStillConflicted.Count -gt 0 -and -not $DryRun) {
        $left += 'Cargo.lock : Cargo.toml に衝突が残っているので解き直していません (解いてからもう一度実行してください)'
    } else {
        [void](Resolve-Lock)
    }
}

Write-Host ''
Write-Host "Cargo.toml の塊を $resolvedTotal 個解きました。git add はしていません。"
if ($left.Count -gt 0) {
    Write-Host "残した衝突 ($($left.Count)):"
    $left | ForEach-Object { Write-Host "  $_" }
}
if ($others.Count -gt 0) {
    Write-Host "この道具が扱わない衝突ファイル ($($others.Count)):"
    $others | ForEach-Object { Write-Host "  $_" }
}
