//! CLI subcommand handlers — wired from `bin/app.rs`.

use std::path::{Path, PathBuf};

use crate::cloud::{client::Client, pack, resolve_cloud_url, wssync, CloudConfig};

pub async fn login(
    cloud_url: Option<&str>,
    token: Option<String>,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = match token {
        Some(t) => t.trim().to_string(),
        None => prompt_token()?,
    };
    if !token.starts_with("thc_") {
        return Err(
            "expected token to start with 'thc_' — get one from the dashboard at /dashboard".into(),
        );
    }
    let client = Client::new(&url, Some(token.clone()));
    let me = client.me().await?;
    crate::cloud::set_token(&token).map_err(|e| format!("save token: {}", e))?;
    // Persist the URL too so subsequent CLI calls don't need --cloud-url.
    // Same field the GUI Settings → Cloud section writes.
    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
    project.set_cloud_url(Some(&url));
    if let Err(e) = project.save() {
        eprintln!("  warning: couldn't persist URL to settings.json: {}", e);
    }
    eprintln!("✓ Signed in to {} as {}", url, me.email);
    if me.can_publish {
        eprintln!("  Publishing enabled.");
    } else {
        eprintln!("  Publishing not enabled for this account.");
    }
    Ok(())
}

fn prompt_token() -> Result<String, String> {
    use std::io::{BufRead, Write};
    eprint!("Paste CLI token (from /dashboard): ");
    std::io::stderr().flush().ok();
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("stdin: {}", e))?;
    let t = line.trim().to_string();
    if t.is_empty() {
        return Err("no token entered".into());
    }
    Ok(t)
}

pub fn logout() -> Result<(), String> {
    crate::cloud::clear_token().map_err(|e| format!("clear token: {}", e))?;
    eprintln!("✓ Signed out");
    Ok(())
}

/// Print where the CLI currently thinks the catalog lives and whether
/// it has credentials. Mirrors the Settings → Cloud panel so users can
/// confirm CLI-side state without opening the GUI.
pub fn status(cloud_url: Option<&str>, cloud_cfg: Option<&CloudConfig>) -> Result<(), String> {
    for line in status_lines(cloud_url, cloud_cfg) {
        eprintln!("{line}");
    }
    Ok(())
}

/// Return the same lines `status()` prints, but as a `Vec<String>` so
/// the REPL / GUI slash-command dispatchers can route them through
/// their own output channels (`println!` vs `ViewEvent::SlashOutput`).
pub fn status_lines(cloud_url: Option<&str>, cloud_cfg: Option<&CloudConfig>) -> Vec<String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let has_token = crate::cloud::token().is_some();
    let agent = crate::config::ProjectConfig::load().and_then(|c| c.agent.clone());
    let mut lines = vec![
        format!("Cloud URL: {url}"),
        format!("Token:     {}", if has_token { "set" } else { "(none)" }),
    ];
    match agent {
        Some(a) => {
            lines.push(format!(
                "Agent:     {} ({})",
                a.name.as_deref().unwrap_or("(unnamed)"),
                a.id.as_deref().unwrap_or("?")
            ));
            lines.push(format!(
                "UUID:      {}",
                a.uuid
                    .as_deref()
                    .map(|u| format!("{u} (bound)"))
                    .unwrap_or_else(|| "(unbound — next publish creates new entry)".to_string())
            ));
        }
        None => {
            lines.push("Agent:     (no settings.json::agent block in this folder)".to_string());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let b = wssync::read_binding(&cwd);
        if let Some(slug) = b.slug.as_deref() {
            lines.push(format!(
                "Sync:      {} — rev {}{}{}",
                slug,
                b.revision
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "? (synced before revisions)".to_string()),
                b.last_push
                    .as_deref()
                    .map(|t| format!(", pushed {}", age_string(t)))
                    .unwrap_or_default(),
                b.last_pull
                    .as_deref()
                    .map(|t| format!(", pulled {}", age_string(t)))
                    .unwrap_or_default(),
            ));
        }
    }
    lines
}

/// Workspace-wide cloud operations act on ONE directory tree, addressed via
/// the process working directory. A multiuser pod shares one process across
/// every tenant, so that directory is the POD ROOT holding everyone's
/// `workspace-<id>/` — a push would ship every tenant's files to one tenant's
/// hosted workspace, and a get/pull would overwrite all of them. The server
/// half of this is closed in `server::sync_bearer_gate`; this is the client
/// half, which reads the filesystem directly and never goes through it.
fn refuse_in_multiuser(op: &str) -> Option<String> {
    crate::workdir::is_multiuser().then(|| {
        format!(
            "{op} isn't available in a multiuser workspace — it operates on the whole pod \
             directory, which is shared across tenants here"
        )
    })
}

/// `/cloud revision` — what sync revision each end is on.
///
/// Strictly read-only: it resolves and stats the workspace but never wakes a
/// paused one (a status query must not cost a pod resume) and never writes a
/// binding. The local half always prints, so the command is still useful
/// offline or logged out.
pub async fn revision_lines(
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
    workspace: Option<&str>,
) -> Vec<String> {
    if let Some(msg) = refuse_in_multiuser("/cloud revision") {
        return vec![msg];
    }
    let b = wssync::read_binding(cwd);
    let local_line = format!(
        "Local:     {}{}{}",
        fmt_revision(b.revision),
        b.last_push
            .as_deref()
            .map(|t| format!(", pushed {}", age_string(t)))
            .unwrap_or_default(),
        b.last_pull
            .as_deref()
            .map(|t| format!(", pulled {}", age_string(t)))
            .unwrap_or_default(),
    );
    if workspace.is_none() && b.slug.is_none() && b.workspace_id.is_none() {
        return vec![
            local_line,
            "Cloud:     (this folder isn't paired with a hosted workspace — /cloud push pairs it)"
                .to_string(),
        ];
    }
    // Local-only, so it prints whether or not the cloud answers.
    let drift = local_drift_lines(cwd, b.revision);
    match fetch_remote_revision(cloud_url, cloud_cfg, workspace, &b).await {
        Ok((ws, stat)) => {
            let mut lines = vec![format!("Workspace: {} ({})", ws.slug, ws.id), local_line];
            lines.extend(drift);
            lines.push(format!(
                "Cloud:     {}{}",
                fmt_revision(stat.revision),
                if stat.busy {
                    " — busy (active turn)"
                } else {
                    ""
                }
            ));
            // Comparing counts across a rebind is meaningless — they belong to
            // different pairings. Say so instead of printing a bogus verdict.
            if b.workspace_id
                .as_deref()
                .is_some_and(|bound| bound != ws.id)
            {
                lines.push(format!(
                    "⚠ this folder is bound to {} — the two counts belong to different pairings.",
                    b.workspace_id.as_deref().unwrap_or("?")
                ));
            } else {
                lines.push(revision_verdict(b.revision, stat.revision));
            }
            lines
        }
        Err(e) => {
            let mut lines = vec![local_line];
            lines.extend(drift);
            lines.push(format!("Cloud:     unavailable — {}", e));
            lines
        }
    }
}

/// "Is this folder dirty?" — what changed here since the base recorded at the
/// last successful sync. Hashes the working tree (no network, no cloud view),
/// so it answers even when the workspace is asleep.
fn local_drift_lines(cwd: &Path, rev: Option<u64>) -> Vec<String> {
    let since = rev
        .map(|r| format!("rev {r}"))
        .unwrap_or_else(|| "the last sync".to_string());
    let clean = || {
        vec![format!(
            "Changes:   clean — nothing changed here since {since}"
        )]
    };
    let manifest = match wssync::build_manifest(cwd) {
        Ok(m) => m,
        Err(e) => return vec![format!("Changes:   (couldn't scan the folder — {e})")],
    };
    match wssync::read_sync_base_manifests(cwd) {
        Some((base_local, _)) => {
            let (changed, removed) = wssync::drift_since_base(&base_local, &manifest);
            // Runtime state rewrites itself just by the engine running, so
            // counting it as "your changes" would mark every folder dirty
            // forever. It still rides the next push — hence counted, not
            // hidden — but it's tallied apart from real work.
            let is_state = |p: &String| wssync::is_runtime_state(p);
            let (state_changed, changed): (Vec<String>, Vec<String>) =
                changed.into_iter().partition(is_state);
            let (state_removed, removed): (Vec<String>, Vec<String>) =
                removed.into_iter().partition(is_state);
            let state_n = state_changed.len() + state_removed.len();
            if changed.is_empty() && removed.is_empty() {
                return match state_n {
                    0 => clean(),
                    n => vec![format!(
                        "Changes:   clean — no work changed since {since} ({n} runtime state file(s) did)"
                    )],
                };
            }
            let mut counts = Vec::new();
            if !changed.is_empty() {
                counts.push(format!("{} changed", changed.len()));
            }
            if !removed.is_empty() {
                counts.push(format!("{} deleted", removed.len()));
            }
            let named: Vec<String> = changed.into_iter().chain(removed).collect();
            vec![
                format!(
                    "Changes:   dirty — {} since {}{}",
                    counts.join(", "),
                    since,
                    match state_n {
                        0 => String::new(),
                        n => format!(" (+{n} runtime state)"),
                    }
                ),
                format!("           {}", name_paths(&named)),
            ]
        }
        // A base written before per-file views existed answers yes/no only.
        None if wssync::read_sync_base(cwd).is_some() => {
            if wssync::diverged_from_base(cwd, &manifest) {
                vec![format!(
                    "Changes:   dirty — something changed since {since} \
                     (per-file detail needs a newer base — the next sync records one)"
                )]
            } else {
                clean()
            }
        }
        None => vec![
            "Changes:   (no sync recorded yet — a push would upload everything here)".to_string(),
        ],
    }
}

async fn fetch_remote_revision(
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
    workspace: Option<&str>,
    binding: &wssync::Binding,
) -> Result<
    (
        crate::cloud::client::WorkspaceSummary,
        crate::cloud::client::SyncStatResp,
    ),
    String,
> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    if token.is_none() {
        return Err("not logged in — paste your CLI token in Settings → thClaws.cloud".into());
    }
    let client = Client::new(&url, token);
    let ws = resolve_workspace(&client, workspace.or(binding.slug.as_deref())).await?;
    let jwt = client.cli_exchange().await?;
    let stat = client.ws_sync_stat(&ws.url, &jwt).await.map_err(|e| {
        format!(
            "'{}' isn't responding ({}) — it may be paused; a push or pull resumes it",
            ws.slug, e
        )
    })?;
    Ok((ws, stat))
}

