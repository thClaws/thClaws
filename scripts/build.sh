#!/usr/bin/env bash
# Build helper for thClaws. POSIX bash equivalent of scripts/build.ps1.
#
# Default: build the frontend (pnpm install + pnpm build) then `cargo build
# --features gui`. The Rust GUI build needs frontend/dist/index.html at compile
# time (gui.rs:61 has an `include_str!`), so skipping the frontend step gives
# a confusing compile error. This script enforces the order.
#
# Usage: scripts/build.sh [--release] [--no-frontend] [--check] [--help]

set -euo pipefail

# ── Args ----------------------------------------------------------------─────
RELEASE=0
NO_FRONTEND=0
CHECK=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)     RELEASE=1; shift ;;
        --no-frontend) NO_FRONTEND=1; shift ;;
        --check)       CHECK=1; shift ;;
        -h|--help)
            cat <<'EOF'
thClaws build helper

  scripts/build.sh                  build frontend + cargo build --features gui (debug)
  scripts/build.sh --release        same, release profile
  scripts/build.sh --no-frontend    skip pnpm steps (assumes frontend/dist exists)
  scripts/build.sh --check          run the verification suite from CONTRIBUTING.md:
                                      cargo fmt --check
                                      cargo clippy --features gui -- -D warnings
                                      cd frontend && pnpm tsc --noEmit
                                      cargo test --features gui
EOF
            exit 0
            ;;
        *)
            printf 'unknown arg: %s (try --help)\n' "$1" >&2
            exit 2
            ;;
    esac
done

# ── Paths ----------------------------------------------------------------────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

# ── Pretty output ────────────────────────────────────────────────────────────
if [[ -t 1 ]] && command -v tput >/dev/null 2>&1; then
    BOLD="$(tput bold)"; DIM="$(tput dim)"; RED="$(tput setaf 1)"; RESET="$(tput sgr0)"
else
    BOLD=""; DIM=""; RED=""; RESET=""
fi
step() { printf '\n%s==> %s%s\n' "$BOLD" "$1" "$RESET"; }
fail() { printf '%serror:%s %s\n' "$RED" "$RESET" "$1" >&2; exit 1; }

# ── Prereqs ----------------------------------------------------------------──
command -v cargo >/dev/null 2>&1 || fail "cargo not found -- install Rust 1.85+ from https://rustup.rs"
if [[ "$NO_FRONTEND" -eq 0 || "$CHECK" -eq 1 ]]; then
    command -v pnpm >/dev/null 2>&1 || fail "pnpm not found -- install pnpm 9+ (https://pnpm.io)"
fi

# ── Check mode ───────────────────────────────────────────────────────────────
if [[ "$CHECK" -eq 1 ]]; then
    step "cargo fmt --check"
    cargo fmt --all -- --check

    step "cargo clippy --features gui -- -D warnings"
    cargo clippy --features gui -- -D warnings

    step "frontend: pnpm tsc --noEmit"
    (cd frontend && pnpm tsc --noEmit)

    step "cargo test --features gui"
    cargo test --features gui

    step "all checks passed"
    exit 0
fi

# ── Build ----------------------------------------------------------------────
if [[ "$NO_FRONTEND" -eq 0 ]]; then
    step "frontend: pnpm install"
    (cd frontend && pnpm install --frozen-lockfile)

    step "frontend: pnpm build"
    (cd frontend && pnpm build)
fi

if [[ ! -f "$ROOT_DIR/frontend/dist/index.html" ]]; then
    fail "frontend/dist/index.html missing -- drop --no-frontend or build the frontend first"
fi

if [[ "$RELEASE" -eq 1 ]]; then
    step "cargo build --release --features gui"
    cargo build --release --features gui
    BIN="$ROOT_DIR/target/release"
else
    step "cargo build --features gui"
    cargo build --features gui
    BIN="$ROOT_DIR/target/debug"
fi

step "build complete"
printf '%sbinary:%s %s\n' "$DIM" "$RESET" "$BIN"
