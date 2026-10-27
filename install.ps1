# DRT, the Diluvium RunTime: start, install, get and help on Windows.
#
#   irm https://diluvium.aloecraft.org/start-drt.ps1 | iex
#   & ([scriptblock]::Create((irm https://diluvium.aloecraft.org/get-drt.ps1))) install -r v0.8.0
#
# The PowerShell twin of install.sh: the same commands, the same stamps, the
# same sources in the same order. Every release ships it eight times, as
# start.ps1, start-drt.ps1, install.ps1, install-drt.ps1, get.ps1,
# get-drt.ps1, help.ps1 and help-drt.ps1, and release.yml stamps two lines
# in each copy, because a script piped to iex cannot see its URL. A piped
# script takes no arguments, so the second line above is how a command
# reaches it.
#
# Windows PowerShell 5.1 and PowerShell 7. Everything runs inside one script
# block so `irm | iex` leaves nothing behind in the caller's session, and a
# failure throws rather than exits, which would close the caller's window.
& {
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# ---------------------------------------------------------------- surface
# Entry points. With no command, MODE decides:
#   start    install the latest stable release
#   install  install the release that shipped this copy
#   get      download and verify that release into the current directory
#   help     print the usage below
function Usage {
@"
usage: $Self [command] [--version VERSION] [--prefix DIR] [--slim]

  (none)            $(ModeSays)
  install [VERSION] install VERSION, or the default above, and use it
  get [VERSION]     download and verify VERSION into the current directory
  latest            as VERSION: the newest stable release
  dev               as VERSION: the newest dev build
  --version, -r, --release [VERSION]
                    with VERSION, the release to act on; alone, print the
                    release this copy came from
  list              stable releases available
  list-all          every release, candidates and dev builds included
  check             what is installed, and which one is in use
  use VERSION       switch to an installed release
  remove [VERSION]  uninstall VERSION, or every release
  help, --help      this

  --prefix DIR      install under DIR\bin and DIR\share\drt
                    (default %LOCALAPPDATA%\Programs\Aloecraft)
  --slim            the slim profile

Env: DRT_VERSION, DRT_PREFIX, DRT_SLIM=1, DRT_MIRROR and DRT_RELEASE_LOG
(file:// URLs work).

Piped from irm, a command goes through a script block:
  & ([scriptblock]::Create((irm https://diluvium.aloecraft.org/start-drt.ps1))) list
"@
}

# Stamped by release.yml. Empty means this copy came from no release.
$StampedVersion = ''
$StampedMode = 'start'

# Configurable values.
$Name = 'drt'
$Repo = 'Aloecraft-org/diluvium-drt'
$Mirror = if ($env:DRT_MIRROR) { $env:DRT_MIRROR } else { 'https://software.aloecraft.org/releases/diluvium-drt' }
$GitHub = "https://github.com/$Repo"
$Raw = "https://raw.githubusercontent.com/$Repo"
# Every published release, dev builds included, one per line: tag, channel
# (stable, rc or dev), date. release.yml rewrites it on its own branch.
$ReleaseLog = if ($env:DRT_RELEASE_LOG) { $env:DRT_RELEASE_LOG } else { "$Raw/release-log/.github/release-log" }
$SelfTest = '--version'
$Prefix = $env:DRT_PREFIX
$Slim = ($env:DRT_SLIM -eq '1')

# Fan-out: the command picks the action.
function Dispatch {
    switch ($Action) {
        'print'    { if ($StampedVersion) { $StampedVersion } else { 'none (a checkout copy, which installs latest)' } }
        'help'     { Usage }
        'install'  { DoInstall }
        'get'      { DoGet }
        'list'     { DoList 'stable' }
        'list-all' { DoList 'all' }
        'check'    { DoCheck }
        'use'      { DoUse }
        'remove'   { DoRemove }
    }
}

# The files a release carries for Windows, as @(installed name, asset,
# older asset name). The first is required; any after it are extras.
function Platform {
    if ($env:OS -ne 'Windows_NT') { Die "this script installs on Windows; on Linux and macOS use install.sh: curl -fsSL https://diluvium.aloecraft.org/start-drt | sh" }
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { }
        'ARM64' { Say "note: there is no arm64 Windows build; installing x86_64, which Windows 11 runs under emulation" }
        default { Die "$($env:PROCESSOR_ARCHITECTURE) has no prebuilt DRT" }
    }
    if ($Slim) { ,@('drt.exe', 'drt_windows_x86_64_slim.exe', 'drt_slim_windows_x86_64.exe') }
    else       { ,@('drt.exe', 'drt_windows_x86_64.exe', 'drt_windows_x86_64.exe') }
}

# ---------------------------------------------------------------- arguments
$Self = "$StampedMode-drt.ps1"
function Say([string]$m) { Write-Host $m }
function Die([string]$m) { throw "${Self}: $m" }

function ModeSays {
    $v = if ($StampedVersion) { $StampedVersion } else { 'latest' }
    switch ($StampedMode) {
        'start'   { 'install the latest stable release' }
        'install' { "install $v, the release this copy came from" }
        'get'     { "get $v, the release this copy came from" }
        'help'    { 'print this help' }
    }
}

$Commands = @('install', 'get', 'list', 'list-all', 'check', 'use', 'remove', 'help')
# A word after --version is its value unless it is an option or a command.
function IsValue([string]$w) { -not ($w.StartsWith('-') -or ($Commands -contains $w)) }

$Action = ''; $Target = ''; $PrintVersion = $false
$argv = @($args)
for ($i = 0; $i -lt $argv.Count; $i++) {
    $a = [string]$argv[$i]
    if ($Commands -contains $a) {
        if ($Action) { Die "one command at a time: $Action, then $a" }
        $Action = $a
    } elseif (@('--version', '-r', '--release') -contains $a) {
        if ($i + 1 -lt $argv.Count -and (IsValue ([string]$argv[$i + 1]))) { $i++; $Target = [string]$argv[$i] }
        else { $PrintVersion = $true }
    } elseif ($a -match '^--(version|release)=(.*)$') {
        $Target = $Matches[2]
    } elseif ($a -eq '--prefix') {
        if ($i + 1 -ge $argv.Count) { Die '--prefix needs a directory' }
        $i++; $Prefix = [string]$argv[$i]
    } elseif ($a -match '^--prefix=(.*)$') {
        $Prefix = $Matches[1]
    } elseif ($a -eq '--slim') {
        $Slim = $true
    } elseif (@('-h', '--help') -contains $a) {
        $Action = 'help'
    } elseif ($a.StartsWith('-')) {
        Die "unknown option: $a (try --help)"
    } else {
        if ($Target) { Die "two versions given: $Target and $a" }
        $Target = $a
    }
}

if (-not $Action) {
    if ($PrintVersion -and -not $Target) { $Action = 'print' }
    elseif (@('get', 'help') -contains $StampedMode) { $Action = $StampedMode }
    else { $Action = 'install' }
}
if (-not $Target) {
    $Target = if ($env:DRT_VERSION) { $env:DRT_VERSION }
              elseif ($StampedMode -ne 'start' -and $StampedVersion) { $StampedVersion }
              else { 'latest' }
    # `remove` with no version means every version, not the default one.
    if ($Action -eq 'remove') { $Target = '' }
}
if ($Action -eq 'use' -and -not $Target) { Die "use needs a version (see '$Self check')" }

# ---------------------------------------------------------------- depth: fetching and verifying
# GitHub refuses the TLS that Windows PowerShell 5.1 offers by default.
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

function Fetch([string]$url, [string]$out) {
    try {
        if ($url.StartsWith('file://')) {
            $path = ([Uri]$url).LocalPath
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $false }
            Copy-Item -LiteralPath $path -Destination $out -Force
        } else {
            Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $out
        }
        return $true
    } catch {
        Remove-Item -LiteralPath $out -Force -ErrorAction SilentlyContinue
        return $false
    }
}

function FetchText([string]$url) {
    try {
        if ($url.StartsWith('file://')) {
            $path = ([Uri]$url).LocalPath
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $null }
            return [IO.File]::ReadAllText($path)
        }
        $r = Invoke-WebRequest -UseBasicParsing -Uri $url
        if ($r.Content -is [byte[]]) { return [Text.Encoding]::UTF8.GetString($r.Content) }
        return [string]$r.Content
    } catch { return $null }
}

function FetchJson([string]$url) {
    $t = FetchText $url
    if (-not $t) { return $null }
    try { return $t | ConvertFrom-Json } catch { return $null }
}

# The release log, without its comments: objects with Tag, Channel, Date.
function ReleaseLog {
    $t = FetchText $ReleaseLog
    if (-not $t) { return @() }
    foreach ($line in ($t -split "`r?`n")) {
        if (-not $line.Trim() -or $line.StartsWith('#')) { continue }
        $f = $line -split "`t"
        [pscustomobject]@{ Tag = $f[0]; Channel = $f[1]; Date = $(if ($f.Count -gt 2) { $f[2] } else { '' }) }
    }
}

# Releases as objects with Tag and Channel, newest first, from a
# releases.json or a changelog.json. An entry not released yet is left out.
function JsonReleases($doc) {
    if (-not $doc -or -not $doc.releases) { return @() }
    foreach ($r in $doc.releases) {
        if (-not $r.tag) { continue }
        if ($r.PSObject.Properties['status'] -and $r.status -eq 'unreleased') { continue }
        $ch = if ($r.tag -match '-dev\.') { 'dev' } elseif ($r.stable) { 'stable' } else { 'rc' }
        [pscustomobject]@{ Tag = $r.tag; Channel = $ch }
    }
}

# GitHub answers /releases/latest with a redirect to the tag.
function GitHubLatest {
    try {
        $req = [Net.HttpWebRequest]::Create("$GitHub/releases/latest")
        $req.AllowAutoRedirect = $false
        $req.Method = 'HEAD'
        $resp = $req.GetResponse()
        $loc = $resp.Headers['Location']
        $resp.Close()
        if ($loc -match '/releases/tag/([^/?#\s]+)') { return $Matches[1] }
    } catch { }
    return $null
}

# Every source that can name the newest stable release: the mirror, then
# what GitHub Releases can actually serve, and the changelog on main last,
# since it can call a release out before its assets are published.
function ResolveLatest {
    $doc = FetchJson "$Mirror/releases.json"
    if ($doc -and $doc.latest) { return [string]$doc.latest }
    $s = @(ReleaseLog | Where-Object { $_.Channel -eq 'stable' }) | Select-Object -First 1
    if ($s) { return $s.Tag }
    $t = GitHubLatest
    if ($t) { return $t }
    $doc = FetchJson "$Raw/main/changelog.json"
    if ($doc -and $doc.latest) { return [string]$doc.latest }
    Die 'cannot tell which release is latest: the mirror, the release log, GitHub and the changelog all failed'
}

function Resolve([string]$v) {
    switch -regex ($v) {
        '^latest$' { return ResolveLatest }
        '^dev$' {
            $d = @(ReleaseLog | Where-Object { $_.Channel -eq 'dev' }) | Select-Object -First 1
            if (-not $d) { Die "cannot find a dev build in $ReleaseLog" }
            return $d.Tag
        }
        '^v' { return $v }
        default { return "v$v" }
    }
}

function Sha256([string]$path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant() }

# A missing sums file, or an asset it does not list, warns rather than
# refuses: that is a release older than the sums-publishing workflow. A
# mismatch always refuses.
function Verify([string]$path, [string]$asset, [string]$sums, [string]$source) {
    $want = $null
    if (Test-Path -LiteralPath $sums) {
        foreach ($line in [IO.File]::ReadAllLines($sums)) {
            $f = $line -split '\s+', 2
            if ($f.Count -eq 2 -and $f[1].TrimStart('*') -eq $asset) { $want = $f[0].ToLowerInvariant() }
        }
    }
    if (-not $want) { Write-Warning "$source/SHA256SUMS.txt does not list $asset; not verified"; return }
    $have = Sha256 $path
    if ($want -ne $have) { Die "checksum mismatch for $asset from $source`n  expected $want`n  got      $have" }
}

# Download the release's files into $dir, verified. Returns the base URL
# that served them. The mirror first, then GitHub Releases: a dev build is
# only ever on GitHub.
function Download([string]$tag, [string]$dir) {
    $want = @(Platform)
    $first = $want[0]
    $source = $null; $got = $null
    foreach ($base in @("$Mirror/$tag", "$GitHub/releases/download/$tag")) {
        foreach ($asset in $first[1..($first.Count - 1)]) {
            if (Fetch "$base/$asset" (Join-Path $dir $first[0])) { $source = $base; $got = $asset; break }
        }
        if ($source) { break }
    }
    if (-not $source) { Die "no $($first[1]) for $tag at $Mirror/$tag or $GitHub/releases/download/$tag (see '$Self list-all')" }
    $sums = Join-Path $dir '.sums'
    if (-not (Fetch "$source/SHA256SUMS.txt" $sums)) { Remove-Item -LiteralPath $sums -ErrorAction SilentlyContinue }
    Verify (Join-Path $dir $first[0]) $got $sums $source
    foreach ($extra in ($want | Select-Object -Skip 1)) {
        $out = Join-Path $dir $extra[0]
        $done = $false
        foreach ($asset in $extra[1..($extra.Count - 1)]) {
            if (Fetch "$source/$asset" $out) { Verify $out $asset $sums $source; $done = $true; break }
        }
        if (-not $done) { Say "note: $tag has no $($extra[1])" }
    }
    Remove-Item -LiteralPath $sums -Force -ErrorAction SilentlyContinue
    return $source
}

# ---------------------------------------------------------------- depth: the install layout
# PREFIX\share\drt\versions\<tag>\drt.exe holds each release. PREFIX\bin\drt.exe
# is a copy of the one in use, since a symlink on Windows needs an
# administrator or developer mode, and PREFIX\share\drt\active names it.
function DefaultPrefix { Join-Path $env:LOCALAPPDATA 'Programs\Aloecraft' }
function VersionsDir([string]$p) { Join-Path $p "share\$Name\versions" }
function ActiveFile([string]$p) { Join-Path $p "share\$Name\active" }
function ActiveIn([string]$p) {
    $f = ActiveFile $p
    if (Test-Path -LiteralPath $f) { return ([IO.File]::ReadAllText($f)).Trim() }
    return ''
}

# For use, remove and check: the prefix that already holds installs.
function InstalledPrefixes {
    if ($Prefix) { return @($Prefix) }
    $d = DefaultPrefix
    if (Test-Path -LiteralPath (VersionsDir $d)) { return @($d) }
    return @()
}

# Replace a file that may be running: Windows will not overwrite a running
# executable, but it will rename one.
function PlaceFile([string]$from, [string]$to) {
    if (Test-Path -LiteralPath $to) {
        $old = "$to.old"
        Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
        Move-Item -LiteralPath $to -Destination $old -Force
        Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
    }
    Copy-Item -LiteralPath $from -Destination $to -Force
}

function UseVersion([string]$p, [string]$tag) {
    $vdir = Join-Path (VersionsDir $p) $tag
    $bin = Join-Path $p 'bin'
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    $prev = ActiveIn $p
    if ($prev -and $prev -ne $tag) {
        # A file the previous release had and this one lacks would leave
        # half of it in use.
        $pdir = Join-Path (VersionsDir $p) $prev
        if (Test-Path -LiteralPath $pdir) {
            foreach ($f in Get-ChildItem -LiteralPath $pdir -File) {
                if (-not (Test-Path -LiteralPath (Join-Path $vdir $f.Name))) {
                    Remove-Item -LiteralPath (Join-Path $bin $f.Name) -Force -ErrorAction SilentlyContinue
                }
            }
        }
    }
    foreach ($f in Get-ChildItem -LiteralPath $vdir -File) { PlaceFile $f.FullName (Join-Path $bin $f.Name) }
    [IO.File]::WriteAllText((ActiveFile $p), $tag)
    AddToPath $bin
}

# The user's PATH, so a new window finds the binary; this one too.
function AddToPath([string]$bin) {
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @(); if ($user) { $parts = $user -split ';' | Where-Object { $_ } }
    if ($parts -notcontains $bin) {
        [Environment]::SetEnvironmentVariable('Path', (@($parts) + $bin) -join ';', 'User')
        Say "added $bin to your PATH; new windows will find $Name"
    }
    if (($env:Path -split ';') -notcontains $bin) { $env:Path = "$env:Path;$bin" }
}

function RemoveFromPath([string]$bin) {
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $user) { return }
    $parts = $user -split ';' | Where-Object { $_ -and $_ -ne $bin }
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

# A native command's stderr, redirected, is an error record in Windows
# PowerShell 5.1, and under Stop it would end the script.
function RunNative([string]$exe, [string]$arg) {
    $ErrorActionPreference = 'Continue'
    $out = & $exe $arg 2>$null
    if ($LASTEXITCODE -ne 0) { return @() }
    return @($out)
}

function RunSelfTest([string]$exe) { RunNative $exe $SelfTest | Select-Object -First 1 }

# ---------------------------------------------------------------- depth: the commands
function DoInstall {
    $tag = Resolve $Target
    if (-not $Prefix) { $Prefix = DefaultPrefix }
    $versions = VersionsDir $Prefix
    New-Item -ItemType Directory -Force -Path $versions | Out-Null
    $tmp = Join-Path $versions ".$tag.$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        $source = Download $tag $tmp
        $exe = Join-Path $tmp (@(Platform)[0][0])
        if (-not (RunSelfTest $exe)) { Die "the downloaded $Name does not run here ($source)" }
        $vdir = Join-Path $versions $tag
        if (Test-Path -LiteralPath $vdir) { Remove-Item -LiteralPath $vdir -Recurse -Force }
        Move-Item -LiteralPath $tmp -Destination $vdir
    } finally {
        if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }
    }
    UseVersion $Prefix $tag
    $bin = Join-Path $Prefix "bin\$Name.exe"
    Say "installed $(RunSelfTest $bin) to $bin"
    Say "  source: $source"
    # What the binary carries, asked of it rather than read off its name.
    RunNative $bin 'buildinfo' | ForEach-Object { Say "  $_" }
}

