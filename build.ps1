# Builds OTIUM and its installer into .\dist
#   .\build.ps1                 release build
#   $env:CARGO_TARGET_DIR=...   to build on another drive
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }

Write-Host "Building OTIUM $version..." -ForegroundColor Cyan
cargo build --release -p otium
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$env:OTIUM_EXE = Join-Path $target 'release\otium.exe'
Write-Host "Building installer..." -ForegroundColor Cyan
cargo build --release -p otium-installer
if ($LASTEXITCODE) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item $env:OTIUM_EXE "dist\OTIUM-$version-portable.exe" -Force
Copy-Item (Join-Path $target 'release\otium-setup.exe') "dist\OTIUM-$version-setup.exe" -Force
Get-ChildItem dist | Format-Table Name, @{n = 'MB'; e = { [math]::Round($_.Length / 1MB, 2) } }
