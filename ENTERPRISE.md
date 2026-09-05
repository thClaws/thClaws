# thClaws Enterprise Edition — Administrator Guide

> **Status:** Phases 0–4 (policy infrastructure, branding, plugin/skill/MCP
> allow-list, gateway enforcement, OIDC SSO) **shipped in v0.6.0**. The
> EE foundation is feature-complete for the four most-asked-for
> enterprise controls. See the "Status by phase" section for what each
> phase covers.

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
│  No policy file        Verified policy file             │
│  → open-core           → org rules apply                │
│    behavior              (branding, allow-list,         │
│                          gateway, SSO, etc.)            │
└─────────────────────────────────────────────────────────┘
```

**Key properties:**

- **Without a policy file**, thClaws behaves exactly as it does for
  the open-source community. Zero overhead, zero behavior change.
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

There are two pieces an organization deploys:

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
# Or run the binary with no policy file — it should still start
# (today's UX preserved when no policy is present).
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
    }
  }
}
```

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
| `audit` (client-side tool-call records, file + http sinks) | 5 | 📝 Design accepted — [RFC 0001](docs/rfc/0001-tool-call-audit.md), tracking [#203](https://github.com/thClaws/thClaws/issues/203) | — |

A policy file with all four blocks present is valid against any v0.5.x+
build; blocks for unimplemented phases are accepted but inert. Once
the corresponding phase ships, the same policy file gains enforcement
without re-signing.

**v0.5.0 caveats**: Frontend (React) branding strings still render
"thClaws" literals — backend branding (REPL banner, GUI title, system
prompt template) is fully active. Wiring the frontend through an IPC
bridge to the branding module is planned for v0.5.x. Until then,
end-user-visible "thClaws" strings inside the GUI window are unbranded;
the window title and CLI surfaces are correctly branded.

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
confined, which files it touched) is **Phase 5**: a `policies.audit`
block that writes thin, payload-free records keyed to the session
JSONL, with `file` and `http` sinks. The accepted design is
[RFC 0001](docs/rfc/0001-tool-call-audit.md); implementation is
tracked in [#203](https://github.com/thClaws/thClaws/issues/203).

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

### File search paths

```
Policy file (JSON):
  1. $THCLAWS_POLICY_FILE
  2. /etc/thclaws/policy.json
  3. ~/.config/thclaws/policy.json

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
