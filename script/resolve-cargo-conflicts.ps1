<#
.SYNOPSIS
Mechanically resolves the conflicts that `git merge` of an upstream release leaves in
Cargo.toml and Cargo.lock.

.DESCRIPTION
Rewrites only the Cargo.toml and Cargo.lock files that still hold conflict markers
(<<<<<<< / ||||||| / ======= / >>>>>>>) and reports what it did. It neither runs
git add nor commits.

Cargo.toml (the root and every crate):
  Each conflict hunk is merged 3-way from its base, fork and upstream sides, one line
  being one item (a dependency line, an element of members or of features, a comment
  line).
    - An item only one side changed takes that side. Lines both sides added are both kept.
    - When both sides are in alphabetical order, so is the result.
    - Lines of the crates the fork removed (the root Cargo.toml's `exclude` plus LiveKit's
      native dependencies) stay removed where the fork removed them, and are not added
      where upstream added them (except in `[workspace.dependencies]`;
      .forgekairos-docs/DESIGN.md section 4.6, ADR-F06).
    - When upstream's line without the features of removed crates is the fork's line, the
      fork's line is taken (for example `git_ui = { workspace = true, features = ["call"] }`
      and `git_ui.workspace = true`).
    - A line both sides changed takes upstream's when the fork's line is still the line of
      the upstream release the fork merged last (-ForkPoint; by default the newest
      refs/upstream/tags/* reachable from HEAD). The merge base is where the release branch
      left upstream's main, so the release branch's cherry-picks (the `[package]` version,
      wasmtime version bumps) land here every time.
    - `version` in `[package]` takes upstream's even when none of the above applies.
  Anything else (an item both sides changed differently, a value over several lines, a
  table header) is left as it is and listed. A hunk with one item that cannot be resolved
  is left whole.

Cargo.lock:
  Puts upstream's side (:3:Cargo.lock) in place and lets `cargo update --workspace` resolve
  it again; the dependencies the fork added come in from Cargo.toml. Does nothing while a
  Cargo.toml still has conflicts (resolve them, then run this again). Afterwards it checks
  that the packages whose versions the fork's Cargo.lock changed from -ForkPoint's (pins
  that only exist in Cargo.lock) kept them, and lists the ones that did not.

A hunk without its base side (|||||||) is first rebuilt with `git merge-file --diff3` from
the index stages :1: / :2: / :3:, as long as the file was not edited by hand. Setting
`git config merge.conflictStyle zdiff3` (or diff3) before merging avoids this.

.EXAMPLE
git merge refs/upstream/tags/v1.18.1
powershell -NoProfile -File script\resolve-cargo-conflicts.ps1
powershell -NoProfile -File script\resolve-cargo-conflicts.ps1 -DryRun

.EXAMPLE
# Outside a merge, on copies of conflicted files (git is asked in the repository I:\zed-jp-llm)
powershell -NoProfile -File script\resolve-cargo-conflicts.ps1 -RepoRoot C:\tmp\merged -GitRepo I:\zed-jp-llm `
    -Path "Cargo.toml,crates\zed\Cargo.toml,Cargo.lock" -ForkPoint refs/upstream/tags/v1.17.2 `
    -TheirsLock C:\tmp\theirs.lock -OursLock C:\tmp\ours.lock
#>
param(
    # The Cargo.toml / Cargo.lock files to resolve, relative to RepoRoot (comma-separated
    # works too). By default the files git considers conflicted.
    [string[]]$Path,
    # The root of the working tree (by default this script's parent folder).
    [string]$RepoRoot,
    # The repository git is asked in (by default RepoRoot).
    [string]$GitRepo,
    # The upstream release the fork merged last (by default the newest
    # refs/upstream/tags/* reachable from HEAD).
    [string]$ForkPoint,
    # Read upstream's and the fork's Cargo.lock from these files instead of the index
    # (:3: / :2:), to try this outside a merge.
    [string]$TheirsLock,
    [string]$OursLock,
    # Do not resolve Cargo.lock again.
    [switch]$SkipLock,
    # Change nothing; only report what would be done.
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

# The third-party dependencies that went away with LiveKit, which the fork removed.
# The fork's own removed crates are read from the root Cargo.toml's `exclude`.
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
    # A PowerShell pipe changes the trailing newline and the encoding, so the output is
    # read as UTF-8 directly.
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
    # cargo writes its progress to stderr, and Windows PowerShell turns stderr redirected
    # with `2>&1` into an exception under $ErrorActionPreference = 'Stop', so it is
    # relaxed here only.
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
        Write-Warning 'The root Cargo.toml has no exclude (the rule for removed crates only covers the LiveKit dependencies).'
    }
    return , $names
}

# --- Cargo.toml -------------------------------------------------------------------------------

function ConvertTo-ManifestItem([string]$Line, [string]$Context) {
    # Classifies one line as an item; $null for a shape this cannot resolve.
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
        # "crates/call" in members, "call/test-support" or "dep:call" in features.
        $name = $name -replace '^dep:', ''
        $name = ($name -split '/')[0]
        if ($Item.Name -like 'crates/*') {
            $name = Split-Path -Leaf $Item.Name
        }
    }
    return $Excluded.Contains($name)
}

function Remove-ExcludedFeatures([string]$Line, $Excluded) {
    # Drops the features of removed crates from `x = { workspace = true, features = ["call"] }`,
    # and writes `x.workspace = true` when only workspace is left.
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
    # The occurrence count goes into the key so that repeated lines (blank lines,
    # comments) stay apart.
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
    # Returns the resulting lines when resolved, or the reason as a string when not.
    if ($null -eq $Hunk.Base) {
        return 'the base side (|||||||) is missing'
    }
    $ours = ConvertTo-Items $Hunk.Ours $Context
    $base = ConvertTo-Items $Hunk.Base $Context
    $theirs = ConvertTo-Items $Hunk.Theirs $Context
    if ($null -eq $ours -or $null -eq $base -or $null -eq $theirs) {
        return 'not one item per line (a value over several lines, a table header, ...)'
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
                $Notes.Add("  not adding the line upstream added for a removed crate: $($tLine.Trim())")
                $decided[$key] = $null
            } else {
                $decided[$key] = $t
            }
        } elseif ($tLine -ceq $bLine) {
            $decided[$key] = $o
        } elseif ($null -eq $o -and $b -and (Test-ExcludedItem $any $Excluded)) {
            $Notes.Add("  keeping the removed crate's line the fork removed out (upstream changed it to: $($tLine.Trim()))")
            $decided[$key] = $null
        } elseif ($o -and $t -and (Remove-ExcludedFeatures $tLine $Excluded) -ceq $oLine) {
            $Notes.Add("  taking the fork's line, which is upstream's without removed crates' features: $($oLine.Trim())")
            $decided[$key] = $o
        } elseif ($o -and $t -and $any.Kind -ne 'text' -and $ForkPointLines.Contains($oLine.Trim())) {
            $Notes.Add("  taking upstream's line, since the fork's is still $($script:ResolvedForkPoint)'s (a release branch change): $($tLine.Trim())")
            $decided[$key] = $t
        } elseif ($o -and $t -and $Section -eq 'package' -and $any.Kind -eq 'entry' -and $any.FullKey -eq 'version') {
            $Notes.Add("  taking upstream's [package] version: $($tLine.Trim())")
            $decided[$key] = $t
        } else {
            return "an item both sides changed differently: ours=[$oLine] theirs=[$tLine]"
        }
    }

    # Starts from the fork's order and inserts each line only upstream has after the line
    # it follows in upstream.
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
        # Sort-Object depends on the culture, so this insertion sort compares ordinally,
        # as Test-Sorted does.
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
    # Splits the text into lines and conflict hunks; $null when the markers are malformed.
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
    # Finds, from the lines before a hunk, the table ([section]) it is in and the array
    # (members = [ and the like) that is open.
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
    # Rebuilds markers without a base side (merge style) in diff3 style from the three
    # index stages. So as not to break a file edited by hand, it is replaced only when the
    # merge-style rebuild matches the current file apart from the marker lines.
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
        # merge-file exits with the number of conflicts, so its exit code is not a failure.
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
        return @{ Resolved = 0; Left = @("$RelativePath : the conflict markers are malformed, so it is left alone") }
    }
    $hunks = @($segments | Where-Object { $_.Hunk })
    if ($hunks.Count -eq 0) {
        return @{ Resolved = 0; Left = @() }
    }
    if (@($hunks | Where-Object { $null -eq $_.Hunk.Base }).Count -gt 0) {
        $repaired = Repair-MissingBase $RelativePath $text
        if ($null -ne $repaired) {
            Write-Host "$RelativePath : rebuilt in diff3 style from the index, since the base side was missing"
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
            $left += "$RelativePath : left hunk $hunkIndex (near line $firstLine of the result, [$($contextInfo.Section)]): $resolution"
            foreach ($rawLine in $hunk.Raw) { $output.Add($rawLine) }
            continue
        }
        $resolvedCount++
        Write-Host "$RelativePath : resolved hunk $hunkIndex ([$($contextInfo.Section)] $($contextInfo.Context)) ($($hunk.Ours.Count)/$($hunk.Theirs.Count) lines -> $(@($resolution).Count) lines)"
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
        Write-Host "Cargo.lock : upstream's side (:3:Cargo.lock / -TheirsLock) is not available, so it is left alone"
        return $false
    }
    $ours = Get-LockSide $OursLock 2
    # Pins are checked against the release the fork merged, not the merge base (:1:): :1: is
    # main before the release branch left it, against which the release branch's version
    # bumps would look like pins too.
    $base = Get-ForkPointText 'Cargo.lock'
    $lockFile = Join-Path $RepoRoot 'Cargo.lock'
    if ($DryRun) {
        Write-Host "Cargo.lock : would put upstream's side in place and resolve it with cargo update --workspace (nothing done with -DryRun)"
        return $true
    }
    Write-Text $lockFile $theirs
    # `--workspace` updates only the workspace's crates and resolves only what is missing
    # from Cargo.lock otherwise: upstream's versions stay, the fork's dependencies come in
    # and unused ones go. Unlike cargo metadata it does not download package sources.
    Write-Host "Cargo.lock : putting upstream's side in place and resolving it with cargo update --workspace"
    $result = Invoke-Cargo @('update', '--workspace', '--offline')
    if ($result.ExitCode -ne 0) {
        Write-Host 'Cargo.lock : --offline could not resolve it, so trying again with the network'
        $result = Invoke-Cargo @('update', '--workspace')
        if ($result.ExitCode -ne 0) {
            Write-Host "Cargo.lock : cargo update failed. Cargo.lock is still upstream's side"
            $result.Output | ForEach-Object { Write-Host "  $_" }
            return $false
        }
    }
    $changes = @($result.Output | Where-Object { $_ -match '^\s*(Adding|Removing|Updating|Downgrading)\s' } | ForEach-Object { $_.Trim() })
    $summary = @($changes | Group-Object { ($_ -split '\s+')[0] } | ForEach-Object { "$($_.Name) $($_.Count)" }) -join ' / '
    Write-Host "Cargo.lock : changes from upstream's side: $summary"
    $changes | Where-Object { $_ -like 'Adding*' } | ForEach-Object { Write-Host "  $_" }

    if ($null -eq $ours -or $null -eq $base) {
        Write-Host "Cargo.lock : the fork's side or -ForkPoint's Cargo.lock is not available, so pins are not checked"
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
            # A package -ForkPoint does not have is a dependency the fork added, which comes
            # back in from Cargo.toml, so it is not a pin.
            if (-not $basePackages.ContainsKey($name)) { continue }
            $pins++
            $inTheirs = $theirsPackages.ContainsKey($name) -and $theirsPackages[$name].Contains($entry)
            $kept = $resolved.ContainsKey($name) -and $resolved[$name].Contains($entry)
            if (-not $kept -and -not $inTheirs) {
                $now = if ($resolved.ContainsKey($name)) { ($resolved[$name] -join ', ') } else { '(none)' }
                $broken += "  $name : fork $entry / after resolving $now  (to restore: cargo update -p $name --precise $(($entry -split ' ')[0]))"
            }
        }
    }
    if ($broken.Count -gt 0) {
        Write-Host "Cargo.lock : $($broken.Count) of the versions the fork pinned in Cargo.lock changed"
        $broken | ForEach-Object { Write-Host $_ }
    } else {
        Write-Host "Cargo.lock : resolved (versions the fork pinned only in Cargo.lock: $pins, none changed)"
    }
    return $true
}

# --- main -------------------------------------------------------------------------------------

if (-not $Path) {
    if (-not (Test-InGitRepo)) {
        Write-Error 'Pass -Path, or run this in a working tree in the middle of a merge.'
    }
    $Path = @((Invoke-Git @('diff', '--name-only', '--diff-filter=U')).Output -split "`n" | Where-Object { $_ -ne '' })
}
# `powershell -File` passes an array as one string, so commas separate paths too.
$Path = @($Path | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() -replace '\\', '/' } | Where-Object { $_ -ne '' })
$manifests = @($Path | Where-Object { $_ -eq 'Cargo.toml' -or $_ -like '*/Cargo.toml' })
$lockConflicted = @($Path | Where-Object { $_ -eq 'Cargo.lock' }).Count -gt 0
$others = @($Path | Where-Object { $_ -ne 'Cargo.lock' -and $_ -ne 'Cargo.toml' -and $_ -notlike '*/Cargo.toml' })

$excluded = Get-ExcludedCrates
Write-Host ("Names treated as removed crates: " + (($excluded | Sort-Object) -join ', '))
$script:ResolvedForkPoint = Resolve-ForkPoint
if ($script:ResolvedForkPoint) {
    Write-Host "The upstream release the fork merged: $($script:ResolvedForkPoint)"
} else {
    Write-Host 'The upstream release the fork merged is unknown, so the release branch rule and the pin check are skipped (-ForkPoint names it).'
}

$left = @()
$resolvedTotal = 0
foreach ($manifest in $manifests) {
    $file = Join-Path $RepoRoot $manifest
    if (-not (Test-Path -LiteralPath $file)) {
        $left += "$manifest : not in the working tree (a delete/modify conflict or the like)"
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
        $left += 'Cargo.lock : not resolved, since a Cargo.toml still has conflicts (resolve them and run this again)'
    } else {
        [void](Resolve-Lock)
    }
}

Write-Host ''
Write-Host "Resolved $resolvedTotal Cargo.toml hunks. Nothing was added with git add."
if ($left.Count -gt 0) {
    Write-Host "Conflicts left ($($left.Count)):"
    $left | ForEach-Object { Write-Host "  $_" }
}
if ($others.Count -gt 0) {
    Write-Host "Conflicted files this tool does not handle ($($others.Count)):"
    $others | ForEach-Object { Write-Host "  $_" }
}
