# KMS — Knowledge Management System

A KMS is a directory of markdown pages plus an `index.md` (table of contents), a `log.md` (append-only change history), a `SCHEMA.md` (page conventions), and split `pages/` + `sources/` subdirs. The LLM is both **reader** and **maintainer**: `KmsRead` / `KmsSearch` consult, `KmsWrite` / `KmsAppend` author, `/kms ingest` adds sources, `/kms lint` audits, `/kms file-answer` files explorations back. Karpathy-style: no embeddings, just grep + read + frontmatter.

This doc covers: the three-layer architecture, on-disk layout, YAML frontmatter convention, ingest semantics (split source/page + URL/PDF support), system-prompt injection, slash commands, the four KMS tools, lint operations, the re-ingest cascade, security model, Obsidian compatibility, and the cross-process behavior.

**Source modules:**
- `crates/core/src/kms.rs` — `KmsRef`, `KmsScope`, `KmsManifest` + `KMS_SCHEMA_VERSION`, `create`, `resolve`, `list_all`, `ingest` + `ingest_url` + `ingest_pdf`, `write_page` + `append_to_page` + `delete_page` + `writable_page_path`, `parse_frontmatter` + `write_frontmatter`, `lint` + `LintReport` (six categories incl. `missing_required_fields`), `system_prompt_section` + categorized index, `mark_dependent_pages_stale` + `scan_stale_markers`, Migration framework (`Migration` + `migrations()` + `migrate` + `detect_schema_version` + `MigrationReport` + `LEGACY_SCHEMA_VERSION`)
- `crates/core/src/tools/kms.rs` — `KmsReadTool`, `KmsSearchTool`, `KmsWriteTool`, `KmsAppendTool`, `KmsDeleteTool`
- `crates/core/src/shell_dispatch.rs` — `/kms` slash-command handlers (GUI path); `format_lint_report` + `format_wrap_up_report` + `format_migration_report`, `has_actionable_issues`, `compose_kms_linker_prompt` + `compose_kms_reconcile_prompt`, `sanitize_alias_for_dispatch`
- `crates/core/src/repl.rs` — `SlashCommand::Kms*` enum + parser + CLI dispatch; `build_kms_ingest_session_prompt` + `build_kms_dump_prompt` + `build_kms_challenge_prompt` (inline prompt builders for the rewrite-before-match commands)
- `crates/core/src/shared_session.rs` — `kms_active`-driven tool registration at worker boot; rewrite-before-match intercepts for `KmsIngestSession` + `KmsDump` + `KmsChallenge`
- `crates/core/src/config.rs` — `kms_active` persistence in `.thclaws/settings.json` via `ProjectConfig::set_active_kms`
- `crates/core/src/default_prompts/kms-linker.md` + `kms-reconcile.md` — built-in subagent definitions (`include_str!`-embedded via `agent_defs::seed_builtins`)
- `crates/core/src/agent_defs.rs` — `BUILTINS` array registers `kms-linker` + `kms-reconcile` alongside `dream` + `translator`

**Cross-references:**
- [`built-in-tools.md`](built-in-tools.md) §3 — `KmsRead` + `KmsSearch` + `KmsWrite` + `KmsAppend` + `KmsDelete` tool surface
- [`context-composer.md`](context-composer.md) — `kms::system_prompt_section()` injects per-active-KMS Schema/Index/Tools blocks
- [`permissions.md`](permissions.md) — `KmsWrite` / `KmsAppend` / `KmsDelete` `requires_approval()` posture (mutating; gated in Ask mode)
- [`sessions.md`](sessions.md) — `/kms file-answer` reads from `state.session.messages` (the live session)
- [`commands.md`](commands.md) — `/kms` is a built-in slash command (not a `.md` prompt template)
- [`schedule.md`](schedule.md) §15 — pre-packaged KMS-maintenance schedule presets (`schedule_presets::add_from_preset`)

---

## 1. Overview

### Concept

A KMS implements the [llm-wiki pattern](../docs/llm-wiki.md): a persistent, **compounding** knowledge base maintained by the LLM. Most LLM-document workflows look like RAG — index a corpus, retrieve chunks at query time, generate. Knowledge gets re-derived on every question. A KMS instead has the LLM build and maintain a structured wiki between you and the raw sources: cross-references compiled once and kept current, contradictions flagged, summaries refreshed when sources change.

The maintenance burden is the LLM's job; the curation + question-asking + direction is yours.

### Three layers

```
┌── sources/           layer 1: raw, immutable. LLM reads, never modifies.
│   ├── article.md     The source of truth. CSV, txt, json, md, fetched HTML.
│   └── paper.txt
│
├── pages/             layer 2: the wiki. LLM-authored markdown with frontmatter.
│   ├── api-x.md       Curated summaries, entity pages, concept pages,
│   ├── paper-y.md     comparisons. Each page references its sources via
│   └── synthesis.md   frontmatter `sources:` field. Cross-links via
│                      [label](pages/other.md). LLM owns this layer.
│
├── SCHEMA.md          layer 3: the schema. Human-edited rules for layer 2.
├── index.md           Auto-maintained table of contents (one bullet per page).
└── log.md             Auto-maintained change history (## [date] verb | alias).
```

### Lifecycle

```
USER  /kms new mynotes              → create() seeds index/log/SCHEMA + dirs
USER  /kms use mynotes              → adds to .thclaws/settings.json kms_active
                                      → registers KmsRead/Search/Write/Append tools
                                      → next system prompt includes KMS block
USER  /kms ingest mynotes file.md   → copy to sources/, write stub in pages/
LLM   reads stub, KmsRead source,   → enriched page with curated summary +
      KmsWrite enriched page          frontmatter category/tags
USER  asks question                 → LLM consults index, KmsRead pages, answers
USER  /kms file-answer mynotes "X"  → assistant message → new page (compounds)
USER  /kms lint mynotes             → broken links / orphans / drift / missing FM
USER  /kms ingest mynotes file.md   → cascade marks dependent pages STALE
        --force                       (frontmatter sources: <alias>)
USER  /kms off mynotes              → unregisters tools, removes from kms_active
```

---

## 2. On-disk layout

### Two scopes

```
<project>/.thclaws/kms/<name>/     # Project scope — only visible from this project (DEFAULT)
~/.config/thclaws/kms/<name>/      # User scope — visible from any project (--user opt-in)
```

`KmsScope` is a closed enum (`User` | `Project`). Both directories are walked by `list_all()`. `resolve(name)` checks **project first**, falls back to user — so a project-scope `notes` overrides a user-scope `notes` for that project. Same precedence pattern as project CLAUDE.md vs user CLAUDE.md.

