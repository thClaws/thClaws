# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — `OpenAICompat` provider

A first-class slot for generic OpenAI-compatible HTTP endpoints — LLM
gateways like LiteLLM, Portkey, Helicone, internal corporate proxies,
self-hosted inference servers (vLLM, text-generation-inference,
lm-deploy), and any other service that speaks OpenAI's
`/v1/chat/completions` wire format with a Bearer token.

Mirrors the existing `LMStudio` / `DashScope` / `ZAi` /
`AzureAIFoundry` template — a configurable base URL (`OPENAI_COMPAT_BASE_URL`
or Settings UI), Bearer token from `OPENAI_COMPAT_API_KEY`, and a
`oai/<id>` model prefix that is stripped before the request reaches
the upstream. Real OpenAI (`OPENAI_API_KEY` + `gpt-*` / `o*` models)
is unaffected — there is no env-var collision and no slot shadowing.

- `crates/core/src/providers/mod.rs` — new `ProviderKind::OpenAICompat`
  variant with full method coverage (`name`, `default_model`,
  `endpoint_env`, `endpoint_user_configurable`, `default_endpoint`,
  `api_key_env`, `resolve_alias_for_provider`, `detect`, `ALL`).
  Detect prefix is `oai/`, name is `openai-compat`.
- `crates/core/src/repl.rs` — `build_provider()` arm constructs an
  `OpenAIProvider` with `with_base_url(...)` and
  `with_strip_model_prefix("oai/")`.
- `crates/core/src/secrets.rs` — added to the `MANAGED` keychain
  bundle list so the API key is stored alongside other auth-required
  providers (one keychain prompt, not N).
- `crates/core/src/model_catalogue.rs` — `provider_kind_name()`
  mapping so `/models refresh` dedicates a `openai-compat` block.
- `frontend/src/components/SettingsModal.tsx` — `PROVIDER_LABELS`
  entry; the existing generic `endpoint_status` / `endpoint_set` IPC
  handlers automatically render a Base URL + API Key card because
  `endpoint_user_configurable()` is true.
- New test `config::tests::detect_provider_covers_openai_compat`
  plus an extended assertion in `secrets::tests::status_lists_known_providers`.
  502 lib tests pass.

Usage:

```sh
# .env or shell
export OPENAI_COMPAT_BASE_URL=http://localhost:8000/v1
export OPENAI_COMPAT_API_KEY=...

# in REPL or via --model flag
/model oai/<upstream-model-id>
```

The `oai/` prefix is stripped before the wire payload, so an upstream
model named `meta-llama/Llama-3.1-70B-Instruct` is reached via
`/model oai/meta-llama/Llama-3.1-70B-Instruct`.

## [0.6.0] — 2026-04-27

Minor release — Enterprise Edition Phase 4 (OIDC SSO) + admin
deployment UX. Open-core users see zero behavior change; every
feature below is inert unless a verified org policy with
`policies.sso.enabled` is loaded.

### Added — OIDC SSO (Phase 4)

- **Browser-driven OIDC authorization-code + PKCE flow.** Works
  against any standards-compliant IdP — Okta, Azure AD / Entra ID,
  Auth0, Keycloak, Google Workspace, AWS Cognito — selected by
  `policies.sso.issuer_url` in the active org policy. New module
  surface under `crates/core/src/sso/`:
  - `pkce.rs` — RFC 7636 verifier/challenge generator (32-byte
    OS-RNG verifier → SHA-256 → S256 challenge), RFC 7636 Appendix B
    test vector covered.
  - `discovery.rs` — fetches `<issuer>/.well-known/openid-configuration`,
    validates S256 PKCE support, decodes endpoints. One implementation,
    all IdPs.
  - `loopback.rs` — minimal HTTP listener on `127.0.0.1:<random>`
    (~60 lines `std::net`, no extra HTTP-server dep). Reads request
    line, extracts `code`/`state`/`error`, returns a friendly "you can
    close this tab" HTML page, shuts down. 5-minute timeout so a user
    who closes their browser doesn't hang the agent.
  - `storage.rs` — keychain persistence via the existing `secrets`
    module. Cache key is `thclaws-sso-<sha256-of-issuer>` so flipping
    IdPs doesn't pollute new claims with stale ones. Tokens never
    touch disk plaintext.
  - `mod.rs` — public API: `login`, `logout`, `current_session`,
    `current_access_token`, `status`, `decode_id_token_claims`. Token
    exchange via `reqwest`. Background refresh kicked off via
    `tokio::spawn` when within 60s of expiry. CSRF-safe `state` parameter
    refused on mismatch.

- **Slash commands**: `/sso`, `/sso login`, `/sso logout`, `/sso status`.
  Wired in both REPL and GUI dispatch.

- **GUI sidebar Identity section**. Three new IPC handlers
  (`sso_status` / `sso_login` / `sso_logout`) + a React component that
  renders only when the active policy has `sso.enabled`. Shows
  signed-in state with email + token-expiry pill + sign-out link, or
  not-signed-in state with a sign-in button. Open-core deployments
  never see the section at all.

- **Gateway `{{sso_token}}` substitution wired**. The Phase 3 gateway's
  auth-header template now resolves `{{sso_token}}` from the active
  SSO session at request time. Per-user identity flows through to the
  gateway audit log: instead of "device-token-X used claude-sonnet-4-6"
  the audit shows "alice@acme.com used claude-sonnet-4-6". Phase 3
  rendered this placeholder as empty string; v0.6.0 makes it active.

### Added — Policy schema (SsoPolicy fields)

- **`clientSecret`** (inline literal) — for "non-confidential" secrets
  that ship embedded in every binary copy by design (Google's Desktop
  OAuth being the canonical example, with Google's own docs explicitly
  classifying these as not-actually-secret). Recommended for those
  IdPs because it collapses the deploy story to "one signed file =
  one deployment artifact."

- **`clientSecretEnv`** (env var name) — for real confidential
  secrets that should never embed in deployed artifacts. The named env
  var is read at token-exchange time. Deploy via MDM / login script /
  OS keychain alongside the binary, in the same channel as the signed
  policy file.

  Resolution order: `clientSecret` (inline) → `clientSecretEnv` (env
  lookup) → none (PKCE-only public client). Each layer treats blank /
  missing as "not set" so a stray space or a left-over `=""` line in
  `.env` doesn't accidentally authenticate as the empty string.

### Added — Operator workflow (Make targets)

Six new Make targets that drive the EE lifecycle end-to-end:

- `make gen-key` — generates Ed25519 keypair at `thclaws-config/policy.{pub,key}`,
  chmod 600 on Unix, refuses to overwrite without `FORCE=1`.
- `make policy-google` — signed policy template targeting
  `accounts.google.com`, reads `GOOGLE_CLIENT_ID` / optional
  `GOOGLE_CLIENT_SECRET` from `.env`, embeds inline.
