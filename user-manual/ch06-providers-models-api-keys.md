# Chapter 6 — Providers, models & API keys

thClaws talks to **eleven providers**, auto-detected from the model name.
Switch any time with `/model` or `/provider`.

## Provider overview

| Provider | Model prefix | Auth env var | Notes |
|---|---|---|---|
| Agentic Press | `ap/*` | `AGENTIC_PRESS_LLM_API_KEY` | OpenAI-compatible gateway; many backends under one key |
| Anthropic | `claude-*` | `ANTHROPIC_API_KEY` | Extended thinking, prompt caching (system + tools) |
| Anthropic Agent SDK | `agent/*` | — (uses Claude Code's own auth) | Drives the `claude` CLI under your Claude Pro / Max subscription instead of API billing. ⚠ thClaws's tool registry doesn't cross the subprocess boundary — the model only sees Claude Code's built-in toolset. KMS / MCP / Agent Teams tools are unreachable from this provider; switch to `claude-*` for those. |
| OpenAI | `gpt-*`, `o1-*`, `o3*`, `o4-*` | `OPENAI_API_KEY` | Chat Completions; automatic prompt caching |
| OpenAI Responses | `codex/*` | `OPENAI_API_KEY` | Responses API — newer agentic-native shape |
| OpenAI-Compatible | `oai/*` | `OPENAI_COMPAT_API_KEY` (+ `OPENAI_COMPAT_BASE_URL`) | Generic OAI-compat endpoint — point at any LiteLLM/Portkey/Helicone/vLLM/internal-proxy that speaks `/v1/chat/completions`; `oai/` prefix stripped before forwarding |
| OpenRouter | `openrouter/*` | `OPENROUTER_API_KEY` | Unified gateway to 300+ models across every major LLM vendor |
| Gemini | `gemini-*`, `gemma-*` | `GEMINI_API_KEY` | Gemma served via Google AI Studio |
| Ollama | `ollama/*` | — (local) | NDJSON streaming; no auth |
| Ollama Anthropic | `oa/*` | — (local, v0.14+) | Ollama's Anthropic-compatible `/v1/messages` endpoint |
| DashScope | `qwen-*`, `qwq-*` | `DASHSCOPE_API_KEY` | Alibaba Qwen; automatic caching |

The default on first run is `claude-sonnet-4-6`; change it with
`--model` on the command line or persist in `settings.json`.

## Switching providers

```
❯ /providers
    agentic-press → ap/gemma4-12b
  * anthropic     → claude-sonnet-4-6
    anthropic-agent → agent/claude-sonnet-4-6
    openrouter    → openrouter/anthropic/claude-sonnet-4-6
    ...

❯ /provider openai
provider → openai (model: gpt-4o, saved to .thclaws/settings.json; new session sess-…)

❯ /provider
current provider: openai (model: gpt-4o)
```

Switching always **forks a fresh session** (see [Chapter 7](ch07-sessions.md)) — the old
conversation is saved, a new one starts with the new provider.

## Switching models

`/model` takes the full model id or a short alias:

| Alias | Resolves to |
|---|---|
| `sonnet` | `claude-sonnet-4-6` |
| `opus`   | `claude-opus-4-6` |
| `haiku`  | `claude-haiku-4-5` |
| `flash`  | `gemini-2.0-flash` |

```
❯ /model sonnet
(alias 'sonnet' → 'claude-sonnet-4-6')
model → claude-sonnet-4-6 (saved to .thclaws/settings.json; new session sess-…)

❯ /models
  claude-haiku-4-5
  claude-opus-4-6
  claude-sonnet-4-6
  ...
```

`/model` **validates** the name against `list_models` before committing.
A typo like `/model gemma4-9999` leaves the current model in place and
prints `unknown model '…' — try /models`.

`/models` lists the server's reported catalogue for the current
provider. For Ollama and Agentic Press, IDs come back prefixed (e.g.
`ollama/llama3.2`, `ap/gemma4-26b`) so you can paste them straight
into `/model`.

## Reasoning / "thinking" models

Models in the families below emit a `reasoning_content` field
alongside their normal `content`, and the provider **requires the
prior `reasoning_content` to be sent back** on every subsequent turn —
otherwise the API rejects with HTTP 400 *"The reasoning_content in the
thinking mode must be passed back to the API"*.

thClaws handles this transparently: it captures the reasoning into a
hidden `Thinking` content block on the assistant message, and on the
next request echoes it back **only** for providers that need it.

| Family | Example model id | Provider |
|---|---|---|
| DeepSeek v4 | `deepseek/deepseek-v4-flash`, `deepseek/deepseek-v4-pro` | OpenRouter |
| DeepSeek r1 | `deepseek/deepseek-r1`, `deepseek-r1` | OpenRouter, native |
| OpenAI o-series | `openai/o1-mini`, `openai/o3`, `openai/o4-*` | OpenRouter |

For everything outside those families (gpt-4o, claude-sonnet-4-6,
qwen3.6-plus, etc.) the `Thinking` block is **dropped** during
serialization — no extra input tokens, no risk of the provider
rejecting an unknown field.

If you switch from a thinking model to a non-thinking model
mid-session, the prior reasoning blocks stay in your session JSONL but
don't go on the wire. No token leak.

## For contributors — `make catalogue`

If you build from source and want to refresh the bundled
`model_catalogue.json` (the compile-time baseline that ships with the
binary), the workspace Makefile has a target:

```sh
make catalogue
```

It pulls the model lists from OpenRouter (always, no key required)
plus Anthropic / OpenAI / Gemini if their API keys are set, plus
Ollama if reachable at `localhost:11434`, and merges into the
catalogue **insert-only** (hand-curated rows are never overwritten).
The report shows new IDs added per provider plus counts of unchanged +
skipped (no context_length) entries, then prints `git diff --stat` so
you can review before committing.

## API key hierarchy

Keys are **never stored in `settings.json`**. thClaws looks in four
places, highest priority wins:

| Level | Location | Scope |
|---|---|---|
| Shell export | `~/.zshrc`, CI env, etc. | Every process |
| OS keychain | macOS Keychain / Windows Credential Manager / Linux Secret Service | Every thClaws session on this machine |
| User `.env` | `~/.config/thclaws/.env` | Every thClaws session |
| Project `.env` | `./.env` in the working directory | This project only |

**Recommended**: use the **Settings modal (GUI)** — it saves keys to
the OS keychain, which is materially more secure than any `.env`
path:

| | OS keychain (via Settings modal) | `.env` file |
|---|---|---|
| At-rest encryption | ✓ derived from your login password (Secure Enclave on modern Macs) | ✗ plaintext |
| Access control | ✓ tied to your user account | ✗ any process with filesystem read access |
| Accidental git commit | ✓ impossible (not a file in the repo) | ⚠ easy (people forget `.gitignore`) |
| Leaks via Time Machine / cloud sync / rsync | ✓ no | ⚠ yes — the file goes where your backups go |
| Works headless / in CI | ✗ no Secret Service on most headless Linux | ✓ yes |

So: use the Settings modal on your laptop or workstation; fall back
to `.env` only when you're in an environment that lacks a keychain
(CI runners, minimal Docker images, headless servers).

## Secrets backend chooser {#secrets-backend-chooser}

The **first time you launch thClaws**, right after you pick a working
directory (Chapter 3), a dialog asks you to pick how your secrets
should be stored. This runs before thClaws touches the OS keychain at
all — pick `.env` and no keychain prompt ever fires.

![Where should thClaws store API keys? — OS keychain (recommended) vs .env file](../user-manual-img/ch-04/secrets-backend-chooser.png)

Two choices:

- **OS keychain (recommended)** — macOS Keychain / Windows Credential
  Manager / Linux Secret Service. Encrypted at rest and tied to your
  user account. The first time thClaws reads a key you'll get a
  one-time OS access prompt; click "Always Allow" and it's silent
  after.
- **`.env` file** — plain-text at `~/.config/thclaws/.env`. No
  keychain prompts ever. Works on headless Linux boxes that lack
  Secret Service. Trade-off: anyone with read access to your home
  directory can read the file, so treat it like any other secret.

Your choice is saved to `~/.config/thclaws/secrets.json` and respected
forever after. You can change your mind later: Settings → Provider API
keys → "Change…" link in the modal header re-opens the chooser.

### Single-entry keychain bundle (one prompt per launch)

When you pick the keychain backend, all provider keys live inside **one**
keychain item — service `thclaws`, account `api-keys`, storing a JSON map
`{"anthropic": "sk-ant-…", "openai": "sk-…", …}`. This matters because macOS
Keychain ACLs are per-item: with N separate items you'd get N prompts at every
launch of a rebuilt binary. With one bundle, you see **one** prompt, click
"Always Allow", and subsequent launches of the signed binary are silent.

Migration is automatic — first time thClaws reads the bundle, any legacy
per-provider entries get pulled into the bundle and the bundle is written back.

### Cross-process key visibility

The desktop GUI and the PTY-child REPL are separate OS processes. When you
save a key in Settings, the GUI sets the env var for *itself*, but the already-
running REPL child can't see the GUI process's env changes. To keep both in
sync, every request reads the keychain live if the env var isn't present —
so a key saved in Settings is immediately usable in the Terminal tab's REPL.

### Auto-switch on key save {#auto-switch-on-key-save}

The tricky case: you save an Anthropic key, but `config.model` is still
`gpt-4o` (OpenAI). Without auto-switch you'd still see the red "no API key"
indicator.

thClaws handles this: right after a successful key save, if the currently
configured model's provider has no credentials, the active model rewrites to
the newly-usable provider's default (Anthropic → `claude-sonnet-4-6`, OpenAI →
`gpt-4o`, etc.). The sidebar flips to green within a second, and the next
chat turn just works.