fn fmt_revision(r: Option<u64>) -> String {
    r.map(|n| format!("rev {n}"))
        .unwrap_or_else(|| "no revision recorded".to_string())
}

/// Read the two counters against each other. Deliberately phrased about the
/// SYNC, not the content: matching revisions mean the ends last agreed at that
/// number, not that the files are identical today. The `Changes:` line above
/// answers that for THIS end; the cloud's side needs a `--dry-run`.
fn revision_verdict(local: Option<u64>, cloud: Option<u64>) -> String {
    match (local, cloud) {
        (Some(l), Some(c)) if l == c => format!(
            "✓ both ends last synced at rev {l} — the cloud may have changed since; /cloud pull --dry-run to check"
        ),
        (Some(l), Some(c)) if l > c => format!(
            "⚠ local is {} rev ahead (cloud at rev {c}) — the cloud missed a sync, or a revision write didn't land",
            l - c
        ),
        (Some(l), Some(c)) => format!(
            "⚠ cloud is {} rev ahead (local at rev {l}) — something else synced this workspace; /cloud pull --dry-run first",
            c - l
        ),
        (None, Some(c)) => format!(
            "⚠ this folder has no revision yet but the cloud is at rev {c} — it was paired from another folder, or synced by an older engine"
        ),
        (Some(l), None) => format!(
            "⚠ the cloud reports no revision (its engine predates revisions) — local is at rev {l}"
        ),
        (None, None) => {
            "Neither end has a revision yet — the next successful sync lands rev 1.".to_string()
        }
    }
}

/// Render a stored unix-seconds stamp as a rough age. The binding holds raw
/// seconds (no chrono dep in this module); users only need "how stale".
fn age_string(stamp: &str) -> String {
    let Ok(then) = stamp.parse::<u64>() else {
        return stamp.to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Hit the catalog and return the lines the slash dispatchers (REPL +
/// GUI) print. Errors surface as a single line so both surfaces render
/// identically.
pub async fn list_lines(
    mine: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    let client = Client::new(&url, token);
    match client.list_agents(mine).await {
        Ok(agents) if agents.is_empty() => vec!["(no agents in catalog)".to_string()],
        Ok(agents) => agents
            .into_iter()
            .map(|a| {
                format!(
                    "{:30}  v{:<10}  {}",
                    a.slug,
                    a.current_version.unwrap_or_default(),
                    a.name
                )
            })
            .collect(),
        Err(e) => vec![format!("/cloud list: {e}")],
    }
}

/// `/cloud publish` from inside a session — packs cwd + uploads.
/// Returns ordered progress lines (including any error). Mirrors
/// [`get_into_cwd_lines`].
pub async fn publish_cwd_lines(
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Err(e) = publish_inner(cwd.to_path_buf(), cloud_url, false, cloud_cfg, &mut lines).await
    {
        lines.push(format!("/cloud publish: {e}"));
    }
    lines
}

pub async fn publish(
    path: PathBuf,
    cloud_url: Option<&str>,
    dry_run: bool,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    // CLI-facing thin wrapper: mirror the slash-friendly inner impl
    // and dump its lines to stderr so terminal output matches the old
    // eprintln shape exactly.
    let mut lines = Vec::new();
    let result = publish_inner(path, cloud_url, dry_run, cloud_cfg, &mut lines).await;
    for ln in &lines {
        eprintln!("{ln}");
    }
    result
}

async fn publish_inner(
    path: PathBuf,
    cloud_url: Option<&str>,
    dry_run: bool,
    cloud_cfg: Option<&CloudConfig>,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();

    // The chdir below is a PROCESS-GLOBAL mutation, so it needs the same two
    // guards every other global-cwd mutator in the engine carries:
    //
    // - multiuser: one process serves every tenant, so relocating cwd would
    //   move every OTHER tenant's in-flight path resolution too. `ipc.rs`
    //   refuses `set_cwd` here for exactly this reason.
    // - active turn: even single-tenant, Terminal and Chat share one session;
    //   yanking cwd out from under a running turn re-points its file tools
    //   mid-flight. `/cloud push` already refuses on a busy engine.
    if let Some(msg) = refuse_in_multiuser("/cloud publish") {
        return Err(msg);
    }
    if crate::agent_activity::busy_count() > 0 {
        return Err("a turn is active — wait for it to finish before publishing".into());
    }

    // Load this folder's project settings so we can read agent identity
    // + write the UUID back after the server assigns one. We deliberately
    // chdir-style here: ProjectConfig::load reads from cwd, so we cd
    // into the agent folder for the duration of the call. (We restore
    // cwd at the end so the caller's environment is unchanged.)
    let prior_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(&path).map_err(|e| format!("entering {}: {}", path.display(), e))?;
    let _restore = scopeguard_chdir(prior_cwd);

    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
    let agent = ensure_agent_identity(&mut project, &path)?;

    let fused =
        crate::cloud::manifest::Manifest::fuse_for_publish(&agent, &path.join("manifest.json"))?;
    let fused_json =
        serde_json::to_vec_pretty(&fused).map_err(|e| format!("serialize fused manifest: {e}"))?;

    log.push(format!("Packing {} …", path.display()));
    let result = pack::pack(&path, Some(&fused_json))?;
    log.push(format!(
        "  Included {} file(s), stripped {} file(s), {:.1} KB",
        result.included.len(),
        result.stripped.len(),
        result.bytes.len() as f64 / 1024.0
    ));
    if !result.stripped.is_empty() {
        log.push("  Stripped (showing first 10):".to_string());
        for s in result.stripped.iter().take(10) {
            log.push(format!("    - {}", s));
        }
    }
    if agent.uuid.is_some() {
        log.push(format!(
            "  Publishing as existing agent (uuid: {}…)",
            &agent
                .uuid
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(8)
                .collect::<String>()
        ));
    } else {
        log.push("  First publish — server will assign a UUID.".to_string());
    }
    if dry_run {
        log.push("Dry run — not uploading.".to_string());
        return Ok(());
    }

    if token.is_none() {
        return Err(
            "not logged in — paste your CLI token in Settings → thClaws.cloud (mint one at /dashboard)".into()
        );
    }

    log.push(format!("Uploading to {} …", url));
    let client = Client::new(&url, token);
    let resp = client.publish(result.bytes).await?;
    log.push(format!(
        "✓ Published {} v{} ({} bytes)",
        resp.slug, resp.version, resp.size_bytes
    ));
    log.push(format!("  {}", resp.url));

    // Write the assigned UUID back to settings.json so re-publish from
    // this folder targets the same catalog entry.
    if agent.uuid.as_deref() != Some(resp.uuid.as_str()) {
        project.merge_agent(crate::config::AgentConfig {
            uuid: Some(resp.uuid.clone()),
            ..Default::default()
        });
        project
            .save()
            .map_err(|e| format!("write settings.json: {e}"))?;
        log.push(format!(
            "  settings.json::agent.uuid updated → {}…",
            resp.uuid.chars().take(8).collect::<String>()
        ));
    }
    Ok(())
}

/// Settings-or-manifest helper: pull the agent identity to use for
/// `cloud publish`. If `settings.json::agent` is populated, use it. If
/// it's missing but the legacy `manifest.json` carries id/name/
/// description (pre-Option-A folders), auto-migrate to settings.json
/// and emit a one-line notice. Returns the resolved identity.
fn ensure_agent_identity(
    project: &mut crate::config::ProjectConfig,
    folder: &Path,
) -> Result<crate::config::AgentConfig, String> {
    if let Some(existing) = project.agent.as_ref() {
        if existing.id.is_some() && existing.name.is_some() && existing.description.is_some() {
            return Ok(existing.clone());
        }
    }

    // Try to read identity from legacy manifest.json.
    let manifest_path = folder.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "no settings.json::agent block and can't read manifest.json: {e}\n\
             — add an [agent] section to ./.thclaws/settings.json with id/name/description"
        )
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("manifest.json: {e}"))?;
    let id = v.get("id").and_then(|x| x.as_str()).map(String::from);
    let name = v.get("name").and_then(|x| x.as_str()).map(String::from);
    let description = v
        .get("description")
        .and_then(|x| x.as_str())
        .map(String::from);
    if id.is_none() || name.is_none() || description.is_none() {
        return Err(
            "settings.json::agent.{id,name,description} required for publish (none of these \
             could be derived from manifest.json either)"
                .into(),
        );
    }
    eprintln!("  Migrating identity from manifest.json → settings.json::agent");
    project.merge_agent(crate::config::AgentConfig {
        id,
        name,
        description,
        uuid: None,
    });
    project
        .save()
        .map_err(|e| format!("write settings.json: {e}"))?;
    Ok(project.agent.clone().unwrap())
}

/// Restore cwd on drop. Used by `publish` so the caller's environment
/// is unchanged after the publish call returns.
fn scopeguard_chdir(prior: Option<PathBuf>) -> impl Drop {
    struct Guard(Option<PathBuf>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = std::env::set_current_dir(p);
            }
        }
    }
    Guard(prior)
}

pub fn unbind() -> Result<(), String> {
    for ln in unbind_lines() {
        eprintln!("{ln}");
    }
    Ok(())
}

/// `/cloud unbind` from inside a session. Same logic as [`unbind`]
/// but returns lines for the SlashOutput stream instead of eprintln.
/// `/cloud unbind` — blank the folder's bound agent UUID. This is the single
/// "detach" operation and it serves both intents: afterwards a DIFFERENT agent
/// can be `/cloud get`'d over this folder (the get guard treats an unbound
/// folder as free — see the uuid check in `get_lines`), AND a `/cloud publish`
/// with no bound uuid registers a NEW catalog entry (the backend mints a fresh
/// uuid and writes it back) instead of trying to update the original — i.e. a
/// fork. No separate `/cloud fork` needed; unbind + publish is the fork flow.
pub fn unbind_lines() -> Vec<String> {
    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
    let prior = project
        .agent
        .as_ref()
        .and_then(|a| a.uuid.clone())
        .unwrap_or_default();
    if prior.is_empty() {
        return vec![
            "Already unbound — `/cloud get <slug>` can replace this folder, or edit + `/cloud publish` for a new entry.".to_string(),
        ];
    }
    project.clear_agent_uuid();
    if let Err(e) = project.save() {
        return vec![format!("/cloud unbind: write settings.json: {e}")];
    }
    vec![format!(
        "✓ Detached agent {}… — folder is now unbound. `/cloud get <slug>` can replace it \
         with a different agent, or edit + `/cloud publish` to fork it into a new catalog entry.",
        prior.chars().take(8).collect::<String>()
    )]
}

