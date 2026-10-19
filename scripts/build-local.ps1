param([switch]$Release)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
. "$PSScriptRoot/use-local-toolchain.ps1"
if ($Release) { cargo build --locked --release } else { cargo build --locked }
if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed' }