### Diagnostic env vars

| Env var | Effect |
|---|---|
| `THCLAWS_DISABLE_KEYCHAIN=1` | Skip keychain entirely. Use for tests and to diagnose flakiness. |
| `THCLAWS_KEYCHAIN_TRACE=1` | Print purple diagnostic lines every time a keychain call is made. Shows process ID and the "already loaded" flag. |
| `THCLAWS_KEYCHAIN_LOADED=1` | Set automatically by the GUI after first keychain read so the spawned PTY child skips its own walk. You shouldn't need to touch this. |

## The Settings modal (GUI)

Click the gear icon in the bottom status bar. Each provider card shows:

- **API Key** field — pre-filled with `*****` (asterisks sized to the
  stored key's length, capped at 64). Typing anything replaces the
  sentinel; the field flips from plain text to masked. Save is disabled
  until you type a real new value.
- **Base URL** field (Ollama only) — pre-filled with the current
  configured value or the default placeholder. Stored in
  `~/.config/thclaws/endpoints.json`.

DashScope is locked to its default in the Settings UI but can be
pointed at a regional endpoint with the `DASHSCOPE_BASE_URL` env var
if you need it (e.g. the Alibaba Cloud International URL).

Clear a key with the trash icon; the keychain entry is deleted and the
env var unset for the running session.

![thClaws setting LLM Keys](../user-manual-img/ch-05/thClaws-setting-llm-keys.png)

## `.env` files (CI, headless, quick-start)

When the keychain isn't an option — CI runners, headless Linux boxes
without Secret Service, or when you want a key that CLI-only tools
(scripts, `thclaws -p` in pipelines) can read — the classic `.env`
path still works:

```bash
# ~/.config/thclaws/.env
ANTHROPIC_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
OPENROUTER_API_KEY=sk-or-v1-...
GEMINI_API_KEY=AI...
AGENTIC_PRESS_LLM_API_KEY=llm_v1_...
DASHSCOPE_API_KEY=sk-...
OLLAMA_BASE_URL=http://localhost:11434   # defaults to this anyway
OPENAI_COMPAT_BASE_URL=http://localhost:8000/v1   # any OAI-compat gateway
OPENAI_COMPAT_API_KEY=...
```

> ⚠️ **If you use git, add `.env` to `.gitignore` immediately** — before
> you paste any key into it. An `.env` committed to a public (or even
> private shared) repo is the single most common way API keys leak.
> The project-scope `./.env` is especially risky because it lives in
> your repo root; the user-scope `~/.config/thclaws/.env` is outside
> any repo so it's safe there, but still treat it as a secret file.
>
> A one-line fix:
>
> ```bash
> $ echo ".env" >> .gitignore && git add .gitignore
> ```
>
> And if you think you already committed one — rotate the key right
> away at the provider's dashboard. Git history preserves deleted
> files forever; rewriting history is messy and anyone who cloned
> before you noticed already has the key.

## Using Ollama locally

1. Install Ollama: `brew install ollama` (macOS) or see ollama.com.
2. Pull a model: `ollama pull llama3.2`.
3. Tell thClaws: `/model ollama/llama3.2`.

No API key. For a remote Ollama server, set `OLLAMA_BASE_URL`
(Settings modal or env var).

## Using Agentic Press (hosted multi-model)

Agentic Press is a gateway that serves several backends (Gemma 3, GPT
4o-mini, Claude Sonnet, Llama 4, Qwen 3) under one API key. Great for
trying different models without signing up everywhere.

1. Get a key from your Agentic Press dashboard.
2. Paste it into Settings → API Keys (Agentic Press) — or set
   `AGENTIC_PRESS_LLM_API_KEY`.
3. `/model ap/gemma4-26b` (or any listed model).

The `ap/` prefix routes requests through the gateway. `/models` lists
everything the gateway currently serves.

## Using OpenRouter (300+ models via one key)

OpenRouter is a unified gateway to every major LLM vendor (Anthropic,
OpenAI, Google, Meta, Mistral, xAI, DeepSeek, Alibaba, and more). One
API key, 300+ models.

1. Get a key from [openrouter.ai/keys](https://openrouter.ai/keys).
2. Paste it into Settings → API Keys (OpenRouter) — or set
   `OPENROUTER_API_KEY`.
3. Pick a model: `/model openrouter/anthropic/claude-sonnet-4-6` (or
   any of the hundreds listed by `/models`).

Model IDs follow the `openrouter/<vendor>/<model>` shape — copy them
from [openrouter.ai/models](https://openrouter.ai/models) or paste the
exact string you see in `/models` output.

Good for:

- Comparing responses across vendors without signing up everywhere
- Testing a new model without a separate account
- Single billing relationship for hobby / small-team use

Note: OpenRouter adds a small markup on top of each vendor's cost.
For high-volume production use, go direct to the source provider.

## Using a generic OpenAI-compatible endpoint (`oai/*`)

The `OpenAICompat` provider is a single configurable slot for **any
service that speaks OpenAI's `/v1/chat/completions` wire format with
a Bearer token**. Common targets:

- LLM gateways: LiteLLM, Portkey, Helicone, internal corporate
  proxies that consolidate vendor billing and apply org-wide policy.
- Self-hosted inference: vLLM, text-generation-inference, lm-deploy,
  llama.cpp's `server` binary in OpenAI-compat mode, MLX-LM, etc.
- Aggregator services other than OpenRouter that follow the same
  shape but live on a private URL.

Two env vars (or the matching Settings modal card):

```sh
OPENAI_COMPAT_BASE_URL=http://localhost:8000/v1
OPENAI_COMPAT_API_KEY=...
```

Then pick a model:

```
/model oai/<upstream-model-id>
```

The `oai/` prefix is **stripped** before the request reaches the
upstream — pass any model id the gateway accepts. Examples:

- `/model oai/gpt-4o-mini` → wire payload `model: "gpt-4o-mini"`
- `/model oai/meta-llama/Llama-3.1-70B-Instruct` → wire payload
  `model: "meta-llama/Llama-3.1-70B-Instruct"`
- `/model oai/anthropic/claude-sonnet-4-6` → wire payload
  `model: "anthropic/claude-sonnet-4-6"`

This is intentionally separate from `ProviderKind::OpenAI` so real
OpenAI usage (`OPENAI_API_KEY` + `gpt-*` / `o*` models) is
unaffected. Both can coexist — set both env vars and switch with
`/model gpt-4o` (real OpenAI) vs `/model oai/<id>` (your gateway).

The Base URL accepts either form:

- Ending in `/v1` — `/chat/completions` is appended automatically.
- Ending in `/v1/chat/completions` — used as-is.

Authentication is a standard `Authorization: Bearer
$OPENAI_COMPAT_API_KEY` header. Gateways with non-Bearer auth (custom
header names, mTLS, etc.) are out of scope — file an issue or use
the EE Phase 3 org-policy `gateway` route.

If your endpoint also implements `/v1/models`, `/models refresh`
will populate the catalogue automatically. If it doesn't, the
refresh fails silently and chat continues to work.