pub async fn get(
    slug: String,
    target: PathBuf,
    version: Option<String>,
    force: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    let outcome = get_lines(slug, target, version, force, cloud_url, cloud_cfg).await;
    for line in &outcome.lines {
        eprintln!("{line}");
    }
    Ok(())
}

/// `cloud get <slug>` into the caller's cwd with the safety check the
/// slash command needs:
///   - empty cwd → extract fresh
///   - non-empty cwd + matching agent UUID → extract over (safe update)
///   - non-empty cwd + UUID mismatch or no UUID → abort
///
/// No `--force` — for that, use the CLI's `cloud get <slug> <target> --force`.
pub async fn get_into_cwd_lines(
    slug: String,
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    get_lines(
        slug,
        cwd.to_path_buf(),
        None,
        /*force=*/ false,
        cloud_url,
        cloud_cfg,
    )
    .await
    .lines
}

/// What `get_lines` did, for a caller that has to act on it. The slash
/// command only ever renders the lines, but installing a bot (dev-plan/59
/// Step 5) must not register a bot whose download or extraction failed.
pub(crate) struct GetOutcome {
    pub ok: bool,
    pub lines: Vec<String>,
}

impl GetOutcome {
    fn ok(lines: Vec<String>) -> Self {
        Self { ok: true, lines }
    }
    fn failed(lines: Vec<String>) -> Self {
        Self { ok: false, lines }
    }
}

/// Underlying get-and-report. Errors come back as a single line so
/// both surfaces (CLI eprintln, GUI/REPL SlashOutput) render identically.
/// `force` bypasses the UUID-match safety check on non-empty targets.
pub(crate) async fn get_lines(
    slug: String,
    target: PathBuf,
    version: Option<String>,
    force: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> GetOutcome {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    if token.is_none() {
        return GetOutcome::failed(vec![
            "/cloud get: not logged in — paste your CLI token in Settings → thClaws.cloud (mint one at /dashboard)"
                .to_string(),
        ]);
    }

    if let Some(msg) = refuse_in_multiuser("/cloud get") {
        return GetOutcome::failed(vec![msg]);
    }
    // `unpack` overwrites AGENTS.md / settings.json / .thclaws/ in place, so
    // an install while a turn is running swaps the agent's own definition out
    // from under it. Same guard `/cloud push` carries.
    if crate::agent_activity::busy_count() > 0 {
        return GetOutcome::failed(vec![
            "/cloud get: a turn is active — wait for it to finish before installing an agent"
                .to_string(),
        ]);
    }

    let mut lines = Vec::new();
    // "Has agent content" — not just "is non-empty". REPL startup
    // auto-bootstraps a placeholder .thclaws/settings.json in cwd
    // (via ProjectConfig::ensure_default_exists), which would make
    // a genuinely-fresh folder look non-empty. The real signal that
    // an agent already lives here is AGENTS.md or manifest.json.
    let has_agent_content =
        target.join("AGENTS.md").exists() || target.join("manifest.json").exists();

    lines.push(format!("Downloading {} …", slug));
    let client = Client::new(&url, token);
    let dl = match client.download(&slug, version.as_deref()).await {
        Ok(d) => d,
        Err(e) => {
            lines.push(format!("/cloud get: {e}"));
            return GetOutcome::failed(lines);
        }
    };
    lines.push(format!(
        "  v{} ({:.1} KB, sha256 {}…)",
        dl.version,
        dl.bytes.len() as f64 / 1024.0,
        &dl.sha256.chars().take(12).collect::<String>()
    ));

    // Fail CLOSED on a missing digest. This is the only integrity check on
    // bytes another user published, so "no header" must mean "don't install",
    // not "install unverified" — matching how the X-Agent-UUID guard below
    // already behaves. The catalog sets X-Agent-SHA256 on every download
    // (`routers/agents.py`), so an absent one means a broken or spoofed
    // endpoint, not an old backend.
    if dl.sha256.is_empty() {
        lines.push(
            "/cloud get: server didn't return an X-Agent-SHA256 header — refusing to install \
             unverified bytes. (Catalog backend or proxy probably needs a look.)"
                .into(),
        );
        return GetOutcome::failed(lines);
    }
    if let Err(e) = pack::verify_sha256(&dl.bytes, &dl.sha256) {
        lines.push(format!("/cloud get: {e}"));
        return GetOutcome::failed(lines);
    }

    // Safety check on folders that already hold an agent: refuse unless
    // the bound agent UUID matches what we just downloaded. `--force`
    // (CLI-only) bypasses.
    if has_agent_content && !force {
        let server_uuid = match dl.uuid.as_deref() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => {
                lines.push(
                    "/cloud get: server didn't return an X-Agent-UUID header — refusing to \
                     overwrite an existing agent folder. (Catalog backend probably needs an update.)"
                        .into(),
                );
                return GetOutcome::failed(lines);
            }
        };
        let local_uuid = load_local_agent_uuid(&target);
        match local_uuid.as_deref() {
            Some(local) if local == server_uuid => {
                lines.push(format!(
                    "  Folder matches agent UUID {}… — overwriting in-place.",
                    server_uuid.chars().take(8).collect::<String>()
                ));
            }
            Some(local) => {
                lines.push(format!(
                    "/cloud get: refusing to overwrite. This folder is bound to agent {}…, but \
                     the downloaded agent is {}…. To replace this folder with the downloaded \
                     agent, run /cloud unbind first OR cd to an empty directory.",
                    local.chars().take(8).collect::<String>(),
                    server_uuid.chars().take(8).collect::<String>()
                ));
                return GetOutcome::failed(lines);
            }
            None => {
                // No bound UUID = no published agent to protect. The folder was
                // either explicitly detached (`/cloud unbind`, which blanks the
                // uuid) or hand-assembled; either way an explicit `/cloud get`
                // is a clear intent to install here, so replace it. (The guard
                // exists to stop clobbering a DIFFERENT bound agent — case 2 —
                // not an unbound folder.)
                lines.push(
                    "  Folder is unbound (no agent UUID) — replacing it with the downloaded agent."
                        .into(),
                );
            }
        }
    }

    // Snapshot installer-owned settings before the overwrite. `unpack`
    // (force=true) replaces the agent's `.thclaws/settings.json` wholesale,
    // which would wipe local session/account config the agent has no
    // business carrying — notably `gatewayProxy`. Losing it drops the user
    // off the gateway, and the next agent rebuild then fails with a
    // misleading "no API key found for provider 'anthropic'". These keys
    // are carried forward after extraction (see `restore_installer_settings`).
    let prior_settings = std::fs::read(target.join(".thclaws").join("settings.json")).ok();
    let auto_trust = may_auto_trust(&target, &url, dl.official);

    lines.push(format!("Extracting to {} …", target.display()));
    // After the UUID match (or empty target, or --force) we always
    // allow overwrite — pack::unpack's per-file refusal is bypassed
    // because the safety gate already lives above.
    let files = match pack::unpack(&dl.bytes, &target, /*force=*/ true) {
        Ok(f) => f,
        Err(e) => {
            // `unpack` writes in place with no rollback, so a mid-extract
            // failure leaves the folder holding a MIX of old and new files.
            // Carrying the installer-owned keys back is what stops that from
            // compounding into "no API key found for provider 'anthropic'" on
            // the next rebuild — do it here too, not just on the happy path.
            lines.push(format!("/cloud get: {e}"));
            lines.push(format!(
                "  ⚠ {} may be partially extracted — re-run /cloud get to finish, \
                 or restore the folder from a backup.",
                target.display()
            ));
            if let Some(prior) = prior_settings {
                match restore_installer_settings(&target, &prior) {
                    Ok(()) => {
                        lines.push("  Local settings (gateway, model) were preserved.".to_string())
                    }
                    Err(e) => lines.push(format!(
                        "  warning: couldn't preserve local settings ({e}) — \
                         re-enable the gateway in Settings → thClaws.cloud if needed"
                    )),
                }
            }
            return GetOutcome::failed(lines);
        }
    };
    lines.push(format!("✓ Extracted {} file(s)", files.len()));

    let manifest_path = target.join("manifest.json");
    if let Ok(m) = crate::cloud::manifest::Manifest::from_path(&manifest_path) {
        if let Err(e) = split_unified_manifest(&target, &m, dl.uuid.as_deref()) {
            lines.push(format!(
                "  warning: couldn't split manifest into settings.json: {e}"
            ));
        }
        for line in post_install_hint_lines(&m, &target) {
            lines.push(line);
        }
    }

    // Carry installer-owned keys (gatewayProxy, …) back over the extracted
    // settings.json so the install doesn't knock the user off the gateway.
    if let Some(prior) = prior_settings {
        if let Err(e) = restore_installer_settings(&target, &prior) {
            lines.push(format!(
                "  warning: couldn't preserve local settings ({e}) — \
                 re-enable the gateway in Settings → thClaws.cloud if needed"
            ));
        }
    }
    if auto_trust {
        match crate::config::trust::trust_folder(&target) {
            Ok(()) => {
                lines.push("  Trusted this folder: it holds an official thClaws agent.".into())
            }
            Err(e) => lines.push(format!("  warning: couldn't trust this folder ({e})")),
        }
    }
    GetOutcome::ok(lines)
}

/// Whether installing an official (platform-owned) agent from `url` into
/// `target` may trust the folder, so the agent's own gui-shell, hooks and
/// bindings work: only from this install's own catalog, and only when
/// nothing in the folder was already waiting on trust, which would
/// otherwise ride in with the agent.
fn may_auto_trust(target: &Path, url: &str, official: bool) -> bool {
    if !official || crate::config::trust::hosted() || !crate::cloud::is_own_catalog(url) {
        return false;
    }
    let shells = target.join(".thclaws").join("gui-shell");
    let no_shells = std::fs::read_dir(&shells).map_or(true, |mut d| d.next().is_none());
    no_shells && crate::config::trust::withheld(target, Vec::new()).is_none()
}

