# REMAINDER Automated One-Step Installer (Windows)
$ErrorActionPreference = "Stop"

Write-Host "============================================================" -ForegroundColor Cyan
Write-Host "  REMAINDER: Industrial Memory Bunker Installer" -ForegroundColor Cyan
Write-Host "============================================================" -ForegroundColor Cyan

$installDir = "$env:USERPROFILE\.local\bin"
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

$sourceExe = Join-Path $PSScriptRoot "target\release\remainder.exe"
if (-not (Test-Path $sourceExe)) {
    Write-Host "[BUILD] Release binary not found locally. Building with Cargo..." -ForegroundColor Yellow
    $env:LIBRARY_PATH = "C:\Program Files\Rust stable GNU 1.99\lib\rustlib\x86_64-pc-windows-gnu\lib\self-contained"
    cargo build --release --bin remainder
}

$destExe = Join-Path $installDir "remainder.exe"
Copy-Item -Path $sourceExe -Destination $destExe -Force
Write-Host "[OK] Installed remainder.exe to: $destExe" -ForegroundColor Green

# Ensure user's PATH contains ~/.local/bin
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$installDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
    $env:PATH = "$env:PATH;$installDir"
    Write-Host "[OK] Added $installDir to user PATH." -ForegroundColor Green
} else {
    Write-Host "[OK] User PATH already configured." -ForegroundColor Green
}

Write-Host "------------------------------------------------------------" -ForegroundColor Cyan
& $destExe --version
Write-Host "============================================================" -ForegroundColor Cyan
Write-Host "  QUICKSTART:" -ForegroundColor White
Write-Host "  1. Navigate to your project folder:  cd ~/my-project"
Write-Host "  2. Initialize bunker & MCP config:   remainder init ."
Write-Host "  3. Ingest project context:           remainder mine ."
Write-Host "  4. Real-time background sync:        remainder watch ."
Write-Host "============================================================" -ForegroundColor Cyan

