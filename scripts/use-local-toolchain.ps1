$taskRoot = Split-Path $PSScriptRoot -Parent
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) { $env:Path = "$cargoBin;$env:Path" }
$localGcc = Join-Path $taskRoot '.tools\w64devkit\bin'
if (Test-Path $localGcc) {
    $env:Path = "$localGcc;$env:Path"
    $env:CC = Join-Path $localGcc 'gcc.exe'
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = Join-Path $localGcc 'gcc.exe'
    $env:RUSTFLAGS = '-C link-self-contained=yes'
}
