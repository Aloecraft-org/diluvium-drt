# Loads `drt completions powershell` the way README.md's line for
# PowerShell does, and asks PowerShell's own completer, TabExpansion2,
# what a partial verb and a partial flag complete to. CI runs it on
# Windows; it runs the same under pwsh anywhere.
#
#   pwsh -File script/completions-check.ps1 [path/to/drt]   # default: drt on PATH
param([string]$Drt = 'drt')
$ErrorActionPreference = 'Stop'
$Cases = @(
    # words typed            what must be offered
    @('drt bui', 'buildinfo'),
    @('drt run --fi', '--file'),
    @('drt completions po', 'powershell')
)

"completions-check: PowerShell $($PSVersionTable.PSVersion)"
& $Drt completions powershell | Out-String | Invoke-Expression
foreach ($case in $Cases) {
    $typed, $want = $case
    $offered = (TabExpansion2 -inputScript $typed -cursorColumn $typed.Length).CompletionMatches.CompletionText
    if ($offered -notcontains $want) {
        Write-Error "completions-check: '$typed' offered '$($offered -join ' ')', not $want"
    }
    "completions-check: '$typed' offers $want"
}