`/kms new <name>` defaults to project scope (a KMS is typically tied to the code you're working on, so `./.thclaws/kms/<name>` follows the repo). `--user` opts out into user-global. `--project` is accepted as a no-op alias so muscle memory from the old default doesn't break.

### Directory contents (`kms::create` seeds)

```
<kms_root>/
├── index.md       # "# <name>\n\nKnowledge base index — list each page with a one-line summary.\n"
├── log.md         # "# Change log\n\nAppend-only list of ingests / edits / lints.\n"
├── SCHEMA.md      # Starter schema content (edit this to set conventions)
├── manifest.json  # Schema version + optional frontmatter requirements (see §2.3)
├── pages/         # Wiki pages (LLM-authored, frontmatter-tagged)
└── sources/       # Raw source files (immutable; copied here by `/kms ingest`)
```

`SCHEMA.md`, `index.md`, `log.md` stems are reserved (`RESERVED_PAGE_STEMS`); ingest + write tools refuse those aliases so the LLM can't clobber them by mistake.

### Manifest (`manifest.json`)

Machine-readable schema, separate from `SCHEMA.md` (which is prose for the LLM). Created by `kms::create` with `schema_version: "1.0"` and an empty `frontmatter_required` map — opt-in policy keeps the lint behaviour identical to legacy KMSes by default.

```rust
pub struct KmsManifest {
    pub schema_version: String,
    pub frontmatter_required: BTreeMap<String, Vec<String>>,
}
pub const KMS_SCHEMA_VERSION: &str = "1.0";
```

Both fields are `#[serde(default)]` so adding new fields in future versions doesn't break older manifests on read.

| Method | Returns |
|---|---|
| `KmsRef::manifest_path()` | `<root>/manifest.json` |
| `KmsRef::read_manifest() -> Option<KmsManifest>` | `None` for: file absent, path is a symlink, malformed JSON. Legacy-KMS-tolerant by design. |

`frontmatter_required` is keyed by `"global"` (every page) or a `category:` value (per-category rule). Consumed by `lint` (§4.6) — empty map disables enforcement entirely. Schema version anchors `migrate` (§4.9).

---

## 3. Frontmatter convention

Pages may begin with a YAML frontmatter block. The convention covers eight fields; any others are stored verbatim and re-emitted on round-trip:

```markdown
---
title: Topic title
topic: One-line description
sources: ["https://…", "session-XYZ", "memory"]   # required (warns if absent)
category: research
tags: ai, retrieval
created: 2026-05-03
updated: 2026-05-11
verified: 2026-05-11                              # /research stamps this; manual writes leave it absent
---

(body — KmsWrite auto-injects `# {title}\nDescription: {topic}\n---` when the body doesn't start with a `# heading`)
```

| Field | Meaning | Used by |
|---|---|---|
| `title` | Human-readable page title | `write_page::maybe_inject_canonical_header` (falls back to page stem) |
| `topic` | One-line summary | Rendered as the `Description: …` line in the canonical header (omitted when blank) |
| `sources` | URLs / `session-<id>` / `memory` / `[]` | Provenance — `KmsWriteTool` warns when this key is missing or empty (audit finding "Traceability") |
| `category` | One-word grouping | Categorized index in system prompt |
| `tags` | Comma-separated labels | Dataview queries (Obsidian) |
| `created` | YYYY-MM-DD; auto-stamped on first write | Audit |
| `updated` | YYYY-MM-DD; auto-stamped on every write | Sort / freshness |
| `verified` | YYYY-MM-DD; stamped explicitly by callers that verified (e.g. `/research`) | `KmsReadTool::staleness_warning` — pages with `verified:` older than 90 d get a banner; missing field gets a softer "no verification record" hint |

### Canonical header injection

`kms::write_page` invokes `maybe_inject_canonical_header(body, stem, fm)` after parsing frontmatter:

- If `body.trim_start().starts_with("# ")` → body is left alone (model intentionally wrote a heading)
- Otherwise: prepend `\n# {title}\nDescription: {topic}\n---\n\n` to the body. Title falls back to the page stem when `title:` is absent; the `Description:` line is omitted when `topic:` is missing/blank
- Idempotent on re-writes: `body_has_leading_heading` detects the previously-injected `# {title}` and skips re-injection

### Index summary uses pre-injection body

`first_meaningful_line(body)` runs against the **user-supplied** body, NOT the canonical-header version. Otherwise the model's first real paragraph would be replaced by the auto-injected `# {title}` line in `index.md`, which adds zero signal beyond the link text itself.

### Parser

`kms::parse_frontmatter(s) -> (BTreeMap<String, String>, String)` is hand-rolled (no `serde_yaml` dep). Single-line string values only — no nesting, anchors, or multiline. Pages without frontmatter return `(empty, original)`.

`kms::write_frontmatter(map, body) -> String` round-trips. Auto-quotes values containing `:`, `#`, leading whitespace, `"`, or `\n`:

```rust
fm.insert("note".into(), "has: colon".into());
write_frontmatter(&fm, "body\n");
// → "---\nnote: \"has: colon\"\n---\nbody\n"
```

---

## 4. Operations

### `ingest` — adding raw sources (M6.25 BUG #2)

`kms::ingest(kref, source_path, alias, force)` does a **two-step split**:

1. Copy raw bytes to `sources/<alias>.<ext>` (immutable; never re-touched by LLM tools)
2. Write a stub page `pages/<alias>.md` with frontmatter pointing back at the source:

```markdown
---
category: uncategorized
created: 2026-05-03
sources: <alias>
updated: 2026-05-03
---
# <alias>

Stub page — raw source at `sources/<alias>.<ext>`. Summary line: <first content line>

_Replace this stub with a curated summary, key takeaways, cross-references to other pages, etc._
```

The LLM enriches the stub via `KmsWrite`. Pre-M6.25 ingest copied the source straight into `pages/`, conflating layer 1 (raw) with layer 2 (synthesis) — fixed in M6.25 ([dev-log/143](../dev-log/143-kms-m6-25-llm-wiki-alignment.md)).

`force=true` re-runs the copy + stub write AND triggers the re-ingest cascade (§4.6).

Allowed source extensions: `md`, `markdown`, `txt`, `rst`, `log`, `json` (`INGEST_EXTENSIONS`). Anything else → "not supported — allowed: …" error. URL + PDF flow through dedicated wrappers (§4.2 + §4.3).

### `ingest_url` — fetching remote sources (M6.25 BUG #8)

```rust
kms::ingest_url(kref, url, alias, force).await
```

Fetches via `reqwest::Client::builder().timeout(30s)`, prepends a `<!-- fetched from {url} on {date} -->` banner to the response body, stages to a temp `.md` file, routes through standard `ingest()`. Status check rejects non-2xx.

Alias derivation: explicit `--alias` wins; otherwise the last path segment (stripped of query string). Sanitized via `sanitize_alias` (`[A-Za-z0-9_-]` only; trim outer `_`).

### `ingest_pdf` — extracting PDF text (M6.25 BUG #8)

```rust
kms::ingest_pdf(kref, pdf_path, alias, force).await
```

Spawns `pdftotext -layout -enc UTF-8 <path> -` in a `tokio::task::spawn_blocking` (same shape as `PdfReadTool`), prepends `<!-- extracted from PDF '<path>' on <date> -->`, stages to temp, routes through `ingest()`. Requires `poppler-utils` installed locally.

Alias: explicit `--alias` wins; otherwise the file stem.

### `write_page` — author or replace a page (M6.25 BUG #1)

```rust
kms::write_page(kref, page_name, content) -> Result<PathBuf>
```

Create-or-replace. Parses frontmatter from `content`; merges with auto-stamped:
- `created:` (only on new pages — preserved on replace)
- `updated:` (always today's date)

User-supplied frontmatter keys win on conflict. Then writes the merged frontmatter + body to `pages/<page>.md`, updates `index.md`, appends a `## [date] wrote | <stem>` log entry.

Path validation: `kms::writable_page_path` rejects empty / `..` / path separators / control chars / absolute paths / reserved stems. Canonicalizes parent inside `pages_dir` (defeats symlink escape). Refuses if `pages/` itself is a symlink.

### `append_to_page` — incremental updates (M6.25 BUG #1)

```rust
kms::append_to_page(kref, page_name, chunk) -> Result<PathBuf>
```

If the page exists with frontmatter: bumps `updated:`, appends chunk after a separating newline, re-serializes. If exists without frontmatter: plain `OpenOptions::append`. If doesn't exist: creates with bare body (no frontmatter — LLM can rewrite via `KmsWrite` later to add metadata). Always appends a `## [date] appended | <stem>` log entry.

### `lint` — health check (M6.25 BUG #3)

```rust
kms::lint(kref) -> Result<LintReport>
```

Pure-read; no mutation. Walks `pages/`, returns six issue categories:

| Field | Meaning |
|---|---|
| `broken_links: Vec<(page, target)>` | `[label](pages/x.md)` where `pages/x.md` doesn't exist |
| `orphan_pages: Vec<String>` | Page on disk with no inbound link from any other page |
| `index_orphans: Vec<String>` | Index entry with no underlying file |
| `missing_in_index: Vec<String>` | Page on disk with no index entry |
| `missing_frontmatter: Vec<String>` | Page with no `---\n…\n---\n` block |
| `missing_required_fields: Vec<(stem, source_key, field)>` | Page violates a `frontmatter_required` rule from `manifest.json`. `source_key` is `"global"` or the page's `category:` value, indicating which manifest rule fired. |

`LintReport::total_issues()` sums all. `format_lint_report(name, &report)` (in `shell_dispatch.rs`) renders the user-facing summary with per-category counts.

The `missing_required_fields` check is gated on `kref.read_manifest()`. Absent manifest, malformed JSON, or empty `frontmatter_required` map → check is skipped silently (legacy-KMS contract). Pages already flagged for `missing_frontmatter` are *not* double-reported here — frontmatter absence is one bug; once fixed, the per-field check can fire on the next lint.

### Re-ingest cascade (M6.25 BUG #10)

When `ingest()` replaces an existing alias (`force=true` + `page_existed`), `mark_dependent_pages_stale(kref, changed_alias)` walks every page; if frontmatter `sources:` mentions the changed alias (comma- or whitespace-separated), the page's body gets a stale marker:

```markdown
> ⚠ STALE: source `<alias>` was re-ingested on 2026-05-03. Refresh this page.
```

The page's `updated:` field bumps. Returned in `IngestResult.cascaded`; surfaced in slash-command output: `"marked N dependent page(s) stale"`. The user (or LLM next turn) acts on the markers via `KmsWrite`.

### `scan_stale_markers` — pure-read inverse of the cascade

```rust
kms::scan_stale_markers(kref) -> Result<Vec<StaleEntry>>

pub struct StaleEntry {
    pub page_stem: String,
    pub source_alias: String,
    pub date: String,
}
```

Walks `pages/` and surfaces every `> ⚠ STALE: source \`<alias>\` was re-ingested on <date>.` marker `mark_dependent_pages_stale` has produced. Multiple entries per page are returned when a source has been re-ingested in successive waves without the page being refreshed — refresh debt accumulates.

The regex is anchored on the producer's exact output:

```
> ⚠ STALE: source `([^`]+)` was re-ingested on ([^.\s]+)
```

Date pattern is intentionally loose so a future format change in `usage::today_str()` doesn't silently break detection. **Producer/consumer marker contract** is locked by an end-to-end test: ingest → derived page → `--force` re-ingest → `scan_stale_markers` finds exactly one entry.

Sort order: `(page_stem, source_alias, date)` — stable for diffing across runs.

Used by `/kms wrap-up` (§6) to surface refresh debt at session end.

### `migrate` — chained schema upgrades

```rust
pub struct Migration {
    pub from: &'static str,
    pub to: &'static str,
    pub apply: fn(&KmsRef, dry_run: bool) -> Result<Vec<String>>,
}

pub fn migrations() -> Vec<Migration>;
pub fn detect_schema_version(kref: &KmsRef) -> String;
pub fn migrate(kref: &KmsRef, dry_run: bool) -> Result<MigrationReport>;

pub const LEGACY_SCHEMA_VERSION: &str = "0.x";
```

`detect_schema_version` returns `"0.x"` when the manifest is absent OR `schema_version` is empty — that's how every KMS predating manifests looks on disk. Otherwise returns the declared value.

`migrate` walks the chain `current → KMS_SCHEMA_VERSION`. Each step calls its `apply` function with the requested mode:

- `dry_run=true` — `apply` must not touch the filesystem. Returns the action list as if it had run.
- `dry_run=false` — `apply` writes, returns descriptions of what was actually written. After all steps run, `append_log_header` records `migrated | <from> → <to>`.

Idempotent at latest version: returns a `MigrationReport` with empty `steps` and `current_version == target_version`. Bounded loop (`for _ in 0..table.len() + 1`) defends against table cycles; an unfound `from` step yields `"no migration path from schema version 'X'"`.

Current chain: only `0.x → 1.0` (`migrate_0_to_1`) — pure additive: writes `manifest.json` with `schema_version: "1.0"` + empty `frontmatter_required`. Page bodies untouched. Future schema changes register `Migration { from: "1.0", to: "2.0", apply: migrate_1_to_2 }` and chain naturally.

```rust
pub struct MigrationReport {
    pub current_version: String,
    pub target_version: String,
    pub steps: Vec<MigrationStep>,
    pub dry_run: bool,
}
pub struct MigrationStep {
    pub from: String,
    pub to: String,
    pub actions: Vec<String>,  // human-readable per-step summary
}
```

---

## 5. System-prompt injection (M6.25 BUG #5 + #6)

`kms::system_prompt_section(active: &[String]) -> String` is called by [`context-composer`](context-composer.md) and the REPL system-prompt builder. Returns `""` when no active KMS or when names resolve to nothing.

Output shape (per active KMS):

```markdown
# Active knowledge bases

The following KMS are attached to this conversation. Their schemas + indices are below
— consult them before answering when the user's question overlaps. Treat KMS content
as authoritative over your training data for the topics it covers. You are both reader
AND maintainer: file new findings, update entity pages when sources contradict them,
and run `/kms lint <name>` periodically.

## KMS: mynotes (project)

### Schema
<first 100 lines / 5 KB of SCHEMA.md>

### Index
**research**
- [paper-a](pages/paper-a.md) — Paper A summary line
- [paper-b](pages/paper-b.md) — Paper B summary line

**api**
- [api-x](pages/api-x.md) — API X reference
```

(`## KMS tools` is now rendered globally — see "Globalised tool reference" below — so each per-KMS block no longer carries its own duplicated Tools subsection.)

> M6.38.2 audit fix (Bug B): the `KmsDelete` line was missing from the Tools block before this fix — the tool was registered when `kms_active` was non-empty but had no narrative context in the system prompt. The "last resort" / "prefer KmsWrite" framing biases the model toward merge/supersede patterns over destructive deletion (matching the `dream` and `kms-reconcile` agents' written posture).

### Globalised tool reference (audit finding B)

The per-KMS `### Tools` subsection used to render inside every KMS block (~250 bytes each). With N KMSes attached that scaled O(N) for content that didn't vary across KMSes (only the `kms: "<name>"` argument changes). `system_prompt_section` now renders the tool reference exactly **once** as a top-level `## KMS tools (apply to every KMS below — substitute the kms: argument)` block, sitting between the MANDATORY consultation procedure and the per-KMS subsections. Each per-KMS block now carries only Schema + Index. Savings: ~200 bytes per additional attached KMS.

The global block also surfaces `KmsCreate` (previously discoverable only via the tool registry), so `/dream` bootstrap workflows are visible to any agent reading the system prompt.

### Concise SCHEMA template (audit finding C)

`kms::create` previously seeded `SCHEMA.md` with two fenced-code blocks (input shape + "Final on-disk shape" example). The on-disk example was inert for the model — `KmsWrite` stamps it automatically. The template now carries only the input shape, saving ~300 bytes per KMS rendered into the prompt. Existing KMSes keep their old SCHEMA.md (human-editable, no migration).

### Categorized index

`render_index_section(kref)` walks `pages/`, parses frontmatter, groups bullets under `**<category>**` headers (BTreeMap-sorted). Pages without `category:` go under `**uncategorized**`. **Falls back** to the raw `index.md` (capped per the M6.18 BUG M7 limits — 200 lines / 25 KB) when no page has frontmatter — preserves backwards compat with pre-M6.25 KMSes that haven't adopted frontmatter yet.

The categorized form is also capped at `MEMORY_INDEX_MAX_LINES` (200); if exceeded:

```
_… index truncated at 200 entries (total: 487)_
```

### Schema cap

`SCHEMA.md` injection is capped at **100 lines / 5 KB** via `read_text_capped` — schemas are meant to be brief instructions, not archives.

---

## 6. Slash commands

| Syntax | Effect |
|---|---|
| `/kms` (or `/kms list` / `/kms ls`) | List KMSes, mark active with `*` |
| `/kms new <name>` | Create **project-scope** KMS (default — `./.thclaws/kms/<name>`) |
| `/kms new --user <name>` | Create user-scope KMS (`~/.config/thclaws/kms/<name>`) |
| `/kms use <name>` | Attach (registers tools, includes in prompt, persists to `.thclaws/settings.json`) |
| `/kms off <name>` | Detach (drops tools when last KMS detaches) |
| `/kms show <name>` | Show scope + path + attached state |
| `/kms ingest <kms> <file.md>` | Standard text ingest |
| `/kms ingest <kms> <file.pdf>` | Auto-routed to `ingest_pdf` (`pdftotext`) |
| `/kms ingest <kms> https://...` | Auto-routed to `ingest_url` (HTTP fetch + 30s timeout) |
| `/kms ingest <kms> $` (M6.28) | `$` source = current chat session. Triggers an agent turn that summarizes history and calls `KmsWrite` to file the page. Page name resolves from session.title (sanitized) when set, else session.id (`sess-<hex>`); user can override via `as <alias>`. Frontmatter pre-set to `category: session, sources: chat`. |
| `/kms ingest <kms> <target> as <alias>` | Override derived alias |
| `/kms ingest <kms> <target> --force` | Replace + cascade dependents |
| `/kms dump <kms> <text>` (or `capture`) | Freeform capture — agent classifies the dump and routes via `KmsWrite` / `KmsAppend`. Same agent-loop rewrite path as `KmsIngestSession`. |
| `/kms challenge <kms> <idea>` (or `redteam`) | Pre-decision red-team — agent searches the KMS for past failures / reversed decisions / contradictions on `<idea>` and produces a Red Team analysis. Read-only; same rewrite path as `/kms dump`. |
| `/kms lint <name>` (or `check` / `doctor`) | Health-check report (six categories, see §4.6) |
| `/kms wrap-up <name>` | Lint + `scan_stale_markers` rolled into one summary |
| `/kms wrap-up <name> --fix` | GUI-only — dispatches `kms-linker` subagent to act on the report (see §15) |
| `/kms reconcile <name> [<focus>]` (or `resolve`) | GUI-only — dispatches `kms-reconcile` subagent (see §15) for dry-run contradiction scan; classifies findings as clear-winner / ambiguous / evolution |
| `/kms reconcile <name> [<focus>] --apply` | Same, but executes — rewrites outdated pages with `## History`, creates `Conflict — <topic>.md` for ambiguous cases |
| `/kms migrate <name>` | Dry-run preview of the schema chain |
| `/kms migrate <name> --apply` | Execute the chain. Aliases: `--execute`, `--run` (and `--dry-run` / `--plan` to opt back) |
| `/kms file-answer <kms> <title>` (or `file`) | File latest assistant message as a new page |

**Source auto-detection** in `parse_slash`: `t == "$"` → `KmsIngestSession` (M6.28); `t.starts_with("http://") || t.starts_with("https://")` → `KmsIngestUrl`; `t.to_ascii_lowercase().ends_with(".pdf")` → `KmsIngestPdf`; otherwise `KmsIngest`.

### `/kms ingest <name> $` — file the current chat session (M6.28)

Special source target `$` triggers an **agent turn**, not a synchronous ingest. The slash command rewrites itself into a prompt that instructs the model to:

1. Summarize the current conversation as a self-contained wiki page (200-1500 words, synthesized — not transcribed).
2. Call `KmsWrite(kms: "<name>", page: "<page>", content: "...")` with frontmatter `category: session, sources: chat, description: <one-line hook>`.
3. Confirm to the user with the resolved path.

Page name resolves at the call site via `repl::resolve_session_alias` with this precedence:

1. **User-supplied** via `as <alias>` (sanitized through `kms::sanitize_alias`)
2. **Session title** if `state.session.title` is set (sanitized — spaces and punctuation become `_`)
3. **Session id** as fallback (`sess-<hex>`, already slug-safe)

The provenance is passed alongside the resolved slug as `KmsIngestSessionAliasSource` so the prompt's "Page name:" hint tells the model where the slug came from — and lets it refine the slug only when the conversation has a clearer theme than the auto-derived one.

`--force` flag forwarded as a hint to overwrite on collision.

Implementation: `parse_kms_subcommand` returns `SlashCommand::KmsIngestSession` for the `$` target. Both CLI (`run_repl`) and GUI (`shared_session::handle_line`) intercept this variant in their **rewrite-before-match** blocks (alongside skill / command rewrites). The slash command `line` is replaced with the prompt from `repl::build_kms_ingest_session_prompt`, then the regular agent-turn pipeline takes over — the rewrite becomes the user prompt for that turn.

`shell_dispatch::dispatch` has a defensive arm for `KmsIngestSession` that emits a clear error if it's ever reached directly (which shouldn't happen in normal flow — the rewrite intercepts first).

Every dispatch handler exists in **two places** — `shell_dispatch.rs` (GUI worker, async) and `repl.rs` (CLI loop). Both call the same `kms::*` functions; only the output formatting differs (CLI uses `COLOR_DIM`/`COLOR_YELLOW` ANSI codes).

### `/kms dump <name> <text>` — freeform capture, agent-routed

Same architectural shape as `/kms ingest <name> $`: a **rewrite-before-match** intercept replaces the slash command with a structured prompt, the agent turn picks it up, the agent uses the standard KMS tool surface to act.

Parser produces `SlashCommand::KmsDump { name: String, text: String }` — the rest of the line after the KMS name is the dump body verbatim (multi-line paste fine; no length cap beyond the model's context window). Empty text → `Unknown` with `usage: /kms dump <name> <text...>`.

`repl::build_kms_dump_prompt(kms_name, dump_text)` composes the agent-facing prompt inline (no template file, matching `build_kms_ingest_session_prompt`). Embeds the dump verbatim between `=== DUMP CONTENT ===` markers, then declares the **announce-then-execute** contract:

1. Scan the dump for distinct chunks (one decision, one observation, one new source per chunk).
2. Per chunk, pick a destination: `append-to-existing` (search first, then `KmsAppend`), `create-new-page` (`KmsWrite` with frontmatter + at least one cross-link), `defer` (skip and report).
3. Print the routing plan in plain text **before** any tool calls — user can ⌃C to abort.
4. Execute tool calls.
5. End with a fixed-shape `**Created** / **Appended** / **Skipped**` report.

Hard rules baked into the prompt: no inventing sources, no `KmsDelete`, every new page must reference at least one existing page. `kms: "<name>"` is provided as the tool argument.

Intercept lives in two places (mirroring `KmsIngestSession`):
- `repl.rs:run_repl` rewrite block — CLI path
- `shared_session::handle_line` rewrite block — GUI path

Both check **two** preconditions before rewriting:

1. `kms::resolve(&name).is_some()` — KMS exists on disk; missing KMS falls through to the dispatch arm which reports `"no KMS named '<name>'"`
2. `state.config.kms_active.is_empty()` is **false** (M6.38.1 audit fix) — KMS tools register only when at least one KMS is active. The dump prompt's `KmsWrite` / `KmsAppend` calls would otherwise hit "tool not found." Refusal message: `"/kms dump <name>: no KMS attached to this session. Run \`/kms use <name>\` first."`

Both checks must pass before the rewrite happens; otherwise the appropriate error surfaces and the slash command never becomes a real agent turn. The dispatch arm itself is otherwise unreachable in normal flow.

### `/kms wrap-up <name> [--fix]` — session-end review

Parser produces `SlashCommand::KmsWrapUp { name: String, fix: bool }`. Order-insensitive flag parsing — `--fix` and the name can appear in either order.

Without `--fix`: pure orchestration over existing `kms::*` functions.

```rust
let lint = kms::lint(&k)?;
let stale = kms::scan_stale_markers(&k)?;
emit(format_wrap_up_report(&name, &lint, &stale));
```

`shell_dispatch::format_wrap_up_report` reuses `format_lint_report` for the lint section (drops its header line so there's only one), appends a "stale pages awaiting refresh" block, ends with a "next steps" hint. Clean state output: `"KMS '<name>': clean — nothing to wrap up."`

With `--fix`: GUI-only side-channel dispatch. After emitting the report, the dispatch is gated on **two** preconditions before spawning the `kms-linker` subagent (§15):

1. `has_actionable_issues(&lint, &stale)` returns true — actionable = `broken_links ∪ missing_in_index ∪ missing_required_fields ∪ stale ≠ ∅`. Orphans and `missing_frontmatter` are excluded (the subagent's prompt forbids acting on them).
2. `state.config.kms_active.is_empty()` is **false** (M6.38.1 audit fix). The subagent inherits the parent's tool registry via `ProductionAgentFactory::build`; KMS tools register only when `kms_active` is non-empty. Without that, the subagent would spawn with no usable KMS tools. Refusal message: `"/kms wrap-up <name> --fix: no KMS attached to this session. Run \`/kms use <name>\` first so KMS tools are registered."`

```rust
let prompt = compose_kms_linker_prompt(&name, &lint, &stale);
side_channel::spawn_side_channel(
    "kms-linker".to_string(),
    prompt,
    state.agent_factory.clone(),
    state.agent_defs.clone(),
    events_tx.clone(),
).await
```

CLI dispatch always emits the report; when `fix=true`, prints a "GUI-only" message (matches `/dream`'s precedent — heavy side-channel work belongs in the GUI surface).

### `/kms migrate <name> [--apply]` — schema chain

Parser produces `SlashCommand::KmsMigrate { name: String, apply: bool }`. Flag aliases: `--apply` / `--execute` / `--run` (execute), `--dry-run` / `--plan` (force dry-run, useful in scripts that re-process the same input).

GUI dispatch:

```rust
match kms::migrate(&k, !apply) {
    Ok(report) => {
        emit(format_migration_report(&name, &report));
        if apply { broadcast_kms_update(events_tx); }  // refresh sidebar
    }
    Err(e) => emit(format!("migrate failed: {e}")),
}
```

`shell_dispatch::format_migration_report` renders three shapes:
- empty `steps` → `"already at schema version <target> — nothing to migrate"`
- dry-run → step list + `"this was a dry-run preview. re-run with \`--apply\` to execute."`
- applied → step list + `"logged to log.md. /kms lint to verify."`

CLI mirrors GUI; the sidebar broadcast is GUI-only.

### `/kms challenge <name> <idea>` — pre-decision red-team

Parser produces `SlashCommand::KmsChallenge { name: String, idea: String }`. Same shape as `KmsDump` — splits the rest of the line on first whitespace into `<name>` and `<idea...>`. Multi-line text after the name preserved verbatim. Empty/whitespace-only idea or missing name → `Unknown` with `usage: /kms challenge <name> <idea...>`. Aliases: `redteam`.

`repl::build_kms_challenge_prompt(kms_name, idea)` composes the agent-facing prompt inline (matches `build_kms_dump_prompt`'s precedent of no template file). Embeds the user's position verbatim between markers, then declares the procedure:

1. Extract the key premises behind the position
2. `KmsSearch` for each premise — past failures, reversed decisions, risk flags, contradictions. Try synonyms and related concepts.
3. `KmsRead` matches that look substantive (full page, not just the matched line)
4. Produce structured analysis: **Your position**, **Counter-evidence from your vault** (with citations), **Blind spots**, **Verdict**

Hard rules baked into the prompt:
- **Don't be agreeable** — push back when the vault gives ammunition
- **Cite specific pages** so the user can re-read
- If no counter-evidence found, say so honestly — but search broadly first (synonyms, related concepts)
- **Read-only** — no `KmsWrite` / `KmsAppend` calls; the analysis is the entire output

Intercept lives in two places (mirroring `KmsDump` from §6):
- `repl.rs:run_repl` rewrite block — CLI path
- `shared_session::handle_line` rewrite block — GUI path

Both check the same two preconditions as `KmsDump` (M6.38.1 audit fix):
- `kms::resolve(&name).is_some()` — KMS exists on disk
- `state.config.kms_active.is_empty()` is **false** — KMS tools registered

If both pass, the rewrite happens and the agent turn fires. Either failure surfaces a clear error and skips the rewrite. The dispatch arm is otherwise unreachable in normal flow.

Why this design over a subagent: the work is bounded (search → analyze → report) and fits in one main-agent turn. The user wants the analysis inline, not in a side-channel bubble. No `KmsWrite` calls means the main agent's existing tool registry is sufficient — a subagent would add side-channel overhead for no benefit.

### `/kms reconcile <name> [<focus>] [--apply]` — auto-resolve contradictions

Parser produces `SlashCommand::KmsReconcile { name: String, focus: Option<String>, apply: bool }`. Order-insensitive flag parsing (matches `/kms migrate`); flag aliases: `--apply` / `--execute` (execute), `--dry-run` / `--plan` (force dry-run). Aliases: `resolve`.

GUI dispatch fires `kms-reconcile` subagent (see §15) as a side channel — same shape as `/kms wrap-up --fix`'s `kms-linker` dispatch, with the same two-precondition gate:

```rust
SlashCommand::KmsReconcile { name, focus, apply } => {
    let Some(_k) = kms::resolve(&name) else {
        emit("no KMS named '{name}'");
        return;
    };
    if state.config.kms_active.is_empty() {
        // M6.38.1 audit fix: subagent inherits parent's tool registry;
        // KMS tools register only when kms_active is non-empty.
        emit(format!(
            "/kms reconcile {name}: no KMS attached to this session. \
             Run `/kms use {name}` first so KMS tools are registered."
        ));
        return;
    }
    let prompt = compose_kms_reconcile_prompt(&name, focus.as_deref(), apply);
    side_channel::spawn_side_channel(
        "kms-reconcile".to_string(),
        prompt,
        state.agent_factory.clone(),
        state.agent_defs.clone(),
        events_tx.clone(),
    ).await
}
```

`shell_dispatch::compose_kms_reconcile_prompt(name, focus, apply)` builds the brief — KMS name + optional focus + mode (apply vs dry-run). The subagent's body has the four-pass procedure; the helper just hands over scope.

CLI emits `"/kms reconcile is only available in GUI mode (thclaws or thclaws --serve). It dispatches the built-in kms-reconcile agent as a side channel."` — same posture as `/dream`, `/kms wrap-up --fix` (heavy side-channel work belongs in the chat surface where streaming is visible).

Hard rules in the agent prompt: never silently delete a claim (every claim survives in `## History` or a `Conflict — ` page); recency markers + source URLs intact across rewrites; "user changed their mind" classifies as Evolution, not contradiction; never invent dates or sources. In dry-run mode, **no** `KmsWrite` / `KmsAppend` calls — produce the report describing what *would* change.

---

## 7. Tool surface (LLM-callable)

When at least one KMS is in `kms_active`, **five tools** register into the `ToolRegistry`:

| Tool | Approval | Purpose |
|---|---|---|
| `KmsRead` | No | Read a single page |
| `KmsSearch` | No | Two modes: `pattern:` (regex line grep, byte-identical pre-Tier-2 output) OR `query:` (BM25 ranked, requires `kms_search_index` feature). Optional `tags:` / `category:` / `limit:` filter the `query:` path. Mutually exclusive — `pattern` and `query` together return a clear error. See §"dev-plan/36 BM25 search architecture" below. |
| `KmsWrite` | **Yes** | Create or replace a page |
| `KmsAppend` | **Yes** | Append to a page |
| `KmsDelete` | **Yes** | Remove a page (last resort; framed as "prefer KmsWrite to merge or supersede" in the system-prompt Tools block) |

When `kms_active` empties (last `/kms off`), all five tools are removed from the registry so the model doesn't see stale affordances. See [`built-in-tools.md`](built-in-tools.md) §3 for the full input-schema definitions.

> M6.38.2 audit fix (Bug A): pre-fix the `/kms off` cleanup removed only Read/Search/Write/Append — `KmsDelete` had been added in M6.27 (`/dream` work) but never paired with a remove. The `kms_active.is_empty()` branch in `shell_dispatch.rs` now removes all five.

### Sandbox carve-out (M6.25 BUG #1)

`KmsWrite` and `KmsAppend` deliberately bypass `Sandbox::check_write`. Rationale: project-scope KMS lives at `.thclaws/kms/.../pages/...` which the sandbox blocks (the `.thclaws/` reserved-dir rule). User-scope KMS lives at `~/.config/thclaws/kms/...` which is also outside any project root.

Path safety is enforced at finer grain via `kms::writable_page_path` instead:
- Reject `..`, path separators, control chars, absolute paths, reserved stems
- Canonicalize the parent dir inside `pages_dir` (symlink-escape defeated)
- Refuse if `pages/` itself is a symlink

Same intentional carve-out pattern as `TodoWrite`'s `.thclaws/todos.md` write — clear precedent in the codebase.

### Tool registration sites

Three places register the five tools when `!config.kms_active.is_empty()`:

- `shared_session::build_state` — GUI worker boot
- `shell_dispatch::dispatch` — `/kms use` arm (live-register so the next turn sees them)
- `repl::run_print_mode` and `repl::run_repl` — CLI boot, both modes

(Line numbers omitted — they drift as the codebase grows. Search for `KmsReadTool` registration to locate.)

`/kms off`'s dispatch arm removes them when `kms_active` becomes empty:

```rust
state.tool_registry.remove("KmsRead");
state.tool_registry.remove("KmsSearch");
state.tool_registry.remove("KmsWrite");
state.tool_registry.remove("KmsAppend");
state.tool_registry.remove("KmsDelete");
```

If a future change adds another KMS tool, both the registration paths AND this remove block need to be updated together.

---

## 8. Security model

### Path traversal defense

Every page-name input goes through one of:
- `KmsRef::page_path(page)` — used by `KmsRead`; canonicalizes the resolved file (must exist) and verifies it's under the KMS root
- `kms::writable_page_path(kref, page_name)` — used by `KmsWrite` / `KmsAppend`; canonicalizes the parent dir, verifies inside `pages_dir`, refuses symlinked `pages/`

Both reject before touching the filesystem: empty / `..` / `/` / `\` / `\0` / control chars / absolute paths / reserved stems (`index`, `log`, `SCHEMA`).

### Symlink defense (multi-layer)

| Vector | Defense |
|---|---|
| `~/.config/thclaws/kms/evil` is a symlink to `/etc` | `resolve()` uses `symlink_metadata` + `is_symlink()` check → refuses |
| `pages/` itself is a symlink to `/etc` | `KmsSearch` + `writable_page_path` refuse via `symlink_metadata` |
| `pages/leak.md` is a symlink to `~/.ssh/id_rsa` | `KmsSearch` skips entries where `entry.file_type().is_symlink()`; `KmsRead` rejects via `page_path`'s canonicalize-then-verify-under-root check |

The `system_prompt_section` injection also refuses to read `index.md` / `SCHEMA.md` if they are symlinks (`read_index` + `read_text_capped` both check `symlink_metadata`).

### KMS-name validation

`kms::create(name, scope)` rejects names that contain `/`, `\`, `..`, `\0`, control chars, or start with `.` or that are absolute paths.

### Approval gating

`KmsWrite`, `KmsAppend`, and `KmsDelete` all set `requires_approval(_) = true` — same posture as `Write`. In `PermissionMode::Ask` (default), every call surfaces an approval modal showing the page path and a content preview (or the deletion target).

### Reserved aliases

`RESERVED_PAGE_STEMS = ["index", "log", "SCHEMA"]` — `ingest()`, `write_page`, `append_to_page` all refuse these (case-insensitive). Prevents accidental clobber of the seed files.

---

## 9. Obsidian compatibility

A KMS root opens cleanly as an Obsidian vault — pages, index, log, schema are all plain `.md` with valid YAML frontmatter:

1. Obsidian → "Open folder as vault" → `~/.config/thclaws/kms/<name>` (user) or `<project>/.thclaws/kms/<name>` (project — `.thclaws` is hidden, use the path bar).
2. Install **Dataview** plugin → query frontmatter:
   ```dataview
   LIST FROM "pages" WHERE category = "research"
   TABLE updated, sources FROM "pages" SORT updated DESC
   ```
3. Graph view shows edges from our standard `[label](pages/x.md)` links.

### Caveats vs hand-built Obsidian vault

- We emit standard markdown links, not wikilinks (`[[x]]`). Both render in graph view, but markdown links don't auto-update if you rename a page from inside Obsidian. The LLM can write either form via `KmsWrite` — Obsidian renders both, and our `lint` link-detection regex only catches markdown form (so wikilinks won't trigger broken-link warnings — fine for cross-references between local-only pages).
- `tags:` is single-string in our frontmatter (`tags: a, b`). Obsidian/Dataview also support list form (`tags: [a, b]`). If you want list form, write that via `KmsWrite` — our parser keeps the raw string and Dataview parses it correctly.

### Mutual coexistence

Obsidian creates `.obsidian/` config inside the vault root. KMS code never reads it. KMS creates `pages/`, `sources/`, `index.md`, `log.md`, `SCHEMA.md`. Obsidian renders all of those as regular files. No conflicts.

---

## 10. Cross-process behavior

### Concurrent reads

Multiple processes (CLI + GUI in same project, two GUIs) can read freely — `KmsRead`, `KmsSearch`, and `system_prompt_section` only `std::fs::read_to_string`.

### Concurrent writes

`KmsWrite`, `KmsAppend`, `ingest()` use plain `std::fs::write` / `OpenOptions::append` — **no file locking**. Same posture as the rest of `.thclaws/` reserved files (`todos.md`, plan_state). Last-writer-wins on `pages/<x>.md` and `index.md`. The `log.md` append uses `OpenOptions::append` which is per-write atomic for ≤ PIPE_BUF (~4KB); log entries are small headers so this is safe in practice.

If you run heavy concurrent edits across processes, add file locking via `fs2::FileExt::lock_exclusive` (the M6.24 sessions pattern) — not currently warranted by the access pattern.

---

## 11. Configuration

### `.thclaws/settings.json`

Active KMS list persists per-project:

```json
{
  "kms_active": ["mynotes", "team-wiki"]
}
```

Mutated only via `ProjectConfig::set_active_kms(Vec<String>)`, called by `/kms use` and `/kms off`. The list is consumed at:
- Worker boot (`shared_session.rs::build_state`) → registers KMS tools
- Every `kms::system_prompt_section(&config.kms_active)` call → builds the prompt block

### Settings layering (per [`config.rs`](../crates/core/src/config.rs))

`kms_active` is a project-scope-only setting — there's no user-scope or compiled-in default. New projects start with `kms_active: []` (no KMSes attached, even if the user has user-scope KMSes available).

---

## 12. Code organization

```
crates/core/src/
├── kms.rs (~2100 LOC)                  ── core: KmsRef, KmsManifest, scopes, create/resolve,
│                                          ingest + ingest_url + ingest_pdf,
│                                          write_page + append_to_page,
│                                          parse/write_frontmatter,
│                                          lint + LintReport (six categories),
│                                          system_prompt_section + categorized index,
│                                          mark_dependent_pages_stale + scan_stale_markers,
│                                          Migration + migrations() + migrate +
│                                          detect_schema_version + MigrationReport
├── tools/
│   └── kms.rs (430 LOC)                ── KmsRead, KmsSearch, KmsWrite, KmsAppend, KmsDelete
├── shell_dispatch.rs (selected lines)  ── /kms slash handlers (GUI), format_lint_report,
│                                          format_wrap_up_report, format_migration_report,
│                                          has_actionable_issues, compose_kms_linker_prompt,
│                                          compose_kms_reconcile_prompt,
│                                          sanitize_alias_for_dispatch
├── repl.rs (selected lines)            ── SlashCommand::Kms* enum + parser + CLI dispatch,
│                                          build_kms_ingest_session_prompt + build_kms_dump_prompt +
│                                          build_kms_challenge_prompt
├── shared_session.rs (selected lines)  ── KMS tool registration at worker boot,
│                                          KmsIngestSession + KmsDump + KmsChallenge turn-rewrite
│                                          intercepts
├── config.rs (selected lines)          ── kms_active persistence
├── default_prompts/
│   ├── kms-linker.md                   ── built-in subagent (registered in agent_defs::seed_builtins)
│   └── kms-reconcile.md                ── built-in subagent (registered in agent_defs::seed_builtins)
└── agent_defs.rs (selected lines)      ── BUILTINS array includes "kms-linker", "kms-reconcile"
```

`tools/kms.rs` keeps a process-wide test env-lock (`test_env_lock`) shared with `kms.rs`'s test module — both mutate `HOME` + cwd to scope test KMSes to a tempdir, which would race without the lock.

---

## 13. Testing

| Module | Tests | Coverage |
|---|---|---|
| `kms::tests` | 50 | create/resolve, scope precedence, traversal/symlink rejection, ingest split, ingest collision, frontmatter round-trip, write_page (new + replace + index dedup), append_to_page, writable_page_path, lint (six categories incl. `missing_required_fields`), system_prompt_section (schema injection + categorized index + **KmsDelete listed in Tools block** post-M6.38.2), re-ingest cascade, manifest read/seed/legacy/malformed, scan_stale_markers (cascade-end-to-end + multiple-per-page), schema migration (legacy detection, dry-run no-op, apply, idempotent, page preservation, unknown-version error) |
| `tools::kms::tests` | 11 | read round-trip, missing extension fallback, unknown KMS, search semantics, write tool (create + traversal + unknown KMS), append tool (create + extend), approval-gating posture |
| `repl::tests` (KMS-only) | 18 | `/kms dump` parser (text capture, `capture` alias, missing-text + missing-name rejects), `build_kms_dump_prompt` shape, `/kms challenge` parser + `redteam` alias + missing-rejects + `build_kms_challenge_prompt` shape (5 tests), `/kms reconcile` parser + `resolve` alias + flag handling + missing-rejects (6 tests), `/schedule preset` parser (5 tests) |
| `agent_defs::tests` (KMS-only) | 2 | `seed_builtins_includes_kms_linker` + `seed_builtins_includes_kms_reconcile` — both assert tool whitelists (no `KmsDelete`, no `Bash`) and presence of procedure-defining keywords in the body |

Tests use `scoped_home()` (drops on test end via `EnvGuard`) to set `HOME` + `USERPROFILE` + cwd to a fresh tempdir. Every test that mutates env acquires `test_env_lock` to serialize against parallel test execution.

Symlink-rejection tests are `#[cfg(unix)]` only (Windows symlinks need extra permissions); Windows is excluded from CI tests anyway (per `CLAUDE.md`).

---

## 14. Migration / known limitations

### Backwards compatibility

Pre-M6.25 KMSes (no frontmatter, no source/page split) load and read fine:
- `system_prompt_section` falls back to raw `index.md` rendering when no page has frontmatter
- `KmsRead` reads any `pages/*.md` regardless of frontmatter presence
- `KmsSearch` greps any `pages/*.md`

Re-ingesting an old file with `--force` produces the new split shape (raw → `sources/`, stub → `pages/`). One-shot upgrade per source.

Pre-manifest KMSes (no `manifest.json`) are detected as `LEGACY_SCHEMA_VERSION = "0.x"` by `detect_schema_version`:
- `lint` skips `missing_required_fields` silently (the rules map comes from the manifest)
- `KmsRead` / `KmsSearch` / `KmsWrite` / `KmsAppend` are unaffected — manifest is purely advisory
- `read_manifest()` returns `None` for: file absent, path is a symlink, malformed JSON

Cure: `/kms migrate <name> --apply` runs the `0.x → 1.0` step (writes `manifest.json` with empty enforcement, appends `migrated | 0.x → 1.0` to `log.md`). Idempotent; re-runs are no-ops at latest version.

### M6.25 changes (`dev-log/143`)

10 of 11 audit issues from `docs/llm-wiki.md` gap analysis shipped:
- BUG #1 — `KmsWrite` + `KmsAppend` tools (sandbox carve-out)
- BUG #2 — Source/page split in `ingest()`
- BUG #3 — `/kms lint` health check
- BUG #4 — `/kms file-answer` files latest assistant message
- BUG #5 — `SCHEMA.md` injected into system prompt
- BUG #6 — Categorized index by frontmatter `category:`
- BUG #7 — Log format → `## [date] verb | alias` (greppable)
- BUG #8 — URL ingest (HTTP) + PDF ingest (`pdftotext`)
- BUG #9 — YAML frontmatter parser/serializer
- BUG #10 — Re-ingest cascade marks dependent pages stale

### M6.18 fix (`dev-log/136`)

BUG M7 — `system_prompt_section` index cap. Three active KMSes each with an 80K index used to burn 240K tokens of system prompt every turn. Now capped at 200 lines / 25 KB per KMS via `crate::memory::truncate_for_prompt`. Schema injection (M6.25) reuses the same pattern with 100 lines / 5 KB.

### Deferred (not yet shipped)

- **BUG #11** — `qmd` hybrid search (BM25 + vector + LLM rerank). External dep ([github.com/tobi/qmd](https://github.com/tobi/qmd)), opt-in. Could ship as a `KmsSearchHybrid` tool that shells out to `qmd` if installed; not blocking core llm-wiki alignment.

### Known limitations

- **No file locking** on concurrent KMS writes from multiple processes (§10). Last-writer-wins. Not currently a footgun given the access pattern.
- **`tags:` single-string only** in our frontmatter parser. Obsidian list form (`tags: [a, b]`) is preserved verbatim but treated as one string by our `lint` (which doesn't query tags) and the categorized-index renderer (which only reads `category`).
- **Inbound link detection** in `lint` only matches `[label](pages/x.md)` markdown form. Wikilinks (`[[x]]`) won't trigger broken-link warnings; orphan detection won't credit them as inbound either. Acceptable since our generated pages use markdown form.
- **No `qmd` integration** — at scale (>500 pages), `KmsSearch` regex + read may be slow.

### Sprint chronology

- **M6.18** — system-prompt index cap (BUG M7) — `dev-log/136`
- **M6.25** — llm-wiki concept alignment, 10 of 11 audit issues — `dev-log/143`
- **M6.37** — KMS extensions: manifest + schema-aware lint (`dev-log/157`), `/kms wrap-up` + `scan_stale_markers` (`dev-log/158`), `/kms migrate` (`dev-log/159`), `kms-linker` subagent (`dev-log/160`), `/kms dump` (`dev-log/161`)
- **M6.38** — KMS extensions inspired by obsidian-second-brain: `/kms challenge` (`dev-log/166`), `/kms reconcile` + `kms-reconcile` subagent (`dev-log/167`), pre-packaged schedule presets (`dev-log/168`)
- **M6.38.1** — KMS integration audit fixes (`dev-log/169`): preset prompts rewritten as natural-language tool directives (slash commands don't work in `--print` mode), `kms_active.is_empty()` guards on subagent dispatch, wikilink → markdown link wording in reconcile/linker prompts, test isolation hook for `add_from_preset`
- **M6.38.2** — KMS technical-manual audit fixes (`dev-log/170`): two code bugs found while auditing the docs — Bug A: `/kms off` didn't remove `KmsDelete` (registered in M6.27 but never paired with a remove); Bug B: system-prompt Tools block omitted `KmsDelete` (model had access via registry but no narrative context). Both fixed; technical manual brought up to date for M6.37/M6.38 surface area, line numbers replaced with function names, kms-linker description quote synced post-M6.38.1.

---

## 15. Built-in subagents

Three KMS-related agent definitions are compiled into the binary alongside `dream` and `translator`, registered via `agent_defs::AgentDefsConfig::seed_builtins`:

```rust
const BUILTINS: &[(&str, &str)] = &[
    ("dream", include_str!("default_prompts/dream.md")),
    ("translator", include_str!("default_prompts/translator.md")),
    ("kms-linker", include_str!("default_prompts/kms-linker.md")),
    ("kms-reconcile", include_str!("default_prompts/kms-reconcile.md")),
];
```

User overrides at `.thclaws/agents/<name>.md` (project) or `~/.config/thclaws/agents/<name>.md` (user) win over built-ins on name collision — same rule as `dream`.

### `kms-linker` — targeted fixes for one KMS

Frontmatter contract (declared in `default_prompts/kms-linker.md`):

```yaml
---
name: kms-linker
description: Fix broken markdown page links, refresh STALE pages, and patch missing index entries in a thClaws KMS
tools: KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite
permissionMode: auto
maxTurns: 80
color: cyan
---
```

> M6.38.1 fix: pre-fix the description said "Fix broken **wikilinks**" — but thClaws's `kms::lint::broken_links` regex (`\(pages/([^)]+?)\.md\)`) only catches markdown form, so `[[wikilinks]]` are invisible to lint. Description corrected to "markdown page links" so the agent's framing matches the lint surface it's acting against.

Tool whitelist is **strictly narrower than `dream`** — no `KmsDelete`, no `Bash`, no `Read`/`Glob`/`Grep`. The agent works only on the report `compose_kms_linker_prompt` hands it; it doesn't read sessions or external files.

Invocation path (GUI-only, fired from `/kms wrap-up <name> --fix`):

```rust
let prompt = compose_kms_linker_prompt(&name, &lint, &stale);
side_channel::spawn_side_channel(
    "kms-linker".to_string(),
    prompt,
    state.agent_factory.clone(),
    state.agent_defs.clone(),
    events_tx.clone(),
).await
```

The composed prompt embeds the lint report (broken_links, missing_in_index, missing_required_fields, orphan_pages — orphans flagged "do NOT modify, list in final report") and the stale-marker list. Agent's operating procedure (encoded in the .md body):

| Lint category | Agent action |
|---|---|
| Broken link `(page → target)` | `KmsSearch` for the target stem; one strong match → rewrite the link, otherwise defer |
| Stale page `(stem, alias, date)` | `KmsRead` the source stub + the stale page; rewrite preserving structure, drop the `> ⚠ STALE` line |
| Missing-in-index | `KmsAppend` a one-line bullet to `index.md` under the matching category section |
| Missing required field | Fill only when derivable from page body or sources; else defer |
| Orphan page | Don't act — list in final report |

Final-report contract: `**Fixed**` block (every change with KMS:page identifier) followed by `**Skipped (need human judgment)**` block (every defer with reason).

`/kms wrap-up --fix` only fires `spawn_side_channel` when `has_actionable_issues` returns true (`broken_links ∪ missing_in_index ∪ missing_required_fields ∪ stale ≠ ∅`). Clean state path emits `"/kms wrap-up --fix: nothing actionable for kms-linker; skipping dispatch."` and returns without spawning.

CLI emits `"/kms wrap-up --fix is only available in GUI mode (thclaws or thclaws --serve). It dispatches the built-in kms-linker agent as a side channel."` — same precedent as `/dream` (heavy side-channel work belongs in the chat surface).

### `kms-reconcile` — auto-resolve contradictions across pages

Frontmatter contract (declared in `default_prompts/kms-reconcile.md`):

```yaml
---
name: kms-reconcile
description: Find and resolve contradictions across pages in a thClaws KMS. Rewrites outdated pages with History sections, flags ambiguous cases as Conflict pages.
tools: KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite
permissionMode: auto
maxTurns: 120
color: orange
---
```

Same tool whitelist as `kms-linker` — KMS read/write surface plus `TodoWrite` for progress tracking. Critically **no `KmsDelete`** — reconcile preserves every original claim, either in a `## History` section appended to the rewritten page or in a freshly-written `Conflict — <topic>.md` page. No `Bash`, no `Read`/`Glob`/`Grep`; the agent works only on what's already in the KMS.

Invocation path (GUI-only, fired from `/kms reconcile <name> [<focus>] [--apply]`):

```rust
let prompt = compose_kms_reconcile_prompt(&name, focus.as_deref(), apply);
side_channel::spawn_side_channel(
    "kms-reconcile".to_string(),
    prompt,
    state.agent_factory.clone(),
    state.agent_defs.clone(),
    events_tx.clone(),
).await
```

`compose_kms_reconcile_prompt(name, focus, apply)` builds the brief — KMS name, optional focus narrowing, and a mode clause that switches between dry-run and apply. The subagent's body declares the four-pass procedure; the helper just hands over scope.

Agent's operating procedure (encoded in the .md body):

| Pass | Detects |
|---|---|
| **Claims** | Concept and project pages with overlapping factual claims that disagree (different numbers, dates, facts about the same thing) |
| **Entities** | Entity pages where role / company / title / relationship has drifted (e.g., `Person.md` says "role: X at Y" but a later daily note says "former role: X") |
| **Decisions** | Decision pages contradicted by later pages without an explicit `supersedes:` link |
| **Source-freshness** | Wiki pages whose `sources:` cite old sources when newer sources on the same topic exist in the KMS |

Per finding, classify and act:

| Classification | Action |
|---|---|
| **Clear winner** — newer + more authoritative side (peer-reviewed > article > transcript > opinion) | Rewrite the older page; **append** (don't replace) a `## History` section preserving the change with reason |
| **Genuinely ambiguous** — both sides have evidence | Create `Conflict — <topic>.md` with `status: open`, both positions documented, evidence cited; link to the original conflicting pages so the graph surfaces it |
| **Evolution** — user changed their mind (not a contradiction) | Update the entity/concept page with current state, add (or extend) a `## Timeline` section showing how thinking progressed |

Hard rules baked into the prompt:
- **Preserve every original claim** somewhere — `## History`, `Conflict — ` page, or `## Timeline`. Never silently delete.
- **Recency markers and source URLs intact** across rewrites — `(as of 2026-04, mem0.ai/blog/series-a)` style stays.
- **"Someone changed their mind" is not a contradiction** — classify as Evolution.
- **Don't invent dates or sources.**
- **Dry-run mode**: produce the report describing what would change, but make NO `KmsWrite` / `KmsAppend` calls.

Final-report contract: three blocks — `**Auto-resolved**`, `**Flagged for user**` (Conflict pages), `**Stale pages updated**` (rewrites with fresher sources). Each block lists per-page entries; empty blocks show as `(none)`.

CLI emits the same `"GUI-only — dispatches the built-in kms-reconcile agent as a side channel."` message as `/kms wrap-up --fix`. Heavy reconciliation work spans many tool calls and benefits from the streaming chat surface.

### Differences from `kms-linker`

| | `kms-linker` | `kms-reconcile` |
|---|---|---|
| Trigger | `/kms wrap-up <name> --fix` | `/kms reconcile <name> [--apply]` |
| Input shape | `LintReport` + `Vec<StaleEntry>` (pre-computed by Rust) | Just KMS name + focus (the agent does its own scanning) |
| Detection authority | Rust (lint + scan_stale_markers) | LLM (four parallel passes against page bodies) |
| `KmsDelete` allowed | No | No |
| Preserves history | Bullet rewrites in place | `## History` / `## Timeline` / Conflict page |
| Default mode | Always acts on the report | Dry-run by default; `--apply` to execute |
| Produces "Conflict" pages | No | Yes (for genuinely-ambiguous findings) |

The split is deliberate. `kms-linker` is *deterministic* — the lint report is generated mechanically and the agent acts on each entry. `kms-reconcile` is *judgment-driven* — every contradiction needs LLM evaluation (which side is more authoritative? is this evolution or contradiction?). Different jobs; same architectural seam.

---

## dev-plan/36 BM25 search architecture

Tier 1–3 of dev-plan/36 add a tantivy-backed BM25 index sibling
to the existing regex line-grep, exposed via `KmsSearch`'s new
`query:` argument. Layout, write-path wiring, tokenizer, and
manifest are documented here.

### On-disk layout

```text
<kms_root>/
├── pages/              ← source of truth (markdown)
├── .index/             ← tantivy index dir (`kms_search_index` owns)
│   ├── meta.json       ← tantivy's own metadata
│   ├── *.fast / *.idx  ← tantivy segments
│   └── manifest.json   ← thClaws metadata: {index_version,
│                          last_full_rebuild_at}
└── .index/vectors/     ← reserved for a future semantic-search
                          dev-plan; never touched in Tier 1–3
```

Reserving `.index/vectors/` as a sibling means a future semantic-
search PR can land without disk-schema migration.

### Schema + field boosts

| Field    | Type | Stored | Indexed   | Boost (query-time) |
|----------|------|--------|-----------|--------------------|
| page     | text | yes    | raw       | — (identity)       |
| title    | text | yes    | tokenized | 4.0                |
| topic    | text | yes    | tokenized | 2.0                |
| body     | text | no     | tokenized | 1.0                |
| tags     | text | yes    | raw, multi| — (filter)         |
| category | text | yes    | raw       | — (filter)         |
| sources  | text | no     | raw, multi| — (filter)         |
| updated  | i64  | yes    | INDEXED + FAST | — (Tier 4 recency boost) |

Body is **indexed but NOT stored** — page content lives on disk;
duplicating it into the index would double disk usage. Snippet
generation in `tools/kms.rs::format_hits` re-reads page bodies from
disk for the top-K hits only.

Field boosts are applied **at query time** via `QueryParser::
set_field_boost`, not baked into the schema. This lets us revisit
the boost values (or make them config-driven) without rebuilding
existing indexes.

### Tokenizer

Custom `ThaiOrEnglishTokenizer` (in `kms_search_index.rs`) splits
input by script:

- ASCII whitespace + punct → separators
- ASCII alphanumeric run → one token (English identifiers + numbers)
- Non-ASCII run → segmented via `crate::thai::Segmenter`
  (newmm-style maximum matching over an `fst::Set` Thai dict)

Token filter chain: `LowerCaser` only. English stemming is
intentionally NOT in the pipeline — applying en-stemmer to Thai
tokens mangles them, and per-token language detection costs more
than it's worth for BM25 indexing.

The Thai segmenter is documented in `crates/core/src/thai/` —
see the module's `//!` docs for the algorithm + the
`scripts/build_thai_dict/README.md` for the Wiktionary dictionary
pipeline.

### Write-path wiring

`crates/core/src/kms.rs` fires `kms_search_index::on_page_mutated`
after every successful page mutation:

| KMS fn          | Op                              |
|-----------------|---------------------------------|
| `write_page`    | `Upsert`                        |
| `append_to_page`| `Upsert` (re-reads whole page) |
| `delete_page`   | `Delete`                        |
| `rename_page`   | `Delete(old)` + `Upsert(new)`   |
| `merge_into`    | NOT wired (per-page) — relies on auto-rebuild-on-stale or `/kms reindex` |
| `auto_link`     | NOT wired (per-page) — same     |

`merge_into` + `auto_link` mutate many pages in one call; wiring
per-page hooks there would require threading the affected-page set
through their internals. The auto-rebuild-on-stale path catches
them; operators can also `/kms reindex <name>` after a bulk op.

### Concurrency: per-kms-root SearchIndex registry

Tantivy's `IndexWriter` holds a directory-level lock. Naive
"open a fresh index per mutation" collides on `LockBusy`.
`kms_search_index::registry()` is a process-wide
`OnceLock<Mutex<HashMap<PathBuf, Arc<SearchIndex>>>>` that hands
out one cached `Arc<SearchIndex>` per `kms_root` for the process
lifetime. The `SearchIndex`'s internal writer `Mutex` serialises
concurrent upsert/delete cleanly. `drop_cached(kms_root)` is the
explicit eviction handle used by `full_rebuild` (which deletes
`.index/` and needs to reopen).

### Auto-build-on-stale manifest

`<kms_root>/.index/manifest.json`:

```json
{
  "index_version": 1,
  "last_full_rebuild_at": 1717200000
}
```

On every `KmsSearch(query: …)`, `tools/kms.rs::kms_search_query_path`
reads the manifest. If absent OR `index_version` doesn't match the
current binary's `kms_search_index::INDEX_VERSION` const, calls
`full_rebuild` before serving the query + emits an
`[index rebuilt — N page(s) indexed]` advisory. Bump
`INDEX_VERSION` whenever a non-backward-compatible schema or
tokenizer change ships; users get a one-time rebuild on first
search after the upgrade.

### Feature gating: `kms_search_index`

Per dev-plan/36 D3 the feature is **opt-in forever** at the Cargo
level (`default = []`; `kms_search_index = ["dep:tantivy",
"dep:fst"]`). Adds ~4-5 MB to the binary.

Build wiring:

- `Makefile` `build-cli` / `build-app` add `--features
  kms_search_index` so day-to-day local builds match released
  binaries.
- `.github/workflows/release.yml` adds the feature to the matrix
  that produces shipped binaries.
- `.github/workflows/ci.yml` runs a second job with
  `--features kms_search_index` (build + test) to prevent
  bitrot in the BM25 path; the default `--features gui` job
  catches the regex-only path.
- `cargo install thclaws-core` defaults to OFF; users opt in via
  `--features kms_search_index`.
- `KmsSearch(query: …)` from a feature-off binary returns a clear
  "feature not enabled, use `pattern:` instead" error.

### `/kms reindex <name>` slash command

`SlashCommand::KmsReindex(name)`; handlers in `repl.rs` (CLI) +
`shell_dispatch.rs` (GUI / `--serve`). Drops `.index/` and calls
`full_rebuild`. Operator-only (no `KmsReindex` model-callable
tool — the auto-build-on-stale path covers self-healing).
