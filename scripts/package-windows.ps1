param([string]$Target = '', [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
. "$PSScriptRoot/use-local-toolchain.ps1"
$release = Get-Content "$PSScriptRoot/release-toolchain.json" -Raw | ConvertFrom-Json
$rust = (& rustc --version)
if ($rust -notmatch "^rustc $([regex]::Escape($release.rust)) ") { throw "Release requires Rust $($release.rust); found $rust" }
$hostTarget = ((& rustc -vV | Select-String '^host: ').ToString()).Substring(6)
if (-not $Target) { $Target = $hostTarget }
if ($Target -notin @('x86_64-pc-windows-gnu','x86_64-pc-windows-msvc')) { throw "Unsupported release target $Target" }
$binary = "target/$Target/release/mygoimage.exe"
if (-not $SkipBuild) {
    & cargo build --locked --release -p mygoimage --target $Target
    if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
} elseif (-not (Test-Path $binary)) {
    if ($Target -ne $hostTarget) { throw "No release binary exists for target $Target; build it first" }
    $binary = 'target/release/mygoimage.exe'
}
python "$PSScriptRoot/package-release.py" --binary $binary --target $Target
if ($LASTEXITCODE -ne 0) { throw 'Packaging failed' }
