# thClaws Enterprise Edition — Administrator Guide

> ฉบับภาษาไทย: [`ENTERPRISE-th.md`](ENTERPRISE-th.md) — this English
> document is authoritative; the Thai one is a translation of it.

> **Status:** Phases 0–4 (policy infrastructure, branding, plugin/skill/MCP
> allow-list, gateway enforcement, OIDC SSO) shipped in **v0.6.0**;
> `audit` (client-side tool-call records) in **v0.120.0**; `runtime`
> (forced permission mode, tool deny-list, Remote and `--serve` switches)
> in the next release. See "Status by phase" for what each covers and
> where the known limitations are.

This document is for IT/Security administrators evaluating or deploying
thClaws inside an organization. Read this if you need to:

- Force every LLM call through your private gateway (LiteLLM, Portkey,
  Azure OpenAI proxy) for cost control, audit logs, or rate limiting
- Restrict which MCP servers, skills, and plugins users can install
- Pin a specific list of allowed model providers
- Brand the binary (logo, name, support contact) for internal rollout
- Lock client behavior so end users can't override safety policies

If you're an end user trying to use thClaws, see the main
[`README.md`](README.md) instead.

---

## Before you start: what this document assumes

Most administrators reading this have deployed managed software before
but have not deployed an *agent* before. The difference matters, so this
section states it plainly. Skip it if the vocabulary is already familiar.

### An agent is not a chatbot

A chatbot takes text and returns text; its worst failure is being wrong.
thClaws is an **agent**: the model's reply can be a request to run a
tool, the engine runs that tool on the user's machine, the result is fed
back, and the loop repeats until the model stops asking. Reading files,
editing them, and running shell commands are ordinary steps in that
loop.

That is why an organization needs controls here that it would not need
for an ordinary desktop application. The four questions this document
answers are the standard ones:

| Question | Answered by |
|---|---|
| **Who is allowed to use it?** | the `sso` block |
| **What can it reach?** | the `gateway`, `plugins` and `runtime` blocks |
| **Who pays, and how much?** | the `gateway` block |
| **What happened?** | the `audit` block |

### Terms used throughout

- **Tool / tool call** — a named capability the model can ask the engine
  to run (`Read`, `Write`, `Edit`, `Bash`, `WebFetch`…). One request to
  run one is a *tool call*: the unit that gets approved, denied and
  audited.
- **Permission mode** — whether a tool call needs a human to say yes:
  `ask` (prompt before any mutating tool), `auto` (never prompt), `plan`
  (read-only exploration; mutating tools are blocked at dispatch).
- **Confinement** — an OS-level restriction on what a shell command may
  touch, independent of what the command says. Seatbelt on macOS,
  Landlock on Linux, with bubblewrap as a fallback. Where no confiner is
  available the command still runs unconfined, and the audit record says
  so.
- **Provider** — the company or server hosting a model (Anthropic,
  OpenAI, Google, or a local runtime such as Ollama).
- **Gateway** — a server *you* run that speaks the same HTTP API as a
  provider. Every laptop talks to it instead of to the provider, and it
  holds the credentials. LiteLLM, Portkey, an Azure OpenAI deployment or
  something in-house all work.
- **MCP / plugin / skill** — the three ways capability is added to the
  agent. An **MCP server** exposes extra tools, either over **stdio** (a
  local program launched as a subprocess) or **HTTP** (a remote URL); a
  **plugin** is a packaged bundle installed from a URL or repo; a
  **skill** is a markdown instruction file that may carry scripts. All
  three are places third-party code enters the machine, which is why one
  policy block covers them together.
- **Session log** — the JSONL file on the user's machine holding the
  actual conversation content. Distinct from the audit record, which
  holds facts about tool calls and deliberately no payloads.
- **IdP** — identity provider: Okta, Microsoft Entra ID, Google
  Workspace, Auth0, Keycloak, Ping.
- **SIEM** — where your security team collects logs (Splunk, Sentinel,
  Elastic, QRadar). The `audit` block's `http` sink posts to one.
- **Fail-closed / fail-open** — when a control cannot do its job, does
  the system stop or continue without it? thClaws' policy loader is
  **fail-closed** (an unverifiable policy stops the program); its audit
  sinks are **fail-open** (an unreachable SIEM never blocks a user's
  work, and drops are counted). Both are deliberate.