/// Keys in `.thclaws/settings.json` that belong to the installing user's
/// session/account, not to the agent being installed. `/cloud get`
/// overwrites the whole file from the tarball, so these are snapshotted
/// before extraction and carried forward after — without this, an install
/// silently wipes the user's gateway routing (`gatewayProxy`) and cloud URL,
/// which surfaces as a misleading "no API key found for provider 'anthropic'"
/// on the next agent rebuild.
///
/// `model` is here too: `/model` persists the user's pick to the project
/// settings, and losing it on install meant the post-get `/reload` silently
/// fell back to the global default. A bundle that explicitly ships `model`
/// (a publisher pin) still wins — restore only fills keys the bundle left
/// unset.
const INSTALLER_OWNED_SETTINGS_KEYS: &[&str] =
    &["gatewayProxy", "gateway_use_for", "cloudUrl", "model"];

/// Restore installer-owned keys from `prior_raw` onto the freshly-extracted
/// `.thclaws/settings.json`. Only fills keys the agent's bundle did NOT set,
/// so a publisher that legitimately ships one of these still wins.
fn restore_installer_settings(target: &Path, prior_raw: &[u8]) -> Result<(), String> {
    let prior: serde_json::Value =
        serde_json::from_slice(prior_raw).map_err(|e| format!("parse prior settings.json: {e}"))?;
    let Some(prior_obj) = prior.as_object() else {
        return Ok(());
    };

    let settings_path = target.join(".thclaws").join("settings.json");
    let mut cur: serde_json::Value = match std::fs::read(&settings_path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    let Some(cur_obj) = cur.as_object_mut() else {
        return Ok(());
    };

    let mut restored = false;
    for k in INSTALLER_OWNED_SETTINGS_KEYS {
        if !cur_obj.contains_key(*k) {
            if let Some(v) = prior_obj.get(*k) {
                cur_obj.insert((*k).to_string(), v.clone());
                restored = true;
            }
        }
    }
    if !restored {
        return Ok(());
    }
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&cur).map_err(|e| format!("serialize settings.json: {e}"))?,
    )
    .map_err(|e| format!("write settings.json: {e}"))
}

/// Read just `<target>/.thclaws/settings.json::agent.uuid` without
/// touching the rest of project config.
fn load_local_agent_uuid(target: &Path) -> Option<String> {
    let prior = std::env::current_dir().ok();
    if std::env::set_current_dir(target).is_err() {
        return None;
    }
    let uuid = crate::config::ProjectConfig::load()
        .and_then(|c| c.agent)
        .and_then(|a| a.uuid);
    if let Some(p) = prior {
        let _ = std::env::set_current_dir(p);
    }
    uuid
}

fn split_unified_manifest(
    target: &Path,
    manifest: &crate::cloud::manifest::Manifest,
    server_uuid: Option<&str>,
) -> Result<(), String> {
    let prior_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(target)
        .map_err(|e| format!("entering {}: {}", target.display(), e))?;
    let _restore = scopeguard_chdir(prior_cwd);

    // Identity AND UUID travel with the package. Preserving the UUID
    // makes "re-get into the same folder" act as an update (same agent
    // → CLI overwrites in place). Fork-safety is enforced server-side:
    // if the recipient tries to `cloud publish`, the server checks
    // UUID ownership and 403s with a clear "run cloud unbind to fork".
    // UUID precedence: server X-Agent-UUID header (authoritative) >
    // manifest.uuid inside the tarball (may be stale or absent).
    let resolved_uuid = server_uuid
        .map(|s| s.to_string())
        .or_else(|| manifest.uuid.clone());
    let agent_block = serde_json::json!({
        "id": manifest.id,
        "name": manifest.name,
        "description": manifest.description,
        "uuid": resolved_uuid,
    });
    // Direct JSON-level merge — set just the `agent` key, preserve
    // everything else the tarball shipped (guiShell, model, etc.).
    // Going through ProjectConfig::save() would also write every
    // Option<bool> default-false field (shellTabEnabled, teamEnabled,
    // …), bloating the installer's settings.json with noise.
    let settings_path = std::path::Path::new(".thclaws").join("settings.json");
    let mut existing: serde_json::Value = match std::fs::read(&settings_path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    if let Some(obj) = existing.as_object_mut() {
        obj.insert("agent".to_string(), agent_block);
    }
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&existing)
            .map_err(|e| format!("serialize settings.json: {e}"))?,
    )
    .map_err(|e| format!("write settings.json: {e}"))?;

    // Strip identity fields from the on-disk manifest.json so the local
    // source of truth is unambiguous (settings.json::agent).
    let manifest_path = target.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;
    if let Some(obj) = v.as_object_mut() {
        for k in ["id", "name", "description", "uuid", "author"] {
            obj.remove(k);
        }
    }
    let slim =
        serde_json::to_string_pretty(&v).map_err(|e| format!("serialize slim manifest: {e}"))?;
    std::fs::write(&manifest_path, slim)
        .map_err(|e| format!("write {}: {e}", manifest_path.display()))?;
    Ok(())
}

pub async fn list(
    mine: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    let client = Client::new(&url, token);
    let agents = client.list_agents(mine).await?;
    if agents.is_empty() {
        eprintln!("(no agents)");
        return Ok(());
    }
    for a in agents {
        println!(
            "{:30}  v{:<10}  {}",
            a.slug,
            a.current_version.unwrap_or_default(),
            a.name
        );
    }
    Ok(())
}

