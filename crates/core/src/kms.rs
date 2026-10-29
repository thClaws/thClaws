//! Knowledge Management System (KMS) — Karpathy-style LLM wikis.
//!
//! A KMS is a directory of markdown pages plus an `index.md` table of
//! contents and a `log.md` change history. Two scopes:
//!
//! - **User**: `~/.config/thclaws/kms/<name>/`
//! - **Project**: `<workspace>/.thclaws/state/kms/<name>/` — shared by every
//!   agent in the workspace (dev-plan/64 D1)
//!
//! Users mark any subset of KMS as "active" in `.thclaws/settings.json`'s
//! `kms.active` array. When a chat turn runs, each active KMS's
//! `index.md` is concatenated into the system prompt, and the
//! `KmsRead` / `KmsSearch` tools let the model pull in specific pages
//! on demand. No embeddings, no vector store — just grep + read, per
//! Karpathy's pattern.
//!
//! Layout of a KMS directory:
//!
//! ```text
//! <kms_root>/
//!   index.md     — table of contents, one line per page (model reads this)
//!   log.md       — append-only change log (human and model write here)
//!   SCHEMA.md    — optional: shape rules for pages (not enforced in code)
//!   pages/       — individual wiki pages, one per topic
//!   sources/     — raw source material (URLs, PDFs, notes) — optional
//! ```

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KmsScope {
    User,
    Project,
    /// Read-only KMS mounted from a shared-agent brain
    /// (`$THCLAWS_SHARED_AGENT_DIR/kms`). See dev-plan/41. Never written.
    Shared,
}

impl KmsScope {
    pub fn as_str(self) -> &'static str {
        match self {
            KmsScope::User => "user",
            KmsScope::Project => "project",
            KmsScope::Shared => "shared",
        }
    }
}

/// A KMS instance — its scope, name, and root directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmsRef {
    pub name: String,
    pub scope: KmsScope,
    pub root: PathBuf,
}

impl KmsRef {
    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.md")
    }

    pub fn log_path(&self) -> PathBuf {
        self.root.join("log.md")
    }

    pub fn pages_dir(&self) -> PathBuf {
        self.root.join("pages")
    }

    pub fn sources_dir(&self) -> PathBuf {
        self.root.join("sources")
    }

    pub fn schema_path(&self) -> PathBuf {
        self.root.join("SCHEMA.md")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    /// True for a KMS mounted read-only from a shared-agent brain
    /// (dev-plan/41). Write tools (`KmsWrite`/`KmsAppend`/`KmsDelete`/
    /// ingest/auto-learn) must refuse when this is set — members fork to
    /// edit. Reads (`KmsRead`/`KmsSearch`) are unaffected.
    pub fn read_only(&self) -> bool {
        self.scope == KmsScope::Shared
    }

    /// Read `index.md`. Returns `""` (not an error) when the file is absent,
    /// OR when the path is a symlink (refused to prevent a cloned KMS
    /// with `index.md -> /etc/passwd` from exfiltrating through the
    /// system prompt). A fresh KMS with no entries yet is a valid state.
    pub fn read_index(&self) -> String {
        let path = self.index_path();
        if let Ok(md) = std::fs::symlink_metadata(&path) {
            if md.file_type().is_symlink() {
                return String::new();
            }
        }
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Read `manifest.json`. Returns `None` when the file is absent (legacy
    /// KMS predating manifests is a valid state), when the path is a symlink
    /// (same exfiltration concern as `read_index`), or when the JSON fails
    /// to parse (treat malformed as absent rather than poisoning lint).
    pub fn read_manifest(&self) -> Option<KmsManifest> {
        let path = self.manifest_path();
        if let Ok(md) = std::fs::symlink_metadata(&path) {
            if md.file_type().is_symlink() {
                return None;
            }
        }
        let raw = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Resolve a page name to a file path inside `pages/`. `.md` is added
    /// if missing. Returns an error if the resolved path escapes the KMS
    /// directory via `..`, an absolute path, path separators, null bytes,
    /// or symlink trickery (e.g. `pages/` itself symlinked outside, or a
    /// page file symlinked to `/etc/passwd`).
    pub fn page_path(&self, page: &str) -> Result<PathBuf> {
        // Reject obviously-bad names before touching the filesystem.
        if page.is_empty()
            || page.contains("..")
            || page.contains('/')
            || page.contains('\\')
            || page.contains('\0')
            || page.chars().any(|c| c.is_control())
            || Path::new(page).is_absolute()
        {
            return Err(Error::Tool(format!(
                "invalid page name '{page}' — no '..', path separators, or control chars"
            )));
        }
        let name = if page.ends_with(".md") {
            page.to_string()
        } else {
            format!("{page}.md")
        };
        let candidate = self.pages_dir().join(&name);

        // Canonicalize the scope root and require the candidate to resolve
        // *within* this specific KMS directory under it. This defeats
        // symlink bypasses: if `pages/` or the page file itself is a
        // symlink pointing outside, the canonical candidate escapes the
        // KMS root and we reject.
        let canon_candidate = std::fs::canonicalize(&candidate).map_err(|e| {
            Error::Tool(format!(
                "cannot resolve page path '{}': {e}",
                candidate.display()
            ))
        })?;
        let canon_kms_roots: Vec<PathBuf> = scope_roots(self.scope)
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .map(|p| p.join(&self.name))
            .collect();
        if canon_kms_roots.is_empty() {
            return Err(Error::Tool("kms scope root not resolvable".into()));
        }
        if !canon_kms_roots
            .iter()
            .any(|r| canon_candidate.starts_with(r))
        {
            return Err(Error::Tool(format!(
                "page '{page}' resolves outside the KMS directory — symlink escape rejected"
            )));
        }
        // Also require it's a regular file, not a directory.
        let meta = std::fs::metadata(&canon_candidate)
            .map_err(|e| Error::Tool(format!("cannot stat page '{page}': {e}")))?;
        if !meta.is_file() {
            return Err(Error::Tool(format!("page '{page}' is not a regular file")));
        }
        Ok(candidate)
    }
}

/// Optional per-KMS manifest at `<root>/manifest.json`. Declares the schema
/// version (for `/kms migrate` later) and required frontmatter fields per
/// page category (consumed by `lint`). Absent for legacy KMSes; new ones
/// seeded by `create()` get a v1.0 manifest with empty enforcement so
/// existing tests + workflows are unaffected and policy is opt-in.
///
/// `#[serde(default)]` on every field means future additions don't break
/// older manifests on read — they just take the field's default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct KmsManifest {
    #[serde(default)]
    pub schema_version: String,
    /// Keys: `"global"` (every page) or a category name (e.g. `"research"`).
    /// Values: required frontmatter field names. Lint flags any page whose
    /// `category:` matches a key but is missing one of the listed fields.
    #[serde(default)]
    pub frontmatter_required: std::collections::BTreeMap<String, Vec<String>>,
    /// The page a reader should land on. Recorded when the KMS's first
    /// page is created — whatever a vault starts with is what it is
    /// about — and settable with `/kms entry`. Absent means "infer it"
    /// (see [`entry_page`]), which is what every KMS did before this
    /// existed and what a KMS whose entry page was deleted goes back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

pub const KMS_SCHEMA_VERSION: &str = "1.0";

/// What a new KMS's `SCHEMA.md` says. A schema still equal to this is not
/// worth a line of system prompt — see [`system_prompt_section`].
pub const SCHEMA_TEMPLATE: &str = "# Schema\n\n\
         Describe the shape of pages in this KMS — required sections, naming\n\
         conventions, cross-link style.\n\
         \n\
         ## Canonical page shape\n\
         \n\
         Write frontmatter + body. `title:`, `topic:`, and `sources:` are\n\
         the three keys every page should carry. `KmsWrite` auto-injects\n\
         a `# {title}` heading between the frontmatter and the body when\n\
         the body doesn't already start with a `# heading`.\n\
         \n\
         ```\n\
         ---\n\
         title: Human-readable title\n\
         topic: One-line description of what this page covers\n\
         sources: [\"https://…\", \"session-XYZ\", \"memory\"]   # required: provenance\n\
         category: optional grouping for the index\n\
         tags: [optional, free-form]\n\
         ---\n\
         \n\
         (body content)\n\
         ```\n\
         \n\
         `sources:` values: external URLs for web-sourced facts,\n\
         `session-<id>` for facts learned in a chat session, `memory`\n\
         for stable user-supplied context, or `[]` for opinion /\n\
         convention pages that genuinely have no external source\n\
         (still write the empty list — it's an explicit ack, not an\n\
         omission).\n\
         \n\
         Pages with no `verified:` frontmatter pick up a soft warning\n\
         when read; pages with `verified:` older than 90 days get a\n\
         staleness banner. The research pipeline stamps `verified:` on\n\
         every page it writes — manual `KmsWrite` callers can stamp it\n\
         too when they've checked the source against current reality.\n";

/// dev-plan/64 P3.2: replace a file in one step. `std::fs::write` truncates
/// and then writes, so a crash, a kill or a full disk between the two leaves
/// an empty or half-written page where a whole one was — and a reader in
/// another process (every agent in a workspace shares these files since D1)
/// can see that half. The bytes go to a sibling temp file, are flushed, and
/// are renamed over the target: a reader sees the old file or the new one.
///
/// The temp name starts with a dot and does not end in `.md`, so nothing
/// that lists `pages/` or `sources/` mistakes it for content.
pub(crate) fn write_file<P: AsRef<Path>, C: AsRef<[u8]>>(
    path: P,
    contents: C,
) -> std::io::Result<()> {
    use std::io::Write as _;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = path.as_ref();
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(
        ".{name}.tmp{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_ref())?;
        f.sync_all()?;
        // A page that was read-only, or 0600, stays that way.
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Hold a KMS's advisory lock for the length of `f`.
///
/// For the small JSON files that are read, changed and written back —
/// the source catalogue, the manifest. Since D1 every agent in a workspace
/// shares one vault, and two ingests finishing together each read the
/// catalogue, add their own record, and write: the second write silently
/// drops the first record. [`write_file`] makes each write whole; this
/// makes the read-change-write one step.
///
/// A lock file made with `create_new`, which is atomic on every platform.
/// A holder that died leaves it behind, so one older than [`LOCK_STALE`]
/// is taken over. Waiting is bounded: past [`LOCK_WAIT`] the work goes
/// ahead unlocked, because a lost catalogue row is recoverable
/// (`/kms reindex` rebuilds it) and a command that hangs is not.
pub(crate) fn with_kms_lock<T>(kref: &KmsRef, f: impl FnOnce() -> T) -> T {
    const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(30);
    let path = kref.root.join(".lock");
    let started = std::time::Instant::now();
    let mut held = false;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => {
                held = true;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if started.elapsed() > LOCK_WAIT {
                    eprintln!(
                        "[kms] '{}' stayed locked — going ahead without it",
                        kref.name
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            // No vault folder, read-only media: nothing to serialise against.
            Err(_) => break,
        }
    }
    let out = f();
    if held {
        let _ = std::fs::remove_file(&path);
    }
    out
}

fn user_root() -> Option<PathBuf> {
    crate::util::home_dir()
        .map(|h| h.join(format!(".config/{}/kms", crate::profile::app_dir_name())))
}

const PROJECT_KMS_DIR: &str = ".thclaws/state/kms";

/// dev-plan/64 D1: a project knowledge base belongs to the workspace, not
/// to the agent that happened to create it. Under a workspace host every
/// agent runs from its own folder, and the project root used to follow
/// the process cwd — so a vault built by one agent did not exist for the
/// next. Outside a host `workspace_root()` IS the cwd, so nothing moves.
fn project_root() -> PathBuf {
    crate::workdir::workspace_root().join(PROJECT_KMS_DIR)
}

/// Where this agent's project vaults lived before D1, when that is a
/// different place. After [`migrate_legacy_project_kms`] it holds only
/// what could not move — a vault whose name another agent's vault took
/// first — and that one stays reachable to the agent that owns it.
fn legacy_project_root() -> Option<PathBuf> {
    let legacy = std::env::current_dir().ok()?.join(PROJECT_KMS_DIR);
    let shared = project_root();
    let same = match (legacy.canonicalize(), shared.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => legacy == shared,
    };
    (!same && legacy.is_dir()).then_some(legacy)
}

/// The scope folders a user can create and drop bases in, project first.
pub fn writable_scope_roots() -> Vec<PathBuf> {
    [KmsScope::Project, KmsScope::User]
        .into_iter()
        .filter_map(scope_root)
        .collect()
}

/// Where new vaults in a scope are created.
fn scope_root(scope: KmsScope) -> Option<PathBuf> {
    match scope {
        KmsScope::User => user_root(),
        KmsScope::Project => Some(project_root()),
        KmsScope::Shared => crate::shared::shared_kms_root(),
    }
}

/// Every directory a scope's vaults are read from, highest priority
/// first. Only the project scope has more than one: an agent's own
/// unmoved vault shadows the workspace's same-named one, which is what
/// that agent saw before D1 and loses nothing.
fn scope_roots(scope: KmsScope) -> Vec<PathBuf> {
    if scope == KmsScope::Project {
        migrate_legacy_project_kms();
        let mut roots: Vec<PathBuf> = legacy_project_root().into_iter().collect();
        roots.push(project_root());
        return roots;
    }
    scope_root(scope).into_iter().collect()
}

/// Move every agent's project vaults up to the workspace. Runs once per
/// workspace per process, from whichever agent touches the KMS first.
///
/// Only ever a `rename`, and only onto a name that is free: a collision,
/// a cross-device move or a permission error leaves the vault exactly
/// where it was, still served through [`legacy_project_root`]. Agents
/// starting together may race for one name; `rename` onto a non-empty
/// directory fails, so the loser lands in the same "left in place" case.
fn migrate_legacy_project_kms() {
    static DONE: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let ws = crate::workdir::workspace_root();
    {
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        if done.contains(&ws) {
            return;
        }
        done.push(ws.clone());
    }
    for line in migrate_project_kms_in(&ws) {
        eprintln!("[kms] {line}");
    }
}

fn migrate_project_kms_in(ws: &Path) -> Vec<String> {
    let mut report = Vec::new();
    let Ok(bots) = std::fs::read_dir(ws.join(".thclaws/bots")) else {
        return report;
    };
    let mut bots: Vec<PathBuf> = bots.flatten().map(|e| e.path()).collect();
    // `main` is the workspace's own agent from before there were others;
    // when two agents hold the same name, its vault is the one that moves.
    bots.sort_by_key(|p| (p.file_name().map(|n| n != "main"), p.clone()));
    let target_root = ws.join(PROJECT_KMS_DIR);
    for bot in bots {
        let slug = bot
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let legacy = bot.join(PROJECT_KMS_DIR);
        let Ok(vaults) = std::fs::read_dir(&legacy) else {
            continue;
        };
        let mut vaults: Vec<_> = vaults.flatten().collect();
        vaults.sort_by_key(|e| e.file_name());
        for v in vaults {
            let name = v.file_name().to_string_lossy().into_owned();
            let is_vault = v.file_type().map(|t| t.is_dir() && !t.is_symlink());
            if name.starts_with('.') || !is_vault.unwrap_or(false) {
                continue;
            }
            let to = target_root.join(&name);
            if std::fs::symlink_metadata(&to).is_ok() {
                report.push(format!(
                    "'{name}' stays with agent '{slug}' — the workspace already has a knowledge base by that name. Rename one of them to share it."
                ));
                continue;
            }
            if let Err(e) = std::fs::create_dir_all(&target_root) {
                report.push(format!("'{name}' stays with agent '{slug}': {e}"));
                continue;
            }
            match std::fs::rename(v.path(), &to) {
                Ok(()) => {
                    report.push(format!(
                        "'{name}' moved from agent '{slug}' to the workspace — every agent here can use it now"
                    ));
                    let kref = KmsRef {
                        name,
                        scope: KmsScope::Project,
                        root: to,
                    };
                    let _ = append_log_header(
                        &kref,
                        "moved",
                        &format!("from agent '{slug}' to the workspace"),
                    );
                }
                Err(e) => report.push(format!("'{name}' stays with agent '{slug}': {e}")),
            }
        }
    }
    report
}

/// Enumerate KMS directories under one scope. Silently ignores missing
/// roots — fresh installs have neither. Symlinks are intentionally
/// skipped: a user can't turn a KMS directory into a symlink to `/etc`
/// and have thClaws enumerate it.
fn list_in(scope: KmsScope) -> Vec<KmsRef> {
    let mut out: Vec<KmsRef> = Vec::new();
    for root in scope_roots(scope) {
        for kref in list_dir(scope, &root) {
            if !out.iter().any(|k| k.name == kref.name) {
                out.push(kref);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn list_dir(scope: KmsScope, root: &Path) -> Vec<KmsRef> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        // symlink_metadata → file_type doesn't follow the symlink, so
        // a `ln -s /etc foo` sitting in the kms dir returns is_symlink.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        out.push(KmsRef {
            name,
            scope,
            root: entry.path(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Is there a knowledge base at all, in any scope? Cheaper than
/// [`list_all`] where it matters — it stops at the first one.
pub fn any_exists() -> bool {
    [KmsScope::Project, KmsScope::User, KmsScope::Shared]
        .into_iter()
        .any(|scope| !list_in(scope).is_empty())
}

/// List every KMS visible to this process — project entries first, then
/// user. If the same name exists in both scopes, both are returned;
/// callers that need to pick one treat project as higher priority.
pub fn list_all() -> Vec<KmsRef> {
    let mut out = list_in(KmsScope::Project);
    out.extend(list_in(KmsScope::User));
    // Read-only shared-agent KMS (dev-plan/41) — only present when shared
    // mode is active; listed last so a member's own same-named KMS wins.
    out.extend(list_in(KmsScope::Shared));
    out
}

/// Find a KMS by name. Project scope wins over user, then the read-only
/// shared-agent scope last (dev-plan/41) — a member's own same-named KMS
/// shadows the company one. Returns `None` when no KMS by that name
/// exists, or when the matching directory is a symlink (symlinks are
/// rejected to prevent `ln -s /etc <kms-name>` style exfiltration).
/// Find a KMS by name: the exact name first, and failing that, the one
/// KMS whose name reads the same once case, spaces and punctuation are
/// ignored — so `age-of-abundance`, `age_of_abundance` and
/// `AgeOfAbundance` all find "Age of Abundance".
///
/// A KMS has no slug; its name is its folder, exactly as typed at
/// creation. That made a name with a space awkward on every command line,
/// and it is how a model usually asks for one anyway (`kms:
/// "age-of-abundance"` → "no KMS named"). The returned [`KmsRef::name`]
/// is always the real folder name, never the spelling that found it, so
/// nothing downstream records a second name for the same base.
pub fn resolve(name: &str) -> Option<KmsRef> {
    resolve_exact(name).or_else(|| resolve_folded(name))
}

/// Loose lookup. Scope order matches [`resolve_exact`]. Two bases in one
/// scope that fold to the same key are ambiguous, and an ambiguous name
/// finds nothing rather than guessing which base to write into.
fn resolve_folded(name: &str) -> Option<KmsRef> {
    let want = fold_for_compare(name);
    if want.is_empty() {
        return None;
    }
    for scope in [KmsScope::Project, KmsScope::User, KmsScope::Shared] {
        // One name held in both of a scope's folders is one base, not two.
        let mut hits: Vec<String> = scope_roots(scope)
            .iter()
            .filter_map(|root| std::fs::read_dir(root).ok())
            .flat_map(|rd| rd.flatten())
            .filter(|e| {
                // Same rule as the exact path: a real directory, never a symlink.
                std::fs::symlink_metadata(e.path())
                    .map(|m| m.is_dir() && !m.is_symlink())
                    .unwrap_or(false)
            })
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.') && fold_for_compare(n) == want)
            .collect();
        hits.sort();
        hits.dedup();
        match hits.len() {
            0 => continue,
            1 => return resolve_exact(&hits.remove(0)),
            _ => {
                hits.sort();
                eprintln!(
                    "[kms] '{name}' is ambiguous — it matches {} — use the exact name",
                    hits.join(", ")
                );
                return None;
            }
        }
    }
    None
}

/// The name exactly as given. For callers that must not be helped — the
/// slash-command parser uses it to decide where a name ends.
pub fn resolve_exact(name: &str) -> Option<KmsRef> {
    // The name is often the model's: `../../skills` would land a "KMS"
    // in `.thclaws/` or, from user scope, anywhere under `$HOME`.
    validate_name(name).ok()?;
    for scope in [KmsScope::Project, KmsScope::User, KmsScope::Shared] {
        for root in scope_roots(scope) {
            let candidate = root.join(name);
            // symlink_metadata doesn't follow the symlink.
            let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
                continue;
            };
            if meta.is_symlink() || !meta.is_dir() {
                continue;
            }
            // On a case-insensitive disk (macOS by default) this lookup
            // succeeds for `AGE OF ABUNDANCE` too. Ask the filesystem what
            // the folder is really called, so the name and the path a
            // caller gets back are the base's own and not the spelling
            // that happened to find it — otherwise `/kms use age of
            // abundance` records a second name for a base already attached.
            let real = candidate
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| name.to_string());
            return Some(KmsRef {
                root: candidate.with_file_name(&real),
                name: real,
                scope,
            });
        }
    }
    None
}

/// Ensure a KMS exists when the caller did NOT pin a scope. Reuses an
/// existing same-named KMS in any scope (project > user > shared, per
/// `resolve`); otherwise creates it **project-scoped**. Knowledge bases
/// are per-workspace by design, so an unqualified create must never
/// silently land in user scope and shadow the project one as a
/// duplicate — that's the bug behind "two identical KMS entries". Paths
/// that DO pin a scope (`create(name, scope)`, `/kms new --user`) keep
/// their explicit behavior, so an intentional cross-scope same-name KMS
/// is still possible.
pub fn ensure_default(name: &str) -> Result<KmsRef> {
    if let Some(k) = resolve(name) {
        return Ok(k);
    }
    create(name, KmsScope::Project)
}

/// Create a new KMS. Seeds `index.md`, `log.md`, and `SCHEMA.md` with
/// minimal starter content so the model has something to read on day
/// one. No-op and returns `Ok(existing)` if a KMS by that name already
/// exists at the requested scope.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Config("kms name must not be empty".into()));
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
        || name.starts_with('.')
        || Path::new(name).is_absolute()
    {
        return Err(Error::Config(format!(
            "invalid kms name '{name}' — no path separators, '..', control chars, or leading '.'"
        )));
    }
    Ok(())
}

/// Viewer "Create page": turn the first plain occurrence of `text` in
/// `page`'s body into `[[slug|text]]`. Skips frontmatter, headings,
/// fenced code, and anything already inside a wikilink / markdown link
/// / inline code. Returns `false` (and leaves the page untouched) when
/// the phrase is not found in plain prose — the rendered selection may
/// have crossed a citation or a link.
pub fn link_phrase(kref: &KmsRef, page: &str, text: &str, slug: &str) -> Result<bool> {
    use regex::Regex;
    let path = kref.page_path(page)?;
    let original = std::fs::read_to_string(&path)
        .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
    let (fm_block, body) = split_frontmatter_block(&original);
    let protect_re = Regex::new(r"(?:\[\[[^\]\n]+\]\]|\[[^\]\n]+\]\([^)\n]+\)|`[^`\n]+`)")
        .expect("static regex");
    let restore_re = Regex::new(r"\u{0000}P(\d+)\u{0000}").expect("static regex");
    let mut out = String::with_capacity(body.len() + 32);
    let mut in_fence = false;
    let mut done = false;
    for line in body.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if done || in_fence || t.starts_with('#') {
            out.push_str(line);
            continue;
        }
        let mut placeholders: Vec<String> = Vec::new();
        let protected = protect_re.replace_all(line, |caps: &regex::Captures| {
            let idx = placeholders.len();
            placeholders.push(caps[0].to_string());
            format!("\u{0000}P{idx}\u{0000}")
        });
        let mut working = protected.into_owned();
        if let Some(pos) = working.find(text) {
            working.replace_range(pos..pos + text.len(), &format!("[[{slug}|{text}]]"));
            done = true;
        }
        let restored = restore_re.replace_all(&working, |caps: &regex::Captures| {
            let n: usize = caps[1].parse().unwrap_or(usize::MAX);
            placeholders
                .get(n)
                .cloned()
                .unwrap_or_else(|| caps[0].to_string())
        });
        out.push_str(&restored);
    }
    if !done {
        return Ok(false);
    }
    let mut full = String::with_capacity(fm_block.len() + out.len());
    full.push_str(fm_block);
    full.push_str(&out);
    write_page(kref, page, &full)?;
    Ok(true)
}

/// Rename a KMS directory in place (same scope). If the old name is
/// attached in project settings the attachment follows the rename, so
/// the next config reload resolves the new name instead of a dangling
/// one. Pages keep their content; wikilinks are slugs, not KMS names,
/// so nothing inside the KMS changes.
pub fn rename(old: &str, new: &str) -> Result<KmsRef> {
    validate_name(new)?;
    let kref = resolve(old).ok_or_else(|| Error::Tool(format!("KMS '{old}' not found")))?;
    if old == new {
        return Ok(kref);
    }
    let parent = kref
        .root
        .parent()
        .ok_or_else(|| Error::Tool(format!("KMS '{old}' has no parent directory")))?;
    let new_root = parent.join(new);
    if new_root.exists() {
        return Err(Error::Tool(format!(
            "a KMS named '{new}' already exists at {}",
            new_root.display()
        )));
    }
    drop_search_handle(&kref);
    drop_search_index(&kref);
    std::fs::rename(&kref.root, &new_root).map_err(|e| {
        Error::Tool(format!(
            "rename {} → {}: {e}",
            kref.root.display(),
            new_root.display()
        ))
    })?;
    let _ = crate::config::ProjectConfig::rename_attached_kms(old, new);
    let new_ref = KmsRef {
        name: new.to_string(),
        scope: kref.scope,
        root: new_root,
    };
    let _ = append_log_header(&new_ref, "rename", &format!("renamed from '{old}'"));
    Ok(new_ref)
}

pub fn create(name: &str, scope: KmsScope) -> Result<KmsRef> {
    validate_name(name)?;
    let root = scope_root(scope)
        .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?
        .join(name);
    if root.is_dir() {
        return Ok(KmsRef {
            name: name.to_string(),
            scope,
            root,
        });
    }
    std::fs::create_dir_all(root.join("pages"))?;
    std::fs::create_dir_all(root.join("sources"))?;
    let kref = KmsRef {
        name: name.to_string(),
        scope,
        root,
    };
    write_file(
        kref.index_path(),
        format!("# {name}\n\nKnowledge base index — list each page with a one-line summary.\n"),
    )?;
    write_file(
        kref.log_path(),
        "# Change log\n\nAppend-only list of ingests / edits / lints.\n",
    )?;
    write_file(
        kref.schema_path(),
        // Concise schema template (audit finding C): the previous
        // version duplicated the "Final on-disk shape" example, which
        // the model never needs to author (the tool stamps it on
        // write). Showing only the input shape saves ~300 bytes per
        // KMS in the system prompt. Human authors editing this file
        // directly can extend it with project-specific conventions.
        SCHEMA_TEMPLATE,
    )?;
    let manifest = KmsManifest {
        schema_version: KMS_SCHEMA_VERSION.into(),
        frontmatter_required: std::collections::BTreeMap::new(),
        entry: None,
    };
    write_file(
        kref.manifest_path(),
        serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".into()),
    )?;
    Ok(kref)
}

/// Extensions a user can ingest into a KMS. Deliberately narrow: these
/// are the text formats `KmsRead` can hand to the model meaningfully,
/// and that a human would expect to grep with `KmsSearch`. Binary
/// formats (PDF, images, archives) are rejected with a hint to convert
/// them to markdown first — we'd rather make the user choose the
/// conversion than silently store a blob the model can't read.
/// What to do with a document once it is archived (dev-plan/64 P4.9).
///
/// These are not four flavours of the same thing — they buy different
/// guarantees at different prices, and the choice belongs to whoever
/// knows how much the document matters:
///
/// - `Archive`: keep the source, write nothing. It is searchable and
///   citable by hand. Costs nothing.
/// - `Summary`: the main agent reads it and writes a page. One turn,
///   cheap, and the page carries **no claims, no quotes, no `[N]`
///   citations, no `uncited:` and no `verified:`** — the trust strip
///   and `/kms verify` have nothing to work with. It looks like a
///   checkable page and is not one.
/// - `Cited`: the research pipeline digests it into claims, checks
///   every quote against the archived text, and writes **one** page
///   with real citations and every trust key stamped. Several model
///   calls and minutes rather than one turn.
/// - `Atomic`: the same pipeline without the one-page cap — a topic
///   page and a note per idea.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestMode {
    Archive,
    Summary,
    Cited,
    Atomic,
}

impl IngestMode {
    /// Anything unrecognised is `Summary`, which is what every caller
    /// that predates this meant by sending nothing.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "archive" | "none" | "raw" => IngestMode::Archive,
            "cited" | "sourced" => IngestMode::Cited,
            "atomic" | "notes" => IngestMode::Atomic,
            _ => IngestMode::Summary,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            IngestMode::Archive => "archive",
            IngestMode::Summary => "summary",
            IngestMode::Cited => "cited",
            IngestMode::Atomic => "atomic",
        }
    }

    /// The cap to put on the research pipeline. Only consulted when
    /// [`is_research`](Self::is_research); `None` means "leave the run's
    /// own default", which is what `Atomic` wants.
    pub fn research_max_notes(self) -> Option<u32> {
        match self {
            IngestMode::Cited => Some(1),
            _ => None,
        }
    }

    /// Whether this mode runs the research pipeline at all.
    pub fn is_research(self) -> bool {
        matches!(self, IngestMode::Cited | IngestMode::Atomic)
    }
}

pub const INGEST_EXTENSIONS: &[&str] = &[
    "md", "markdown", "txt", "rst", "log", "json", "html", "htm", "csv", "yaml", "yml", "toml",
];

/// Image extensions pulled into `sources/<alias>-assets/` when a
/// markdown source is ingested (see [`localize_markdown_images`]). A
/// narrow allowlist so a crafted `![](../secrets.env)` link can't
/// smuggle a non-image file into the store.
const INGEST_IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "bmp", "ico", "heic", "heif", "tif", "tiff",
];

/// Per-image size cap when localizing markdown images on ingest. Skip
/// (leave the link untouched) anything larger so an ingest can't bloat
/// the KMS with a huge asset. Mirrors content-extractor's 25 MB cap.
const INGEST_IMAGE_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Reserved aliases that collide with the KMS starter files — refuse
/// to ingest into them, otherwise a `/kms ingest notes README.md as index`
/// would clobber the index with no way back except `--force`.
const RESERVED_PAGE_STEMS: &[&str] = &["index", "log", "SCHEMA"];

/// True for a page name the KMS keeps for itself. For callers that mint
/// page names — the research planner titled a note "Index", and the write
/// that followed took the whole run down with it.
pub fn is_reserved_page_stem(stem: &str) -> bool {
    let stem = stem.trim_end_matches(".md");
    RESERVED_PAGE_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
}

// ────────────────────────────────────────────────────────────────────────
// sources/ as a first-class layer.
//
// Pre-fix `sources/` was a write-only archive: `ingest` copied raw
// material in, and nothing else ever looked at it. The regex and BM25
// search paths walked `pages/` only, no tool could read a source, and
// the GUI browser hard-coded `.md` so every `.txt` / `.json` / `.log`
// ingest was invisible AND unopenable. That made ingest a no-op in
// practice — the content landed on disk and left no trace anybody
// could reach.
//
// Sources are now enumerable, resolvable by stem across every
// supported extension, searchable, and readable through the same
// surfaces pages use.

/// Extensions a file in `sources/` may carry. Superset of
/// [`INGEST_EXTENSIONS`] — `html` is produced by URL ingest when the
/// markdown conversion is declined, and research writes `.md`.
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "md", "markdown", "txt", "rst", "log", "json", "html", "htm", "csv", "yaml", "yml", "toml",
];

/// One file in `sources/`. `stem` is the name callers address it by
/// (no extension); `ext` is what it actually carries on disk.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceFile {
    pub stem: String,
    pub ext: String,
    pub bytes: u64,
}

impl SourceFile {
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.stem, self.ext)
    }
}

/// Enumerate `sources/`, sorted by stem. Skips symlinks (same
/// exfiltration concern as `pages/`), dotfiles, the per-alias
/// `<alias>-assets/` image directories, and any extension outside
/// [`SOURCE_EXTENSIONS`].
pub fn list_sources(kref: &KmsRef) -> Vec<SourceFile> {
    let Ok(entries) = std::fs::read_dir(kref.sources_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<SourceFile> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // `_`-prefixed files are this layer's own bookkeeping (the
        // provenance catalogue); aliases are sanitised with leading
        // underscores trimmed, so no archived source can collide.
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let ext = ext.to_ascii_lowercase();
        if !SOURCE_EXTENSIONS.iter().any(|e| *e == ext) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push(SourceFile {
            stem: stem.to_string(),
            ext,
            bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
        });
    }
    out.sort_by(|a, b| a.stem.cmp(&b.stem).then(a.ext.cmp(&b.ext)));
    out
}

/// An archive name is cut at a fixed length, and a model rebuilding one
/// from its URL does not know where: it asks for the whole slug and is told
/// there is no such source. When exactly one archived file's stem is the
/// start of what was asked for — and long enough that this is a cut and not
/// a coincidence — that is the file.
fn truncated_source(sources_dir: &Path, asked: &str) -> Option<PathBuf> {
    const MIN_STEM: usize = 40;
    let mut hits: Vec<PathBuf> = std::fs::read_dir(sources_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| stem.len() >= MIN_STEM && asked.starts_with(stem))
        })
        .collect();
    (hits.len() == 1).then(|| hits.remove(0))
}

/// Resolve a source by stem (or full `stem.ext`) to a path inside
/// `sources/`. Applies the same name validation + canonical-containment
/// check `page_path` uses, then picks the on-disk extension: an exact
/// `stem.ext` match when the caller supplied one, otherwise the first
/// [`SOURCE_EXTENSIONS`] hit in preference order.
pub fn source_path(kref: &KmsRef, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
        || Path::new(name).is_absolute()
    {
        return Err(Error::Tool(format!(
            "invalid source name '{name}' — no '..', path separators, or control chars"
        )));
    }
    let sources_dir = kref.sources_dir();
    if let Ok(md) = std::fs::symlink_metadata(&sources_dir) {
        if md.file_type().is_symlink() {
            return Err(Error::Tool(format!(
                "kms '{}' has a symlinked sources/ directory — refusing to read",
                kref.name
            )));
        }
    }

    // An explicit extension wins when the file exists; otherwise treat
    // the whole string as a stem (a source legitimately named
    // "notes.v2" resolves to notes.v2.md).
    let mut candidates: Vec<String> = Vec::new();
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if SOURCE_EXTENSIONS
            .iter()
            .any(|e| e.eq_ignore_ascii_case(ext))
            && !stem.is_empty()
        {
            candidates.push(name.to_string());
        }
    }
    for ext in SOURCE_EXTENSIONS {
        candidates.push(format!("{name}.{ext}"));
    }

    let found = candidates
        .iter()
        .map(|c| sources_dir.join(c))
        .find(|p| p.is_file())
        .or_else(|| truncated_source(&sources_dir, name))
        .ok_or_else(|| {
            Error::Tool(format!(
                "no source '{name}' in kms '{}' (tried: {})",
                kref.name,
                SOURCE_EXTENSIONS
                    .iter()
                    .map(|e| format!(".{e}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ))
        })?;

    // Symlink escape: canonicalize and require containment.
    let canon_dir = std::fs::canonicalize(&sources_dir)
        .map_err(|e| Error::Tool(format!("canonicalize sources dir: {e}")))?;
    let canon_found = std::fs::canonicalize(&found)
        .map_err(|e| Error::Tool(format!("canonicalize {}: {e}", found.display())))?;
    if !canon_found.starts_with(&canon_dir) {
        return Err(Error::Tool(format!(
            "source '{name}' resolves outside sources/ — symlink escape rejected"
        )));
    }
    Ok(found)
}

/// True when any source file shares this stem, whatever its extension.
/// `ingest` uses it so `notes.txt` and `notes.md` can't both claim the
/// alias `notes` (they'd map to one page and one search identity).
pub fn source_stem_taken(kref: &KmsRef, stem: &str) -> Option<String> {
    list_sources(kref)
        .into_iter()
        .find(|s| s.stem == stem)
        .map(|s| s.file_name())
}

/// Summary returned by [`remove`] — counts so the dispatcher can
/// report "deleted N pages, M sources" instead of just "ok".
#[derive(Debug, Default)]
pub struct DropReport {
    pub pages_removed: u32,
    pub sources_removed: u32,
    pub root: PathBuf,
    /// Where the dropped KMS sits until it is pruned or restored.
    pub trashed: PathBuf,
}

/// Delete a KMS from disk. Removes the entire scope-rooted directory
/// — pages, sources, index, log, manifest, schema — and returns a
/// count summary. Symlinks at the KMS root are refused (resolve()
/// already filters them out, so this is just belt-and-braces).
///
/// Destructive: caller is responsible for any "are you sure?" prompt
/// and for clearing the KMS from any active-set config. The
/// underlying directory tree is removed via `fs::remove_dir_all`.
pub fn remove(name: &str) -> Result<DropReport> {
    let kref = resolve(name).ok_or_else(|| Error::Tool(format!("KMS '{name}' not found")))?;

    let pages_removed = std::fs::read_dir(kref.pages_dir())
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
                .count() as u32
        })
        .unwrap_or(0);
    // Counted across every source extension — filtering to `.md` here
    // under-reported any KMS holding txt/json/log/html archives.
    let sources_removed = list_sources(&kref).len() as u32;

    // Moved, not removed: `/kms restore <name>` brings it back.
    drop_search_handle(&kref);
    drop_search_index(&kref);
    let trashed = crate::kms_trash::drop_kms(&kref)?;

    Ok(DropReport {
        pages_removed,
        sources_removed,
        root: kref.root,
        trashed,
    })
}

/// What `ingest()` did. `overwrote == true` means `--force` replaced an
/// existing page; the handler surfaces that to the user so a typo in
/// the alias doesn't silently nuke a page. `cascaded` is the count of
/// dependent pages marked stale (M6.25 BUG #10).
#[derive(Debug)]
pub struct IngestResult {
    pub alias: String,
    pub target: PathBuf,
    pub summary: String,
    pub overwrote: bool,
    pub cascaded: usize,
    /// Local relative images copied into `sources/<alias>-assets/` and
    /// re-linked in the archived source (markdown ingest only; 0 for
    /// text/URL/PDF sources or when the file has no local images).
    pub images_copied: usize,
    /// Set when an existing source has byte-identical content under a
    /// different alias. The ingest still completes — the caller may
    /// legitimately want two entry points — but the duplicate is
    /// surfaced instead of silently doubling the archive.
    pub duplicate_of: Option<String>,
}

impl IngestResult {
    /// Trailing note for the operator message: content-identical
    /// archives, and the reminder that the page still needs curating.
    pub fn notes(&self) -> String {
        let mut s = String::new();
        if let Some(dup) = &self.duplicate_of {
            s.push_str(&format!(
                " [duplicate content — byte-identical to sources/{dup}]"
            ));
        }
        s.push_str(" — page is `status: derived`; curate it or ask the agent to");
        s
    }
}

/// M6.25 BUG #2: Ingest now SPLITS raw source from wiki page.
///
/// Pre-fix: `ingest()` copied the source straight into `pages/` and
/// treated it as both layer-1 (raw, immutable) and layer-2 (LLM-
/// authored synthesis). The llm-wiki concept requires those to be
/// distinct.
///
/// Post-fix: copy raw to `sources/<alias>.<ext>`, then write a stub
/// page in `pages/<alias>.md` with frontmatter pointing at the
/// source. The page stub is plain markdown the LLM can later enrich
/// via `KmsWrite`. `--force` re-copies the source AND triggers a
/// cascade: any page whose frontmatter `sources:` includes this
/// alias gets a "stale" marker appended (BUG #10). User then runs
/// `/kms lint` or asks the agent to refresh affected pages.
pub fn ingest(
    kms: &KmsRef,
    source: &Path,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let origin_ref = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf())
        .display()
        .to_string();
    ingest_with_origin(
        kms,
        source,
        alias,
        force,
        crate::kms_sources::Origin::File,
        &origin_ref,
        None,
    )
}

/// [`ingest`] plus the provenance the caller knows. `origin`/
/// `origin_ref` land in the source catalogue; `converted_from` records
/// that the archived copy is a conversion (HTML → Markdown, PDF →
/// text) rather than the original bytes, so nobody later mistakes the
/// archive for a faithful copy.
pub fn ingest_with_origin(
    kms: &KmsRef,
    source: &Path,
    alias: Option<&str>,
    force: bool,
    origin: crate::kms_sources::Origin,
    origin_ref: &str,
    converted_from: Option<&str>,
) -> Result<IngestResult> {
    ensure_writable(kms)?;
    let meta = std::fs::metadata(source)
        .map_err(|e| Error::Tool(format!("cannot stat source '{}': {e}", source.display())))?;
    if !meta.is_file() {
        return Err(Error::Tool(format!(
            "source '{}' is not a regular file",
            source.display()
        )));
    }

    let ext_raw = source.extension().and_then(|e| e.to_str()).ok_or_else(|| {
        Error::Tool(format!(
            "'{}' has no extension — ingest requires one of: {}",
            source.display(),
            INGEST_EXTENSIONS.join(", "),
        ))
    })?;
    let ext = ext_raw.to_ascii_lowercase();
    if !INGEST_EXTENSIONS.iter().any(|e| *e == ext) {
        return Err(Error::Tool(format!(
            "extension '.{ext}' not supported — allowed: {} (or use the URL/PDF ingest variants)",
            INGEST_EXTENSIONS.join(", "),
        )));
    }

    let raw_alias = match alias {
        Some(a) => a.to_string(),
        None => source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("page")
            .to_string(),
    };
    let alias = sanitize_alias(&raw_alias);
    if alias.is_empty() {
        return Err(Error::Tool(format!(
            "alias '{raw_alias}' sanitises to empty — use letters, numbers, '-' or '_'"
        )));
    }
    if RESERVED_PAGE_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&alias))
    {
        return Err(Error::Tool(format!(
            "alias '{alias}' is reserved — pick another"
        )));
    }

    // Source path lives under sources/, derived page under pages/.
    std::fs::create_dir_all(kms.sources_dir())
        .map_err(|e| Error::Tool(format!("ensure sources dir: {e}")))?;
    let source_target = kms.sources_dir().join(format!("{alias}.{ext}"));
    let page_target = kms.pages_dir().join(format!("{alias}.md"));
    let page_existed = page_target.exists();
    // Same rule as `write_page`: what a vault starts with is what it is
    // about. Ingest writes its page directly, so it never reached that
    // hook — a vault seeded by an ingest had no entry page, and every
    // system-prompt build paid for inferring one from a full scan.
    let is_first_page = !page_existed && page_count(kms) == 0;
    // Collision is checked across EVERY source extension, not just the
    // one being written: `notes.txt` and `notes.md` would both claim
    // the alias `notes`, mapping two different documents onto one page
    // and one search identity.
    let existing_source = source_stem_taken(kms, &alias);
    let source_existed = existing_source.is_some();
    if (page_existed || source_existed) && !force {
        return Err(Error::Tool(format!(
            "alias '{alias}' already exists ({}{}{}) — re-run with --force to overwrite",
            existing_source
                .as_deref()
                .map(|f| format!("source {f}"))
                .unwrap_or_default(),
            if source_existed && page_existed {
                " + "
            } else {
                ""
            },
            if page_existed { "page" } else { "" },
        )));
    }
    // A --force re-ingest under a different extension must not leave
    // the old archive behind as a second document.
    if let Some(old) = existing_source.filter(|f| *f != format!("{alias}.{ext}")) {
        let _ = crate::kms_trash::move_to_trash(kms, &format!("sources/{old}"));
        let _ = crate::kms_sources::forget(kms, &old);
        fire_source_index_delete(kms, &old);
    }

    // A local `.html` file is converted the same way a fetched URL is:
    // archiving tag soup makes the copy unreadable in the viewer and
    // worthless as a search document. The archive lands as `.md` with
    // the conversion recorded, so nobody mistakes it for the original.
    let (ext, source_target, converted_from) = if matches!(ext.as_str(), "html" | "htm") {
        let raw = std::fs::read_to_string(source)
            .map_err(|e| Error::Tool(format!("read {} for conversion: {e}", source.display())))?;
        let (title, md) = crate::html_md::convert(&raw);
        let mut fm = std::collections::BTreeMap::new();
        fm.insert("type".to_string(), "source".to_string());
        fm.insert("converted_from".to_string(), "text/html".to_string());
        if !title.is_empty() {
            fm.insert("title".to_string(), title);
        }
        let target = kms.sources_dir().join(format!("{alias}.md"));
        let converted = write_frontmatter(&fm, &md);
        crate::kms_trash::keep_before_overwrite(
            kms,
            &format!("sources/{alias}.md"),
            converted.as_bytes(),
        );
        write_file(&target, converted.as_bytes())
            .map_err(|e| Error::Tool(format!("write {}: {e}", target.display())))?;
        ("md".to_string(), target, Some("text/html"))
    } else {
        // `--force` replaces the archive; the one it replaces is kept.
        if let (Ok(next), Some(file)) = (
            std::fs::read(source),
            source_target.file_name().and_then(|n| n.to_str()),
        ) {
            crate::kms_trash::keep_before_overwrite(kms, &format!("sources/{file}"), &next);
        }
        std::fs::copy(source, &source_target).map_err(|e| {
            Error::Tool(format!(
                "copy {} → {} failed: {e}",
                source.display(),
                source_target.display()
            ))
        })?;
        (ext, source_target, converted_from)
    };

    // Pull local relative images referenced by a markdown source into
    // sources/<alias>-assets/ and re-link them, so the archived copy is
    // self-contained (best-effort — remote/data/missing/oversized images
    // and non-markdown sources leave links untouched and return 0).
    let images_copied = if matches!(ext.as_str(), "md" | "markdown") {
        let orig_dir = source.parent().unwrap_or_else(|| Path::new("."));
        localize_markdown_images(orig_dir, &source_target, &kms.root.join("sources"), &alias)
            .unwrap_or(0)
    } else {
        0
    };

    // Derive an actual reading surface from the source instead of the
    // fixed placeholder this used to write. Pre-fix every ingest
    // produced the same "Stub page — raw source at …" body: the KMS
    // gained a file and zero knowledge, the index bullet carried the
    // source's first line (a `<!doctype html>` or a PDF banner), and
    // the source itself was unreachable from every search path. The
    // page below is a genuine entry point — title, provenance, lead
    // paragraph, outline of the source's own structure, and a real
    // relative link so the graph and backlink views connect it.
    let file_name = format!("{alias}.{ext}");
    let outline = outline_source(&source_target, &ext);
    let summary = ingest_summary(&outline, &alias);
    let sha256 = crate::kms_sources::hash_file(&source_target);
    let bytes = std::fs::metadata(&source_target)
        .map(|m| m.len())
        .unwrap_or(0);
    let duplicate_of = crate::kms_sources::find_by_hash(kms, &sha256)
        .filter(|r| r.file != file_name)
        .map(|r| r.file);

    // What is already at `pages/<alias>.md`, if anything. A stub this
    // function wrote earlier still says `status: derived` and is ours to
    // regenerate. Anything else has been written over — by a research run
    // that turned it into the topic page, or by a person — and a re-ingest
    // used to replace it with a fresh outline stub: a 17,000-character
    // topic page with its `kind: moc`, its `related:` list and its claim
    // count, gone behind a generic "Replace?" prompt, while the notes that
    // link to it stayed.
    let existing_fm = if page_existed {
        std::fs::read_to_string(&page_target)
            .ok()
            .map(|raw| parse_frontmatter(&raw).0)
    } else {
        None
    };
    let page_is_ours = existing_fm
        .as_ref()
        .map(|fm| fm.get("status").map(|s| s.trim()) == Some("derived"))
        .unwrap_or(true);

    let mut fm = std::collections::BTreeMap::new();
    let today = crate::usage::today_str();
    // `created` is when the page first appeared, not when it was last
    // regenerated; it used to be dropped on every re-ingest.
    let created = existing_fm
        .as_ref()
        .and_then(|fm| fm.get("created").cloned())
        .unwrap_or_else(|| today.clone());
    fm.insert("created".into(), created);
    fm.insert("updated".into(), today.clone());
    fm.insert("category".into(), "uncategorized".into());
    fm.insert("sources".into(), alias.clone());
    // `status: derived` marks a page nobody has curated yet. lint and
    // `/kms maintain` use it to find the ingest backlog; a KmsWrite
    // that replaces the body drops the marker naturally.
    fm.insert("status".into(), "derived".into());
    if !origin_ref.is_empty() {
        fm.insert("origin".into(), origin_ref.to_string());
    }
    let body = derive_page_body(
        &alias,
        &file_name,
        &outline,
        origin,
        origin_ref,
        converted_from,
        bytes,
        &sha256,
        &today,
    );
    if page_is_ours {
        let serialized = write_frontmatter(&fm, &body);
        if let Some(file) = page_target.file_name().and_then(|n| n.to_str()) {
            crate::kms_trash::keep_before_overwrite(
                kms,
                &format!("pages/{file}"),
                serialized.as_bytes(),
            );
        }
        write_file(&page_target, serialized.as_bytes())
            .map_err(|e| Error::Tool(format!("write page {}: {e}", page_target.display())))?;
    } else {
        eprintln!(
            "[kms] re-ingest of '{alias}': the source was replaced, the page was kept — \
             it is no longer an ingest stub"
        );
    }
    if is_first_page && kms.read_manifest().and_then(|m| m.entry).is_none() {
        let _ = set_entry_page(kms, Some(&alias));
    }

    crate::kms_sources::upsert(
        kms,
        crate::kms_sources::SourceRecord {
            file: file_name.clone(),
            title: outline.title.clone(),
            origin,
            origin_ref: origin_ref.to_string(),
            ingested: today.clone(),
            bytes,
            sha256,
            converted_from: converted_from.map(String::from),
        },
    )?;

    update_index_for_write(kms, &alias, &summary, Some("uncategorized"), page_existed)?;
    fire_index_upsert(kms, &alias);
    fire_source_index_upsert(kms, &file_name);
    append_log_header(
        kms,
        if page_existed {
            "re-ingested"
        } else {
            "ingested"
        },
        &alias,
    )?;

    // BUG #10: cascade on re-ingest. Pages whose frontmatter
    // `sources:` mentions this alias get a stale marker appended so
    // the next reader (human or agent) knows to refresh.
    let cascade_count = if page_existed && force {
        mark_dependent_pages_stale(kms, &alias).unwrap_or(0)
    } else {
        0
    };

    Ok(IngestResult {
        alias,
        target: page_target,
        summary,
        overwrote: page_existed,
        cascaded: cascade_count,
        images_copied,
        duplicate_of,
    })
}

// ────────────────────────────────────────────────────────────────────────
// Deriving a page from an ingested source.

/// What we could learn about a source's shape without an LLM. Enough
/// to build a page that is a usable entry point rather than a
/// placeholder, and to give the index a summary worth reading.
#[derive(Debug, Default, Clone)]
pub struct SourceOutline {
    /// The source's own title — frontmatter `title:`, first ATX
    /// heading, RST-underlined first line, or the de-slugged stem.
    pub title: String,
    /// First substantive paragraph, trimmed for the page lead.
    pub lead: String,
    /// `(level, text)` for each heading found, document order.
    pub headings: Vec<(usize, String)>,
    /// Total non-blank lines — cheap size signal shown on the page.
    pub lines: usize,
}

/// Longest lead paragraph carried onto the derived page.
const LEAD_MAX_CHARS: usize = 600;
/// Cap on outline entries so a 300-heading document doesn't produce a
/// page longer than the source.
const OUTLINE_MAX: usize = 60;

/// Read a source and describe its structure. Markdown and RST get real
/// heading extraction; JSON gets its top-level keys; anything else
/// falls back to a lead paragraph only. Unreadable files yield an empty
/// outline — never an error, because the archive copy already
/// succeeded and the page is a convenience on top of it.
pub fn outline_source(path: &Path, ext: &str) -> SourceOutline {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("source")
        .to_string();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return SourceOutline {
            title: stem.replace(['-', '_'], " "),
            ..Default::default()
        };
    };
    let (fm, body) = parse_frontmatter(&raw);
    let mut out = SourceOutline {
        lines: body.lines().filter(|l| !l.trim().is_empty()).count(),
        ..Default::default()
    };
    if let Some(t) = fm.get("title").map(|s| s.trim().trim_matches('"')) {
        if !t.is_empty() {
            out.title = t.to_string();
        }
    }

    // `titled_headings` is false for formats whose "outline" is a key
    // list rather than a document structure — a JSON file's first key
    // is not its title.
    let titled_headings = match ext {
        "md" | "markdown" => {
            collect_markdown_headings(&body, &mut out);
            true
        }
        "rst" => {
            collect_rst_headings(&body, &mut out);
            true
        }
        "json" => {
            collect_json_keys(&body, &mut out);
            false
        }
        _ => true,
    };

    if out.title.is_empty() {
        out.title = out
            .headings
            .first()
            .filter(|_| titled_headings)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| stem.replace(['-', '_'], " "));
    }
    out.lead = first_paragraph(&body);
    out
}

/// ATX headings, skipping fenced code blocks (a `# comment` inside a
/// shell snippet is not a section) and HTML comment banners.
fn collect_markdown_headings(body: &str, out: &mut SourceOutline) {
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if level > 6 {
            continue;
        }
        let text = trimmed[level..].trim().trim_end_matches('#').trim();
        if text.is_empty() {
            continue;
        }
        if out.title.is_empty() && level == 1 {
            out.title = text.chars().take(160).collect();
        }
        if out.headings.len() < OUTLINE_MAX {
            out.headings.push((level, text.chars().take(160).collect()));
        }
    }
}

/// RST section headers: a line of text followed by a line of a single
/// repeated punctuation char at least as long. Level is assigned by
/// first-seen order of the underline character, matching RST's own
/// "whatever you use first is level 1" rule.
fn collect_rst_headings(body: &str, out: &mut SourceOutline) {
    let lines: Vec<&str> = body.lines().collect();
    let mut char_levels: Vec<char> = Vec::new();
    for i in 0..lines.len().saturating_sub(1) {
        let text = lines[i].trim();
        let rule = lines[i + 1].trim();
        if text.is_empty() || rule.len() < text.chars().count() || rule.len() < 3 {
            continue;
        }
        let Some(c) = rule.chars().next() else {
            continue;
        };
        if c.is_alphanumeric() || c.is_whitespace() || !rule.chars().all(|x| x == c) {
            continue;
        }
        let level = match char_levels.iter().position(|x| *x == c) {
            Some(p) => p + 1,
            None => {
                char_levels.push(c);
                char_levels.len()
            }
        };
        if out.title.is_empty() && level == 1 {
            out.title = text.chars().take(160).collect();
        }
        if out.headings.len() < OUTLINE_MAX {
            out.headings.push((level, text.chars().take(160).collect()));
        }
    }
}

/// Top-level keys of a JSON document, as a flat outline. An array at
/// the root reports its length instead — there are no keys to list.
fn collect_json_keys(body: &str, out: &mut SourceOutline) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter().take(OUTLINE_MAX) {
                let shape = match val {
                    serde_json::Value::Object(o) => format!("object ({} keys)", o.len()),
                    serde_json::Value::Array(a) => format!("array ({} items)", a.len()),
                    serde_json::Value::String(_) => "string".into(),
                    serde_json::Value::Number(_) => "number".into(),
                    serde_json::Value::Bool(_) => "bool".into(),
                    serde_json::Value::Null => "null".into(),
                };
                out.headings.push((1, format!("`{k}` — {shape}")));
            }
        }
        serde_json::Value::Array(a) => {
            out.headings
                .push((1, format!("array of {} items", a.len())));
        }
        _ => {}
    }
}

/// First run of prose: skips frontmatter (already stripped), HTML
/// comment banners, headings, fences, list markers, and blank lines,
/// then joins the following non-blank lines up to [`LEAD_MAX_CHARS`].
fn first_paragraph(body: &str) -> String {
    let mut buf = String::new();
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            if !buf.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.starts_with('#')
            || trimmed.starts_with("<!--")
            || trimmed.starts_with("---")
            || trimmed.starts_with("===")
        {
            continue;
        }
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(trimmed);
        if buf.chars().count() >= LEAD_MAX_CHARS {
            break;
        }
    }
    let mut s: String = buf.chars().take(LEAD_MAX_CHARS).collect();
    if buf.chars().count() > LEAD_MAX_CHARS {
        s.push('…');
    }
    s
}

/// Index summary for a freshly-ingested source. Prefers the lead
/// sentence over the title — an index whose every bullet repeats its
/// own link text tells the model nothing about what is inside.
fn ingest_summary(outline: &SourceOutline, alias: &str) -> String {
    let candidate = if !outline.lead.is_empty() {
        outline.lead.as_str()
    } else if !outline.title.is_empty() && outline.title != alias {
        outline.title.as_str()
    } else if !outline.headings.is_empty() {
        return format!(
            "{} section(s): {}",
            outline.headings.len(),
            outline
                .headings
                .iter()
                .take(4)
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        return "(archived source — no extractable summary)".into();
    };
    let mut s: String = candidate.chars().take(160).collect();
    if candidate.chars().count() > 160 {
        s.push('…');
    }
    s
}

/// Render the page an ingest creates. Every section is derived from
/// the source, and the `../sources/<file>` link is a real relative
/// markdown link — the format the graph view and backlink resolution
/// already understand, which the old stub's inline-code reference was
/// not.
#[allow(clippy::too_many_arguments)]
fn derive_page_body(
    alias: &str,
    file_name: &str,
    outline: &SourceOutline,
    origin: crate::kms_sources::Origin,
    origin_ref: &str,
    converted_from: Option<&str>,
    bytes: u64,
    sha256: &str,
    today: &str,
) -> String {
    let title = if outline.title.trim().is_empty() {
        alias.replace(['-', '_'], " ")
    } else {
        outline.title.clone()
    };
    let mut out = format!("# {title}\n\n");
    out.push_str(&format!(
        "> Derived from [`sources/{file_name}`](../sources/{file_name}) on {today}. \
         This page has not been curated yet — replace the body with a synthesis \
         and drop `status: derived` from the frontmatter when you do.\n\n"
    ));

    if !outline.lead.is_empty() {
        out.push_str(&outline.lead);
        out.push_str("\n\n");
    }

    if !outline.headings.is_empty() {
        out.push_str("## Outline of the source\n\n");
        let base = outline.headings.iter().map(|(l, _)| *l).min().unwrap_or(1);
        for (level, text) in &outline.headings {
            let indent = "  ".repeat(level.saturating_sub(base).min(4));
            out.push_str(&format!("{indent}- {text}\n"));
        }
        if outline.headings.len() >= OUTLINE_MAX {
            out.push_str(&format!(
                "- _… outline truncated at {OUTLINE_MAX} entries_\n"
            ));
        }
        out.push('\n');
    }

    out.push_str("## Provenance\n\n");
    let origin_line = match origin {
        crate::kms_sources::Origin::Url => format!("- Fetched from <{origin_ref}>\n"),
        crate::kms_sources::Origin::Pdf => format!("- Extracted from PDF `{origin_ref}`\n"),
        crate::kms_sources::Origin::Session => format!("- Distilled from session `{origin_ref}`\n"),
        crate::kms_sources::Origin::Research => format!("- Research fetch: {origin_ref}\n"),
        _ if origin_ref.is_empty() => String::new(),
        _ => format!("- Copied from `{origin_ref}`\n"),
    };
    out.push_str(&origin_line);
    out.push_str(&format!(
        "- Archived at [`sources/{file_name}`](../sources/{file_name})"
    ));
    if bytes > 0 {
        out.push_str(&format!(" · {}", crate::kms_sources::human_bytes(bytes)));
    }
    if outline.lines > 0 {
        out.push_str(&format!(" · {} lines", outline.lines));
    }
    out.push('\n');
    if let Some(from) = converted_from {
        out.push_str(&format!(
            "- Converted from `{from}` — the archive is a rendering, not the original bytes\n"
        ));
    }
    if !sha256.is_empty() {
        out.push_str(&format!("- sha256 `{}`\n", &sha256[..sha256.len().min(16)]));
    }
    out.push_str(&format!(
        "\nSearch inside the full source with \
         `KmsSearch(pattern: \"…\", scope: \"sources\")`, or read it with \
         `KmsRead(kind: \"source\", page: \"{file_name}\")`.\n"
    ));
    out
}

/// Notify the BM25 index that a source file changed. Mirrors
/// [`fire_index_upsert`] for the `sources/` layer.
fn fire_source_index_upsert(kref: &KmsRef, file_name: &str) {
    #[cfg(feature = "kms_search_index")]
    crate::kms_search_index::on_source_mutated(
        &kref.root,
        file_name,
        crate::kms_search_index::Op::Upsert,
    );
    #[cfg(not(feature = "kms_search_index"))]
    let _ = (kref, file_name);
}

fn fire_source_index_delete(kref: &KmsRef, file_name: &str) {
    #[cfg(feature = "kms_search_index")]
    crate::kms_search_index::on_source_mutated(
        &kref.root,
        file_name,
        crate::kms_search_index::Op::Delete,
    );
    #[cfg(not(feature = "kms_search_index"))]
    let _ = (kref, file_name);
}

/// Copy every *local, relative* image an ingested markdown file
/// references into `sources/<alias>-assets/` and rewrite the links in
/// `copied_md` (the archived `sources/<alias>.md`) to point at the local
/// copies. Returns the number of images copied.
///
/// Best-effort and conservative — a link is left exactly as-is when it
/// is a remote URL (`http(s)://`, protocol-relative `//`, any `scheme:`),
/// a `data:` URI, an absolute filesystem path, doesn't resolve to an
/// existing file under `orig_dir`, has an extension outside
/// [`INGEST_IMAGE_EXTENSIONS`], or exceeds [`INGEST_IMAGE_MAX_BYTES`]. A
/// per-image copy failure is swallowed (link untouched), never aborting
/// the ingest. Non-markdown callers match nothing and get 0.
///
/// On every call the alias's assets dir is recreated fresh, so a
/// `--force` re-ingest doesn't accumulate stale images.
fn localize_markdown_images(
    orig_dir: &Path,
    copied_md: &Path,
    sources_dir: &Path,
    alias: &str,
) -> Result<usize> {
    let text = std::fs::read_to_string(copied_md).map_err(|e| {
        Error::Tool(format!(
            "read {} for image localize: {e}",
            copied_md.display()
        ))
    })?;

    // Inline markdown images: `![alt](target)` / `![alt](target "title")`
    // / `![alt](<target>)`. Capture the parens payload; parse the target
    // out of it in the closure so titles and angle-bracket forms survive.
    static IMG_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = IMG_RE.get_or_init(|| {
        regex::Regex::new(r"(?P<pre>!\[[^\]]*\]\()(?P<inner>[^)]*)(?P<post>\))")
            .expect("static image regex")
    });

    let assets_dir = sources_dir.join(format!("{alias}-assets"));
    let _ = std::fs::remove_dir_all(&assets_dir);

    let dir_rel = format!("{alias}-assets");
    // Canonical source path → already-assigned local relative link, so a
    // file referenced twice copies once and both links converge.
    let mut copied: std::collections::HashMap<PathBuf, String> = std::collections::HashMap::new();
    let mut count: usize = 0;

    let rewritten = re.replace_all(&text, |caps: &regex::Captures| {
        let pre = &caps["pre"];
        let inner = &caps["inner"];
        let post = &caps["post"];
        let original = format!("{pre}{inner}{post}");

        // Split the parens payload into the URL and an optional trailing
        // title / whitespace we must preserve verbatim.
        let (url_raw, suffix) = split_link_target(inner);
        let url = url_raw.trim();
        if url.is_empty() {
            return original;
        }

        // Remote / non-file targets: leave untouched.
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("//")
            || lower.starts_with("data:")
            || lower.starts_with("mailto:")
            || url.contains("://")
        {
            return original;
        }

        // Strip a leading `./`; absolute paths are out of scope.
        let rel = url.strip_prefix("./").unwrap_or(url);
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return original;
        }

        let src_path = orig_dir.join(rel_path);
        let Ok(canon) = std::fs::canonicalize(&src_path) else {
            return original;
        };
        if !canon.is_file() {
            return original;
        }
        let ext_ok = canon
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                INGEST_IMAGE_EXTENSIONS.iter().any(|x| *x == e)
            })
            .unwrap_or(false);
        if !ext_ok {
            return original;
        }

        // Reuse an earlier copy of the same file, else copy it now.
        let new_rel = if let Some(existing) = copied.get(&canon) {
            existing.clone()
        } else {
            let meta = match std::fs::metadata(&canon) {
                Ok(m) => m,
                Err(_) => return original,
            };
            if meta.len() > INGEST_IMAGE_MAX_BYTES {
                return original;
            }
            let base = sanitize_asset_name(
                canon
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image"),
            );
            // Index-prefix guarantees uniqueness without collision logic.
            let fname = format!("{:03}-{base}", count + 1);
            if std::fs::create_dir_all(&assets_dir).is_err() {
                return original;
            }
            if std::fs::copy(&canon, assets_dir.join(&fname)).is_err() {
                return original;
            }
            let new_rel = format!("{dir_rel}/{fname}");
            copied.insert(canon.clone(), new_rel.clone());
            count += 1;
            new_rel
        };

        format!("{pre}{new_rel}{suffix}{post}")
    });

    if count > 0 {
        std::fs::write(copied_md, rewritten.as_bytes()).map_err(|e| {
            Error::Tool(format!(
                "rewrite image links in {}: {e}",
                copied_md.display()
            ))
        })?;
    }
    Ok(count)
}

/// Split a markdown link's parens payload into `(target, trailing)`.
/// `trailing` is the optional title + surrounding whitespace, preserved
/// verbatim so only the URL slice is rewritten. The `<...>` angle-bracket
/// target form isn't special-cased — such a target fails the file
/// resolution below and is left untouched, which is the safe outcome.
fn split_link_target(inner: &str) -> (String, String) {
    let trimmed_start = inner.trim_start();
    let lead_ws = &inner[..inner.len() - trimmed_start.len()];
    match trimmed_start.find(char::is_whitespace) {
        Some(idx) => (
            trimmed_start[..idx].to_string(),
            format!("{lead_ws}{}", &trimmed_start[idx..]),
        ),
        None => (trimmed_start.to_string(), lead_ws.to_string()),
    }
}

/// Filesystem-safe basename for a copied asset — keep it recognisable but
/// strip anything that could escape the assets dir or confuse tooling.
fn sanitize_asset_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned
    }
}

/// M6.25 BUG #10: re-ingest cascade. Walk every page; if its
/// frontmatter `sources:` contains the changed alias (comma- or
/// space- separated list), append a stale-marker line at the bottom
/// of the page body (after frontmatter). Returns the count of pages
/// touched.
fn mark_dependent_pages_stale(kref: &KmsRef, changed_alias: &str) -> Result<usize> {
    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    let today = crate::usage::today_str();
    // A research note cites by registry index (`sources: [3, 7]`), not by
    // alias, so the alias has to be looked up there too. Without this no
    // page written by `/research` was ever marked stale.
    let indices: Vec<String> = crate::research::registry::SourceRegistry::load(kref)
        .meta()
        .into_iter()
        .filter(|(_, _, url)| crate::research::kms_writer::url_to_filename(url) == changed_alias)
        .map(|(i, _, _)| i.to_string())
        .collect();
    let mut count = 0usize;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem == changed_alias {
            // Don't mark the freshly-written page as stale.
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut fm, body) = parse_frontmatter(&raw);
        let sources_field = match fm.get("sources") {
            Some(s) => s.clone(),
            None => continue,
        };
        let mentions = sources_entries(&sources_field)
            .into_iter()
            .any(|s| s == changed_alias || indices.iter().any(|i| i == s));
        if !mentions {
            continue;
        }
        // Not `updated:` — nothing about the page was updated, and bumping
        // it made the stalest pages sort as the freshest. `stale_since:`
        // keeps the first date: that is how long the debt has stood.
        fm.entry("stale_since".into())
            .or_insert_with(|| today.clone());
        let mut new_body = body;
        if !new_body.ends_with('\n') {
            new_body.push('\n');
        }
        new_body.push_str(&format!(
            "\n> ⚠ STALE: source `{changed_alias}` was re-ingested on {today}. Refresh this page.\n"
        ));
        let serialized = write_frontmatter(&fm, &new_body);
        if write_file(&path, serialized.as_bytes()).is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

/// One stale marker found on a page. Multiple entries per page are possible
/// when a source has been re-ingested several times without the page being
/// refreshed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleEntry {
    pub page_stem: String,
    pub source_alias: String,
    pub date: String,
}

/// Pure-read inverse of `mark_dependent_pages_stale`: walks every page and
/// returns every `> ⚠ STALE: source \`<alias>\` was re-ingested on <date>.`
/// marker found in the body. Used by `/kms wrap-up` to surface refresh debt
/// so the user (or the agent) acts on it before the session closes.
pub fn scan_stale_markers(kref: &KmsRef) -> Result<Vec<StaleEntry>> {
    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    // Anchor on the marker prefix from `mark_dependent_pages_stale`. Date
    // format is `crate::usage::today_str()` (YYYY-MM-DD); regex stays loose
    // on the date so a future format change in one place doesn't silently
    // break detection in the other.
    let re =
        regex::Regex::new(r"> ⚠ STALE: source `([^`]+)` was re-ingested on ([^.\s]+)").unwrap();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        for cap in re.captures_iter(&body) {
            out.push(StaleEntry {
                page_stem: stem.clone(),
                source_alias: cap[1].to_string(),
                date: cap[2].to_string(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.page_stem
            .cmp(&b.page_stem)
            .then(a.source_alias.cmp(&b.source_alias))
            .then(a.date.cmp(&b.date))
    });
    Ok(out)
}

/// M6.25 BUG #8: ingest a remote URL by fetching it via the existing
/// WebFetchTool then writing the response body to a temp file and
/// running `ingest()` against it. The HTML→markdown conversion is
/// out of scope — we save the raw response. Pages can be cleaned up
/// by the LLM via KmsWrite.
/// Whether a fetched body is a PDF (dev-plan/64 P4.8).
///
/// The header decides where it says anything useful. The magic bytes
/// are the fallback, because a good share of servers hand a paper over
/// as `application/octet-stream` or with no type at all — and getting
/// this wrong is not a cosmetic miss: the body would be decoded as
/// UTF-8 and archived as mush no reader or search index can use.
fn looks_like_pdf(content_type: &str, body: &[u8]) -> bool {
    content_type.contains("pdf") || body.starts_with(b"%PDF-")
}

pub async fn ingest_url(
    kref: &KmsRef,
    url: &str,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let resolved_alias = alias.map(String::from).unwrap_or_else(|| {
        // Derive an alias from the last path segment.
        url.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("page")
            .split('?')
            .next()
            .unwrap_or("page")
            .to_string()
    });
    let alias_clean = sanitize_alias(&resolved_alias);
    if alias_clean.is_empty() {
        return Err(Error::Tool(format!(
            "could not derive alias from URL '{url}' — pass --alias explicitly"
        )));
    }

    crate::net_guard::guard(url).await.map_err(Error::Tool)?;
    let client = crate::net_guard::guarded_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(crate::version::WEB_USER_AGENT)
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("fetch {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Tool(format!(
            "fetch {url}: HTTP {}",
            resp.status().as_u16()
        )));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Bytes, not text: a PDF decoded as UTF-8 is unrecoverable, and the
    // decision about what this *is* has to come before the decoding.
    let raw = resp
        .bytes()
        .await
        .map_err(|e| Error::Tool(format!("read body: {e}")))?;

    // dev-plan/64 P4.8: a URL that serves a PDF is a PDF. This branch
    // did not exist, so `resp.text()` ran over the binary and archived
    // lossy mush as `sources/<alias>.md` — reachable from the GUI's URL
    // field since P5.3, and the commonest shape of a link to a paper.
    // The header decides, with the magic bytes as the fallback for a
    // server that says `application/octet-stream`.
    if looks_like_pdf(&content_type, &raw) {
        let tmp = std::env::temp_dir().join(format!("kms-url-{alias_clean}.pdf"));
        std::fs::write(&tmp, &raw)
            .map_err(|e| Error::Tool(format!("stage {}: {e}", tmp.display())))?;
        let out = ingest_pdf(kref, &tmp, Some(&alias_clean), force, Some(url)).await;
        let _ = std::fs::remove_file(&tmp);
        return out;
    }
    let body = String::from_utf8_lossy(&raw).into_owned();

    // Archive readable Markdown, not tag soup. Pre-fix this wrote the
    // response bytes verbatim into `sources/<alias>.md`, so every web
    // ingest produced a `<!doctype html>` blob: unreadable in the
    // viewer, worthless as a search document, and the index summary
    // became whatever the first line of the HTML happened to be.
    let is_html = content_type.contains("html")
        || (content_type.is_empty() && crate::html_md::looks_like_html(&body));
    let (title, content, converted_from) = if is_html {
        let (title, md) = crate::html_md::convert(&body);
        let converted = if content_type.is_empty() {
            "text/html".to_string()
        } else {
            content_type.clone()
        };
        (title, md, Some(converted))
    } else {
        (String::new(), body, None)
    };

    // Stage to a tempfile with a markdown extension so the existing
    // ingest path accepts it.
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("kms-url-{alias_clean}.md"));
    let mut fm = std::collections::BTreeMap::new();
    fm.insert("type".to_string(), "source".to_string());
    fm.insert("source_url".to_string(), url.to_string());
    fm.insert("fetched".to_string(), crate::usage::today_str());
    if !title.is_empty() {
        fm.insert("title".to_string(), title);
    }
    if let Some(ct) = &converted_from {
        fm.insert("converted_from".to_string(), ct.clone());
    }
    let staged = write_frontmatter(&fm, &content);
    std::fs::write(&tmp_path, staged.as_bytes())
        .map_err(|e| Error::Tool(format!("stage {}: {e}", tmp_path.display())))?;
    let result = ingest_with_origin(
        kref,
        &tmp_path,
        Some(&alias_clean),
        force,
        crate::kms_sources::Origin::Url,
        url,
        converted_from.as_deref(),
    );
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// Outcome of a directory ingest.
#[derive(Debug, Default)]
pub struct BulkIngestResult {
    pub ingested: Vec<String>,
    /// `(path, reason)` for files that were skipped or failed.
    pub skipped: Vec<(String, String)>,
    pub images_copied: usize,
}

impl BulkIngestResult {
    pub fn summary(&self) -> String {
        let mut s = format!("{} file(s) ingested", self.ingested.len());
        if self.images_copied > 0 {
            s.push_str(&format!(", {} image(s) localized", self.images_copied));
        }
        if !self.skipped.is_empty() {
            s.push_str(&format!(", {} skipped", self.skipped.len()));
        }
        s
    }
}

/// Ingest every supported file under a directory.
///
/// A KMS is normally seeded from a folder of notes, a docs tree, or an
/// export — and the only ingest surface was one file per invocation, so
/// that meant either a shell loop or giving up. Recurses (bounded),
/// derives each alias from the file's path so `api/auth.md` and
/// `web/auth.md` don't collide on `auth`, and never aborts the batch
/// for one bad file.
pub fn ingest_dir(kms: &KmsRef, dir: &Path, force: bool) -> Result<BulkIngestResult> {
    ensure_writable(kms)?;
    let meta = std::fs::metadata(dir)
        .map_err(|e| Error::Tool(format!("cannot stat '{}': {e}", dir.display())))?;
    if !meta.is_dir() {
        return Err(Error::Tool(format!(
            "'{}' is not a directory — use the single-file ingest",
            dir.display()
        )));
    }
    let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut result = BulkIngestResult::default();
    let mut files: Vec<PathBuf> = Vec::new();
    collect_ingestable(&root, 0, &mut files, &mut result);
    files.sort();

    // One index rebuild for the whole batch, not one per file.
    let _batch = IndexBatch::new(kms);
    for path in files {
        let alias = alias_from_relative(&root, &path);
        match ingest(kms, &path, Some(&alias), force) {
            Ok(r) => {
                result.images_copied += r.images_copied;
                result.ingested.push(r.alias);
            }
            Err(e) => result
                .skipped
                .push((path.display().to_string(), e.to_string())),
        }
    }
    Ok(result)
}

/// Depth limit for [`ingest_dir`]. Deep enough for a docs tree,
/// shallow enough that pointing it at a home directory by accident
/// doesn't walk the world.
const INGEST_DIR_MAX_DEPTH: usize = 6;
/// Ceiling on files per directory ingest, so one command can't fill a
/// KMS with thousands of derived pages.
const INGEST_DIR_MAX_FILES: usize = 500;

fn collect_ingestable(
    dir: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
    result: &mut BulkIngestResult,
) {
    if depth > INGEST_DIR_MAX_DEPTH || out.len() >= INGEST_DIR_MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= INGEST_DIR_MAX_FILES {
            result.skipped.push((
                dir.display().to_string(),
                format!("file cap {INGEST_DIR_MAX_FILES} reached"),
            ));
            return;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if ft.is_dir() {
            collect_ingestable(&path, depth + 1, out, result);
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if INGEST_EXTENSIONS.iter().any(|e| *e == ext) {
            out.push(path);
        }
    }
}

/// `docs/api/auth.md` under root `docs` → `api-auth`. Path-derived so
/// same-named files in sibling directories stay distinct instead of
/// colliding on the bare stem.
fn alias_from_relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut parts: Vec<String> = rel
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .filter(|s| !s.is_empty() && s != ".")
                .collect()
        })
        .unwrap_or_default();
    if let Some(stem) = rel.file_stem().and_then(|s| s.to_str()) {
        parts.push(stem.to_string());
    }
    let joined = parts.join("-");
    let cleaned = sanitize_alias(&joined);
    if cleaned.is_empty() {
        "page".into()
    } else {
        cleaned
    }
}

/// M6.25 BUG #8: ingest a PDF by extracting text via pdftotext
/// (the same path PdfReadTool uses). Output is markdown with a
/// short "extracted from PDF" banner. The agent can refine it
/// with KmsWrite.
pub async fn ingest_pdf(
    kref: &KmsRef,
    pdf_path: &Path,
    alias: Option<&str>,
    force: bool,
    // `origin_url`: where the PDF really came from, when that is not the
    // path being read. A URL ingest stages the download in a temp file,
    // and the temp path is of no use to anyone who later asks where a
    // claim came from (dev-plan/64 P4.8).
    origin_url: Option<&str>,
) -> Result<IngestResult> {
    let resolved_alias = alias.map(String::from).unwrap_or_else(|| {
        pdf_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("pdf-page")
            .to_string()
    });
    let alias_clean = sanitize_alias(&resolved_alias);
    if alias_clean.is_empty() {
        return Err(Error::Tool(format!(
            "alias derived from PDF is empty — pass --alias"
        )));
    }
    // Extraction and the Thai repair travel together in one function
    // now. They used to be a copy here that carried the first and not
    // the second, which is how an ingested Thai paper kept the
    // vowel/tone fragmentation `-layout` introduces.
    let extracted = crate::tools::pdf_read::extract_text(pdf_path, None, None).await?;

    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("kms-pdf-{alias_clean}.md"));
    let origin_ref = match origin_url {
        Some(u) => u.to_string(),
        None => pdf_path
            .canonicalize()
            .unwrap_or_else(|_| pdf_path.to_path_buf())
            .display()
            .to_string(),
    };
    let mut fm = std::collections::BTreeMap::new();
    fm.insert("type".to_string(), "source".to_string());
    fm.insert("source_pdf".to_string(), origin_ref.clone());
    fm.insert("extracted".to_string(), crate::usage::today_str());
    fm.insert("converted_from".to_string(), "application/pdf".to_string());
    let staged = write_frontmatter(&fm, &extracted);
    std::fs::write(&tmp_path, staged.as_bytes())
        .map_err(|e| Error::Tool(format!("stage {}: {e}", tmp_path.display())))?;
    let result = ingest_with_origin(
        kref,
        &tmp_path,
        Some(&alias_clean),
        force,
        crate::kms_sources::Origin::Pdf,
        &origin_ref,
        Some("application/pdf"),
    );
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// Keep only `[A-Za-z0-9_-]`; collapse anything else to `_`. An empty
/// result returns empty so the caller can reject it with a useful
/// message rather than writing a page named "".
///
/// Made `pub` in M6.28 so the `/kms ingest <name> $` rewrite can
/// derive a slug from the active session's title (which may contain
/// spaces / punctuation) without re-implementing the sanitizer.
pub fn sanitize_alias(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c == '-' || c == '_' {
                c
            } else if c.is_ascii() {
                // ASCII: keep alphanumerics, fold everything else (spaces,
                // path separators, punctuation, the Windows-reserved set) to
                // '_'. The '.' folds too so it can't split the stem/extension.
                if c.is_ascii_alphanumeric() {
                    c
                } else {
                    '_'
                }
            } else if c.is_whitespace() || c.is_control() {
                '_'
            } else {
                // Non-ASCII letters and combining marks (Thai, CJK, …) are
                // valid in UTF-8 filenames — keep them so non-Latin names
                // survive instead of sanitising to empty.
                c
            }
        })
        .collect();
    cleaned.trim_matches('_').to_string()
}

// `append_index_entry` + `append_log_entry` removed in M6.25 — the
// new `update_index_for_write` and `append_log_header` (defined
// below in the BUG #1 + #7 sections) replace them with the
// frontmatter-aware index update and the greppable `## [date] verb |
// alias` log format.

/// Render the concatenated active-KMS block to splice into a system
/// prompt. One section per KMS with: SCHEMA.md (M6.25 BUG #5), the
/// index (categorized when pages have YAML frontmatter `category:`,
/// flat otherwise — M6.25 BUG #6), and the read/write/append/search
/// tool affordances.
///
/// Empty string when no active KMS or when active names resolve to
/// nothing.
pub fn system_prompt_section(active: &[String]) -> String {
    let mut parts = Vec::new();
    let mut traits = VaultTraits::default();
    for name in active {
        let Some(kref) = resolve(name) else { continue };

        // A schema the owner wrote is an instruction; the one every KMS
        // is created with is not — what it says about page shape is in
        // `KmsWrite`'s own description, and it cost 1.3 KB per attached
        // base on every request. `KmsRead(kind: "schema")` still reads it.
        let schema = read_text_capped(&kref.schema_path(), 100, 5000);
        let mut block = format!("## KMS: {name} ({scope})\n", scope = kref.scope.as_str());
        if !schema.trim().is_empty() && schema.trim() != SCHEMA_TEMPLATE.trim() {
            block.push_str(&format!("\n### Schema\n{}\n", schema.trim()));
        }
        let (header, t) = index_header_with(&kref);
        traits.provenance_pages |= t.provenance_pages;
        traits.raw_layer |= t.raw_layer;
        block.push_str(&format!("\n{header}\n"));
        parts.push(block);
    }
    if parts.is_empty() {
        return String::new();
    }
    // M6.39.5: the numbered MUST procedure, "do not skip" and the no-hits
    // sentence are what stopped models answering from training data with
    // an on-topic base attached; they are pinned by a test. dev-plan/64
    // P2.2 cut everything around them that a tool description already
    // says, and explains a kind of page only to a base that has one.
    let mut out = String::from(
        "# Active knowledge bases (CONSULT BEFORE ANSWERING)\n\n\
         The user curated the knowledge bases below for this project. On any topic \
         they cover they are authoritative over your training data.\n\n\
         **MANDATORY procedure.** When a message's subject could plausibly fall under \
         what a base below covers, your FIRST action is tool calls, before any prose:\n\n\
         1. `KmsSearch(kms: \"<name>\", query: \"<1-3 keywords>\")` — keywords in the \
         language the pages are written in; a page matches only words it contains, so a \
         translated or romanized keyword finds nothing. Thai, Chinese and Japanese work \
         as written, and any part of a word matches. Slugs are searchable; if a slug or \
         title below already looks right, read that page directly. `pattern:` is for an \
         exact shape only (a regex, an identifier).\n\
         2. `KmsRead(kms: \"<name>\", page: \"<slug>\")` for each page that matches.\n\
         3. Only then answer, citing pages inline as `(see KMS: <name>/<page>)`.\n\n\
         Do NOT skip steps 1-2 because the question seems familiar — answering without a \
         lookup when a base looks relevant is a correctness bug, not a shortcut. A long \
         base lists only some of its pages, so a missing name proves nothing: search. If \
         the search finds nothing, fall back to training-data knowledge and say so (\"the \
         KMS has nothing on this; answering from general knowledge\"). Having searched is \
         what earns that sentence.\n\n\
         A `<system-reminder>` on the user's message naming KMS pages is the result of \
         that search already run for you: read those pages before answering.\n\n\
         **KMS before the web; write back after.** Search the KMS before `WebSearch` / \
         `WebFetch`. What you do learn from the web, file with `KmsWrite` (a page named \
         for its topic) before you finish, so the next session answers from the KMS. You \
         maintain these bases as well as read them: `KmsEdit` changes part of a page \
         (prefer it to rewriting one), `KmsAppend` adds to it, `KmsCreate` starts a new base, and `KmsDelete` is a last resort — prefer \
         `KmsWrite` to merge or supersede. **Never change a base silently:** in the \
         same reply, tell the user which page you wrote and what changed — and never \
         say you wrote something until the tool call has returned.\n\n",
    );
    if traits.provenance_pages {
        out.push_str(
            "**Prefer topic pages.** A page named for its subject is the curated answer. \
             `sess-…` and `dream-…` pages are provenance and audit: read them only to \
             trace where a fact came from.\n\n",
        );
    }
    if traits.raw_layer {
        out.push_str(
            "**Two layers.** `pages/` are curated; `sources/` are the raw documents they \
             were built from, and search covers both, labelling each hit. A page marked \
             _(derived — uncurated)_ has not been written up: read its **source** \
             (`KmsRead(kind: \"source\")`) and prefer writing the page up over answering \
             from training data. An **uncited** source in `KmsRead(kind: \"index\")` is \
             material to mine before searching the web for the same thing.\n\n",
        );
    }
    out.push_str(&parts.join("\n\n"));
    out
}

/// A base's `SCHEMA.md`, for `KmsRead(kind: "schema")`. Never follows a
/// symlink, and is bounded like everything else a tool returns.
pub fn read_schema(kref: &KmsRef) -> String {
    read_text_capped(&kref.schema_path(), 200, 8_000)
}

/// Read a text file, cap by lines and bytes for prompt safety.
/// Returns "" when the file is missing or symlinked.
fn read_text_capped(path: &Path, max_lines: usize, max_bytes: usize) -> String {
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if md.file_type().is_symlink() {
            return String::new();
        }
    }
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.is_empty() {
        return raw;
    }
    crate::memory::truncate_for_prompt(
        raw.trim(),
        max_lines,
        max_bytes,
        &path.display().to_string(),
    )
}

// `render_index_section` lived here: the categorised page list, rendered
// straight into the system prompt. `full_index` is the same rendering,
// reached through `KmsRead(kind: "index")` instead of paid for on every
// turn. The legacy `raw_index_capped` fallback for KMSes with no
// frontmatter moved in there too.

/// One row of the index, gathered from a page's own frontmatter+body.
#[derive(Clone)]
struct IndexEntry {
    stem: String,
    /// Frontmatter `title:`, empty when absent. A research-built vault
    /// has English slugs and Thai titles, so the title is often the only
    /// part of a row a reader — or the model — recognises.
    title: String,
    category: String,
    summary: String,
    derived: bool,
}

/// What the scanners need from one page, parsed once per version of it.
struct ParsedPage {
    entry: IndexEntry,
    updated: String,
    /// `status:` when it marks the page as unfinished; empty otherwise.
    status: String,
    /// Outbound `[[links]]` and `pages/x.md` links, as `outbound_page_links`
    /// reports them.
    links: Vec<String>,
}

/// dev-plan/64 P3.3: read and parse a page only when it has changed.
///
/// Every `KmsRead` builds the backlink map, which read and parsed every
/// page in the vault to find the few that link here; every system-prompt
/// build walked them three times for the index header. On a 47-page vault
/// that is 47 file reads and frontmatter parses per tool call, and it grows
/// with the vault. A file's (mtime, length) says whether it changed, so a
/// scan is now one `stat` per page. External edits — Obsidian, git, another
/// agent — change one of the two and are picked up the same way.
///
/// In memory only, shared by every KMS in the process, and emptied when it
/// outgrows [`PAGE_CACHE_MAX`] rather than tracking what is least used.
fn parsed_page(path: &Path, stem: &str) -> Option<std::sync::Arc<ParsedPage>> {
    type Key = (std::time::SystemTime, u64, u64);
    const PAGE_CACHE_MAX: usize = 20_000;
    static CACHE: std::sync::Mutex<
        Option<std::collections::HashMap<PathBuf, (Key, std::sync::Arc<ParsedPage>)>>,
    > = std::sync::Mutex::new(None);

    let meta = std::fs::metadata(path).ok()?;
    // Every write here replaces the file (`write_file`), so on unix the
    // inode changes even when a same-length edit lands inside one tick of a
    // coarse mtime.
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let ino = 0u64;
    let key: Key = (meta.modified().ok()?, meta.len(), ino);
    {
        let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, page)) = guard.as_ref().and_then(|m| m.get(path)) {
            if *k == key {
                return Some(page.clone());
            }
        }
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let (fm, body) = parse_frontmatter(&raw);
    let page = std::sync::Arc::new(ParsedPage {
        entry: IndexEntry {
            stem: stem.to_string(),
            title: fm
                .get("title")
                .map(|t| t.trim().trim_matches('"').to_string())
                .unwrap_or_default(),
            category: fm
                .get("category")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "uncategorized".into()),
            summary: page_summary(&fm, &body, stem),
            derived: fm.get("status").map(|s| s.trim()) == Some("derived"),
        },
        updated: fm.get("updated").cloned().unwrap_or_default(),
        status: fm
            .get("status")
            .map(|s| s.trim().to_lowercase())
            .filter(|s| matches!(s.as_str(), "researching" | "derived" | "failed"))
            .unwrap_or_default(),
        links: outbound_page_links(&raw),
    });
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(Default::default);
    if map.len() >= PAGE_CACHE_MAX {
        map.clear();
    }
    map.insert(path.to_path_buf(), (key, page.clone()));
    Some(page)
}

/// Walk `pages/` and describe every page. This is the single reader
/// behind both the on-disk `index.md` and the prompt block — before,
/// `update_index_for_write` appended bullets to `index.md` in write
/// order while the prompt rebuilt a categorised list from frontmatter
/// and ignored `index.md` entirely, so the human and the model were
/// reading two different, diverging indexes.
fn scan_index_entries(kref: &KmsRef) -> Vec<IndexEntry> {
    let Ok(entries) = std::fs::read_dir(kref.pages_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        if let Some(page) = parsed_page(&path, stem) {
            out.push(page.entry.clone());
        }
    }
    out.sort_by(|a, b| a.category.cmp(&b.category).then(a.stem.cmp(&b.stem)));
    out
}

/// The whole page list, for `KmsRead(kind: "index")`.
///
/// This is what [`index_header`] stopped injecting. Bounded the same way
/// the injected copy was, so asking for it cannot blow the context
/// either — a base past the cap says so rather than trailing off.
pub fn full_index(kref: &KmsRef) -> String {
    let entries = scan_index_entries(kref);
    if entries.is_empty() {
        // A KMS whose pages carry no frontmatter scans to nothing here;
        // its hand-written `index.md` is all it has.
        let raw = raw_index_capped(kref);
        if raw.trim() == "(empty index)" {
            return format!("KMS '{}' has no pages yet.\n", kref.name);
        }
        return raw;
    }
    let mut out = format!("# {} — {} page(s)\n", kref.name, entries.len());
    out.push_str(&render_page_index(
        &entries,
        crate::memory::MEMORY_INDEX_MAX_LINES,
    ));
    out.push_str(&crate::kms_sources::render_index_block(kref, 40));
    // The entry cap above bounds rows, and a row of Thai is three bytes a
    // character: 200 pages came to ~80 KB. Bytes are what context costs.
    if out.len() > FULL_INDEX_MAX_BYTES {
        let mut cut = FULL_INDEX_MAX_BYTES;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(nl) = out[..cut].rfind('\n') {
            cut = nl;
        }
        out.truncate(cut);
        out.push_str(&format!(
            "\n… index cut at {} KB — the rest is reachable with KmsSearch.\n",
            FULL_INDEX_MAX_BYTES / 1024
        ));
    }
    out
}

const FULL_INDEX_MAX_BYTES: usize = 16 * 1024;

/// What a KMS announces about itself in the system prompt.
///
/// The whole page list used to go in — 15.5 KB for a 39-page base, on
/// every turn, capped by entry count rather than bytes so a 200-page
/// base would have injected around 80 KB per attached KMS. It bought
/// nothing the procedure needs: that procedure is already search-first
/// (`KmsSearch`, then `KmsRead`), and the list served only to tell the
/// model whether this base is worth searching at all. A few lines say
/// that as well as a few hundred, and `KmsRead(kind: "index")` hands
/// over the full list to a model that decides it wants one.
///
/// What must survive the cut is the relevance trigger: how big the base
/// is, what it is about, and what its categories are. The failure this
/// guards against is a model answering from training data because it
/// never realised the base was on topic — which is the failure M6.39.5
/// was written for, and why the imperative prelude around this stays.
#[cfg(test)]
fn index_header(kref: &KmsRef) -> String {
    index_header_with(kref).0
}

/// What a base contains that the prelude only needs to explain when it is
/// there.
#[derive(Default, Clone, Copy)]
struct VaultTraits {
    /// `sess-…` / `dream-…` pages: provenance and audit, not answers.
    provenance_pages: bool,
    /// Archived sources, or pages nobody has written up yet.
    raw_layer: bool,
}

fn index_header_with(kref: &KmsRef) -> (String, VaultTraits) {
    let entries = scan_index_entries(kref);
    let sources = list_sources(kref).len();
    let derived = entries.iter().filter(|e| e.derived).count();
    let traits = VaultTraits {
        provenance_pages: entries
            .iter()
            .any(|e| e.stem.starts_with("sess-") || e.stem.starts_with("dream-")),
        raw_layer: sources > 0 || derived > 0,
    };
    let mut out = format!(
        "{} page(s){}, {sources} archived source(s).\n",
        entries.len(),
        if derived > 0 {
            format!(" ({derived} uncurated)")
        } else {
            String::new()
        }
    );
    // The entry page is the one a reader lands on, so its summary is
    // the closest thing a KMS has to a statement of what it covers.
    if let Some(entry) = entry_page(kref) {
        if let Some(e) = entries.iter().find(|e| e.stem == entry) {
            let about = strip_wikilink_syntax(e.summary.trim());
            if !about.is_empty() {
                out.push_str(&format!("About: {about}\n"));
            }
        }
    }
    let mut cats: Vec<&str> = entries
        .iter()
        .map(|e| e.category.as_str())
        .filter(|c| *c != "uncategorized")
        .collect();
    cats.sort_unstable();
    cats.dedup();
    if !cats.is_empty() {
        out.push_str(&format!("Categories: {}\n", cats.join(", ")));
    }

    // Every page by name, no summaries. Search cannot yet be trusted to
    // find a Thai page (dev-plan/64 §1: ~35% of pages containing the
    // query are missed), so until it can, the prompt has to be able to
    // name each one. Names are roughly a tenth of the bytes the
    // summaries were, and the list stops at a byte budget — never at a
    // count, which for Thai titles at three bytes a character is no
    // bound at all — saying how many it left out.
    if !entries.is_empty() {
        out.push_str("Pages (`slug — title`):\n");
        let mut used = 0usize;
        let mut shown = 0usize;
        for e in &entries {
            let row = if e.title.is_empty() || e.title == e.stem {
                format!("- {}\n", e.stem)
            } else {
                format!("- {} — {}\n", e.stem, e.title)
            };
            if used + row.len() > HEADER_PAGE_LIST_BYTES {
                break;
            }
            used += row.len();
            shown += 1;
            out.push_str(&row);
        }
        if shown < entries.len() {
            out.push_str(&format!(
                "… and {} more — not listed is not the same as not there.\n",
                entries.len() - shown
            ));
        }
    }
    out.push_str("One-line summaries for all of them: `KmsRead(kms: \"…\", kind: \"index\")`.\n");
    (out, traits)
}

/// Byte budget for the page-name list in [`index_header`].
const HEADER_PAGE_LIST_BYTES: usize = 3_000;

/// `[[slug|shown]]` → `shown`, `[[slug]]` → `slug`. For text that is
/// about to be read as prose rather than rendered.
fn strip_wikilink_syntax(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("[[") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        match after.find("]]") {
            Some(close) => {
                let inner = &after[..close];
                out.push_str(
                    inner
                        .rsplit_once('|')
                        .map(|(_, shown)| shown)
                        .unwrap_or(inner),
                );
                rest = &after[close + 2..];
            }
            // A clipped summary can end mid-link; keep what is there.
            None => {
                out.push_str(
                    after
                        .rsplit_once('|')
                        .map(|(_, shown)| shown)
                        .unwrap_or(after),
                );
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render the categorised page list. `cap` bounds the entry count so
/// a large KMS can't crowd out the rest of the system prompt.
fn render_page_index(entries: &[IndexEntry], cap: usize) -> String {
    let mut out = String::new();
    let mut current = "";
    for (shown, e) in entries.iter().enumerate() {
        if shown >= cap {
            out.push_str(&format!(
                "\n_… index truncated at {cap} entries (total: {})_\n",
                entries.len()
            ));
            break;
        }
        if e.category != current {
            out.push_str(&format!("\n**{}**\n", e.category));
            current = &e.category;
        }
        // A derived page is an ingest nobody has curated yet. Saying
        // so in the index stops the model citing a machine-generated
        // outline as a vetted answer, and shows the maintainer where
        // the backlog is.
        let mark = if e.derived {
            " _(derived — uncurated)_"
        } else {
            ""
        };
        out.push_str(&format!(
            "- [{}](pages/{}.md) — {}{mark}\n",
            e.stem, e.stem, e.summary
        ));
    }
    out
}

thread_local! {
    /// Depth of the enclosing [`IndexBatch`] guards. Non-zero means a
    /// bulk operation is running and per-write index regeneration is
    /// deferred to the guard's drop.
    static INDEX_BATCH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Suppress per-write index regeneration for the duration of a bulk
/// operation, then rebuild once.
///
/// `rebuild_index` reads every page, and it runs after every single
/// page write. That is right for one write and quadratic for a batch —
/// a 500-file directory ingest would do 250k page reads. Bulk callers
/// (directory ingest, merge, OKF import) hold this guard instead.
pub struct IndexBatch {
    kref: KmsRef,
}

impl IndexBatch {
    pub fn new(kref: &KmsRef) -> Self {
        INDEX_BATCH.with(|c| c.set(c.get() + 1));
        Self { kref: kref.clone() }
    }
}

impl Drop for IndexBatch {
    fn drop(&mut self) {
        let last = INDEX_BATCH.with(|c| {
            let n = c.get().saturating_sub(1);
            c.set(n);
            n == 0
        });
        if last && !self.kref.read_only() {
            // Best-effort: the guard runs on unwind too, and failing to
            // regenerate an index must not mask the original error.
            let _ = rebuild_index(&self.kref);
        }
    }
}

fn index_batch_active() -> bool {
    INDEX_BATCH.with(|c| c.get() > 0)
}

/// Regenerate `index.md` from what is actually on disk. Called after
/// every mutating KMS operation so the on-disk index can no longer
/// drift from the pages, and directly by `/kms reindex`.
pub fn rebuild_index(kref: &KmsRef) -> Result<usize> {
    let entries = scan_index_entries(kref);
    let count = entries.len();
    let mut out = format!("# {} — index\n", kref.name);
    out.push_str(
        "\n_Generated by thClaws on every KMS write — edits here are overwritten. \
         Change a page's `summary:` frontmatter or its lead paragraph instead._\n",
    );
    if entries.is_empty() {
        out.push_str("\n_No pages yet._\n");
    } else {
        out.push_str(&render_page_index(&entries, usize::MAX));
    }
    out.push_str(&crate::kms_sources::render_index_block(kref, usize::MAX));
    let path = kref.index_path();
    write_file(&path, out.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(count)
}

/// What [`reindex`] rebuilt.
#[derive(Debug, Default)]
pub struct ReindexReport {
    pub pages: usize,
    pub sources: usize,
    /// Provenance-catalogue changes (backfilled / dropped / refreshed).
    pub catalog_changes: usize,
    /// Documents in the BM25 index, `None` when the binary was built
    /// without the `kms_search_index` feature.
    pub indexed: Option<usize>,
}

/// Rebuild everything derived: the source provenance catalogue, the
/// on-disk `index.md`, and the BM25 index.
///
/// `/kms reindex` used to rebuild the BM25 index alone, so the two
/// other derived artefacts had no repair path at all — a hand-edited
/// page, a merge, or a source dropped in by hand left `index.md` and
/// the catalogue permanently wrong with nothing to fix them. All three
/// are regenerated from disk here, and the catalogue + index halves
/// work in builds without the search feature.
pub fn reindex(kref: &KmsRef) -> Result<ReindexReport> {
    let mut report = ReindexReport::default();
    if !kref.read_only() {
        report.catalog_changes = crate::kms_sources::reconcile(kref)?.total();
        report.pages = rebuild_index(kref)?;
    } else {
        report.pages = scan_index_entries(kref).len();
    }
    report.sources = list_sources(kref).len();
    #[cfg(feature = "kms_search_index")]
    if !kref.read_only() {
        report.indexed = Some(
            crate::kms_search_index::full_rebuild(&kref.root)
                .map_err(|e| Error::Tool(format!("rebuild search index: {e}")))?,
        );
    }
    Ok(report)
}

impl ReindexReport {
    pub fn summary(&self) -> String {
        let mut s = format!("{} page(s), {} source(s)", self.pages, self.sources);
        if self.catalog_changes > 0 {
            s.push_str(&format!(", {} catalog fix(es)", self.catalog_changes));
        }
        match self.indexed {
            Some(n) => s.push_str(&format!(", {n} doc(s) indexed")),
            None => s.push_str(" (no search index — built without `kms_search_index`)"),
        }
        s
    }
}

/// A page's index summary. Explicit `summary:` / `topic:` frontmatter
/// wins; otherwise the first line of real prose.
///
/// The fallback used to be "first non-blank line of the body", which
/// `maybe_inject_canonical_header` had just filled with `# <title>` —
/// so nearly every bullet read `- [welsh-corgi](…) — Welsh Corgi`,
/// restating its own link text and telling the model nothing about
/// whether the page was worth opening.
fn page_summary(fm: &std::collections::BTreeMap<String, String>, body: &str, stem: &str) -> String {
    for key in ["summary", "topic", "description"] {
        if let Some(v) = fm.get(key).map(|s| s.trim().trim_matches('"')) {
            if !v.is_empty() {
                return clip(v, 120);
            }
        }
    }
    let title_norm = normalize_for_compare(stem);
    let mut in_fence = false;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || t.is_empty() {
            continue;
        }
        // Skip the injected canonical header block, rules, callout
        // blockquotes and table scaffolding — none of them summarise
        // the page.
        if t.starts_with('#')
            || t.starts_with("---")
            || t.starts_with("===")
            || t.starts_with('>')
            || t.starts_with("<!--")
            || t.starts_with('|')
        {
            continue;
        }
        if let Some(rest) = t.strip_prefix("Description:") {
            let rest = rest.trim();
            if !rest.is_empty() {
                return clip(rest, 120);
            }
            continue;
        }
        let cleaned = t.trim_start_matches(['-', '*', '+']).trim();
        if cleaned.is_empty() {
            continue;
        }
        // A first line that only repeats the page name is no summary.
        if normalize_for_compare(cleaned) == title_norm {
            continue;
        }
        return clip(cleaned, 120);
    }
    "(no summary — add `summary:` frontmatter or a lead paragraph)".into()
}

fn normalize_for_compare(s: &str) -> String {
    fold_for_compare(s)
}

/// Scripts written without spaces between words — Thai, Lao, Khmer,
/// Myanmar, Chinese, Japanese kana. Anything that needs a "word" from
/// running text has to treat these differently: there is no delimiter to
/// split on, so the search index matches them by overlapping character
/// pairs and the autolinker only links them where something else sets
/// them apart.
pub fn is_spaceless_script(c: char) -> bool {
    matches!(
        c as u32,
        0x0E00..=0x0E7F      // Thai
            | 0x0E80..=0x0EFF  // Lao
            | 0x1000..=0x109F  // Myanmar
            | 0x1780..=0x17FF  // Khmer
            | 0x3040..=0x30FF  // Hiragana, Katakana
            | 0x3400..=0x4DBF  // CJK extension A
            | 0x4E00..=0x9FFF // CJK
    )
}

/// Case-, space- and punctuation-insensitive key for deciding whether two
/// names are the same name.
///
/// This was `filter(is_alphanumeric)`, which reads as harmless and is not:
/// Rust counts Thai vowel signs as alphabetic but not the tone marks
/// (U+0E48–0E4B) or thanthakhat, so the filter did not skip Thai, it
/// respelled it. `ก้าว` ("step") and `กาว` ("glue") got one key, as did
/// `หน้า`/`หนา` and `เสื้อ`/`เสือ` — and the research planner merges notes
/// on this key. So: ASCII is filtered by class, everything else is kept
/// unless it is spacing, an invisible format character, or general
/// punctuation.
pub fn fold_for_compare(s: &str) -> String {
    s.chars()
        .filter(|c| {
            if c.is_ascii() {
                c.is_ascii_alphanumeric()
            } else {
                !c.is_whitespace()
                    && !matches!(*c, '\u{2000}'..='\u{206F}' | '\u{00AD}' | '\u{FEFF}')
            }
        })
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Shorten an index summary to `max` characters, ending somewhere a
/// reader would.
///
/// `index.md` is injected into the system prompt whenever a KMS is
/// attached, and `raw_index_capped` truncates it by bytes — so a longer
/// bullet does not merely cost tokens, it pushes later pages out of the
/// model's view entirely. The cap stays. What changes is where it lands:
/// a sentence end inside the budget if there is one, otherwise the last
/// space. Thai puts spaces between phrases rather than words and often
/// carries no sentence-ending punctuation at all, so that fallback is the
/// one that usually does the work, and it beats stopping mid-syllable.
/// A boundary in the first 60% is ignored — a stray abbreviation should
/// not cost most of the summary.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    let floor = head.len() * 3 / 5;
    let cut = head
        .char_indices()
        .filter(|(_, c)| matches!(c, '.' | '!' | '?' | '。' | '！' | '？'))
        // Past the terminator, not onto it — and by the character's own
        // width, because the CJK ones are three bytes wide and `+ 1`
        // would land inside them.
        .map(|(i, c)| i + c.len_utf8())
        .filter(|i| *i >= floor)
        .next_back()
        .or_else(|| head.rfind(' ').filter(|i| *i >= floor))
        .unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end())
}

fn raw_index_capped(kref: &KmsRef) -> String {
    let index = kref.read_index();
    if index.trim().is_empty() {
        return "(empty index)".into();
    }
    crate::memory::truncate_for_prompt(
        index.trim(),
        crate::memory::MEMORY_INDEX_MAX_LINES,
        crate::memory::MEMORY_INDEX_MAX_BYTES,
        &format!("KMS index `{}`", kref.name),
    )
}

/// First non-empty line of body text, stripped of markdown markers,
/// trimmed to 80 chars. Used for index summaries.
fn first_meaningful_line(body: &str) -> String {
    for line in body.lines() {
        let stripped = line.trim_start_matches(|c: char| {
            c == '#' || c == '-' || c == '*' || c == '>' || c.is_whitespace()
        });
        let trimmed = stripped.trim();
        if !trimmed.is_empty() {
            let mut s: String = trimmed.chars().take(80).collect();
            if trimmed.chars().count() > 80 {
                s.push('…');
            }
            return s;
        }
    }
    "(empty)".into()
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #9: YAML frontmatter convention for KMS pages.
//
// Tiny, hand-rolled parser — we deliberately don't pull in `serde_yaml`
// for this. Pages either start with `---\n<key>: <value>\n...\n---\n`
// or they don't. Values are flat strings (single line), no nesting,
// no anchors, no multiline. That matches the documented convention
// (`category:`, `tags:`, `sources:`, `created:`, `updated:`) — anything
// fancier should live in the page body, not the metadata.

/// A line that belongs to the key above it: indented, or a list item.
fn is_fm_continuation(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t') || line == "-" || line.starts_with("- ")
}

/// `- item` with nothing nested under it and no inline map.
fn is_plain_list_item(line: &str) -> bool {
    let t = line.trim();
    let Some(item) = t.strip_prefix('-') else {
        return false;
    };
    let item = item.trim();
    let quoted = item.starts_with('"') || item.starts_with('\'');
    !item.is_empty() && !item.starts_with('-') && (quoted || !item.contains(": "))
}

/// What the quotes around a scalar mean, undone. The writer escapes `\"`;
/// the parser used to strip the outer quotes and leave the escapes, so each
/// round trip added a backslash.
fn unquote_fm_scalar(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(n @ ('"' | '\\')) => out.push(n),
                    Some(n) => {
                        out.push('\\');
                        out.push(n);
                    }
                    None => out.push('\\'),
                },
                c => out.push(c),
            }
        }
        return out;
    }
    if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        return v[1..v.len() - 1].replace("\'\'", "\'");
    }
    v.to_string()
}

fn quote_fm_item(item: &str) -> String {
    let plain = !item.is_empty()
        && !item.contains([',', '[', ']', '{', '}', '"', '#', ':', '\''])
        && item == item.trim();
    if plain {
        item.to_string()
    } else {
        format!("\"{}\"", item.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Parse `(frontmatter, body)` from a page. Frontmatter map preserves
/// insertion order via Vec under the hood (BTreeMap is fine — keys
/// are conventional and small). Returns `(empty, original)` when no
/// frontmatter delimiter present.
/// The entries of a `sources:` frontmatter value.
///
/// One parser, because there were four, and they disagreed. The field
/// carries three different vocabularies depending on who wrote the page
/// — registry indices from `/research` (`sources: [3, 7]`), session ids
/// from `/dream` (`sources: ["sess-abc"]`), and a bare archive alias
/// from an ingest (`sources: my-paper`) — and each reader hand-rolled a
/// tokeniser for the shape it expected. `kms_sources` stripped quotes
/// but not brackets, so the first entry of every flow list reached it as
/// `["sess-abc` and matched nothing.
///
/// Brackets come off the value once, then entries split on commas and
/// whitespace, then quotes and any stray bracket come off each entry.
/// Skip what nothing can check with [`source_entry_is_external_provenance`].
pub fn sources_entries(raw: &str) -> Vec<&str> {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|t| {
            t.trim()
                .trim_matches(|c| matches!(c, '"' | '\'' | '[' | ']'))
                .trim()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// Is this `sources:` entry provenance that no archive and no registry
/// can be expected to know?
///
/// A URL, a `session-`/`sess-` id and the literal `memory` are all
/// legitimate values that name nothing on disk, and reporting them as
/// missing archives is how `/kms lint` used to cry wolf on a vault
/// `/dream` had written to.
///
/// A citation index is deliberately NOT external: it is checkable, just
/// against `.research/sources.json` rather than the `sources/` folder,
/// and the caller does that. Folding "is it a file?" and "should I look
/// at it at all?" into one predicate is what made lint stop reporting an
/// index the registry had never heard of.
pub fn source_entry_is_external_provenance(entry: &str) -> bool {
    entry.starts_with("http")
        || entry.starts_with("session-")
        || entry.starts_with("sess-")
        || entry == "memory"
}

pub fn parse_frontmatter(s: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let mut map = std::collections::BTreeMap::new();
    let trimmed = s.trim_start_matches('\u{FEFF}');
    let Some(after_open) = trimmed.strip_prefix("---\n") else {
        return (map, s.to_string());
    };
    // Find the closing `---\n` (or `---` at EOF) anchored to start-of-line.
    let close_idx = after_open.find("\n---\n").or_else(|| {
        if after_open.ends_with("\n---") {
            Some(after_open.len() - 4)
        } else {
            None
        }
    });
    let Some(close) = close_idx else {
        return (map, s.to_string());
    };
    let yaml = &after_open[..close];
    let body = if close + 5 <= after_open.len() {
        // skip "\n---\n"
        &after_open[close + 5..]
    } else {
        ""
    };
    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        i += 1;
        if line.is_empty() || line.starts_with('#') || is_fm_continuation(line) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let first = v.trim();
        // dev-plan/64 P3.4: a value can run on. A block list, a folded or
        // literal scalar and a nested map all put their content on the
        // lines below the key, and this parser read one line: `tags:` came
        // back empty and the next `write_frontmatter` — any append, edit or
        // stale mark — wrote it back empty. Obsidian writes lists this way.
        let opens_block =
            first.is_empty() || matches!(first, ">" | "|" | ">-" | "|-" | ">+" | "|+");
        let mut block: Vec<&str> = Vec::new();
        if opens_block {
            while i < lines.len() && (lines[i].trim().is_empty() || is_fm_continuation(lines[i])) {
                block.push(lines[i].trim_end());
                i += 1;
            }
            while block.last().is_some_and(|l| l.trim().is_empty()) {
                block.pop();
            }
        }
        let val = if block.is_empty() {
            unquote_fm_scalar(first)
        } else if first.is_empty()
            && block
                .iter()
                .all(|l| l.trim().is_empty() || is_plain_list_item(l))
        {
            // A plain block list becomes the flow list every reader of
            // `tags:` / `sources:` / `related:` already understands.
            let items: Vec<String> = block
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| quote_fm_item(&unquote_fm_scalar(l.trim()[1..].trim())))
                .collect();
            format!("[{}]", items.join(", "))
        } else {
            // Anything else is kept exactly as written and written back so.
            format!("{first}\n{}", block.join("\n"))
        };
        map.insert(key, val);
    }
    (map, body.to_string())
}

/// Serialize a frontmatter map + body into a page string. Empty map →
/// just the body (no `---` block).
pub fn write_frontmatter(map: &std::collections::BTreeMap<String, String>, body: &str) -> String {
    if map.is_empty() {
        return body.to_string();
    }
    let mut out = String::from("---\n");
    for (k, v) in map {
        // A YAML flow collection (`[a, b]` / `{a: b}`) is already valid
        // YAML — emit it verbatim. Quoting it would turn a real list
        // like `sources: ["session-x", "session-y"]` into the opaque
        // string `"[\"session-x\", \"session-y\"]"`, which breaks every
        // consumer that expects `sources:` to be a sequence. Single-line
        // only — a flow value with a newline can't be emitted inline, so
        // fall through to quoting.
        // A block kept verbatim by the parser (folded/literal scalar,
        // nested map, list of maps) goes back exactly as it came.
        let first = v.lines().next().unwrap_or("");
        if v.contains('\n')
            && (first.is_empty() || matches!(first, ">" | "|" | ">-" | "|-" | ">+" | "|+"))
        {
            if first.is_empty() {
                out.push_str(&format!("{k}:{v}\n"));
            } else {
                out.push_str(&format!("{k}: {v}\n"));
            }
            continue;
        }
        let is_flow_collection = !v.contains('\n')
            && ((v.starts_with('[') && v.ends_with(']'))
                || (v.starts_with('{') && v.ends_with('}')));
        // YAML-safe scalars: if the value contains `:`, `#`, leading
        // whitespace, or quote chars, wrap in double quotes and
        // escape internal double quotes.
        let needs_quote = !is_flow_collection
            && (v.contains(':')
                || v.contains('#')
                || v.starts_with(' ')
                || v.contains('"')
                || v.contains('\n'));
        if needs_quote {
            let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!("{k}: \"{escaped}\"\n"));
        } else {
            out.push_str(&format!("{k}: {v}\n"));
        }
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #1 + #4: write helpers for KMS pages.
//
// `KmsWrite` / `KmsAppend` tools and the `/kms file-answer` slash
// command bypass `Sandbox::check_write` to land inside the KMS root
// (project-scope `.thclaws/state/kms/.../pages/...` is otherwise blocked).
// Same pattern as TodoWrite's intentional `.thclaws/state/todos.md` carve-
// out: the path is computed from a validated KMS name + a validated
// page name (no `..`, no path separators, no symlinks, must resolve
// inside the KMS root via `KmsRef::page_path`-style canonicalization).
//
// We don't want the LLM passing an arbitrary file path here.

/// Resolve `page_name` to a writable path inside `kref.pages_dir()`.
/// Differs from `KmsRef::page_path` — that one requires the file to
/// EXIST so canonicalize works. This one is for create-or-replace, so
/// it canonicalizes the parent directory and ensures the candidate
/// resolves under it.
pub fn writable_page_path(kref: &KmsRef, page_name: &str) -> Result<PathBuf> {
    if page_name.is_empty()
        || page_name.contains("..")
        || page_name.contains('/')
        || page_name.contains('\\')
        || page_name.contains('\0')
        || page_name.chars().any(|c| c.is_control())
        || Path::new(page_name).is_absolute()
    {
        return Err(Error::Tool(format!(
            "invalid page name '{page_name}' — no '..', path separators, or control chars"
        )));
    }
    let stem = page_name.trim_end_matches(".md");
    if RESERVED_PAGE_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
    {
        return Err(Error::Tool(format!(
            "page name '{page_name}' is reserved — pick another stem"
        )));
    }
    let name = if page_name.ends_with(".md") {
        page_name.to_string()
    } else {
        format!("{page_name}.md")
    };

    let pages_dir = kref.pages_dir();
    std::fs::create_dir_all(&pages_dir)
        .map_err(|e| Error::Tool(format!("ensure pages dir for '{}': {e}", kref.name)))?;
    // Refuse if pages/ itself is a symlink (would let an attacker
    // redirect writes outside the KMS root).
    if let Ok(md) = std::fs::symlink_metadata(&pages_dir) {
        if md.file_type().is_symlink() {
            return Err(Error::Tool(format!(
                "kms '{}' has a symlinked pages/ directory — refusing to write",
                kref.name
            )));
        }
    }
    let canon_pages = std::fs::canonicalize(&pages_dir)
        .map_err(|e| Error::Tool(format!("canonicalize pages dir: {e}")))?;
    let candidate = canon_pages.join(&name);
    // The candidate may not exist yet (create case) — verify the
    // parent canonicalizes inside pages_dir, and that the file
    // (if it exists) is not a symlink to outside.
    if let Ok(canon_existing) = std::fs::canonicalize(&candidate) {
        if !canon_existing.starts_with(&canon_pages) {
            return Err(Error::Tool(format!(
                "page '{page_name}' resolves outside pages/ — symlink escape rejected"
            )));
        }
    }
    Ok(candidate)
}

/// Write (create-or-replace) a page. Bumps `updated:` frontmatter to
/// today, preserves existing other frontmatter when the body itself
/// includes a `---` block. Updates the index.md bullet under the
/// page's category. Appends a log entry.
/// dev-plan/36 Tier 1.D: notify the BM25 index that a page changed.
/// No-op when the `kms_search_index` Cargo feature is off; called
/// from every successful page mutation in this module
/// (write_page / append_to_page / delete_page / rename_page /
/// merge_into / auto_link). Errors inside the indexer are logged
/// + swallowed there — the underlying KMS write has already
/// succeeded and shouldn't roll back due to index drift.
fn fire_index_upsert(kref: &KmsRef, page_stem: &str) {
    #[cfg(feature = "kms_search_index")]
    crate::kms_search_index::on_page_mutated(
        &kref.root,
        page_stem,
        crate::kms_search_index::Op::Upsert,
    );
    #[cfg(not(feature = "kms_search_index"))]
    let _ = (kref, page_stem);
}

/// A dropped or renamed KMS must not leave its index handle — and the
/// directory lock with it — cached under a path that no longer exists.
fn drop_search_handle(kref: &KmsRef) {
    #[cfg(feature = "kms_search_index")]
    crate::kms_search_index::drop_cached(&kref.root);
    #[cfg(not(feature = "kms_search_index"))]
    let _ = kref;
}

/// The index lives beside the vault (dev-plan/64 D7), so a vault that is
/// dropped or renamed leaves it behind under the old name. It is a cache:
/// remove it, and the next search rebuilds one where it belongs.
fn drop_search_index(kref: &KmsRef) {
    #[cfg(feature = "kms_search_index")]
    {
        let _ = std::fs::remove_dir_all(crate::kms_search_index::index_dir(&kref.root));
    }
    #[cfg(not(feature = "kms_search_index"))]
    let _ = kref;
}

fn fire_index_delete(kref: &KmsRef, page_stem: &str) {
    #[cfg(feature = "kms_search_index")]
    crate::kms_search_index::on_page_mutated(
        &kref.root,
        page_stem,
        crate::kms_search_index::Op::Delete,
    );
    #[cfg(not(feature = "kms_search_index"))]
    let _ = (kref, page_stem);
}

/// Reject any mutation of a read-only shared-agent KMS (dev-plan/41).
/// Guards the core write paths so slash commands, ingest, and merge are
/// covered uniformly — not just the model-callable tools.
fn ensure_writable(kref: &KmsRef) -> Result<()> {
    if kref.read_only() {
        return Err(Error::Tool(format!(
            "KMS '{}' is read-only (shared agent) — fork the agent to edit it",
            kref.name
        )));
    }
    Ok(())
}

/// Is the page on disk one an ingest wrote and nobody has touched?
///
/// `status: derived` is the ingest's marker for "not curated yet"; a
/// write that replaces the body drops it. Used to decide whether an
/// overwrite is worth keeping a copy of.
fn page_is_uncurated_stub(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .map(|raw| parse_frontmatter(&raw).0.get("status").map(|s| s.trim()) == Some("derived"))
        .unwrap_or(false)
}

pub fn write_page(kref: &KmsRef, page_name: &str, content: &str) -> Result<PathBuf> {
    ensure_writable(kref)?;
    let path = writable_page_path(kref, page_name)?;
    // Whatever a vault starts with is what it is about, so the first
    // page created becomes its entry page. Recorded once: after this
    // the manifest holds an answer and nothing overwrites it except
    // `/kms entry`, a rename, or deleting that page.
    let is_first_page = page_count(kref) == 0 && !path.exists();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    let existed = path.exists();

    // Merge user-supplied content's frontmatter with auto-stamped
    // `updated:` (and `created:` on new pages). User-supplied keys
    // win on conflict — they explicitly set them.
    let (mut fm, body) = parse_frontmatter(content);
    let today = crate::usage::today_str();
    fm.entry("updated".into()).or_insert_with(|| today.clone());
    // A page written without its stale marker has been refreshed; the
    // debt date goes with the marker, however the caller got its frontmatter.
    if !body.contains("⚠ STALE:") {
        fm.remove("stale_since");
    }
    if existed {
        // A rewrite carries no `created:` unless the caller re-supplied
        // one, and the creation date is not the caller's to forget —
        // `/research` rewrites a note on every run and used to reset it.
        if !fm.contains_key("created") {
            if let Some(prev) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| parse_frontmatter(&raw).0.remove("created"))
                .filter(|c| !c.trim().is_empty())
            {
                fm.insert("created".into(), prev);
            }
        }
    } else {
        fm.entry("created".into()).or_insert(today.clone());
    }
    // Canonical page header: `# {title}\n\n` injected between the
    // frontmatter and the body. Skipped when the body already starts
    // with its own `# heading` — model gets to keep an intentional
    // title (e.g. dream's "Dream consolidation — YYYY-MM-DD"). title
    // falls back to the page stem when frontmatter `title:` is absent.
    // Re-writes are idempotent because `body_has_leading_heading`
    // detects the previously-injected `# title` and skips re-injection.
    let canonical_body = maybe_inject_canonical_header(&body, &stem, &fm);
    let serialized = write_frontmatter(&fm, &canonical_body);
    // Keep the version being replaced — unless it is an uncurated stub.
    //
    // An ingest writes `pages/<alias>.md` with `status: derived` and the
    // research pipeline then overwrites it with the real page seconds
    // later, so every ingest used to leave the user a Trash row for a
    // version they never saw and would never want back. A stub is
    // derived entirely from a source file that is still archived, so
    // nothing is lost by not keeping it; and a page a person has edited
    // drops the marker on that write, so their work is kept as before.
    if existed && !page_is_uncurated_stub(&path) {
        crate::kms_trash::keep_before_overwrite(
            kref,
            &format!("pages/{stem}.md"),
            serialized.as_bytes(),
        );
    }
    write_file(&path, serialized.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;

    // Index summary: prefer the `topic:` frontmatter — it's a purpose-
    // built one-line description of the page ("Dog breed profile — Welsh
    // Corgi"), exactly what signals relevance when scanning the index.
    // Fall back to the first real body line only when `topic:` is
    // absent. (Pre-fix this always used the first body line, so pages
    // whose body opens with a section heading surfaced as a useless
    // "Overview".) The title is already the index link text, so we don't
    // repeat it here.
    let summary = fm
        .get("topic")
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let mut s: String = t.chars().take(80).collect();
            if t.chars().count() > 80 {
                s.push('…');
            }
            s
        })
        .unwrap_or_else(|| first_meaningful_line(&body));
    let category = fm.get("category").cloned();
    update_index_for_write(kref, &stem, &summary, category.as_deref(), existed)?;
    append_log_header(kref, if existed { "edited" } else { "wrote" }, &stem)?;
    fire_index_upsert(kref, &stem);
    if is_first_page && kref.read_manifest().and_then(|m| m.entry).is_none() {
        let _ = set_entry_page(kref, Some(&stem));
    }
    Ok(path)
}

/// Inject the canonical KMS-page header — `# {title}\n\n` — between
/// the frontmatter close and the body, when the body doesn't already
/// start with its own `# heading`. Lenient by design: a model that
/// intentionally wrote its own title (e.g. dream's "Dream
/// consolidation — YYYY-MM-DD" or a research-pipeline page with a
/// specifically-formatted title line) gets left alone. Pages that
/// arrived as pure body — common when the model treats KmsWrite as a
/// dump-content sink — get the canonical shape stamped on so the
/// vault stays readable.
///
/// `title:` missing or empty → the page stem verbatim (e.g.
/// `dream-2026-05-11`). Ugly but always present — the alternative is
/// failing the write, which corrodes UX more than a stem-titled page
/// corrodes the index.
///
/// dev-plan/64 P3.9: the header used to carry a `Description: {topic}`
/// line and a `---` rule as well. Both were noise — `topic:` is in the
/// frontmatter, which the index and the trust strip already read, and
/// the rule rendered as a second horizontal line directly under the
/// title's own underline. A page written before this still has them;
/// `strip_legacy_header` takes them off the next time it is written.
fn maybe_inject_canonical_header(
    body: &str,
    stem: &str,
    fm: &std::collections::BTreeMap<String, String>,
) -> String {
    if body_has_leading_heading(body) {
        return strip_legacy_header(body);
    }
    let title = fm
        .get("title")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(stem);

    let mut out = String::from("\n");
    out.push_str("# ");
    out.push_str(title);
    out.push_str("\n\n");
    out.push_str(body.trim_start());
    out
}

/// Take the pre-P3.9 header remnant off a body that opens with a
/// heading: the `Description: …` line and the `---` rule that used to
/// follow it. Blank lines between them are crossed — on a refreshed
/// page the rule sits a line below the title — but the scan stops at
/// the first real content, so a rule anywhere else in the page
/// survives untouched.
fn strip_legacy_header(body: &str) -> String {
    let mut lines: Vec<&str> = body.split('\n').collect();
    let Some(i) = lines.iter().position(|l| !l.trim().is_empty()) else {
        return body.to_string();
    };
    if !lines[i].trim_start().starts_with("# ") {
        return body.to_string();
    }
    let mut j = i + 1;
    let mut saw_legacy = false;
    while j < lines.len() {
        let t = lines[j].trim();
        if t.is_empty() {
            j += 1;
        } else if t == "---" || t.starts_with("Description:") {
            saw_legacy = true;
            j += 1;
        } else {
            break;
        }
    }
    if !saw_legacy {
        return body.to_string();
    }
    lines.splice(i + 1..j, std::iter::once(""));
    lines.join("\n")
}

/// Detect whether the body opens with a `# ` ATX heading — the signal
/// that the model wrote its own title block and we should leave it
/// alone (idempotent re-writes + respect for intentional formatting).
/// Skips leading whitespace so trailing-newline noise from the
/// frontmatter parse doesn't fool the check.
fn body_has_leading_heading(body: &str) -> bool {
    body.trim_start().starts_with("# ")
}

/// Append a chunk to a page. If the page doesn't exist, create it
/// (no frontmatter — the model can write a full page later via
/// `KmsWrite` to add metadata). Bumps `updated:` if frontmatter
/// already present.
pub fn append_to_page(kref: &KmsRef, page_name: &str, chunk: &str) -> Result<PathBuf> {
    ensure_writable(kref)?;
    use std::io::Write;
    let path = writable_page_path(kref, page_name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    let existed = path.exists();
    if existed {
        // Bump updated: in frontmatter if present, leave body alone,
        // append the new chunk after a newline.
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut fm, body) = parse_frontmatter(&raw);
        if !fm.is_empty() {
            fm.insert("updated".into(), crate::usage::today_str());
            let mut new_body = body;
            if !new_body.ends_with('\n') {
                new_body.push('\n');
            }
            new_body.push_str(chunk);
            let serialized = write_frontmatter(&fm, &new_body);
            write_file(&path, serialized.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        } else {
            // No frontmatter — straight append.
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
            if !raw.ends_with('\n') {
                writeln!(f).ok();
            }
            f.write_all(chunk.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        }
    } else {
        // Create with bare body (no frontmatter); subsequent
        // writes can add metadata.
        std::fs::write(&path, chunk.as_bytes())
            .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        let summary = first_meaningful_line(chunk);
        update_index_for_write(kref, &stem, &summary, None, false)?;
    }
    append_log_header(kref, "appended", &stem)?;
    fire_index_upsert(kref, &stem);
    Ok(path)
}

/// Replace one exact span of a page's text — frontmatter included — and
/// leave every other byte as it was.
///
/// Until this existed the only way to change a word was `write_page` with
/// the whole page. A session spent 16.7k output tokens writing one 19 KB
/// page twice to fix a detail; `/dream` reported a typo and left it because
/// "it would mean rewriting the whole page"; and every such rewrite is a
/// chance to write back less than was read. An edit cannot lose what it
/// does not mention.
///
/// `old` must occur exactly once unless `replace_all`: an edit that lands
/// somewhere other than where the caller was looking is worse than one that
/// is refused. Returns the path and how many spans were replaced.
pub fn edit_page(
    kref: &KmsRef,
    page_name: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(PathBuf, usize)> {
    ensure_writable(kref)?;
    if old.is_empty() {
        return Err(Error::Tool("`old` is empty — nothing to find".into()));
    }
    if old == new {
        return Err(Error::Tool("`old` and `new` are the same".into()));
    }
    let path = kref.page_path(page_name)?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
    let hits = raw.matches(old).count();
    match hits {
        0 => {
            return Err(Error::Tool(format!(
                "`old` does not occur in '{page_name}'. It must match the page text exactly, \
                 including spaces and line breaks — KmsRead the page (or the section) and copy it."
            )))
        }
        n if n > 1 && !replace_all => {
            return Err(Error::Tool(format!(
                "`old` occurs {n} times in '{page_name}'. Include more of the surrounding text \
                 so it is unique, or pass `replace_all: true`."
            )))
        }
        _ => {}
    }
    let edited = if replace_all {
        raw.replace(old, new)
    } else {
        raw.replacen(old, new, 1)
    };
    // Through `write_page` for everything a write owes the vault — index
    // row, log line, search index — with `updated:` moved to today, which
    // `write_page` on its own leaves alone when the page already has one.
    let (mut fm, body) = parse_frontmatter(&edited);
    let content = if fm.is_empty() {
        edited
    } else {
        fm.insert("updated".into(), crate::usage::today_str());
        write_frontmatter(&fm, &body)
    };
    let path = write_page(kref, page_name, &content)?;
    Ok((path, if replace_all { hits } else { 1 }))
}

/// Delete a KMS page. Validates the name via `writable_page_path`
/// (same path-safety carve-out as write/append), removes the file,
/// strips the matching bullet from `index.md`, and appends a
/// `## [YYYY-MM-DD] deleted | <stem>` entry to `log.md`.
pub fn delete_page(kref: &KmsRef, page_name: &str) -> Result<PathBuf> {
    ensure_writable(kref)?;
    let path = writable_page_path(kref, page_name)?;
    if !path.exists() {
        return Err(Error::Tool(format!("page not found: {}", path.display())));
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    crate::kms_trash::move_to_trash(kref, &format!("pages/{stem}.md"))?;
    remove_index_bullet(kref, &stem)?;
    append_log_header(kref, "deleted", &stem)?;
    fire_index_delete(kref, &stem);
    // Clear rather than guess a replacement: with nothing recorded,
    // `entry_page` infers one from what is left.
    if kref.read_manifest().and_then(|m| m.entry).as_deref() == Some(stem.as_str()) {
        let _ = set_entry_page(kref, None);
    }
    Ok(path)
}

/// Rename a KMS page: move `pages/<old>.md` → `pages/<new>.md` and
/// rewrite every inbound link (`pages/<old>.md`, `[[old]]`,
/// `[[old|display]]`) across the KMS's pages, sources, and `index.md`
/// so the vault stays self-consistent — same machinery the `merge`
/// path uses for collision renames. `new_name` is slugified the same
/// way new pages are. The page's frontmatter `title:` (its display
/// heading) is intentionally left alone — this renames the page's
/// identity/filename, not its title. Refuses to overwrite an existing
/// page. Returns the new path.
pub fn rename_page(kref: &KmsRef, old_name: &str, new_name: &str) -> Result<PathBuf> {
    ensure_writable(kref)?;
    let entry_before = kref.read_manifest().and_then(|m| m.entry);
    let old_path = writable_page_path(kref, old_name)?;
    if !old_path.exists() {
        return Err(Error::Tool(format!(
            "page not found: {}",
            old_path.display()
        )));
    }
    let old_stem = old_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(old_name)
        .to_string();

    let new_slug = sanitize_alias(new_name);
    if new_slug.is_empty() {
        return Err(Error::Tool(
            "new name has no usable characters for a filename".into(),
        ));
    }
    if new_slug == old_stem {
        return Ok(old_path); // no-op rename
    }
    let new_path = writable_page_path(kref, &new_slug)?;
    if new_path.exists() {
        return Err(Error::Tool(format!(
            "a page named '{new_slug}' already exists"
        )));
    }

    std::fs::rename(&old_path, &new_path)
        .map_err(|e| Error::Tool(format!("rename {}: {e}", old_path.display())))?;

    // Rewrite inbound links across pages/ + sources/ (the renamed file
    // now lives in pages/, so its own self-links get fixed too).
    let mut page_renames: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    page_renames.insert(old_stem.clone(), new_slug.clone());
    let source_renames: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for dir in [kref.pages_dir(), kref.root.join("sources")] {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| Error::Tool(format!("readdir {}: {e}", dir.display())))?
        {
            let entry = entry.map_err(|e| Error::Tool(format!("readdir entry: {e}")))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            let rewritten = rewrite_merge_links(&body, &page_renames, &source_renames);
            if rewritten != body {
                write_file(&path, rewritten.as_bytes())
                    .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
            }
        }
    }

    // Fix the index link target (preserves the bullet's summary +
    // category placement, unlike remove+re-add).
    let index = kref.read_index();
    if !index.is_empty() {
        let rewritten = rewrite_merge_links(&index, &page_renames, &source_renames);
        if rewritten != index {
            write_file(kref.index_path(), rewritten.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", kref.index_path().display())))?;
        }
    }

    append_log_header(kref, "renamed", &format!("{old_stem} → {new_slug}"))?;
    fire_index_delete(kref, &old_stem);
    fire_index_upsert(kref, &new_slug);
    // The entry page is named by slug, so a rename has to follow it.
    if entry_before.as_deref() == Some(old_stem.as_str()) {
        let _ = set_entry_page(kref, Some(&new_slug));
    }
    Ok(new_path)
}

/// M6.39.9: list every readable `*.md` file inside a KMS, split by
/// kind (`pages/` and `sources/`). Drives the right-edge KMS browser
/// panel — clicking the title of a KMS row in the sidebar opens this
/// listing, clicking a list entry opens the viewer overlay.
///
/// Filenames returned without the `.md` extension (so the frontend
/// can use them as page-name keys consistent with `KmsRead`).
/// Sorted alphabetically. Hidden files (`.foo`) skipped.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowseFile {
    pub name: String,
    pub bytes: u64,
    /// On-disk extension without the dot. Always `"md"` for pages;
    /// sources carry whatever they were ingested as. The GUI shows it
    /// as a badge and passes `name` back unchanged — the backend
    /// re-resolves the extension via [`source_path`].
    pub ext: String,
    /// What a person calls it (dev-plan/64 P5.7): a page's `title:`, a
    /// source's catalogued title. A research-built vault has English slugs
    /// over Thai titles, and the sidebar listed the slugs — a Thai reader
    /// looked down a column of names that were not the names of anything.
    /// Empty when there is none, and the GUI falls back to `name`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// `status:` when it says the page is not a finished note —
    /// `researching`, `derived`, `failed` — so the list can say so.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
}

/// One entry in `runs/` — a research run or a verify run. dev-plan/64
/// P5.6: the provenance ledger. Everything here is read from the run
/// log's own frontmatter, which both writers already stamp, so listing
/// the folder costs one small read per file and never parses a body.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RunEntry {
    /// File stem, which is also what `read_browse_file` takes back.
    pub name: String,
    /// `research` or `verify`, from `type:`. Empty for a log this
    /// build does not recognise — shown, not hidden.
    pub kind: String,
    /// What kind of run: `research`, `refresh`, `ingest`, `selection`
    /// or `verify`. From the log's `mode:` where it has one; a log
    /// written before P5.4 stamped it falls back to the `refresh-`
    /// prefix its name carries, which is all those logs ever recorded.
    pub mode: String,
    pub date: String,
    /// The research query. Verify runs have none.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claims: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_calls: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowseListing {
    pub kms: String,
    pub pages: Vec<BrowseFile>,
    pub sources: Vec<BrowseFile>,
    /// The page a reader should land on — see [`entry_page`].
    pub entry: Option<String>,
}

/// List browseable files for a KMS by name. Returns `None` if the
/// KMS isn't found. `pages/` and `sources/` are independent — a KMS
/// that predates M6.39.5 may have no `sources/` dir; that's fine,
/// returns empty list for that side.
pub fn browse(name: &str) -> Option<BrowseListing> {
    let kref = resolve(name)?;
    let pages = scan_dir_md(&kref.pages_dir());
    // Sources are NOT `.md`-only — `/kms ingest` accepts txt/rst/log/
    // json and URL ingest can archive html. Listing them through
    // `scan_dir_md` hid every non-markdown source from the browser
    // (and `read_browse_file` then couldn't open one either).
    let catalogue = crate::kms_sources::load(&kref);
    let sources = list_sources(&kref)
        .into_iter()
        .map(|s| {
            let title = catalogue
                .entries
                .get(&format!("{}.{}", s.stem, s.ext))
                .map(|r| r.title.trim().to_string())
                .filter(|t| !t.is_empty() && *t != s.stem)
                .unwrap_or_default();
            BrowseFile {
                name: s.stem,
                bytes: s.bytes,
                ext: s.ext,
                title,
                status: String::new(),
            }
        })
        .collect();
    let entry = entry_page(&kref);
    Some(BrowseListing {
        kms: name.to_string(),
        pages,
        sources,
        entry,
    })
}

/// Every run log in `runs/`, newest first. dev-plan/64 P5.6.
///
/// A run log is the only record of what a research or audit pass read,
/// wrote and cost, and until the GUI listed them the folder was
/// reachable only from a filesystem browser. Sorted by date then name
/// descending, so same-day runs come back in the order they were
/// numbered (`-3`, `-2`, then the first).
pub fn list_runs(kref: &KmsRef) -> Vec<RunEntry> {
    let dir = kref.root.join("runs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<RunEntry> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_string_lossy().to_string();
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            // Only the frontmatter is wanted; a verify log's body is
            // the whole report and can run to hundreds of KB.
            let head = read_head(&path, 4096);
            let (fm, _) = parse_frontmatter(&head);
            let num = |k: &str| fm.get(k).and_then(|v| v.trim().parse::<u64>().ok());
            let kind: String = match fm.get("type").map(|s| s.trim()) {
                Some("research-run") => "research".into(),
                Some("verify-run") => "verify".into(),
                _ => String::new(),
            };
            Some(RunEntry {
                name: stem.clone(),
                kind: kind.clone(),
                mode: fm
                    .get("mode")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| match kind.as_str() {
                        "research" if stem.splitn(2, "-refresh-").count() > 1 => "refresh".into(),
                        other => other.into(),
                    }),
                date: fm
                    .get("date")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                title: fm
                    .get("query")
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .unwrap_or_default(),
                bytes,
                cost_usd: fm
                    .get("cost_usd")
                    .and_then(|v| v.trim().parse::<f64>().ok()),
                claims: num("claims"),
                findings: num("findings"),
                elapsed_secs: num("elapsed_secs"),
                llm_calls: num("llm_calls"),
            })
        })
        .collect();
    out.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.name.cmp(&a.name)));
    out
}

/// What a run of one kind has cost in this vault before (dev-plan/64
/// P5.4). The median, not the mean: one 40-minute run should not make
/// every later click look expensive.
///
/// Deliberately not a token model. A theoretical estimate is a guess
/// about a model's behaviour; this is what this vault actually paid,
/// and `runs` says how much history it rests on — `0` means the honest
/// answer is "not known yet", which the GUI has to be able to say.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModeCost {
    /// How many past runs of this kind the numbers come from.
    pub runs: usize,
    /// Median USD across those runs, absent when none recorded a cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    /// Median pages written. Absent for kinds that write none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claims: Option<u64>,
}

fn median<T: Copy + PartialOrd>(mut v: Vec<T>) -> Option<T> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

/// [`ModeCost`] per run mode, for pricing a click before it is made.
pub fn cost_by_mode(kref: &KmsRef) -> std::collections::BTreeMap<String, ModeCost> {
    let mut out: std::collections::BTreeMap<String, Vec<RunEntry>> = Default::default();
    for r in list_runs(kref) {
        if r.mode.is_empty() {
            continue;
        }
        out.entry(r.mode.clone()).or_default().push(r);
    }
    out.into_iter()
        .map(|(mode, rs)| {
            let stats = ModeCost {
                runs: rs.len(),
                cost_usd: median(rs.iter().filter_map(|r| r.cost_usd).collect()),
                elapsed_secs: median(rs.iter().filter_map(|r| r.elapsed_secs).collect()),
                claims: median(rs.iter().filter_map(|r| r.claims).collect()),
            };
            (mode, stats)
        })
        .collect()
}

/// Read at most `cap` bytes of a file, trimmed to a char boundary so
/// the result is always valid UTF-8 (a run log's `query:` is often
/// Thai, and a naive cut lands mid-codepoint).
pub(crate) fn read_head(path: &Path, cap: usize) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf = vec![0u8; cap];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => {
            let good = e.utf8_error().valid_up_to();
            String::from_utf8_lossy(&e.into_bytes()[..good]).into_owned()
        }
    }
}

/// Record `slug` as the KMS's entry page, or clear it with `None`.
/// Rewrites `manifest.json` through a JSON value so a field this build
/// does not know about survives.
pub fn set_entry_page(kref: &KmsRef, slug: Option<&str>) -> Result<()> {
    ensure_writable(kref)?;
    with_kms_lock(kref, || set_entry_page_locked(kref, slug))
}

fn set_entry_page_locked(kref: &KmsRef, slug: Option<&str>) -> Result<()> {
    let path = kref.manifest_path();
    let mut doc = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({"schema_version": KMS_SCHEMA_VERSION}));
    let obj = doc.as_object_mut().expect("object");
    match slug {
        Some(s) => {
            obj.insert("entry".into(), serde_json::json!(s));
        }
        None => {
            obj.remove("entry");
        }
    }
    write_file(
        &path,
        serde_json::to_string_pretty(&doc).unwrap_or_default(),
    )
    .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))
}

/// `/kms entry` for both surfaces: show, set, or clear, and say what
/// happened. Setting a page that does not exist is refused — a
/// recorded entry that points nowhere is worse than none, because the
/// reader is left on an empty pane instead of the inferred page.
pub fn apply_entry(kms_name: &str, set: Option<&str>, clear: bool) -> Result<String> {
    let kref =
        resolve(kms_name).ok_or_else(|| Error::Tool(format!("no KMS named '{kms_name}'")))?;
    if clear {
        set_entry_page(&kref, None)?;
        let inferred = entry_page(&kref);
        return Ok(match inferred {
            Some(s) => format!("KMS '{kms_name}': entry page cleared — now inferred as `{s}`."),
            None => format!("KMS '{kms_name}': entry page cleared; the KMS has no pages."),
        });
    }
    if let Some(slug) = set {
        let slug = slug.trim().trim_end_matches(".md");
        if !kref.pages_dir().join(format!("{slug}.md")).is_file() {
            return Err(Error::Tool(format!("no page '{slug}' in KMS '{kms_name}'")));
        }
        set_entry_page(&kref, Some(slug))?;
        return Ok(format!("KMS '{kms_name}': entry page → `{slug}`."));
    }
    let recorded = kref.read_manifest().and_then(|m| m.entry);
    Ok(match (recorded, entry_page(&kref)) {
        (Some(r), Some(effective)) if r == effective => {
            format!("KMS '{kms_name}': entry page is `{r}` (recorded).")
        }
        (Some(r), Some(effective)) => format!(
            "KMS '{kms_name}': recorded entry `{r}` no longer exists; using `{effective}` (inferred)."
        ),
        (_, Some(effective)) => {
            format!("KMS '{kms_name}': entry page is `{effective}` (inferred — `/kms entry {kms_name} --set <slug>` to pin it).")
        }
        (_, None) => format!("KMS '{kms_name}': no pages yet."),
    })
}

/// Number of pages currently on disk. Used to spot the first one.
pub(crate) fn page_count(kref: &KmsRef) -> usize {
    std::fs::read_dir(kref.pages_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path().extension().and_then(|x| x.to_str()) == Some("md")
                        && !e.file_name().to_string_lossy().starts_with('.')
                })
                .count()
        })
        .unwrap_or(0)
}

/// Where a reader should start.
///
/// The recorded `entry` in `manifest.json` wins — set when the KMS's
/// first page was created, or by `/kms entry`. A recording that points
/// at a page that no longer exists is ignored rather than obeyed.
///
/// With nothing recorded it is inferred, in order:
///
/// 1. A map of content (`kind: moc`) — `/research` writes exactly one
///    per query, and it is the page that describes the whole topic.
/// 2. Otherwise the most linked-to page: in a vault nobody planned,
///    the hub is whatever everything else points at.
/// 3. Otherwise the most recently updated page, then the first by name.
///
/// Ties inside each rule break the same way, so the answer is stable
/// between calls on an unchanged KMS.
pub fn entry_page(kref: &KmsRef) -> Option<String> {
    if let Some(slug) = kref.read_manifest().and_then(|m| m.entry) {
        let slug = slug.trim().trim_end_matches(".md").to_string();
        if !slug.is_empty() && kref.pages_dir().join(format!("{slug}.md")).is_file() {
            return Some(slug);
        }
    }
    infer_entry_page(kref)
}

fn infer_entry_page(kref: &KmsRef) -> Option<String> {
    let backlinks = backlink_map(kref);
    let mut best: Option<(u8, usize, String, String)> = None; // (rank, backlinks, updated, slug)
    let rd = std::fs::read_dir(kref.pages_dir()).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.starts_with('.') || stem == "_summary" {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (fm, _) = parse_frontmatter(&raw);
        let rank = if fm.get("kind").map(|k| k.trim()) == Some("moc") {
            1
        } else {
            0
        };
        let inbound = backlinks.get(stem).map(|v| v.len()).unwrap_or(0);
        let updated = fm.get("updated").cloned().unwrap_or_default();
        let cand = (rank, inbound, updated, stem.to_string());
        // Higher rank, then more inbound links, then newer, then the
        // earlier name — `Ord` on the tuple does the first three; the
        // slug has to invert so "first by name" wins a full tie.
        let better = match &best {
            None => true,
            Some(b) => {
                (cand.0, cand.1, &cand.2) > (b.0, b.1, &b.2)
                    || ((cand.0, cand.1, &cand.2) == (b.0, b.1, &b.2) && cand.3 < b.3)
            }
        };
        if better {
            best = Some(cand);
        }
    }
    best.map(|(_, _, _, slug)| slug)
}

fn scan_dir_md(dir: &Path) -> Vec<BrowseFile> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<BrowseFile> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || !name.ends_with(".md") {
            continue;
        }
        let stem = name.trim_end_matches(".md").to_string();
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        // From the parsed-page cache: one stat per page, not one read.
        let (title, status) = parsed_page(&entry.path(), &stem)
            .map(|p| {
                let t = p.entry.title.trim().to_string();
                (if t == stem { String::new() } else { t }, p.status.clone())
            })
            .unwrap_or_default();
        out.push(BrowseFile {
            name: stem,
            bytes,
            ext: "md".into(),
            title,
            status,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// M6.39.13: build an Obsidian-style graph of one KMS — every page
/// is a node, every `[[slug]]` wikilink is a directed edge. Used by
/// the right-pane "Graph" view that mirrors Obsidian's visualization
/// of the same data.
///
/// Pages without outgoing OR incoming links are still emitted as
/// isolated nodes — the user wants to see them and decide whether
/// to link them.
///
/// Edge resolution: a `[[other-slug]]` in `karpathy.md` becomes an
/// edge `karpathy → other-slug` IF `other-slug.md` exists in the
/// same KMS. Dangling links (slug not present) are dropped silently
/// — the graph view shouldn't show ghost nodes for broken refs.
///
/// When `include_sources` is true, source files in `<root>/sources/`
/// are emitted as `kind: "source"` nodes and edges are added from
/// any page whose body cites them via `(../sources/<slug>.md)` (the
/// format produced by `linkify_citations` and the `## Sources`
/// section). Source nodes without any backlink are still listed —
/// orphan archives are useful to surface.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphNode {
    pub id: String, // page slug (filename stem); for sources we use `source:<stem>` to namespace
    pub label: String, // title from frontmatter, falls back to id
    pub size: u32,  // total link count (in + out) — sized in UI
    pub kind: GraphNodeKind,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GraphNodeKind {
    Page,
    Source,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphData {
    pub kms: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Build the graph for `kms_name`. Returns `None` if the KMS isn't
/// found. Always succeeds for a valid KMS even if pages are empty.
///
/// `include_sources` toggles whether source archives in `<root>/sources/`
/// are emitted as nodes. When true, page → source citation edges are
/// also added (parsed from `(../sources/<slug>.md)` markdown links
/// inside page bodies — the format produced by `linkify_citations`
/// and the `## Sources` section).
///
/// Source node IDs are namespaced as `source:<stem>` so they can't
/// collide with page slugs and the frontend can route clicks back
/// to `read_browse_file(kind="source", name="<stem>.md")`.
pub fn graph(kms_name: &str, include_sources: bool) -> Option<GraphData> {
    let kref = resolve(kms_name)?;
    let pages_dir = kref.pages_dir();
    let pages_iter = std::fs::read_dir(&pages_dir).ok();

    // First pass: collect every page slug + its title. Skip
    // hidden / non-md / `_summary` (it's an index, not a real
    // research page) so the graph isn't dominated by it.
    let mut nodes: std::collections::BTreeMap<String, GraphNode> =
        std::collections::BTreeMap::new();
    let mut bodies: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(entries) = pages_iter {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let filename = entry.file_name().to_string_lossy().to_string();
            if filename.starts_with('.') || !filename.ends_with(".md") {
                continue;
            }
            let stem = filename.trim_end_matches(".md").to_string();
            if stem == "_summary" {
                continue;
            }
            let body = match std::fs::read_to_string(entry.path()) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let (fm, _) = parse_frontmatter(&body);
            let label = fm
                .get("title")
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| stem.clone());
            nodes.insert(
                stem.clone(),
                GraphNode {
                    id: stem.clone(),
                    label,
                    size: 0,
                    kind: GraphNodeKind::Page,
                },
            );
            bodies.insert(stem, body);
        }
    }

    // Optional: list sources/ as nodes (`source:<stem>` IDs) and
    // register their stems for citation-edge resolution. Title comes
    // from frontmatter if the source archive has it (HAL-fetched
    // markdown often does), else falls back to the bare stem.
    let mut source_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
    if include_sources {
        let sources_dir = kref.root.join("sources");
        if let Ok(entries) = std::fs::read_dir(&sources_dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_file() {
                    continue;
                }
                let filename = entry.file_name().to_string_lossy().to_string();
                if filename.starts_with('.') || !filename.ends_with(".md") {
                    continue;
                }
                let stem = filename.trim_end_matches(".md").to_string();
                let label = std::fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|raw| {
                        let (fm, _) = parse_frontmatter(&raw);
                        fm.get("title")
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or_else(|| stem.clone());
                let node_id = format!("source:{stem}");
                nodes.insert(
                    node_id.clone(),
                    GraphNode {
                        id: node_id,
                        label,
                        size: 0,
                        kind: GraphNodeKind::Source,
                    },
                );
                source_stems.insert(stem);
            }
        }
    }

    // Second pass: scan each body for `[[slug]]` wikilinks (page→page)
    // and `(../sources/<stem>.md)` markdown links (page→source) and
    // emit edges where the target exists in the node set.
    // One edge per (page, target): a page cites the same source twenty
    // times and links a note both inline and in its Map — drawing every
    // repeat inflated a 50-page KMS to 2 000+ springs and stalled the
    // force layout.
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut seen_edges: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for (source, body) in &bodies {
        // `related:` frontmatter counts as an edge too, so a note whose
        // prose never spelled the link still connects.
        for target in outbound_page_links(body) {
            if !nodes.contains_key(&target) {
                continue;
            }
            if &target == source {
                continue;
            }
            if seen_edges.insert((source.clone(), target.clone())) {
                edges.push(GraphEdge {
                    source: source.clone(),
                    target,
                });
            }
        }
        if include_sources {
            for stem in extract_source_link_targets(body) {
                if !source_stems.contains(&stem) {
                    continue;
                }
                let target = format!("source:{stem}");
                if seen_edges.insert((source.clone(), target.clone())) {
                    edges.push(GraphEdge {
                        source: source.clone(),
                        target,
                    });
                }
            }
        }
    }

    // Compute node `size` = total in + out degree, used by the
    // frontend to scale node radii.
    for e in &edges {
        if let Some(n) = nodes.get_mut(&e.source) {
            n.size += 1;
        }
        if let Some(n) = nodes.get_mut(&e.target) {
            n.size += 1;
        }
    }

    Some(GraphData {
        kms: kms_name.to_string(),
        nodes: nodes.into_values().collect(),
        edges,
    })
}

/// Extract source filenames from `](../sources/<stem>.md)` markdown
/// links — the canonical citation format produced by
/// `linkify_citations` + the auto-generated `## Sources` section.
/// Returns the bare stem (no path, no `.md`).
fn extract_source_link_targets(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "](../sources/";
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find(needle) {
        let abs = search_from + rel + needle.len();
        let rest = &body[abs..];
        let end = rest.find(')').unwrap_or(rest.len());
        let target = &rest[..end];
        // Strip optional `.md` suffix and any URL fragment / query.
        let cleaned = target
            .split(|c| c == '#' || c == '?')
            .next()
            .unwrap_or(target)
            .trim_end_matches(".md");
        if !cleaned.is_empty() && !cleaned.contains('/') && cleaned.len() <= 200 {
            out.push(cleaned.to_string());
        }
        search_from = abs + end;
    }
    out
}

/// Walk the markdown body, return every `[[slug]]` (or `[[slug|display]]`)
/// target as a list. Slug is the part before `|`; display is dropped
/// (we only need the link target). Multiline / oversized brackets
/// skipped to avoid pathological inputs.
/// Every page this body points at: `[[wikilinks]]` in the prose plus
/// the `related:` frontmatter list. One definition so the graph view,
/// `/kms lint` and [`backlinks`] agree on what an edge is — they used
/// to disagree, and a note connected only through `related:` showed as
/// an orphan in lint while the graph drew it linked.
pub(crate) fn outbound_page_links(body: &str) -> Vec<String> {
    let mut out = extract_wikilink_targets(body);
    // `[text](pages/x.md)` is the other link form a KMS carries: it is
    // what hand-written pages use and what an OKF bundle round-trips
    // wikilinks into. The graph and backlinks used to miss it, so an
    // imported vault drew as a field of unconnected dots.
    for cap in markdown_page_link_re().captures_iter(body) {
        let t = cap[1].to_string();
        if !out.contains(&t) {
            out.push(t);
        }
    }
    let (fm, _) = parse_frontmatter(body);
    if let Some(rel) = fm.get("related") {
        for t in rel
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\''))
            .filter(|s| !s.is_empty())
        {
            if !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// Every page that links to each page, newest-updated first: the
/// reverse of [`outbound_page_links`], built in one pass over `pages/`.
///
/// Derived data, never stored. A backlink is a property of the graph —
/// the fact that A links to B lives in A's file — so writing it into B
/// would duplicate it, and keeping the duplicate correct would mean
/// rewriting every target of every edit. That would also bump each
/// target's `updated:`, which is the signal `/research refresh
/// --older-than` uses to decide what has gone stale.
pub fn backlink_map(kref: &KmsRef) -> std::collections::BTreeMap<String, Vec<(String, String)>> {
    let mut map: std::collections::BTreeMap<String, Vec<(String, String, String)>> =
        std::collections::BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(kref.pages_dir()) else {
        return std::collections::BTreeMap::new();
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.starts_with('.') || stem == "_summary" {
            continue;
        }
        let Some(page) = parsed_page(&path, stem) else {
            continue;
        };
        let title = if page.entry.title.is_empty() {
            stem.to_string()
        } else {
            page.entry.title.clone()
        };
        let updated = page.updated.clone();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for target in page.links.iter().cloned() {
            if target == stem || !seen.insert(target.clone()) {
                continue;
            }
            map.entry(target)
                .or_default()
                .push((stem.to_string(), title.clone(), updated.clone()));
        }
    }
    map.into_iter()
        .map(|(k, mut v)| {
            v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
            (k, v.into_iter().map(|(s, t, _)| (s, t)).collect())
        })
        .collect()
}

fn markdown_page_link_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\((?:\./)?pages/([^)]+?)\.md\)").expect("static regex"))
}

/// Pages that link to `page`, newest-updated first.
pub fn backlinks(kms_name: &str, page: &str) -> Vec<(String, String)> {
    let Some(kref) = resolve(kms_name) else {
        return Vec::new();
    };
    backlink_map(&kref).remove(page).unwrap_or_default()
}

/// Heading of the backlink block materialised into exported bundles.
/// Inside a live KMS the block never exists — it is recomputed on read.
pub const BACKLINK_HEADING: &str = "## Linked from";

/// Remove every `## Linked from` block (through the next `## ` or the
/// end). Keeps export idempotent and keeps an imported bundle's pages
/// free of derived data the importing KMS recomputes anyway.
pub fn strip_backlink_section(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find(BACKLINK_HEADING) {
        let line_start = rest[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        out.push_str(&rest[..line_start]);
        let after = &rest[at + BACKLINK_HEADING.len()..];
        rest = match after.find("\n## ") {
            Some(next) => &after[next + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out.trim_end().to_string()
}

fn extract_wikilink_targets(body: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < body.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(end_rel) = body[i + 2..].find("]]") {
                let inner = &body[i + 2..i + 2 + end_rel];
                if inner.len() <= 120 && !inner.contains('\n') {
                    let slug = inner
                        .split_once('|')
                        .map(|(s, _)| s.trim().to_string())
                        .unwrap_or_else(|| inner.trim().to_string());
                    if !slug.is_empty() {
                        out.push(slug);
                    }
                }
                i = i + 2 + end_rel + 2;
                continue;
            }
        }
        // Advance to next char boundary.
        let mut j = i + 1;
        while j < body.len() && !body.is_char_boundary(j) {
            j += 1;
        }
        i = j;
    }
    out
}

/// Per-file size ceiling for the viewer-overlay reader. Scraped KMS
/// sources can be multi-megabyte HTML; shipping that through IPC and
/// running `marked.parse()` + `dangerouslySetInnerHTML` on the result
/// locks the renderer thread. Cap is generous for normal markdown
/// (which is hand-written and rarely exceeds tens of KB) but bounds
/// the worst case. Files larger than this come back truncated with a
/// header line so the user knows.
pub const BROWSE_FILE_BYTE_CAP: u64 = 256 * 1024;
/// Result of [`read_browse_file`]: includes truncation metadata so
/// the GUI can surface a "showing first N KB of Y KB" banner.
pub struct BrowseFileRead {
    pub content: String,
    pub total_bytes: u64,
    pub truncated: bool,
}

/// M6.39.9: read a file from a KMS's `pages/` or `sources/` dir
/// for the viewer overlay. `kind` is `"page"` or `"source"`; `name`
/// is the bare filename stem (no `.md`). Path-safety mirrors
/// [`writable_page_path`] — the viewer is read-only, but we still
/// don't want a crafted IPC reading `/etc/passwd` via traversal.
///
/// Reads up to [`BROWSE_FILE_BYTE_CAP`] bytes. Larger files come back
/// with `truncated = true` and a small leading notice prepended to
/// the content so the viewer always shows *something* without hanging.
pub fn read_browse_file(kms_name: &str, kind: &str, name: &str) -> Result<BrowseFileRead> {
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
        || Path::new(name).is_absolute()
    {
        return Err(Error::Tool(format!(
            "invalid file name '{name}' — no path separators or traversal"
        )));
    }
    let kref =
        resolve(kms_name).ok_or_else(|| Error::Tool(format!("KMS '{kms_name}' not found")))?;
    // Sources resolve across every supported extension — hard-coding
    // `.md` here made every `.txt` / `.json` / `.log` / `.html` source
    // unopenable from the viewer even though `/kms ingest` accepts them.
    let canon_path = match kind {
        "source" => {
            let path = source_path(&kref, name)?;
            std::fs::canonicalize(&path)
                .map_err(|e| Error::Tool(format!("canonicalize {}: {e}", path.display())))?
        }
        // dev-plan/64 P5.6: a run log opens in the same viewer as a
        // page. Same directory containment check, different folder.
        "page" | "run" => {
            let dir = if kind == "run" {
                kref.root.join("runs")
            } else {
                kref.pages_dir()
            };
            let stem = name.trim_end_matches(".md");
            let path = dir.join(format!("{stem}.md"));
            if !path.exists() {
                return Err(Error::Tool(format!("not found: {}", path.display())));
            }
            // Canonicalize both and confirm path lives inside dir —
            // defense in depth even though the bare-name validation
            // above already blocks `..`.
            let canon_dir = std::fs::canonicalize(&dir)
                .map_err(|e| Error::Tool(format!("canonicalize {}: {e}", dir.display())))?;
            let canon_path = std::fs::canonicalize(&path)
                .map_err(|e| Error::Tool(format!("canonicalize {}: {e}", path.display())))?;
            if !canon_path.starts_with(&canon_dir) {
                return Err(Error::Tool(format!(
                    "path '{}' escaped KMS root",
                    path.display()
                )));
            }
            canon_path
        }
        other => return Err(Error::Tool(format!("invalid kind '{other}'"))),
    };
    let total_bytes = std::fs::metadata(&canon_path).map(|m| m.len()).unwrap_or(0);
    if total_bytes <= BROWSE_FILE_BYTE_CAP {
        let content = std::fs::read_to_string(&canon_path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", canon_path.display())))?;
        return Ok(BrowseFileRead {
            content,
            total_bytes,
            truncated: false,
        });
    }
    // Bounded read: open + read exactly the cap, then trim to a UTF-8
    // char boundary so the returned string is always valid (scraped
    // HTML often contains multi-byte chars right at our cap offset).
    use std::io::Read;
    let mut f = std::fs::File::open(&canon_path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", canon_path.display())))?;
    let mut buf = vec![0u8; BROWSE_FILE_BYTE_CAP as usize];
    let n = f
        .read(&mut buf)
        .map_err(|e| Error::Tool(format!("read {}: {e}", canon_path.display())))?;
    buf.truncate(n);
    let mut end = buf.len();
    while end > 0 && std::str::from_utf8(&buf[..end]).is_err() {
        end -= 1;
    }
    let head =
        std::str::from_utf8(&buf[..end]).unwrap_or("[unreadable: invalid UTF-8 in file head]");
    let notice = format!(
        "> **Large file — showing first {} KB of {} KB.** Open the file directly to view the rest.\n\n---\n\n",
        BROWSE_FILE_BYTE_CAP / 1024,
        total_bytes / 1024,
    );
    Ok(BrowseFileRead {
        content: format!("{notice}{head}"),
        total_bytes,
        truncated: true,
    })
}

/// Summary of what [`merge_into`] copied. Counts are per directory so
/// the user can tell at a glance whether anything had to be renamed
/// due to slug collisions with the destination KMS.
#[derive(Debug, Default)]
pub struct MergeReport {
    pub pages_copied: u32,
    pub pages_renamed: u32,
    /// Aggregator pages (`_`-prefixed stem) that existed in both KMSes
    /// and whose bodies were concatenated rather than renamed.
    pub pages_combined: u32,
    pub sources_copied: u32,
    pub sources_renamed: u32,
    pub index_entries_added: u32,
    /// (kind, original_stem, new_stem) for every file that had to be
    /// renamed due to a collision. `kind` is "page" or "source".
    pub renames: Vec<(String, String, String)>,
    /// Stems of `_`-prefixed pages that were combined on collision
    /// rather than renamed.
    pub combined: Vec<String>,
}

/// Outcome of [`consolidate`] — every writable KMS folded into one.
#[derive(Debug, Default)]
pub struct ConsolidateReport {
    pub dst: String,
    /// True if `dst` didn't exist and was created for this consolidation.
    pub created_dst: bool,
    /// (source name, its merge report) for each KMS merged into `dst`.
    pub merged: Vec<(String, MergeReport)>,
    /// Sources removed afterwards (only when `drop` was set).
    pub dropped: Vec<String>,
    /// Read-only (Shared) KMSes skipped — never merged or dropped.
    pub skipped_shared: Vec<String>,
}

impl ConsolidateReport {
    /// Human-readable summary, shared by the CLI + GUI dispatch.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.merged.is_empty() {
            out.push(format!(
                "/kms consolidate: nothing to merge into '{}' (no other writable KMS found).",
                self.dst
            ));
            if !self.skipped_shared.is_empty() {
                out.push(format!(
                    "  skipped read-only: {}",
                    self.skipped_shared.join(", ")
                ));
            }
            return out;
        }
        let sum = |f: fn(&MergeReport) -> u32| self.merged.iter().map(|(_, r)| f(r)).sum::<u32>();
        out.push(format!(
            "consolidated {} KMS(es) into '{}'{}: {} page(s) copied ({} renamed, {} combined), {} source(s).",
            self.merged.len(),
            self.dst,
            if self.created_dst { " (created)" } else { "" },
            sum(|r| r.pages_copied),
            sum(|r| r.pages_renamed),
            sum(|r| r.pages_combined),
            sum(|r| r.sources_copied),
        ));
        for (name, r) in &self.merged {
            out.push(format!(
                "  ← {name}: {} page(s), {} source(s)",
                r.pages_copied, r.sources_copied
            ));
        }
        if !self.skipped_shared.is_empty() {
            out.push(format!(
                "  skipped read-only: {}",
                self.skipped_shared.join(", ")
            ));
        }
        if !self.dropped.is_empty() {
            out.push(format!("  dropped sources: {}", self.dropped.join(", ")));
        } else {
            let drops = self
                .merged
                .iter()
                .map(|(n, _)| format!("/kms drop {n} --force"))
                .collect::<Vec<_>>()
                .join("; ");
            out.push(format!(
                "  sources left intact — verify, then drop: {drops}"
            ));
        }
        out.push(format!(
            "  then `/kms reindex {}` (or just search it — a stale index auto-rebuilds).",
            self.dst
        ));
        out
    }
}

/// Merge every *writable* KMS (Project + User scope) into `dst`, creating `dst`
/// in `scope` if it doesn't already exist. Shared/read-only KMSes are skipped.
/// When `drop` is set, each merged source is removed afterwards, leaving just
/// `dst` (otherwise sources are kept intact for the caller to verify + drop).
///
/// Thin orchestration over [`merge_into`] (per-source) — same rename-on-collision
/// + aggregator-combine + link-rewrite semantics apply to each source.
pub fn consolidate(dst_name: &str, scope: KmsScope, drop: bool) -> Result<ConsolidateReport> {
    let sources = list_all();
    let created_dst = resolve(dst_name).is_none();
    if created_dst {
        create(dst_name, scope)?;
    }
    let mut report = ConsolidateReport {
        dst: dst_name.to_string(),
        created_dst,
        ..Default::default()
    };
    let mut seen = std::collections::HashSet::new();
    for k in sources {
        if k.name == dst_name {
            continue;
        }
        if matches!(k.scope, KmsScope::Shared) {
            report.skipped_shared.push(k.name);
            continue;
        }
        if !seen.insert(k.name.clone()) {
            continue; // name shadowed across scopes — merge once
        }
        let mr = merge_into(&k.name, dst_name)?;
        report.merged.push((k.name, mr));
    }
    if drop {
        for (name, _) in &report.merged {
            if remove(name).is_ok() {
                report.dropped.push(name.clone());
            }
        }
    }
    Ok(report)
}

/// Pages whose stem starts with `_` are aggregator/summary pages
/// (e.g. `_summary.md`, `_journal.md`) — they collect content over
/// time rather than describing one bounded topic. When two KMSes both
/// have one, merging should *append* the src body under the dst body
/// rather than rename src to a sibling file, which would defeat the
/// page's purpose.
fn is_aggregator_stem(stem: &str) -> bool {
    stem.starts_with('_') && stem.len() > 1
}

/// Build the combined body for an aggregator-page collision during
/// `merge_into`. dst's content (frontmatter + body) is preserved; src's
/// body is appended below a provenance marker. If src's body is empty
/// or only whitespace, dst is returned unchanged.
fn combine_aggregator_bodies(dst_full: &str, src_body_only: &str, src_kms: &str) -> String {
    if src_body_only.trim().is_empty() {
        return dst_full.to_string();
    }
    let mut out = dst_full.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "<!-- merged from {} on {} -->\n\n",
        src_kms,
        crate::usage::today_str()
    ));
    out.push_str(src_body_only.trim_start());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Merge `src` KMS *into* `dst` KMS, leaving `src` intact.
///
/// Semantics:
/// - Pages and sources from `src` are copied into `dst`'s respective
///   directories. If a same-name file already exists in `dst`, the
///   incoming file is renamed to `<stem>-from-<src>.md`.
/// - **Aggregator pages** — those whose stem starts with `_`
///   (`_summary.md`, `_journal.md`, …) — are *combined* on collision
///   rather than renamed: src's body is appended to dst's body under
///   a `<!-- merged from <src> on <date> -->` marker, preserving
///   dst's frontmatter. Renaming would defeat the page's purpose
///   (its job is to aggregate, not to fork).
/// - Any links inside copied content that referenced renamed siblings
///   are rewritten (`pages/<old>.md` → `pages/<old>-from-<src>.md` and
///   Obsidian-style `[[<old>]]` → `[[<old>-from-<src>]]`) so the
///   merged KMS stays internally consistent.
/// - `index.md` entries from `src` are appended to `dst`'s index,
///   line-deduped against existing entries, with the same link
///   rewriting applied.
/// - `log.md` gets a `merge` header so the operation is greppable.
///
/// `src` is read-only during the merge — its pages, sources, index,
/// and log are left exactly as found. The caller can `/kms drop`
/// afterwards once they've verified the merged result.
///
/// **dev-plan/36 Tier 1.D note:** `merge_into` mutates many pages
/// in one call (rename-on-collision + body rewrites + cascade);
/// firing per-page index hooks inline here would require threading
/// the rename map through every helper. Deferred to Tier 3, which
/// adds auto-rebuild-on-stale-manifest — after a merge, the next
/// `KmsSearch(query: …)` against the destination KMS will detect
/// the manifest staleness and rebuild before serving. Operators can
/// also run `/kms reindex <dst>` immediately after a merge to
/// force a fresh build (~1 s per 100 pages).
pub fn merge_into(src_name: &str, dst_name: &str) -> Result<MergeReport> {
    if src_name == dst_name {
        return Err(Error::Config("cannot merge a KMS into itself".into()));
    }
    let src =
        resolve(src_name).ok_or_else(|| Error::Tool(format!("KMS '{src_name}' not found")))?;
    let dst =
        resolve(dst_name).ok_or_else(|| Error::Tool(format!("KMS '{dst_name}' not found")))?;
    ensure_writable(&dst)?;

    // Copies pages in a loop; one index rebuild covers the lot.
    let _batch = IndexBatch::new(&dst);
    let mut report = MergeReport::default();
    // (original_stem → new_stem) for renamed pages, used to rewrite
    // intra-KMS links inside the copied content.
    let mut page_renames: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut source_renames: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // ── Copy pages ────────────────────────────────────────────────
    let src_pages = src.pages_dir();
    let dst_pages = dst.pages_dir();
    std::fs::create_dir_all(&dst_pages)
        .map_err(|e| Error::Tool(format!("mkdir {}: {e}", dst_pages.display())))?;
    if src_pages.is_dir() {
        for entry in std::fs::read_dir(&src_pages)
            .map_err(|e| Error::Tool(format!("readdir {}: {e}", src_pages.display())))?
        {
            let entry = entry.map_err(|e| Error::Tool(format!("readdir entry: {e}")))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if path.extension().and_then(|e| e.to_str()) == Some("md") => s.to_string(),
                _ => continue,
            };
            let dst_path = dst_pages.join(format!("{stem}.md"));
            // Aggregator pages (e.g. `_summary.md`) — combine on
            // collision instead of renaming. The dst body keeps its
            // frontmatter; src's body lands underneath with a
            // provenance marker. No rename happens, so no link
            // rewrite is needed for these.
            if dst_path.exists() && is_aggregator_stem(&stem) {
                let dst_body = std::fs::read_to_string(&dst_path)
                    .map_err(|e| Error::Tool(format!("read {}: {e}", dst_path.display())))?;
                let src_body = std::fs::read_to_string(&path)
                    .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
                let (_src_fm_map, src_body_only) = parse_frontmatter(&src_body);
                let combined = combine_aggregator_bodies(&dst_body, &src_body_only, src_name);
                write_file(&dst_path, combined.as_bytes())
                    .map_err(|e| Error::Tool(format!("write {}: {e}", dst_path.display())))?;
                report.pages_combined += 1;
                report.combined.push(stem.clone());
                continue;
            }
            let target_stem = if dst_path.exists() {
                let renamed = format!("{stem}-from-{src_name}");
                page_renames.insert(stem.clone(), renamed.clone());
                report.pages_renamed += 1;
                report
                    .renames
                    .push(("page".into(), stem.clone(), renamed.clone()));
                renamed
            } else {
                report.pages_copied += 1;
                stem.clone()
            };
            // Read + rewrite (we may need to rewrite later once we know
            // the full rename map, but for first pass write the raw
            // bytes; a second pass below rewrites in place).
            let bytes = std::fs::read(&path)
                .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
            let target = dst_pages.join(format!("{target_stem}.md"));
            write_file(&target, &bytes)
                .map_err(|e| Error::Tool(format!("write {}: {e}", target.display())))?;
        }
    }

    // ── Copy sources ──────────────────────────────────────────────
    let src_sources = src.root.join("sources");
    let dst_sources = dst.root.join("sources");
    if src_sources.is_dir() {
        std::fs::create_dir_all(&dst_sources)
            .map_err(|e| Error::Tool(format!("mkdir {}: {e}", dst_sources.display())))?;
        for entry in std::fs::read_dir(&src_sources)
            .map_err(|e| Error::Tool(format!("readdir {}: {e}", src_sources.display())))?
        {
            let entry = entry.map_err(|e| Error::Tool(format!("readdir entry: {e}")))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if path.extension().and_then(|e| e.to_str()) == Some("md") => s.to_string(),
                _ => continue,
            };
            let target_stem = if dst_sources.join(format!("{stem}.md")).exists() {
                let renamed = format!("{stem}-from-{src_name}");
                source_renames.insert(stem.clone(), renamed.clone());
                report.sources_renamed += 1;
                report
                    .renames
                    .push(("source".into(), stem.clone(), renamed.clone()));
                renamed
            } else {
                report.sources_copied += 1;
                stem.clone()
            };
            let bytes = std::fs::read(&path)
                .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
            let target = dst_sources.join(format!("{target_stem}.md"));
            write_file(&target, &bytes)
                .map_err(|e| Error::Tool(format!("write {}: {e}", target.display())))?;
        }
    }

    // ── Rewrite intra-KMS link references in the copied files ───
    // Only the *renamed* stems need rewriting; non-collided files
    // keep the same link targets. We patch every copied file (not
    // just renamed ones) because a copied page may reference another
    // copied page whose name *did* change.
    if !page_renames.is_empty() || !source_renames.is_empty() {
        for dir in [&dst_pages, &dst_sources] {
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(dir)
                .map_err(|e| Error::Tool(format!("readdir {}: {e}", dir.display())))?
            {
                let entry = entry.map_err(|e| Error::Tool(format!("readdir entry: {e}")))?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let rewritten = rewrite_merge_links(&body, &page_renames, &source_renames);
                if rewritten != body {
                    write_file(&path, rewritten.as_bytes())
                        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
                }
            }
        }
    }

    // ── Merge index.md (append + dedupe + rewrite renamed links) ──
    let src_index = src.read_index();
    let dst_index_existing = dst.read_index();
    let mut dst_lines: Vec<String> = if dst_index_existing.is_empty() {
        Vec::new()
    } else {
        dst_index_existing.lines().map(String::from).collect()
    };
    for raw_line in src_index.lines() {
        let line = rewrite_merge_links(raw_line, &page_renames, &source_renames);
        if line.trim().is_empty() {
            continue;
        }
        if !dst_lines.iter().any(|l| l == &line) {
            dst_lines.push(line);
            report.index_entries_added += 1;
        }
    }
    let mut new_index = dst_lines.join("\n");
    if !new_index.ends_with('\n') && !new_index.is_empty() {
        new_index.push('\n');
    }
    write_file(dst.index_path(), new_index.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", dst.index_path().display())))?;

    // ── Log the merge on the destination ─────────────────────────
    append_log_header(&dst, "merge", src_name)?;

    Ok(report)
}

/// Frontmatter keys whose values are page slugs.
const SLUG_KEYS: &[&str] = &[
    "related",
    "supersedes",
    "superseded_by",
    "parent",
    "children",
    "see_also",
    "links",
];

/// dev-plan/64 P3.5: follow a rename or a merge into the frontmatter.
///
/// Links in the body were rewritten; `related: ["old", …]` was not, and the
/// graph view, the research planner's neighbour lookup and lint all read
/// `related:`. After renaming one page, every note that listed it pointed
/// at nothing — silently, because a dangling `related:` is not a broken
/// link to anything that checked.
///
/// Only a whole value is replaced, in a slug key's flow list, scalar, or
/// block list — never a substring, so renaming `ai` leaves `ai-slop` alone.
fn rewrite_slug_keys(text: &str, old: &str, new: &str) -> String {
    let Some(rest) = text.strip_prefix("---") else {
        return text.to_string();
    };
    let Some(end) = rest.find("\n---") else {
        return text.to_string();
    };
    let (fm, tail) = rest.split_at(end);
    let swap = |v: &str| -> Option<String> {
        let t = v.trim();
        let bare = t.trim_matches(|c| c == '"' || c == '\'');
        (bare == old).then(|| v.replacen(bare, new, 1))
    };
    let mut in_slug_block = false;
    let mut changed = false;
    let mut lines: Vec<String> = Vec::new();
    for line in fm.split('\n') {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            in_slug_block = false;
            if let Some((key, val)) = line.split_once(':') {
                if SLUG_KEYS.contains(&key.trim()) {
                    let v = val.trim();
                    if v.is_empty() {
                        in_slug_block = true;
                    } else if let Some(inner) =
                        v.strip_prefix('[').and_then(|x| x.strip_suffix(']'))
                    {
                        let items: Vec<String> = inner
                            .split(',')
                            .map(|it| swap(it).unwrap_or_else(|| it.to_string()))
                            .collect();
                        let joined = items.join(",");
                        if joined != inner {
                            changed = true;
                            lines.push(format!("{key}: [{joined}]"));
                            continue;
                        }
                    } else if let Some(sw) = swap(v) {
                        changed = true;
                        lines.push(format!("{key}: {}", sw.trim()));
                        continue;
                    }
                }
            }
        } else if in_slug_block {
            if let Some(item) = line.trim_start().strip_prefix("- ") {
                if let Some(sw) = swap(item) {
                    let pad = &line[..line.len() - line.trim_start().len()];
                    changed = true;
                    lines.push(format!("{pad}- {}", sw.trim()));
                    continue;
                }
            }
        }
        lines.push(line.to_string());
    }
    if !changed {
        return text.to_string();
    }
    format!("---{}{tail}", lines.join("\n"))
}

/// Rewrite the renamed-on-collision link forms inside a body of
/// markdown so the merged KMS stays self-consistent. Handles:
/// - `pages/<old>.md` (relative md link target)
/// - `sources/<old>.md`
/// - `[[<old>]]` and `[[<old>|display]]` Obsidian wikilinks
fn rewrite_merge_links(
    body: &str,
    page_renames: &std::collections::HashMap<String, String>,
    source_renames: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = body.to_string();
    for (old, new) in page_renames {
        out = out.replace(&format!("pages/{old}.md"), &format!("pages/{new}.md"));
        out = out.replace(&format!("[[{old}]]"), &format!("[[{new}]]"));
        out = out.replace(&format!("[[{old}|"), &format!("[[{new}|"));
        out = out.replace(&format!("[[{old}#"), &format!("[[{new}#"));
        out = rewrite_slug_keys(&out, old, new);
    }
    for (old, new) in source_renames {
        out = out.replace(&format!("sources/{old}.md"), &format!("sources/{new}.md"));
    }
    out
}

// ────────────────────────────────────────────────────────────────────────
// OKF (Open Knowledge Format) import/export.
//
// OKF (Google, v0.1 — `GoogleCloudPlatform/knowledge-catalog`) is the
// Karpathy "LLM wiki" pattern formalized: a directory of markdown concept
// files with YAML frontmatter, an `index.md`, a `log.md`, and markdown
// cross-links. Our KMS is an opinionated superset, so this is a thin
// frontmatter/layout adapter — not a new store. Field mapping:
//
//   KMS                         OKF
//   ───                         ───
//   category:                ↔  type:        (OKF's only REQUIRED field)
//   topic:                   ↔  description:
//   updated:                 →  timestamp:   (kept; ISO 8601)
//   tags: a, b               ↔  tags: [a, b]
//   pages/<stem>.md          ↔  pages/<stem>.md  (a "concept")
//   sources/<f>              ↔  references/<f>
//   [[wikilink]]             →  [wikilink](/pages/wikilink.md)
//   "## [date] verb | x"     ↔  "## date" + "* **Verb**: x"
//
// KMS-specific keys with no OKF home (`sources`, `verified`, `created`)
// ride along verbatim — OKF tolerates arbitrary producer keys, so the
// round-trip KMS→OKF→KMS is lossless for them. Export is conformant OKF
// v0.1 (every `.md` carries a `type`); import is permissive per §9 —
// it tolerates unknown types, missing fields, broken links, and
// concepts at any directory level, not just `pages/`.

/// `a, b` or `[a, b]` → canonical OKF inline list `[a, b]`. Empty → `[]`.
fn tags_to_yaml_list(raw: &str) -> String {
    let s = raw.trim();
    if s.starts_with('[') {
        return s.to_string();
    }
    let items: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    format!("[{}]", items.join(", "))
}

/// `[a, b]` (or already-CSV `a, b`) → KMS comma string `a, b`.
fn tags_to_csv(raw: &str) -> String {
    let mut s = raw.trim();
    if s.starts_with('[') && s.ends_with(']') {
        s = &s[1..s.len() - 1];
    }
    s.split(',')
        .map(|t| t.trim().trim_matches('"').trim_matches('\'').trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Convert Obsidian `[[target]]` / `[[target|display]]` wikilinks into
/// standard bundle-relative OKF markdown links. Existing
/// `[label](pages/x.md)` links are left alone — relative links are valid
/// OKF (§5.2). Unterminated or empty `[[…]]` are emitted verbatim.
fn wikilinks_to_okf(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    loop {
        let Some(start) = rest.find("[[") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            // No closing — emit the rest literally and stop.
            out.push_str("[[");
            rest = after;
            continue;
        };
        let inner = &after[..end];
        let (target, display) = match inner.split_once('|') {
            Some((t, d)) => (t.trim(), d.trim()),
            None => (inner.trim(), inner.trim()),
        };
        if target.is_empty() {
            out.push_str("[[");
            out.push_str(inner);
            out.push_str("]]");
        } else {
            out.push_str(&format!("[{display}](/pages/{target}.md)"));
        }
        rest = &after[end + 2..];
    }
    out
}

/// Rewrite OKF absolute bundle-relative link targets (`/pages/…`,
/// `/sources/…`, `/references/…`) into KMS-relative form so `lint` /
/// `auto_link` / the search index recognise them.
fn okf_links_to_kms(body: &str) -> String {
    let body = strip_backlink_section(body);
    let body = body.as_str();
    body.replace("](/pages/", "](pages/")
        .replace("](/sources/", "](sources/")
        .replace("](/references/", "](sources/")
        .replace("](references/", "](sources/")
}

/// Rewrite markdown link targets that point at OKF concepts (by their
/// bundle-relative path) so they land on the flattened KMS page stem.
/// Handles the absolute (`/tables/x.md`) and bare (`tables/x.md`) forms.
fn rewrite_okf_concept_links(
    body: &str,
    rel_to_stem: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = body.to_string();
    for (rel, stem) in rel_to_stem {
        let target = format!("](pages/{stem}.md)");
        out = out.replace(&format!("](/{rel})"), &target);
        out = out.replace(&format!("]({rel})"), &target);
    }
    out
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// KMS `## [date] verb | alias` history → OKF date-grouped log (§7).
fn kms_log_to_okf(raw: &str) -> String {
    let mut out = String::from("# Change log\n");
    let mut cur_date: Option<String> = None;
    for line in raw.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("## [") {
            if let Some((date, tail)) = rest.split_once(']') {
                let date = date.trim();
                let tail = tail.trim();
                let (verb, alias) = match tail.split_once('|') {
                    Some((v, a)) => (v.trim(), a.trim()),
                    None => (tail, ""),
                };
                if cur_date.as_deref() != Some(date) {
                    out.push_str(&format!("\n## {date}\n"));
                    cur_date = Some(date.to_string());
                }
                let verb = capitalize_first(verb);
                if alias.is_empty() {
                    out.push_str(&format!("* **{verb}**\n"));
                } else {
                    out.push_str(&format!("* **{verb}**: {alias}\n"));
                }
                continue;
            }
        }
        // Already-OKF date heading: re-emit, tracking the current date.
        if let Some(date) = t.strip_prefix("## ") {
            let date = date.trim();
            if cur_date.as_deref() != Some(date) {
                out.push_str(&format!("\n## {date}\n"));
                cur_date = Some(date.to_string());
            }
            continue;
        }
        // Bullets under an existing date heading pass through; other
        // lines (e.g. the old "# Change log" preamble prose) are dropped.
        if t.starts_with('*') && cur_date.is_some() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// OKF date-grouped log → KMS `## [date] verb | alias` history. Best
/// effort: the bullet's bold word becomes the verb, the remainder the
/// alias. Lossy for prose entries, but KMS log is a greppable trail,
/// not structured data.
fn okf_log_to_kms(raw: &str) -> String {
    let mut out = String::from("# Change log\n\n");
    let mut cur_date: Option<String> = None;
    for line in raw.lines() {
        let t = line.trim();
        if let Some(date) = t.strip_prefix("## ") {
            cur_date = Some(
                date.trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string(),
            );
            continue;
        }
        let Some(rest) = t.strip_prefix("* ").or_else(|| t.strip_prefix("- ")) else {
            continue;
        };
        let Some(date) = &cur_date else { continue };
        let rest = rest.trim();
        let (verb, alias) = if let Some(after) = rest.strip_prefix("**") {
            match after.split_once("**") {
                Some((v, tail)) => (
                    v.trim().to_string(),
                    tail.trim_start().trim_start_matches(':').trim().to_string(),
                ),
                None => ("update".to_string(), rest.to_string()),
            }
        } else {
            ("update".to_string(), rest.to_string())
        };
        let verb = verb.to_lowercase();
        if alias.is_empty() {
            out.push_str(&format!("## [{date}] {verb}\n"));
        } else {
            out.push_str(&format!("## [{date}] {verb} | {alias}\n"));
        }
    }
    out
}

/// Map a KMS page's frontmatter to OKF frontmatter. `type` is always
/// present (OKF's only requirement); KMS-only keys ride along verbatim.
fn kms_fm_to_okf(
    fm: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut okf = std::collections::BTreeMap::new();
    let category = fm
        .get("category")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    okf.insert("type".into(), category.unwrap_or("Note").to_string());
    if let Some(t) = fm.get("title") {
        okf.insert("title".into(), t.clone());
    }
    if let Some(d) = fm.get("topic").or_else(|| fm.get("description")) {
        okf.insert("description".into(), d.clone());
    }
    if let Some(u) = fm.get("updated").or_else(|| fm.get("timestamp")) {
        okf.insert("timestamp".into(), u.clone());
    }
    if let Some(tg) = fm.get("tags") {
        okf.insert("tags".into(), tags_to_yaml_list(tg));
    }
    // Preserve remaining KMS keys (category, created, updated, sources,
    // verified, …) without clobbering the OKF-normalised ones above.
    for (k, v) in fm {
        if matches!(k.as_str(), "title" | "topic" | "description" | "tags") {
            continue;
        }
        okf.entry(k.clone()).or_insert_with(|| v.clone());
    }
    okf
}

/// Map an OKF concept's frontmatter back to KMS frontmatter.
fn okf_fm_to_kms(
    fm: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut kms = std::collections::BTreeMap::new();
    let category = fm
        .get("category")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .or_else(|| fm.get("type").map(|s| s.trim()).filter(|s| !s.is_empty()))
        .unwrap_or("uncategorized");
    kms.insert("category".into(), category.to_string());
    if let Some(t) = fm.get("title") {
        kms.insert("title".into(), t.clone());
    }
    if let Some(d) = fm.get("description").or_else(|| fm.get("topic")) {
        kms.insert("topic".into(), d.clone());
    }
    if let Some(tg) = fm.get("tags") {
        kms.insert("tags".into(), tags_to_csv(tg));
    }
    if let Some(u) = fm.get("updated").or_else(|| fm.get("timestamp")) {
        // KMS dates are day-granular — take the date part of an ISO 8601 stamp.
        let date = u.split('T').next().unwrap_or(u).trim().to_string();
        kms.insert("updated".into(), date);
    }
    if let Some(c) = fm.get("created") {
        kms.insert("created".into(), c.clone());
    }
    for (k, v) in fm {
        if matches!(
            k.as_str(),
            "title"
                | "description"
                | "topic"
                | "tags"
                | "type"
                | "timestamp"
                | "category"
                | "created"
                | "updated"
        ) {
            continue;
        }
        kms.entry(k.clone()).or_insert_with(|| v.clone());
    }
    kms
}

/// Result of [`export_okf`].
#[derive(Debug, Default)]
pub struct OkfExportReport {
    pub pages: u32,
    pub sources: u32,
    pub out_dir: PathBuf,
}

/// Export a KMS as a conformant OKF v0.1 bundle into `out_dir`.
///
/// Layout produced:
/// ```text
/// out_dir/
///   index.md        — okf_version frontmatter + the KMS index body
///   log.md          — date-grouped OKF history
///   SCHEMA.md       — KMS schema, given `type: OKF Schema` frontmatter
///   manifest.json   — copied verbatim (non-.md; OKF ignores it, aids round-trip)
///   pages/<stem>.md — concepts, frontmatter normalised, wikilinks → md links
///   references/<f>  — raw sources (md gets a `type: Source` wrapper)
/// ```
pub fn export_okf(name: &str, out_dir: &Path) -> Result<OkfExportReport> {
    let kref = resolve(name).ok_or_else(|| Error::Tool(format!("KMS '{name}' not found")))?;
    std::fs::create_dir_all(out_dir)
        .map_err(|e| Error::Tool(format!("create {}: {e}", out_dir.display())))?;
    let mut report = OkfExportReport {
        out_dir: out_dir.to_path_buf(),
        ..Default::default()
    };

    // ── pages → pages/ ────────────────────────────────────────────
    let backlinks = backlink_map(&kref);
    let okf_pages = out_dir.join("pages");
    std::fs::create_dir_all(&okf_pages)
        .map_err(|e| Error::Tool(format!("mkdir {}: {e}", okf_pages.display())))?;
    if let Ok(entries) = std::fs::read_dir(kref.pages_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = entry.file_type().ok();
            if ft.map(|f| f.is_symlink() || !f.is_file()).unwrap_or(true) {
                continue;
            }
            let fname = match path.file_name().and_then(|s| s.to_str()) {
                Some(f) if f.ends_with(".md") => f.to_string(),
                _ => continue,
            };
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            let (fm, body) = parse_frontmatter(&raw);
            // A bundle leaves the KMS behind, so the reverse edges have
            // to travel with it: nothing outside recomputes them, and a
            // snapshot cannot go stale.
            let stem = fname.trim_end_matches(".md");
            let mut body = strip_backlink_section(&body);
            if let Some(links) = backlinks.get(stem).filter(|l| !l.is_empty()) {
                body.push_str(&format!("\n\n{BACKLINK_HEADING}\n\n"));
                for (slug, title) in links {
                    body.push_str(&format!("- [{title}](/pages/{slug}.md)\n"));
                }
            }
            let okf = write_frontmatter(&kms_fm_to_okf(&fm), &wikilinks_to_okf(&body));
            std::fs::write(okf_pages.join(&fname), okf.as_bytes())
                .map_err(|e| Error::Tool(format!("write page {fname}: {e}")))?;
            report.pages += 1;
        }
    }

    // ── sources → references/ ─────────────────────────────────────
    let src_dir = kref.root.join("sources");
    if src_dir.is_dir() {
        let okf_refs = out_dir.join("references");
        std::fs::create_dir_all(&okf_refs)
            .map_err(|e| Error::Tool(format!("mkdir {}: {e}", okf_refs.display())))?;
        if let Ok(entries) = std::fs::read_dir(&src_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = entry.file_type().ok();
                if ft.map(|f| f.is_symlink() || !f.is_file()).unwrap_or(true) {
                    continue;
                }
                let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let is_md = matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("md") | Some("markdown")
                );
                let dst = okf_refs.join(fname);
                if is_md {
                    // Make raw markdown sources conformant: ensure a `type`.
                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    let (mut sfm, sbody) = parse_frontmatter(&content);
                    if !sfm.contains_key("type") {
                        sfm.insert("type".into(), "Source".into());
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(fname)
                            .to_string();
                        sfm.entry("title".into()).or_insert(stem);
                        std::fs::write(&dst, write_frontmatter(&sfm, &sbody).as_bytes())
                            .map_err(|e| Error::Tool(format!("write reference {fname}: {e}")))?;
                    } else {
                        std::fs::copy(&path, &dst)
                            .map_err(|e| Error::Tool(format!("copy reference {fname}: {e}")))?;
                    }
                } else {
                    std::fs::copy(&path, &dst)
                        .map_err(|e| Error::Tool(format!("copy reference {fname}: {e}")))?;
                }
                report.sources += 1;
            }
        }
    }

    // ── index.md (root): okf_version frontmatter + KMS index body ──
    let mut idx_fm = std::collections::BTreeMap::new();
    idx_fm.insert("okf_version".into(), "0.1".into());
    let idx_body = kref.read_index();
    std::fs::write(
        out_dir.join("index.md"),
        write_frontmatter(&idx_fm, &idx_body).as_bytes(),
    )
    .map_err(|e| Error::Tool(format!("write index.md: {e}")))?;

    // ── log.md ────────────────────────────────────────────────────
    let log_raw = std::fs::read_to_string(kref.log_path()).unwrap_or_default();
    std::fs::write(out_dir.join("log.md"), kms_log_to_okf(&log_raw).as_bytes())
        .map_err(|e| Error::Tool(format!("write log.md: {e}")))?;

    // ── SCHEMA.md (give it a type so it's a conformant concept) ────
    if let Ok(schema) = std::fs::read_to_string(kref.schema_path()) {
        let (mut sfm, sbody) = parse_frontmatter(&schema);
        sfm.insert("type".into(), "OKF Schema".into());
        sfm.entry("title".into()).or_insert_with(|| "Schema".into());
        std::fs::write(
            out_dir.join("SCHEMA.md"),
            write_frontmatter(&sfm, &sbody).as_bytes(),
        )
        .map_err(|e| Error::Tool(format!("write SCHEMA.md: {e}")))?;
    }

    // ── manifest.json (verbatim; ignored by OKF, restores on import) ─
    if kref.manifest_path().is_file() {
        let _ = std::fs::copy(kref.manifest_path(), out_dir.join("manifest.json"));
    }

    Ok(report)
}

/// Result of [`import_okf`].
#[derive(Debug, Default)]
pub struct OkfImportReport {
    pub pages: u32,
    pub sources: u32,
    pub root: PathBuf,
}

/// Derive a flat KMS page stem from a concept's bundle-relative path,
/// dropping a leading `pages/` and joining nested components with `-`.
fn okf_concept_stem(rel: &Path) -> String {
    let mut parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(|s| s.to_string()),
            _ => None,
        })
        .collect();
    if let Some(last) = parts.last_mut() {
        *last = last
            .trim_end_matches(".md")
            .trim_end_matches(".markdown")
            .to_string();
    }
    if parts.first().map(|p| p == "pages").unwrap_or(false) {
        parts.remove(0);
    }
    let joined = parts.join("-");
    let stem = sanitize_alias(&joined);
    if stem.is_empty() {
        "page".to_string()
    } else {
        stem
    }
}

/// Recursively collect `.md` concept files under `dir`, skipping
/// symlinks, reserved files (index.md/log.md/SCHEMA.md at any level),
/// and the `references/` subtree (handled as sources).
fn collect_okf_concepts(bundle: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            if path.file_name().and_then(|s| s.to_str()) == Some("references") {
                continue;
            }
            collect_okf_concepts(bundle, &path, out);
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !(fname.ends_with(".md") || fname.ends_with(".markdown")) {
            continue;
        }
        if matches!(fname, "index.md" | "log.md" | "SCHEMA.md") {
            continue;
        }
        out.push(path);
    }
}

/// Import an OKF bundle as a new KMS named `name` at `scope`.
///
/// Permissive per OKF §9: concepts may live anywhere in the tree (not
/// just `pages/`), unknown types / missing fields / broken links are
/// tolerated. The KMS `index.md` is rebuilt fresh from the imported
/// pages rather than translated, so the result is always KMS-native.
/// Errors if a KMS by that name already exists at the target scope.
pub fn import_okf(bundle: &Path, name: &str, scope: KmsScope) -> Result<OkfImportReport> {
    validate_name(name)?;
    if !bundle.is_dir() {
        return Err(Error::Tool(format!(
            "'{}' is not a directory",
            bundle.display()
        )));
    }
    let target_root = scope_root(scope)
        .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?
        .join(name);
    if target_root.exists() {
        return Err(Error::Tool(format!(
            "KMS '{name}' already exists at {} scope — drop it or pick another name",
            scope.as_str()
        )));
    }
    let kref = create(name, scope)?;
    let mut report = OkfImportReport {
        root: kref.root.clone(),
        ..Default::default()
    };

    // ── concepts → pages/ ─────────────────────────────────────────
    // Two passes: first assign every concept a flat stem and build a
    // bundle-path → stem map, then write each page rewriting its links
    // to follow the flattening (a concept at `/tables/x.md` becomes
    // `pages/tables-x.md`, so links to it must too).
    let mut concepts = Vec::new();
    collect_okf_concepts(bundle, bundle, &mut concepts);
    concepts.sort();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stem_for: Vec<String> = Vec::with_capacity(concepts.len());
    let mut rel_to_stem: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for path in &concepts {
        let rel = path.strip_prefix(bundle).unwrap_or(path);
        let base = okf_concept_stem(rel);
        let mut stem = base.clone();
        let mut n = 2;
        while used.contains(&stem) {
            stem = format!("{base}-{n}");
            n += 1;
        }
        used.insert(stem.clone());
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        rel_to_stem.insert(rel_str, stem.clone());
        stem_for.push(stem);
    }
    for (path, stem) in concepts.iter().zip(stem_for.iter()) {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let (fm, body) = parse_frontmatter(&raw);
        let body = rewrite_okf_concept_links(&body, &rel_to_stem);
        let page = write_frontmatter(&okf_fm_to_kms(&fm), &okf_links_to_kms(&body));
        write_file(kref.pages_dir().join(format!("{stem}.md")), page.as_bytes())
            .map_err(|e| Error::Tool(format!("write page {stem}: {e}")))?;
        report.pages += 1;
    }

    // ── references/ → sources/ ────────────────────────────────────
    let refs_dir = bundle.join("references");
    if refs_dir.is_dir() {
        let sources_dir = kref.root.join("sources");
        std::fs::create_dir_all(&sources_dir)
            .map_err(|e| Error::Tool(format!("mkdir sources: {e}")))?;
        if let Ok(entries) = std::fs::read_dir(&refs_dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() || !ft.is_file() {
                    continue;
                }
                let path = entry.path();
                let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let dst = sources_dir.join(fname);
                let is_md = matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("md") | Some("markdown")
                );
                if is_md {
                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    let (sfm, sbody) = parse_frontmatter(&content);
                    // Unwrap the `type: Source` shim we add on export.
                    let restored = if sfm.get("type").map(|t| t == "Source").unwrap_or(false) {
                        sbody
                    } else {
                        content
                    };
                    write_file(&dst, restored.as_bytes())
                        .map_err(|e| Error::Tool(format!("write source {fname}: {e}")))?;
                } else {
                    std::fs::copy(&path, &dst)
                        .map_err(|e| Error::Tool(format!("copy source {fname}: {e}")))?;
                }
                report.sources += 1;
            }
        }
    }

    // ── log.md (OKF → KMS form), if present ───────────────────────
    if let Ok(log_raw) = std::fs::read_to_string(bundle.join("log.md")) {
        write_file(kref.log_path(), okf_log_to_kms(&log_raw).as_bytes())
            .map_err(|e| Error::Tool(format!("write log.md: {e}")))?;
    }

    // ── SCHEMA.md (strip the type shim), if present ───────────────
    if let Ok(schema) = std::fs::read_to_string(bundle.join("SCHEMA.md")) {
        let (sfm, sbody) = parse_frontmatter(&schema);
        let restored = if sfm.get("type").map(|t| t == "OKF Schema").unwrap_or(false) {
            sbody
        } else {
            schema
        };
        write_file(kref.schema_path(), restored.as_bytes())
            .map_err(|e| Error::Tool(format!("write SCHEMA.md: {e}")))?;
    }

    // ── manifest.json (verbatim), if present ──────────────────────
    if bundle.join("manifest.json").is_file() {
        let _ = std::fs::copy(bundle.join("manifest.json"), kref.manifest_path());
    }

    // ── Rebuild the KMS index from the imported pages ─────────────
    rebuild_index_from_pages(&kref)?;

    Ok(report)
}

/// Rebuild `index.md` from the current `pages/` contents — one bullet
/// per page, summary taken from the page's `topic`/`description`
/// frontmatter, falling back to its first body line.
fn rebuild_index_from_pages(kref: &KmsRef) -> Result<()> {
    // Was a third, subtly-different index renderer (topic-only
    // summaries, no categories, no source block). Folded into the one
    // generator so an OKF import lands the same index every other
    // write path produces.
    rebuild_index(kref).map(|_| ())
}

/// Knobs for [`auto_link`].
#[derive(Debug, Clone)]
pub struct AutoLinkOptions {
    /// Minimum length (in chars) for a dictionary key to be eligible.
    /// Anything shorter risks linking on incidental words ("do" matches
    /// inside "domain", "test" inside "testing", etc.).
    pub min_len: usize,
    /// Dry-run by default. `true` writes the modified pages back to disk.
    pub apply: bool,
}

impl Default for AutoLinkOptions {
    fn default() -> Self {
        Self {
            min_len: 4,
            apply: false,
        }
    }
}

/// One proposed link insertion. Useful for dry-run preview + reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkHit {
    pub page_stem: String,
    pub target_slug: String,
    pub matched: String,
}

/// Aggregate report returned by [`auto_link`].
#[derive(Debug, Default)]
pub struct AutoLinkReport {
    pub pages_scanned: u32,
    pub pages_modified: u32,
    pub links_added: u32,
    pub hits: Vec<LinkHit>,
}

/// Walk every page in `kref`, build a dictionary of slugs + frontmatter
/// titles + aliases, and insert `[[slug]]` wikilinks at the first
/// occurrence of each candidate inside other pages' bodies.
///
/// Skips (does not match inside):
/// - YAML frontmatter at the top of each page
/// - fenced code blocks (lines between ```` ``` ```` markers)
/// - Markdown headings (lines starting with `#`)
/// - existing wikilinks `[[...]]`, markdown links `[text](url)`, and
///   inline code spans `` `...` ``
/// - mentions of the page's own slug / title
///
/// Per page, each target is linked at most once (first occurrence) to
/// keep the rewrite quiet — heavy auto-linking turns prose into a
/// thicket. `opts.apply == false` (the default) returns the report
/// without writing anything.
///
/// **dev-plan/36 Tier 1.D note:** `auto_link` rewrites N pages in
/// one call. Per-page index hooks here would require threading the
/// affected-page set through. Deferred to Tier 3's auto-rebuild-on-
/// stale-manifest path (same rationale as `merge_into` above); or
/// the operator can run `/kms reindex <name>` after a bulk
/// auto-link to force a fresh build.
pub fn auto_link(kref: &KmsRef, opts: AutoLinkOptions) -> Result<AutoLinkReport> {
    use regex::Regex;
    use std::collections::HashMap;

    // dev-plan/41: a read-only shared KMS can't be rewritten. Dry-run
    // (preview) stays allowed; `--apply` is refused.
    if opts.apply {
        ensure_writable(kref)?;
    }

    let pages_dir = kref.pages_dir();
    if !pages_dir.is_dir() {
        return Ok(AutoLinkReport::default());
    }

    // ── 1. Pass: build the dictionary from every page ─────────────
    // Map from a *literal text key* to a target slug. Multiple keys
    // (slug, frontmatter title, alias entries) may all point at the
    // same target.
    let mut dictionary: HashMap<String, String> = HashMap::new();
    let mut page_files: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&pages_dir)
        .map_err(|e| Error::Tool(format!("readdir {}: {e}", pages_dir.display())))?
    {
        let entry = entry.map_err(|e| Error::Tool(format!("readdir entry: {e}")))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Don't link to the reserved starter pages.
        if RESERVED_PAGE_STEMS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&stem))
        {
            continue;
        }
        page_files.push((stem.clone(), path.clone()));
        if stem.chars().count() >= opts.min_len {
            dictionary.entry(stem.clone()).or_insert(stem.clone());
        }
        // Frontmatter-derived synonyms.
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let (fm, _) = parse_frontmatter(&body);
        if let Some(title) = fm.get("title") {
            let t = title.trim().trim_matches('"').trim();
            if t.chars().count() >= opts.min_len {
                dictionary.entry(t.to_string()).or_insert(stem.clone());
            }
        }
        if let Some(aliases) = fm.get("aliases") {
            // `aliases: foo, bar, baz` — comma-separated. The hand-rolled
            // YAML parser doesn't understand `[..]` list syntax, so the
            // value is one string.
            for raw in aliases.split(',') {
                let alias = raw.trim().trim_matches('"').trim_matches('\'').trim();
                if alias.chars().count() >= opts.min_len {
                    dictionary.entry(alias.to_string()).or_insert(stem.clone());
                }
            }
        }
    }

    // Sort candidates longest-first so "PostgreSQL Driver" wins over
    // "PostgreSQL" when both are in the dictionary (avoid the shorter
    // key claiming a substring of the longer one's match).
    let mut candidates: Vec<(String, String)> = dictionary.into_iter().collect();
    candidates.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));

    // Pre-compile a case-insensitive whole-token regex per candidate.
    // `\b` in the `regex` crate is Unicode-aware, so non-ASCII titles
    // also match cleanly at word boundaries.
    let mut compiled: Vec<(Regex, String, String)> = Vec::new();
    for (key, slug) in &candidates {
        let escaped = regex::escape(key);
        // `\b` only makes sense next to a word character; a key such as
        // "Alibaba (Qwen)" ends in `)` and would never match with it.
        let is_word = |c: Option<char>| c.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
        let lead = if is_word(key.chars().next()) {
            r"\b"
        } else {
            ""
        };
        let tail = if is_word(key.chars().next_back()) {
            r"\b"
        } else {
            ""
        };
        let re = match Regex::new(&format!(r"(?i){lead}{escaped}{tail}")) {
            Ok(r) => r,
            Err(_) => continue, // pathological key; skip rather than abort
        };
        compiled.push((re, key.clone(), slug.clone()));
    }

    // Pattern for "protected" inline regions we must not match inside:
    // existing wikilinks, markdown links, inline code spans — and bare
    // URLs. A bare URL is none of the first three, so a vault-wide
    // `/kms link --apply` used to rewrite a word inside the address
    // itself: research pages ended up citing
    // `https://…deepseek-v4-adapted-[[huawei]]-chips…`, which resolves
    // nowhere. Found by `/kms verify` on an 18-page vault.
    let protect_re = Regex::new(
        r"(?:\[\[[^\]\n]+\]\]|\[[^\]\n]+\]\([^)\n]+\)|`[^`\n]+`|<?(?:kms://[^\n]*|[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s)>\]]+)>?)",
    )
    .expect("static regex");

    let mut report = AutoLinkReport::default();

    // ── 2. Pass: rewrite each page ────────────────────────────────
    for (stem, path) in &page_files {
        report.pages_scanned += 1;
        let original = std::fs::read_to_string(path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;

        // Preserve frontmatter verbatim — match only inside the body.
        let (frontmatter_block, body) = split_frontmatter_block(&original);

        let mut rewritten_body = String::with_capacity(body.len());
        let mut in_fence = false;
        // Slugs already linked in this page — first-occurrence policy.
        let mut linked_in_page: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // Also seed with self so a page never links to itself.
        linked_in_page.insert(stem.clone());
        // Bold mentions anywhere in the page win over an earlier plain
        // mention: reserve their slugs now so the first-occurrence
        // pass below leaves the plain text alone.
        for (_re, key, slug) in &compiled {
            if slug != stem && body.contains(&format!("**{key}**")) {
                linked_in_page.insert(slug.clone());
            }
        }

        for line in body.split_inclusive('\n') {
            let trimmed_start = line.trim_start();
            if trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~") {
                in_fence = !in_fence;
                rewritten_body.push_str(line);
                continue;
            }
            if in_fence || trimmed_start.starts_with('#') {
                rewritten_body.push_str(line);
                continue;
            }

            // Protect inline-code / existing-link spans by replacing with
            // sentinel placeholders, then run candidate matching on the
            // sanitized buffer, then restore.
            let mut placeholders: Vec<String> = Vec::new();
            let protected = protect_re.replace_all(line, |caps: &regex::Captures| {
                let idx = placeholders.len();
                placeholders.push(caps[0].to_string());
                format!("\u{0000}P{idx}\u{0000}")
            });
            let mut working = protected.into_owned();

            // A bold mention (`**Alibaba (Qwen)**`) is the author marking
            // a key entry: link every one of them, not just the first
            // plain occurrence, and drop the bold so the link is bare.
            for (_re, key, slug) in &compiled {
                if slug == stem {
                    continue;
                }
                let bold = format!("**{key}**");
                if working.contains(&bold) {
                    working = working.replace(&bold, &format!("[[{slug}|{key}]]"));
                    linked_in_page.insert(slug.clone());
                    report.links_added += 1;
                    report.hits.push(LinkHit {
                        page_stem: stem.clone(),
                        target_slug: slug.clone(),
                        matched: bold,
                    });
                }
            }

            for (re, _key, slug) in &compiled {
                if linked_in_page.contains(slug) {
                    continue;
                }
                if let Some(m) = re.find(&working) {
                    let matched_text = m.as_str().to_string();
                    let (start, end) = (m.start(), m.end());
                    // Keep what the author wrote as the display text —
                    // `[[alibaba]]` would render as "alibaba".
                    let replacement = if matched_text == *slug {
                        format!("[[{slug}]]")
                    } else {
                        format!("[[{slug}|{matched_text}]]")
                    };
                    working.replace_range(start..end, &replacement);
                    linked_in_page.insert(slug.clone());
                    report.links_added += 1;
                    report.hits.push(LinkHit {
                        page_stem: stem.clone(),
                        target_slug: slug.clone(),
                        matched: matched_text,
                    });
                }
            }

            // Restore protected placeholders.
            let restore_re = Regex::new(r"\u{0000}P(\d+)\u{0000}").expect("static regex");
            let restored = restore_re.replace_all(&working, |caps: &regex::Captures| {
                let n: usize = caps[1].parse().unwrap_or(usize::MAX);
                placeholders
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| caps[0].to_string())
            });
            rewritten_body.push_str(&restored);
        }

        if rewritten_body == body {
            continue;
        }
        report.pages_modified += 1;
        if opts.apply {
            let mut new_full =
                String::with_capacity(frontmatter_block.len() + rewritten_body.len());
            new_full.push_str(frontmatter_block);
            new_full.push_str(&rewritten_body);
            write_file(path, new_full.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        }
    }

    if opts.apply && report.pages_modified > 0 {
        append_log_header(kref, "link", "auto-link")?;
    }
    Ok(report)
}

/// Split a page into `(frontmatter_block_including_delimiters, body)`.
/// When no frontmatter is present, returns `("", whole)` so the caller
/// can blindly concatenate.
fn split_frontmatter_block(s: &str) -> (&str, &str) {
    if !s.starts_with("---\n") {
        return ("", s);
    }
    let after_first = 4;
    if let Some(end) = s[after_first..].find("\n---\n") {
        let split = after_first + end + "\n---\n".len();
        return (&s[..split], &s[split..]);
    }
    ("", s)
}

/// LLM-driven sibling of [`auto_link`]. For each page in the KMS,
/// send the body plus a digest of every *other* page (slug + title +
/// description) to the active model and ask which natural mentions
/// should become `[[<slug>]]` wikilinks. Then validate the model's
/// suggestions in Rust (anchor must appear in the body, target slug
/// must exist, no overlap with existing links / code / headings,
/// no self-references, first-occurrence-only) before writing.
///
/// Pages-only — `sources/` is deliberately excluded; sources are
/// raw artifacts, not navigable nodes.
///
/// Per-page call timeout: 900s (a single long-context call can
/// legitimately pause mid-stream while the model thinks). Cancellation
/// honored between pages and inside the chunked stream.
pub async fn auto_link_llm(
    kref: &KmsRef,
    opts: AutoLinkOptions,
    provider: &dyn crate::providers::Provider,
    model: &str,
    cancel: &crate::cancel::CancelToken,
) -> Result<AutoLinkReport> {
    use std::collections::HashSet;

    // dev-plan/41: refuse writes to a read-only shared KMS (dry-run ok).
    if opts.apply {
        ensure_writable(kref)?;
    }

    let pages_dir = kref.pages_dir();
    if !pages_dir.is_dir() {
        return Ok(AutoLinkReport::default());
    }

    struct PageEntry {
        stem: String,
        title: String,
        description: String,
        body: String,
        frontmatter_block: String,
        path: std::path::PathBuf,
    }

    // ── 1. Page index ─────────────────────────────────────────────
    let mut entries: Vec<PageEntry> = Vec::new();
    for entry in std::fs::read_dir(&pages_dir)
        .map_err(|e| Error::Tool(format!("readdir {}: {e}", pages_dir.display())))?
    {
        let entry = entry.map_err(|e| Error::Tool(format!("readdir entry: {e}")))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if RESERVED_PAGE_STEMS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&stem))
        {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
        let (fm_block, body) = split_frontmatter_block(&raw);
        let (fm, _) = parse_frontmatter(&raw);
        let title = fm
            .get("title")
            .map(|t| t.trim().trim_matches('"').trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| stem.clone());
        let description = fm
            .get("description")
            .map(|d| d.trim().trim_matches('"').trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| first_meaningful_line(body));
        entries.push(PageEntry {
            stem,
            title,
            description,
            body: body.to_string(),
            frontmatter_block: fm_block.to_string(),
            path,
        });
    }

    let valid_slugs: HashSet<String> = entries.iter().map(|e| e.stem.clone()).collect();
    let mut report = AutoLinkReport::default();

    // ── 2. Per-page LLM call ──────────────────────────────────────
    for page in &entries {
        if cancel.is_cancelled() {
            return Err(Error::Tool("/kms link --llm cancelled".into()));
        }
        report.pages_scanned += 1;

        let others: Vec<(String, String, String)> = entries
            .iter()
            .filter(|e| e.stem != page.stem)
            .map(|e| (e.stem.clone(), e.title.clone(), e.description.clone()))
            .collect();
        if others.is_empty() {
            continue;
        }

        let prompt = build_llm_link_prompt(&page.stem, &page.body, &others);
        let raw = match llm_link_oneshot(
            provider,
            model,
            prompt,
            std::time::Duration::from_secs(900),
            cancel,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "\x1b[33m[/kms link --llm] page {}: LLM call failed: {e}; skipping\x1b[0m",
                    page.stem
                );
                continue;
            }
        };
        let parsed = match parse_llm_link_response(&raw) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "\x1b[33m[/kms link --llm] page {}: response unparseable: {e}; skipping\x1b[0m",
                    page.stem
                );
                continue;
            }
        };

        let (new_body, hits) = apply_llm_links(&page.body, &parsed, &page.stem, &valid_slugs);
        if hits.is_empty() {
            continue;
        }
        report.pages_modified += 1;
        report.links_added += hits.len() as u32;
        for hit in hits {
            report.hits.push(hit);
        }
        if opts.apply {
            let mut full = String::with_capacity(page.frontmatter_block.len() + new_body.len());
            full.push_str(&page.frontmatter_block);
            full.push_str(&new_body);
            write_file(&page.path, full.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", page.path.display())))?;
        }
    }

    if opts.apply && report.pages_modified > 0 {
        append_log_header(kref, "link-llm", "auto-link-llm")?;
    }
    Ok(report)
}

/// Build the prompt sent to the model for one page. We include the
/// full body so the model has surrounding context, and the digest of
/// every other page (slug + title + 1-line description) as the
/// candidate target set. The response schema is fixed JSON so Rust
/// can validate every suggestion before writing.
fn build_llm_link_prompt(
    source_stem: &str,
    source_body: &str,
    others: &[(String, String, String)],
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are linking pages in a thClaws KMS (knowledge management system).\n\n\
        Given the SOURCE page below and a digest of OTHER pages in the same KMS, \
        return a JSON object listing the `[[wikilink]]` insertions you would make.\n\n\
        Rules:\n\
        - Each link has an `anchor` (an exact substring of the source body) and \
          a `target_slug` (one of the slugs in the digest).\n\
        - Only insert a link when the anchor naturally refers to the target page's topic.\n\
        - Do not link inside existing wikilinks `[[..]]`, markdown links `[text](url)`, \
          inline code `` `..` ``, fenced code blocks, headings, or YAML frontmatter.\n\
        - Each target slug appears AT MOST ONCE per source page (first natural mention).\n\
        - Skip generic / weak relationships — only link when the connection is specific \
          and would genuinely help a reader follow the thought.\n\
        - Do NOT invent slugs that aren't in the digest. Do NOT modify the body in any \
          way other than the wikilink insertions described.\n\
        - Return ONLY a JSON object — no prose, no markdown code fences.\n\n",
    );
    prompt.push_str(&format!("SOURCE PAGE SLUG: {source_stem}\n\n"));
    prompt.push_str("SOURCE BODY:\n---\n");
    prompt.push_str(source_body);
    prompt.push_str("\n---\n\nOTHER PAGES (slug — title — description):\n");
    for (slug, title, desc) in others {
        let desc_trimmed: String = desc.chars().take(160).collect();
        prompt.push_str(&format!("- {slug} — {title} — {desc_trimmed}\n"));
    }
    prompt.push_str(
        "\nRespond with this exact schema:\n\
        {\"links\": [{\"anchor\": \"<exact body substring>\", \"target_slug\": \"<digest slug>\"}, ...]}\n\
        \nIf nothing should link, return: {\"links\": []}\n",
    );
    prompt
}

/// Parse the LLM's JSON response. Tolerant of code-fence wrappers
/// (` ```json\n{...}\n``` `) and leading/trailing prose. Returns
/// `(anchor, target_slug)` pairs.
fn parse_llm_link_response(raw: &str) -> Result<Vec<(String, String)>> {
    let trimmed = raw.trim();
    // Strip ``` / ```json fences if the model wrapped its output.
    let inner = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```").trim())
        .unwrap_or(trimmed);
    let start = inner
        .find('{')
        .ok_or_else(|| Error::Tool("LLM response contained no JSON object".into()))?;
    let end = inner
        .rfind('}')
        .ok_or_else(|| Error::Tool("LLM response had no closing brace".into()))?;
    if end < start {
        return Err(Error::Tool("LLM response braces in wrong order".into()));
    }
    let json = &inner[start..=end];

    #[derive(serde::Deserialize)]
    struct Item {
        anchor: String,
        target_slug: String,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        links: Vec<Item>,
    }
    let resp: Resp =
        serde_json::from_str(json).map_err(|e| Error::Tool(format!("LLM JSON parse: {e}")))?;
    Ok(resp
        .links
        .into_iter()
        .map(|i| (i.anchor, i.target_slug))
        .collect())
}

/// Validate + apply each `(anchor, target_slug)` candidate to the
/// page body. Same protection logic as the deterministic
/// [`auto_link`]: fenced code, headings, inline code, existing
/// wikilinks / markdown links are all off-limits. First occurrence
/// only per target. Self-references and unknown slugs are dropped.
fn apply_llm_links(
    body: &str,
    candidates: &[(String, String)],
    source_stem: &str,
    valid_slugs: &std::collections::HashSet<String>,
) -> (String, Vec<LinkHit>) {
    use regex::Regex;
    let protect_re = Regex::new(r"(?:\[\[[^\]\n]+\]\]|\[[^\]\n]+\]\([^)\n]+\)|`[^`\n]+`)")
        .expect("static regex");

    let mut working = body.to_string();
    let mut hits: Vec<LinkHit> = Vec::new();
    let mut linked: std::collections::HashSet<String> = std::collections::HashSet::new();
    linked.insert(source_stem.to_string()); // never self-link

    for (anchor, target) in candidates {
        if !valid_slugs.contains(target) {
            continue;
        }
        if linked.contains(target) {
            continue;
        }
        if anchor.trim().is_empty() {
            continue;
        }
        let Some(pos) = find_unprotected_occurrence(&working, anchor, &protect_re) else {
            continue;
        };
        let end = pos + anchor.len();
        // Use `[[slug]]` when the anchor matches the slug exactly
        // (case-sensitive), otherwise `[[slug|anchor]]` to preserve
        // the visible text the page already used.
        let replacement = if anchor == target {
            format!("[[{target}]]")
        } else {
            format!("[[{target}|{anchor}]]")
        };
        working.replace_range(pos..end, &replacement);
        linked.insert(target.clone());
        hits.push(LinkHit {
            page_stem: source_stem.to_string(),
            target_slug: target.clone(),
            matched: anchor.clone(),
        });
    }
    (working, hits)
}

/// Find the first byte offset of `anchor` in `body` that lives
/// outside a fenced code block, a heading line, and any protected
/// inline region (existing wikilink / markdown link / inline code).
/// Returns `None` if `anchor` doesn't appear anywhere safe.
fn find_unprotected_occurrence(
    body: &str,
    anchor: &str,
    protect_re: &regex::Regex,
) -> Option<usize> {
    let mut offset = 0;
    let mut in_fence = false;
    for line in body.split_inclusive('\n') {
        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~") {
            in_fence = !in_fence;
            offset += line.len();
            continue;
        }
        if in_fence || trimmed_start.starts_with('#') {
            offset += line.len();
            continue;
        }
        let protected_ranges: Vec<(usize, usize)> = protect_re
            .find_iter(line)
            .map(|m| (m.start(), m.end()))
            .collect();
        if let Some(local_pos) = line.find(anchor) {
            let local_end = local_pos + anchor.len();
            let inside_protected = protected_ranges
                .iter()
                .any(|(s, e)| *s <= local_pos && local_end <= *e);
            if !inside_protected {
                return Some(offset + local_pos);
            }
        }
        offset += line.len();
    }
    None
}

/// Streaming one-shot helper for the LLM auto-linker. Mirrors the
/// research-pipeline `oneshot` (same cancel + chunk-timeout
/// semantics) but lives here so kms.rs doesn't pull in the research
/// module just for one call shape.
async fn llm_link_oneshot(
    provider: &dyn crate::providers::Provider,
    model: &str,
    prompt: String,
    timeout: std::time::Duration,
    cancel: &crate::cancel::CancelToken,
) -> Result<String> {
    use crate::providers::{ProviderEvent, StreamRequest};
    use crate::types::Message;
    use futures::StreamExt;

    let req = StreamRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
        max_tokens: 4096,
        thinking_budget: None,
        stream_chunk_timeout_override: Some(timeout),
    };
    let stream_fut = provider.stream(req);
    let mut stream = match tokio::time::timeout(timeout, stream_fut).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(Error::Tool(
                "auto-link LLM call timed out building stream".into(),
            ))
        }
    };
    let mut text = String::new();
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Tool("auto-link LLM call cancelled".into()));
        }
        let next = tokio::select! {
            ev = tokio::time::timeout(timeout, stream.next()) => ev,
            _ = cancel.cancelled() => {
                return Err(Error::Tool("auto-link LLM call cancelled".into()));
            }
        };
        match next {
            Ok(Some(Ok(ProviderEvent::TextDelta(s)))) => text.push_str(&s),
            Ok(Some(Ok(ProviderEvent::MessageStop { usage, .. }))) => {
                if let Some(u) = &usage {
                    crate::usage::note_side_usage(model, u);
                }
                break;
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => break,
            Err(_) => {
                return Err(Error::Tool(
                    "auto-link LLM call timed out reading stream".into(),
                ))
            }
        }
    }
    Ok(text)
}

/// Index maintenance after a page is removed. `index.md` is fully
/// regenerated rather than line-edited: the old code filtered out
/// lines containing `(pages/<stem>.md)`, which also nuked any index
/// line whose *summary text* happened to mention another page's link.
fn remove_index_bullet(kref: &KmsRef, _stem: &str) -> Result<()> {
    if index_batch_active() {
        return Ok(());
    }
    rebuild_index(kref).map(|_| ())
}

/// Index maintenance after a page is written.
///
/// This used to append a bullet to `index.md` in write order while the
/// system prompt rebuilt its own categorised list from frontmatter and
/// ignored `index.md` entirely — two indexes for the same KMS, drifting
/// apart, with the human reading one and the model the other. Both now
/// come from [`scan_index_entries`], so `index.md` IS what the model
/// sees (plus the source catalogue), regenerated on every write.
///
/// The `summary` / `category` / `existed` arguments are kept so call
/// sites read the same; the values are re-derived from the page on
/// disk, which is authoritative.
fn update_index_for_write(
    kref: &KmsRef,
    _stem: &str,
    _summary: &str,
    _category: Option<&str>,
    _existed: bool,
) -> Result<()> {
    if index_batch_active() {
        return Ok(());
    }
    rebuild_index(kref).map(|_| ())
}

/// M6.25 BUG #7: append a header-style log entry for greppability.
/// `## [YYYY-MM-DD] verb | alias`. Pre-fix `- date verb src → dest`
/// bullets weren't greppable as "give me the last 5 ingests".
fn append_log_header(kref: &KmsRef, verb: &str, alias: &str) -> Result<()> {
    use std::io::Write;
    let path = kref.log_path();
    let line = format!("## [{}] {verb} | {alias}\n", crate::usage::today_str());
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    f.write_all(line.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #3: lint — pure-read health check.

/// What `lint()` found. Each list is a category of issue.
#[derive(Debug, Default)]
pub struct LintReport {
    pub orphan_pages: Vec<String>, // page exists but no inbound link from any other page
    pub broken_links: Vec<(String, String)>, // (page, target) where pages/<target>.md doesn't exist
    pub index_orphans: Vec<String>, // index entry but no underlying file
    pub missing_in_index: Vec<String>, // page file but no index entry
    pub missing_frontmatter: Vec<String>, // page has no `---` block
    /// Archived sources no page cites — ingested material nothing was
    /// ever built from. Previously invisible: lint looked at `pages/`
    /// only, so an ingest that produced nothing usable left no trace.
    pub orphan_sources: Vec<String>,
    /// `(page, source)` where the page's frontmatter `sources:` names a
    /// file that isn't in `sources/`.
    pub dangling_source_refs: Vec<(String, String)>,
    /// Pages still carrying `status: derived` — an ingest whose page
    /// nobody has curated yet. Not an error; a work queue.
    pub derived_pages: Vec<String>,
    /// `(page, updated)` still carrying `status: researching` — the
    /// placeholder `/research` writes before it starts and replaces when
    /// it finishes. One that is still here is a run that died: nothing
    /// else flagged it, it sat in the index advertising its placeholder
    /// text, and every later planner read it as a note that already
    /// covers the subject.
    pub abandoned_research: Vec<(String, String)>,
    /// (page_stem, source_key, missing_field) — `source_key` is `"global"`
    /// or the page's `category:` value, indicating which manifest rule the
    /// field came from. Empty when no manifest exists or the manifest's
    /// `frontmatter_required` map is empty.
    pub missing_required_fields: Vec<(String, String, String)>,
}

impl LintReport {
    pub fn total_issues(&self) -> usize {
        self.orphan_pages.len()
            + self.broken_links.len()
            + self.index_orphans.len()
            + self.missing_in_index.len()
            + self.missing_frontmatter.len()
            + self.missing_required_fields.len()
            + self.orphan_sources.len()
            + self.dangling_source_refs.len()
            + self.abandoned_research.len()
    }
}

/// One lint issue, flattened so the GUI can render a list and open
/// what each row is about (dev-plan/64 P5.6). `/kms lint` prints the
/// same report grouped by category; a person reading it in a sidebar
/// wants to click the page, not retype its name.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LintFinding {
    /// Category slug, for grouping and for a stable key.
    pub kind: String,
    /// What is wrong, as a sentence about `target`.
    pub detail: String,
    /// `page` | `source` | empty when there is nothing to open.
    pub target_kind: String,
    /// The file stem the viewer should open.
    pub target: String,
}

/// [`LintReport`] as a flat list, worst first: things that are broken,
/// then things that are unfinished, then things that are merely
/// unreferenced.
pub fn lint_findings(report: &LintReport) -> Vec<LintFinding> {
    let mut out = Vec::new();
    let mut push = |kind: &str, detail: String, target_kind: &str, target: &str| {
        out.push(LintFinding {
            kind: kind.into(),
            detail,
            target_kind: target_kind.into(),
            target: target.into(),
        });
    };
    for (page, target) in &report.broken_links {
        push(
            "broken_link",
            format!("links to `{target}`, which is not a page"),
            "page",
            page,
        );
    }
    for (page, source) in &report.dangling_source_refs {
        push(
            "dangling_source",
            format!("cites `{source}`, which is not archived"),
            "page",
            page,
        );
    }
    for page in &report.missing_frontmatter {
        push(
            "missing_frontmatter",
            "has no frontmatter".into(),
            "page",
            page,
        );
    }
    for (page, key, field) in &report.missing_required_fields {
        push(
            "missing_field",
            format!("has no `{field}:` (required by {key})"),
            "page",
            page,
        );
    }
    for stem in &report.index_orphans {
        push(
            "index_orphan",
            format!("`{stem}` is in the index, but the file is gone"),
            "",
            "",
        );
    }
    for (page, updated) in &report.abandoned_research {
        push(
            "abandoned_research",
            format!("still `status: researching` since {updated} — the run died"),
            "page",
            page,
        );
    }
    for page in &report.derived_pages {
        push(
            "derived",
            "came from an ingest and has not been written up".into(),
            "page",
            page,
        );
    }
    for page in &report.missing_in_index {
        push(
            "missing_in_index",
            "is not in the index".into(),
            "page",
            page,
        );
    }
    for page in &report.orphan_pages {
        push("orphan_page", "nothing links to it".into(), "page", page);
    }
    for src in &report.orphan_sources {
        // The report names the file; the viewer takes the stem, and
        // re-resolves the extension itself (a source is not always .md).
        let stem = Path::new(src)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| src.clone());
        push(
            "orphan_source",
            "is archived and nothing cites it".into(),
            "source",
            &stem,
        );
    }
    out
}

/// Walk a KMS and report common health issues. Pure-read; doesn't
/// modify the wiki. Inbound-link detection is greedy: any markdown
/// link `[*](pages/<stem>.md)` counts.
pub fn lint(kref: &KmsRef) -> Result<LintReport> {
    use std::collections::HashSet;
    let mut report = LintReport::default();

    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(report),
    };

    let mut all_stems: HashSet<String> = HashSet::new();
    let mut page_bodies: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        all_stems.insert(stem.clone());
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        page_bodies.push((stem, body));
    }

    // Frontmatter audit + outbound link extraction.
    // Load the manifest's required-fields map once. Empty (or absent) skips
    // the per-page required-field check entirely — keeps legacy KMSes silent.
    let required_fields = kref
        .read_manifest()
        .map(|m| m.frontmatter_required)
        .unwrap_or_default();
    let mut inbound_targets: HashSet<String> = HashSet::new();
    let source_stems: HashSet<String> = list_sources(kref)
        .iter()
        .flat_map(|s| [s.stem.clone(), s.file_name()])
        .collect();
    let mut cited_sources: HashSet<String> = HashSet::new();
    // `/research` writes `sources: [3, 4, 5]` — indices into the KMS's
    // citation registry, not filenames. Lint read them as filenames, so a
    // freshly researched vault reported every page as citing missing
    // archives (99 findings on a 39-page base, all false), which is the
    // quickest way to teach someone never to run it again.
    let registry: std::collections::HashMap<u32, String> =
        crate::research::registry::SourceRegistry::load(kref)
            .meta()
            .into_iter()
            // `meta()` is `(index, title, url)` — the URL is last.
            .map(|(index, _title, url)| (index, url))
            .collect();
    for (stem, body) in &page_bodies {
        let (fm, _rest) = parse_frontmatter(body);
        // Source provenance: `sources:` naming a file that isn't there
        // is a broken citation, and every named file counts as cited so
        // the orphan-source pass below can tell dead archives from live
        // ones.
        if let Some(raw) = fm.get("sources") {
            for token in sources_entries(raw) {
                if source_entry_is_external_provenance(token) {
                    continue;
                }
                // A citation index: good if the registry knows it, and its
                // archive (when one was written) counts as cited.
                if let Ok(index) = token.parse::<u32>() {
                    match registry.get(&index) {
                        Some(url) => {
                            let archive = crate::research::kms_writer::url_to_filename(url);
                            if source_stems.contains(&archive) {
                                cited_sources.insert(archive);
                            }
                        }
                        None => report.dangling_source_refs.push((
                            stem.clone(),
                            format!("[{index}] (not in the citation registry)"),
                        )),
                    }
                    continue;
                }
                if source_stems.contains(token) {
                    cited_sources.insert(
                        token
                            .rsplit_once('.')
                            .map(|(s, _)| s)
                            .unwrap_or(token)
                            .to_string(),
                    );
                } else {
                    report
                        .dangling_source_refs
                        .push((stem.clone(), token.to_string()));
                }
            }
        }
        if fm.get("status").map(|s| s.trim()) == Some("derived") {
            report.derived_pages.push(stem.clone());
        }
        if fm.get("status").map(|s| s.trim()) == Some("researching") {
            report.abandoned_research.push((
                stem.clone(),
                fm.get("updated")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
            ));
        }
        for target in extract_source_link_targets(body) {
            cited_sources.insert(
                target
                    .rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(&target)
                    .to_string(),
            );
        }
        if fm.is_empty() {
            report.missing_frontmatter.push(stem.clone());
        } else if !required_fields.is_empty() {
            // Check global rules first, then any category-specific rules.
            // The same field listed under both keys is reported twice — by
            // design, so the user can see which rule fired and remove the
            // redundancy from their manifest.
            let category = fm.get("category").map(String::as_str).unwrap_or("");
            for source_key in ["global", category] {
                if source_key.is_empty() {
                    continue;
                }
                if let Some(fields) = required_fields.get(source_key) {
                    for field in fields {
                        if !fm.contains_key(field) {
                            report.missing_required_fields.push((
                                stem.clone(),
                                source_key.to_string(),
                                field.clone(),
                            ));
                        }
                    }
                }
            }
        }
        // Markdown links, `[[wikilinks]]` and `related:` all count as
        // inbound links. Only the first did before — and `auto_link`,
        // the KMS's own linker, writes wikilinks, so running
        // `/kms link --apply` linked the whole vault and lint still
        // reported every page as an orphan.
        for target in outbound_page_links(body) {
            inbound_targets.insert(target.clone());
            if !all_stems.contains(&target) {
                report.broken_links.push((stem.clone(), target));
            }
        }
    }

    // Orphan pages: exist on disk but no other page links to them.
    for (stem, _) in &page_bodies {
        if !inbound_targets.contains(stem) {
            report.orphan_pages.push(stem.clone());
        }
    }

    // Orphan sources: archived but nothing stands on them.
    for src in list_sources(kref) {
        if !cited_sources.contains(&src.stem) {
            report.orphan_sources.push(src.file_name());
        }
    }

    // Index <-> filesystem cross-check.
    let index = kref.read_index();
    let index_re = regex::Regex::new(r"\(pages/([^)]+?)\.md\)").unwrap();
    let mut indexed: HashSet<String> = HashSet::new();
    for cap in index_re.captures_iter(&index) {
        indexed.insert(cap[1].to_string());
    }
    for stem in &indexed {
        if !all_stems.contains(stem) {
            report.index_orphans.push(stem.clone());
        }
    }
    for stem in &all_stems {
        if !indexed.contains(stem) {
            report.missing_in_index.push(stem.clone());
        }
    }

    report.orphan_pages.sort();
    report.broken_links.sort();
    report.index_orphans.sort();
    report.missing_in_index.sort();
    report.missing_frontmatter.sort();
    report.missing_required_fields.sort();
    report.orphan_sources.sort();
    report.dangling_source_refs.sort();
    report.derived_pages.sort();
    report.abandoned_research.sort();
    report.broken_links.dedup();
    Ok(report)
}

// ────────────────────────────────────────────────────────────────────────
// Schema migrations — chained version upgrades anchored on KmsManifest.

/// Sentinel for any KMS that predates the manifest entirely. Treated as
/// "0.x" by the migration chain so legacy stores get bumped to 1.0 the
/// first time `/kms migrate` runs.
pub const LEGACY_SCHEMA_VERSION: &str = "0.x";

/// One step in the migration chain. `from`/`to` are the `schema_version`
/// strings as they appear in `manifest.json`. The `apply` function takes
/// a `dry_run` flag — in dry-run mode it must not touch the filesystem;
/// in live mode it returns descriptions of what was actually written.
pub struct Migration {
    pub from: &'static str,
    pub to: &'static str,
    pub apply: fn(&KmsRef, dry_run: bool) -> Result<Vec<String>>,
}

/// Registry of known migrations, in chain order. Add a new entry when
/// the schema changes; the resolver in `migrate()` walks `from → to`
/// until it reaches `KMS_SCHEMA_VERSION`.
pub fn migrations() -> Vec<Migration> {
    vec![Migration {
        from: LEGACY_SCHEMA_VERSION,
        to: "1.0",
        apply: migrate_0_to_1,
    }]
}

/// 0.x → 1.0: write the initial manifest with empty enforcement.
/// Pure additive change — no page bodies touched, no index changes.
/// Lint behaviour is identical before and after; the manifest just
/// anchors future migrations and gives users a place to declare
/// `frontmatter_required` rules.
fn migrate_0_to_1(kref: &KmsRef, dry_run: bool) -> Result<Vec<String>> {
    let manifest_path = kref.manifest_path();
    let actions = vec![format!(
        "write {} (schema_version: 1.0, frontmatter_required: empty)",
        manifest_path.display()
    )];
    if !dry_run {
        let manifest = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: std::collections::BTreeMap::new(),
            entry: entry_page(kref),
        };
        write_file(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".into()),
        )
        .map_err(|e| Error::Tool(format!("write {}: {e}", manifest_path.display())))?;
        append_log_header(kref, "migrated", "0.x → 1.0")?;
    }
    Ok(actions)
}

/// Detect the current schema version. Absent manifest, or manifest with
/// empty `schema_version`, is treated as legacy `0.x` — that's how every
/// KMS created before the manifest feature looks on disk.
pub fn detect_schema_version(kref: &KmsRef) -> String {
    match kref.read_manifest() {
        Some(m) if !m.schema_version.is_empty() => m.schema_version,
        _ => LEGACY_SCHEMA_VERSION.into(),
    }
}

#[derive(Debug)]
pub struct MigrationStep {
    pub from: String,
    pub to: String,
    pub actions: Vec<String>,
}

#[derive(Debug)]
pub struct MigrationReport {
    pub current_version: String,
    pub target_version: String,
    pub steps: Vec<MigrationStep>,
    pub dry_run: bool,
}

/// Walk the migration chain from the KMS's current schema_version up to
/// `KMS_SCHEMA_VERSION`. In dry-run mode, returns the plan without
/// writing. In live mode, applies each step and returns what happened.
///
/// Idempotent: a KMS already at the latest version returns a report
/// with no steps and `current_version == target_version`.
pub fn migrate(kref: &KmsRef, dry_run: bool) -> Result<MigrationReport> {
    let initial = detect_schema_version(kref);
    let target = KMS_SCHEMA_VERSION.to_string();
    let mut report = MigrationReport {
        current_version: initial.clone(),
        target_version: target.clone(),
        steps: Vec::new(),
        dry_run,
    };
    if initial == target {
        return Ok(report);
    }
    let table = migrations();
    let mut current = initial;
    // Bound the loop defensively — `table` is hand-edited, but a bad
    // edit (e.g. a cycle 1.0 → 1.0) shouldn't spin forever.
    for _ in 0..table.len() + 1 {
        if current == target {
            break;
        }
        let Some(m) = table.iter().find(|m| m.from == current) else {
            return Err(Error::Tool(format!(
                "no migration path from schema version '{current}' to '{target}'"
            )));
        };
        let actions = (m.apply)(kref, dry_run)?;
        report.steps.push(MigrationStep {
            from: m.from.to_string(),
            to: m.to.to_string(),
            actions,
        });
        current = m.to.to_string();
    }
    if current != target {
        return Err(Error::Tool(format!(
            "migration chain stalled at '{current}', target '{target}' (likely a cycle in migrations())"
        )));
    }
    Ok(report)
}

// ────────────────────────────────────────────────────────────────────────
// User-facing report formatters. Live here (not in shell_dispatch.rs)
// because the CLI binary `thclaws-cli` is built without the `gui`
// feature — and `shell_dispatch` is gated behind `gui`. Pure functions:
// `&LintReport` / `&MigrationReport` / `&[StaleEntry]` → `String`.
// (M6.38.3 audit fix.)

/// Render a `LintReport` as the user-facing summary block emitted by
/// `/kms lint <name>`. Six issue categories; clean state returns a
/// short "no issues found" line.
pub fn format_lint_report(name: &str, report: &LintReport) -> String {
    let total = report.total_issues();
    if total == 0 {
        return format!("KMS '{name}': clean — no issues found.");
    }
    let mut out = format!("KMS '{name}': {total} issue(s)\n");
    if !report.broken_links.is_empty() {
        out.push_str(&format!(
            "\nbroken links ({}):\n",
            report.broken_links.len()
        ));
        for (page, target) in &report.broken_links {
            out.push_str(&format!("  - {page} → pages/{target}.md (missing)\n"));
        }
    }
    if !report.index_orphans.is_empty() {
        out.push_str(&format!(
            "\nindex entries with no underlying file ({}):\n",
            report.index_orphans.len()
        ));
        for stem in &report.index_orphans {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.missing_in_index.is_empty() {
        out.push_str(&format!(
            "\npages missing from index ({}):\n",
            report.missing_in_index.len()
        ));
        for stem in &report.missing_in_index {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.orphan_pages.is_empty() {
        out.push_str(&format!(
            "\norphan pages (no inbound links from other pages, {}):\n",
            report.orphan_pages.len()
        ));
        for stem in &report.orphan_pages {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.missing_frontmatter.is_empty() {
        out.push_str(&format!(
            "\npages without YAML frontmatter ({}):\n",
            report.missing_frontmatter.len()
        ));
        for stem in &report.missing_frontmatter {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.missing_required_fields.is_empty() {
        out.push_str(&format!(
            "\nmissing required frontmatter fields ({}):\n",
            report.missing_required_fields.len()
        ));
        for (page, source_key, field) in &report.missing_required_fields {
            out.push_str(&format!(
                "  - {page}: '{field}' (required by {source_key})\n"
            ));
        }
    }
    if !report.dangling_source_refs.is_empty() {
        out.push_str(&format!(
            "\npages citing a source that isn't archived ({}):\n",
            report.dangling_source_refs.len()
        ));
        for (page, src) in &report.dangling_source_refs {
            out.push_str(&format!("  - {page} → sources/{src} (missing)\n"));
        }
    }
    if !report.orphan_sources.is_empty() {
        out.push_str(&format!(
            "\narchived sources no page cites ({}):\n",
            report.orphan_sources.len()
        ));
        for file in &report.orphan_sources {
            out.push_str(&format!("  - sources/{file}\n"));
        }
    }
    if !report.abandoned_research.is_empty() {
        out.push_str(&format!(
            "\n{} page(s) still `status: researching` — a research run that never finished. \
             Re-run `/research` on the topic, or delete the placeholder:\n",
            report.abandoned_research.len()
        ));
        for (stem, updated) in &report.abandoned_research {
            if updated.is_empty() {
                out.push_str(&format!("  - {stem}\n"));
            } else {
                out.push_str(&format!("  - {stem} (since {updated})\n"));
            }
        }
    }
    // Not counted in total_issues — a backlog, not a defect.
    if !report.derived_pages.is_empty() {
        out.push_str(&format!(
            "\nnote: {} page(s) still `status: derived` (ingested, not yet curated):\n",
            report.derived_pages.len()
        ));
        for stem in &report.derived_pages {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    out
}

/// Session-end review: lint output plus any STALE markers left behind
/// by re-ingest cascades. Both are pure-read; the user (or agent) acts
/// on them via KmsWrite. The "next step" hints surface what's most
/// actionable.
pub fn format_wrap_up_report(name: &str, lint: &LintReport, stale: &[StaleEntry]) -> String {
    let lint_total = lint.total_issues();
    let stale_count = stale.len();
    if lint_total == 0 && stale_count == 0 {
        return format!("KMS '{name}': clean — nothing to wrap up.");
    }
    let mut out = format!(
        "KMS '{name}': wrap-up — {lint_total} lint issue(s), {stale_count} stale marker(s)\n"
    );
    if lint_total > 0 {
        // Reuse the lint formatter so both surfaces stay consistent.
        let lint_body = format_lint_report(name, lint);
        // Drop the lint formatter's own header line; we already wrote one.
        if let Some((_, rest)) = lint_body.split_once('\n') {
            out.push_str(rest);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    if stale_count > 0 {
        out.push_str(&format!(
            "\nstale pages awaiting refresh ({stale_count}):\n"
        ));
        for entry in stale {
            out.push_str(&format!(
                "  - {}: source `{}` re-ingested on {} (page not yet refreshed)\n",
                entry.page_stem, entry.source_alias, entry.date
            ));
        }
    }
    out.push_str("\nnext steps: ask the agent to refresh stale pages and fix lint issues, or run `/kms lint <name>` again after edits.\n");
    out
}

/// Render a `MigrationReport` from `kms::migrate`. Three shapes —
/// empty steps (already at latest), dry-run preview, applied summary.
pub fn format_migration_report(name: &str, report: &MigrationReport) -> String {
    let mode = if report.dry_run { "plan" } else { "applied" };
    if report.steps.is_empty() {
        return format!(
            "KMS '{name}': already at schema version {} — nothing to migrate.",
            report.target_version
        );
    }
    let mut out = format!(
        "KMS '{name}': migration {mode} ({} → {}, {} step(s))\n",
        report.current_version,
        report.target_version,
        report.steps.len()
    );
    for step in &report.steps {
        out.push_str(&format!("\n{} → {}:\n", step.from, step.to));
        for action in &step.actions {
            out.push_str(&format!("  - {action}\n"));
        }
    }
    if report.dry_run {
        out.push_str("\nthis was a dry-run preview. re-run with `--apply` to execute.\n");
    } else {
        out.push_str("\nlogged to log.md. /kms lint to verify.\n");
    }
    out
}

/// Build the `kms_update` envelope the frontend's KMS sidebar
/// consumes. M6.36 SERVE9c — moved from `gui.rs` to an always-on
/// module so the WS transport's `kms_list` IPC arm can call it from
/// `crate::ipc::handle_ipc`. Same JSON shape both transports emit.
pub fn build_update_payload() -> serde_json::Value {
    let active: std::collections::HashSet<String> = crate::config::ProjectConfig::load()
        .and_then(|c| c.kms.map(|k| k.active))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let kmss: Vec<serde_json::Value> = list_all()
        .into_iter()
        .map(|k| {
            serde_json::json!({
                "name": k.name,
                "scope": k.scope.as_str(),
                "active": active.contains(&k.name),
            })
        })
        .collect();
    serde_json::json!({
        "type": "kms_update",
        "kmss": kmss,
    })
}

/// Test-only lock shared by every test in this module *and* in
/// `tools::kms` that mutates the process env (HOME, cwd). Without
/// this, parallel tests race on env — which can also break unrelated
/// tests (bash/grep) whose sandbox resolver reads cwd.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod name_traversal_tests {
    use super::*;

    #[test]
    fn resolve_refuses_names_that_leave_the_kms_root() {
        // Re-audit N11: KmsWrite passed the model's `kms` straight in.
        for bad in ["../../skills", "..", "a/b", "/etc", ".thclaws", "x\\..\\y"] {
            assert!(resolve_exact(bad).is_none(), "{bad}");
            assert!(resolve(bad).is_none(), "{bad}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── sources as a first-class layer ───────────────────────────────

    #[test]
    fn an_uncurated_stub_is_not_kept_in_the_trash() {
        let _home = scoped_home();
        let kref = create("stub-trash-rt", KmsScope::Project).unwrap();
        let before = crate::kms_trash::list_rows(&kref).len();

        // What an ingest writes: a page nobody has curated.
        write_page(&kref, "paper", "---\nstatus: derived\n---\n\nstub body\n").unwrap();
        // What the research pipeline writes over it moments later.
        write_page(
            &kref,
            "paper",
            "---\ntitle: \"Paper\"\n---\n\nthe real page\n",
        )
        .unwrap();
        assert_eq!(
            crate::kms_trash::list_rows(&kref).len(),
            before,
            "a stub the user never saw must not become a row they have to read"
        );

        // A page someone has written is kept, as before — an edit drops
        // the `derived` marker, which is exactly the signal used here.
        write_page(
            &kref,
            "paper",
            "---\ntitle: \"Paper\"\n---\n\nedited again\n",
        )
        .unwrap();
        assert_eq!(
            crate::kms_trash::list_rows(&kref).len(),
            before + 1,
            "overwriting a real page still keeps the version it replaced"
        );
    }

    #[test]
    fn one_sources_parser_reads_all_three_vocabularies() {
        // `/research` writes indices, `/dream` writes session ids, an
        // ingest writes a bare alias. All three arrive at the same
        // readers and all three have to survive the same parse.
        assert_eq!(sources_entries("[3, 7]"), vec!["3", "7"]);
        assert_eq!(
            sources_entries(r#"["sess-abc", "sess-def"]"#),
            vec!["sess-abc", "sess-def"]
        );
        assert_eq!(sources_entries("my-paper"), vec!["my-paper"]);

        // The regression this consolidation closes: `kms_sources`
        // stripped quotes but not brackets, so the FIRST entry of a flow
        // list reached it as `["sess-abc` and matched nothing.
        assert_eq!(
            sources_entries(r#"["sess-abc"]"#).first().copied(),
            Some("sess-abc"),
            "the opening bracket must not ride along on the first entry"
        );

        // Empty and absent both mean "nothing declared", not one entry
        // of empty string.
        assert!(sources_entries("[]").is_empty());
        assert!(sources_entries("").is_empty());
        assert!(sources_entries("   ").is_empty());

        // Provenance that names nothing checkable is skipped; an alias
        // and an index are both checkable, against the `sources/` folder
        // and the citation registry respectively, so neither is
        // "external" — folding those two together made `/kms lint` stop
        // reporting an index the registry had never heard of.
        assert!(source_entry_is_external_provenance("sess-abc"));
        assert!(source_entry_is_external_provenance("session-abc"));
        assert!(source_entry_is_external_provenance("memory"));
        assert!(source_entry_is_external_provenance("https://example.com/x"));
        assert!(!source_entry_is_external_provenance("my-paper"));
        assert!(
            !source_entry_is_external_provenance("3"),
            "a citation index is checkable against the registry"
        );
    }

    #[test]
    fn source_path_resolves_every_supported_extension() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::create_dir_all(k.sources_dir()).unwrap();
        for (name, body) in [
            ("notes.txt", "plain"),
            ("data.json", "{}"),
            ("run.log", "line"),
            ("page.html", "<p>x</p>"),
        ] {
            std::fs::write(k.sources_dir().join(name), body).unwrap();
        }
        // Bare stem resolves…
        assert!(source_path(&k, "notes").unwrap().ends_with("notes.txt"));
        assert!(source_path(&k, "data").unwrap().ends_with("data.json"));
        assert!(source_path(&k, "run").unwrap().ends_with("run.log"));
        // …and so does the full filename.
        assert!(source_path(&k, "page.html").unwrap().ends_with("page.html"));
        assert!(source_path(&k, "missing").is_err());
    }

    #[test]
    fn source_path_rejects_traversal() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        for bad in ["../secret", "a/b", "..", "\0x"] {
            assert!(source_path(&k, bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn browse_lists_non_markdown_sources() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::create_dir_all(k.sources_dir()).unwrap();
        std::fs::write(k.sources_dir().join("spec.txt"), "raw text").unwrap();
        std::fs::write(k.sources_dir().join("cfg.json"), "{}").unwrap();

        let listing = browse("nb").expect("kms browses");
        let names: Vec<(String, String)> = listing
            .sources
            .iter()
            .map(|f| (f.name.clone(), f.ext.clone()))
            .collect();
        assert!(names.contains(&("spec".into(), "txt".into())), "{names:?}");
        assert!(names.contains(&("cfg".into(), "json".into())), "{names:?}");

        // And the viewer can actually open them — the old code
        // hard-coded `.md` and reported "not found".
        let read = read_browse_file("nb", "source", "spec").unwrap();
        assert_eq!(read.content, "raw text");
    }

    #[test]
    fn ingest_cross_extension_alias_collision_is_refused() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("notes.md");
        let b = dir.path().join("notes.txt");
        std::fs::write(&a, "# A\n\nfirst").unwrap();
        std::fs::write(&b, "second").unwrap();

        ingest(&k, &a, None, false).unwrap();
        let err = ingest(&k, &b, None, false).unwrap_err().to_string();
        assert!(err.contains("already exists"), "got: {err}");
        assert!(err.contains("notes.md"), "collision not named: {err}");

        // --force replaces rather than leaving two archives claiming
        // the same alias.
        ingest(&k, &b, None, true).unwrap();
        let files: Vec<String> = list_sources(&k)
            .into_iter()
            .map(|s| s.file_name())
            .collect();
        assert_eq!(files, vec!["notes.txt".to_string()], "{files:?}");
    }

    #[test]
    fn ingest_records_provenance_in_the_catalog() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("spec.md");
        std::fs::write(&src, "# The Spec\n\nBody text here.\n").unwrap();

        ingest(&k, &src, None, false).unwrap();
        let cat = crate::kms_sources::load(&k);
        let rec = cat.entries.get("spec.md").expect("catalog entry written");
        assert_eq!(rec.origin, crate::kms_sources::Origin::File);
        assert!(rec.origin_ref.ends_with("spec.md"), "{}", rec.origin_ref);
        assert_eq!(rec.title, "The Spec");
        assert!(!rec.sha256.is_empty());
        assert!(rec.bytes > 0);
    }

    #[test]
    fn ingest_detects_byte_identical_duplicate() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("one.md");
        let b = dir.path().join("two.md");
        let content = "# Same\n\nIdentical bytes.\n";
        std::fs::write(&a, content).unwrap();
        std::fs::write(&b, content).unwrap();

        assert!(ingest(&k, &a, None, false).unwrap().duplicate_of.is_none());
        let second = ingest(&k, &b, None, false).unwrap();
        assert_eq!(second.duplicate_of.as_deref(), Some("one.md"));
    }

    #[test]
    fn ingest_derives_outline_from_source_headings() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("guide.md");
        std::fs::write(
            &src,
            "# Guide\n\nOpening paragraph.\n\n## Setup\n\ntext\n\n## Usage\n\n\
             ```\n# not a heading\n```\n\n### Flags\n\nmore\n",
        )
        .unwrap();

        let r = ingest(&k, &src, None, false).unwrap();
        let body = std::fs::read_to_string(&r.target).unwrap();
        assert!(body.contains("## Outline of the source"), "{body}");
        assert!(body.contains("- Setup"), "{body}");
        assert!(body.contains("- Usage"), "{body}");
        assert!(body.contains("  - Flags"), "nesting lost:\n{body}");
        assert!(
            !body.contains("not a heading"),
            "fenced code treated as heading:\n{body}"
        );
        // The index summary is the lead, not a restatement of the name.
        assert_eq!(r.summary, "Opening paragraph.");
    }

    #[test]
    fn ingest_html_file_archives_markdown_not_tag_soup() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("doc.html");
        std::fs::write(
            &src,
            "<html><head><title>The Doc</title><style>p{color:red}</style></head>\
             <body><nav><a href='/x'>Nav</a></nav><h1>The Doc</h1>\
             <p>Real prose here.</p><ul><li>one</li></ul></body></html>",
        )
        .unwrap();

        let r = ingest(&k, &src, None, false).unwrap();
        // Archived as markdown, not `.html`.
        let archive = k.sources_dir().join("doc.md");
        assert!(archive.exists(), "archive not converted to markdown");
        assert!(!k.sources_dir().join("doc.html").exists());
        let raw = std::fs::read_to_string(&archive).unwrap();
        assert!(raw.contains("# The Doc"), "{raw}");
        assert!(raw.contains("Real prose here."), "{raw}");
        assert!(raw.contains("- one"), "{raw}");
        assert!(!raw.contains("color:red"), "style leaked: {raw}");
        assert!(!raw.contains("<body"), "tag soup survived: {raw}");

        // Conversion is recorded, so the archive isn't mistaken for the
        // original bytes.
        let rec = crate::kms_sources::load(&k)
            .entries
            .remove("doc.md")
            .unwrap();
        assert_eq!(rec.converted_from.as_deref(), Some("text/html"));
        assert_eq!(rec.title, "The Doc");
        let page = std::fs::read_to_string(&r.target).unwrap();
        assert!(page.contains("Converted from `text/html`"), "{page}");
    }

    #[test]
    fn ingest_json_source_outlines_top_level_keys() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cfg.json");
        std::fs::write(&src, r#"{"name":"x","items":[1,2,3],"nested":{"a":1}}"#).unwrap();

        let r = ingest(&k, &src, None, false).unwrap();
        let body = std::fs::read_to_string(&r.target).unwrap();
        assert!(body.contains("`name` — string"), "{body}");
        assert!(body.contains("`items` — array (3 items)"), "{body}");
        assert!(body.contains("`nested` — object (1 keys)"), "{body}");
    }

    // ─── index ────────────────────────────────────────────────────────

    /// The page-name list is bounded by bytes and says what it left out.
    /// A count cap is no bound for Thai titles at three bytes a character,
    /// and a silent cut reads to the model as "those pages do not exist".
    #[test]
    fn the_prompt_page_list_is_byte_bounded_and_admits_what_it_omits() {
        let _home = scoped_home();
        let k = create("big", KmsScope::User).unwrap();
        let _batch = IndexBatch::new(&k);
        for i in 0..200 {
            write_page(
                &k,
                &format!("page-{i:03}"),
                &format!(
                    "---\ntitle: {}\n---\n\nbody\n",
                    "มาตรฐานการครองชีพ".repeat(3)
                ),
            )
            .unwrap();
        }
        let header = index_header(&k);
        assert!(
            header.len() < HEADER_PAGE_LIST_BYTES + 1_000,
            "header is {} bytes",
            header.len()
        );
        assert!(header.contains("200 page(s)"), "{header}");
        assert!(header.contains("more — not listed"), "{header}");
    }

    /// A KMS is found by slug as well as by its exact name — and always
    /// answers with its real name, so nothing records a second spelling.
    #[test]
    fn a_kms_is_found_by_slug_and_answers_with_its_real_name() {
        let _home = scoped_home();
        create("Age of Abundance", KmsScope::Project).unwrap();
        create("คลังความรู้ ไทย", KmsScope::Project).unwrap();
        for typed in [
            "Age of Abundance",
            "age-of-abundance",
            "age_of_abundance",
            "AgeOfAbundance",
            "AGE OF ABUNDANCE",
        ] {
            let k = resolve(typed).unwrap_or_else(|| panic!("`{typed}` found nothing"));
            assert_eq!(k.name, "Age of Abundance", "from `{typed}`");
            assert!(k.root.ends_with("Age of Abundance"), "{:?}", k.root);
        }
        assert_eq!(resolve("คลังความรู้-ไทย").unwrap().name, "คลังความรู้ ไทย");
        // Tone marks still matter: a different word is a different base.
        assert!(resolve("คลังความรู-ไทย").is_none());

        assert!(resolve("age-of").is_none(), "a prefix is not a match");
        assert!(resolve("---").is_none());
        // The exact lookup stays exact.
        assert!(resolve_exact("age-of-abundance").is_none());
    }

    /// Two bases that read the same once folded: the exact name still
    /// finds each, and the slug finds neither rather than picking one to
    /// write into.
    #[test]
    fn an_ambiguous_slug_finds_nothing() {
        let _home = scoped_home();
        create("My Notes", KmsScope::Project).unwrap();
        create("my-notes", KmsScope::Project).unwrap();
        assert_eq!(resolve("My Notes").unwrap().name, "My Notes");
        assert_eq!(resolve("my-notes").unwrap().name, "my-notes");
        assert!(resolve("my_notes").is_none(), "ambiguous must not guess");
    }

    /// Two different names must never fold to one key. The old filter
    /// kept Thai vowels and dropped Thai tone marks, so it turned words
    /// into other words — and the research planner merges notes on this.
    #[test]
    fn folding_a_name_never_makes_two_thai_words_equal() {
        for (a, b) in [("ก้าว", "กาว"), ("หน้า", "หนา"), ("เสื้อ", "เสือ")]
        {
            assert_ne!(fold_for_compare(a), fold_for_compare(b), "{a} vs {b}");
        }
        // What it is for still works.
        assert_eq!(
            fold_for_compare("Deep-Seek  V4!"),
            fold_for_compare("deepseek v4")
        );
        assert_eq!(
            fold_for_compare("มาตรฐาน การครองชีพ"),
            fold_for_compare("มาตรฐาน\u{200B}การครองชีพ")
        );
    }

    #[test]
    fn wikilink_syntax_is_stripped_for_prose() {
        assert_eq!(
            strip_wikilink_syntax("ตอบโดย [[herbert-simon|Herbert Simon]] ในปี 1971"),
            "ตอบโดย Herbert Simon ในปี 1971"
        );
        assert_eq!(
            strip_wikilink_syntax("see [[jevons-paradox]]."),
            "see jevons-paradox."
        );
        // A clipped summary can end inside a link.
        assert_eq!(
            strip_wikilink_syntax("see [[herbert-simon|Herbert Si…"),
            "see Herbert Si…"
        );
        assert_eq!(strip_wikilink_syntax("plain"), "plain");
    }

    /// An index bullet has to stop somewhere a reader would. It used to
    /// stop at exactly 120 characters, which in Thai lands mid-syllable.
    #[test]
    fn an_index_summary_stops_at_a_boundary() {
        let short = "A small herding breed.";
        assert_eq!(clip(short, 120), short, "under the cap, untouched");

        // English: the sentence end inside the budget wins.
        let two = "First sentence here. And then a second one that runs past the budget entirely.";
        let cut = clip(two, 40);
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.starts_with("First sentence here."), "{cut}");

        // Thai: no sentence-ending punctuation at all, so it falls back
        // to a space — which in Thai separates phrases, not words.
        let thai = "มาตรฐานการครองชีพ คือระดับความเป็นอยู่ที่วัดจากสินค้าและบริการ ที่ครัวเรือนเข้าถึงได้จริง";
        let cut = clip(thai, 40);
        assert!(cut.ends_with('…'), "{cut}");
        assert!(
            !cut.trim_end_matches('…').ends_with(' '),
            "trailing space should be trimmed: {cut}"
        );
        assert!(
            thai.starts_with(cut.trim_end_matches('…')),
            "the kept part must be a real prefix, not a broken one: {cut}"
        );

        // A boundary in the first 60% is ignored rather than costing
        // most of the summary.
        let early =
            "Dr. Somchai went on to describe the entire programme in considerable detail here";
        let cut = clip(early, 60);
        assert!(
            cut.chars().count() > 40,
            "early period cost too much: {cut}"
        );
    }

    #[test]
    fn index_summary_is_not_the_page_title_restated() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Exactly what KmsWrite produces: injected `# title` heading,
        // then the body.
        write_page(
            &k,
            "welsh-corgi",
            "---\ntitle: Welsh Corgi\ncategory: dogs\n---\n\n\
             A small herding breed from Pembrokeshire.\n",
        )
        .unwrap();
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(
            index.contains("A small herding breed from Pembrokeshire."),
            "summary is not the lead:\n{index}"
        );
        assert!(
            !index.contains("— Welsh Corgi\n"),
            "summary restates the title:\n{index}"
        );
    }

    #[test]
    fn on_disk_index_matches_the_prompt_index() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        write_page(&k, "alpha", "---\ncategory: one\n---\n\nAlpha body.\n").unwrap();
        write_page(&k, "beta", "---\ncategory: two\n---\n\nBeta body.\n").unwrap();

        let on_disk = std::fs::read_to_string(k.index_path()).unwrap();
        // Both readers describe the same pages with the same summaries;
        // pre-fix index.md was an append-ordered bullet list the prompt
        // never looked at.
        for needle in ["**one**", "**two**", "Alpha body.", "Beta body."] {
            assert!(
                on_disk.contains(needle),
                "index.md missing {needle}:\n{on_disk}"
            );
        }
        let on_demand = full_index(&k);
        for needle in ["**one**", "**two**", "Alpha body.", "Beta body."] {
            assert!(
                on_demand.contains(needle),
                "KmsRead(kind: \"index\") missing {needle}:\n{on_demand}"
            );
        }
    }

    #[test]
    fn index_marks_uncurated_derived_pages() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("raw.md");
        std::fs::write(&src, "# Raw\n\nSome ingested prose.\n").unwrap();
        ingest(&k, &src, None, false).unwrap();

        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index.contains("(derived — uncurated)"), "{index}");
        assert!(index.contains("## Sources"), "no source block:\n{index}");
        assert!(index.contains("raw.md"), "{index}");

        // Curating the page drops the marker.
        write_page(&k, "raw", "---\ncategory: notes\n---\n\nCurated now.\n").unwrap();
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(!index.contains("(derived — uncurated)"), "{index}");
    }

    #[test]
    fn rebuild_index_drops_entries_for_deleted_pages() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        write_page(&k, "keep", "---\ncategory: c\n---\n\nKeep me.\n").unwrap();
        write_page(&k, "drop", "---\ncategory: c\n---\n\nDrop me.\n").unwrap();
        std::fs::remove_file(k.pages_dir().join("drop.md")).unwrap();
        rebuild_index(&k).unwrap();
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index.contains("keep"), "{index}");
        assert!(!index.contains("(pages/drop.md)"), "{index}");
    }

    // ─── lint ─────────────────────────────────────────────────────────

    #[test]
    fn lint_counts_wikilinks_as_inbound() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("hub.md"),
            "---\ncategory: c\n---\n\nSee [[spoke]] for detail.\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("spoke.md"),
            "---\ncategory: c\n---\n\nDetail.\n",
        )
        .unwrap();

        let report = lint(&k).unwrap();
        // `spoke` is wikilinked, so it is NOT an orphan. Pre-fix lint
        // only saw `(pages/x.md)` links, so `auto_link`'s own output
        // never counted and every page stayed "orphan" forever.
        assert!(
            !report.orphan_pages.contains(&"spoke".to_string()),
            "wikilink ignored: {:?}",
            report.orphan_pages
        );
        assert!(report.orphan_pages.contains(&"hub".to_string()));
    }

    #[test]
    fn lint_findings_name_what_to_open_worst_first() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("hub.md"),
            "---\ncategory: c\n---\n\nSee [[nowhere]].\n",
        )
        .unwrap();
        let sources = k.root.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(sources.join("unused.md"), "nobody cites me").unwrap();

        let out = lint_findings(&lint(&k).unwrap());
        assert!(!out.is_empty());
        // A broken link is a thing that is broken; an uncited archive is
        // only unreferenced. Broken comes first.
        let pos = |kind: &str| out.iter().position(|f| f.kind == kind);
        let broken = pos("broken_link").expect("broken link missing");
        let orphan_src = pos("orphan_source").expect("orphan source missing");
        assert!(broken < orphan_src, "{out:?}");

        let f = &out[broken];
        // The row opens the page that has the bad link, not the target,
        // which by definition is not there to open.
        assert_eq!(f.target, "hub");
        assert_eq!(f.target_kind, "page");
        assert!(f.detail.contains("nowhere"), "{}", f.detail);
        assert_eq!(out[orphan_src].target_kind, "source");
        assert_eq!(out[orphan_src].target, "unused");
        // Every finding either names something to open or names nothing
        // at all — never a target with no kind to open it as.
        for f in &out {
            assert_eq!(
                f.target.is_empty(),
                f.target_kind.is_empty(),
                "half-specified target: {f:?}"
            );
        }
    }

    #[test]
    fn lint_reports_broken_wikilinks() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("hub.md"),
            "---\ncategory: c\n---\n\nSee [[nowhere]].\n",
        )
        .unwrap();
        let report = lint(&k).unwrap();
        assert!(
            report
                .broken_links
                .contains(&("hub".to_string(), "nowhere".to_string())),
            "{:?}",
            report.broken_links
        );
    }

    #[test]
    fn lint_flags_orphan_and_dangling_sources() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::create_dir_all(k.sources_dir()).unwrap();
        std::fs::write(k.sources_dir().join("unused.md"), "# Unused").unwrap();
        std::fs::write(k.sources_dir().join("used.md"), "# Used").unwrap();
        std::fs::write(
            k.pages_dir().join("p.md"),
            "---\ncategory: c\nsources: used, ghost\n---\n\nBody.\n",
        )
        .unwrap();

        let report = lint(&k).unwrap();
        assert_eq!(report.orphan_sources, vec!["unused.md".to_string()]);
        assert_eq!(
            report.dangling_source_refs,
            vec![("p".to_string(), "ghost".to_string())]
        );
    }

    /// What `/research` actually writes: `sources: [1, 2]`, indices into
    /// the citation registry. Lint read them as filenames and reported
    /// every researched page as citing missing archives — 99 findings on
    /// a clean 39-page vault, all false.
    #[test]
    fn lint_reads_research_citation_indices() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut reg = crate::research::registry::SourceRegistry::load(&k);
        let one = reg.index_for("https://example.com/a", "A");
        let two = reg.index_for("https://example.com/b", "B");
        reg.save(&k).unwrap();
        std::fs::create_dir_all(k.sources_dir()).unwrap();
        // Only the first was archived; the second is cited but was never
        // written to disk, which is not a lint matter.
        std::fs::write(k.sources_dir().join("example-com-a.md"), "# A").unwrap();
        std::fs::write(
            k.pages_dir().join("p.md"),
            format!("---\ntype: note\nsources: [{one}, {two}]\n---\n\nBody.\n"),
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("q.md"),
            "---\ntype: note\nsources: [99]\n---\n\nBody.\n",
        )
        .unwrap();

        let report = lint(&k).unwrap();
        assert_eq!(
            report.dangling_source_refs,
            vec![(
                "q".to_string(),
                "[99] (not in the citation registry)".to_string()
            )],
            "known indices are fine; only an unknown one is a finding"
        );
        assert!(
            report.orphan_sources.is_empty(),
            "an archive cited by index is not an orphan: {:?}",
            report.orphan_sources
        );
    }

    /// A placeholder left by a research run that died is a finding. It
    /// used to pass every check: valid frontmatter, an inbound link, a
    /// fresh date, no numbers to be uncited.
    #[test]
    fn lint_flags_a_research_placeholder_nobody_finished() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("herbert-simon.md"),
            "---\ntitle: Herbert Simon\ntype: note\nstatus: researching\nupdated: 2026-09-18\n---\n\n\
             Researching \"Herbert Simon\" — this page is being written by `/research`.\n",
        )
        .unwrap();
        let report = lint(&k).unwrap();
        assert_eq!(
            report.abandoned_research,
            vec![("herbert-simon".to_string(), "2026-09-18".to_string())]
        );
        assert!(report.total_issues() >= 1);
        assert!(format_lint_report("nb", &report).contains("never finished"));
    }

    #[test]
    fn lint_accepts_non_file_provenance_values() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("p.md"),
            "---\ncategory: c\nsources: session-abc123 memory https://x.test/a\n---\n\nBody.\n",
        )
        .unwrap();
        let report = lint(&k).unwrap();
        assert!(
            report.dangling_source_refs.is_empty(),
            "{:?}",
            report.dangling_source_refs
        );
    }

    // ─── directory ingest ─────────────────────────────────────────────

    #[test]
    fn ingest_dir_walks_and_disambiguates_by_path() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("api")).unwrap();
        std::fs::create_dir_all(dir.path().join("web")).unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("api/auth.md"), "# API auth\n\napi body").unwrap();
        std::fs::write(dir.path().join("web/auth.md"), "# Web auth\n\nweb body").unwrap();
        std::fs::write(dir.path().join("top.txt"), "top body").unwrap();
        std::fs::write(dir.path().join("skip.bin"), "binary").unwrap();
        std::fs::write(dir.path().join(".hidden/x.md"), "hidden").unwrap();

        let r = ingest_dir(&k, dir.path(), false).unwrap();
        let mut got = r.ingested.clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                "api-auth".to_string(),
                "top".to_string(),
                "web-auth".to_string()
            ],
            "unexpected set: {got:?}"
        );
        // Same-named files in sibling dirs did not collide.
        assert!(k.pages_dir().join("api-auth.md").exists());
        assert!(k.pages_dir().join("web-auth.md").exists());

        // The batch guard defers per-file index rebuilds; the one at
        // the end must still cover everything.
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        for stem in ["api-auth", "top", "web-auth"] {
            assert!(index.contains(&format!("(pages/{stem}.md)")), "{index}");
        }
        assert!(index.contains("## Sources"), "{index}");
    }

    #[test]
    fn index_batch_defers_then_rebuilds_once() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        {
            let _batch = IndexBatch::new(&k);
            write_page(&k, "a", "---\ncategory: c\n---\n\nA body.\n").unwrap();
            write_page(&k, "b", "---\ncategory: c\n---\n\nB body.\n").unwrap();
            // Deferred: index.md has not seen either page yet.
            let mid = std::fs::read_to_string(k.index_path()).unwrap();
            assert!(!mid.contains("(pages/a.md)"), "not deferred:\n{mid}");
        }
        // Rebuilt on drop.
        let after = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(after.contains("A body."), "{after}");
        assert!(after.contains("B body."), "{after}");
    }

    #[test]
    fn ingest_dir_rejects_a_file() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.md");
        std::fs::write(&f, "x").unwrap();
        assert!(ingest_dir(&k, &f, false).is_err());
    }

    // ─── reindex ──────────────────────────────────────────────────────

    #[test]
    fn reindex_repairs_hand_edited_state() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        write_page(&k, "a", "---\ncategory: c\n---\n\nA body.\n").unwrap();
        // Simulate everything that bypasses the write hooks: a page
        // dropped in by hand, a source dropped in by hand, and a
        // corrupted index.
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: c\n---\n\nB body.\n",
        )
        .unwrap();
        std::fs::create_dir_all(k.sources_dir()).unwrap();
        std::fs::write(k.sources_dir().join("dropped-in.txt"), "raw").unwrap();
        std::fs::write(k.index_path(), "garbage\n").unwrap();

        let r = reindex(&k).unwrap();
        assert_eq!(r.pages, 2);
        assert_eq!(r.sources, 1);
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index.contains("A body."), "{index}");
        assert!(index.contains("B body."), "{index}");
        assert!(index.contains("dropped-in.txt"), "{index}");
        assert!(!index.contains("garbage"), "{index}");
        // The catalogue backfilled the hand-dropped source.
        assert!(crate::kms_sources::load(&k)
            .entries
            .contains_key("dropped-in.txt"));
    }

    #[test]
    fn sanitize_alias_keeps_thai_and_other_unicode() {
        // The reported bug: an all-Thai name used to fold to empty.
        let thai = "ข้อบังคับเกี่ยวกับการทำงาน";
        assert_eq!(sanitize_alias(thai), thai);
        // Combining tone marks/vowels are preserved, not stripped.
        assert_eq!(sanitize_alias("ภาษาไทย"), "ภาษาไทย");
        assert_eq!(sanitize_alias("日本語"), "日本語");
    }

    #[test]
    fn sanitize_alias_folds_unsafe_ascii_and_whitespace() {
        assert_eq!(sanitize_alias("hello world"), "hello_world");
        assert_eq!(sanitize_alias("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_alias("notes.md"), "notes_md");
        assert_eq!(sanitize_alias("__trim__"), "trim");
        // Thai with trailing spaces still trims and survives.
        assert_eq!(sanitize_alias("  รายงาน  "), "รายงาน");
    }

    #[test]
    fn sanitize_alias_empty_only_for_no_word_chars() {
        assert_eq!(sanitize_alias("   "), "");
        assert_eq!(sanitize_alias("///"), "");
    }

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<String>,
        prev_userprofile: Option<String>,
        prev_cwd: std::path::PathBuf,
        _home_dir: tempfile::TempDir,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore cwd first — set_current_dir against a dropped
            // tempdir would fail silently otherwise.
            let _ = std::env::set_current_dir(&self.prev_cwd);
            match &self.prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_userprofile {
                Some(h) => std::env::set_var("USERPROFILE", h),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    /// Acquire exclusive access to the process env + cwd for this
    /// test, set HOME (+ USERPROFILE on Windows) to a fresh tempdir,
    /// leave cwd pointing at that tempdir. Dropped at end of test to
    /// restore.
    fn scoped_home() -> EnvGuard {
        let lock = test_env_lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_userprofile = std::env::var("USERPROFILE").ok();
        let prev_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("USERPROFILE", dir.path());
        std::env::set_current_dir(dir.path()).unwrap();
        EnvGuard {
            _lock: lock,
            prev_home,
            prev_userprofile,
            prev_cwd,
            _home_dir: dir,
        }
    }

    /// A workspace with agents, as the host lays it out. Returns the
    /// workspace root; cwd and `THCLAWS_WORKSPACE_ROOT` are what an agent
    /// process spawned by the host would see.
    struct HostGuard {
        prev_ws: Option<String>,
        _env: EnvGuard,
    }

    impl Drop for HostGuard {
        fn drop(&mut self) {
            match &self.prev_ws {
                Some(v) => std::env::set_var("THCLAWS_WORKSPACE_ROOT", v),
                None => std::env::remove_var("THCLAWS_WORKSPACE_ROOT"),
            }
        }
    }

    fn legacy_vault(ws: &Path, bot: &str, name: &str, page: &str) {
        let pages = ws
            .join(".thclaws/bots")
            .join(bot)
            .join(PROJECT_KMS_DIR)
            .join(name)
            .join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(
            pages.join(format!("{page}.md")),
            format!("# {page} of {bot}\n"),
        )
        .unwrap();
    }

    fn as_agent(bot: &str) -> (HostGuard, PathBuf) {
        let env = scoped_home();
        let ws = std::env::current_dir().unwrap().canonicalize().unwrap();
        let bot_dir = ws.join(".thclaws/bots").join(bot);
        std::fs::create_dir_all(&bot_dir).unwrap();
        let prev_ws = std::env::var("THCLAWS_WORKSPACE_ROOT").ok();
        std::env::set_var("THCLAWS_WORKSPACE_ROOT", &ws);
        std::env::set_current_dir(&bot_dir).unwrap();
        (HostGuard { prev_ws, _env: env }, ws)
    }

    /// dev-plan/64 P3.10. Six bugs in 48 hours were one bug: a function that
    /// takes a name assumed the name was ASCII. Every such function goes
    /// through one fixture here and must keep three promises — it returns
    /// something, it does not change its own output, and it does not make
    /// two different names the same.
    #[test]
    fn every_name_function_keeps_its_promises_on_every_script() {
        const NAMES: &[&str] = &[
            "ยุคที่ความฉลาดล้นเหลือ",
            "ก้าว",
            "กาว",
            "ก้าวหน้า",
            "กาวหนา",
            "มาตรฐานการครองชีพ",
            "知識管理",
            "知识管理",
            "ナレッジ",
            "إدارة المعرفة",
            "Ζήνων",
            "naïve café",
            "naive cafe",
            "Age of Abundance",
            "claude-code",
            "claude code hooks",
            "🧠 second brain",
            "v2.0 — final (draft)",
            "a/b\\c:d*e?f",
        ];
        type NameFn = (&'static str, fn(&str) -> String);
        let fns: &[NameFn] = &[
            ("kms::sanitize_alias", sanitize_alias),
            (
                "research::sanitize_slug",
                crate::research::digest::sanitize_slug,
            ),
        ];
        for (label, f) in fns {
            let mut seen: std::collections::HashMap<String, &str> = Default::default();
            for name in NAMES {
                let out = f(name);
                assert!(!out.is_empty(), "{label}({name:?}) is empty");
                assert_eq!(f(&out), out, "{label} is not idempotent on {name:?}");
                assert!(
                    !out.contains(['/', '\\', '\0']) && !out.contains(".."),
                    "{label}({name:?}) = {out:?} is not a safe file name"
                );
                assert!(
                    !out.chars().any(|c| c.is_control()) && !Path::new(&out).is_absolute(),
                    "{label}({name:?}) = {out:?} would be refused as a page name"
                );
                if let Some(other) = seen.insert(out.clone(), name) {
                    panic!("{label} makes {other:?} and {name:?} the same: {out:?}");
                }
            }
        }
        // Loose matching may merge spellings of ONE name; never two names.
        let mut folded: std::collections::HashMap<String, &str> = Default::default();
        for name in NAMES {
            let out = fold_for_compare(name);
            assert!(!out.is_empty(), "fold_for_compare({name:?}) is empty");
            assert_eq!(
                fold_for_compare(&out),
                out,
                "fold is not idempotent on {name:?}"
            );
            if let Some(other) = folded.insert(out.clone(), name) {
                let same_name = matches!(
                    (other, *name),
                    ("naïve café", "naive cafe") | ("naive cafe", "naïve café")
                );
                assert!(
                    same_name,
                    "fold makes {other:?} and {name:?} the same: {out:?}"
                );
            }
        }
        // Archive names: a Thai URL and its neighbour stay apart.
        let urls = [
            "https://th.wikipedia.org/wiki/ก้าว",
            "https://th.wikipedia.org/wiki/กาว",
            "https://example.com/a?x=1",
            "https://example.com/a?x=2",
            "https://example.com/a#frag",
        ];
        let names: Vec<String> = urls
            .iter()
            .map(|u| crate::research::kms_writer::url_to_filename(u))
            .collect();
        for (u, n) in urls.iter().zip(&names) {
            assert!(!n.is_empty() && !n.contains('/'), "{u} → {n:?}");
        }
        assert_ne!(names[0], names[1]);
        assert_ne!(names[2], names[3]);
    }

    /// dev-plan/64 P3.4. Every append, edit and stale mark parses a page's
    /// frontmatter and writes it back. What the parser does not understand
    /// it must carry, not drop: a block list (how Obsidian writes `tags:`)
    /// came back empty and was written back empty.
    #[test]
    fn frontmatter_survives_a_round_trip() {
        let src = "---\n# a comment\ntitle: \"He said \\\"hi\\\": ok\"\ntags:\n  - alpha\n  - \"b, c\"\naliases: [\"x\", y]\nsummary: >\n  folded line one\n  folded line two\nnested:\n  key: value\n  other: 2\nempty:\nurl: https://example.com/a#b\n---\nbody\n";
        let (fm, body) = parse_frontmatter(src);
        assert_eq!(fm.get("title").unwrap(), "He said \"hi\": ok");
        assert_eq!(fm.get("tags").unwrap(), "[alpha, \"b, c\"]");
        assert_eq!(fm.get("aliases").unwrap(), "[\"x\", y]");
        assert_eq!(
            fm.get("summary").unwrap(),
            ">\n  folded line one\n  folded line two"
        );
        assert_eq!(fm.get("nested").unwrap(), "\n  key: value\n  other: 2");
        assert!(
            !fm.contains_key("key"),
            "a nested key is not a top-level key"
        );
        assert_eq!(fm.get("empty").unwrap(), "");
        assert_eq!(fm.get("url").unwrap(), "https://example.com/a#b");
        assert_eq!(body, "body\n");

        let once = write_frontmatter(&fm, &body);
        assert!(
            once.contains("summary: >\n  folded line one\n  folded line two\n"),
            "{once}"
        );
        assert!(
            once.contains("nested:\n  key: value\n  other: 2\n"),
            "{once}"
        );
        assert!(once.contains("tags: [alpha, \"b, c\"]\n"), "{once}");
        let (fm2, body2) = parse_frontmatter(&once);
        assert_eq!(fm, fm2, "parse ∘ write is the identity");
        assert_eq!(write_frontmatter(&fm2, &body2), once, "and stays so");

        // The path that lost data: an append to a page Obsidian wrote.
        let _g = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("p.md"), src).unwrap();
        append_to_page(&k, "p", "more\n").unwrap();
        let after = std::fs::read_to_string(k.pages_dir().join("p.md")).unwrap();
        assert!(after.contains("alpha") && after.contains("b, c"), "{after}");
        assert!(
            after.contains("folded line two") && after.contains("other: 2"),
            "{after}"
        );
    }

    /// Not a test: a sweep. Every page of a real vault must parse to the
    /// same map after a write, and keep every top-level key it has.
    /// `KMS_BENCH_VAULT=<kms folder> cargo test --lib sweep_frontmatter -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sweep_frontmatter_on_a_real_vault() {
        let Ok(root) = std::env::var("KMS_BENCH_VAULT") else {
            return;
        };
        let (mut pages, mut bad) = (0, 0);
        for dir in ["pages", "sources"] {
            let Ok(rd) = std::fs::read_dir(PathBuf::from(&root).join(dir)) else {
                continue;
            };
            for e in rd.flatten() {
                let Ok(raw) = std::fs::read_to_string(e.path()) else {
                    continue;
                };
                let (fm, body) = parse_frontmatter(&raw);
                if fm.is_empty() {
                    continue;
                }
                pages += 1;
                let out = write_frontmatter(&fm, &body);
                let (fm2, body2) = parse_frontmatter(&out);
                let head = raw.split("\n---\n").next().unwrap_or("");
                let keys = head
                    .lines()
                    .skip(1)
                    .filter(|l| !l.starts_with([' ', '\t', '-', '#']) && l.contains(':'))
                    .count();
                if fm != fm2 || body != body2 || keys != fm.len() {
                    bad += 1;
                    eprintln!("UNSTABLE {:?}: keys {keys} vs {}", e.file_name(), fm.len());
                }
            }
        }
        eprintln!("swept {pages} files with frontmatter, {bad} unstable");
        assert_eq!(bad, 0);
    }

    /// Not a test: a stopwatch. `KMS_BENCH_VAULT=<path to a KMS folder>
    /// cargo test --lib bench_page_scans -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_page_scans_on_a_real_vault() {
        let Ok(root) = std::env::var("KMS_BENCH_VAULT") else {
            return;
        };
        let k = KmsRef {
            name: "bench".into(),
            scope: KmsScope::User,
            root: PathBuf::from(root),
        };
        let time = |label: &str| {
            let t = std::time::Instant::now();
            let n = backlink_map(&k).len() + scan_index_entries(&k).len();
            eprintln!("{label}: {:?} ({n} rows)", t.elapsed());
        };
        time("cold (read + parse every page)");
        time("warm (one stat per page)");
        time("warm again");
    }

    /// dev-plan/64 P5.7: the sidebar is given what a person calls each file.
    #[test]
    fn a_listing_carries_titles_and_unfinished_status() {
        let _g = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        write_page(
            &k,
            "hidden-poverty-households",
            "---\ntitle: ครัวเรือนยากจนแฝง\n---\nbody\n",
        )
        .unwrap();
        write_page(&k, "plain", "---\ntitle: plain\n---\nbody\n").unwrap();
        std::fs::write(
            k.pages_dir().join("stub.md"),
            "---\ntitle: Stub\nstatus: researching\n---\nplaceholder\n",
        )
        .unwrap();
        crate::research::kms_writer::write_source(
            "nb",
            "q",
            "2026-09-20",
            1,
            "OECD — Foundations for Growth",
            "https://www.oecd.org/x",
            "body",
        )
        .unwrap();

        let listing = browse("nb").unwrap();
        let page = |n: &str| listing.pages.iter().find(|p| p.name == n).unwrap().clone();
        assert_eq!(page("hidden-poverty-households").title, "ครัวเรือนยากจนแฝง");
        assert_eq!(
            page("plain").title,
            "",
            "a title equal to the slug adds nothing"
        );
        assert_eq!(page("stub").status, "researching");
        assert_eq!(page("plain").status, "");
        assert_eq!(listing.sources[0].title, "OECD — Foundations for Growth");

        let json = serde_json::to_value(&page("plain")).unwrap();
        assert!(
            json.get("title").is_none() && json.get("status").is_none(),
            "{json}"
        );
    }

    /// dev-plan/64 P3.3: the cache never serves a page that has changed —
    /// by our own writer, or by something else editing the file in place.
    #[test]
    fn the_page_cache_follows_every_kind_of_edit() {
        let _g = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        write_page(&k, "a", "---\ntitle: One\n---\n\nlinks [[b]]\n").unwrap();
        write_page(&k, "b", "---\ntitle: B\n---\n\nbody\n").unwrap();
        let title = |k: &KmsRef| {
            scan_index_entries(k)
                .into_iter()
                .find(|e| e.stem == "a")
                .unwrap()
                .title
        };
        assert_eq!(title(&k), "One");
        assert_eq!(backlink_map(&k).get("b").unwrap()[0].0, "a");

        // Same length, straight after: a coarse mtime alone would miss it.
        write_page(&k, "a", "---\ntitle: Two\n---\n\nlinks [[b]]\n").unwrap();
        assert_eq!(title(&k), "Two");

        // An outside editor writing in place, as Obsidian does.
        let path = k.pages_dir().join("a.md");
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("title: Two", "title: Three and longer")
            .replace("[[b]]", "nothing");
        std::fs::write(&path, edited).unwrap();
        assert_eq!(title(&k), "Three and longer");
        assert!(backlink_map(&k).get("b").is_none(), "the link is gone");

        delete_page(&k, "a").unwrap();
        assert!(scan_index_entries(&k).iter().all(|e| e.stem != "a"));
    }

    /// dev-plan/64 P3.8. A re-ingested source marks the pages built on it,
    /// however they name it — bare, in a flow list, or by research index —
    /// and records when the debt began without pretending the page changed.
    #[test]
    fn a_reingested_source_marks_every_page_that_cites_it() {
        let _g = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut reg = crate::research::registry::SourceRegistry::load(&k);
        let idx = reg.index_for("kms://nb/sources/report", "Report");
        reg.index_for("https://example.com/other", "Other");
        reg.save(&k).unwrap();
        let page = |sources: &str| {
            format!("---\ntitle: T\nsources: {sources}\nupdated: 2026-01-01\n---\n\nbody\n")
        };
        std::fs::write(k.pages_dir().join("bare.md"), page("report")).unwrap();
        std::fs::write(k.pages_dir().join("flow.md"), page("[\"report\", \"x\"]")).unwrap();
        std::fs::write(
            k.pages_dir().join("research.md"),
            page(&format!("[{idx}, 99]")),
        )
        .unwrap();
        std::fs::write(k.pages_dir().join("other.md"), page("[\"reporting\", 2]")).unwrap();

        assert_eq!(mark_dependent_pages_stale(&k, "report").unwrap(), 3);
        for stem in ["bare", "flow", "research"] {
            let raw = std::fs::read_to_string(k.pages_dir().join(format!("{stem}.md"))).unwrap();
            assert!(raw.contains("⚠ STALE: source `report`"), "{stem}: {raw}");
            assert!(raw.contains("stale_since:"), "{stem}: {raw}");
            assert!(
                raw.contains("updated: 2026-01-01"),
                "{stem} was not updated: {raw}"
            );
        }
        let other = std::fs::read_to_string(k.pages_dir().join("other.md")).unwrap();
        assert!(!other.contains("STALE"), "{other}");

        // A second re-ingest keeps the first date; a refresh clears it.
        let first = std::fs::read_to_string(k.pages_dir().join("bare.md")).unwrap();
        mark_dependent_pages_stale(&k, "report").unwrap();
        let (fm1, _) = parse_frontmatter(&first);
        let (fm2, _) =
            parse_frontmatter(&std::fs::read_to_string(k.pages_dir().join("bare.md")).unwrap());
        assert_eq!(fm1.get("stale_since"), fm2.get("stale_since"));
        write_page(
            &k,
            "bare",
            "---\ntitle: T\nsources: report\nstale_since: 2026-09-01\n---\n\nrefreshed\n",
        )
        .unwrap();
        let fresh = std::fs::read_to_string(k.pages_dir().join("bare.md")).unwrap();
        assert!(!fresh.contains("stale_since"), "{fresh}");
    }

    /// dev-plan/64 P3.5: a rename reaches `related:` in every form it is
    /// written in, and never a slug that merely starts the same.
    #[test]
    fn a_rename_follows_the_page_into_frontmatter() {
        let _g = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        write_page(&k, "claude-code", "---\ntitle: Claude Code\n---\nbody\n").unwrap();
        write_page(&k, "claude-code-hooks", "---\ntitle: Hooks\n---\nbody\n").unwrap();
        let flow = "---\ntitle: A\nrelated: [\"claude-code\", \"claude-code-hooks\", jevons]\nsupersedes: claude-code\n---\n\nSee [[claude-code#Spawning|spawn]] and [[claude-code-hooks]].\n";
        let block = "---\ntitle: B\nrelated:\n  - claude-code\n  - \"claude-code-hooks\"\ntags: [claude-code]\n---\n\nbody mentions claude-code in prose\n";
        std::fs::write(k.pages_dir().join("a.md"), flow).unwrap();
        std::fs::write(k.pages_dir().join("b.md"), block).unwrap();

        rename_page(&k, "claude-code", "claude-code-subagent-spawning").unwrap();

        let a = std::fs::read_to_string(k.pages_dir().join("a.md")).unwrap();
        assert!(
            a.contains(
                "related: [\"claude-code-subagent-spawning\", \"claude-code-hooks\", jevons]"
            ),
            "{a}"
        );
        assert!(
            a.contains("supersedes: claude-code-subagent-spawning"),
            "{a}"
        );
        assert!(
            a.contains("[[claude-code-subagent-spawning#Spawning|spawn]]"),
            "{a}"
        );
        assert!(a.contains("[[claude-code-hooks]]"), "{a}");
        let b = std::fs::read_to_string(k.pages_dir().join("b.md")).unwrap();
        assert!(
            b.contains("  - claude-code-subagent-spawning\n  - \"claude-code-hooks\""),
            "{b}"
        );
        assert!(
            b.contains("tags: [claude-code]"),
            "a tag is not a slug: {b}"
        );
        assert!(b.contains("mentions claude-code in prose"), "{b}");
    }

    /// dev-plan/64 P3.2. A write replaces the file whole or not at all, and
    /// leaves nothing behind that a page lister would count.
    #[test]
    fn a_write_replaces_the_file_in_one_step() {
        let td = tempfile::tempdir().unwrap();
        let page = td.path().join("ยุค.md");
        write_file(&page, "v1").unwrap();
        write_file(&page, "ฉบับที่สอง").unwrap();
        assert_eq!(std::fs::read_to_string(&page).unwrap(), "ฉบับที่สอง");
        let names: Vec<String> = std::fs::read_dir(td.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["ยุค.md"], "no temp file left");

        // A write that cannot complete leaves the old bytes and no temp.
        let gone = td.path().join("no-such-dir").join("x.md");
        assert!(write_file(&gone, "x").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&page, std::fs::Permissions::from_mode(0o600)).unwrap();
            write_file(&page, "v3").unwrap();
            let mode = std::fs::metadata(&page).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "permissions survive the replace");
        }
    }

    /// dev-plan/64 D1. The vault one agent built is the workspace's: a
    /// second agent in the same workspace resolves it, and it sits at the
    /// workspace root with every file it had.
    #[test]
    fn a_project_kms_is_shared_by_every_agent_in_the_workspace() {
        let (_g, ws) = as_agent("writer");
        legacy_vault(&ws, "main", "Age of Abundance", "ยุคที่ความฉลาดล้นเหลือ");

        let kref = resolve("age-of-abundance").expect("the writer sees main's vault");
        assert_eq!(kref.name, "Age of Abundance");
        assert_eq!(kref.root, ws.join(PROJECT_KMS_DIR).join("Age of Abundance"));
        assert!(kref.page_path("ยุคที่ความฉลาดล้นเหลือ").is_ok());
        assert!(!ws
            .join(".thclaws/bots/main")
            .join(PROJECT_KMS_DIR)
            .join("Age of Abundance")
            .exists());
        let log = std::fs::read_to_string(kref.root.join("log.md")).unwrap_or_default();
        assert!(
            log.contains("moved"),
            "the move is on the vault's own log: {log}"
        );

        let made = create("notes", KmsScope::Project).unwrap();
        assert_eq!(made.root, ws.join(PROJECT_KMS_DIR).join("notes"));
    }

    /// Two agents each hold a vault called `notes`. One moves; the other
    /// must not be overwritten, merged, or lost — it stays where it is
    /// and its own agent still reads ITS pages, not the workspace's.
    #[test]
    fn a_name_two_agents_both_use_moves_once_and_loses_nothing() {
        let (_g, ws) = as_agent("writer");
        legacy_vault(&ws, "main", "notes", "from-main");
        legacy_vault(&ws, "writer", "notes", "from-writer");
        legacy_vault(&ws, "writer", "drafts", "ch1");

        let report = migrate_project_kms_in(&ws);
        assert_eq!(report.len(), 3, "{report:?}");
        assert!(report
            .iter()
            .any(|l| l.contains("stays with agent 'writer'")));

        let shared = ws.join(PROJECT_KMS_DIR);
        assert!(shared.join("notes/pages/from-main.md").is_file());
        assert!(!shared.join("notes/pages/from-writer.md").exists());
        assert!(shared.join("drafts/pages/ch1.md").is_file());
        let kept = ws
            .join(".thclaws/bots/writer")
            .join(PROJECT_KMS_DIR)
            .join("notes");
        assert!(kept.join("pages/from-writer.md").is_file());

        let mine = resolve("notes").unwrap();
        assert_eq!(
            mine.root.canonicalize().unwrap(),
            kept.canonicalize().unwrap()
        );
        assert!(mine.page_path("from-writer").is_ok());
        let names: Vec<String> = list_all().into_iter().map(|k| k.name).collect();
        assert_eq!(names, vec!["drafts", "notes"], "one row per name");

        assert!(migrate_project_kms_in(&ws)
            .iter()
            .all(|l| l.contains("stays")));
        assert!(
            kept.join("pages/from-writer.md").is_file(),
            "a second run moves nothing"
        );
    }

    /// No host, no variable: the workspace root is the cwd and a project
    /// vault is exactly where it has always been.
    #[test]
    fn outside_a_host_a_project_kms_stays_under_the_cwd() {
        let _g = scoped_home();
        let prev = std::env::var("THCLAWS_WORKSPACE_ROOT").ok();
        std::env::remove_var("THCLAWS_WORKSPACE_ROOT");
        let made = create("solo", KmsScope::Project).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(made.root, cwd.join(PROJECT_KMS_DIR).join("solo"));
        assert!(legacy_project_root().is_none());
        assert!(resolve("solo").is_some());
        if let Some(v) = prev {
            std::env::set_var("THCLAWS_WORKSPACE_ROOT", v);
        }
    }

    #[test]
    fn create_seeds_starter_files() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::User).unwrap();
        assert!(k.index_path().exists());
        assert!(k.log_path().exists());
        assert!(k.schema_path().exists());
        assert!(k.pages_dir().is_dir());
    }

    #[test]
    fn create_is_idempotent() {
        let _home = scoped_home();
        let a = create("notes", KmsScope::User).unwrap();
        let b = create("notes", KmsScope::User).unwrap();
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn create_rejects_path_traversal() {
        let _home = scoped_home();
        assert!(create("../evil", KmsScope::User).is_err());
        assert!(create("foo/bar", KmsScope::User).is_err());
    }

    #[test]
    fn resolve_prefers_project_over_user() {
        let _home = scoped_home();
        create("shared", KmsScope::User).unwrap();
        create("shared", KmsScope::Project).unwrap();
        let found = resolve("shared").unwrap();
        assert_eq!(found.scope, KmsScope::Project);
    }

    #[test]
    fn list_all_returns_project_then_user() {
        let _home = scoped_home();
        create("user-only", KmsScope::User).unwrap();
        create("proj-only", KmsScope::Project).unwrap();
        let all = list_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].scope, KmsScope::Project);
        assert_eq!(all[1].scope, KmsScope::User);
    }

    #[test]
    fn ensure_default_creates_project_when_absent() {
        let _home = scoped_home();
        let k = ensure_default("fresh").unwrap();
        assert_eq!(k.scope, KmsScope::Project);
    }

    #[test]
    fn ensure_default_reuses_existing_user_scope_no_duplicate() {
        let _home = scoped_home();
        // A same-named KMS already lives in user scope (e.g. created long
        // ago). An unqualified ensure must reuse it, NOT mint a project
        // duplicate — the two-identical-entries bug.
        create("kb", KmsScope::User).unwrap();
        let k = ensure_default("kb").unwrap();
        assert_eq!(k.scope, KmsScope::User);
        // exactly one KMS named "kb" exists across all scopes
        assert_eq!(list_all().iter().filter(|r| r.name == "kb").count(), 1);
    }

    #[test]
    fn ensure_default_reuses_existing_project_scope() {
        let _home = scoped_home();
        create("kb", KmsScope::Project).unwrap();
        let k = ensure_default("kb").unwrap();
        assert_eq!(k.scope, KmsScope::Project);
        assert_eq!(list_all().iter().filter(|r| r.name == "kb").count(), 1);
    }

    #[test]
    fn system_prompt_section_empty_when_no_active() {
        let _home = scoped_home();
        assert_eq!(system_prompt_section(&[]), "");
    }

    /// The prompt announces a base; it does not list it.
    ///
    /// The whole page list used to be injected on every turn — 15.5 KB
    /// for 39 pages, and capped by entry count rather than bytes, so a
    /// large base could have taken ~80 KB per turn. The procedure was
    /// already search-first, so the list only ever answered "is this
    /// base worth searching", which the header answers far cheaper.
    #[test]
    fn the_prompt_announces_a_base_rather_than_listing_it() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        // `foo` is written first, so it is the entry page.
        write_page(&k, "foo", "---\ntitle: Foo\n---\n\nA page about foo.\n").unwrap();
        write_page(&k, "bar", "---\ntitle: Bar\n---\n\nA page about bar.\n").unwrap();
        let out = system_prompt_section(&["nb".into()]);

        assert!(out.contains("## KMS: nb"), "{out}");
        assert!(
            out.contains("2 page(s)"),
            "size is the relevance cue: {out}"
        );
        assert!(
            out.contains("KmsRead") && out.contains("\"index\""),
            "must point at the on-demand list: {out}"
        );
        // Names, yes — search cannot yet be relied on to find a Thai page,
        // so every page has to be nameable from the prompt. Summaries, no:
        // they were nine tenths of the bytes.
        assert!(out.contains("- foo — Foo"), "page names are listed: {out}");
        assert!(out.contains("- bar — Bar"), "page names are listed: {out}");
        // The entry page's summary IS the base's `About:` line, so that one
        // belongs here. No other page's summary does.
        assert!(out.contains("About: A page about foo."), "{out}");
        assert!(
            !out.contains("A page about bar."),
            "summaries must NOT be injected:\n{out}"
        );
        assert!(!out.contains("pages/foo.md"), "{out}");

        // And the list is still reachable, with the summary in it.
        let full = full_index(&k);
        assert!(full.contains("pages/foo.md"), "{full}");
        assert!(full.contains("A page about foo."), "{full}");
    }

    /// M6.39.5: pin the strong-imperative wording of the prelude.
    /// User reported via /system inspection that even when KMS was
    /// active and the index summary was descriptive, the LLM still
    /// answered from training data. Pre-fix prelude said "consult
    /// them before answering" — soft language. This test locks the
    /// directive form so a future "smooth out the wording" refactor
    /// can't regress it.
    #[test]
    fn system_prompt_section_uses_mandatory_consultation_directive() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.index_path(), "# nb\n- [foo](pages/foo.md) — foo\n").unwrap();
        let out = system_prompt_section(&["nb".into()]);
        // MUST include the strong imperative form
        assert!(
            out.contains("MANDATORY"),
            "prelude must use MANDATORY (got soft 'consult'-style wording)"
        );
        // MUST name the tool call sequence explicitly — `KmsSearch`
        // first, then `KmsRead`, then answer. This is the procedure
        // the model needs to follow.
        assert!(out.contains("KmsSearch"));
        assert!(out.contains("KmsRead"));
        // MUST forbid the shortcut (answering from training when KMS
        // could match). Without this the model rationalizes skipping
        // ("I already know the answer").
        let lower = out.to_ascii_lowercase();
        assert!(
            lower.contains("do not skip"),
            "prelude must forbid skipping the lookup steps"
        );
        // MUST acknowledge the no-match fallback so the model doesn't
        // feel boxed in when KMS genuinely has nothing.
        assert!(
            lower.contains("fall back to training-data knowledge"),
            "prelude must allow training-data fallback when KMS has no hits"
        );
    }

    /// dev-plan/64 P2.2. The section is paid for on every request, so it
    /// has a budget, and it explains a kind of page only to a base that
    /// has one: the two-layer and provenance paragraphs were 1.3 KB sent
    /// to every conversation whether or not they described anything.
    #[test]
    fn the_prelude_fits_its_budget_and_explains_only_what_is_there() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("foo.md"), "---\ntitle: Foo\n---\nbody\n").unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.len() <= 2_500, "{} bytes:\n{out}", out.len());
        assert!(!out.contains("Two layers"), "{out}");
        assert!(!out.contains("Prefer topic pages"), "{out}");
        assert!(out.contains("<system-reminder>"), "{out}");

        std::fs::write(k.pages_dir().join("sess-1.md"), "---\ntitle: S\n---\nx\n").unwrap();
        std::fs::create_dir_all(k.root.join("sources")).unwrap();
        std::fs::write(k.root.join("sources/raw.txt"), "raw").unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.contains("Two layers"), "{out}");
        assert!(out.contains("Prefer topic pages"), "{out}");
    }

    #[test]
    fn system_prompt_section_skips_missing() {
        let _home = scoped_home();
        let out = system_prompt_section(&["does-not-exist".into()]);
        assert_eq!(out, "");
    }

    /// Audit finding B: the per-KMS `### Tools` subsection that
    /// pre-fix appeared in every attached KMS block (~250 bytes each)
    /// is now globalised — rendered once near the top. With N KMSes
    /// attached the saving compounds linearly. Lock the dedup so a
    /// future "add a Tools section to each KMS block for clarity"
    /// can't quietly regress us back to O(N) duplication.
    #[test]
    fn system_prompt_section_globalises_tools_reference() {
        let _home = scoped_home();
        let a = create("alpha", KmsScope::User).unwrap();
        let b = create("beta", KmsScope::User).unwrap();
        std::fs::write(a.index_path(), "# alpha\n- [x](pages/x.md) — x\n").unwrap();
        std::fs::write(b.index_path(), "# beta\n- [y](pages/y.md) — y\n").unwrap();

        let out = system_prompt_section(&["alpha".into(), "beta".into()]);

        // dev-plan/64 P2.2: the tools block is gone altogether — it
        // repeated the tool definitions the request already carries. The
        // write tools are still named, once, where the prelude says when
        // to use them.
        assert!(!out.contains("## KMS tools"), "{out}");
        assert_eq!(out.matches("`KmsDelete`").count(), 1, "{out}");
        // Each KMS still has its own block (Schema + Index).
        assert!(out.contains("## KMS: alpha"));
        assert!(out.contains("## KMS: beta"));
        // The per-KMS `### Tools` subsection must NOT reappear —
        // that was the bug. (We still allow the global `## KMS tools`
        // h2 to match `KMS tools` substring; check the h3 form
        // specifically.)
        assert!(
            !out.contains("### Tools"),
            "no per-KMS `### Tools` h3 subsection should remain (globalised): {out}"
        );
        // The tools themselves must still be reachable from the
        // prompt — name-only check on the three most-called ones.
        assert!(out.contains("KmsRead"));
        assert!(out.contains("KmsWrite"));
        assert!(out.contains("KmsSearch"));
        // KmsCreate is now in the global block too — fix from the
        // earlier dreams-KMS rollout that had previously surfaced
        // KmsCreate only via the tool registry.
        assert!(
            out.contains("KmsCreate"),
            "KmsCreate must appear in the globalised tools so /dream + bootstrap workflows are discoverable: {out}"
        );
    }

    /// Audit finding C: SCHEMA.md template trimmed to a single input
    /// example. The pre-fix template carried two fenced-code blocks
    /// (input shape + "Final on-disk shape") — the second one was
    /// inert for the model since `KmsWrite` stamps it automatically.
    /// Save ~300 bytes per KMS by dropping it. Lock the trim so a
    /// future "let's add the on-disk example back for clarity" edit
    /// can't quietly re-balloon every prompt.
    #[test]
    fn create_writes_concise_schema_template() {
        let _home = scoped_home();
        let k = create("trimmed", KmsScope::User).unwrap();
        let schema = std::fs::read_to_string(k.schema_path()).unwrap();
        // Must still teach the canonical shape — the input frontmatter
        // example. The model needs this to write correctly.
        assert!(
            schema.contains("title:"),
            "schema must show title: frontmatter key"
        );
        assert!(
            schema.contains("topic:"),
            "schema must show topic: frontmatter key"
        );
        // Must NOT carry the dual-example bloat that ballooned the
        // template (the "Final on-disk shape:" header + `created:` /
        // `updated:` example were the markers of the verbose template).
        assert!(
            !schema.contains("Final on-disk shape"),
            "schema template must not include the redundant on-disk example: {schema}"
        );
        assert!(
            !schema.contains("created: 2026"),
            "schema template must not bake a specific date — implies the on-disk example is back: {schema}"
        );
    }

    #[test]
    fn page_path_rejects_traversal() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        assert!(k.page_path("../../etc/passwd").is_err());
        assert!(k.page_path("/etc/passwd").is_err());
        assert!(k.page_path("foo/bar").is_err()); // path separator
        assert!(k.page_path("").is_err()); // empty name
        assert!(k.page_path("foo\0bar").is_err()); // null byte

        // The happy path: create the file first (page_path now requires
        // the file to exist so it can canonicalize + symlink-check).
        std::fs::write(k.pages_dir().join("ok-page.md"), "body").unwrap();
        assert!(k.page_path("ok-page").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn page_path_rejects_symlink_to_outside() {
        use std::os::unix::fs::symlink;
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();

        // Attacker plants a symlink in pages/ to an outside target.
        let target_dir = tempfile::tempdir().unwrap();
        let outside_file = target_dir.path().join("secret.md");
        std::fs::write(&outside_file, "top secret").unwrap();
        let symlink_path = k.pages_dir().join("leaked.md");
        symlink(&outside_file, &symlink_path).unwrap();

        // Despite the file existing (via symlink), page_path rejects
        // because canonical candidate escapes the KMS root.
        let result = k.page_path("leaked");
        assert!(result.is_err(), "expected symlink to be rejected");
        let err_str = format!("{}", result.unwrap_err());
        assert!(
            err_str.contains("symlink escape") || err_str.contains("outside the KMS"),
            "unexpected error: {err_str}"
        );
    }

    /// M6.25 BUG #2: ingest now SPLITS source from page. Raw content
    /// lands in `sources/<alias>.<ext>`; a stub page with frontmatter
    /// lands in `pages/<alias>.md` pointing at it. Verifies the new
    /// shape end-to-end.
    #[test]
    fn ingest_splits_source_from_page() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("intro.md");
        std::fs::write(&src, "# Intro\n\nFirst real line of content.\n").unwrap();

        let result = ingest(&k, &src, None, false).unwrap();
        assert_eq!(result.alias, "intro");
        assert!(!result.overwrote);
        assert!(result.target.exists());
        // The target is the derived page, not the raw source.
        assert!(result.target.ends_with("pages/intro.md"));

        // Raw source lives under sources/ — verbatim.
        let source_copy = k.root.join("sources/intro.md");
        let raw = std::fs::read_to_string(&source_copy).unwrap();
        assert!(raw.contains("First real line"));

        // The page is DERIVED from the source, not a fixed placeholder:
        // it carries the source's own title and lead, is marked
        // uncurated, and links the archive with a real relative link so
        // the graph/backlink views connect the two.
        let page_body = std::fs::read_to_string(&result.target).unwrap();
        let (fm, body) = parse_frontmatter(&page_body);
        assert_eq!(fm.get("sources").map(String::as_str), Some("intro"));
        assert_eq!(
            fm.get("category").map(String::as_str),
            Some("uncategorized")
        );
        assert_eq!(fm.get("status").map(String::as_str), Some("derived"));
        assert!(fm.contains_key("created"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("# Intro"), "title not derived:\n{body}");
        assert!(
            body.contains("First real line of content."),
            "lead not carried:\n{body}"
        );
        assert!(
            body.contains("(../sources/intro.md)"),
            "no relative source link (graph/backlinks depend on it):\n{body}"
        );
        assert!(body.contains("## Provenance"), "{body}");
        assert!(!body.contains("Stub page"), "placeholder survived:\n{body}");

        // Index.md now has a bullet pointing at the page.
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(
            index.contains("- [intro](pages/intro.md)"),
            "index missing bullet, got:\n{index}"
        );

        // M6.25 BUG #7: log uses `## [date] verb | alias` header form.
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(
            log.contains("## [") && log.contains("] ingested | intro"),
            "log missing header-style entry, got:\n{log}"
        );
    }

    #[test]
    fn ingest_localizes_local_markdown_images() {
        let _home = scoped_home();
        let k = create("clips", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src_dir.path().join("images")).unwrap();
        // A real local image the markdown references (relative, twice).
        std::fs::write(src_dir.path().join("images/pic.png"), b"\x89PNGfake").unwrap();

        let src = src_dir.path().join("article.md");
        std::fs::write(
            &src,
            "# Article\n\nLead line.\n\n\
             ![local](images/pic.png)\n\
             ![again](./images/pic.png)\n\
             ![titled](images/pic.png \"cap\")\n\
             ![remote](https://example.com/x.png)\n\
             ![missing](images/nope.png)\n",
        )
        .unwrap();

        let result = ingest(&k, &src, None, false).unwrap();
        // One physical image, copied once even though referenced 3×.
        assert_eq!(
            result.images_copied, 1,
            "the single local image should copy exactly once"
        );

        // Asset landed under sources/<alias>-assets/.
        let asset = k.root.join("sources/article-assets/001-pic.png");
        assert!(
            asset.is_file(),
            "copied asset missing at {}",
            asset.display()
        );

        // Archived source: local links rewritten (title preserved),
        // remote + missing links left exactly as they were.
        let raw = std::fs::read_to_string(k.root.join("sources/article.md")).unwrap();
        assert!(
            raw.contains("![local](article-assets/001-pic.png)"),
            "local link not rewritten, got:\n{raw}"
        );
        assert!(
            raw.contains("![again](article-assets/001-pic.png)"),
            "deduped link not rewritten, got:\n{raw}"
        );
        assert!(
            raw.contains("![titled](article-assets/001-pic.png \"cap\")"),
            "title must survive rewrite, got:\n{raw}"
        );
        assert!(
            raw.contains("![remote](https://example.com/x.png)"),
            "remote link must stay untouched, got:\n{raw}"
        );
        assert!(
            raw.contains("![missing](images/nope.png)"),
            "missing-file link must stay untouched, got:\n{raw}"
        );
    }

    #[test]
    fn ingest_collides_without_force() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("page.md");
        std::fs::write(&src, "a").unwrap();

        ingest(&k, &src, Some("topic"), false).unwrap();
        let err = ingest(&k, &src, Some("topic"), false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("already exists"),
            "expected collision, got: {msg}"
        );

        // --force replaces, and is flagged as overwrote. The raw source
        // copy carries the new bytes; the page stub is regenerated.
        std::fs::write(&src, "b").unwrap();
        let r = ingest(&k, &src, Some("topic"), true).unwrap();
        assert!(r.overwrote);
        let raw = std::fs::read_to_string(k.root.join("sources/topic.md")).unwrap();
        assert_eq!(raw, "b");
    }

    #[test]
    fn ingest_rejects_unknown_extension() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("bin.xyz");
        std::fs::write(&src, "data").unwrap();
        let err = ingest(&k, &src, None, false).unwrap_err();
        assert!(format!("{err}").contains("not supported"));
    }

    /// Re-ingesting a document refreshes its source and regenerates its
    /// stub — but only while the page still IS a stub. Once a research
    /// run or a person has written it, `--force` must not put the outline
    /// back over their work.
    #[test]
    fn a_forced_reingest_keeps_a_page_that_is_no_longer_a_stub() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("ยุคใหม่.md");
        std::fs::write(&src, "# ยุคใหม่\n\nversion one\n").unwrap();
        let first = ingest(&k, &src, None, false).unwrap();
        let page = k.pages_dir().join(format!("{}.md", first.alias));
        assert!(std::fs::read_to_string(&page)
            .unwrap()
            .contains("status: derived"));

        // A stub is ours to regenerate, and keeps its birthday.
        let stamped = std::fs::read_to_string(&page).unwrap().replace(
            &format!("created: {}", crate::usage::today_str()),
            "created: 2020-01-01",
        );
        std::fs::write(&page, stamped).unwrap();
        ingest(&k, &src, None, true).unwrap();
        assert!(
            std::fs::read_to_string(&page)
                .unwrap()
                .contains("created: 2020-01-01"),
            "`created:` was dropped on re-ingest"
        );

        // The research run turns the stub into the topic page…
        std::fs::write(
            &page,
            "---\ncreated: 2020-01-01\nkind: moc\nrelated: [\"a\", \"b\"]\ntitle: ยุคใหม่\ntype: note\n---\n\nthe written topic page\n",
        )
        .unwrap();
        // …and a later re-ingest of an edited document must leave it alone
        // while still replacing the archived source.
        std::fs::write(&src, "# ยุคใหม่\n\nversion two\n").unwrap();
        ingest(&k, &src, None, true).unwrap();
        let after = std::fs::read_to_string(&page).unwrap();
        assert!(
            after.contains("the written topic page"),
            "page overwritten:\n{after}"
        );
        assert!(after.contains("kind: moc"), "{after}");
        let archived =
            std::fs::read_to_string(k.sources_dir().join(format!("{}.md", first.alias))).unwrap();
        assert!(archived.contains("version two"), "source not refreshed");
    }

    #[test]
    fn ingest_rejects_reserved_alias() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("file.md");
        std::fs::write(&src, "x").unwrap();
        let err = ingest(&k, &src, Some("index"), false).unwrap_err();
        assert!(format!("{err}").contains("reserved"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_kms_dir() {
        use std::os::unix::fs::symlink;
        let _home = scoped_home();

        // Attacker plants a symlink where a KMS dir should be.
        let target = tempfile::tempdir().unwrap();
        let kms_root = scope_root(KmsScope::User).unwrap();
        std::fs::create_dir_all(&kms_root).unwrap();
        symlink(target.path(), kms_root.join("evil")).unwrap();

        // resolve() should not return a KmsRef for a symlinked dir.
        assert!(
            resolve("evil").is_none(),
            "symlinked KMS dir should be rejected"
        );
    }

    // ─── M6.25: frontmatter (BUG #9) ──────────────────────────────────────

    // ─── M6.39.13: graph builder ──────────────────────────────────────────

    #[test]
    fn graph_extracts_wikilink_targets() {
        let body = "see [[alpha]] and [[beta|Beta Display]]\nrandom [text](http://x).\n[[gamma]]";
        let targets = extract_wikilink_targets(body);
        assert_eq!(targets, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn link_phrase_links_first_plain_mention_only() {
        let _home = scoped_home();
        let k = create("lp", KmsScope::User).unwrap();
        write_page(
            &k,
            "topic",
            "---\ntitle: \"Topic\"\n---\n\n# Hunyuan Hy4 heading\n\nSee [[x|Hunyuan Hy4]] and `Hunyuan Hy4` first; then Hunyuan Hy4 in prose. Hunyuan Hy4 again.\n",
        )
        .unwrap();
        assert!(link_phrase(&k, "topic", "Hunyuan Hy4", "hunyuan-hy4").unwrap());
        let on_disk = std::fs::read_to_string(k.pages_dir().join("topic.md")).unwrap();
        assert!(on_disk.contains("# Hunyuan Hy4 heading"), "{on_disk}");
        assert!(on_disk.contains("[[x|Hunyuan Hy4]] and `Hunyuan Hy4` first; then [[hunyuan-hy4|Hunyuan Hy4]] in prose. Hunyuan Hy4 again."), "{on_disk}");
        assert!(on_disk.starts_with("---\n"), "frontmatter kept: {on_disk}");
        assert!(!link_phrase(&k, "topic", "not in the page", "nope").unwrap());
    }

    #[test]
    fn rename_moves_the_directory_and_refuses_collisions() {
        let _home = scoped_home();
        let k = create("old-name", KmsScope::User).unwrap();
        write_page(&k, "alpha", "---\ntitle: \"Alpha\"\n---\n\nbody\n").unwrap();
        let _other = create("taken", KmsScope::User).unwrap();
        assert!(rename("old-name", "taken").is_err(), "collision refused");
        assert!(rename("old-name", "../evil").is_err(), "bad name refused");
        let r = rename("old-name", "new-name").unwrap();
        assert_eq!(r.name, "new-name");
        assert!(r.pages_dir().join("alpha.md").is_file());
        assert!(resolve("old-name").is_none());
        assert!(resolve("new-name").is_some());
    }

    #[test]
    fn write_page_keeps_the_creation_date_across_a_rewrite() {
        let _home = scoped_home();
        let k = create("created-rt", KmsScope::User).unwrap();
        write_page(&k, "p", "---\ntitle: \"P\"\n---\n\nfirst\n").unwrap();
        let first = std::fs::read_to_string(k.pages_dir().join("p.md")).unwrap();
        let created = parse_frontmatter(&first).0.remove("created").unwrap();
        // A rewrite that supplies no `created:` — what /research does on
        // every run over a note it already wrote.
        write_page(&k, "p", "---\ntitle: \"P\"\nkind: concept\n---\n\nsecond\n").unwrap();
        let again = std::fs::read_to_string(k.pages_dir().join("p.md")).unwrap();
        assert_eq!(
            parse_frontmatter(&again).0.remove("created").as_deref(),
            Some(created.as_str()),
            "{again}"
        );
        assert!(again.contains("second"));
    }

    #[test]
    fn the_first_page_created_becomes_the_recorded_entry() {
        let _home = scoped_home();
        let k = create("first-rt", KmsScope::User).unwrap();
        assert_eq!(k.read_manifest().and_then(|m| m.entry), None);

        write_page(&k, "opening", "---\ntitle: O\n---\n\nfirst thing here\n").unwrap();
        assert_eq!(
            k.read_manifest().and_then(|m| m.entry).as_deref(),
            Some("opening")
        );
        // A later page does not steal it, and neither does rewriting
        // the first one.
        write_page(&k, "second", "---\ntitle: S\nkind: moc\n---\n\nx\n").unwrap();
        write_page(&k, "opening", "---\ntitle: O\n---\n\nedited\n").unwrap();
        assert_eq!(entry_page(&k).as_deref(), Some("opening"));

        // A rename carries it; the inference would have said `second`
        // (it is the only moc), so this proves the record wins.
        rename_page(&k, "opening", "the-opening").unwrap();
        assert_eq!(entry_page(&k).as_deref(), Some("the-opening"));

        // Deleting it falls back to inference.
        delete_page(&k, "the-opening").unwrap();
        assert_eq!(k.read_manifest().and_then(|m| m.entry), None);
        assert_eq!(entry_page(&k).as_deref(), Some("second"));
    }

    #[test]
    fn apply_entry_shows_sets_clears_and_refuses_a_missing_page() {
        let _home = scoped_home();
        let k = create("apply-rt", KmsScope::User).unwrap();
        write_page(&k, "a", "---\ntitle: A\n---\n\nx\n").unwrap();
        write_page(&k, "b", "---\ntitle: B\n---\n\nx\n").unwrap();
        assert!(apply_entry("apply-rt", None, false)
            .unwrap()
            .contains("`a`"));
        assert!(apply_entry("apply-rt", Some("nope"), false).is_err());
        assert!(apply_entry("apply-rt", Some("b.md"), false)
            .unwrap()
            .contains("`b`"));
        assert_eq!(entry_page(&k).as_deref(), Some("b"));
        let cleared = apply_entry("apply-rt", None, true).unwrap();
        assert!(cleared.contains("cleared"), "{cleared}");
        assert_eq!(k.read_manifest().and_then(|m| m.entry), None);
        assert!(apply_entry("nosuch", None, false).is_err());
    }

    #[test]
    fn entry_page_prefers_the_map_of_content_then_the_hub() {
        let _home = scoped_home();
        let k = create("entry-rt", KmsScope::User).unwrap();
        // The inference chain, tested directly: `entry_page` would
        // short-circuit on the record the first write leaves behind.
        assert_eq!(infer_entry_page(&k), None);

        // Nothing linked: the most recently updated wins.
        write_page(&k, "old", "---\ntitle: O\nupdated: 2026-01-01\n---\n\nx\n").unwrap();
        write_page(&k, "new", "---\ntitle: N\nupdated: 2026-09-01\n---\n\nx\n").unwrap();
        assert_eq!(infer_entry_page(&k).as_deref(), Some("new"));

        // A hub outranks a merely-recent page.
        write_page(&k, "hub", "---\ntitle: H\nupdated: 2026-02-01\n---\n\nx\n").unwrap();
        write_page(&k, "a", "---\ntitle: A\n---\n\n[[hub]]\n").unwrap();
        write_page(&k, "b", "---\ntitle: B\n---\n\n[[hub]]\n").unwrap();
        assert_eq!(infer_entry_page(&k).as_deref(), Some("hub"));

        // A map of content outranks the hub even with fewer backlinks.
        write_page(
            &k,
            "topic",
            "---\ntitle: T\nkind: moc\nupdated: 2026-03-01\n---\n\n[[hub]]\n",
        )
        .unwrap();
        assert_eq!(infer_entry_page(&k).as_deref(), Some("topic"));

        // The record the first write left behind still wins overall.
        assert_eq!(entry_page(&k).as_deref(), Some("old"));
    }

    #[test]
    fn backlink_map_builds_the_whole_reverse_index_in_one_pass() {
        let _home = scoped_home();
        let k = create("bmap-rt", KmsScope::User).unwrap();
        write_page(&k, "hub", "---\ntitle: \"Hub\"\n---\n\nnothing\n").unwrap();
        write_page(
            &k,
            "a",
            "---\ntitle: \"A\"\nrelated: [\"hub\"]\n---\n\n[[hub]] twice: [[hub|Hub]]\n",
        )
        .unwrap();
        write_page(&k, "b", "---\ntitle: \"B\"\n---\n\nsee [[a]] and [[hub]]\n").unwrap();
        write_page(&k, "self", "---\ntitle: \"S\"\n---\n\n[[self]]\n").unwrap();
        let map = backlink_map(&k);
        assert_eq!(
            map.get("hub").map(|v| v.len()),
            Some(2),
            "one edge per linking page, not per mention: {map:?}"
        );
        assert_eq!(
            map.get("a")
                .map(|v| v.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>()),
            Some(vec!["b"])
        );
        assert!(!map.contains_key("self"), "a page is not its own backlink");
        // The markdown link form counts too — it is what a hand-written
        // page uses and what an OKF round trip leaves behind.
        write_page(&k, "md", "---\ntitle: \"MD\"\n---\n\n[Hub](pages/hub.md)\n").unwrap();
        assert!(
            backlink_map(&k)["hub"].iter().any(|(s, _)| s == "md"),
            "markdown page links are edges"
        );
    }

    #[test]
    fn strip_backlink_section_removes_the_block_and_nothing_else() {
        let body = "intro\n\n## Linked from\n\n- [A](/pages/a.md)\n\n## Sources\n\n1. x\n";
        let out = strip_backlink_section(body);
        assert!(out.starts_with("intro"), "{out}");
        assert!(out.contains("## Sources"), "{out}");
        assert!(!out.contains("Linked from"), "{out}");
        // Trailing block, and a body with no block at all.
        assert_eq!(
            strip_backlink_section("body\n\n## Linked from\n\n- [A](/pages/a.md)\n"),
            "body"
        );
        assert_eq!(strip_backlink_section("plain body\n"), "plain body");
    }

    #[test]
    fn backlinks_read_wikilinks_and_related_frontmatter() {
        let _home = scoped_home();
        let k = create("backlink-rt", KmsScope::User).unwrap();
        write_page(&k, "target", "---\ntitle: \"Target\"\n---\n\nthe note\n").unwrap();
        write_page(
            &k,
            "prose",
            "---\ntitle: \"Prose\"\nupdated: 2026-09-02\n---\n\nsee [[target|Target]]\n",
        )
        .unwrap();
        write_page(
            &k,
            "frontmatter-only",
            "---\ntitle: \"FM only\"\nrelated: [\"target\"]\nupdated: 2026-09-05\n---\n\nno inline link\n",
        )
        .unwrap();
        write_page(&k, "unrelated", "---\ntitle: \"Other\"\n---\n\nnothing\n").unwrap();
        let b = backlinks("backlink-rt", "target");
        let slugs: Vec<&str> = b.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["frontmatter-only", "prose"],
            "newest first: {b:?}"
        );
        assert_eq!(b[0].1, "FM only");
        assert!(backlinks("backlink-rt", "unrelated").is_empty());
        // A page linked only through `related:` is not an orphan.
        let report = lint(&k).unwrap();
        assert!(
            !report.orphan_pages.contains(&"target".to_string()),
            "{:?}",
            report.orphan_pages
        );
    }

    #[test]
    fn graph_skips_dangling_and_self_links() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        write_page(
            &k,
            "alpha",
            "---\ntitle: \"Alpha\"\nrelated: [\"beta\"]\n---\n\nlinks to [[beta]] and [[ghost]] and self [[alpha]], and [[beta|Beta]] again\n",
        )
        .unwrap();
        write_page(
            &k,
            "beta",
            "---\ntitle: \"Beta\"\n---\n\nback to [[alpha]]\n",
        )
        .unwrap();
        let g = graph("nb", false).expect("graph");
        let ids: Vec<_> = g.nodes.iter().map(|n| n.id.clone()).collect();
        assert!(ids.contains(&"alpha".to_string()));
        assert!(ids.contains(&"beta".to_string()));
        assert!(!ids.contains(&"ghost".to_string()));
        // alpha → beta + beta → alpha; alpha → ghost dropped (dangling);
        // alpha → alpha dropped (self-link); alpha → beta counted ONCE
        // although it appears inline twice and in `related:`.
        assert_eq!(g.edges.len(), 2, "{:?}", g.edges);
        let alpha = g.nodes.iter().find(|n| n.id == "alpha").unwrap();
        assert_eq!(alpha.label, "Alpha");
        assert_eq!(alpha.kind, GraphNodeKind::Page);
    }

    #[test]
    fn graph_extracts_source_link_targets() {
        let body = "see [1](../sources/foo.md) and [2](../sources/bar) and [3](../sources/baz.md#x)\n[ignore](other/path.md)";
        let targets = extract_source_link_targets(body);
        assert_eq!(targets, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn graph_includes_sources_when_requested() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        write_page(
            &k,
            "alpha",
            "---\ntitle: \"Alpha\"\n---\n\nciting [1](../sources/example-com.md) and [2](../sources/ghost-source.md)\n",
        )
        .unwrap();
        // Create a sources/ archive that the page cites.
        let sources_dir = k.root.join("sources");
        std::fs::create_dir_all(&sources_dir).unwrap();
        std::fs::write(
            sources_dir.join("example-com.md"),
            "---\ntitle: \"Example Inc.\"\n---\n\nbody\n",
        )
        .unwrap();
        // Note: ghost-source.md does NOT exist on disk — should be dropped.

        // Without flag: only the page node, no source nodes/edges.
        let g_off = graph("nb", false).expect("graph");
        assert_eq!(g_off.nodes.len(), 1);
        assert!(g_off.edges.is_empty());

        // With flag: page node + 1 source node + 1 page→source edge
        // (the dangling ghost-source citation is dropped).
        let g_on = graph("nb", true).expect("graph");
        assert_eq!(g_on.nodes.len(), 2);
        let src = g_on
            .nodes
            .iter()
            .find(|n| n.kind == GraphNodeKind::Source)
            .expect("source node");
        assert_eq!(src.id, "source:example-com");
        assert_eq!(src.label, "Example Inc.");
        assert_eq!(g_on.edges.len(), 1);
        assert_eq!(g_on.edges[0].source, "alpha");
        assert_eq!(g_on.edges[0].target, "source:example-com");
    }

    #[test]
    fn parse_frontmatter_extracts_keys_and_strips_block() {
        let s = "---\ncategory: research\ntags: ai\nsources: paper-x\n---\n# Body\n\nHello.\n";
        let (fm, body) = parse_frontmatter(s);
        assert_eq!(fm.get("category").map(String::as_str), Some("research"));
        assert_eq!(fm.get("tags").map(String::as_str), Some("ai"));
        assert_eq!(fm.get("sources").map(String::as_str), Some("paper-x"));
        assert_eq!(body, "# Body\n\nHello.\n");
    }

    #[test]
    fn parse_frontmatter_no_block_returns_empty_and_original() {
        let s = "# No frontmatter\n\nHello.\n";
        let (fm, body) = parse_frontmatter(s);
        assert!(fm.is_empty());
        assert_eq!(body, s);
    }

    #[test]
    fn write_frontmatter_round_trips() {
        let mut fm = std::collections::BTreeMap::new();
        fm.insert("category".into(), "research".into());
        fm.insert("note".into(), "has: colon".into()); // forces quoting
        let serialized = write_frontmatter(&fm, "body text\n");
        let (parsed, body) = parse_frontmatter(&serialized);
        assert_eq!(parsed.get("category").map(String::as_str), Some("research"));
        assert_eq!(parsed.get("note").map(String::as_str), Some("has: colon"));
        assert_eq!(body, "body text\n");
    }

    #[test]
    fn write_frontmatter_preserves_flow_list_unquoted() {
        // A `sources:` YAML list must round-trip as a real sequence, not
        // get quoted into the opaque string `"[\"a\", \"b\"]"`.
        let mut fm = std::collections::BTreeMap::new();
        fm.insert("sources".into(), "[\"sess-abc\", \"sess-def\"]".into());
        let serialized = write_frontmatter(&fm, "body\n");
        assert!(
            serialized.contains("sources: [\"sess-abc\", \"sess-def\"]"),
            "flow list should be emitted verbatim, got:\n{serialized}"
        );
        // No outer quoting / escaping of the list.
        assert!(!serialized.contains("sources: \"["));
        assert!(!serialized.contains("\\\""));
    }

    // ─── M6.25: write_page + append_to_page (BUG #1) ──────────────────────

    #[test]
    fn write_page_creates_with_stamps_and_index_bullet() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(&k, "topic", "# Topic\n\nBody.\n").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert!(fm.contains_key("created"), "created stamp missing");
        assert!(fm.contains_key("updated"), "updated stamp missing");
        assert!(body.contains("Body."));
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index.contains("- [topic](pages/topic.md)"));
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(log.contains("] wrote | topic"));
    }

    #[test]
    fn write_page_index_summary_prefers_topic_frontmatter() {
        // A page whose body opens with a `## Overview` heading must
        // surface its `topic:` line in the index — not "Overview".
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        write_page(
            &k,
            "welsh-corgi",
            "---\ntitle: Welsh Corgi\ntopic: Dog breed profile — Welsh Corgi\n---\n\n## Overview\n\nThe Welsh Corgi is a herding dog.\n",
        )
        .unwrap();
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(
            index.contains(
                "- [welsh-corgi](pages/welsh-corgi.md) — Dog breed profile — Welsh Corgi"
            ),
            "index should use topic: frontmatter, got:\n{index}"
        );
        assert!(!index.contains("— Overview"));
    }

    #[test]
    fn write_page_index_summary_falls_back_to_body_without_topic() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        write_page(
            &k,
            "note",
            "---\ntitle: Note\n---\n\nFirst real line here.\n",
        )
        .unwrap();
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(
            index.contains("First real line here."),
            "no topic: should fall back to first body line, got:\n{index}"
        );
    }

    #[test]
    fn write_page_replace_preserves_created_bumps_updated() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(&k, "topic", "v1").unwrap();
        let raw1 = std::fs::read_to_string(&path).unwrap();
        let (fm1, _) = parse_frontmatter(&raw1);
        let created = fm1.get("created").cloned().unwrap();

        // Write again with explicit created override that should win.
        let _ = write_page(&k, "topic", "---\ncreated: 1999-01-01\n---\nv2").unwrap();
        let raw2 = std::fs::read_to_string(&path).unwrap();
        let (fm2, body2) = parse_frontmatter(&raw2);
        // User-supplied frontmatter wins on conflict.
        assert_eq!(fm2.get("created").map(String::as_str), Some("1999-01-01"));
        // updated still gets a stamp.
        assert!(fm2.contains_key("updated"));
        // Canonical header was injected (body had no `# heading`), so
        // body2 carries `# topic\n---\n\nv2` rather than just `v2`. The
        // v2 payload must still be present at the tail.
        assert!(body2.contains("v2"));
        assert!(
            body2.contains("# topic"),
            "expected canonical `# {{stem}}` header to be injected when body had no heading; got: {body2}"
        );
        // Index has exactly one entry for `topic` (no duplicates).
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        let count = index.matches("(pages/topic.md)").count();
        assert_eq!(count, 1, "expected one entry, got {count}\n{index}");
        // Sanity: original `created` was today, the override moved it.
        assert_ne!(created, "1999-01-01");
    }

    #[test]
    fn write_page_injects_canonical_header_with_title_and_topic() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(
            &k,
            "auth-tokens",
            "---\ntitle: Auth tokens\ntopic: how the API stores session tokens\n---\nWe rotate JWTs nightly.\n",
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (_, body) = parse_frontmatter(&raw);
        assert!(
            body.contains("# Auth tokens"),
            "title heading missing: {body}"
        );
        // dev-plan/64 P3.9: the title stands alone. `topic:` is in the
        // frontmatter, and a bare `---` under the title rendered as a
        // second rule right below the heading's own underline.
        assert!(
            !body.contains("Description:"),
            "Description line should no longer be injected: {body}"
        );
        assert!(
            !body.contains("\n---"),
            "the rule under the title should be gone: {body}"
        );
        assert!(body.contains("We rotate JWTs nightly."));
    }

    #[test]
    fn write_page_strips_the_pre_p39_header_from_an_older_page() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        // Exactly what a page written before P3.9 looks like when it is
        // read back and re-written (an edit, a refresh, Mark reviewed).
        let path = write_page(
            &k,
            "legacy",
            "---\ntitle: Legacy\n---\n\n# Legacy\n\nDescription: the old topic line\n---\n\nFirst paragraph.\n\n---\n\nA rule the writer meant to keep.\n",
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (_, body) = parse_frontmatter(&raw);
        assert!(
            !body.contains("Description:"),
            "legacy Description line survived: {body}"
        );
        assert_eq!(
            body.matches("\n---\n").count(),
            1,
            "only the rule further down should remain: {body}"
        );
        assert!(body.contains("A rule the writer meant to keep."));
        assert!(body.contains("# Legacy"));
    }

    #[test]
    fn write_page_falls_back_to_stem_when_title_missing() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(
            &k,
            "dream-2026-05-11",
            "---\ntopic: KMS audit log\n---\nSome dream content.\n",
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (_, body) = parse_frontmatter(&raw);
        // No `title:` → fall back to the page stem verbatim.
        assert!(
            body.contains("# dream-2026-05-11"),
            "stem fallback missing: {body}"
        );
        assert!(body.contains("Some dream content."));
    }

    #[test]
    fn write_page_omits_description_when_topic_missing() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(&k, "bare", "Just body, no topic.\n").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (_, body) = parse_frontmatter(&raw);
        assert!(body.contains("# bare"));
        assert!(
            !body.contains("Description:"),
            "Description line should be omitted entirely when topic is missing; got: {body}"
        );
    }

    #[test]
    fn write_page_skips_injection_when_body_has_heading() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(
            &k,
            "intentional",
            "---\ntitle: A different title\ntopic: would-be description\n---\n# My Custom Heading\n\nbody\n",
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (_, body) = parse_frontmatter(&raw);
        // Model's heading is respected — neither title nor Description
        // line is injected when the body already opens with a `# heading`.
        assert!(body.contains("# My Custom Heading"));
        assert!(
            !body.contains("# A different title"),
            "should not have injected frontmatter title when body already had its own heading: {body}"
        );
        assert!(
            !body.contains("Description: would-be description"),
            "should not have injected Description when body already had its own heading: {body}"
        );
    }

    #[test]
    fn write_page_re_write_is_idempotent_on_canonical_pages() {
        // A page that's been through write_page once carries a
        // `# title` heading at the top of its body. Reading it back and
        // re-writing should not pile on a second copy of the header —
        // `body_has_leading_heading` detects the prior `# heading` and
        // skips re-injection.
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(
            &k,
            "tokens",
            "---\ntitle: Tokens\ntopic: jwt storage\n---\nBody\n",
        )
        .unwrap();
        let raw1 = std::fs::read_to_string(&path).unwrap();
        // Round-trip: re-write with the same content we just read.
        write_page(&k, "tokens", &raw1).unwrap();
        let raw2 = std::fs::read_to_string(&path).unwrap();
        let heading_count = raw2.matches("# Tokens").count();
        assert_eq!(
            heading_count, 1,
            "canonical heading should appear exactly once after a round-trip re-write; got {heading_count}:\n{raw2}"
        );
    }

    #[test]
    fn append_to_page_creates_then_appends_with_frontmatter_bump() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        // First call creates with bare body (no frontmatter).
        append_to_page(&k, "log-page", "first chunk\n").unwrap();
        // Now write a frontmatter version then append more.
        write_page(&k, "log-page", "---\ncategory: log\n---\noriginal\n").unwrap();
        append_to_page(&k, "log-page", "second chunk\n").unwrap();
        let path = k.pages_dir().join("log-page.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert_eq!(fm.get("category").map(String::as_str), Some("log"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("original"));
        assert!(body.contains("second chunk"));
    }

    #[test]
    fn writable_page_path_rejects_traversal_and_reserved() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        assert!(writable_page_path(&k, "../etc/passwd").is_err());
        assert!(writable_page_path(&k, "foo/bar").is_err());
        assert!(writable_page_path(&k, "").is_err());
        assert!(writable_page_path(&k, "index").is_err()); // reserved
        assert!(writable_page_path(&k, "log").is_err());
        assert!(writable_page_path(&k, "SCHEMA").is_err());
        assert!(writable_page_path(&k, "ok-page").is_ok());
    }

    // ─── M6.25: lint (BUG #3) ─────────────────────────────────────────────

    #[test]
    fn lint_finds_orphans_broken_links_and_missing_frontmatter() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Page A links to non-existent target → broken link.
        // Page B has no inbound links → orphan.
        // Page C has no frontmatter → flagged.
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink: [nope](pages/missing.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: y\n---\nIsland.\n",
        )
        .unwrap();
        std::fs::write(k.pages_dir().join("c.md"), "no frontmatter here\n").unwrap();

        let report = lint(&k).unwrap();
        assert!(report
            .broken_links
            .iter()
            .any(|(p, t)| p == "a" && t == "missing"));
        assert!(report.orphan_pages.contains(&"b".to_string()));
        assert!(report.missing_frontmatter.contains(&"c".to_string()));
        assert!(report.total_issues() >= 3);
    }

    #[test]
    fn lint_clean_kms_reports_no_issues() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink to [b](pages/b.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: x\n---\nLink to [a](pages/a.md)\n",
        )
        .unwrap();
        std::fs::write(k.index_path(), "- [a](pages/a.md)\n- [b](pages/b.md)\n").unwrap();
        let report = lint(&k).unwrap();
        assert_eq!(report.total_issues(), 0, "{report:?}");
    }

    // ─── M6.25: SCHEMA injection in system prompt (BUG #5) ────────────────

    #[test]
    fn system_prompt_includes_schema_when_present() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(
            k.schema_path(),
            "Pages must have category: in frontmatter.\n",
        )
        .unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.contains("### Schema"));
        assert!(out.contains("Pages must have category"));

        // dev-plan/64 P2.3: the schema every KMS is born with is not an
        // instruction from its owner, and is not worth 1.3 KB a request.
        let plain = create("plain", KmsScope::User).unwrap();
        assert_eq!(read_schema(&plain).trim(), SCHEMA_TEMPLATE.trim());
        let out = system_prompt_section(&["plain".into()]);
        assert!(!out.contains("### Schema"), "{out}");
        assert!(!out.contains("Canonical page shape"), "{out}");
        assert!(out.contains("KmsWrite")); // tool affordance listed
        assert!(out.contains("KmsAppend"));
    }

    /// M6.38.2 audit fix (Bug B): KmsDelete is registered alongside the
    /// other write tools when a KMS is active. Before this fix the system
    /// prompt's Tools block omitted KmsDelete — the model had access to
    /// the tool via the registry but no narrative context for when to use
    /// it. Now it's listed with a "last resort" hint to bias the model
    /// toward KmsWrite for merge/supersede flows.
    #[test]
    fn system_prompt_tools_block_includes_kms_delete() {
        let _home = scoped_home();
        let _k = create("nb", KmsScope::User).unwrap();
        let out = system_prompt_section(&["nb".into()]);
        // Audit finding B: tools block is now globalised as a
        // top-level `## KMS tools` h2 instead of a per-KMS `### Tools`
        // h3 subsection. The substantive assertions (every tool
        // listed + "last resort" framing) are unchanged.
        assert!(out.contains("KmsRead"));
        assert!(out.contains("KmsSearch"));
        assert!(out.contains("KmsWrite"));
        assert!(out.contains("KmsAppend"));
        assert!(
            out.contains("KmsDelete"),
            "Tools block should list KmsDelete (M6.38.2 fix). Got:\n{out}"
        );
        // The "last resort" framing biases the model away from default
        // deletion behavior — locks the prompt's stance.
        assert!(
            out.contains("last resort") || out.contains("prefer `KmsWrite`"),
            "KmsDelete entry should bias model toward KmsWrite for merges. Got:\n{out}"
        );
    }

    // ─── M6.25: categorized index (BUG #6) ────────────────────────────────

    #[test]
    fn system_prompt_categorizes_index_by_frontmatter() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(
            k.pages_dir().join("paper-a.md"),
            "---\ncategory: research\n---\n# Paper A\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("api-x.md"),
            "---\ncategory: api\n---\n# API X\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("paper-b.md"),
            "---\ncategory: research\n---\n# Paper B\n",
        )
        .unwrap();
        // dev-plan/64: the prompt names the categories and the pages; the
        // categorised list itself moved behind `KmsRead(kind: "index")`.
        let out = system_prompt_section(&["nb".into()]);
        assert!(
            out.contains("Categories: api, research"),
            "categories must still reach the model: {out}"
        );
        assert!(out.contains("paper-a"));
        assert!(out.contains("paper-b"));
        assert!(out.contains("api-x"));
        assert!(
            !out.contains("**research**"),
            "sections are on demand: {out}"
        );

        let full = full_index(&k);
        assert!(
            full.contains("**research**"),
            "missing research section: {full}"
        );
        assert!(full.contains("**api**"), "missing api section: {full}");
    }

    // ─── M6.25: re-ingest cascade (BUG #10) ───────────────────────────────

    #[test]
    fn reingest_marks_dependent_pages_stale() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Ingest source `topic`.
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("topic.md");
        std::fs::write(&src, "v1").unwrap();
        ingest(&k, &src, Some("topic"), false).unwrap();

        // Write a derived page that mentions `topic` in `sources:`.
        write_page(
            &k,
            "summary",
            "---\ncategory: synthesis\nsources: topic\n---\n# Summary\n",
        )
        .unwrap();

        // Re-ingest topic with --force → cascade fires.
        std::fs::write(&src, "v2").unwrap();
        let r = ingest(&k, &src, Some("topic"), true).unwrap();
        assert_eq!(r.cascaded, 1, "expected 1 dependent page marked stale");

        let derived = std::fs::read_to_string(k.pages_dir().join("summary.md")).unwrap();
        assert!(derived.contains("STALE"), "stale marker missing: {derived}");
        assert!(derived.contains("source `topic`"));
    }

    // ─── manifest + schema-aware lint ─────────────────────────────────────

    #[test]
    fn create_seeds_manifest_with_empty_required() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let manifest = k.read_manifest().expect("manifest seeded by create()");
        assert_eq!(manifest.schema_version, KMS_SCHEMA_VERSION);
        assert!(
            manifest.frontmatter_required.is_empty(),
            "starter manifest must not enforce policy by default"
        );
    }

    #[test]
    fn read_manifest_returns_none_for_legacy_kms() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        assert!(k.read_manifest().is_none());
    }

    #[test]
    fn read_manifest_returns_none_for_malformed_json() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.manifest_path(), "{ this is not json").unwrap();
        assert!(k.read_manifest().is_none());
    }

    #[test]
    fn read_manifest_round_trips_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["category".into(), "tags".into()]);
        required.insert("research".into(), vec!["sources".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
            entry: None,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        let read = k.read_manifest().unwrap();
        assert_eq!(read.schema_version, "1.0");
        assert_eq!(
            read.frontmatter_required.get("global").unwrap(),
            &vec!["category".to_string(), "tags".to_string()]
        );
        assert_eq!(
            read.frontmatter_required.get("research").unwrap(),
            &vec!["sources".to_string()]
        );
    }

    #[test]
    fn lint_skips_required_check_when_manifest_has_empty_map() {
        // The starter manifest is present but enforcement is empty — must
        // behave identically to legacy KMSes for required-field reporting.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "---\ncategory: x\n---\nbody\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(report.missing_required_fields.is_empty());
    }

    #[test]
    fn lint_skips_required_check_when_manifest_absent() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "---\ncategory: x\n---\nbody\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(report.missing_required_fields.is_empty());
    }

    #[test]
    fn lint_finds_missing_global_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["category".into(), "tags".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
            entry: None,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "---\ncategory: x\n---\nbody\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(
            report
                .missing_required_fields
                .iter()
                .any(|(p, src, f)| p == "a" && src == "global" && f == "tags"),
            "expected missing 'tags' on page 'a': {:?}",
            report.missing_required_fields
        );
        // 'category' is present on the page so must NOT appear.
        assert!(!report
            .missing_required_fields
            .iter()
            .any(|(_, _, f)| f == "category"));
    }

    #[test]
    fn lint_finds_missing_per_category_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("research".into(), vec!["sources".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
            entry: None,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        // Research page without `sources:` → flagged.
        std::fs::write(
            k.pages_dir().join("paper.md"),
            "---\ncategory: research\n---\nbody\n",
        )
        .unwrap();
        // Non-research page without `sources:` → NOT flagged (rule is
        // category-scoped, not global).
        std::fs::write(
            k.pages_dir().join("note.md"),
            "---\ncategory: misc\n---\nbody\n",
        )
        .unwrap();
        let report = lint(&k).unwrap();
        assert!(
            report
                .missing_required_fields
                .iter()
                .any(|(p, src, f)| p == "paper" && src == "research" && f == "sources"),
            "expected research/sources flag on 'paper': {:?}",
            report.missing_required_fields
        );
        assert!(!report
            .missing_required_fields
            .iter()
            .any(|(p, _, _)| p == "note"));
    }

    #[test]
    fn lint_skips_required_check_for_pages_with_no_frontmatter() {
        // A page with no `---` block is already flagged via
        // `missing_frontmatter`. Don't double-report by also emitting
        // every required field as missing — the user fixes the
        // frontmatter once and both classes resolve.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["category".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
            entry: None,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        std::fs::write(k.pages_dir().join("bare.md"), "no frontmatter\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(report.missing_frontmatter.contains(&"bare".to_string()));
        assert!(report.missing_required_fields.is_empty());
    }

    #[test]
    fn scan_stale_markers_finds_cascade_output() {
        // End-to-end: ingest a source, write a derived page that references
        // it, re-ingest with --force to trigger the cascade, then verify
        // scan_stale_markers picks up exactly what mark_dependent_pages_stale
        // wrote. Locks the producer/consumer marker contract.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("topic.md");
        std::fs::write(&src, "v1").unwrap();
        ingest(&k, &src, Some("topic"), false).unwrap();
        write_page(
            &k,
            "summary",
            "---\ncategory: synthesis\nsources: topic\n---\n# Summary\n",
        )
        .unwrap();
        std::fs::write(&src, "v2").unwrap();
        ingest(&k, &src, Some("topic"), true).unwrap();

        let stale = scan_stale_markers(&k).unwrap();
        assert_eq!(stale.len(), 1, "expected 1 stale marker: {stale:?}");
        assert_eq!(stale[0].page_stem, "summary");
        assert_eq!(stale[0].source_alias, "topic");
        assert!(!stale[0].date.is_empty(), "date must be captured");
    }

    #[test]
    fn scan_stale_markers_returns_empty_when_no_markers() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("clean.md"),
            "---\ncategory: x\n---\nNo markers here.\n",
        )
        .unwrap();
        assert!(scan_stale_markers(&k).unwrap().is_empty());
    }

    #[test]
    fn scan_stale_markers_collects_multiple_per_page() {
        // A page that has been left stale across two re-ingest waves
        // should surface both markers — refresh debt accumulates.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("debt.md"),
            "---\ncategory: synthesis\n---\nbody\n\n\
             > ⚠ STALE: source `alpha` was re-ingested on 2026-01-01. Refresh this page.\n\
             > ⚠ STALE: source `beta` was re-ingested on 2026-02-15. Refresh this page.\n",
        )
        .unwrap();
        let stale = scan_stale_markers(&k).unwrap();
        assert_eq!(stale.len(), 2);
        // Sorted by (stem, alias, date) — alpha before beta.
        assert_eq!(stale[0].source_alias, "alpha");
        assert_eq!(stale[0].date, "2026-01-01");
        assert_eq!(stale[1].source_alias, "beta");
        assert_eq!(stale[1].date, "2026-02-15");
    }

    // ─── schema migrations ────────────────────────────────────────────────

    #[test]
    fn detect_schema_version_returns_legacy_when_manifest_absent() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        assert_eq!(detect_schema_version(&k), LEGACY_SCHEMA_VERSION);
    }

    #[test]
    fn detect_schema_version_returns_legacy_when_version_field_empty() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Manifest exists but schema_version is empty — same legacy treatment.
        std::fs::write(
            k.manifest_path(),
            r#"{"schema_version": "", "frontmatter_required": {}}"#,
        )
        .unwrap();
        assert_eq!(detect_schema_version(&k), LEGACY_SCHEMA_VERSION);
    }

    #[test]
    fn detect_schema_version_reads_explicit_version() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Default seed is "1.0".
        assert_eq!(detect_schema_version(&k), "1.0");
    }

    #[test]
    fn migrate_is_noop_when_already_at_latest() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let report = migrate(&k, false).unwrap();
        assert_eq!(report.current_version, "1.0");
        assert_eq!(report.target_version, "1.0");
        assert!(report.steps.is_empty());
    }

    #[test]
    fn migrate_dry_run_writes_no_files_for_legacy_kms() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        let log_before = std::fs::read_to_string(k.log_path()).unwrap();

        let report = migrate(&k, true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.current_version, LEGACY_SCHEMA_VERSION);
        assert_eq!(report.target_version, "1.0");
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].from, LEGACY_SCHEMA_VERSION);
        assert_eq!(report.steps[0].to, "1.0");

        // No filesystem changes.
        assert!(!k.manifest_path().exists(), "dry-run wrote manifest");
        let log_after = std::fs::read_to_string(k.log_path()).unwrap();
        assert_eq!(log_before, log_after, "dry-run touched log.md");
    }

    #[test]
    fn migrate_apply_writes_manifest_for_legacy_kms() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        assert!(!k.manifest_path().exists());

        let report = migrate(&k, false).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.steps.len(), 1);

        // Manifest now exists at v1.0 with empty enforcement.
        let manifest = k.read_manifest().expect("manifest written");
        assert_eq!(manifest.schema_version, "1.0");
        assert!(manifest.frontmatter_required.is_empty());

        // Log entry was appended.
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(
            log.contains("migrated | 0.x → 1.0"),
            "log missing migration entry: {log}"
        );

        // Idempotent: a second migrate is a no-op.
        let report2 = migrate(&k, false).unwrap();
        assert!(report2.steps.is_empty());
        assert_eq!(report2.current_version, "1.0");
    }

    #[test]
    fn migrate_preserves_existing_pages() {
        // Migration must not touch page bodies — only the manifest changes.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        let page_path = k.pages_dir().join("preserve.md");
        let original = "---\ncategory: x\n---\nimportant content\n";
        std::fs::write(&page_path, original).unwrap();

        migrate(&k, false).unwrap();

        let after = std::fs::read_to_string(&page_path).unwrap();
        assert_eq!(after, original, "page body modified by migration");
    }

    #[test]
    fn migrate_errors_on_unknown_schema_version() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Plant a manifest with a version that has no migration path.
        std::fs::write(
            k.manifest_path(),
            r#"{"schema_version": "99.0", "frontmatter_required": {}}"#,
        )
        .unwrap();
        let err = migrate(&k, false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("no migration path") && msg.contains("99.0"),
            "expected unknown-version error: {msg}"
        );
    }

    #[test]
    fn lint_total_issues_includes_missing_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["tags".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
            entry: None,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        // Self-linked pages so we don't trip orphan/broken-link checks.
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink to [b](pages/b.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: x\n---\nLink to [a](pages/a.md)\n",
        )
        .unwrap();
        std::fs::write(k.index_path(), "- [a](pages/a.md)\n- [b](pages/b.md)\n").unwrap();
        let report = lint(&k).unwrap();
        // Both pages missing 'tags' → 2 missing-required-field issues.
        assert_eq!(report.missing_required_fields.len(), 2);
        assert_eq!(report.total_issues(), 2);
    }

    #[test]
    fn list_runs_reads_the_ledger_out_of_each_log_newest_first() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let runs = k.root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(
            runs.join("2026-09-19-jevons.md"),
            "---\ntype: research-run\nquery: \"ปฏิทรรศน์เจวอนส์\"\ndate: 2026-09-19\nelapsed_secs: 384\nclaims: 70\nllm_calls: 2\ncost_usd: 0.0114\n---\n\n# Research run\n",
        )
        .unwrap();
        std::fs::write(
            runs.join("2026-09-20-verify.md"),
            "---\ntype: verify-run\ndate: 2026-09-20\nfindings: 41\nllm_calls: 39\ncost_usd: 0.2100\n---\n\n```\nreport\n```\n",
        )
        .unwrap();
        std::fs::write(
            runs.join("2026-09-20-verify-2.md"),
            "---\ntype: verify-run\ndate: 2026-09-20\nfindings: 3\n---\n\n```\nreport\n```\n",
        )
        .unwrap();
        // Not a run log; must not appear.
        std::fs::write(runs.join("notes.txt"), "scratch").unwrap();

        let out = list_runs(&k);
        let names: Vec<&str> = out.iter().map(|r| r.name.as_str()).collect();
        // Newest date first; same-day runs in the order they were numbered.
        assert_eq!(
            names,
            vec![
                "2026-09-20-verify-2",
                "2026-09-20-verify",
                "2026-09-19-jevons"
            ]
        );
        assert_eq!(out[0].kind, "verify");
        assert_eq!(out[0].findings, Some(3));
        assert_eq!(out[0].cost_usd, None, "a log with no cost says so");
        assert_eq!(out[1].cost_usd, Some(0.21));
        let research = &out[2];
        assert_eq!(research.kind, "research");
        assert_eq!(research.title, "ปฏิทรรศน์เจวอนส์", "query, unquoted");
        assert_eq!(research.claims, Some(70));
        assert_eq!(research.elapsed_secs, Some(384));
        assert_eq!(research.cost_usd, Some(0.0114));
        assert!(research.bytes > 0);
    }

    #[test]
    fn cost_by_mode_prices_a_click_from_runs_of_the_same_kind() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let runs = k.root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let log = |name: &str, body: &str| {
            std::fs::write(runs.join(format!("{name}.md")), body).unwrap();
        };
        // Three refreshes: 1¢, 2¢ and one wild 90¢ outlier.
        log(
            "2026-09-18-refresh-a",
            "---\ntype: research-run\nmode: refresh\ndate: 2026-09-18\nelapsed_secs: 60\ncost_usd: 0.0100\n---\n",
        );
        log(
            "2026-09-19-refresh-b",
            "---\ntype: research-run\nmode: refresh\ndate: 2026-09-19\nelapsed_secs: 120\ncost_usd: 0.0200\n---\n",
        );
        log(
            "2026-09-20-refresh-c",
            "---\ntype: research-run\nmode: refresh\ndate: 2026-09-20\nelapsed_secs: 2400\ncost_usd: 0.9000\n---\n",
        );
        log(
            "2026-09-20-topic",
            "---\ntype: research-run\nmode: research\ndate: 2026-09-20\nelapsed_secs: 400\ncost_usd: 0.0500\n---\n",
        );
        // Written before P5.4 stamped `mode:` — the name is all it has.
        log(
            "2026-09-17-refresh-old",
            "---\ntype: research-run\ndate: 2026-09-17\ncost_usd: 0.0300\n---\n",
        );

        let by = cost_by_mode(&k);
        let refresh = by.get("refresh").expect("refresh mode");
        assert_eq!(refresh.runs, 4, "the unstamped log is a refresh too");
        // The median, not the mean: one 90¢ run must not make every
        // later click look expensive. Mean here would be 24.75¢.
        assert_eq!(refresh.cost_usd, Some(0.03));
        assert_eq!(by.get("research").map(|m| m.runs), Some(1));
        assert_eq!(by.get("research").and_then(|m| m.cost_usd), Some(0.05));
        // A kind nobody has run here has no entry, so the GUI says
        // "not known" instead of borrowing another kind's number.
        assert!(by.get("selection").is_none());
    }

    /// A URL that serves a paper is a paper. Before this, `resp.text()`
    /// ran over the bytes and archived lossy mush — and the GUI's URL
    /// field made that the commonest thing a researcher would paste.
    /// dev-plan/64 P4.9. The mode decides whether a page gets claims,
    /// quotes and citations or none of them, so an unknown value must
    /// land somewhere safe and predictable rather than on the most
    /// expensive option — and the cheap paths must not run the
    /// pipeline by accident.
    #[test]
    fn an_ingest_mode_that_is_not_recognised_falls_back_to_the_old_behaviour() {
        use IngestMode::*;
        // What every caller written before the choice existed sends.
        assert_eq!(IngestMode::parse(""), Summary);
        assert_eq!(IngestMode::parse("nonsense"), Summary);
        assert_eq!(IngestMode::parse("SUMMARY"), Summary);
        assert_eq!(IngestMode::parse(" Cited "), Cited);
        assert_eq!(IngestMode::parse("archive"), Archive);
        assert_eq!(IngestMode::parse("atomic"), Atomic);

        // Only the two expensive ones reach the research pipeline.
        assert!(!Archive.is_research());
        assert!(!Summary.is_research());
        assert!(Cited.is_research());
        assert!(Atomic.is_research());

        // `cited` is one page; `atomic` keeps the run's own ceiling.
        assert_eq!(Cited.research_max_notes(), Some(1));
        assert_eq!(Atomic.research_max_notes(), None);

        // Round-trips, since the mode rides an IPC envelope as a string.
        for m in [Archive, Summary, Cited, Atomic] {
            assert_eq!(IngestMode::parse(m.as_str()), m);
        }
    }

    #[test]
    fn a_pdf_over_http_is_recognised_however_the_server_labels_it() {
        let pdf = b"%PDF-1.7\n1 0 obj\n";
        let html = b"<!doctype html><html><body>hi</body></html>";
        // The header, when it says something.
        assert!(looks_like_pdf("application/pdf", html));
        assert!(looks_like_pdf("application/x-pdf", b""));
        // The bytes, when it does not — the shape that made this a bug.
        assert!(looks_like_pdf("application/octet-stream", pdf));
        assert!(looks_like_pdf("", pdf));
        // And neither for an ordinary page.
        assert!(!looks_like_pdf("text/html", html));
        assert!(!looks_like_pdf("", html));
        // A page that merely mentions the word is not one.
        assert!(!looks_like_pdf(
            "text/html",
            b"<p>download the pdf here</p>"
        ));
    }

    #[test]
    fn read_browse_file_opens_a_run_log_but_not_a_traversal() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let runs = k.root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(runs.join("2026-09-20-verify.md"), "the report").unwrap();
        let read = read_browse_file("nb", "run", "2026-09-20-verify").unwrap();
        assert_eq!(read.content, "the report");
        // A page of the same stem is a different file: the folder is
        // chosen by `kind`, not guessed.
        assert!(read_browse_file("nb", "page", "2026-09-20-verify").is_err());
        assert!(read_browse_file("nb", "run", "../pages/x").is_err());
    }

    #[test]
    fn read_browse_file_passes_through_small_files() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let sources = k.root.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(sources.join("note.md"), "hello world").unwrap();
        let read = read_browse_file("nb", "source", "note").unwrap();
        assert!(!read.truncated);
        assert_eq!(read.content, "hello world");
        assert_eq!(read.total_bytes, "hello world".len() as u64);
    }

    #[test]
    fn merge_into_copies_disjoint_pages_and_sources() {
        let _home = scoped_home();
        let src = create("alpha", KmsScope::Project).unwrap();
        let dst = create("beta", KmsScope::Project).unwrap();
        // src: page `a.md`, source `s1.md`. dst empty.
        std::fs::write(src.pages_dir().join("a.md"), "# A\n").unwrap();
        let src_sources = src.root.join("sources");
        std::fs::create_dir_all(&src_sources).unwrap();
        std::fs::write(src_sources.join("s1.md"), "src content").unwrap();
        std::fs::write(src.index_path(), "- [a](pages/a.md)\n").unwrap();

        let report = merge_into("alpha", "beta").unwrap();
        assert_eq!(report.pages_copied, 1);
        assert_eq!(report.pages_renamed, 0);
        assert_eq!(report.sources_copied, 1);
        assert_eq!(report.sources_renamed, 0);
        assert_eq!(report.index_entries_added, 1);

        // dst has the copied files.
        assert!(dst.pages_dir().join("a.md").exists());
        assert!(dst.root.join("sources/s1.md").exists());
        // src is untouched.
        assert!(src.pages_dir().join("a.md").exists());
    }

    #[test]
    fn consolidate_merges_all_writable_into_a_new_dst() {
        let _home = scoped_home();
        let a = create("alpha", KmsScope::Project).unwrap();
        let b = create("beta", KmsScope::Project).unwrap();
        std::fs::write(a.pages_dir().join("a.md"), "# A\n").unwrap();
        std::fs::write(a.index_path(), "- [a](pages/a.md)\n").unwrap();
        std::fs::write(b.pages_dir().join("b.md"), "# B\n").unwrap();
        std::fs::write(b.index_path(), "- [b](pages/b.md)\n").unwrap();

        let report = consolidate("master", KmsScope::Project, false).unwrap();
        assert!(report.created_dst);
        assert_eq!(report.merged.len(), 2);
        assert!(report.dropped.is_empty(), "no --drop keeps sources");
        let master = resolve("master").unwrap();
        assert!(master.pages_dir().join("a.md").exists());
        assert!(master.pages_dir().join("b.md").exists());
        assert!(resolve("alpha").is_some() && resolve("beta").is_some());
    }

    #[test]
    fn consolidate_with_drop_leaves_only_dst() {
        let _home = scoped_home();
        let a = create("alpha", KmsScope::Project).unwrap();
        std::fs::write(a.pages_dir().join("a.md"), "# A\n").unwrap();
        std::fs::write(a.index_path(), "- [a](pages/a.md)\n").unwrap();
        create("master", KmsScope::Project).unwrap();

        let report = consolidate("master", KmsScope::Project, true).unwrap();
        assert!(!report.created_dst);
        assert_eq!(report.merged.len(), 1);
        assert_eq!(report.dropped, vec!["alpha".to_string()]);
        assert!(resolve("alpha").is_none(), "source dropped");
        assert!(resolve("master").unwrap().pages_dir().join("a.md").exists());
    }

    #[test]
    fn merge_into_renames_on_collision_and_rewrites_links() {
        let _home = scoped_home();
        let src = create("alpha", KmsScope::Project).unwrap();
        let dst = create("beta", KmsScope::Project).unwrap();
        // dst already has `a.md`; src has `a.md` (collision) + `b.md`
        // which links to `a` via both relative md and wikilink syntax.
        std::fs::write(dst.pages_dir().join("a.md"), "destination a\n").unwrap();
        std::fs::write(src.pages_dir().join("a.md"), "source a\n").unwrap();
        std::fs::write(
            src.pages_dir().join("b.md"),
            "See [a](pages/a.md) and [[a]] and [[a|the a page]].\n",
        )
        .unwrap();
        std::fs::write(src.index_path(), "- [a](pages/a.md)\n- [b](pages/b.md)\n").unwrap();

        let report = merge_into("alpha", "beta").unwrap();
        assert_eq!(report.pages_copied, 1, "b should land as `b.md`");
        assert_eq!(report.pages_renamed, 1, "a should be renamed");

        // dst's original `a.md` is intact.
        let dst_a = std::fs::read_to_string(dst.pages_dir().join("a.md")).unwrap();
        assert_eq!(dst_a, "destination a\n");
        // Incoming `a` landed as `a-from-alpha.md`.
        let copied_a = std::fs::read_to_string(dst.pages_dir().join("a-from-alpha.md")).unwrap();
        assert_eq!(copied_a, "source a\n");
        // `b.md` got copied and its links to `a` were rewritten to
        // point at the renamed file.
        let copied_b = std::fs::read_to_string(dst.pages_dir().join("b.md")).unwrap();
        assert!(copied_b.contains("pages/a-from-alpha.md"));
        assert!(copied_b.contains("[[a-from-alpha]]"));
        assert!(copied_b.contains("[[a-from-alpha|the a page]]"));
        // Index entries from src got merged with the link rewrite.
        let dst_index = dst.read_index();
        assert!(dst_index.contains("(pages/a-from-alpha.md)"));
        assert!(dst_index.contains("(pages/b.md)"));
    }

    #[test]
    fn merge_into_combines_aggregator_pages_instead_of_renaming() {
        let _home = scoped_home();
        let src = create("alpha", KmsScope::Project).unwrap();
        let dst = create("beta", KmsScope::Project).unwrap();
        // Both KMSes have a `_summary.md`. Without the aggregator rule
        // we'd end up with `_summary.md` and `_summary-from-alpha.md`,
        // defeating the file's purpose.
        std::fs::write(
            dst.pages_dir().join("_summary.md"),
            "---\ncategory: meta\n---\n# Summary\n- dst point one\n",
        )
        .unwrap();
        std::fs::write(
            src.pages_dir().join("_summary.md"),
            "---\ncategory: meta\n---\n- src point one\n- src point two\n",
        )
        .unwrap();

        let report = merge_into("alpha", "beta").unwrap();
        assert_eq!(report.pages_combined, 1);
        assert_eq!(report.pages_renamed, 0);
        assert_eq!(report.combined, vec!["_summary".to_string()]);
        // The renamed sibling must NOT exist.
        assert!(!dst.pages_dir().join("_summary-from-alpha.md").exists());

        let combined = std::fs::read_to_string(dst.pages_dir().join("_summary.md")).unwrap();
        // dst frontmatter preserved.
        assert!(combined.starts_with("---\ncategory: meta\n---"));
        // dst body preserved.
        assert!(combined.contains("- dst point one"));
        // Provenance marker present.
        assert!(combined.contains("<!-- merged from alpha on "));
        // src body appended (frontmatter stripped).
        assert!(combined.contains("- src point one"));
        assert!(combined.contains("- src point two"));
        assert!(!combined.contains("category: meta\n---\n- src"));
    }

    #[test]
    fn merge_into_aggregator_with_no_collision_just_copies() {
        let _home = scoped_home();
        let src = create("alpha", KmsScope::Project).unwrap();
        let _dst = create("beta", KmsScope::Project).unwrap();
        // dst does NOT have `_summary.md`; src does.
        std::fs::write(src.pages_dir().join("_summary.md"), "src only\n").unwrap();
        let report = merge_into("alpha", "beta").unwrap();
        // Plain copy — no combine, no rename.
        assert_eq!(report.pages_copied, 1);
        assert_eq!(report.pages_combined, 0);
        assert_eq!(report.pages_renamed, 0);
    }

    #[test]
    fn merge_into_rejects_self_merge() {
        let _home = scoped_home();
        let _ = create("nb", KmsScope::Project).unwrap();
        let err = merge_into("nb", "nb").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("itself"),
            "expected self-merge error, got: {msg}"
        );
    }

    #[test]
    fn auto_link_inserts_first_mention_dry_run_by_default() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("postgresql.md"), "stub").unwrap();
        std::fs::write(
            k.pages_dir().join("indexing.md"),
            "We talk a lot about PostgreSQL here. PostgreSQL is great.\n",
        )
        .unwrap();
        let report = auto_link(&k, AutoLinkOptions::default()).unwrap();
        assert_eq!(report.pages_scanned, 2);
        assert_eq!(report.pages_modified, 1);
        assert_eq!(report.links_added, 1, "first occurrence only");
        // Dry-run — file unchanged on disk.
        let on_disk = std::fs::read_to_string(k.pages_dir().join("indexing.md")).unwrap();
        assert!(!on_disk.contains("[[postgresql]]"));
    }

    #[test]
    fn auto_link_apply_writes_changes_and_preserves_frontmatter() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("postgresql.md"), "stub").unwrap();
        std::fs::write(
            k.pages_dir().join("indexing.md"),
            "---\ncategory: db\n---\nPostgreSQL is great.\n",
        )
        .unwrap();
        let opts = AutoLinkOptions {
            apply: true,
            ..AutoLinkOptions::default()
        };
        let report = auto_link(&k, opts).unwrap();
        assert_eq!(report.links_added, 1);
        let on_disk = std::fs::read_to_string(k.pages_dir().join("indexing.md")).unwrap();
        assert!(on_disk.starts_with("---\ncategory: db\n---\n"));
        assert!(
            on_disk.contains("[[postgresql|PostgreSQL]]"),
            "display text preserved: {on_disk}"
        );
    }

    #[test]
    fn auto_link_skips_code_fences_headings_existing_links_and_inline_code() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("postgresql.md"), "stub").unwrap();
        let body = "\
# PostgreSQL is in a heading and must not link
Mention 1: PostgreSQL here should NOT link because of the heading rule\n\
just kidding — first prose mention: PostgreSQL.\n\
Already linked: [[postgresql]] and [text](pages/postgresql.md).\n\
```\ncode block PostgreSQL inside fence\n```\n\
Inline `PostgreSQL` in code span.\n\
";
        std::fs::write(k.pages_dir().join("notes.md"), body).unwrap();
        let opts = AutoLinkOptions {
            apply: true,
            ..AutoLinkOptions::default()
        };
        let report = auto_link(&k, opts).unwrap();
        // One link inserted — the first prose mention. Heading,
        // existing wikilink, md-link, fenced code, and inline code
        // span are all skipped.
        assert_eq!(report.links_added, 1);
        let on_disk = std::fs::read_to_string(k.pages_dir().join("notes.md")).unwrap();
        // The heading line is intact.
        assert!(on_disk.contains("# PostgreSQL is in a heading"));
        // Code-fence block intact.
        assert!(on_disk.contains("code block PostgreSQL inside fence"));
        // Inline code intact.
        assert!(on_disk.contains("Inline `PostgreSQL` in code span."));
        // First prose mention got linked.
        let linked_count = on_disk.matches("[[postgresql").count();
        // Original body already had ONE [[postgresql]], plus the one we add.
        assert_eq!(linked_count, 2, "{on_disk}");
    }

    #[test]
    fn auto_link_never_links_a_page_to_itself() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("postgresql.md"),
            "PostgreSQL talks about PostgreSQL.\n",
        )
        .unwrap();
        let opts = AutoLinkOptions {
            apply: true,
            ..AutoLinkOptions::default()
        };
        let report = auto_link(&k, opts).unwrap();
        assert_eq!(report.links_added, 0);
        let on_disk = std::fs::read_to_string(k.pages_dir().join("postgresql.md")).unwrap();
        assert!(!on_disk.contains("[[postgresql]]"));
    }

    /// A locally ingested document's alias is also a page name, and its
    /// Sources line ends in `kms://<kms>/sources/<alias>`. The URL guard
    /// knew `https?://` only, so the linker rewrote the tail of that
    /// address into a wikilink on 25 of 39 pages of a real vault — the
    /// same defect dev-plan/58 fixed once, reopened by a new scheme.
    #[test]
    fn auto_link_leaves_a_kms_url_alone_even_when_the_kms_name_has_spaces() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("my-doc.md"),
            "---\ntitle: My Doc\n---\nstub\n",
        )
        .unwrap();
        let line = "1. [T](../sources/my-doc.md) — kms://Age of Abundance/sources/my-doc\n";
        std::fs::write(
            k.pages_dir().join("note.md"),
            format!("This note rests on one source.\n\n## Sources\n\n{line}"),
        )
        .unwrap();
        let opts = AutoLinkOptions {
            apply: true,
            ..AutoLinkOptions::default()
        };
        auto_link(&k, opts).unwrap();
        let on_disk = std::fs::read_to_string(k.pages_dir().join("note.md")).unwrap();
        assert!(
            on_disk.contains(line),
            "the address was rewritten:\n{on_disk}"
        );
    }

    #[test]
    fn auto_link_picks_up_frontmatter_title_and_aliases() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("pg.md"),
            "---\ntitle: PostgreSQL\naliases: postgres, psql, pgsql\n---\nstub\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("note.md"),
            "today I used postgres at work.\n",
        )
        .unwrap();
        let opts = AutoLinkOptions {
            apply: true,
            ..AutoLinkOptions::default()
        };
        let report = auto_link(&k, opts).unwrap();
        assert_eq!(report.links_added, 1);
        let on_disk = std::fs::read_to_string(k.pages_dir().join("note.md")).unwrap();
        assert!(on_disk.contains("[[pg|postgres]]"), "{on_disk}");
    }

    #[test]
    fn auto_link_links_every_bold_mention_and_keys_ending_in_punctuation() {
        let _home = scoped_home();
        let k = create("bold-kms", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("alibaba.md"),
            "---\ntitle: Alibaba (Qwen)\n---\nstub\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("topic.md"),
            "Big tech like Alibaba lead.\n\n- **Alibaba (Qwen)** publishes the most.\n- **Alibaba (Qwen)** again.\n",
        )
        .unwrap();
        let opts = AutoLinkOptions {
            apply: true,
            ..AutoLinkOptions::default()
        };
        let report = auto_link(&k, opts).unwrap();
        let on_disk = std::fs::read_to_string(k.pages_dir().join("topic.md")).unwrap();
        assert_eq!(
            on_disk.matches("[[alibaba|Alibaba (Qwen)]]").count(),
            2,
            "both bold mentions, bold dropped: {on_disk}"
        );
        assert!(
            on_disk.contains("like Alibaba lead"),
            "plain mention untouched once a bold one is linked: {on_disk}"
        );
        assert!(report.links_added >= 2);
    }

    #[test]
    fn parse_llm_link_response_accepts_bare_json() {
        let raw = r#"{"links":[{"anchor":"PostgreSQL","target_slug":"postgresql"}]}"#;
        let out = parse_llm_link_response(raw).unwrap();
        assert_eq!(
            out,
            vec![("PostgreSQL".to_string(), "postgresql".to_string())]
        );
    }

    #[test]
    fn parse_llm_link_response_strips_code_fences() {
        let raw =
            "```json\n{\"links\":[{\"anchor\":\"db indexing\",\"target_slug\":\"indexing\"}]}\n```";
        let out = parse_llm_link_response(raw).unwrap();
        assert_eq!(
            out,
            vec![("db indexing".to_string(), "indexing".to_string())]
        );
    }

    #[test]
    fn parse_llm_link_response_tolerates_leading_prose() {
        // Some models prepend "Here is the JSON:" or similar despite
        // the prompt instructing them not to. We grab from first `{`
        // to last `}` so this still works.
        let raw = "Here you go:\n{\"links\":[]}\n— done.";
        let out = parse_llm_link_response(raw).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn parse_llm_link_response_errors_on_no_json() {
        let err = parse_llm_link_response("nothing here").unwrap_err();
        assert!(format!("{err}").contains("no JSON"));
    }

    #[test]
    fn apply_llm_links_validates_and_inserts_first_occurrence() {
        let mut valid = std::collections::HashSet::new();
        valid.insert("postgres".to_string());
        valid.insert("indexing".to_string());
        let body = "We use PostgreSQL daily. PostgreSQL is great. We also love PostgreSQL.\n";
        let candidates = vec![
            ("PostgreSQL".to_string(), "postgres".to_string()),
            ("PostgreSQL".to_string(), "postgres".to_string()), // duplicate target — must be ignored
        ];
        let (out, hits) = apply_llm_links(body, &candidates, "self", &valid);
        // First-occurrence policy: only ONE link inserted.
        assert_eq!(hits.len(), 1);
        assert_eq!(out.matches("[[postgres|PostgreSQL]]").count(), 1);
        // The other two mentions of PostgreSQL remain unlinked.
        assert!(out.contains("PostgreSQL is great"));
        assert!(out.contains("We also love PostgreSQL"));
    }

    #[test]
    fn apply_llm_links_drops_unknown_targets_and_self_refs() {
        let mut valid = std::collections::HashSet::new();
        valid.insert("postgres".to_string());
        let body = "a self ref to me and a bogus link to nothing.\n";
        let candidates = vec![
            ("self".to_string(), "self".to_string()), // self-reference
            ("nothing".to_string(), "nope".to_string()), // target not in valid_slugs
        ];
        let (out, hits) = apply_llm_links(body, &candidates, "self", &valid);
        assert!(hits.is_empty());
        assert_eq!(out, body, "body must be unchanged when nothing applies");
    }

    #[test]
    fn apply_llm_links_skips_anchors_inside_code_fences_and_headings() {
        let mut valid = std::collections::HashSet::new();
        valid.insert("postgres".to_string());
        let body = "# PostgreSQL heading\n\
                    Inline `PostgreSQL` is code.\n\
                    Existing [[postgres|pg]] wikilink.\n\
                    Body mention PostgreSQL here.\n\
                    ```\n\
                    PostgreSQL in code fence.\n\
                    ```\n";
        let candidates = vec![("PostgreSQL".to_string(), "postgres".to_string())];
        let (out, hits) = apply_llm_links(body, &candidates, "self", &valid);
        // The first acceptable occurrence is the prose line.
        assert_eq!(hits.len(), 1);
        assert!(out.contains("Body mention [[postgres|PostgreSQL]] here."));
        // Heading + code-fence + inline-code + existing-wikilink are untouched.
        assert!(out.contains("# PostgreSQL heading"));
        assert!(out.contains("Inline `PostgreSQL` is code."));
        assert!(out.contains("PostgreSQL in code fence."));
        assert!(out.contains("[[postgres|pg]]"));
    }

    #[test]
    fn apply_llm_links_uses_bare_form_when_anchor_equals_slug() {
        let mut valid = std::collections::HashSet::new();
        valid.insert("postgres".to_string());
        let body = "Note about postgres.\n";
        let candidates = vec![("postgres".to_string(), "postgres".to_string())];
        let (out, _) = apply_llm_links(body, &candidates, "self", &valid);
        // anchor == slug → `[[postgres]]`, no pipe form.
        assert!(out.contains("[[postgres]]"));
        assert!(!out.contains("[[postgres|"));
    }

    #[test]
    fn build_llm_link_prompt_includes_body_and_digest() {
        let others = vec![(
            "postgres".to_string(),
            "PostgreSQL".to_string(),
            "open-source RDB".to_string(),
        )];
        let p = build_llm_link_prompt("indexing", "Pages talk about postgres.", &others);
        assert!(p.contains("indexing"));
        assert!(p.contains("Pages talk about postgres."));
        assert!(p.contains("- postgres — PostgreSQL — open-source RDB"));
        // The schema instruction is present so the model knows the shape.
        assert!(p.contains("\"links\":"));
        assert!(p.contains("target_slug"));
    }

    #[test]
    fn auto_link_min_len_filter_excludes_short_keys() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // 2-char slug is below the default min_len of 4.
        std::fs::write(k.pages_dir().join("go.md"), "stub").unwrap();
        std::fs::write(k.pages_dir().join("notes.md"), "I write go every day.\n").unwrap();
        let report = auto_link(&k, AutoLinkOptions::default()).unwrap();
        assert_eq!(report.links_added, 0);
        // Lowering min_len picks it up.
        let opts = AutoLinkOptions {
            min_len: 2,
            apply: false,
        };
        let report = auto_link(&k, opts).unwrap();
        assert_eq!(report.links_added, 1);
    }

    #[test]
    fn remove_deletes_directory_tree_and_counts_files() {
        let _home = scoped_home();
        let k = create("doomed", KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "x").unwrap();
        std::fs::write(k.pages_dir().join("b.md"), "y").unwrap();
        let sources = k.root.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(sources.join("s.md"), "z").unwrap();
        let root_before = k.root.clone();
        assert!(root_before.exists());

        let report = remove("doomed").unwrap();
        assert_eq!(report.pages_removed, 2);
        assert_eq!(report.sources_removed, 1);
        assert_eq!(report.root, root_before);
        assert!(!root_before.exists(), "root should be gone after remove");
        assert!(resolve("doomed").is_none());
    }

    #[test]
    fn remove_errors_on_unknown_kms() {
        let _home = scoped_home();
        let err = remove("ghost").unwrap_err();
        assert!(format!("{err}").contains("'ghost'"));
    }

    #[test]
    fn merge_into_errors_on_unknown_kms() {
        let _home = scoped_home();
        let _ = create("present", KmsScope::Project).unwrap();
        let err = merge_into("missing", "present").unwrap_err();
        assert!(format!("{err}").contains("'missing'"));
        let err = merge_into("present", "missing").unwrap_err();
        assert!(format!("{err}").contains("'missing'"));
    }

    #[test]
    fn read_browse_file_truncates_oversize_with_notice() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let sources = k.root.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        // Slightly over the cap: 256 KB cap + 1 KB filler.
        let big = "A".repeat(BROWSE_FILE_BYTE_CAP as usize + 1024);
        std::fs::write(sources.join("huge.md"), &big).unwrap();
        let read = read_browse_file("nb", "source", "huge").unwrap();
        assert!(read.truncated);
        assert_eq!(read.total_bytes, big.len() as u64);
        assert!(read.content.starts_with("> **Large file"));
        // The truncated body is bounded — never larger than the cap +
        // a small notice overhead. Loose check: stay well under the
        // full file size to confirm we didn't ship the whole thing.
        assert!(read.content.len() < BROWSE_FILE_BYTE_CAP as usize + 4096);
    }

    // ── OKF import/export ────────────────────────────────────────────

    #[test]
    fn okf_tag_conversions_round_trip() {
        assert_eq!(tags_to_yaml_list("a, b"), "[a, b]");
        assert_eq!(tags_to_yaml_list("[a, b]"), "[a, b]");
        assert_eq!(tags_to_yaml_list(""), "[]");
        assert_eq!(tags_to_csv("[a, b]"), "a, b");
        assert_eq!(tags_to_csv("a,b"), "a, b");
        assert_eq!(tags_to_csv("[\"x\", \"y\"]"), "x, y");
    }

    #[test]
    fn okf_wikilinks_become_bundle_relative_links() {
        let body = "See [[auth-flow]] and [[orders|the orders page]]. Keep [x](pages/x.md).";
        let out = wikilinks_to_okf(body);
        assert!(out.contains("[auth-flow](/pages/auth-flow.md)"));
        assert!(out.contains("[the orders page](/pages/orders.md)"));
        // Existing relative md links are left untouched.
        assert!(out.contains("[x](pages/x.md)"));
        // Round trip back to KMS-relative form.
        assert_eq!(okf_links_to_kms("[a](/pages/a.md)"), "[a](pages/a.md)");
    }

    #[test]
    fn okf_export_produces_conformant_bundle() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("auth.md"),
            "---\ntitle: Auth\ntopic: How login works\ncategory: security\ntags: oauth, sso\nsources: session-1\nupdated: 2026-05-28\n---\n# Auth\n\nSee [[orders]] for the flow.\n",
        )
        .unwrap();
        std::fs::write(
            k.index_path(),
            "# notes\n\n- [auth](pages/auth.md) — How login works\n",
        )
        .unwrap();
        std::fs::write(
            k.log_path(),
            "## [2026-05-28] ingested | auth\n## [2026-05-28] merge | other\n",
        )
        .unwrap();

        let out = k.root.parent().unwrap().join("notes-okf");
        let report = export_okf("notes", &out).unwrap();
        assert_eq!(report.pages, 1);

        // Page: type present (from category), description (from topic),
        // tags list-ified, wikilink converted.
        let page = std::fs::read_to_string(out.join("pages/auth.md")).unwrap();
        assert!(page.contains("type: security"), "got: {page}");
        assert!(page.contains("description: How login works"));
        assert!(page.contains("tags: [oauth, sso]"));
        assert!(page.contains("[orders](/pages/orders.md)"));
        // KMS-only key rides along.
        assert!(page.contains("sources: session-1"));
        // Nothing links to auth, so it carries no backlink block.
        assert!(!page.contains("Linked from"), "got: {page}");

        // Root index declares the OKF version.
        let idx = std::fs::read_to_string(out.join("index.md")).unwrap();
        assert!(idx.contains("okf_version: 0.1"));

        // Log regrouped under a bare date heading.
        let log = std::fs::read_to_string(out.join("log.md")).unwrap();
        assert!(log.contains("## 2026-05-28"));
        assert!(log.contains("* **Ingested**: auth"));

        // Every emitted concept .md carries a `type` (conformance §9).
        let (page_fm, _) = parse_frontmatter(&page);
        assert!(page_fm.get("type").map(|t| !t.is_empty()).unwrap_or(false));
    }

    #[test]
    fn okf_export_carries_backlinks_and_import_drops_them_again() {
        let _home = scoped_home();
        let k = create("bl-src", KmsScope::Project).unwrap();
        write_page(&k, "target", "---\ntitle: \"Target\"\n---\n\nthe note\n").unwrap();
        write_page(
            &k,
            "linker",
            "---\ntitle: \"Linker\"\n---\n\npoints at [[target|Target]]\n",
        )
        .unwrap();

        let bundle = k.root.parent().unwrap().join("bl-okf");
        export_okf("bl-src", &bundle).unwrap();
        let exported = std::fs::read_to_string(bundle.join("pages/target.md")).unwrap();
        assert!(
            exported.contains("## Linked from") && exported.contains("[Linker](/pages/linker.md)"),
            "a bundle leaves the KMS behind, so the reverse edges travel with it: {exported}"
        );
        // Re-exporting must not stack a second block.
        export_okf("bl-src", &bundle).unwrap();
        let again = std::fs::read_to_string(bundle.join("pages/target.md")).unwrap();
        assert_eq!(again.matches("## Linked from").count(), 1, "{again}");

        // Importing recomputes them, so the file must come back clean.
        import_okf(&bundle, "bl-dst", KmsScope::Project).unwrap();
        let dst = resolve("bl-dst").unwrap();
        let imported = std::fs::read_to_string(dst.pages_dir().join("target.md")).unwrap();
        assert!(!imported.contains("Linked from"), "{imported}");
        assert_eq!(
            backlinks("bl-dst", "target")
                .into_iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>(),
            vec!["linker"],
            "the graph survives the round trip through the links themselves"
        );
    }

    #[test]
    fn okf_round_trip_preserves_page_fields() {
        let _home = scoped_home();
        let k = create("src", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("auth.md"),
            "---\ntitle: Auth\ntopic: How login works\ncategory: security\ntags: oauth, sso\nsources: session-1\nverified: 2026-05-01\nupdated: 2026-05-28\n---\n# Auth\n\nBody text.\n",
        )
        .unwrap();
        let src_sources = k.root.join("sources");
        std::fs::create_dir_all(&src_sources).unwrap();
        std::fs::write(
            src_sources.join("spec.md"),
            "raw spec body, no frontmatter\n",
        )
        .unwrap();

        let bundle = k.root.parent().unwrap().join("src-okf");
        export_okf("src", &bundle).unwrap();

        let report = import_okf(&bundle, "dst", KmsScope::Project).unwrap();
        assert_eq!(report.pages, 1);
        assert_eq!(report.sources, 1);

        let dst = resolve("dst").unwrap();
        let page = std::fs::read_to_string(dst.pages_dir().join("auth.md")).unwrap();
        let (fm, _) = parse_frontmatter(&page);
        assert_eq!(fm.get("category").map(String::as_str), Some("security"));
        assert_eq!(fm.get("title").map(String::as_str), Some("Auth"));
        assert_eq!(fm.get("topic").map(String::as_str), Some("How login works"));
        assert_eq!(fm.get("tags").map(String::as_str), Some("oauth, sso"));
        assert_eq!(fm.get("sources").map(String::as_str), Some("session-1"));
        assert_eq!(fm.get("verified").map(String::as_str), Some("2026-05-01"));
        assert_eq!(fm.get("updated").map(String::as_str), Some("2026-05-28"));

        // Raw source restored without the export-time `type: Source` shim.
        let restored = std::fs::read_to_string(dst.root.join("sources/spec.md")).unwrap();
        assert_eq!(restored, "raw spec body, no frontmatter\n");

        // Index rebuilt KMS-native.
        let idx = dst.read_index();
        assert!(idx.contains("(pages/auth.md)"));
    }

    #[test]
    fn okf_import_handles_root_level_concepts_and_missing_type() {
        let _home = scoped_home();
        // Hand-roll an external OKF bundle: a concept at the root (not
        // under pages/), a nested concept, and one missing `type`.
        // `scoped_home` points cwd at a fresh tempdir; build under it.
        let bundle = std::env::current_dir().unwrap().join("ext-bundle");
        std::fs::create_dir_all(bundle.join("tables")).unwrap();
        std::fs::write(
            bundle.join("orders.md"),
            "---\ntype: BigQuery Table\ntitle: Orders\ndescription: One row per order\ntags: [sales, revenue]\n---\n# Orders\n\nSee [customers](/tables/customers.md).\n",
        )
        .unwrap();
        std::fs::write(
            bundle.join("tables/customers.md"),
            "---\ntitle: Customers\n---\nNo type here — should fall back.\n",
        )
        .unwrap();

        let report = import_okf(&bundle, "imported", KmsScope::Project).unwrap();
        assert_eq!(report.pages, 2);

        let k = resolve("imported").unwrap();
        // Root concept kept its stem.
        let orders = std::fs::read_to_string(k.pages_dir().join("orders.md")).unwrap();
        let (ofm, _) = parse_frontmatter(&orders);
        assert_eq!(
            ofm.get("category").map(String::as_str),
            Some("BigQuery Table")
        );
        assert_eq!(
            ofm.get("topic").map(String::as_str),
            Some("One row per order")
        );
        assert_eq!(ofm.get("tags").map(String::as_str), Some("sales, revenue"));
        // Link to the nested concept follows the stem flattening.
        assert!(
            orders.contains("](pages/tables-customers.md)"),
            "got: {orders}"
        );

        // Nested concept flattened to `tables-customers`, missing type
        // falls back to "uncategorized".
        let cust = std::fs::read_to_string(k.pages_dir().join("tables-customers.md")).unwrap();
        let (cfm, _) = parse_frontmatter(&cust);
        assert_eq!(
            cfm.get("category").map(String::as_str),
            Some("uncategorized")
        );
    }

    #[test]
    fn okf_import_rejects_existing_name() {
        let _home = scoped_home();
        create("dup", KmsScope::Project).unwrap();
        let bundle = std::env::current_dir().unwrap().join("ext-bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("a.md"), "---\ntype: Note\n---\nbody\n").unwrap();
        let err = import_okf(&bundle, "dup", KmsScope::Project).unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    // ── Shared-agent mode (dev-plan/41) ──────────────────────────────

    /// Build a fake shared brain under `dir/kms/<name>` and point
    /// THCLAWS_SHARED_AGENT_DIR at it. Caller must remove the env var.
    fn seed_shared_kms(dir: &std::path::Path, name: &str) {
        let kms = dir.join("kms").join(name);
        std::fs::create_dir_all(kms.join("pages")).unwrap();
        std::fs::write(kms.join("index.md"), format!("# {name}\n")).unwrap();
        std::fs::write(
            kms.join("pages").join("intro.md"),
            "---\ncategory: x\n---\n# Intro\nshared knowledge\n",
        )
        .unwrap();
        std::env::set_var("THCLAWS_SHARED_AGENT_DIR", dir);
    }

    #[test]
    fn shared_kms_resolves_read_only_and_blocks_writes() {
        let _home = scoped_home();
        // scoped_home points cwd + HOME at fresh tempdirs; build the
        // shared brain in a sibling dir under cwd.
        let brain = std::env::current_dir().unwrap().join("brain");
        seed_shared_kms(&brain, "company");

        let kref = resolve("company").expect("shared KMS should resolve");
        assert_eq!(kref.scope, KmsScope::Shared);
        assert!(kref.read_only());

        // Every mutation path refuses.
        assert!(write_page(&kref, "newpage", "hi").is_err());
        assert!(append_to_page(&kref, "intro", "more").is_err());
        assert!(delete_page(&kref, "intro").is_err());
        let src = std::env::current_dir().unwrap().join("src.md");
        std::fs::write(&src, "raw").unwrap();
        assert!(ingest(&kref, &src, Some("x"), false).is_err());

        // merge INTO a shared KMS is refused; a normal user KMS still works.
        create("scratch", KmsScope::Project).unwrap();
        assert!(merge_into("scratch", "company").is_err());

        // Reads are unaffected — the page is still listed in the index.
        assert!(kref.read_index().contains("company"));

        std::env::remove_var("THCLAWS_SHARED_AGENT_DIR");
    }

    #[test]
    fn shared_mode_locks_instructions_to_company_agents_md() {
        let _home = scoped_home();
        let cwd = std::env::current_dir().unwrap();
        // Member tries to override via working-dir + user-scope AGENTS.md.
        std::fs::write(cwd.join("AGENTS.md"), "MEMBER OVERRIDE\n").unwrap();
        let user_cfg = crate::util::home_dir().unwrap().join(".config/thclaws");
        std::fs::create_dir_all(&user_cfg).unwrap();
        std::fs::write(user_cfg.join("AGENTS.md"), "USER OVERRIDE\n").unwrap();

        // Without shared mode the member sources are honored.
        let normal = crate::context::find_claude_md_with(&cwd, false).unwrap_or_default();
        assert!(normal.contains("MEMBER OVERRIDE"));

        // With shared mode, ONLY the company AGENTS.md is used.
        let brain = cwd.join("brain");
        std::fs::create_dir_all(&brain).unwrap();
        std::fs::write(brain.join("AGENTS.md"), "COMPANY RULES\n").unwrap();
        std::env::set_var("THCLAWS_SHARED_AGENT_DIR", &brain);

        let locked = crate::context::find_claude_md_with(&cwd, false).unwrap();
        assert_eq!(locked.trim(), "COMPANY RULES");
        assert!(!locked.contains("MEMBER OVERRIDE"));
        assert!(!locked.contains("USER OVERRIDE"));

        std::env::remove_var("THCLAWS_SHARED_AGENT_DIR");
    }

    #[test]
    fn shared_kms_blocks_auto_link_apply_but_allows_dry_run() {
        let _home = scoped_home();
        let brain = std::env::current_dir().unwrap().join("brain");
        seed_shared_kms(&brain, "company");
        let kref = resolve("company").unwrap();
        assert!(kref.read_only());
        // Dry-run (read-only) is allowed.
        assert!(auto_link(
            &kref,
            AutoLinkOptions {
                min_len: 4,
                apply: false
            }
        )
        .is_ok());
        // --apply against a read-only shared KMS is refused.
        let err = auto_link(
            &kref,
            AutoLinkOptions {
                min_len: 4,
                apply: true,
            },
        )
        .unwrap_err();
        assert!(format!("{err}").contains("read-only"));
        std::env::remove_var("THCLAWS_SHARED_AGENT_DIR");
    }

    #[test]
    fn shared_mode_forces_gateway_and_ignores_member_byok() {
        let _home = scoped_home();
        let brain = std::env::current_dir().unwrap().join("brain");
        std::fs::create_dir_all(&brain).unwrap();
        // Company settings pin a model; no provider/BYOK config.
        std::fs::write(
            brain.join("settings.json"),
            "{\"model\":\"claude-opus-4-8\"}",
        )
        .unwrap();
        // Member tries to inject a project-scope provider override.
        std::fs::create_dir_all(".thclaws").unwrap();
        std::fs::write(
            ".thclaws/settings.json",
            "{\"model\":\"gpt-4o\",\"gatewayUseFor\":[]}",
        )
        .unwrap();
        std::env::set_var("THCLAWS_SHARED_AGENT_DIR", &brain);

        let cfg = crate::config::AppConfig::load().unwrap();
        // Company model wins (member's project override ignored).
        assert_eq!(cfg.model, "claude-opus-4-8");
        // Gateway forced for every routable provider.
        assert!(cfg.gateway_use_for.iter().any(|p| p == "anthropic"));
        assert!(cfg.gateway_use_for.iter().any(|p| p == "openai"));

        std::env::remove_var("THCLAWS_SHARED_AGENT_DIR");
    }
}