- `make policy-okta` — Okta tenant template, reads `OKTA_ISSUER_URL` /
  `OKTA_CLIENT_ID` / optional `OKTA_CLIENT_SECRET`, uses
  `clientSecretEnv` (Okta secrets are real).
- `make policy-azure` — Azure / Entra template, reads `AZURE_TENANT_ID`
  / `AZURE_CLIENT_ID` / optional `AZURE_CLIENT_SECRET`, builds the v2
  issuer URL automatically (`login.microsoftonline.com/<tenant>/v2.0`
  — v1 lacks the OIDC discovery doc).
- `make remove-key` — clears the public key + signed policy from the
  build-pickup path. Leaves the private key alone (admin may want to
  keep signing more policies). Idempotent. Useful for "build a clean
  open-core binary from this same checkout" workflows.
- `make remove-keypair FORCE=1` — destructive wipe of all keypair
  material. Refuses without `FORCE=1` because losing the private key
  means existing signed policies can't be re-signed.

Forward path (open-core → enterprise): `gen-key` → `policy-google` (or
`policy-okta` / `policy-azure`) → `make build`.
Backward path (enterprise → open-core): `remove-key` → `make build`.

### Added — Documentation

- **`docs/enterprise-make.md`** — canonical operator reference for the
  EE lifecycle. Covers prerequisites, target reference, lifecycle
  workflows (initial setup, re-sign, annual rotation, multi-customer
  pipeline, switching IdPs, going back to clean open-core),
  troubleshooting, file layout, and design principles.

- **`ENTERPRISE.md`** updated to reflect Phase 4 shipped: status table
  moved SSO from "Planned for v0.6.0" to "Shipped".

### Caveats

- **Live smoke confirmed against Google Workspace.** Okta and Azure
  templates are unit-tested with synthetic credentials but haven't
  been exercised against a real tenant. Any tenant-specific quirks
  surface in early customer feedback, not in this CHANGELOG.

- **Frontend hardcoded "thClaws" strings** in `App.tsx` /
  `ChatView.tsx` still aren't routed through the branding module —
  same caveat carried over from v0.5.0. Phase 4 covered the GUI
  Identity section but didn't expand the branding-IPC surface.

- **Tool-call audit (WebFetch / WebSearch URLs)** is still not
  gateway-routed. Those are general-purpose web fetches, not LLM
  provider calls, and intentionally bypass the gateway in v0.6.0. An
  admin who wants to gate them would do so at the network firewall
  level. A future sub-policy could add this if customers ask.

## [0.5.0] — 2026-04-27

Minor release. Lands the **Enterprise Edition foundation** (Phases 0–3
of `dev-plan/01-enterprise-edition.md`) — policy infrastructure,
branded builds, plugin/skill/MCP allow-list, and gateway enforcement.

**Open-core users see zero behavior change.** Every feature below is
inert unless an Ed25519-signed organization policy file is present at
`~/.config/thclaws/policy.json` or `/etc/thclaws/policy.json` *and*
verifies against either an embedded public key (enterprise builds) or
one supplied at runtime via env var / conventional file path.

### Added — Enterprise Edition foundation

- **Org policy file format** (Phase 0). New `policy/` module with a
  versioned JSON schema covering four sub-policies (branding, plugins,
  gateway, sso), Ed25519 signature verification using a hand-written
  canonical-JSON serializer (no external `canonical-json` dep), expiry
  checks, and optional `binding.binary_fingerprint` matching to prevent
  lifting a customer's policy onto a non-customer build. Loader searches
  `THCLAWS_POLICY_FILE` → `/etc/thclaws/policy.json` → `~/.config/thclaws/policy.json`.
  Public key sources: compile-time embedded → `THCLAWS_POLICY_PUBLIC_KEY`
  env var → `/etc/thclaws/policy.pub` → `~/.config/thclaws/policy.pub`.
  Open-core release binaries embed no key; enterprise builds bake the
  customer's public key at compile time via `THCLAWS_POLICY_PUBKEY_PATH`.
  Refuses to start on signature failure, expiry, binding mismatch, or
  missing verification key — fail-closed by design.

- **`thclaws-policy-tool` operator CLI** (Phase 0). Subcommands:
  `keygen` (generates Ed25519 keypair, chmods private key 0600 on Unix),
  `sign` (signs a policy JSON file), `verify` (checks signature
  against a public key), `inspect` (pretty-prints policy structure),
  `fingerprint` (computes SHA-256 of a binary for `binding`). Signing
  logic lives **only** in this tool — main runtime has zero signing
  code, so a leaked source tree isn't a key-compromise vector.

