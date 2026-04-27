# Chapter 2 — Installation

thClaws ships as two peer binaries: `thclaws` (desktop GUI + CLI, the
primary entrypoint) and `thclaws-cli` (CLI only, for headless / SSH /
scripted use). Both are built from the same engine — pick whichever
matches how you launch. Download the build that matches your OS and
CPU from:

**https://thclaws.ai/downloads**

Builds are provided for:

| OS | Architectures |
|---|---|
| macOS | Apple Silicon (`arm64`), Intel (`x86_64`) |
| Linux | `x86_64`, `arm64` |
| Windows | `x86_64`, `arm64` |

Pick the right macOS build:

- **Apple Silicon (M1/M2/M3/M4/M5)**: use the `arm64` build. The `x86_64`
  build *does* run via Rosetta 2, but the native `arm64` build is
  faster and uses less memory.
- **Intel Macs**: use the `x86_64` build. The `arm64` build won't run
  on Intel.

## System requirements

thClaws itself is small — the binary is ~20 MB unpacked and uses
~250–400 MB of RAM at runtime, most of it the embedded webview
supplied by the operating system (WKWebView on macOS, WebView2 on
Windows, WebKit2GTK on Linux).

| | Minimum | Recommended |
|---|---|---|
| **OS** | macOS 12+ · Windows 10+ · Linux with webkit2gtk-4.1 (Ubuntu 22.04+, Fedora 38+) | latest stable |
| **CPU** | any 64-bit x86_64 or ARM64 from the past ~10 years | modern multi-core |
| **RAM** | 2 GB free | 8 GB total |
| **Disk** | ~50 MB | SSD |
| **Network** | required for cloud providers (Anthropic / OpenAI / Gemini / OpenRouter / Z.ai / DashScope / Agentic Press); optional if you only use local Ollama or LMStudio | broadband |

