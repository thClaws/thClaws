# Setup Guide

Just run:

```powershell
# Windows
.\scripts\install-one.ps1
```

```bash
# macOS/Linux
bash scripts/install-one.sh
```

---

## Troubleshooting

If something goes wrong, see below.

### "Frontend build failed"

**Error:** `pnpm install` or `pnpm build` fails

**Solution:**
```bash
cd frontend
rm -rf node_modules pnpm-lock.yaml
pnpm install
pnpm build
```

---

### "Couldn't find `frontend/dist/index.html`"

**Error:** Rust build fails with this message

**Cause:** Frontend wasn't built before Rust build  
**Solution:** Always build frontend first:
```bash
cd frontend && pnpm build && cd ..
cargo build --release --features gui --bin thclaws
```

---

### "Rust 1.85+ required but I have 1.84"

**Solution:** Update Rust:
```bash
rustup update
```

---

### "pnpm not found"

**Solution:** Install pnpm globally:
```bash
npm install -g pnpm
```

Then verify:
```bash
pnpm --version
```

---

### "cargo: command not found"

**Solution:** Install Rust:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then restart your terminal and verify:
```bash
rustc --version
```

---

### "thclaws: command not found" (after install-one)

**Windows:**
1. Close and reopen PowerShell
2. Or: `$env:Path`to see if `%LOCALAPPDATA%\Programs\thClaws` is listed
3. If not, manually add it via System > Environment Variables

**macOS/Linux:**
1. Run: `source ~/.bashrc` (or `~/.zshrc`)
2. Or restart your terminal
3. Verify: `echo $PATH` includes `~/.local/bin`

---

### Build takes forever

**Normal.** First build (especially in debug mode) is slow:
- Debug build: 5–15 minutes
- Release build: 15–30 minutes (but faster runtime)

**Speed it up:**
```bash
# Use release mode (slower build, but much faster runtime)
cargo build --release --features gui --bin thclaws

# Use sccache (speeds up rebuilds)
cargo install sccache
export RUSTC_WRAPPER=sccache

# Only build CLI, skip GUI
cargo build --release --bin thclaws
```

---

### Out of disk space during build

Rust and Node build artifacts take ~5–10 GB.

**Free up space:**
```bash
cd thClaws
cargo clean     # Removes target/ (~2 GB)
cd frontend && rm -rf node_modules  # ~500 MB
```

---

### Build fails on macOS ARM64

**Error:** Architecture mismatch  
**Solution:**
```bash
# Force native compilation
CARGO_CFG_TARGET_ARCH=arm64 cargo build --release --features gui --bin thclaws
```

---

## Platform-Specific Notes

### Windows

- **PowerShell 5.0+** required (run `$PSVersionTable.PSVersion`)
- Binary is copied to `%LOCALAPPDATA%\Programs\thClaws\thclaws.exe`
- Add to PATH via System Environment Variables (or let install-one.ps1 do it)

### macOS

- **Apple Silicon (M1/M2)** supported natively
- Binary is copied to `~/.local/bin/thclaws`
- May need to allow `thclaws` in **System Preferences > Security & Privacy**

### Linux

- **x86_64** and **ARM64** supported
- Binary is copied to `~/.local/bin/thclaws`
- Ensure `~/.local/bin` is in your `$PATH`

---

## Updating

To update to the latest version:

**If installed from pre-built binary:**
```bash
# Download latest from Releases page
# Extract and replace your binary
```

**If built from source:**
```bash
cd thClaws
git pull origin main
bash scripts/install-one.sh
```

---

## Development Setup

If you want to contribute:

```bash
git clone https://github.com/thClaws/thClaws.git
cd thClaws

# Install build dependencies
bash scripts/install-one.sh

# Development workflow
cargo build                        # Debug build
cargo build --release             # Optimized build
cargo test                         # Run tests
cargo fmt                          # Format code
cargo clippy --features gui -- -D warnings  # Lint

# Watch mode (auto-rebuild on changes)
cargo watch -x build --features gui
```

See [CONTRIBUTING.md](../CONTRIBUTING.md) for details.

---

## Getting Help

- **Issues:** [GitHub Issues](https://github.com/thClaws/thClaws/issues)
- **Docs:** [thclaws.ai](https://thclaws.ai)
- **Manual:** [User manual](../user-manual/index.md)

---

## Next Steps

Once installed, read:

1. **[Quick Start](../README.md#quick-start)** — First commands
2. **[Configuration](../README.md#configuration)** — Settings & API keys
3. **[User Manual](../user-manual/index.md)** — Full guide (24 chapters)
