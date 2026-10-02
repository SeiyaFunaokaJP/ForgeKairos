<#
.SYNOPSIS
ForgeKairos の開発ビルド (Windows)。zed.exe と cli.exe を -j 8 でビルドする。

.DESCRIPTION
BUILD-WINDOWS.md のハマりどころを自動で回避する:
- CMake (VS 同梱) が PATH に無ければ補完する
- ディスク残量が少なければ警告し、-FreeDisk 指定時は安全な掃除
  (target\debug\incremental と target\debug\deps の *.exe / *.pdb) を行う
- 並列度は常に -j 8 (無制限だと rustc がメモリ枯渇で落ちる)
- SSH リモート用の remote_server (musl 静的) を zed.exe の隣に同梱する。
  zig / cargo-zigbuild / rustup target が無ければ自動で導入する (-SkipRemoteServer で省略)
- リモートサンドボックス (ADR-F18) 用の slirp4netns 静的バイナリを GitHub リリースから
  取得し zed.exe の隣に置く (-SkipSlirp4netns で省略)
- C の既定フォーマッタ (DESIGN.md §5.2) が使う clang-format.exe を
  target\debug\bin\ へ置く (-SkipClangFormat で省略)

.EXAMPLE
powershell -NoProfile -File script\build-dev.ps1
powershell -NoProfile -File script\build-dev.ps1 -Run
powershell -NoProfile -File script\build-dev.ps1 -FreeDisk
powershell -NoProfile -File script\build-dev.ps1 -SkipRemoteServer
powershell -NoProfile -File script\build-dev.ps1 -SkipSlirp4netns
powershell -NoProfile -File script\build-dev.ps1 -ClangFormatPath C:\LLVM\bin\clang-format.exe
#>
param(
    # ビルド成功後に zed.exe を起動する
    [switch]$Run,
    # ビルド前に target の肥大分 (incremental / deps の exe・pdb) を削除する
    [switch]$FreeDisk,
    # cli.exe のビルドを省略する (コンソール連携を使わない場合)
    [switch]$SkipCli,
    # SSH リモート用 remote_server アーティファクトの生成を省略する
    [switch]$SkipRemoteServer,
    # SSH リモートのサンドボックス用 slirp4netns 静的バイナリの取得を省略する (ADR-F18)
    [switch]$SkipSlirp4netns,
    # C の既定フォーマッタ用 clang-format.exe の同梱を省略する
    [switch]$SkipClangFormat,
    # 同梱する clang-format.exe の入手元。存在しなければ VS 2022 同梱の LLVM を探す
    [string]$ClangFormatPath = 'I:\CustomFormatter\bin\win32\clang-format.exe'
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

# --- CMake (VS 同梱) の PATH 補完 -------------------------------------------
# crates/remote/src/transport.rs::ensure_cmake_available と同じ候補集合
# (Enterprise/Professional/Community/BuildTools x 両方の Program Files) を探す。
# ここで見つからないと、開発ビルドの SSH 接続でも同じ探索が失敗し、cmake 不在の
# 日本語エラーになる (BUILD-WINDOWS.md 「開発ビルドで SSH 接続に失敗する」参照)。
if (-not (Get-Command cmake.exe -ErrorAction SilentlyContinue)) {
    $vsCmakeCandidates = @('Enterprise', 'Professional', 'Community', 'BuildTools') | ForEach-Object {
        $edition = $_
        @($env:ProgramFiles, ${env:ProgramFiles(x86)}) | Where-Object { $_ } | ForEach-Object {
            Join-Path $_ "Microsoft Visual Studio\2022\$edition\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
        }
    }
    $found = $vsCmakeCandidates | Where-Object { Test-Path (Join-Path $_ 'cmake.exe') } | Select-Object -First 1
    if ($found) {
        $env:PATH = "$found;$env:PATH"
        Write-Host "CMake を PATH に追加: $found"
    } else {
        Write-Warning 'cmake.exe が見つかりません。wasmtime 系のビルドで失敗します。docs/jp-enhance/BUILD-WINDOWS.md の前提条件を確認してください。'
    }
}

# --- ディスク残量 -----------------------------------------------------------
function Get-FreeGB {
    $drive = (Get-Item $repoRoot).PSDrive.Name
    [math]::Round((Get-PSDrive $drive).Free / 1GB, 1)
}

if ($FreeDisk) {
    $incremental = Join-Path $repoRoot 'target\debug\incremental'
    if (Test-Path $incremental) {
        Write-Host 'target\debug\incremental を削除しています…'
        Remove-Item -Recurse -Force $incremental
    }
    $deps = Join-Path $repoRoot 'target\debug\deps'
    if (Test-Path $deps) {
        Write-Host 'target\debug\deps の *.exe / *.pdb (テストバイナリの古い世代) を削除しています…'
        Get-ChildItem $deps -File |
            Where-Object { $_.Extension -in '.exe', '.pdb' } |
            Remove-Item -Force
    }
    Write-Host ("掃除後の空き: {0} GB" -f (Get-FreeGB))
}

$freeGB = Get-FreeGB
if ($freeGB -lt 20) {
    Write-Warning ("ディスク残量が {0} GB しかありません。ビルド途中で枯渇したら -FreeDisk を付けて再実行してください。" -f $freeGB)
}

# --- リモート (SSH) 用 remote_server アーティファクト -------------------------
# zed.exe の隣に zed-remote-server-linux-x86_64.gz を置いておくと、SSH 接続時に
# crates/remote がそれを自動アップロードする。エンドユーザー側の作業もネットワーク
# アクセス (egress は既定で遮断) も不要にするのが狙い。
#
# 失敗は throw で伝える。関数の戻り値で成否を返すと、内部で叩いた cargo/rustup の
# 標準出力まで戻り値パイプラインに混ざって真偽判定が壊れるため。

function Get-ZigPath {
    $zig = Get-Command zig -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($zig) { return $zig.Source }

    # winget で入れた zig は PATH に載らないことがあるので既知の配置を直接探す
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
        throw 'zig が無く、winget も使えないため自動導入できません。https://ziglang.org/ から zig を入れて PATH に通してください。'
    }
    Write-Host 'zig が見つからないので winget で導入します (winget install -e --id zig.zig) …'
    # winget は「既にインストール済み」でも非 0 を返すため、終了コードではなく実体の有無で判定する
    winget install -e --id zig.zig --accept-source-agreements --accept-package-agreements --silent | Out-Host
}