If you're using thClaws purely against cloud providers, any laptop
bought in the past few years works comfortably. The heavy spec floor
for **local** model use is the model runtime (Ollama / LMStudio),
not thClaws — see [Optional: Ollama for fully local use](#optional-ollama-for-fully-local-use)
below for those numbers.

> **Prefer to build from source?** thClaws is open source — clone
> [github.com/thClaws/thClaws](https://github.com/thClaws/thClaws)
> and run `cargo build --release --features gui` (Rust 1.85+,
> Node.js 20+, pnpm 9+). The downloads below are the recommended
> install route for most users.

## Install

### macOS

1. Download the matching `thclaws-<version>-<arch>-apple-darwin.tar.gz`.
2. Extract and move the binary onto your `PATH`:

   ```bash
   $ tar -xzf ~/Downloads/thclaws-*-apple-darwin.tar.gz
   $ mkdir -p ~/.local/bin
   $ mv thclaws thclaws-cli ~/.local/bin/
   $ chmod +x ~/.local/bin/thclaws ~/.local/bin/thclaws-cli
   ```

3. If `~/.local/bin` isn't on your `PATH`, add this to `~/.zshrc`
   (or `~/.bashrc`) and restart your terminal:

   ```bash
   export PATH="$HOME/.local/bin:$PATH"
   ```

4. On first launch, macOS Gatekeeper may block the unsigned binary
   ("cannot be opened because the developer cannot be verified"). Clear
   the quarantine flag one-time:

   ```bash
   $ xattr -d com.apple.quarantine ~/.local/bin/thclaws ~/.local/bin/thclaws-cli
   ```

### Linux

1. Download `thclaws-<version>-<arch>-unknown-linux-gnu.tar.gz`.
2. Extract and install:

   ```bash
   $ tar -xzf ~/Downloads/thclaws-*-linux-gnu.tar.gz
   $ mkdir -p ~/.local/bin
   $ install -m 755 thclaws thclaws-cli ~/.local/bin/
   ```

3. Ensure `~/.local/bin` is on your `PATH` (most distros already do
   this via `~/.profile`; if not, add the `export PATH=...` line from
   the macOS section).

### Windows

> **What `%LOCALAPPDATA%` means** — it's a Windows environment variable
> that expands to `C:\Users\<your-username>\AppData\Local`. So
> `%LOCALAPPDATA%\Programs\thclaws` becomes
> `C:\Users\<you>\AppData\Local\Programs\thclaws`. Per-user, no admin
> rights needed (same place GitHub Desktop, VS Code, Cursor install).
> File Explorer's address bar expands it on Enter; in CMD use
> `%LOCALAPPDATA%\...`, in PowerShell use `$env:LOCALAPPDATA\...`.

1. Download `thclaws-<version>-<arch>-pc-windows-msvc.zip`.
2. Extract to `%LOCALAPPDATA%\Programs\thclaws` (create the folder if
   it doesn't exist).
3. Add that folder to your user `PATH`:
   - Start → "Edit environment variables for your account"
   - Path → Edit → New → `%LOCALAPPDATA%\Programs\thclaws`
   - OK → open a new PowerShell / terminal window.

## Optional: Ollama for fully local use

If you want to run entirely against a local model (no cloud API key),
install Ollama alongside thClaws:

```bash
# macOS
brew install ollama

# Linux (script installer)
curl -fsSL https://ollama.com/install.sh | sh

# Windows
# Download the installer from ollama.com/download
```

Start the Ollama daemon (`ollama serve`, or the desktop app) and pull
a model capable enough for agentic work. Small models (Llama 3.2,
Phi-3, etc.) tend to fumble tool-call formatting and multi-step
reasoning; **use Gemma 4 26B or larger**:

```bash
$ ollama pull gemma4:26b         # recommended minimum
$ ollama pull gemma4:31b         # better if your hardware can host it
```

Rough hardware budget:

| Model | RAM / VRAM needed |
|---|---|
| `gemma4:26b` | ~20 GB |
| `gemma4:31b` | ~24 GB |

Apple Silicon with 32 GB unified memory runs 31B comfortably; 16 GB
Macs should stick with 26B. On a dedicated GPU you want that much
VRAM, not system RAM.

Switch thClaws to the model with `/model ollama/gemma4:26b` (or
whichever you pulled). No API key needed. Chapter 6 covers Ollama
options in more detail, including the `oa/*` Anthropic-compatible
prefix that often gives cleaner tool calls on the same local models.

![Ollama](../user-manual-img/ollama/ollama.png)

## Verify the install

```bash
$ thclaws --version                   # the GUI binary
$ thclaws-cli --version               # the CLI-only binary
$ thclaws --cli                       # interactive REPL
$ thclaws -p "say hi in one word"     # headless one-shot (--print also works)
```

All four should print or run without error. If `-p` / `--print` asks
for a key, you haven't configured one yet — see Chapter 6.

## Updating

Re-download the newer archive from
https://thclaws.ai/downloads and repeat the install
step for your platform. Your existing config (API keys, sessions,
plugins, etc.) under `~/.config/thclaws/` (or `%APPDATA%\thclaws\` on
Windows) is preserved — only the binaries are replaced.

## Uninstalling

```bash
# macOS / Linux
$ rm ~/.local/bin/thclaws ~/.local/bin/thclaws-cli

# Windows (PowerShell)
PS> Remove-Item "$env:LOCALAPPDATA\Programs\thclaws" -Recurse
```

Configuration and saved state live under `~/.config/thclaws/` (or
`%APPDATA%\thclaws\` on Windows). Remove those too for a clean
uninstall:

```bash
$ rm -rf ~/.config/thclaws
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `thclaws: command not found` after install | `~/.local/bin` not on `PATH` — add `export PATH="$HOME/.local/bin:$PATH"` to your shell rc |
| macOS "cannot be opened because the developer cannot be verified" | One-time: `xattr -d com.apple.quarantine ~/.local/bin/thclaws ~/.local/bin/thclaws-cli` |
| Linux: `error while loading shared libraries: libssl.so.3` | Install OpenSSL 3 (`sudo apt install libssl3` / `sudo dnf install openssl`) |
| Windows: `thclaws` not recognised in PowerShell | Folder not on PATH — re-check the PATH env var and open a fresh terminal window |
| GUI window doesn't open | Try `thclaws --cli` first — if that works, the GUI webview is missing system deps (WebKit on Linux / WebView2 on Windows) |

## Next

Chapter 3 covers how thClaws scopes itself to your project directory
and the three run modes (GUI, CLI REPL, one-shot `-p` / `--print`).
Chapter 6 is where you configure providers and API keys.
