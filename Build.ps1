param([switch]$Check)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$localCargo = Join-Path $PSScriptRoot '.tools\cargo\bin\cargo.exe'
if (Test-Path -LiteralPath $localCargo) {
    $env:RUSTUP_HOME = Join-Path $PSScriptRoot '.tools\rustup'
    $env:CARGO_HOME = Join-Path $PSScriptRoot '.tools\cargo'
    $cargo = $localCargo
} else {
    $cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
    if (-not $cargo) { throw 'Install Rust with the MSVC toolchain from https://rust-lang.org/tools/install/, then run this script again.' }
}
if ($Check) {
    & $cargo fmt -- --check
    if ($LASTEXITCODE -ne 0) { throw 'Formatting check failed.' }
    & $cargo clippy --locked --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Static checks failed.' }
    & $cargo test --locked --all-targets
    if ($LASTEXITCODE -ne 0) { throw 'Tests failed.' }
}
& $cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
Write-Output ('Built ' + (Join-Path $PSScriptRoot 'target\release\rl-uploader.exe'))
& (Join-Path $PSScriptRoot 'Build-Installer.ps1')