fn post_install_hint_lines(m: &crate::cloud::manifest::Manifest, target: &Path) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        format!("Installed: {} v{}", m.name, m.version),
        format!("  cd {}", target.display()),
    ];
    if !m.requires.provider_keys.is_empty() {
        lines.push(String::new());
        lines.push("This agent expects these provider keys in .env:".to_string());
        for k in &m.requires.provider_keys {
            let mark = if k.required { "*" } else { " " };
            let purpose = k.purpose.as_deref().unwrap_or("");
            lines.push(format!(
                "  {} {}={}",
                mark,
                k.name,
                if purpose.is_empty() {
                    "<your-key>"
                } else {
                    purpose
                }
            ));
        }
    }
    if !m.requires.mcp_servers.is_empty() {
        lines.push(String::new());
        lines.push("Declared MCP servers (configured in .thclaws/mcp.json):".to_string());
        for s in &m.requires.mcp_servers {
            lines.push(format!("  - {s}"));
        }
    }
    lines.push(String::new());
    lines.push("Next: `thclaws` (CLI) or `thclaws --gui` (desktop).".to_string());
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_clean_folder_from_the_own_catalog_is_auto_trusted() {
        let own = crate::cloud::resolve_cloud_url(None, None);
        let empty = tempfile::tempdir().unwrap();
        assert!(super::may_auto_trust(empty.path(), &own, true));
        assert!(
            !super::may_auto_trust(empty.path(), &own, false),
            "a third-party agent, or a server too old to say"
        );
        assert!(!super::may_auto_trust(
            empty.path(),
            "https://catalog.evil.example",
            true
        ));

        let cloned = tempfile::tempdir().unwrap();
        std::fs::write(
            cloned.path().join(".mcp.json"),
            r#"{"mcpServers": {"x": {"command": "x"}}}"#,
        )
        .unwrap();
        assert!(
            !super::may_auto_trust(cloned.path(), &own, true),
            "a waiting .mcp.json"
        );

        let shelled = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shelled.path().join(".thclaws/gui-shell/x")).unwrap();
        assert!(
            !super::may_auto_trust(shelled.path(), &own, true),
            "a shell already there"
        );
    }

    /// A pull with media in it runs for minutes writing nothing to the
    /// folder — without a readout it reads as a hang, and users kill it
    /// mid-transfer. A chunked response has no total, and must still
    /// show movement rather than a bogus 0%.
    #[test]
    fn download_line_renders_with_and_without_a_total() {
        let known = super::download_line(5 * 1_048_576, 20 * 1_048_576);
        assert_eq!(known, "  downloading… 5.0/20.0 MB (25%)");

        let chunked = super::download_line(3 * 1_048_576, 0);
        assert_eq!(chunked, "  downloading… 3.0 MB", "no total ⇒ no percentage");

        // A body longer than its declared length can't read as >100%.
        assert!(super::download_line(21 * 1_048_576, 20 * 1_048_576).contains("(100%)"));
    }

    use super::*;

    fn write_settings(dir: &Path, json: &str) {
        let p = dir.join(".thclaws");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("settings.json"), json).unwrap();
    }

    fn read_settings(dir: &Path) -> serde_json::Value {
        let raw = std::fs::read(dir.join(".thclaws").join("settings.json")).unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    fn ent(path: &str, sha: &str) -> wssync::FileEntry {
        wssync::FileEntry {
            path: path.to_string(),
            size: sha.len() as u64,
            sha256: sha.to_string(),
        }
    }

    #[tokio::test]
    async fn revision_lines_answer_offline_when_the_folder_is_unpaired() {
        // No binding → nothing to ask the cloud about, so this must return
        // without a network call (and without a token). Keeps `/cloud
        // revision` usable on a plane / logged out.
        let dir = tempfile::tempdir().unwrap();
        let out = revision_lines(dir.path(), None, None, None).await;
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].contains("no revision recorded"), "{out:?}");
        assert!(out[1].contains("isn't paired"), "{out:?}");
    }

    #[tokio::test]
    async fn revision_lines_report_a_recorded_revision_and_age() {
        let dir = tempfile::tempdir().unwrap();
        // A binding with no slug/workspace_id still has nothing to query, so
        // this stays offline while exercising the local half's rendering.
        wssync::write_binding(
            dir.path(),
            &wssync::Binding {
                revision: Some(12),
                last_push: Some(
                    (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        - 7200)
                        .to_string(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
        let out = revision_lines(dir.path(), None, None, None).await;
        assert!(out[0].contains("rev 12"), "{out:?}");
        assert!(out[0].contains("pushed 2h ago"), "{out:?}");
    }

    #[test]
    fn drift_lines_report_clean_dirty_and_never_synced() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(cwd.join("a.txt"), "one").unwrap();

        // No base recorded → nothing to be dirty against.
        let out = local_drift_lines(cwd, None);
        assert!(out[0].contains("no sync recorded yet"), "{out:?}");

        // Record the agreed state, then ask again: clean.
        let base = wssync::build_manifest(cwd).unwrap();
        wssync::write_sync_base_manifests(cwd, &base, &base).unwrap();
        let out = local_drift_lines(cwd, Some(3));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].contains("clean") && out[0].contains("rev 3"),
            "{out:?}"
        );

        // Edit one file, add one, delete one → dirty, named, counted.
        std::fs::write(cwd.join("a.txt"), "two").unwrap();
        std::fs::write(cwd.join("b.txt"), "new").unwrap();
        let out = local_drift_lines(cwd, Some(3));
        assert!(out[0].contains("dirty — 2 changed since rev 3"), "{out:?}");
        assert!(
            out[1].contains("a.txt") && out[1].contains("b.txt"),
            "{out:?}"
        );

        std::fs::remove_file(cwd.join("a.txt")).unwrap();
        let out = local_drift_lines(cwd, Some(3));
        assert!(
            out[0].contains("1 changed, 1 deleted since rev 3"),
            "{out:?}"
        );
    }

    #[test]
    fn drift_lines_keep_engine_runtime_churn_out_of_the_dirty_count() {
        // The engine rewrites `.thclaws/state/` just by running (team status,
        // logs, locks). Counting that as the user's work marks every folder
        // dirty forever — but hiding it would be a lie, since a push carries
        // it. It gets its own tally.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join(".thclaws/state/team")).unwrap();
        std::fs::write(cwd.join("work.md"), "v1").unwrap();
        std::fs::write(cwd.join(".thclaws/state/team/status.json"), "idle").unwrap();
        let base = wssync::build_manifest(cwd).unwrap();
        wssync::write_sync_base_manifests(cwd, &base, &base).unwrap();

        // Only the engine moved → still "clean" for the user, but said out loud.
        std::fs::write(cwd.join(".thclaws/state/team/status.json"), "busy").unwrap();
        let out = local_drift_lines(cwd, Some(5));
        assert!(
            out[0].contains("clean — no work changed") && out[0].contains("1 runtime state"),
            "{out:?}"
        );

        // Real work on top → dirty, counted separately from the churn.
        std::fs::write(cwd.join("work.md"), "v2").unwrap();
        let out = local_drift_lines(cwd, Some(5));
        assert!(
            out[0].contains("dirty — 1 changed since rev 5 (+1 runtime state)"),
            "{out:?}"
        );
        assert_eq!(out[1].trim(), "work.md", "only the user's file is named");
    }

    #[test]
    fn revision_verdict_names_which_end_is_ahead() {
        // Equal counts describe the last agreed SYNC, not today's files — the
        // wording has to point at --dry-run rather than claim "identical".
        let same = revision_verdict(Some(8), Some(8));
        assert!(same.starts_with('✓'), "{same}");
        assert!(
            same.contains("rev 8") && same.contains("--dry-run"),
            "{same}"
        );

        let local_ahead = revision_verdict(Some(10), Some(7));
        assert!(
            local_ahead.contains("local is 3 rev ahead"),
            "{local_ahead}"
        );
        let cloud_ahead = revision_verdict(Some(7), Some(10));
        assert!(
            cloud_ahead.contains("cloud is 3 rev ahead"),
            "{cloud_ahead}"
        );
        assert!(cloud_ahead.contains("/cloud pull"), "{cloud_ahead}");

        // Either end unnumbered is a real state (fresh folder, older engine),
        // not an error — each gets its own explanation.
        assert!(revision_verdict(None, Some(4)).contains("no revision yet"));
        assert!(revision_verdict(Some(4), None).contains("predates revisions"));
        assert!(revision_verdict(None, None).contains("lands rev 1"));
    }

    #[test]
    fn endangered_paths_name_only_what_this_direction_destroys() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let base = vec![
            ent("mine.rs", "a"),
            ent("theirs.rs", "b"),
            ent("both.rs", "c"),
        ];
        wssync::write_sync_base_manifests(cwd, &base, &base).unwrap();
        let local = vec![
            ent("mine.rs", "a_local"),
            ent("theirs.rs", "b"),
            ent("both.rs", "c_local"),
        ];
        let remote = vec![
            ent("mine.rs", "a"),
            ent("theirs.rs", "b_cloud"),
            ent("both.rs", "c_cloud"),
        ];

        // A push destroys the CLOUD's work: its own edit plus the contested
        // file. `mine.rs` is this machine's edit — the push carries it, so it
        // is never at risk and must not be trashed.
        let mut push = endangered_paths(cwd, &local, &remote, true);
        push.sort();
        assert_eq!(push, vec!["both.rs", "theirs.rs"], "{push:?}");

        // A pull is the mirror image.
        let mut pull = endangered_paths(cwd, &local, &remote, false);
        pull.sort();
        assert_eq!(pull, vec!["both.rs", "mine.rs"], "{pull:?}");

        // Agreeing ends endanger nothing.
        assert!(endangered_paths(cwd, &base, &base, true).is_empty());

        // Without a per-file base there is nothing to name — and therefore
        // nothing `--force` can back up, which is what the guard now says.
        let fresh = tempfile::tempdir().unwrap();
        assert!(endangered_paths(fresh.path(), &local, &remote, true).is_empty());
    }

    #[test]
    fn guard_blocks_only_the_far_end_and_names_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let base = vec![ent("mine.rs", "a"), ent("theirs.rs", "b")];
        wssync::write_sync_base_manifests(cwd, &base, &base).unwrap();

        // Only THIS machine moved: a push is free to proceed, a pull would
        // destroy the edit and must stop.
        let local = vec![ent("mine.rs", "a2"), ent("theirs.rs", "b")];
        let remote = base.clone();
        assert!(guard_divergence(cwd, "ws", &local, &remote, true).is_ok());
        let err = guard_divergence(cwd, "ws", &local, &remote, false).unwrap_err();
        assert!(err.contains("1 changed on local only: mine.rs"), "{err}");
        assert!(err.contains("Push first to reconcile"), "{err}");

        // Only the CLOUD moved: mirror image.
        let remote2 = vec![ent("mine.rs", "a"), ent("theirs.rs", "b2")];
        assert!(guard_divergence(cwd, "ws", &base, &remote2, false).is_ok());
        let err = guard_divergence(cwd, "ws", &base, &remote2, true).unwrap_err();
        assert!(err.contains("1 changed on cloud only: theirs.rs"), "{err}");
        assert!(err.contains("Pull first to reconcile"), "{err}");

        // Both ends moved the SAME file differently — blocked either way.
        let local_c = vec![ent("mine.rs", "a_local"), ent("theirs.rs", "b")];
        let remote_c = vec![ent("mine.rs", "a_cloud"), ent("theirs.rs", "b")];
        for is_push in [true, false] {
            let err = guard_divergence(cwd, "ws", &local_c, &remote_c, is_push).unwrap_err();
            assert!(err.contains("1 changed on BOTH ends: mine.rs"), "{err}");
        }

        // Each end moved a DIFFERENT file: still one-directional, so each
        // direction reports only the far end's file.
        let err = guard_divergence(cwd, "ws", &local, &remote2, true).unwrap_err();
        assert!(err.contains("1 changed on cloud only: theirs.rs"), "{err}");
        assert!(!err.contains("BOTH ends"), "{err}");
        let err = guard_divergence(cwd, "ws", &local, &remote2, false).unwrap_err();
        assert!(err.contains("1 changed on local only: mine.rs"), "{err}");
    }

    #[test]
    fn guard_stays_quiet_when_only_plumbing_or_pre_base_skew_differs() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // The runner's recorded view carries a path the client strips.
        let base_local = vec![ent("src/main.rs", "m")];
        let base_remote = vec![ent("src/main.rs", "m"), ent("build/out.js", "stale")];
        wssync::write_sync_base_manifests(cwd, &base_local, &base_remote).unwrap();

        // Same skew today, plus per-end plumbing churn: neither is a change.
        let local = vec![ent("src/main.rs", "m"), ent(".thclaws/settings.json", "s1")];
        let remote = vec![
            ent("src/main.rs", "m"),
            ent("build/out.js", "stale"),
            ent(".thclaws/settings.json", "s2_gateway_overlay"),
        ];
        assert!(guard_divergence(cwd, "ws", &local, &remote, true).is_ok());
        assert!(guard_divergence(cwd, "ws", &local, &remote, false).is_ok());
    }

    #[test]
    fn guard_ignores_engine_churn_but_not_sessions_under_a_host() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let log = ".thclaws/bots/main/.thclaws/state/team/agents/lead/output.log";
        let sess = ".thclaws/bots/main/.thclaws/state/sessions/s1.jsonl";
        let base = vec![ent("plan.md", "p"), ent(log, "l0"), ent(sess, "s0")];
        wssync::write_sync_base_manifests(cwd, &base, &base).unwrap();

        let local = vec![ent("plan.md", "p"), ent(log, "l_local"), ent(sess, "s0")];
        let remote = vec![ent("plan.md", "p2"), ent(log, "l_cloud"), ent(sess, "s0")];
        assert!(guard_divergence(cwd, "ws", &local, &remote, false).is_ok());

        let local_s = vec![ent("plan.md", "p"), ent(log, "l0"), ent(sess, "s_local")];
        let remote_s = vec![ent("plan.md", "p"), ent(log, "l0"), ent(sess, "s_cloud")];
        let err = guard_divergence(cwd, "ws", &local_s, &remote_s, false).unwrap_err();
        assert!(
            err.contains("BOTH ends") && err.contains("s1.jsonl"),
            "{err}"
        );
    }

    #[test]
    fn guard_falls_back_to_the_v1_fingerprint_without_a_per_file_base() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let agreed = vec![ent("a.rs", "1")];
        let moved = vec![ent("a.rs", "2")];
        // A base written by a pre-3-way engine: fingerprint only.
        wssync::write_sync_base(cwd, &wssync::manifest_fingerprint(&agreed)).unwrap();
        assert!(wssync::read_sync_base_manifests(cwd).is_none());

        // Push judges the REMOTE side, pull judges the LOCAL side.
        assert!(guard_divergence(cwd, "ws", &moved, &agreed, true).is_ok());
        let err = guard_divergence(cwd, "ws", &agreed, &moved, true).unwrap_err();
        assert!(
            err.contains("PERMANENTLY") && !err.contains("recoverable"),
            "a v1 base can't name the at-risk files, so --force really is lossy: {err}"
        );
        assert!(guard_divergence(cwd, "ws", &agreed, &moved, false).is_ok());
        assert!(guard_divergence(cwd, "ws", &moved, &agreed, false).is_err());

        // No base at all → first sync, nothing to clobber.
        let fresh = tempfile::tempdir().unwrap();
        assert!(guard_divergence(fresh.path(), "ws", &moved, &agreed, true).is_ok());
    }

    #[test]
    fn restore_carries_gateway_proxy_over_an_agent_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        // The user's pre-install settings: on the gateway.
        let prior = br#"{"gatewayProxy": true, "model": "claude-x", "cloudUrl": "https://c"}"#;
        // What the agent's tarball extracted (no gateway/model keys).
        write_settings(
            dir.path(),
            r#"{"agent": {"id": "image-generator"}, "imageToolsEnabled": true}"#,
        );

        restore_installer_settings(dir.path(), prior).unwrap();

        let s = read_settings(dir.path());
        assert_eq!(
            s["gatewayProxy"],
            serde_json::json!(true),
            "gateway preserved"
        );
        assert_eq!(
            s["cloudUrl"],
            serde_json::json!("https://c"),
            "cloud url preserved"
        );
        // Agent-owned keys survive untouched.
        assert_eq!(s["imageToolsEnabled"], serde_json::json!(true));
        assert_eq!(s["agent"]["id"], serde_json::json!("image-generator"));
        // The user's model pick survives the overwrite — pre-fix the
        // post-get `/reload` reverted to the global default model.
        assert_eq!(
            s["model"],
            serde_json::json!("claude-x"),
            "user's model preserved"
        );
    }

    #[test]
    fn restore_keeps_a_publisher_pinned_model() {
        let dir = tempfile::tempdir().unwrap();
        let prior = br#"{"model": "user-choice"}"#;
        // Bundle explicitly pins a model → the pin wins over the prior.
        write_settings(dir.path(), r#"{"model": "publisher-pin"}"#);

        restore_installer_settings(dir.path(), prior).unwrap();

        assert_eq!(
            read_settings(dir.path())["model"],
            serde_json::json!("publisher-pin")
        );
    }

    #[test]
    fn restore_does_not_clobber_a_key_the_agent_set() {
        let dir = tempfile::tempdir().unwrap();
        let prior = br#"{"gatewayProxy": true}"#;
        // Unusual, but if the agent bundle explicitly ships gatewayProxy=false,
        // that wins — we only fill keys the agent left unset.
        write_settings(dir.path(), r#"{"gatewayProxy": false}"#);

        restore_installer_settings(dir.path(), prior).unwrap();

        assert_eq!(
            read_settings(dir.path())["gatewayProxy"],
            serde_json::json!(false)
        );
    }
}

// ---- workspace sync: /cloud push|pull (dev-plan/51) ----

/// Options for `/cloud push|pull`.
#[derive(Debug, Clone, Default)]
pub struct SyncOpts {
    pub delete: bool,
    pub dry_run: bool,
    pub workspace: Option<String>,
    pub force_rebind: bool,
    /// Skip the divergence guard — overwrite the other end even though it has
    /// changed since the last sync (the clobbered files still land in
    /// `.sync-trash/`).
    pub force: bool,
}

/// Push, streaming each progress line to `emit` the moment it is produced.
pub async fn push_streaming(
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
    opts: SyncOpts,
    emit: &mut (dyn FnMut(String) + Send),
) {
    if let Err(e) = sync_inner(cwd, cloud_url, cloud_cfg, opts, true, emit).await {
        emit(format!("push failed: {}", e));
    }
}

/// Pull, streaming each progress line to `emit` the moment it is produced.
pub async fn pull_streaming(
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
    opts: SyncOpts,
    emit: &mut (dyn FnMut(String) + Send),
) {
    if let Err(e) = sync_inner(cwd, cloud_url, cloud_cfg, opts, false, emit).await {
        emit(format!("pull failed: {}", e));
    }
}

pub async fn push_lines(
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
    opts: SyncOpts,
) -> Vec<String> {
    let mut log = Vec::new();
    push_streaming(cwd, cloud_url, cloud_cfg, opts, &mut |l| log.push(l)).await;
    log
}

pub async fn pull_lines(
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
    opts: SyncOpts,
) -> Vec<String> {
    let mut log = Vec::new();
    pull_streaming(cwd, cloud_url, cloud_cfg, opts, &mut |l| log.push(l)).await;
    log
}

async fn resolve_workspace(
    client: &Client,
    want: Option<&str>,
) -> Result<crate::cloud::client::WorkspaceSummary, String> {
    let mut wss = client.list_workspaces().await?;
    if wss.is_empty() {
        return Err("no hosted workspaces on your account — create one at /dashboard".into());
    }
    if let Some(slug) = want {
        return wss
            .into_iter()
            .find(|w| w.slug == slug)
            .ok_or_else(|| format!("no hosted workspace with slug '{}'", slug));
    }
    if wss.len() == 1 {
        return Ok(wss.remove(0));
    }
    let slugs: Vec<String> = wss.iter().map(|w| w.slug.clone()).collect();
    Err(format!(
        "you have {} workspaces — pass --workspace <slug>: {}",
        slugs.len(),
        slugs.join(", ")
    ))
}

/// Name the paths at stake, capped so a wide divergence stays readable.
fn name_paths(paths: &[String]) -> String {
    const MAX: usize = 5;
    let shown = paths
        .iter()
        .take(MAX)
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > MAX {
        format!("{}, +{} more", shown, paths.len() - MAX)
    } else {
        shown
    }
}

/// Refuse a one-directional sync that would destroy work the OTHER end did
/// since the last agreed state. Per-file when a v2 base exists (says which end
/// moved what), whole-workspace otherwise.
/// The far end's changed paths — exactly the work this direction of sync would
/// destroy. `--force` backs these up before overwriting, which is what makes
/// the guard's "recoverable in .sync-trash" line true rather than a promise.
/// Empty without a per-file base: nothing to name, so nothing to save.
fn endangered_paths(
    cwd: &Path,
    local: &[wssync::FileEntry],
    remote: &[wssync::FileEntry],
    is_push: bool,
) -> Vec<String> {
    let Some((base_local, base_remote)) = wssync::read_sync_base_manifests(cwd) else {
        return Vec::new();
    };
    let r = wssync::reconcile(&base_local, &base_remote, local, remote);
    let theirs = if is_push { r.pull } else { r.push };
    theirs.into_iter().chain(r.conflicts).collect()
}

fn guard_divergence(
    cwd: &Path,
    slug: &str,
    local: &[wssync::FileEntry],
    remote: &[wssync::FileEntry],
    is_push: bool,
) -> Result<(), String> {
    let Some((base_local, base_remote)) = wssync::read_sync_base_manifests(cwd) else {
        // Pre-3-way base (or none): fall back to the fingerprint of whichever
        // side this sync would overwrite.
        let overwritten = if is_push { remote } else { local };
        if wssync::diverged_from_base(cwd, overwritten) {
            // No per-file base ⇒ no way to name (and therefore back up) the
            // at-risk files, so `--force` here really is destructive. Say so
            // instead of the ".sync-trash" reassurance the v2 path can honour.
            return Err(if is_push {
                format!("'{}' has changes since your last sync — pushing would overwrite them PERMANENTLY (this workspace has no per-file sync base, so they can't be backed up). Pull first to reconcile, or re-run with --force to accept the loss.", slug)
            } else {
                format!("this folder has changes since your last sync — pulling '{}' would overwrite them PERMANENTLY (no per-file sync base, so they can't be backed up). Push first to reconcile, or re-run with --force to accept the loss.", slug)
            });
        }
        return Ok(());
    };
    let mut r = wssync::reconcile(&base_local, &base_remote, local, remote);
    // Engine churn changes on both ends just by running; it still rides the
    // sync (and --force still backs it up) but never blocks it.
    for v in [&mut r.push, &mut r.pull, &mut r.conflicts] {
        v.retain(|p| !wssync::is_engine_churn(p));
    }
    // Only the far end's edits are at risk: a push is free to overwrite files
    // that only this machine changed, and vice-versa.
    let theirs = if is_push { &r.pull } else { &r.push };
    if theirs.is_empty() && r.conflicts.is_empty() {
        return Ok(());
    }
    let (head, side, fix) = if is_push {
        (
            format!("'{}' changed on the cloud since your last sync — pushing would overwrite that work", slug),
            "cloud",
            "Pull first to reconcile",
        )
    } else {
        (
            format!(
                "this folder changed since your last sync — pulling '{}' would overwrite that work",
                slug
            ),
            "local",
            "Push first to reconcile",
        )
    };
    let mut msg = format!("{} (recoverable in .sync-trash):", head);
    if !theirs.is_empty() {
        msg.push_str(&format!(
            "\n  {} changed on {} only: {}",
            theirs.len(),
            side,
            name_paths(theirs)
        ));
    }
    if !r.conflicts.is_empty() {
        msg.push_str(&format!(
            "\n  {} changed on BOTH ends: {}",
            r.conflicts.len(),
            name_paths(&r.conflicts)
        ));
    }
    msg.push_str(&format!("\n{}, or re-run with --force.", fix));
    Err(msg)
}

async fn sync_inner(
    cwd: &Path,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
    opts: SyncOpts,
    is_push: bool,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<(), String> {
    if let Some(msg) = refuse_in_multiuser(if is_push {
        "/cloud push"
    } else {
        "/cloud pull"
    }) {
        return Err(msg);
    }
    // dev-plan/51 #3: both ends must be idle. Refuse if a local turn is running.
    if crate::agent_activity::busy_count() > 0 {
        return Err("a local turn is active — wait for it to finish before syncing".into());
    }
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    if token.is_none() {
        return Err("not logged in — paste your CLI token in Settings → thClaws.cloud".into());
    }
    emit("Connecting to thClaws.cloud…".into());
    let client = Client::new(&url, token);
    let ws = resolve_workspace(&client, opts.workspace.as_deref()).await?;
    emit(format!("Workspace: {} ({})", ws.slug, ws.id));
    let jwt = client.cli_exchange().await?;
    // Probe the runner directly — status strings ("ready"/"running") aren't a
    // reliable "is it up" signal, so try /sync/stat and only wake on failure.
    let stat = match client.ws_sync_stat(&ws.url, &jwt).await {
        Ok(s) => s,
        Err(e) if e.contains("404") => {
            return Err(format!(
                "'{}' is up but its engine doesn't expose /workspace/sync yet — \
                 restart it (pause→resume) to pick up the v0.81+ engine",
                ws.slug
            ));
        }
        Err(_) => {
            emit(format!(
                "Workspace not responding ({}) — resuming…",
                ws.status
            ));
            client.wake_workspace(&ws.id).await?;
            wait_for_runner(&client, &ws.url, &jwt, emit).await?
        }
    };
    if stat.busy {
        return Err("the cloud workspace has an active turn — try again when it's idle".into());
    }
    let local_binding = wssync::read_binding(cwd);
    let local_bound = local_binding.workspace_id.clone();
    let bound_note = local_bound
        .as_deref()
        .map(|l| format!(" (folder bound to {})", l))
        .unwrap_or_default();
    // The number this sync will land on if it completes. Computed up front so
    // the dry-run preview can quote the same value the real run would write.
    let revision = wssync::next_revision(&local_binding, &ws.id, stat.revision);
    // The ends disagreeing means one of them synced without the other (a
    // second machine, a --force, a run that died after the far end committed).
    // Worth a line — it's the cheapest signal that a folder isn't the only
    // thing touching this workspace.
    if local_bound.as_deref() == Some(ws.id.as_str()) {
        let (mine, theirs) = (local_binding.revision, stat.revision);
        if let (Some(m), Some(t)) = (mine, theirs) {
            if m != t {
                emit(format!(
                    "Note: local is at rev {} but the cloud is at rev {} — the ends synced separately.",
                    m, t
                ));
            }
        }
    }

    if is_push {
        if !stat.empty && local_bound.as_deref() != Some(ws.id.as_str()) && !opts.force_rebind {
            return Err(format!(
                "cloud workspace '{}' is not empty and this folder isn't bound to it{} — re-run with --force-rebind to overwrite it deliberately",
                ws.slug, bound_note
            ));
        }
        // P2: incremental when the runner exposes a manifest; else full tarball.
        match client.ws_sync_manifest(&ws.url, &jwt).await? {
            Some(remote) => {
                let local = wssync::build_manifest(cwd)?;
                // Divergence guard: refuse to clobber work the cloud did since
                // the last sync. Best-effort — only checkable here, where the
                // runner exposes a manifest to compare against.
                if !opts.force {
                    guard_divergence(cwd, &ws.slug, &local, &remote, true)?;
                }
                let (upload, extraneous) = wssync::diff(&local, &remote);
                if opts.dry_run {
                    emit(format!(
                        "[dry-run] incremental push → '{}': {} file(s) to upload{} (would land rev {})",
                        ws.slug,
                        upload.len(),
                        if opts.delete {
                            format!(", {} to delete on cloud", extraneous.len())
                        } else {
                            String::new()
                        },
                        revision
                    ));
                    return Ok(());
                }
                // `--force` skipped the guard, so the cloud-side work it named
                // is about to be overwritten in place. Move those files to the
                // runner's `.sync-trash/` FIRST — extraction overwrites without
                // a backup, so this is the only moment they can be saved.
                if opts.force {
                    let at_risk = endangered_paths(cwd, &local, &remote, true);
                    if !at_risk.is_empty() {
                        emit(format!(
                            "--force: moving {} cloud file(s) to .sync-trash before overwriting…",
                            at_risk.len()
                        ));
                        client.ws_sync_trash(&ws.url, &jwt, &at_risk).await?;
                    }
                }
                emit(format!("Pushing {} changed file(s)…", upload.len()));
                let tmp = pack_paths_temp(cwd, &upload)?;
                let r = push_with_progress(&client, &ws.url, &jwt, tmp.path(), false, &ws.id, emit)
                    .await?;
                let deleted = if opts.delete && !extraneous.is_empty() {
                    client
                        .ws_sync_trash(&ws.url, &jwt, &extraneous)
                        .await?
                        .deleted
                } else {
                    0
                };
                write_push_binding(cwd, &ws, &url, &local_binding, revision)?;
                emit(format!(
                    "✓ pushed {} file(s){} to '{}' (incremental) — rev {}",
                    r.written,
                    if deleted > 0 {
                        format!(", deleted {}", deleted)
                    } else {
                        String::new()
                    },
                    ws.slug,
                    revision
                ));
            }
            None => {
                let local = wssync::stat_workspace(cwd)?;
                if opts.dry_run {
                    emit(format!(
                        "[dry-run] would push {} local file(s) → cloud '{}' (cloud has {}){} (would land rev {})",
                        local.file_count,
                        ws.slug,
                        stat.file_count,
                        if opts.delete {
                            ", deleting extraneous on cloud"
                        } else {
                            ""
                        },
                        revision
                    ));
                    return Ok(());
                }
                emit(format!(
                    "Packing {} file(s) ({:.1} KB)…",
                    local.file_count,
                    local.bytes as f64 / 1024.0
                ));
                let tmp = pack_workspace_temp(cwd, false)?;
                let r = push_with_progress(
                    &client,
                    &ws.url,
                    &jwt,
                    tmp.path(),
                    opts.delete,
                    &ws.id,
                    emit,
                )
                .await?;
                write_push_binding(cwd, &ws, &url, &local_binding, revision)?;
                emit(format!(
                    "✓ pushed {} file(s){} to '{}' — rev {}",
                    r.written,
                    if r.deleted > 0 {
                        format!(", deleted {}", r.deleted)
                    } else {
                        String::new()
                    },
                    ws.slug,
                    revision
                ));
            }
        }
    } else {
        let local_empty = wssync::is_empty(cwd)?;
        if !local_empty && local_bound.as_deref() != Some(ws.id.as_str()) && !opts.force_rebind {
            return Err(format!(
                "local folder is not empty and isn't bound to '{}'{} — re-run with --force-rebind to overwrite it deliberately",
                ws.slug, bound_note
            ));
        }
        // `local_manifest` is both the divergence-guard input and the
        // incremental branch's diff input — hash the tree once.
        let local_manifest = wssync::build_manifest(cwd)?;
        match client.ws_sync_manifest(&ws.url, &jwt).await? {
            Some(remote) => {
                // Divergence guard: refuse to clobber local work done since the
                // last sync. Needs both sides, so it sits after the fetch.
                if !opts.force {
                    guard_divergence(cwd, &ws.slug, &local_manifest, &remote, false)?;
                }
                let local = local_manifest;
                let (download, extraneous) = wssync::diff(&remote, &local);
                if opts.dry_run {
                    emit(format!(
                        "[dry-run] incremental pull ← '{}': {} file(s) to download{} (would land rev {})",
                        ws.slug,
                        download.len(),
                        if opts.delete {
                            format!(", {} to delete locally", extraneous.len())
                        } else {
                            String::new()
                        },
                        revision
                    ));
                    return Ok(());
                }
                // Mirror of the push side: save this folder's own at-risk work
                // before the incoming tarball overwrites it in place.
                if opts.force {
                    let at_risk = endangered_paths(cwd, &local, &remote, false);
                    if !at_risk.is_empty() {
                        emit(format!(
                            "--force: moving {} local file(s) to .sync-trash before overwriting…",
                            at_risk.len()
                        ));
                        wssync::trash_paths(cwd, &at_risk)?;
                    }
                }
                emit(format!("Pulling {} changed file(s)…", download.len()));
                if !download.is_empty() {
                    let tmp = pull_with_progress(
                        |p| client.ws_sync_export(&ws.url, &jwt, &download, p),
                        emit,
                    )
                    .await?;
                    let f =
                        std::fs::File::open(tmp.path()).map_err(|e| format!("open temp: {}", e))?;
                    emit(format!("  unpacking {} file(s)…", download.len()));
                    for n in wssync::untar_workspace_from(f, cwd, false)?.notes {
                        emit(format!("  ⚠ {n}"));
                    }
                }
                let deleted = if opts.delete && !extraneous.is_empty() {
                    wssync::trash_paths(cwd, &extraneous)?.deleted
                } else {
                    0
                };
                write_pull_binding(cwd, &ws, &url, &local_binding, revision)?;
                emit(format!(
                    "✓ pulled {} file(s){} into {} (incremental) — rev {}",
                    download.len(),
                    if deleted > 0 {
                        format!(", deleted {}", deleted)
                    } else {
                        String::new()
                    },
                    cwd.display(),
                    revision
                ));
            }
            None => {
                // Runner predates the manifest endpoint — no cloud-side view to
                // reconcile against, so guard on the local fingerprint alone.
                if !opts.force && wssync::diverged_from_base(cwd, &local_manifest) {
                    return Err(format!(
                        "this folder has changes since your last sync — pulling '{}' would overwrite them (recoverable in .sync-trash). Push first to reconcile, or re-run with --force.",
                        ws.slug
                    ));
                }
                if opts.dry_run {
                    let local = wssync::stat_workspace(cwd)?;
                    emit(format!(
                        "[dry-run] would pull cloud '{}' ({} file(s)) → local (has {}){} (would land rev {})",
                        ws.slug,
                        stat.file_count,
                        local.file_count,
                        if opts.delete {
                            ", deleting extraneous locally"
                        } else {
                            ""
                        },
                        revision
                    ));
                    return Ok(());
                }
                emit(format!(
                    "Pulling cloud '{}' ({} file(s))…",
                    ws.slug, stat.file_count
                ));
                let tmp =
                    pull_with_progress(|p| client.ws_sync_pull(&ws.url, &jwt, false, p), emit)
                        .await?;
                let f = std::fs::File::open(tmp.path()).map_err(|e| format!("open temp: {}", e))?;
                emit(format!("  unpacking {} file(s)…", stat.file_count));
                let r = wssync::untar_workspace_from(f, cwd, opts.delete)?;
                for n in &r.notes {
                    emit(format!("  ⚠ {n}"));
                }
                write_pull_binding(cwd, &ws, &url, &local_binding, revision)?;
                emit(format!(
                    "✓ pulled {} file(s){} into {} — rev {}",
                    r.written,
                    if r.deleted > 0 {
                        format!(", deleted {}", r.deleted)
                    } else {
                        String::new()
                    },
                    cwd.display(),
                    revision
                ));
            }
        }
    }
    // Record the now-agreed content state so the NEXT sync can tell WHICH end
    // drifted, per file. Best-effort — a watermark failure must not fail the
    // sync itself. (dry-run paths already returned above.)
    //
    // Store BOTH ends' manifests, each re-read from its own end. A local
    // manifest can legitimately differ from the runner's export for identical
    // work — e.g. when the client's sync strip rules change (build/ handling)
    // before the runner's do — so each side must be judged against its OWN
    // recorded view. Comparing local-to-local and runner-to-runner keeps that
    // skew from reading as perpetual divergence and forcing `--force` forever.
    match client.ws_sync_manifest(&ws.url, &jwt).await {
        Ok(Some(remote)) => {
            if let Ok(local) = wssync::build_manifest(cwd) {
                let _ = wssync::write_sync_base_manifests(cwd, &local, &remote);
            }
        }
        Ok(None) => {
            // Runner predates the manifest endpoint (P2 404) — no cloud-side
            // view exists, so fall back to the v1 whole-workspace watermark.
            if let Ok(local) = wssync::build_manifest(cwd) {
                let _ = wssync::write_sync_base(cwd, &wssync::manifest_fingerprint(&local));
            }
        }
        // Transient fetch failure: keep the previous base rather than record a
        // half-known one that would misattribute the next change.
        Err(_) => {}
    }
    // Tell the runner which revision this sync landed on, so both ends quote
    // the same number. One endpoint serves both directions — a pull agrees on
    // a revision just as much as a push does. Best-effort: the local binding
    // already holds it, and `next_revision` takes the max of the two ends, so
    // a runner that missed this call re-converges on the next sync instead of
    // reusing a number.
    match client
        .ws_sync_set_revision(&ws.url, &jwt, revision, &ws.id)
        .await
    {
        // The runner refuses to count backwards, so it can answer with a
        // HIGHER number than we asked for (its binding had already moved past
        // the stat we read). Adopt it locally — the whole point is that a
        // revision names the same state on both ends.
        Ok(Some(landed)) if landed != revision => {
            let mut b = wssync::read_binding(cwd);
            b.revision = Some(landed);
            let _ = wssync::write_binding(cwd, &b);
            emit(format!(
                "Note: the cloud was already past rev {} — this sync is rev {}.",
                revision, landed
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Upload the tarball, emitting a live percentage readout for uploads large
/// enough that the transfer is the slow part. Small tarballs skip the ticker
/// (the surrounding "Pushing…"/"✓ pushed" lines already bracket them).
/// Pack the synced workspace to a temp `.tar.gz` (streamed to disk).
fn pack_workspace_temp(
    cwd: &Path,
    include_runtime: bool,
) -> Result<tempfile::NamedTempFile, String> {
    let tmp = tempfile::NamedTempFile::new().map_err(|e| format!("temp: {}", e))?;
    wssync::tar_workspace_to(cwd, include_runtime, tmp.as_file())?;
    Ok(tmp)
}

/// Pack a specific path list to a temp `.tar.gz` (streamed to disk).
fn pack_paths_temp(cwd: &Path, paths: &[String]) -> Result<tempfile::NamedTempFile, String> {
    let tmp = tempfile::NamedTempFile::new().map_err(|e| format!("temp: {}", e))?;
    wssync::tar_paths_to(cwd, paths, tmp.as_file())?;
    Ok(tmp)
}

/// Run a download and narrate it. A workspace with media in it takes
/// minutes to arrive, and the pull wrote nothing to the folder until the
/// tarball finished — so the app looked hung and users killed it
/// mid-transfer. Ticks every 300 ms while the bytes land; falls silent
/// (no ticker at all) once the transfer is done.
async fn pull_with_progress<F, Fut>(
    start: F,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<tempfile::NamedTempFile, String>
where
    F: FnOnce(Option<std::sync::Arc<crate::cloud::client::DownloadProgress>>) -> Fut,
    Fut: std::future::Future<Output = Result<tempfile::NamedTempFile, String>>,
{
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    let progress = Arc::new(crate::cloud::client::DownloadProgress::default());
    let fut = start(Some(progress.clone()));
    tokio::pin!(fut);
    let mut last = String::new();
    // The cloud packs the tarball before the first byte moves — on a
    // few-hundred-file workspace that's the longest silent stretch of the
    // whole pull, and the one that reads as a hang. Say so once.
    let started = std::time::Instant::now();
    let mut said_waiting = false;
    loop {
        tokio::select! {
            r = &mut fut => return r,
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {
                let done = progress.received.load(Ordering::Relaxed);
                if done == 0 {
                    if !said_waiting && started.elapsed() >= std::time::Duration::from_secs(2) {
                        said_waiting = true;
                        emit("  waiting for the cloud to pack the files…".into());
                    }
                    continue;
                }
                let total = progress.total.load(Ordering::Relaxed);
                let line = download_line(done, total);
                // The percentage moves slower than the ticker on a big
                // tarball; don't repeat the same line at it.
                if line != last {
                    emit(line.clone());
                    last = line;
                }
            }
        }
    }
}

/// Render one progress line. Split out of the ticker so the formatting
/// (and the unknown-total case) is testable without a live transfer.
fn download_line(done: u64, total: u64) -> String {
    if total > 0 {
        format!(
            "  downloading… {:.1}/{:.1} MB ({}%)",
            done as f64 / 1_048_576.0,
            total as f64 / 1_048_576.0,
            done.min(total) * 100 / total
        )
    } else {
        format!("  downloading… {:.1} MB", done as f64 / 1_048_576.0)
    }
}

async fn push_with_progress(
    client: &Client,
    ws_url: &str,
    jwt: &str,
    tarball_path: &Path,
    delete: bool,
    workspace_id: &str,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<crate::cloud::client::SyncPushResp, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    const TICKER_THRESHOLD: u64 = 256 * 1024;
    let total = std::fs::metadata(tarball_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if total < TICKER_THRESHOLD {
        return client
            .ws_sync_push(ws_url, jwt, tarball_path, delete, workspace_id, None)
            .await;
    }
    let sent = Arc::new(AtomicU64::new(0));
    let fut = client.ws_sync_push(
        ws_url,
        jwt,
        tarball_path,
        delete,
        workspace_id,
        Some(sent.clone()),
    );
    tokio::pin!(fut);
    let total_mb = total as f64 / 1_048_576.0;
    let mut last_pct = u64::MAX;
    loop {
        tokio::select! {
            r = &mut fut => return r,
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {
                let done = sent.load(Ordering::Relaxed).min(total);
                let pct = done * 100 / total;
                if pct != last_pct {
                    last_pct = pct;
                    emit(format!(
                        "  uploading… {:.1}/{:.1} MB ({}%)",
                        done as f64 / 1_048_576.0,
                        total_mb,
                        pct
                    ));
                }
            }
        }
    }
}

/// Poll the runner's `/sync/stat` until it answers (after a resume) or times out.
async fn wait_for_runner(
    client: &Client,
    ws_url: &str,
    jwt: &str,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<crate::cloud::client::SyncStatResp, String> {
    let mut last = String::new();
    for attempt in 0..30 {
        match client.ws_sync_stat(ws_url, jwt).await {
            Ok(s) => return Ok(s),
            Err(e) if e.contains("404") => {
                return Err(format!(
                    "engine doesn't expose /workspace/sync — restart the workspace \
                     for the v0.81+ engine ({e})"
                ));
            }
            Err(e) => last = e,
        }
        if attempt == 0 {
            emit("Waiting for the workspace to come up…".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(format!(
        "workspace didn't become reachable in time: {}",
        last
    ))
}

fn write_push_binding(
    cwd: &Path,
    ws: &crate::cloud::client::WorkspaceSummary,
    url: &str,
    prev: &wssync::Binding,
    revision: u64,
) -> Result<(), String> {
    wssync::write_binding(
        cwd,
        &wssync::Binding {
            workspace_id: Some(ws.id.clone()),
            slug: Some(ws.slug.clone()),
            cloud_url: Some(url.to_string()),
            last_push: Some(now_string()),
            last_pull: prev.last_pull.clone(),
            revision: Some(revision),
        },
    )
}

fn write_pull_binding(
    cwd: &Path,
    ws: &crate::cloud::client::WorkspaceSummary,
    url: &str,
    prev: &wssync::Binding,
    revision: u64,
) -> Result<(), String> {
    wssync::write_binding(
        cwd,
        &wssync::Binding {
            workspace_id: Some(ws.id.clone()),
            slug: Some(ws.slug.clone()),
            cloud_url: Some(url.to_string()),
            last_push: prev.last_push.clone(),
            last_pull: Some(now_string()),
            revision: Some(revision),
        },
    )
}

fn now_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// `/publish <file.html>` — put a self-contained page on the web.
///
/// The one cloud command that works logged out. A token is not a gate
/// here, only a lifetime: an hour without, three days with. That
/// asymmetry is the whole design — "show someone this page" has to work
/// the moment the agent finishes writing it, not after a signup.
/// The publish itself, as structured data.
///
/// Split out from [`publish_app_lines`] because the two surfaces want
/// different things from one upload: the CLI wants a block of text, the
/// chat wants the fields so it can build a real message with a markdown
/// link. Sandbox and file validation live here once instead of being
/// copied into a second caller and drifting.
pub async fn publish_app_result(
    path: &str,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<crate::cloud::client::PublishedApp, String> {
    let p = std::path::Path::new(path);
    // Read through the sandbox so /publish can't be talked into
    // uploading a file outside the workspace — this command sends bytes
    // to a public URL, which makes an arbitrary-read a data leak.
    let resolved = crate::sandbox::Sandbox::check(path)
        .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    let body =
        std::fs::read(&resolved).map_err(|e| format!("cannot read {}: {e}", resolved.display()))?;
    // Advisory, not enforced: the server accepts any UTF-8 text. Saying
    // so early beats a confusing render later.
    if !path.to_ascii_lowercase().ends_with(".html") && !path.to_ascii_lowercase().ends_with(".htm")
    {
        return Err(format!(
            "{} doesn't look like an HTML file. /publish serves whatever it \
             uploads as text/html, so a non-HTML file will render as markup.",
            p.display()
        ));
    }

    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let client = Client::new(&url, crate::cloud::token());
    client
        .publish_app(body)
        .await
        .map_err(|e| format!("publish failed: {e}"))
}

/// The one line of facts about a published app, shared by the CLI block
/// and the chat message so the two cannot drift apart.
pub fn publish_detail_line(app: &crate::cloud::client::PublishedApp) -> String {
    format!(
        "{}  ·  {:.0} KB  ·  expires {}",
        app.title.as_deref().unwrap_or("(untitled)"),
        app.size_bytes as f64 / 1024.0,
        app.expires_at,
    )
}

pub async fn publish_app_lines(
    path: &str,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    match publish_app_result(path, cloud_url, cloud_cfg).await {
        Err(e) => vec![e],
        Ok(app) => {
            let mut lines = vec![
                app.url.clone(),
                String::new(),
                format!("  {}", publish_detail_line(&app)),
            ];
            // `app.anonymous` rather than a local `token.is_none()`:
            // same fact, but read from the response that actually
            // decided the expiry, so the sentence below cannot contradict
            // the `expires` above it.
            if app.anonymous {
                // Only reachable once the server opens the anonymous
                // tier — until then a token-less publish is refused with
                // its own message. Told here, at the moment it matters,
                // rather than buried in docs nobody reads first.
                lines.push(String::new());
                lines.push(
                    "  Published anonymously, so the link lives 1 hour. Save a \
                     thClaws.cloud token in Settings for 3 days."
                        .into(),
                );
            }
            lines
        }
    }
}
