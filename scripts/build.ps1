# Build helper for thClaws. PowerShell equivalent of scripts/build.sh.
#
# Default: build the frontend (pnpm install + pnpm build) then `cargo build
# --features gui`. The Rust GUI build needs frontend/dist/index.html at compile
# time (gui.rs:61 has an `include_str!`), so skipping the frontend step gives
# a confusing compile error. This script enforces the order.
#
# Usage: scripts/build.ps1 [-Release] [-NoFrontend] [-Check] [-Help]

[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$NoFrontend,
    [switch]$Check,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'

if ($Help) {
    @'
thClaws build helper

  scripts/build.ps1                build frontend + cargo build --features gui (debug)
  scripts/build.ps1 -Release       same, release profile
  scripts/build.ps1 -NoFrontend    skip pnpm steps (assumes frontend/dist exists)
  scripts/build.ps1 -Check         run the verification suite from CONTRIBUTING.md:
                                     cargo fmt --check
                                     cargo clippy --features gui -- -D warnings
                                     cd frontend; pnpm tsc --noEmit
                                     cargo test --features gui
'@ | Write-Host
    exit 0
}

# ── Paths ----------------------------------------------------------------────
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir   = Resolve-Path (Join-Path $ScriptDir '..')
Set-Location $RootDir

# ── Pretty output ────────────────────────────────────────────────────────────
function Step([string]$msg) {
    Write-Host ""
    Write-Host "==> $msg" -ForegroundColor Cyan
}
function Fail([string]$msg) {
    Write-Host "error: $msg" -ForegroundColor Red
    exit 1
}
function Need([string]$exe, [string]$hint) {
    if (-not (Get-Command $exe -ErrorAction SilentlyContinue)) {
        Fail "$exe not found -- $hint"
    }
}

# ── Prereqs ----------------------------------------------------------------──
Need 'cargo' 'install Rust 1.85+ from https://rustup.rs'
if (-not $NoFrontend -or $Check) {
    Need 'pnpm' 'install pnpm 9+ (https://pnpm.io)'
}

# Native exe failures must terminate the script. PowerShell 5.1 doesn't trip
# $ErrorActionPreference on non-zero exit codes from external commands.
function Invoke-Native([scriptblock]$Block) {
    & $Block
    if ($LASTEXITCODE -ne 0) {
        Fail "command exited with code $LASTEXITCODE"
    }
}

# ── Check mode ───────────────────────────────────────────────────────────────
if ($Check) {
    Step 'cargo fmt --check'
    Invoke-Native { cargo fmt --all -- --check }

    Step 'cargo clippy --features gui -- -D warnings'
    Invoke-Native { cargo clippy --features gui -- -D warnings }

    Step 'frontend: pnpm tsc --noEmit'
    Push-Location frontend
    try { Invoke-Native { pnpm tsc --noEmit } }
    finally { Pop-Location }

    Step 'cargo test --features gui'
    Invoke-Native { cargo test --features gui }

    Step 'all checks passed'
    exit 0
}

# ── Build ----------------------------------------------------------------────
if (-not $NoFrontend) {
    Step 'frontend: pnpm install'
    Push-Location frontend
    try { Invoke-Native { pnpm install --frozen-lockfile } }
    finally { Pop-Location }

    Step 'frontend: pnpm build'
    Push-Location frontend
    try { Invoke-Native { pnpm build } }
    finally { Pop-Location }
}

if (-not (Test-Path (Join-Path $RootDir 'frontend/dist/index.html'))) {
    Fail 'frontend/dist/index.html missing -- drop -NoFrontend or build the frontend first'
}

if ($Release) {
    Step 'cargo build --release --features gui'
    Invoke-Native { cargo build --release --features gui }
    $BinDir = Join-Path $RootDir 'target/release'
} else {
    Step 'cargo build --features gui'
    Invoke-Native { cargo build --features gui }
    $BinDir = Join-Path $RootDir 'target/debug'
}

Step 'build complete'
Write-Host "binary: $BinDir" -ForegroundColor DarkGray