### What the job actually is

Deploying this is four activities with very different rhythms, and
confusing them is the usual source of trouble:

| Activity | How often | If it goes wrong |
|---|---|---|
| **Hold the signing key** | once, then forever | Total compromise. This is the root of trust |
| **Build a binary that trusts it** | once per thClaws release | Nobody can install the new version; existing machines keep working |
| **Write, sign, deploy a policy** | whenever the rules change | Wrong rules apply, or the binary refuses to start |
| **Watch it in production** | continuously | Expiry lands unnoticed and every desk stops at once |

The asymmetry to design around: **changing a rule is cheap** (edit JSON,
re-sign, push one file, no reinstall), **changing the key is expensive**
(a new binary on every machine). Put what you expect to change often in
the policy, and touch the key once a year.

---

## How it works

thClaws is open source and free. There is no separate "Enterprise"
codebase — the same binary runs in both modes. What turns it into an
"enterprise client" is **a signed organization policy file**.

```
┌─────────────────────────────────────────────────────────┐
│         thClaws binary (open source, MIT/Apache)        │
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │ Org Policy Loader (Ed25519 signature verifier)  │    │
│  └────────────────┬────────────────────────────────┘    │
│                   │                                     │
│       ┌───────────┴────────────┐                        │
│       │                        │                        │
│       ▼                        ▼                        │
│  No policy on disk     Verified policy file             │
│  → embedded policy,    → org rules apply                │
│    else open-core        (branding, allow-list,         │
│    behavior              gateway, SSO, etc.)            │
└─────────────────────────────────────────────────────────┘
```

**Key properties:**

