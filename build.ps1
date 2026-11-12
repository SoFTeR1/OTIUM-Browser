# Builds OTIUM and its installer into .\dist
#   .\build.ps1                 release build (+ signed latest.json when the release key exists)
#   .\build.ps1 -Publish        also create the GitHub release v<version> with all files (needs gh)
#   $env:CARGO_TARGET_DIR=...   to build on another drive
#   $env:OTIUM_SIGN_KEY=...     release key path (default: ~\.otium\release.key)
param([switch]$Publish)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }
$repo = 'SoFTeR1/OTIUM-Browser'
$key = if ($env:OTIUM_SIGN_KEY) { $env:OTIUM_SIGN_KEY } else { Join-Path $HOME '.otium\release.key' }

Write-Host "Building OTIUM $version..." -ForegroundColor Cyan
cargo build --release -p otium
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$env:OTIUM_EXE = Join-Path $target 'release\otium.exe'
Write-Host "Building installer..." -ForegroundColor Cyan
cargo build --release -p otium-installer
if ($LASTEXITCODE) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force dist | Out-Null
$portable = "dist\OTIUM-$version-portable.exe"
$setup = "dist\OTIUM-$version-setup.exe"
Copy-Item $env:OTIUM_EXE $portable -Force
Copy-Item (Join-Path $target 'release\otium-setup.exe') $setup -Force

# Signed manifest for the built-in updater: it downloads the portable exe from this release.
$manifest = 'dist\latest.json'
Remove-Item $manifest -ErrorAction SilentlyContinue
if (Test-Path $key) {
    Write-Host "Signing update manifest..." -ForegroundColor Cyan
    cargo build --release -p otium-signer
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    $url = "https://github.com/$repo/releases/download/v$version/OTIUM-$version-portable.exe"
    & (Join-Path $target 'release\otium-sign.exe') manifest $key $portable $version $url $manifest
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
} else {
    Write-Host "No release key at $key - latest.json not created, installed browsers won't see this build." -ForegroundColor Yellow
}

Get-ChildItem dist | Where-Object { $_.Name -like "*$version*" -or $_.Name -eq 'latest.json' } |
    Format-Table Name, @{n = 'MB'; e = { [math]::Round($_.Length / 1MB, 2) } }

if ($Publish) {
    if (-not (Test-Path $manifest)) { throw 'Refusing to publish without a signed latest.json' }
    Write-Host "Publishing v$version to GitHub..." -ForegroundColor Cyan
    gh release create "v$version" $setup $portable $manifest --repo $repo --title "OTIUM Browser $version" --generate-notes
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
}
