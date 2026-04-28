#!/usr/bin/env bash
# thClaws one-command installer
# Usage:
#   macOS/Linux: bash scripts/install-one.sh
#   Windows:     .\scripts\install-one.ps1

set -euo pipefail

CYAN='\033[0;36m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
GRAY='\033[0;90m'
NC='\033[0m'

echo -e "${CYAN}"
echo "=========================================="
echo "      thClaws One-Command Setup"
echo "=========================================="
echo -e "${NC}"

# Check prerequisites
echo -e "${GREEN}[*] Checking requirements...${NC}"
missing=()
command -v rustc >/dev/null 2>&1 || missing+=("Rust")
command -v node >/dev/null 2>&1 || missing+=("Node.js")
command -v pnpm >/dev/null 2>&1 || missing+=("pnpm")

if [[ ${#missing[@]} -gt 0 ]]; then
    echo -e "${YELLOW}[!] Missing: ${missing[*]}${NC}"
    echo -e "${YELLOW}Install from:${NC}"
    [[ " ${missing[*]} " =~ "Rust" ]] && echo -e "  Rust:   https://rustup.rs"
    [[ " ${missing[*]} " =~ "Node.js" ]] && echo -e "  Node:   https://nodejs.org"
    [[ " ${missing[*]} " =~ "pnpm" ]] && echo -e "  pnpm:   npm install -g pnpm"
    exit 1
fi

# Get repo path
if [[ -d .git ]]; then
    REPO=$(pwd)
else
    echo -e "${YELLOW}[*] Cloning repository...${NC}"
    git clone https://github.com/thClaws/thClaws.git
    REPO="$PWD/thClaws"
fi

cd "$REPO"

# Build frontend
echo -e "${GREEN}[*] Building frontend...${NC}"
cd frontend
pnpm install
pnpm build
cd ..

# Build Rust
echo -e "${GREEN}[*] Building Rust...${NC}"
cargo build --release --features gui --bin thclaws

# Install
INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"
cp "target/release/thclaws" "$INSTALL_DIR/thclaws"
chmod +x "$INSTALL_DIR/thclaws"

# Add to PATH
export PATH="$INSTALL_DIR:$PATH"

# Add to shell rc if needed
path_updated=false
shell_rc=""
for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    if [[ -f "$rc" ]]; then
        shell_rc="$rc"
        break
    fi
done

if [[ -n "$shell_rc" ]]; then
    if ! grep -q "$INSTALL_DIR" "$shell_rc" 2>/dev/null; then
        echo "" >> "$shell_rc"
        echo "# thClaws" >> "$shell_rc"
        echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$shell_rc"
        path_updated=true
    fi
fi

echo -e "${GREEN}"
echo "=========================================="
echo "       Setup Complete!"
echo "=========================================="
echo -e "${NC}"

if [[ "$path_updated" == "true" ]]; then
    echo -e "${YELLOW}[*] Added to PATH. Run now:${NC}"
else
    echo -e "${YELLOW}[*] Run:${NC}"
fi
echo -e "  ${GREEN}thclaws --cli${NC}"
echo -e "${GRAY}  thclaws              # GUI${NC}"
echo ""
echo -e "${GRAY}Or run directly: $INSTALL_DIR/thclaws${NC}"