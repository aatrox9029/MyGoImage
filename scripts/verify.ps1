$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
. "$PSScriptRoot/use-local-toolchain.ps1"
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { throw 'Format check failed' }
cargo clippy --locked --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { throw 'Clippy failed' }
cargo test --locked --workspace
if ($LASTEXITCODE -ne 0) { throw 'Tests failed' }
