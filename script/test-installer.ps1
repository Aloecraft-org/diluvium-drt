# Tests for install.ps1, offline, on Windows: a fake mirror and release log
# under a temporary directory, reached through DRT_MIRROR and
# DRT_RELEASE_LOG as file:// URLs. The binaries are copies of Windows' own
# curl.exe, which answers --version, each with its tag appended so every
# release has its own bytes.
#
#   pwsh -File script/test-installer.ps1                 under PowerShell 7
#   pwsh -File script/test-installer.ps1 -Shell powershell   under 5.1
#
# CI runs both (.github/workflows/ci.yml, installer-windows).
param([string]$Shell = 'pwsh')
$ErrorActionPreference = 'Stop'

$Script = Join-Path (Split-Path -Parent $PSScriptRoot) 'install.ps1'
$Name = 'drt'
$Assets = @('drt_windows_x86_64.exe')

# ---------------------------------------------------------------- depth: the fixture
$T = Join-Path ([IO.Path]::GetTempPath()) ("installer-test-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
$M = Join-Path $T 'mirror'
New-Item -ItemType Directory -Force -Path $M | Out-Null

function Release([string]$tag) {
    $d = Join-Path $M $tag
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    $sums = @()
    foreach ($a in $Assets) {
        $f = Join-Path $d $a
        Copy-Item "$env:SystemRoot\System32\curl.exe" $f
        [IO.File]::AppendAllText($f, $tag)
        $sums += "$((Get-FileHash -Algorithm SHA256 $f).Hash.ToLowerInvariant())  $a"
    }
    [IO.File]::WriteAllLines((Join-Path $d 'SHA256SUMS.txt'), $sums)
}
Release 'v1.0.0'; Release 'v1.1.0'; Release 'v1.2.0-dev.1'; Release 'v1.0.1'
# v1.0.1's sums name other bytes: the install must refuse it.
foreach ($a in $Assets) { [IO.File]::AppendAllText((Join-Path $M "v1.0.1\$a"), 'tampered') }

@'
{
  "latest": "v1.1.0",
  "releases": [
    { "tag": "v1.1.0", "stable": true },
    { "tag": "v1.0.0", "stable": true }
  ]
}
'@ | Set-Content -Encoding ASCII (Join-Path $M 'releases.json')
"# test log`nv1.2.0-dev.1`tdev`t2026-01-03`nv1.1.0`tstable`t2026-01-02`nv1.1.0-rc.1`trc`t2026-01-02`nv1.0.0`tstable`t2026-01-01`n" |
    Set-Content -Encoding ASCII -NoNewline (Join-Path $T 'release-log')

$env:DRT_MIRROR = ([Uri]$M).AbsoluteUri
$env:DRT_RELEASE_LOG = ([Uri](Join-Path $T 'release-log')).AbsoluteUri
Remove-Item Env:DRT_VERSION, Env:DRT_PREFIX, Env:DRT_SLIM -ErrorAction SilentlyContinue
$P = Join-Path $T 'prefix'

function Stamp([string]$mode, [string]$tag) {
    $out = Join-Path $T "$mode-$tag.ps1"
    (Get-Content -Raw $Script) -replace "(?m)^\`$StampedVersion = .*$", "`$StampedVersion = '$tag'" `
                               -replace "(?m)^\`$StampedMode = .*$", "`$StampedMode = '$mode'" |
        Set-Content -Encoding UTF8 $out
    $out
}

# Runs a copy in its own process, as `irm | iex` would on a user's machine.
function Run([string]$file) {
    $out = & $Shell -NoProfile -ExecutionPolicy Bypass -File $file @args 2>&1 | ForEach-Object { "$_" }
    $script:Code = $LASTEXITCODE
    ($out -join "`n").Trim()
}

# ---------------------------------------------------------------- depth: the cases
$fails = 0
function Ok([string]$n) { Write-Host "ok   $n" }
function Bad([string]$n, [string]$why) { Write-Host "FAIL $n"; if ($why) { Write-Host "  $why" }; $script:fails++ }
function Same([string]$n, $got, $want) { if ("$got" -eq "$want") { Ok $n } else { Bad $n "want: $want`n  got:  $got" } }
function Has([string]$n, $got, $want) { if ("$got".Contains($want)) { Ok $n } else { Bad $n "want it to contain: $want`n  got: $got" } }
function FailsWith([string]$n, $got, $want) { if ($Code -ne 0) { Has $n $got $want } else { Bad "$n (it succeeded)" } }
function InUse { (Get-Content -Raw (Join-Path $P "share\$Name\active")).Trim() }
function BinIs([string]$tag) {
    (Get-FileHash (Join-Path $P "bin\$Name.exe")).Hash -eq (Get-FileHash (Join-Path $P "share\$Name\versions\$tag\$Name.exe")).Hash
}

try {
    Same 'checkout copy --version' (Run $Script --version) 'none (a checkout copy, which installs latest)'
    Same 'stamped --version' (Run (Stamp 'install' 'v1.0.0') --version) 'v1.0.0'
    Same 'stamped -r' (Run (Stamp 'get' 'v1.0.0') -r) 'v1.0.0'
    Has  'help mode prints usage' (Run (Stamp 'help' 'v1.0.0')) 'list-all'
    Has  '--help in any mode' (Run $Script --help) 'remove [VERSION]'
    $sb = [scriptblock]::Create((Get-Content -Raw $Script))
    Has  'piped, through a script block' ((& $sb --help) -join "`n") 'usage:'
    FailsWith 'unknown option refuses' (Run $Script --bogus) 'unknown option'

    Same 'list: stable only, newest first' ((Run $Script list) -replace "`r?`n", ' ') 'v1.1.0 v1.0.0'
    Has  'list-all: dev builds too' (Run $Script list-all) 'v1.2.0-dev.1'

    Run $Script --prefix $P | Out-Null
    Same 'start installs latest' (InUse) 'v1.1.0'
    if (BinIs 'v1.1.0') { Ok 'bin holds the release in use' } else { Bad 'bin holds the release in use' }

    Run (Stamp 'install' 'v1.0.0') --prefix $P | Out-Null
    Same 'install.ps1 installs its own release' (InUse) 'v1.0.0'

    Run (Stamp 'get' 'v1.0.0') install latest --prefix $P | Out-Null
    Same 'get.ps1 install latest behaves as install' (InUse) 'v1.1.0'

    Run $Script --prefix $P -r 1.2.0-dev.1 | Out-Null
    Same '-r without the v' (InUse) 'v1.2.0-dev.1'
    Run $Script --prefix $P use 1.0.0 | Out-Null
    Run $Script --prefix $P install dev | Out-Null
    Same 'dev resolves through the release log' (InUse) 'v1.2.0-dev.1'

    $check = Run $Script --prefix $P check
    Has 'check lists every release' $check 'v1.0.0'
    Has 'check marks the one in use' $check "v1.2.0-dev.1  $P  (in use)"

    Run $Script --prefix $P use v1.0.0 | Out-Null
    if (BinIs 'v1.0.0') { Ok 'use switches' } else { Bad 'use switches' }
    FailsWith 'use refuses what is not installed' (Run $Script --prefix $P use v9.9.9) 'not installed'

    FailsWith 'a checksum mismatch refuses' (Run $Script --prefix $P -r v1.0.1) 'checksum mismatch'
    Same 'a refused install changes nothing' (InUse) 'v1.0.0'

    Run $Script --prefix $P remove v1.0.0 | Out-Null
    if (-not (Test-Path (Join-Path $P "bin\$Name.exe"))) { Ok 'removing the one in use removes its binary' } else { Bad 'removing the one in use removes its binary' }
    Has 'the others stay' (Run $Script --prefix $P check) 'v1.1.0'

    Run $Script --prefix $P remove | Out-Null
    if (-not (Test-Path (Join-Path $P "share\$Name"))) { Ok 'remove with no version removes all' } else { Bad 'remove with no version removes all' }
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $user -or ($user -split ';') -notcontains (Join-Path $P 'bin')) { Ok 'and takes bin off the PATH' } else { Bad 'and takes bin off the PATH' }

    $here = Join-Path $T 'here'
    New-Item -ItemType Directory -Path $here | Out-Null
    Push-Location $here
    try { Run (Stamp 'get' 'v1.0.0') | Out-Null } finally { Pop-Location }
    if ((Get-FileHash (Join-Path $here "$Name.exe")).Hash -eq (Get-FileHash (Join-Path $M "v1.0.0\$($Assets[0])")).Hash) { Ok 'get.ps1 saves its release here' } else { Bad 'get.ps1 saves its release here' }
} finally {
    Remove-Item -Recurse -Force $T -ErrorAction SilentlyContinue
}

if ($fails -eq 0) { Write-Host "PASS ($Shell)" } else { Write-Host "$fails failed ($Shell)"; exit 1 }