function Get-NewestSourceWriteTimeUtc {
    $newest = Get-ChildItem -Path (Join-Path $repoRoot 'crates') -Recurse -File -Include '*.rs', 'Cargo.toml' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    $times = @()
    if ($newest) { $times += $newest.LastWriteTimeUtc }
    $lockFile = Join-Path $repoRoot 'Cargo.lock'
    if (Test-Path $lockFile) { $times += (Get-Item $lockFile).LastWriteTimeUtc }
    if ($times.Count -eq 0) { return $null }
    return ($times | Sort-Object -Descending | Select-Object -First 1)
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
        # GZipStream はフッタを書くので destination より先に閉じる
        if ($gzipStream) { $gzipStream.Dispose() }
        if ($destination) { $destination.Dispose() }
        if ($source) { $source.Dispose() }
    }
}

function Build-RemoteServerArtifact {
    $target = 'x86_64-unknown-linux-musl'
    $artifactPath = Join-Path $repoRoot 'target\debug\zed-remote-server-linux-x86_64.gz'
    $binaryPath = Join-Path $repoRoot "target\remote_server\$target\debug\remote_server"

    $newestSource = Get-NewestSourceWriteTimeUtc
    if ((Test-Path $artifactPath) -and $newestSource -and ((Get-Item $artifactPath).LastWriteTimeUtc -ge $newestSource)) {
        Write-Host ("remote_server アーティファクトは最新です: {0}" -f $artifactPath)
        return
    }

    $zigPath = Get-ZigPath
    if (-not $zigPath) {
        Install-Zig
        $zigPath = Get-ZigPath
    }
    if (-not $zigPath) {
        throw 'winget 実行後も zig が見つかりません。新しいシェルを開き直すか、zig を PATH に通してください。'
    }
    $zigDir = Split-Path -Parent $zigPath
    if (($env:PATH -split ';') -notcontains $zigDir) {
        $env:PATH = "$zigDir;$env:PATH"
        Write-Host "zig を PATH に追加: $zigDir"
    }

    if (-not (Get-Command rustup -CommandType Application -ErrorAction SilentlyContinue)) {
        throw 'rustup が見つかりません (https://rustup.rs/)。'
    }
    Write-Host "rustup target add $target …"
    rustup target add $target
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $target に失敗しました (exit $LASTEXITCODE)。"
    }

    if (-not (Get-Command cargo-zigbuild -CommandType Application -ErrorAction SilentlyContinue)) {
        Write-Host 'cargo-zigbuild を導入します (cargo install cargo-zigbuild --locked) …'
        cargo install cargo-zigbuild --locked
        if ($LASTEXITCODE -ne 0) {
            throw "cargo install cargo-zigbuild に失敗しました (exit $LASTEXITCODE)。"
        }
    }

    # musl 静的リンク。glibc のバージョン差に依存しない単一バイナリにするため。
    # strip=debuginfo は DWARF だけを落とし、シンボル名 (= バックトレース) は残す。
    # debug ビルドの remote_server は素のままだと 582 MB あり、dev ビルドはバイナリ名に
    # バージョンが入るのでリモートには接続のたびに新しい世代が積み上がる。
    # 2026-08-30 に 31 GB のリモートがこれで枯渇した (DEVLOG 同日)
    $previousRustFlags = $env:RUSTFLAGS
    $rustFlags = '-C target-feature=+crt-static -C strip=debuginfo'
    if ($previousRustFlags) { $rustFlags = "$previousRustFlags $rustFlags" }
    if ($env:ZED_ZSTD_MUSL_LIB) { $rustFlags = "$rustFlags -C link-arg=-L$env:ZED_ZSTD_MUSL_LIB" }

    $env:RUSTFLAGS = $rustFlags
    try {
        Write-Host "cargo zigbuild -p remote_server --target $target -j 8 …"
        cargo zigbuild -p remote_server --features debug-embed --target $target --target-dir target/remote_server -j 8
        $buildExitCode = $LASTEXITCODE
    } finally {
        if ($null -eq $previousRustFlags) {
            if (Test-Path Env:\RUSTFLAGS) { Remove-Item Env:\RUSTFLAGS }
        } else {
            $env:RUSTFLAGS = $previousRustFlags
        }
    }
    if ($buildExitCode -ne 0) {
        throw "cargo zigbuild -p remote_server に失敗しました (exit $buildExitCode)。"
    }
    if (-not (Test-Path $binaryPath)) {
        throw "ビルドは成功しましたが成果物が見つかりません: $binaryPath"
    }

    $artifactDir = Split-Path -Parent $artifactPath
    if (-not (Test-Path $artifactDir)) {
        New-Item -ItemType Directory -Force $artifactDir | Out-Null
    }
    Write-Host "gzip 圧縮しています: $artifactPath"
    Compress-ToGzip -SourcePath $binaryPath -DestinationPath $artifactPath
    # 展開後のサイズがリモートのディスクに効く数字なので、圧縮前後の両方を出す
    Write-Host ("remote_server アーティファクト: {0} ({1:N1} MB / 展開後 {2:N1} MB)" -f `
        $artifactPath, `
        ((Get-Item $artifactPath).Length / 1MB), `
        ((Get-Item $binaryPath).Length / 1MB))
}

# --- リモートサンドボックス用 slirp4netns の同梱 (ADR-F18) --------------------
# リモート (SSH Linux) で AI エージェントのコマンドを netns で隔離する際、ゲスト
# netns の外向き転送を slirp4netns が担う (host-loopback の egress プロキシへ
# --outbound-addr=127.0.0.1 で橋渡しし、外部 NAT は殺す)。zed.exe の隣に
# slirp4netns-linux-x86_64 を置いておくと crates/remote が接続時に ~/.zed_server/
# へ自動アップロードする (remote_server アーティファクトと同じ供給経路)。
#
# ライセンス: slirp4netns = GPLv2+ (本フォーク GPL-3 と両立)、内部の libslirp =
# MIT/BSD。配布時は README / BUILD-WINDOWS.md の note に従うこと。
$slirp4netnsVersion = 'v1.2.3'
function Get-Slirp4netnsBinary {
    # 静的リンクの単一バイナリ (公式リリース資産 slirp4netns-x86_64)。glibc 非依存
    # なので 18.04 / 24.04 の双方で動く (実機で確認済み)。
    $artifactPath = Join-Path $repoRoot 'target\debug\slirp4netns-linux-x86_64'
    # ネットワーク取得物でリポジトリ内ソースには依存しないので、存在すればスキップする
    # (mtime 比較ではなく実体の有無。remote_server と違いバージョン固定のため)。
    if (Test-Path $artifactPath) {
        Write-Host ("slirp4netns は取得済みです: {0}" -f $artifactPath)
        return
    }
    $artifactDir = Split-Path -Parent $artifactPath
    if (-not (Test-Path $artifactDir)) {
        New-Item -ItemType Directory -Force $artifactDir | Out-Null
    }
    $uri = "https://github.com/rootless-containers/slirp4netns/releases/download/$slirp4netnsVersion/slirp4netns-x86_64"
    Write-Host ("slirp4netns {0} を取得しています: {1}" -f $slirp4netnsVersion, $uri)
    $previousProgress = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest の進捗描画は大容量で遅い
    try {
        Invoke-WebRequest -Uri $uri -OutFile $artifactPath -UseBasicParsing
    } finally {
        $ProgressPreference = $previousProgress
    }
    if (-not (Test-Path $artifactPath) -or (Get-Item $artifactPath).Length -lt 100000) {
        throw "slirp4netns の取得に失敗したか、ファイルが不正です: $artifactPath"
    }
    Write-Host ("slirp4netns: {0} ({1:N1} MB)" -f $artifactPath, ((Get-Item $artifactPath).Length / 1MB))
}

# --- C の既定フォーマッタ用 clang-format の同梱 (DESIGN.md §5.2) -------------
# languages.C / languages.C++ の formatter.external は裸の "clang-format" を呼ぶ。
# crates/util の resolve_command_next_to_current_exe が「zed.exe と同じフォルダ →
# その bin\ → PATH」の順で解決するので、bin\ に置けば PATH 設定なしで動く。
# zed.exe とは独立したファイルなので、ビルドの成否や zed.exe の実行中とは無関係に
# 置き換えられる (実行中の Zed はフォーマットのたびに解決し直す)。
function Copy-ClangFormat {
    $destination = Join-Path $repoRoot 'target\debug\bin\clang-format.exe'

    $source = $null
    if ($ClangFormatPath -and (Test-Path -PathType Leaf $ClangFormatPath)) {
        $source = (Resolve-Path $ClangFormatPath).Path
    } else {
        # VS 2022 の C++ ワークロードに付いてくる LLVM。エディションは実機ごとに違う
        $vsCandidates = @('Enterprise', 'Professional', 'Community', 'BuildTools') |
            ForEach-Object { "C:\Program Files\Microsoft Visual Studio\2022\$_\VC\Tools\Llvm\bin\clang-format.exe" }
        $source = $vsCandidates | Where-Object { Test-Path -PathType Leaf $_ } | Select-Object -First 1
        if (-not $source) {
            throw "clang-format.exe が見つかりません (-ClangFormatPath '$ClangFormatPath'、および VS 2022 の VC\Tools\Llvm\bin)。"
        }
    }

    $destinationDir = Split-Path -Parent $destination
    if (-not (Test-Path $destinationDir)) {
        New-Item -ItemType Directory -Force $destinationDir | Out-Null
    }
    Copy-Item -Path $source -Destination $destination -Force
    Write-Host ("clang-format: {0} <- {1}" -f $destination, $source)
}

# --- ビルド (-j 8 固定。無制限並列は rustc がメモリ枯渇で落ちる) ------------
Write-Host 'cargo build -p zed -j 8 …'
cargo build -p zed -j 8
if ($LASTEXITCODE -ne 0) {
    Write-Error "zed のビルドに失敗しました (exit $LASTEXITCODE)。docs/jp-enhance/BUILD-WINDOWS.md の gotcha を確認してください。"
}

if (-not $SkipCli) {
    # コンソールから `zed <パス>` を使うには cli.exe が zed.exe の隣に必要
    Write-Host 'cargo build -p cli -j 8 …'
    cargo build -p cli -j 8
    if ($LASTEXITCODE -ne 0) {
        Write-Error "cli のビルドに失敗しました (exit $LASTEXITCODE)。"
    }
}

if (-not $SkipRemoteServer) {
    # zed.exe 自体のビルドは成功しているので、ここでの失敗は警告に留める
    try {
        Build-RemoteServerArtifact
    } catch {
        Write-Warning ("remote_server アーティファクトを用意できませんでした: {0}" -f $_.Exception.Message)
        Write-Warning 'SSH リモート接続時にサーバーバイナリを供給できません。-SkipRemoteServer で明示的に省略できます。'
    }
}

if (-not $SkipSlirp4netns) {
    # zed.exe 自体のビルドは成功しているので、ここでの失敗は警告に留める
    try {
        Get-Slirp4netnsBinary
    } catch {
        Write-Warning ("slirp4netns を用意できませんでした: {0}" -f $_.Exception.Message)
        Write-Warning 'リモートサンドボックス (ADR-F18) の netns 隔離が利用できません。-SkipSlirp4netns で明示的に省略できます。'
    }
}

if (-not $SkipClangFormat) {
    # zed.exe 自体のビルドは成功しているので、ここでの失敗は警告に留める
    try {
        Copy-ClangFormat
    } catch {
        Write-Warning ("clang-format を用意できませんでした: {0}" -f $_.Exception.Message)
        Write-Warning 'C ファイルのフォーマットが無効になります (-ClangFormatPath で入手元を指定できます)。'
    }
}

$zedExe = Join-Path $repoRoot 'target\debug\zed.exe'
Write-Host ''
Write-Host ("ビルド完了: {0} ({1:N0} MB)" -f $zedExe, ((Get-Item $zedExe).Length / 1MB))

if ($Run) {
    Write-Host '起動します…'
    Start-Process $zedExe
}