- **Branding config** (Phase 1). New `branding` module reads
  `policies.branding` from the active policy with fallback to today's
  defaults. Wired into the REPL banner, version header, `/doctor`
  diagnostics title, GUI window title, and the system prompt
  (`{product}` placeholder substituted at load time so the model
  introduces itself as the org's product name). `{support_email}`
  template substitution available for any prompt that needs it.

- **Plugin/skill/MCP source allow-list** (Phase 2). New
  `policy/allowlist.rs` matcher with host+path glob patterns,
  segment wildcards, host-prefix wildcards (`*.acme.example`), and
  mid-segment globs (`skill-*`). Strips scheme / query / fragment /
  port / `.git` suffix before matching. Wired at:
  - `plugins::install` — rejects URLs not in `allowed_hosts`
  - `skills::install_from_url` — same gate, covers both git and zip
    dispatch paths
  - `skills::enforce_scripts_policy` — rejects skills with non-empty
    `scripts/` dirs when `allow_external_scripts: false`. Bundle path
    rejects scripted skills individually so declarative siblings still
    install.
  - `config::parse_mcp_json` — filters HTTP MCP servers whose URL host
    isn't in `allowed_hosts` when `allow_external_mcp: false`. Logs
    yellow `[mcp] '<name>' skipped: <reason>` to stderr. Stdio MCPs
    pass through (admin's mcp.json content = admin's responsibility).

- **Gateway enforcement** (Phase 3). When `policies.gateway.enabled:
  true`, every cloud-provider call routes through the org's private
  LLM gateway (LiteLLM, Portkey, Helicone, internal proxy). User's
  per-provider API keys are ignored — gateway owns credentials.
  Architecture: `build_provider` returns a single OpenAI-compatible
  client pointing at the gateway URL when active, regardless of which
  `ProviderKind` the user picked. Works because every common gateway
  product speaks OpenAI Chat Completions and routes to upstream
  providers via the `model` field.
  - Auth header template supports `{{env:NAME}}` for env-var-injected
    secrets (keeps gateway tokens out of the auditable signed policy
    file). `{{sso_token}}` placeholder reserved for Phase 4.
  - `read_only_local_models_allowed: true` escape valve lets local
    providers (Ollama, OllamaAnthropic, LMStudio, AgentSdk) bypass
    the gateway and run directly. Off by default (strict enterprise).
  - Validation gate at policy load: refuses to start if
    `gateway.enabled: true` with empty `url` (would otherwise
    fail-open at provider construction). Same check for
    `sso.enabled: true` with empty `issuer_url` / `client_id`.

- **`ENTERPRISE.md`** admin guide added to the public repo. Covers
  the open-core + signed-policy architecture, 10-minute quick-start
  walkthrough, operational concerns (key rotation, expiry, binary
  fingerprint binding, MDM deployment), troubleshooting all four
  startup-refusal modes, and an FAQ.

### Caveats

- **OIDC SSO is not yet implemented.** Phase 4 lands in v0.6.0. Until
  then, the gateway uses static-token / env-var auth via the
  `{{env:NAME}}` template substitution. Works fine for LiteLLM-style
  deployments where the gateway token is the only required credential.
- **Frontend branding strings** (5 hardcoded "thClaws" literals in
  `App.tsx`/`ChatView.tsx`, plus the embedded React-bundled logo
  imports) are NOT yet routed through the branding module. They land
  in a v0.5.x point release once the IPC `branding_get` bridge is
  wired. The Rust-side branding (REPL banner, GUI title, system
  prompt) is fully active in v0.5.0.
- **HTTP-layer fail-closed** for the gateway is currently advisory.
  The provider-replacement approach already eliminates bypass paths
  inside the agent loop. A wrapper `reqwest::Client` for
  defense-in-depth is a planned hardening pass.

## [0.4.2] — 2026-04-26

Small additive release in response to issue [#30](https://github.com/thClaws/thClaws/issues/30)
from Chawasit Tengtrairatana — same reporter who filed the
v0.4.1 Windows bug, with another high-quality writeup that
mapped cleanly to the existing catalogue layering.

### Added

- **User-defined context-window overrides.** A new `modelOverrides`
  block in `settings.json` (project + user, project wins per-key)
  lets the user pin context windows above every catalogue layer.
  Keyed by `provider/model` (e.g. `"anthropic/claude-sonnet-4-6"`).
  Useful for: (a) capping a local Ollama / LMStudio model to fit
  a smaller GPU than the model's native context, (b) per-provider
  variants of the same id (Anthropic vs OpenRouter for the same
  Claude model), (c) brand-new models not yet in the catalogue.
  Override resolution honors aliases in both directions and the
  same `vendor/` prefix-strip rules the catalogue uses.

- **`/models set-context` and `/models unset-context` slash
  commands.** Set: `/models set-context [--project] <provider/model>
  <size>` (size accepts `128000`, `128k`, or `1m`). Unset: `/models
  unset-context [--project] <provider/model>`. Default scope is
  user-global (`~/.config/thclaws/settings.json`); `--project`
  scopes to `.thclaws/settings.json`. Saves preserve every other
  field in the target file (atomic write).

- **`ContextSource` enum.** `effective_context_window_with` now
  returns `(u32, ContextSource)` distinguishing override hits from
  catalogue hits and from fallbacks. `/models` rendering marks
  override rows with a `source: "override"` stamp. Old `(u32,
  bool)` semantics remain available via `ContextSource::is_known()`.

### Policy: trust + warn

Overrides exceeding the catalogue value are accepted (the user
intent always wins) but a yellow warning is printed at save-time
so a typo doesn't silently produce upstream rejections at request
time. No clamp, no validation against the upstream-reported max —
matches the spirit of "user knows their hardware better than we do."

## [0.4.1] — 2026-04-27

Same-day patch release fixing a critical Windows-only bug surfaced
within hours of v0.4.0 shipping.

### Fixed

- **Bash tool unusable on Windows** ([#29](https://github.com/thClaws/thClaws/issues/29),
  Chawasit Tengtrairatana). `/bin/sh` was hardcoded at 4 sites
  (`tools/bash.rs`, `team.rs`, `repl.rs`, `hooks.rs`) — Windows
  doesn't have that path, so spawn returned `os error 3` (path
  not found) and the agent was effectively crippled on Win11.
  Centralized shell resolution into `util::shell_command_{sync,
  async}()`, branching on `cfg!(windows)`. On Windows this is
  `cmd.exe /C <cmd>`; on Unix `/bin/sh -c <cmd>`, unchanged.

### Added

- **`THCLAWS_SHELL` env override.** Power users with `bash` from
  WSL / Git Bash, or who prefer `pwsh`, can set
  `THCLAWS_SHELL="bash -c"` (or `"pwsh -Command"`, etc.). The
  helper splits on whitespace into `(executable, flag)`. Useful on
  Windows where `cmd.exe` doesn't parse bash-syntax commands the
  same as `bash` does.

### Caveats

Bash-syntax commands the agent emits (`find . -name '*.rs'`,
single-quoted args, complex pipelines) may not parse identically
under `cmd.exe`. Set `THCLAWS_SHELL="bash -c"` if you have Git Bash
or WSL `bash` on `PATH` for closer-to-Unix semantics on Windows.

## [0.4.0] — 2026-04-27

Minor release. Provider expansion + agent-loop UX polish + a class
of bugs around credential detection. Substantial accumulated work
from same-day batch PR processing across 7 community contributors.

### Added — Providers (4 new)

- **Z.ai (GLM Coding Plan).** OpenAI-compatible upstream at
  `https://api.z.ai/api/coding/paas/v4`. Routes via `zai/<id>`
  prefix, default `zai/glm-4.6`. API key in `ZAI_API_KEY`. Power
  users on the BigModel SKU can override via `ZAI_BASE_URL`.
  Closes [#14](https://github.com/thClaws/thClaws/issues/14).
- **LMStudio.** Local OpenAI-compatible runtime at `/v1`, default
  `http://localhost:1234/v1`. No auth. User-configurable base URL
  via Settings (mirrors the Ollama UX); env override
  `LMSTUDIO_BASE_URL`.
- **Azure AI Foundry** ([#21](https://github.com/thClaws/thClaws/pull/21),
  Parinya-chab / joparin). Anthropic-Claude-on-Azure via
  `{resource}/anthropic/v1/messages` with `x-api-key` auth. Reuses
  `AnthropicProvider` with a custom base URL — no duplicate stream
  code. Default model placeholder `azure/<deployment>` (Azure
  deployments are user-named); set via
  `/model azure/<your-deployment>` once `AZURE_AI_FOUNDRY_ENDPOINT`
  + `AZURE_AI_FOUNDRY_API_KEY` are configured. Forward-looking
  hooks added to `OpenAIProvider` (`with_api_key_header`,
  `with_list_models_url`) for a future Azure OpenAI provider.
- **Ollama Cloud** ([#28](https://github.com/thClaws/thClaws/pull/28),
  Av0cadoo). Hits `https://ollama.com/api/chat` with Bearer auth;
  reuses local Ollama's NDJSON parser. Round-trips the cloud-
  specific `thinking` field as a sibling on assistant messages
  (DeepSeek V4, Kimi K2.5, GLM-5, etc. emit reasoning content
  separately from the visible answer). 38 cloud-only models
  auto-discovered via the new catalogue-seed probe — including
  `deepseek-v4-flash`, `kimi-k2.5/2.6`, `glm-5/5.1`,
  `qwen3-coder-next`, `mistral-large-3:675b`, `gpt-oss:20b/120b`.
  Closes [#17](https://github.com/thClaws/thClaws/issues/17).

### Added — Agent-loop UX

- **AskUserQuestion GUI bridge** ([#16](https://github.com/thClaws/thClaws/pull/16),
  Kinzen-dev). The agent's `AskUser` tool used to fall through to
  invisible CLI stdin in the GUI — chat hung indefinitely. The
  question now appears as a chat-composer reply prompt; user
  reply routes back through a `oneshot` to the awaiting tool call.
  Falls back to CLI readline when no GUI is registered.
- **macOS Cmd+Q / Cmd+W shutdown shortcuts** ([#16](https://github.com/thClaws/thClaws/pull/16)).
  Two-layer coverage (frontend keydown listener + tao native
  KeyboardInput) so Cmd+Q reaches the SaveAndQuit save path even
  in fullscreen / focus-edge cases.
- **Post-key-entry model picker** ([#13](https://github.com/thClaws/thClaws/issues/13)).
  After successfully saving an API key in Settings, if the
  provider has a non-trivial catalogue (≥3 models, skipping
  runtime-loaded backends), a searchable modal opens so the user
  can pick a default model.
- **`/model` interactive picker on no-args** ([#25](https://github.com/thClaws/thClaws/issues/25),
  tkvision). Typing `/model` with no arguments now opens the
  same picker modal in addition to printing the current model.
  Reuses the post-key picker's UX. CLI-side TUI picker is a future
  follow-up.
- **Slash-command popup** ([#20](https://github.com/thClaws/thClaws/pull/20),
  siharat-th). Typing `/` in chat or terminal opens an
  autocomplete menu — built-in commands grouped by category
  (Session / Model / Context / Extensions / Team / System), plus
  user `.claude/commands/` and installed skills. Arrow keys
  navigate, Tab/Enter accept, Esc cancels. Smart Enter: only
  swallows Enter while composing the command name, falls through
  to submit once arguments are being typed.
- **Terminal caret-aware editing** ([#22](https://github.com/thClaws/thClaws/pull/22),
  siharat-th). Left/Right arrow keys, Home/End, Ctrl-A/Ctrl-E
  navigate the line buffer instead of echoing escape codes.
  Backspace and printable-char insertion are caret-aware: the
  fast `term.write(ch)` / `\b \b` path stays at end-of-line;
  mid-line edits redraw so the tail shifts correctly.

### Added — Catalogue

- **`agent/claude-opus-4-7-1m`** in the agent-sdk catalogue
  ([#26](https://github.com/thClaws/thClaws/issues/26), tkvision).
  Max-subscription users on the `agent/*` provider can now
  explicitly select the 1M-context Opus variant.
- **Ollama Cloud auto-discovery** in `catalogue-seed`. Probes
  `https://ollama.com/v1/models` when `OLLAMA_CLOUD_API_KEY` is
  set; refreshes 38 cloud rows every run.
- **`load_dotenv_walking_up()`** in `catalogue-seed` — walks up
  from cwd to find a workspace-root `.env`, so the operator tool
  picks up API keys regardless of which directory cargo is invoked
  from.

### Changed

- **Default Gemini model** `gemini-2.0-flash` → `gemini-2.5-flash`
  ([#27](https://github.com/thClaws/thClaws/pull/27), gokusenz).
  Google's deprecation page lists 2.0-flash as deprecated with
  shutdown 2026-06-01. Existing user configs that explicitly pin
  2.0-flash still work.
- **Read tool** now errors out clearly when bytes don't match any
  supported image format (PNG/JPEG/WebP/GIF) instead of guessing
  the MIME from the extension. Real images sniff fine; only the
  wrong-extension/corrupted unhappy path changes.

### Fixed

- **Empty `ANTHROPIC_API_KEY=""` (or any provider key) was treated
  as configured.** `std::env::var(...).is_ok()` returns true for
  an exported-but-empty value, so a stale shell rc / VS Code env
  injection blocked `auto_fallback_model` from switching when the
  user added a Gemini/Z.ai/etc. key. Both `kind_has_credentials`
  and `api_key_from_env` now require non-empty values; empty env
  falls through to the keychain. Includes a regression test
  `empty_env_var_treated_as_unset`.
- **`/exit` / `/quit` / `/q` slash commands** route through
  the backend `app_close` save path
  ([#16](https://github.com/thClaws/thClaws/pull/16)) instead of
  frontend-only `window.close()` after a 200 ms timeout.
- **Tool-bubble finalizer** searches backwards for the most recent
  unfinished tool bubble — handles text events arriving between
  `tool_use` and `tool_done`
  ([#16](https://github.com/thClaws/thClaws/pull/16)).
- **Frontend security hardening** from a same-day audit pass:
  10 MB cap on pasted/dropped images with inline error banner
  (was: silent drop, multi-MB paste froze the UI during base64
  encoding); 1 MB cap on terminal clipboard paste; explanatory
  threat-model comment on the `ReactMarkdown` call site;
  `ansiToHtml` documented invariant block.
- **Backend security hardening:** IPC `chat_user_message`
  attachment array bounded at `MAX_ATTACHMENTS_PER_MESSAGE = 10`
  + 67 MB total b64.

### Infrastructure

- **Branch protection ruleset on `main`** — block force-push +
  deletion (non-admin), require PR before merging, require status
  checks (cargo fmt + clippy + test (ubuntu-latest) + audit) to
  pass. Admin bypass for sync-from-private-workspace flow and
  emergency corrections.
- **Private Vulnerability Reporting (PVR)** enabled. SECURITY.md
  refreshed: PVR primary, email alternate, supported versions
  bumped 0.2.x → 0.3.x → 0.4.x.
- **CodeQL default setup** for JavaScript/TypeScript + Actions.
- **`cargo-audit` workflow** runs on PRs touching `Cargo.lock` +
  weekly cron.
- **Node 24 actions runtime opt-in** via
  `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` ahead of GitHub's
  2026-06-02 forced switch.
- **`ci.yml` permissions block** — `contents: read, actions: read`
  at top level (was inheriting GITHUB_TOKEN's default write
  scope; closes 4 CodeQL alerts).

### Acknowledged but deferred

- Copy-button-on-chat-bubble surface scope decision (toast / scope
  restriction / pattern-redaction) — captured in the audit reports
  under `dev-log/103-security-audit-frontend.md`.
- IPC message types still stringly-typed; discriminated-union
  refactor queued.
- Transitive `glib` 0.18.5 / gtk-rs 0.18.x unmaintained warnings
  remain pending the upstream `wry`/`webkit2gtk` GTK4 migration.
- CLI TUI picker for `/model` no-args
  ([#25](https://github.com/thClaws/thClaws/issues/25)) — GUI
  side ships in this release; CLI is future work.
- GitHub Copilot provider
  ([#24](https://github.com/thClaws/thClaws/issues/24)) — needs
  GitHub OAuth web flow; queued for a future minor.
- `output.log` should record tool-call argument detail
  ([#23](https://github.com/thClaws/thClaws/issues/23)).

## [0.3.5] — 2026-04-26

Same-day feature/fix follow-up to v0.3.4: two new providers, the
post-key-entry model picker, plus a real bug fix for users whose
shell rc / VS Code env injects a blank `ANTHROPIC_API_KEY`.

### Added

- **Z.ai (GLM Coding Plan) provider.** OpenAI-compatible upstream
  at `https://api.z.ai/api/coding/paas/v4`. Models route via
  `zai/<id>` prefix (default `zai/glm-4.6`). API key in
  `ZAI_API_KEY`. Power users on the BigModel SKU can override the
  endpoint via `ZAI_BASE_URL`. Closes [#14](https://github.com/thClaws/thClaws/issues/14).
- **LMStudio provider.** Local-runtime, OpenAI-compatible at `/v1`.
  No auth. User-configurable base URL via Settings (default
  `http://localhost:1234/v1`); env override `LMSTUDIO_BASE_URL`.
  Mirrors the Ollama UX so changing port doesn't require a
  settings.json edit.
- **Post-key-entry model picker** ([#13](https://github.com/thClaws/thClaws/issues/13)).
  After successfully saving an API key in Settings, if the
  provider has a non-trivial catalogue (≥3 models, skipping
  runtime-loaded backends like Ollama/LMStudio), a searchable
  modal opens so the user can pick a default model directly —
  instead of landing on whatever `auto_fallback_model` chose.
  Skip / Esc / click-outside leaves the auto-pick in place.
- **AskUserQuestion GUI bridge** ([#16](https://github.com/thClaws/thClaws/pull/16),
  Kinzen-dev). The agent's `AskUser` tool used to fall through to
  invisible CLI stdin in the GUI — chat hung indefinitely. The
  question now appears as a chat-composer reply prompt; user
  reply routes back through a `oneshot` to the awaiting tool call.
  Falls back to CLI readline when no GUI is registered.
- **macOS Cmd+Q / Cmd+W shutdown shortcuts** ([#16](https://github.com/thClaws/thClaws/pull/16)).
  Two-layer coverage (frontend keydown listener + tao native
  KeyboardInput) so Cmd+Q reaches the SaveAndQuit save path even
  in fullscreen / focus-edge cases.

### Fixed

- **Empty `ANTHROPIC_API_KEY=""` (or any provider key) was treated
  as configured.** `std::env::var(...).is_ok()` returns true for an
  exported-but-empty value, so a stale shell rc / VS Code env
  injection blocked `auto_fallback_model` from switching when the
  user added a Gemini/Z.ai/etc. key. Both `kind_has_credentials`
  and `api_key_from_env` now require non-empty values; empty env
  falls through to the keychain. Includes a regression test
  (`empty_env_var_treated_as_unset`).
- **`catalogue-seed` reads workspace-root `.env`.** When invoked
  via `cargo run --bin catalogue-seed` from a nested crate dir,
  the binary now walks up from cwd to find the workspace's `.env`
  and load API keys from it. Added
  `dotenv::load_dotenv_walking_up()`.
- **Tool-bubble finalizer searches backwards for unfinished tools**
  ([#16](https://github.com/thClaws/thClaws/pull/16)). Old code
  assumed `messages[last]` was the matching tool bubble; failed
  when text or other events arrived between `tool_use` and
  `tool_done`.
- **`/exit` / `/quit` / `/q` slash commands** now route through
  the backend `app_close` save path ([#16](https://github.com/thClaws/thClaws/pull/16))
  instead of frontend-only `window.close()` after a 200 ms timeout.

### Internal

- New `model_set` IPC handler — frontend-driven model change path,
  used by the new picker; mirrors what `/model` does in the agent
  loop. Available for any future picker UI.
- Dotenv `load_dotenv_walking_up(start)` helper exposed for
  operator-tool scenarios.

## [0.3.4] — 2026-04-26

Same-day hardening patch following an internal security audit of v0.3.3.
No new features; all changes are defensive limits and clearer errors on
the image-attachment and terminal-paste paths.

### Added

- **Inline error feedback on image attachment.** Pasting or dropping an
  unsupported image type or an image larger than 10 MB now shows a
  short auto-clearing banner ("Image too large: 17.3 MB (max 10 MB)")
  instead of silently dropping. Same path covers
  `image/svg+xml`/etc. → "Unsupported image type".

### Changed

- **Read tool errors cleanly on wrong-extension image files.** Files
  like `screenshot.png` containing non-PNG bytes used to slip through
  with a guessed MIME and get rejected by the provider with an opaque
  400. They now fail at Read with a pointed error message
  ("bytes don't match any supported image format despite extension
  claiming image/png — file may be corrupted, encrypted, or saved
  with the wrong extension"). Real images with these extensions are
  unaffected.

### Fixed (security hardening)

- **ChatView image paste/drop:** 10 MB per-attachment cap. Above the
  cap, the image is rejected with a visible error rather than ballooning
  the IPC payload and freezing the UI during base64 encoding.
- **TerminalView clipboard paste:** 1 MB cap. Multi-MB pastes used to
  freeze the main thread during synchronous `atob()` + `TextDecoder`;
  oversized pastes are now dropped with a console warning.
- **Backend IPC `chat_user_message` attachment array:**
  `MAX_ATTACHMENTS_PER_MESSAGE = 10` and a 67 MB combined-base64 cap.
  Defense-in-depth against a malicious or buggy frontend bypassing
  the per-image cap; worst-case payload now bounded at ~50 MB raw
  per message rather than unbounded.
- **`TeamView.tsx` `ansiToHtml`:** documented the escape-first
  invariant in a JSDoc block. The function's output is consumed via
  `dangerouslySetInnerHTML`; preserving HTML-escape-before-tag-build
  ordering is what keeps it safe. Block lists three changes to NOT
  make.
- **Markdown rendering threat-model comment** added at the
  `ReactMarkdown` call site documenting that `msg.content` is
  untrusted model output and the configured plugin chain
  (`remark-gfm`, `rehype-highlight`) is intentionally the safe stack
  — no `allowDangerousHtml`, no `rehype-raw`.

### CI / Infrastructure

- **Workflow least-privilege.** `ci.yml` now declares an explicit
  top-level `permissions: contents: read, actions: read`, instead of
  inheriting the GITHUB_TOKEN's default write scope. Closes 4
  CodeQL alerts (`actions/missing-workflow-permissions`).
- **CodeQL Rust scan** actually runs now: added `libdbus-1-dev` +
  `pkg-config` install before `cargo build`. The keychain crate's
  transitive `libdbus-sys` was failing pkg_config detection, breaking
  every prior CodeQL Rust run before extraction even started.
- **Node 24 actions runtime opt-in** via
  `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` on both `release.yml`
  and `ci.yml`. Surfaces any action-runtime breakage on our schedule
  rather than at GitHub's 2026-06-02 forced cutover.

### Known issues — acknowledged but deferred

- Copy-button surface on chat bubbles (system/tool/assistant) doesn't
  warn or filter when copying messages that may contain previously-
  pasted secrets. Needs a design choice (toast confirmation vs.
  scope restriction vs. pattern-based redaction); deferred to v0.3.5.
- IPC message types are still stringly-typed; discriminated-union
  refactor queued for a future maintenance pass.
- Transitive `glib` 0.18.5 / gtk-rs 0.18.x unmaintained warnings
  (12 RustSec entries) remain pending the upstream `wry`/`webkit2gtk`
  GTK4 migration.

## [0.3.3] — 2026-04-26

Feature release rolling up image attachment across providers, chat UI
polish, and a community-PR sweep that ran `pnpm lint` to clean. Plus
a transitive postcss XSS patch and a docs-prerequisite correction.

### Added

- **Image attachment across providers.** The Read tool now returns
  inline images for vision-capable models (PNG/JPG/GIF/WebP). Wire
  shaping is per-provider:
  - **Anthropic** — native via serde, zero provider code.
  - **OpenAI** — synthetic user message with `image_url` blocks
    referencing the originating `tool_call_id` (their tool-role
    messages can't carry images).
  - **Gemini** — `inlineData` parts as siblings to `functionResponse`
    in the same content.
  - **Ollama / OpenAI Responses** — text-only flatten on wire (no
    pixels to model).
- **ChatView attachments.** Paste and drag-drop image files into the
  chat composer; thumbnails preview before send.

### Changed

- **Chat rendering.** Assistant turns render as markdown
  (headings/lists/code/tables) instead of raw text. Tool output
  collapses to compact one-line indicators by default, with errors
  always shown in full.
- **Tool result handling on history restore.** `tool_result` blocks
  are dropped on session reload; `tool_use` rendering is unified
  across the streaming and reload paths.

### Fixed

- **postcss 8.5.9 → 8.5.10** ([GHSA-qx2v-qp2m-jg93](https://github.com/advisories/GHSA-qx2v-qp2m-jg93)).
  Transitive frontend dep; thClaws ships pre-compiled Tailwind so
  runtime exposure was minimal but Dependabot was flagging.
- **Documented Rust prerequisite: 1.78 → 1.85** in user-manual.
  The `home` crate v0.5.12 (transitive) needs edition 2024, so the
  effective MSRV moved to 1.85. README + CONTRIBUTING were already
  updated in [#3](https://github.com/thClaws/thClaws/pull/3); this
  catches the user-manual files that PR missed.
- **Read tool: image format sniffing from magic bytes** instead of
  trusting file extensions (which lie often enough — `.jpg` files
  that are actually PNGs, etc.).
- **OpenAI batched tool messages.** Emit batched tool messages
  back-to-back with a single combined image follow-up, instead of
  interleaving.
- **Sidebar.tsx unreachable branch.** Duplicate `sessions_list`
  `else if` removed (#4).
- **Frontend lint sweep** by [@parintorns](https://github.com/parintorns)
  in #4, #6, #7, #8, #9, #10 — `react-hooks/exhaustive-deps`,
  `react-refresh/only-export-components`, `no-empty`, type safety
  in IPC bridge and TeamView. `pnpm lint` is now clean.
- **`.gitignore`: `.thclaws/sessions/` → `.thclaws/`.** Was leaking
  `team/`, `settings.json`, and similar runtime files into
  `git status` (#6).

### Infrastructure

- **Workspace `Cargo.toml` at repo root** by
  [@bombman](https://github.com/bombman) (#2). `cargo build` now
  works from the repo root as the README documents; build output
  is at `target/release/` instead of `crates/core/target/release/`.

## [0.3.2] — 2026-04-25

Patch release fixing two GUI startup-recovery bugs surfaced in the
hours after v0.3.1 shipped. Both reach the user before they've typed
their first prompt, so this release is recommended for everyone on
v0.3.1 — particularly Linux users, who can't launch v0.3.1 at all.

### Fixed

- **Linux GUI startup panic.** v0.3.1 panicked at startup on every
  Linux build with `webview build: UnsupportedWindowHandle`
  (reported on Ubuntu 22.04). `wry` can't construct a WebKit2GTK
  webview from a raw window handle the way it does on macOS / Windows
  — WebKit2GTK is a GTK widget that has to be packed into a GTK
  container. Fixed by switching to `wry`'s Linux-only
  `build_gtk(window.default_vbox().unwrap())` behind
  `#[cfg(target_os = "linux")]`. The cross-platform path is preserved
  for macOS / Windows. (commits 6171815 by @Phruetthiphong + 729538b)
- **First-time API key setup required an app restart.** Pasting a
  provider key in Settings on a fresh install would update the sidebar
  to show the new provider, but the running agent kept holding the
  stale (or no-op) provider it was constructed with at startup —
  resulting in "sidebar shows openai but error mentions anthropic"
  on the first send. Two fixes:
  - The shared-session worker no longer exits on missing-key startup;
    it installs a `NoopProvider` placeholder and stays alive so a
    later config reload can swap in a real provider.
  - Added `ShellInput::ReloadConfig`. The `api_key_set` and
    `api_key_clear` IPC handlers now send it after their save, so the
    worker reloads `AppConfig`, rebuilds the agent's provider in
    place, and broadcasts the sidebar update — all without an app
    restart. (commit 27d163d)

## [0.3.1] — 2026-04-25

Re-release of v0.3.0 — the v0.3.0 tag's release workflow failed
(missing `banner.txt` broke the frontend build). Tag re-cut against
the fix.

### Fixed (v0.3.1 vs v0.3.0)

- **`banner.txt` now ships in the repo** so `vite build` resolves
  `import bannerText from "../../../banner.txt?raw"` in
  `TerminalView.tsx`. v0.3.0 release job failed at this step on every
  platform.
- **`cargo fmt` drift** in `crates/core` cleaned up so the CI fmt
  check passes.
- **`actions/checkout`, `actions/setup-node`, `actions/upload-artifact`,
  `actions/download-artifact` bumped to v5** for Node 24 support
  (v4 is now deprecated on GitHub-hosted runners).

### Providers (since v0.2.2)

- Reasoning-model support end-to-end: DeepSeek v4-flash/pro, DeepSeek r1,
  OpenAI o-series via OpenRouter. `reasoning_content` is captured into a
  Thinking content block and echoed back on subsequent turns (these
  providers 400 without it). Conservative allowlist — non-thinking models
  pay zero extra tokens.
- Provider-aware alias resolution: agent-def `model: sonnet` stays in
  the project's current provider namespace instead of surprise-switching
  to native Anthropic.
- Model catalogue v3 (provider-keyed maps, real ids, per-row provenance).
  `/models` reads from catalogue; `/model` auto-scans Ollama context
  window.

### Agent Teams (since v0.2.2)

- Sandbox boundary anchors to `$THCLAWS_PROJECT_ROOT` (not cwd); worktree
  teammates can write shared artifacts at workspace root; `Write` into
  deep new trees walks up to the longest existing ancestor.
- "Project settings win" on cwd change: GUI reloads `ProjectConfig` and
  rebuilds the agent; worktree teammates pick up the workspace's
  `settings.json` (was silently falling back to user config).
- Role guards on `Bash` / `Write` / `Edit`:
  - Lead can't run `rm -rf`, `git reset --hard`, `git worktree remove`,
    `git push --force`, `git checkout -- …`, or `Write` / `Edit` source
    files. One narrow exception: when a merge is in progress and the
    target file has `<<<<<<<` markers, lead may write the resolved
    content (so package.json-style conflicts can be handled without
    delegating).
  - Teammates can't `git reset --hard <other-branch>`. Same-branch
    recovery (`HEAD~N`, sha, tags) stays allowed.
- `EDITOR` / `GIT_EDITOR` / `VISUAL` / `GIT_SEQUENCE_EDITOR` stubbed to
  `true` for teammates so `vi` / `git commit -e` don't hang waiting for
  input via `/dev/tty`.
- "Plan Approval" convention documented in default `lead.md` /
  `agent_team.md` prompts (lead↔teammate handshake, NOT a user gate).
- `TeamTaskCreate` gains an `owner` field; `claim_next` is role-aware.

### GUI (since v0.2.2)

- Terminal tab: Up/Down arrow prompt history.
- Files tab: WYSIWYG round-trip for `.md` preview + editor; HTML preview
  base-URL fix; off-screen edit-button positioning fix.
- Approval modal; MCP spawn through approval sink; `ReadyGate` for
  deferred startup so the worker accepts prompts before MCP-spawn
  approval returns.
- Context warning banner + per-file size breakdown of the system prompt.
- Settings menu polish: accent-tinted hover + focus highlight; modal
  backdrop dismiss on mousedown-origin (fewer accidental closes).
- Windows GUI fixes backported from upstream: `rfd` file picker,
  `native_dialog` confirm, `ospath()` path-separator helper.

### KMS

- `/kms ingest` slash command; sidebar refreshes live on KMS changes.

### Catalogue tooling

- New `make catalogue` target wraps `catalogue-seed` with a diff-stat
  preview and a per-provider transparency report (new IDs added +
  unchanged + skipped-no-context counts).

### User manual — NEW in this release

- 17-chapter reference manual in English (`user-manual/`) and Thai
  (`user-manual-th/`) with shared images at `user-manual-img/`. Covers
  installation through agent teams. Case-study chapters (18–24) for
  building/deploying real projects remain in workspace draft and will
  graduate to the published manual as each is reviewed.

## [0.2.2] — 2026-04-22

### Added

- **Shared in-process session backing both GUI tabs.** Terminal and Chat tabs now share one Agent + Session + history; typing in either contributes to the same conversation, and `/load` replays the transcript into both.
- **Every REPL slash command works from the GUI.** `/model`, `/provider`, `/permissions`, `/thinking`, `/compact`, `/doctor`, `/mcp`, `/plugin`, `/skill`, `/kms`, `/team`, and the rest all execute identically in Terminal, Chat, and CLI.
- **Live activation for mutations** (no restart required): `/mcp add` spawns the subprocess and registers its tools; `/skill install` refreshes the store and updates the system prompt; `/plugin install` picks up plugin-contributed skills immediately; `/kms use` / `/kms off` register and deregister tools on the fly.
- **Agent Teams toggle in the Settings menu** — one-click on/off for `teamEnabled` without editing `settings.json`.
- **Light/dark/system theme** — click the gear icon → Appearance. Covers app chrome, xterm terminal palette, CodeMirror editor, and Markdown preview; persists to `~/.config/thclaws/theme.json`.
- **Files-tab viewer + editor** — syntax-highlighted preview (CodeMirror 6, ~40 languages), GFM markdown preview (comrak), TipTap markdown editor, CodeMirror code editor with dirty-state tracking and Cmd/Ctrl+S save.
- **Chat tab welcome logo.** Team tab is always visible with an empty-state pointer.

### Fixed

- **Windows startup hang at the secrets-backend dialog.** Every `std::env::var("HOME")` site now goes through a cross-platform `home_dir()` helper that understands `%USERPROFILE%` and `%HOMEDRIVE%%HOMEPATH%`. Previously the silent `Error::Config("HOME is not set")` left the user staring at a silently re-enabled button.
- **Multi-line paste in Terminal tab** submits as one prompt instead of firing one `shell_input` per line.
- **Terminal assistant output concatenates** during streaming — previously each chunk erased the previous one.
- **ANSI escape codes stripped from Chat bubbles** — slash-command output (`render_help`) no longer shows `[2m...[0m` junk.
- **Ctrl+C on empty line cancels the in-flight turn** (was a no-op after the shared-session refactor).
- **Team tab auto-shows** after `TeamCreate` — no longer gated on `teamEnabled`.
- **`/provider X` falls back to the first available model** if the hardcoded default isn't in the live catalogue. `/model X` stays strict so typos fail loud.
- **System-prompt grounding on `agent/*` provider** — the SDK subprocess doesn't receive thClaws's tool registry; when the user asks for teams from `agent/*`, the model is told honestly that team tools are unreachable and to switch provider.

### Removed

- **`managed/*` (Anthropic Managed Agents cloud) provider.** The Managed Agents API is designed for deploying long-running agents to Anthropic's cloud with server-side tool execution — a poor fit for a local interactive CLI where tool calls should hit the user's filesystem.

### Diagnostics

- `THCLAWS_DEVTOOLS=1` opens the WebView devtools so users can Inspect → Console on a blank screen.
- Startup modal shows a diagnostic card after 3 seconds of IPC dead-air, listing `window.ipc` availability, platform, and UserAgent — instead of an indefinite blank screen.

## [0.2.1] — 2026-04-21

First public open-source release — version and date will be set on tag.

### Agent core

- **Native Rust agent loop** — single-binary distribution for macOS, Windows, Linux
- **Streaming provider abstraction** — token-by-token output to the UI, tool-use assembly across chunks
- **History compaction** — automatic when context approaches the configured budget, preserves semantic coherence
- **Permission modes** — `auto`, `ask`, `accept-all` with per-tool approval flow
- **Hooks** — shell commands triggered on agent lifecycle events (before-tool, after-response, etc.)
- **Retry loop with exponential backoff** — skips retries on config errors to surface actionable messages immediately
- **Max-iteration cap** — prevents runaway tool-call loops
- **Compatible session format** (JSONL, append-only) with rename and load-by-name

### Providers

- **Anthropic Claude** — with extended thinking (budget-configurable), prompt caching, and Claude Code CLI bridge
- **OpenAI** — Chat Completions and Responses API
- **Google Gemini** — including multi-byte-safe streaming
- **DashScope / Qwen**
- **Ollama** (local, also exposed as Ollama-Anthropic for drop-in compatibility)
- **Agentic Press LLM gateway** — first-class provider with fixed URL
- **Multi-provider switching mid-session** via `/provider` and `/model`
- **Model validation** — `/model NAME` verifies availability against the active provider before committing
- **Auto-fallback at startup** — picks the first provider with credentials if the configured model has no key

### Tools

- File: `Read`, `Write`, `Edit`, `Glob`, `Ls`, `Grep`
- Shell: `Bash` (with timeout, sandboxed cwd)
- Web: `WebFetch`, `WebSearch` (Tavily / Brave / DuckDuckGo / auto)
- User interaction: `AskUserQuestion`, `TodoWrite`
- Planning: `EnterPlanMode`, `ExitPlanMode`
- Delegation: `Task` (subagent with recursion up to `max_depth`)
- Knowledge: `KmsRead`, `KmsSearch`
- Team coordination: `SpawnTeammate`, `SendMessage`, `CheckInbox`, `TeamStatus`, `TeamCreate`, `TeamTaskCreate`, `TeamTaskList`, `TeamTaskClaim`, `TeamTaskComplete`
- Tool filtering via `allowedTools` / `disallowedTools` in config

### Claude Code compatibility

- Reads `CLAUDE.md` and `AGENTS.md` (walked up from `cwd`)
- `.claude/skills/`, `.claude/agents/`, `.claude/rules/`, `.claude/commands/`
- `.thclaws/` counterparts: `.thclaws/skills/`, `.thclaws/agents/`, `.thclaws/rules/`, `.thclaws/AGENTS.md`, `.thclaws/CLAUDE.md`
- `.mcp.json` at project root (primary) and `.thclaws/mcp.json`
- `~/.claude/settings.json` fallback for users migrating from Claude Code
- Permission shapes: string (`"auto"` / `"ask"`) and Claude Code object (`{allow, deny}` with `Tool(*)` globs)

### Built-in KMS (Knowledge Management System)

- Karpathy-style personal / project wikis under `~/.config/thclaws/kms/` and `.thclaws/kms/`
- Multi-select active list in `.thclaws/settings.json` — multiple KMS feed a single chat
- `index.md` injected into the system prompt; pages pulled on demand via `KmsRead` / `KmsSearch`
- No embeddings in v1 (grep + read); hosted embeddings planned for future RAG upgrade
- Slash commands: `/kms`, `/kms new [--project] NAME`, `/kms use`, `/kms off`, `/kms show`
- Sidebar checkbox UI for attach / detach

### Agent Teams

- Multi-agent coordination via tmux session with a GUI layer
- Role separation: `lead` coordinator + `teammate` executors
- Mailbox-based message passing
- Team tasks (create / list / claim / complete)
- Opt-in via `teamEnabled: true` in settings
- Worktree isolation — teammates can run in separate git worktrees

### Plugin system

- Install from git URL or `.zip` archive
- Enable / disable / show
- Plugins contribute skills, commands, agents, and MCP servers under one manifest
- Project-scope and user-scope installations
- `/plugin` slash command family (install / remove / enable / disable / show)

### MCP (Model Context Protocol)

- stdio transport (spawned subprocess)
- HTTP Streamable transport
- OAuth 2.1 + PKCE for protected MCP servers
- `/mcp add [--user] NAME URL`, `/mcp remove [--user] NAME`
- Discovered tools namespaced by server name

### Skills

- Claude Code's skill format (`SKILL.md` with frontmatter)
- Project, user, and plugin scopes (all merged)
- Exposed as a `Skill` tool AND as slash-command shortcuts (`/skill-name`)
- `/skill install [--user] <git-url-or-.zip> [name]` for installing remote skills
- Skill catalog surfaced in the system prompt

### Desktop GUI

- Native `wry` webview + `tao` windowing (not Electron)
- React + Vite frontend built as a single HTML file
- Sidebar: provider status, active model, sessions, MCP servers, knowledge bases
- Chat panel with streaming text rendering
- xterm.js terminal tab with native clipboard bridge (`arboard`) — Cmd/Ctrl+C/X/V/A/Z
- Ctrl+C heuristic: clears current line when non-empty, otherwise passes SIGINT
- Files tab
- Team view tab (tmux pane preview)
- Settings menu (gear popup): Global instructions, Folder instructions, Provider API keys
- Tiptap-based Markdown editor for AGENTS.md (round-trip through `tiptap-markdown`)
- Startup folder modal — pick working directory on launch
- Provider-ready indicator (green / red dot + strike-through when no key)
- Auto-switch model to a working provider when a key is saved
- Session rename with inline pencil button; `/load by name`
- Turn duration display after each assistant response

### Memory

- Persistent memory store at `~/.config/thclaws/memory/`
- Four memory types: user, feedback, project, reference
- `MEMORY.md` index auto-maintained
- `/memory list`, `/memory read NAME`
- Frontmatter-based classification so future conversations recall relevance

### Secrets & security

- OS keychain integration (macOS Keychain / Windows Credential Manager / Linux Secret Service)
- **Secrets-backend chooser** — first launch asks OS keychain or `.env`
- Single-entry keychain bundle — all provider keys in one item, one ACL prompt per launch
- `.env` fallback when keychain is unavailable (e.g. headless Linux)
- Cross-process key visibility — GUI and PTY-child REPL read the same keychain entry
- Precedence: shell export > keychain > `.env` file
- Sandboxed file tool operations (path-traversal rejection)
- Permission system protects destructive operations
- Env toggles: `THCLAWS_DISABLE_KEYCHAIN` (test opt-out), `THCLAWS_KEYCHAIN_TRACE` (diagnostics)

### Observability

- Per-provider, per-model token usage tracking (`/usage`)
- Turn duration surfaced after each LLM response
- Optional raw-response dump to stderr (`THCLAWS_SHOW_RAW=1`)
- Keychain trace logs for cross-process debugging

### Developer experience

- Slash commands: `/help`, `/clear`, `/history`, `/model`, `/models`, `/provider`, `/providers`, `/config`, `/save`, `/load`, `/sessions`, `/rename`, `/memory`, `/mcp`, `/plugin`, `/plugins`, `/tasks`, `/context`, `/version`, `/cwd`, `/thinking`, `/compact`, `/doctor`, `/skills`, `/skill`, `/permissions`, `/team`, `/usage`, `/kms`
- Shell escape: `! <command>` runs a shell command inline
- `--print` / `-p` non-interactive mode for scripting
- `--resume SESSION_ID` (or `last`) to pick up where you left off
- `--team-agent NAME` for spawning teammates
- Graceful startup — REPL opens with a friendly placeholder if no API key is configured
- Dual CLI + GUI from the same binary
- Compile-time default prompts with `.thclaws/prompt/` overrides

---

*Development prior to 0.2.0 was internal. The public history starts with this release.*