- **Without a policy file**, thClaws behaves exactly as it does for
  the open-source community. Zero overhead, zero behavior change.
  (A build that embeds a policy applies the embedded one instead; a
  build that requires one refuses to start. See [Distributing without
  endpoint management](#distributing-without-endpoint-management).)
- **With a verified policy file**, the binary applies the rules in
  that file and overrides any conflicting user-level settings.
- **With an unverified or expired policy file**, the binary refuses
  to start — there is no silent fallback to "open mode" once a
  policy is present.
- The verification key is **embedded at build time** in your
  organization's binary. Users cannot supply their own key to bypass
  it.

This is the same pattern used by GitLab, Mattermost, and Sentry: open
core, commercial wrapper.

---

## Deployment model

There are two pieces an organization deploys — or one, if you embed
the policy in the binary (see [Distributing without endpoint
management](#distributing-without-endpoint-management)):

### 1. The thClaws binary (one-time per release)

Compiled with your organization's Ed25519 **public key** embedded.
Distributed via your normal software-distribution channels (MDM,
Intune, JAMF, system package, internal portal).

The binary is otherwise identical to the open-source release — same
features, same code, same MIT/Apache license. The only difference is
that this build trusts policies signed by your private key.

### 2. The signed policy file (rotated as needed)

A JSON document signed with your organization's Ed25519 **private
key** (which never leaves your security infrastructure). Distributed
to user machines via:

- `/etc/thclaws/policy.json` (system-wide, deployed by MDM/configmap)
- `~/.config/thclaws/policy.json` (per-user fallback, set by login script)

Either location works; the system-wide path takes precedence when both
exist.

The public key file deploys alongside as `/etc/thclaws/policy.pub` (or
`~/.config/thclaws/policy.pub`) — useful for open-source builds where
you want runtime verification without recompiling. Enterprise builds
embed the key at compile time and don't need this file at runtime.

---

## Quick start (10-minute walkthrough)

This produces a working enterprise deployment on a single machine for
evaluation. Production rollout has additional steps (offline keygen,
MDM deployment, IDP configuration) covered later.

### Prerequisites

- A machine with Rust toolchain (`cargo`) — used once to build the
  custom binary
- thClaws source: `git clone https://github.com/thClaws/thClaws`

### 1. Generate your organization's keypair

```bash
cd thClaws
cargo build --release --bin thclaws-policy-tool
./target/release/thclaws-policy-tool keygen \
    --public  ~/.config/thclaws/policy.pub \
    --private ~/secure/acme-org.key
```

> **Important:** the **private** key is the root of trust for every
> policy you'll ever sign. Keep it offline, in a hardware security
> module, or in your existing secrets manager. A leaked private key
> means an attacker can issue policies your binaries will trust.
>
> The **public** key is safe to publish — it goes into binaries and
> deployed config files.

### 2. Build a thClaws binary that trusts your key

```bash
# The build script picks up ~/.config/thclaws/policy.pub by default.
# To override, set THCLAWS_POLICY_PUBKEY_PATH to a custom location.
cd thclaws/crates/core    # or repo root if using the workspace layout
cargo build --release --bin thclaws --features gui
```

Verify the embed worked:

```bash
strings ./target/release/thclaws | grep -A1 "POLICY_PUBKEY" || true
# Or run the binary with no policy file. An open-core build starts
# normally (today's UX preserved when no policy is present); a build
# that also embeds a policy applies the embedded one, and a build with
# THCLAWS_REQUIRE_POLICY=1 and no policy anywhere refuses with exit 2.
```

For Production deployments build for each target architecture (Linux
x86_64, Linux ARM64, macOS Apple Silicon, macOS Intel, Windows x86_64,
Windows ARM64) and distribute via your usual signing/notarization flow.

### 3. Draft a policy file

Create `policy.json`:

```json
{
  "version": 1,
  "issuer": "ACME Corp Security",
  "issued_at": "2026-04-27T00:00:00Z",
  "expires_at": "2027-04-27T00:00:00Z",
  "binding": {
    "org_id": "acme-corp"
  },
  "policies": {
    "branding": {
      "enabled": true,
      "name": "ACME Agent",
      "support_email": "security@acme.example",
      "banner_text": "ACME internal AI assistant — confidential."
    },
    "plugins": {
      "enabled": true,
      "allowed_hosts": [
        "github.com/acmecorp/*",
        "internal.acme.example/*"
      ],
      "allow_external_scripts": false,
      "allow_external_mcp": false
    },
    "gateway": {
      "enabled": true,
      "url": "https://gateway.acme.internal/v1",
      "auth_header_template": "Bearer {{sso_token}}",
      "fail_closed": true,
      "read_only_local_models_allowed": false
    },
    "sso": {
      "enabled": true,
      "provider": "oidc",
      "issuer_url": "https://acme.okta.com",
      "client_id": "thclaws-internal",
      "audience": "thclaws"
    },
    "runtime": {
      "enabled": true,
      "permission_mode": "ask",
      "deny_tools": ["Bash", "WebFetch"],
      "allow_remote": false,
      "allow_serve": false
    }
  }
}
```

The `runtime` block is the one that says *no*:

| Field | Effect |
|---|---|
| `permission_mode` | Forces `ask`, `auto` or `plan`. Applied after `settings.json` **and** after CLI flags, so `--permission-mode auto` and `--accept-all` cannot climb over it. Omit to leave the user's choice. |
| `deny_tools` | Tool names the agent may not use. Removed from every registry so the model never sees them, and refused again at dispatch — a subagent that built its own registry still cannot call one. Case-insensitive. |
| `allow_remote` | `false` blocks thClaws Remote, the tunnel that makes this machine's agent reachable from the cloud. Every path that starts a session refuses. Default `true`. |
| `allow_serve` | `false` makes `--serve` refuse to bind, closing the HTTP surface that carries the web UI and the OpenAI-compatible API. Default `true`. |

The two booleans default to **true** on purpose: a policy cannot switch
off Remote by forgetting a field, only by saying so. `audit` records what
happened; `runtime` decides what may happen — deploy both, and a denied
tool call is still audited with `decided_by: "policy"`.

Each `policies.<feature>.enabled` flag controls whether that feature
applies. Disabled or omitted blocks fall back to open-source default
behavior — useful for staged rollouts (e.g. start with branding +
plugin allow-list, add gateway later).

### 4. Sign the policy

```bash
./target/release/thclaws-policy-tool sign policy.json \
    --private-key ~/secure/acme-org.key
```

Re-running `sign` on an already-signed file is safe — the previous
signature is stripped and replaced.

### 5. Deploy to a test machine

```bash
# Per-user (good for testing)
mkdir -p ~/.config/thclaws
cp policy.json ~/.config/thclaws/policy.json

# System-wide (production)
sudo mkdir -p /etc/thclaws
sudo cp policy.json /etc/thclaws/policy.json
sudo chown root:root /etc/thclaws/policy.json
sudo chmod 644 /etc/thclaws/policy.json
```

#### Distributing without endpoint management

The two artefacts are the binary (your public key is compiled into it)
and `policy.json`. Your private key never leaves your control.

MDM is what guarantees the policy file actually arrives. Without it, a
user who never places the file — or deletes it — runs an unrestricted
binary, because a build with no policy behaves exactly like open-core.

Build with the signed policy embedded and you ship **one file**:

```bash
THCLAWS_POLICY_PUBKEY_PATH=policy.pub \
THCLAWS_POLICY_FILE_EMBED=policy.json \
  cargo build --release --features gui
```

| On the machine | Result |
|---|---|
| A policy file exists | The file is used |
| No file, policy embedded | The embedded copy is used |
| Neither, and this build requires one | Refuses to start, exit 2 |
| Neither, open-core build | Runs unrestricted |

The file still wins, which is what preserves rotation: re-sign,
redistribute one file, no rebuild. Every source is verified identically —
signature, `expires_at` and `binding` are checked whichever way the
policy arrived.

A build requires a policy when it carries both a key and an embedded
policy. `THCLAWS_REQUIRE_POLICY=1` forces the requirement for a
deployment that ships policy by MDM only.

Give an embedded policy a long `expires_at`: renewing one means
rebuilding and redistributing the binary, and an expired embedded policy
refuses to start.

This closes the accident of a missing file. It does not stop someone
determined — the open-core binary is a public download, and no client
can prevent that. Restricting which binaries may run is a device or
network control.

### 6. Verify it loaded

```bash
./thclaws --version
# Run the GUI or CLI — branding text should reflect "ACME Agent",
# `/plugin install` against a non-allowed host should be rejected, etc.
```

If the binary refuses to start with a `signature verification failed`
or `expired` message, the policy or key is mismatched — see
[Troubleshooting](#troubleshooting).

---

## Status by phase

The policy file format is stable as of v0.5.0; individual policies
become enforceable as their respective phase ships:

| Policy block | Phase | Status | Released in |
|---|---|---|---|
| `branding` (logo, name, support contact, banner) | 1 | ✅ Shipped (Rust-side) | v0.5.0 |
| `plugins` (allow-list, no-external-scripts, no-external-mcp) | 2 | ✅ Shipped | v0.5.0 |
| `gateway` (HTTP routing, fail-closed, identity injection) | 3 | ✅ Shipped | v0.5.0 |
| `sso` (OIDC discovery, PKCE, token storage, gateway identity) | 4 | ✅ Shipped (Google smoke verified) | v0.6.0 |
| `audit` (client-side tool-call records, file + http sinks) | 5 | ✅ Implemented — [RFC 0001](docs/rfc/0001-tool-call-audit.md), [#203](https://github.com/thClaws/thClaws/issues/203) | v0.120.0 |
| `runtime` (forced permission mode, tool deny-list, Remote + `--serve` switches) | 8 | ✅ Implemented | next release |

A policy file with every block present is valid against any v0.5.x+
build; blocks for unimplemented phases are accepted but inert. Once
the corresponding phase ships, the same policy file gains enforcement
without re-signing.

### Known limitations, stated up front

These are all discoverable by a security reviewer, so we would rather
you hear them from us:

| Limitation | What it means in practice |
|---|---|
| A few React GUI strings still render "thClaws" literals | Backend branding (REPL banner, GUI window title, system prompt) is fully active; some strings inside the GUI window are not yet routed through the branding module. Cosmetic |
| The plugin allow-list guards installs, not the disk | With `allow_external_mcp: false` stdio MCP servers are refused and HTTP ones must match `allowed_hosts`, but a file copied straight into a skills or plugins folder (by hand or by the agent's `Bash`) still loads. Pair it with `runtime.deny_tools` and egress filtering |
| `WebFetch` / `WebSearch` are not gateway-routed | They are general web access. Use your network firewall for those |
| `gateway.fail_closed` is enforced by construction | No code path builds a direct provider while the gateway is active, but there is no separate HTTP-layer guard |
| Audit sinks are fail-open | An unreachable SIEM never blocks a tool call; drops are counted and shown in `/policy status`. If your posture requires fail-closed auditing, tell us — it is a change request, not a flag |
| No key revocation without a rebuild | If the signing key leaks, the remedy is a new keypair, a new binary and re-signed policies. There is no remote kill switch today |
| Live IdP coverage is Google Workspace | Okta and Entra ID are supported and unit-tested, with policy templates. Budget one smoke session against your own tenant |
| Shared-server (multiuser) deployments force auto-approve | One worker serving many users cannot route approval prompts per person. Enable `audit` there |

---

## Choosing which blocks to turn on

Each block is independent and inert unless the policy enables it, so a
deployment can be staged. Administrators frequently reach for `plugins`
first because it is easy to reason about, when `gateway` and `runtime`
are the two that change the risk picture.

| Block | The question it answers | Turn it on when |
|---|---|---|
| `branding` | — (adoption, not security) | Staff should see this as internal infrastructure |
| `plugins` | What may be *installed*? | You care about third-party code reaching the machine |
| `gateway` | Where does the data go, and who pays? | Almost always. The highest-value block |
| `sso` | Who is allowed to use it? | You have an IdP and want joiner/leaver to apply |
| `audit` | What happened? | Compliance wants evidence, or you run multiuser |
| `runtime` | What may happen at all? | You need "cannot", not "was logged" |

`audit` answers *what happened*; `runtime` decides *what may happen*.
Auditors ask for the first, security architects for the second.

### A staged rollout that works

| Stage | Turn on | What you learn |
|---|---|---|
| 1. Pilot team | `branding` only | That the build, the MDM push and the policy pipeline all work, with no behaviour change to blame |
| 2. Pilot | `+ gateway` | Whether the gateway's model allow-list matches real needs. Expect a week of "model X is missing" |
| 3. Pilot | `+ sso` | Whether the IdP client is registered correctly |
| 4. Pilot | `+ audit` | Whether your SIEM ingests the schema, and what the volume is |
| 5. Fleet | the same four | — |
| 6. Fleet | `+ plugins`, `+ runtime` | The restrictive ones last, once normal usage is known |

Two rules that save incidents: **never introduce a restriction and a new
binary in the same change** (when something breaks you will not know
which caused it), and **keep the pilot group on a shorter `expires_at`**
than the fleet, so expiry fails first on machines you are watching.

---

## Operational concerns

### Key rotation

Best practice: rotate the keypair annually (or on any suspected
compromise). Rotation requires:

1. Generate a new keypair (`thclaws-policy-tool keygen`).
2. Build a new thClaws binary with the new public key embedded.
3. Distribute the new binary to user machines (replace existing).
4. Re-sign all in-use policies with the new private key.
5. Invalidate the old private key.

Until step 3 completes on a given machine, that machine still trusts
policies signed with the old key. There is no remote-revocation
mechanism in v0.5.x — invalidation is "stop signing with the old key,
ship a new binary that doesn't trust it." Sufficient for most
deployments; CRL/OCSP-style live revocation can be added later if
demand emerges.

**Order matters in an incident:** ship the new binary **before**
retiring the old key, or the binaries still in the field cannot verify
the newly signed policy.

### Policy expiry

Set `expires_at` to a date you'll definitely re-sign by. Yearly
expiries are typical. The binary refuses to start with an expired
policy — no grace period.

For staged rollouts where policy edits are frequent, a 90-day expiry
keeps churn visible. For stable mature deployments, 12 months is
reasonable.

### Binary fingerprint binding

Optional: pin a policy to a specific binary build by setting
`binding.binary_fingerprint`. Use case: prevent a disgruntled employee
from copying their corporate-built binary onto a personal machine and
reusing the policy to talk through your gateway.

```bash
# Compute the fingerprint of the binary you just built:
./target/release/thclaws-policy-tool fingerprint ./target/release/thclaws
# Output: sha256:abc123...
```

Add to policy:

```json
"binding": {
  "org_id": "acme-corp",
  "binary_fingerprint": "sha256:abc123def456..."
}
```

Prefix matches are accepted, so you can use partial fingerprints
(`"sha256:abc123"`) if you regularly rebuild without code changes
(e.g. just to update the embedded git SHA).

### Audit logging

Audit logging happens at your **gateway layer**, not inside thClaws
itself. When `policies.gateway.enabled: true` is enforced (Phase 3+),
every provider call routes through your gateway with the user's SSO
token in the auth header — your gateway's existing audit log captures
who did what.

This is by design — duplicating audit logs in two places creates
divergence risk.

Client-side context (which tool ran, who approved it, how it was
confined, which files it touched) is **Phase 5** (v0.120.0+): a
`policies.audit` block that writes thin, payload-free records keyed to
the session JSONL. Design and record schema:
[RFC 0001](docs/rfc/0001-tool-call-audit.md).

```json
"audit": {
  "enabled": true,
  "sinks": [
    { "type": "file", "path": "/var/log/thclaws/audit-%Y-%m-%d.jsonl" },
    { "type": "http", "url": "https://siem.acme.example/thclaws",
      "auth_header_template": "Bearer {{env:THCLAWS_AUDIT_TOKEN}}",
      "batch": 50, "flush_secs": 5 }
  ],
  "include_summary": true,
  "correlate_gateway": true
}
```

- One JSON line per tool call (`tool_call` / `tool_denied`) plus
  `session_start` / `session_end`. Records carry the tool name, who
  approved it (`auto`, `repl`, `gui`, `bot:line`, …), the Bash confine
  mode and whether it was actually enforced, file paths touched, a
  256-byte summary, SHA-256 digests of the input and output, and timing.
  **Never the input or output itself** — those stay in the session
  JSONL, and the digests let an auditor verify a record against it.
- `file.path` accepts strftime tokens; omit it for a daily file under
  the user's data dir. `http` batches NDJSON POSTs; the auth header uses
  the same `{{env:NAME}}` / `{{sso_token}}` templates as `gateway`.
- `actor` is the SSO email when `policies.sso` is active, the member id
  on a hosted multiuser runner, else the OS login.
- `correlate_gateway` adds `x-thclaws-session` / `x-thclaws-turn` to
  every provider request through the org gateway so both logs join.
- **Fail-open**: a sink failure never blocks a tool call. Dropped counts
  show in `/policy status` and in the `session_end` record.
- `enabled: true` with an empty `sinks` list refuses to start, like an
  enabled gateway with no URL.

### Verification checklist

Run through this on a real endpoint — not the build machine — before
declaring the deployment done. Each line is something that has silently
failed for someone before.

- [ ] `/policy status` names your policy file, your issuer, and the
      blocks you expect. If it says `no org policy active`, nothing else
      on this list means anything.
- [ ] The expiry shown is the one you intended, and someone owns the
      calendar entry to re-sign before it.
- [ ] Rename the policy file once and confirm the machine behaves the
      way you planned — community behaviour, or a refusal if this build
      requires a policy. Knowing which of the two you get is the point.
- [ ] Edit one character of the deployed policy and confirm the binary
      refuses with `signature verification failed`. Then restore it.
- [ ] `gateway` on: `/models` lists the gateway's catalogue, and a
      request appears in the gateway's own log attributed to the
      signed-in user rather than a shared service account.
- [ ] `gateway` on: set a personal `OPENAI_API_KEY` in the environment
      and confirm it is ignored.
- [ ] `sso` on: `/sso login` completes against the real tenant. Then
      disable the test account at the IdP and confirm access ends at the
      next refresh.
- [ ] `plugins` on: an install from a non-approved host is refused with
      a message naming the host.
- [ ] `audit` on: a record reaches the sink, and a `Bash` record carries
      `confine.enforced: true` on the platforms you support. If it is
      `false`, the OS confiner is missing from that image — find out why
      before production.
- [ ] `runtime` on: `--permission-mode auto` does not override a forced
      `ask`, and a denied tool is absent from the agent's tool list.
- [ ] The deployed policy file is root-owned and not writable by the
      logged-in user.

### Common mistakes

| Mistake | Symptom | Fix |
|---|---|---|
| Editing a signed policy in place on the endpoint | `signature verification failed` fleet-wide | Edit the source, re-sign, redeploy |
| Rebuilding without updating `binding.binary_fingerprint` | `binding mismatch` after a routine update | Recompute the fingerprint, or rely on prefix matching |
| Testing against a build with no embedded key | "the control does nothing" | `/policy status` first |
| Registering the IdP client as a Web application | `redirect_uri_mismatch` at first login | Re-register as Native / Desktop / public |
| Using an Azure v1 issuer | `discovery doc … missing authorization_endpoint` | The issuer must end in `/v2.0` |
| Assuming the MDM profile applied | One machine group silently unrestricted | Verify on a real endpoint in each group |
| A short `expires_at` on an *embedded* policy | Fleet-wide stop that needs a rebuild to fix | Long expiry when embedded; short only for files on disk |

### Updating policy without rebuilding the binary

The binary embeds the **public key** at compile time. The **policy
file** is loaded at startup from disk. So:

- New policy with same key → just replace `policy.json`, restart
  thClaws, no rebuild needed.
- New key (rotation) → rebuild binary, redistribute.

In practice this means policy edits are cheap (push a new file via
MDM) and key rotations are scheduled events.

### MDM deployment notes

- **macOS** — use a configuration profile to deploy
  `/etc/thclaws/policy.json` and (optionally) `/etc/thclaws/policy.pub`.
  Standard plist-style file payload.
- **Windows** — Group Policy file copy or Intune file deployment to
  `%PROGRAMDATA%\thclaws\policy.json`. We use POSIX paths in
  documentation for clarity; the runtime resolves
  `$THCLAWS_POLICY_FILE` so you can override the path explicitly.
- **Linux** — your usual config-management tool (Ansible, Puppet,
  configmap+kubectl) drops the file at `/etc/thclaws/policy.json`.

### Allowed providers / models

In v0.5.x there's no explicit "allow only these providers" policy
block — that's enforced via the gateway: configure your gateway to
only accept calls for the providers/models you've approved, and any
other call fails at the gateway. This keeps the policy file declarative
about *intent* and the gateway authoritative about *which models are
actually available*.

---

## Troubleshooting

### Binary refuses to start: `signature verification failed`

The policy file is signed with a key that doesn't match the one
embedded in (or available to) this binary. Causes:

1. Policy was signed with the wrong private key (check key material).
2. Binary was built without your public key embedded (`build.rs`
   couldn't find it — check `THCLAWS_POLICY_PUBKEY_PATH` was set or
   the conventional file existed during build).
3. Policy file was edited after signing — even one byte changes the
   canonical-JSON form and invalidates the signature. Re-sign after
   any edit.

Run `thclaws-policy-tool inspect policy.json` to see the policy's
declared `issuer` and verify it matches what your operations team
issued.

### Binary refuses to start: `policy expired`

The `expires_at` date has passed. Re-sign with a new expiry:

```bash
# Edit policy.json to bump expires_at
./thclaws-policy-tool sign policy.json --private-key ~/secure/acme-org.key
# Redeploy
```

### Binary refuses to start: `no public key configured`

A signed policy file is present, but no verification key is available.
Either:

1. The binary wasn't built with an embedded key. Rebuild with
   `THCLAWS_POLICY_PUBKEY_PATH` set, **or**
2. Drop your public key at `/etc/thclaws/policy.pub` (or
   `~/.config/thclaws/policy.pub`) and the binary will pick it up
   from there at runtime.

The second option is the right answer for open-core builds running
in evaluation mode; the first is the right answer for production EE
deployments.

### Binary refuses to start: `binding mismatch`

You set `binding.binary_fingerprint` but the running binary has a
different fingerprint. Either:

1. The deployed binary isn't the one the policy was bound to (a
   newer/older build is in place).
2. The fingerprint was computed against a different file (e.g. the
   debug build vs the release build).

Recompute the fingerprint of the actually-deployed binary
(`thclaws-policy-tool fingerprint <path>`) and update the policy.

### `/models refresh` fails

This isn't a policy issue — the model catalogue is fetched from
`https://thclaws.ai/api/model_catalogue.json`. If your gateway blocks
outbound traffic to that domain, mirror the file internally and set
`THCLAWS_CATALOGUE_URL` (planned for v0.5.0; currently the URL is
hardcoded — open an issue if this affects you).

---

## Frequently asked questions

**Q: Is the Enterprise binary closed source?**
A: No. The code is identical to the open-source release; the only
difference is which public key is embedded. License is MIT/Apache-2.0
either way. The commercial component is the **support contract,
managed signing infrastructure, deployment assistance, and
customer-specific configuration packaging** — not the code.

**Q: Can users disable the policy by editing settings.json?**
A: No. Policy file values override `settings.json` for any conflicting
keys. There's no path through user-level config to disable enforcement
once a verified policy is loaded.

**Q: What happens if a user deletes the policy file?**
A: thClaws falls back to open-core behavior. To prevent this, deploy
the policy file with appropriate filesystem permissions (root-owned,
read-only to users) and use endpoint-management tooling to detect and
re-deploy missing files. The binary itself doesn't enforce file
existence — it can't, since there's no offline way for the binary to
know "you expected a policy here but it's gone."

This is the same model as `/etc/sudoers` or any other admin-deployed
config file: the file system is the trust boundary, not the binary.

If you have no MDM, see [Distributing without endpoint
management](#distributing-without-endpoint-management) — compiling the
policy into the binary closes this gap.

**Q: We need feature X that isn't in any policy block. Can you add it?**
A: Probably yes. We're explicitly building EE features in the open
core (not behind a paywall), so most enterprise asks land as new
policy blocks anyone can use. File a feature request at
https://github.com/thClaws/thClaws/issues with your specific scenario.
We've shipped same-day on similar requests before — see issue #30 as
a recent example.

**Q: How do we get commercial support?**
A: Email [enterprise@thaigpt.com](mailto:enterprise@thaigpt.com) with
your deployment context (org size, target environments, regulatory
requirements). We offer:

- Custom-built binaries with your public key + branding
- Signing infrastructure setup (offline keygen, HSM integration)
- Deployment assistance (MDM profiles, gateway config templates)
- SLA-backed support, prioritized issue response
- Custom policy primitive development for asks that don't fit the
  open-core roadmap

For evaluation and PoC, the open-source build + the workflow in this
document is fully functional — no contract needed.

---

## Reference: tooling

### `thclaws-policy-tool` subcommands

| Subcommand | Purpose |
|---|---|
| `keygen --public PUB --private KEY` | Generate a fresh Ed25519 keypair |
| `sign INPUT --private-key KEY [--output OUT]` | Sign a policy JSON file |
| `verify INPUT --public-key PUB` | Check a signed policy against a key |
| `inspect INPUT` | Pretty-print a policy's structure |
| `fingerprint BINARY` | Compute SHA-256 of a thClaws binary |

Run `thclaws-policy-tool <subcommand> --help` for full options.

### Environment variables

| Variable | Purpose | Used at |
|---|---|---|
| `THCLAWS_POLICY_PUBKEY_PATH` | Override default pubkey path for build embed | Build time |
| `THCLAWS_POLICY_PUBLIC_KEY` | Pubkey contents (base64/PEM) for runtime override | Runtime |
| `THCLAWS_POLICY_FILE` | Override default policy.json search path | Runtime |
| `THCLAWS_POLICY_FILE_EMBED` | Signed policy file to compile into the binary | Build time |
| `THCLAWS_REQUIRE_POLICY` | `1` = this build refuses to start with no policy anywhere | Build time |

### File search paths

```
Policy file (JSON):
  1. $THCLAWS_POLICY_FILE
  2. /etc/thclaws/policy.json
  3. ~/.config/thclaws/policy.json
  4. (compile-time embedded copy — if the build carries one)

Public key:
  1. (compile-time embedded — highest trust)
  2. $THCLAWS_POLICY_PUBLIC_KEY (env content)
  3. /etc/thclaws/policy.pub
  4. ~/.config/thclaws/policy.pub
```

---

## Contact

- Commercial / EE inquiries: [enterprise@thaigpt.com](mailto:enterprise@thaigpt.com)
- Security issues: see [SECURITY.md](SECURITY.md) (Private Vulnerability
  Reporting on GitHub is the preferred channel)
- Public bug reports / feature requests:
  [github.com/thClaws/thClaws/issues](https://github.com/thClaws/thClaws/issues)
- General discussion: [github.com/thClaws/thClaws/discussions](https://github.com/thClaws/thClaws/discussions)

thClaws is developed by **ThaiGPT Co., Ltd.** Open-source under
MIT/Apache-2.0; Enterprise Edition is a commercial wrapper on the same
codebase.
