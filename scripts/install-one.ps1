#!/usr/bin/env pwsh
# thClaws one-command installer
# Usage:
#   Windows:   .\scripts\install-one.ps1
#   macOS/Lin: bash scripts/install-one.sh

$ErrorActionPreference = 'Stop'

Write-Host @"
==========================================
      thClaws One-Command Setup
==========================================
"@ -ForegroundColor Cyan

# Check prerequisites
Write-Host "`n[*] Checking requirements..." -ForegroundColor Green
$missing = @()
if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) { $missing += "Rust" }
if (-not (Get-Command node -ErrorAction SilentlyContinue)) { $missing += "Node.js" }
if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) { $missing += "pnpm" }

if ($missing) {
    Write-Host "`n[!] Missing: $($missing -join ', ')" -ForegroundColor Red
    Write-Host "`nInstall from:"
    if ($missing -contains "Rust") { Write-Host "  Rust:   https://rustup.rs" }
    if ($missing -contains "Node.js") { Write-Host "  Node:   https://nodejs.org" }
    if ($missing -contains "pnpm") { Write-Host "  pnpm:   npm install -g pnpm" }
    exit 1
}

# Get repo path
$Repo = if (Test-Path ".\.git") {
    Get-Location
} else {
    Write-Host "`n Cloning repository..." -ForegroundColor Yellow
    git clone https://github.com/thClaws/thClaws.git
    if ($LASTEXITCODE -ne 0) { exit 1 }
    ".\thClaws"
}

Set-Location $Repo

# Build
Write-Host "`n[*] Building frontend..." -ForegroundColor Green
Push-Location frontend
pnpm install
if ($LASTEXITCODE -ne 0) { Pop-Location; exit 1 }
pnpm build
Pop-Location
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "`n[*] Building Rust..." -ForegroundColor Green
taskkill /F /IM thclaws.exe 2>$null
cargo build --release --features gui --bin thclaws
if ($LASTEXITCODE -ne 0) { exit 1 }

# Install
$InstallDir = "$env:LOCALAPPDATA\Programs\thClaws"
New-Item $InstallDir -ItemType Directory -Force -ErrorAction SilentlyContinue | Out-Null
Copy-Item "target\release\thclaws.exe" "$InstallDir\" -Force

# Add to PATH if needed
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
$pathUpdated = $false
if (-not $userPath.Contains($InstallDir)) {
    [Environment]::SetEnvironmentVariable("PATH", "$InstallDir;$userPath", "User")
    $env:PATH = "$InstallDir;$env:PATH"
    $pathUpdated = $true
} elseif (-not $env:PATH.Contains($InstallDir)) {
    $env:PATH = "$InstallDir;$env:PATH"
}

Write-Host @"

==========================================
       Setup Complete!
==========================================

"@ -ForegroundColor Green

if ($pathUpdated) {
    Write-Host "Added to PATH. Run now:" -ForegroundColor Yellow
    Write-Host "  thclaws --cli" -ForegroundColor Green
    Write-Host "  thclaws              # GUI" -ForegroundColor Gray
} else {
    Write-Host "Run:" -ForegroundColor Yellow
    Write-Host "  thclaws --cli" -ForegroundColor Green
    Write-Host "  thclaws              # GUI" -ForegroundColor Gray
}

Write-Host "`nOr run directly: $InstallDir\thclaws.exe" -ForegroundColor DarkGray
