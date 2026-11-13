# REMAINDER Automated One-Step Installer (Windows)
param([switch]$SkipBuild, [switch]$SkipModel, [switch]$SkipRuntime, [string]$RuntimeSource, [string]$BinarySource)
$ErrorActionPreference = "Stop"

Write-Host "============================================================" -ForegroundColor Cyan
Write-Host "  REMAINDER: Industrial Memory Bunker Installer" -ForegroundColor Cyan
Write-Host "============================================================" -ForegroundColor Cyan

$installDir = "$env:USERPROFILE\.local\bin"
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
}

$sourceExe = if ($BinarySource) { $BinarySource } else { Join-Path $PSScriptRoot "target\release\remainder.exe" }
if (-not $SkipBuild -and -not $BinarySource) {
    Write-Host "[BUILD] Building current source with Cargo..." -ForegroundColor Yellow
    $previousLibraryPath = $env:LIBRARY_PATH
    try {
        $sysroot = & rustc --print sysroot
        if ($LASTEXITCODE -ne 0) { throw 'Rust toolchain not found.' }
        $gnuLibraries = Join-Path $sysroot 'lib\rustlib\x86_64-pc-windows-gnu\lib\self-contained'
        if (Test-Path -LiteralPath $gnuLibraries) { $env:LIBRARY_PATH = "$gnuLibraries;$previousLibraryPath" }
        & cargo build --release --bin remainder --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml')
        if ($LASTEXITCODE -ne 0) { throw 'Build failed. Close running REMAINDER processes if Windows reports a locked executable.' }
    } finally { $env:LIBRARY_PATH = $previousLibraryPath }
}
if (-not (Test-Path -LiteralPath $sourceExe -PathType Leaf)) { throw "Binary missing: $sourceExe" }
& (Join-Path $PSScriptRoot 'scripts\install-native-assets.ps1') -SkipModel:$SkipModel -SkipRuntime:$SkipRuntime -RuntimeSource $RuntimeSource

$destExe = Join-Path $installDir "remainder.exe"
if (Test-Path -LiteralPath $destExe) { Copy-Item -LiteralPath $destExe -Destination "$destExe.previous" -Force }
Copy-Item -Path $sourceExe -Destination $destExe -Force
Write-Host "[OK] Installed remainder.exe to: $destExe" -ForegroundColor Green

# Ensure user's PATH contains ~/.local/bin
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ';') -notcontains $installDir) {
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