function DoGet {
    $tag = Resolve $Target
    $tmp = Join-Path (Get-Location) ".$Name-get.$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        $source = Download $tag $tmp
        foreach ($f in Get-ChildItem -LiteralPath $tmp -File) {
            Move-Item -LiteralPath $f.FullName -Destination (Join-Path (Get-Location) $f.Name) -Force
            Say "got .\$($f.Name) ($tag, from $source)"
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function DoList([string]$which) {
    # The release log first: it is the one source that has dev builds. The
    # mirror's index next, then the changelog on main.
    $rows = @(ReleaseLog)
    if (-not $rows) { $rows = @(JsonReleases (FetchJson "$Mirror/releases.json")) }
    if (-not $rows) { $rows = @(JsonReleases (FetchJson "$Raw/main/changelog.json")) }
    if (-not $rows) { Die 'cannot list releases: the release log, the mirror and GitHub all failed' }
    foreach ($r in $rows) {
        if ($which -eq 'stable') { if ($r.Channel -eq 'stable') { $r.Tag } }
        else { '{0,-24} {1}' -f $r.Tag, $r.Channel }
    }
}

function DoCheck {
    $found = $false
    foreach ($p in InstalledPrefixes) {
        $active = ActiveIn $p
        $vd = VersionsDir $p
        if (-not (Test-Path -LiteralPath $vd)) { continue }
        foreach ($v in Get-ChildItem -LiteralPath $vd -Directory | Where-Object { -not $_.Name.StartsWith('.') }) {
            $found = $true
            if ($v.Name -eq $active) { "$($v.Name)  $p  (in use)" } else { "$($v.Name)  $p" }
        }
    }
    if (-not $found) { 'no release installed by this script' }
    $onPath = Get-Command "$Name.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($onPath) {
        $ver = RunSelfTest $onPath.Source
        if (-not $ver) { $ver = 'does not run' }
        "on PATH: $($onPath.Source) ($ver)"
    }
}

function DoUse {
    $tag = Resolve $Target
    foreach ($p in InstalledPrefixes) {
        if (Test-Path -LiteralPath (Join-Path (VersionsDir $p) $tag)) {
            UseVersion $p $tag
            Say "using ${tag}: $(Join-Path $p "bin\$Name.exe")"
            return
        }
    }
    Die "$tag is not installed (see '$Self check'; '$Self install $tag' installs it)"
}

function DoRemove {
    $removed = $false
    foreach ($p in InstalledPrefixes) {
        $active = ActiveIn $p
        $vd = VersionsDir $p
        $bin = Join-Path $p 'bin'
        if ($Target) {
            $tag = Resolve $Target
            $vdir = Join-Path $vd $tag
            if (-not (Test-Path -LiteralPath $vdir)) { continue }
            $files = @(Get-ChildItem -LiteralPath $vdir -File | ForEach-Object { $_.Name })
            Remove-Item -LiteralPath $vdir -Recurse -Force
            Say "removed $tag from $p"
            $removed = $true
            if ($tag -eq $active) {
                foreach ($f in $files) { Remove-Item -LiteralPath (Join-Path $bin $f) -Force -ErrorAction SilentlyContinue }
                Remove-Item -LiteralPath (ActiveFile $p) -Force -ErrorAction SilentlyContinue
                Say "$tag was in use; '$Self use VERSION' picks another"
            }
        } else {
            if ($active) {
                $adir = Join-Path $vd $active
                if (Test-Path -LiteralPath $adir) {
                    foreach ($f in Get-ChildItem -LiteralPath $adir -File) { Remove-Item -LiteralPath (Join-Path $bin $f.Name) -Force -ErrorAction SilentlyContinue }
                }
            }
            Remove-Item -LiteralPath (Join-Path $p "share\$Name") -Recurse -Force
            if ((Test-Path -LiteralPath $bin) -and -not (Get-ChildItem -LiteralPath $bin)) {
                Remove-Item -LiteralPath $bin -Force
                RemoveFromPath $bin
            }
            Say "removed every $Name release from $p"
            $removed = $true
        }
    }
    if (-not $removed) { $t = if ($Target) { $Target } else { 'no release' }; Die "$t is not installed (see '$Self check')" }
}

Dispatch
} @args
