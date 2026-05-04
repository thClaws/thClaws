//! Slash-command dispatcher for the GUI's shared session.
//!
//! GUI counterpart to the inline match arms in `repl::run_repl`. Takes
//! the same `SlashCommand` enum the standalone REPL parses, but writes
//! its output to a `broadcast::Sender<ViewEvent>` instead of `println!`
//! — so both the Terminal and Chat tabs render the command's output
//! identically.
//!
//! Commands that mutate runtime state (model / provider / permissions
//! / thinking budget) take `&mut WorkerState` and rebuild the Agent
//! in-place when needed.

#![cfg(feature = "gui")]

use crate::repl::{default_model_for_provider, parse_slash, render_help, SlashCommand};
use crate::session::Session;
use crate::shared_session::{
    build_session_list, save_history, DisplayMessage, ViewEvent, WorkerState,
};
use crate::util::{format_bytes, format_tokens, progress_bar};
use tokio::sync::broadcast;

/// Entry point — dispatch a single slash line against the shared
/// worker state, writing user-visible output to `events_tx` as
/// `SlashOutput` events.
///
/// `input_tx` is needed for commands that spawn background tasks which
/// re-feed input back into the worker queue (M6.29: `/loop` start —
/// the spawned task fires `<body>` every interval via `input_tx.send`).
/// Most commands ignore it.
pub async fn dispatch(
    line: &str,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    input_tx: &std::sync::mpsc::Sender<crate::shared_session::ShellInput>,
) {
    let Some(cmd) = parse_slash(line) else {
        emit(events_tx, format!("Not a slash command: {line}"));
        return;
    };

    match cmd {
        // ─── read-only status ───────────────────────────────────────
        SlashCommand::Help => emit(events_tx, render_help().to_string()),
        SlashCommand::Quit => {
            // CLI handles `/quit` via an early break in the REPL loop
            // and never reaches this dispatch (repl.rs `SlashCommand::
            // Quit => break`). What lands here is the GUI's chat-input
            // path: pop a native confirm dialog, then signal the event
            // loop on accept. Cancel leaves the session running. #52.
            #[cfg(feature = "gui")]
            {
                let confirmed = tokio::task::spawn_blocking(|| {
                    crate::gui::native_confirm("Quit thclaws", "Quit?", "OK", "Cancel")
                })
                .await
                .unwrap_or(false);
                if confirmed {
                    let _ = events_tx.send(ViewEvent::QuitRequested);
                }
            }
            #[cfg(not(feature = "gui"))]
            {
                // Defensive — no GUI feature means no chat input route
                // either, but if a future caller wires this up the
                // user gets a clear next-step instead of silence.
                emit(events_tx, "Use Ctrl+C to quit.".into());
            }
        }
        SlashCommand::Version => emit(events_tx, crate::version::one_line()),
        SlashCommand::Cwd => {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "?".to_string());
            emit(events_tx, format!("cwd: {cwd}"));
        }
        SlashCommand::Context => {
            let history = state.agent.history_snapshot();
            let blocks: usize = history.iter().map(|m| m.content.len()).sum();
            // Token estimate + percentage of the model's real context
            // window. Same estimator the auto-compact trigger uses, so
            // the number here and the 80% threshold line up.
            let history_tokens = crate::compaction::estimate_messages_tokens(&history);
            // System prompt ~1 token per 4 chars (same rule-of-thumb
            // the rest of the estimator uses).
            let system_tokens = state.system_prompt.len() / 4;
            let total_tokens = history_tokens + system_tokens;
            let window = state.agent.budget_tokens.max(1);
            let pct = (total_tokens as f64 / window as f64) * 100.0;
            // Per-contributor size breakdown. Lets the user spot which
            // file is bloating the system prompt — e.g. an AGENTS.md
            // that ballooned past the ch08 soft budget or a
            // `project_context.md` that grew over weeks of auto-memory
            // writes. Each budget check appends "⚠" when exceeded.
            const BUDGET_CLAUDE_MD: u64 = 1024; // 1 KB per file
            const BUDGET_MEMORY_INDEX: u64 = 512; // 500 B (manual)
            const BUDGET_MEMORY_ENTRY: u64 = 1024; // 1 KB per topic
            let claude_files = crate::context::scan_claude_md_sizes(&state.cwd);
            let claude_total: u64 = claude_files.iter().map(|(_, n)| *n).sum();
            let claude_over: Vec<String> = claude_files
                .iter()
                .filter(|(_, n)| *n > BUDGET_CLAUDE_MD)
                .map(|(p, n)| {
                    format!(
                        "{} ({})",
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.display().to_string()),
                        format_bytes(*n),
                    )
                })
                .collect();
            let (mem_index_bytes, mem_entries) = crate::memory::MemoryStore::default_path()
                .map(crate::memory::MemoryStore::new)
                .map(|s| crate::memory::memory_sizes(&s))
                .unwrap_or((0, Vec::new()));
            let mem_entries_total: u64 = mem_entries.iter().map(|(_, n)| *n).sum();
            let mem_entries_over: Vec<String> = mem_entries
                .iter()
                .filter(|(_, n)| *n > BUDGET_MEMORY_ENTRY)
                .map(|(name, n)| format!("{} ({})", name, format_bytes(*n)))
                .collect();

            let mut out = format!(
                "context: {} message(s), {} content block(s), system prompt {} chars\n\
                 model: {} · window: {} tokens · used: ~{} tokens\n\
                 {} {:.1}%",
                history.len(),
                blocks,
                state.system_prompt.len(),
                state.config.model,
                format_tokens(window),
                format_tokens(total_tokens),
                progress_bar(pct, 24),
                pct,
            );
            if !claude_files.is_empty() || mem_index_bytes > 0 || !mem_entries.is_empty() {
                out.push_str("\nsystem-prompt breakdown:");
                if !claude_files.is_empty() {
                    out.push_str(&format!(
                        "\n  CLAUDE.md / AGENTS.md  {}  ({} file{})",
                        format_bytes(claude_total),
                        claude_files.len(),
                        if claude_files.len() == 1 { "" } else { "s" },
                    ));
                    if !claude_over.is_empty() {
                        out.push_str(&format!(
                            "  ⚠ over {} cap: {}",
                            format_bytes(BUDGET_CLAUDE_MD),
                            claude_over.join(", "),
                        ));
                    }
                }
                if mem_index_bytes > 0 {
                    out.push_str(&format!(
                        "\n  MEMORY.md              {}",
                        format_bytes(mem_index_bytes),
                    ));
                    if mem_index_bytes > BUDGET_MEMORY_INDEX {
                        out.push_str(&format!(
                            "  ⚠ over {} cap",
                            format_bytes(BUDGET_MEMORY_INDEX),
                        ));
                    }
                }
                if !mem_entries.is_empty() {
                    out.push_str(&format!(
                        "\n  memory entries         {}  ({} file{})",
                        format_bytes(mem_entries_total),
                        mem_entries.len(),
                        if mem_entries.len() == 1 { "" } else { "s" },
                    ));
                    if !mem_entries_over.is_empty() {
                        out.push_str(&format!(
                            "  ⚠ over {} cap: {}",
                            format_bytes(BUDGET_MEMORY_ENTRY),
                            mem_entries_over.join(", "),
                        ));
                    }
                }
            }
            emit(events_tx, out);
        }
        SlashCommand::History => {
            let history = state.agent.history_snapshot();
            let mut out = format!("{} message(s) in history\n", history.len());
            for (i, m) in history.iter().enumerate() {
                out.push_str(&format!(
                    "  [{i}] {:?} — {} block(s)\n",
                    m.role,
                    m.content.len(),
                ));
            }
            emit(events_tx, out);
        }
        SlashCommand::Tasks => {
            // The `Task` tool maintains its state inside the agent's
            // turn loop; from outside the loop we can only hint.
            emit(
                events_tx,
                "tasks are maintained by the agent's `Task` tool during a turn; ask the agent to list them.".into(),
            );
        }
        SlashCommand::Usage => {
            let tracker =
                crate::usage::UsageTracker::new(crate::usage::UsageTracker::default_path());
            emit(events_tx, tracker.summary());
        }
        SlashCommand::Doctor => emit(events_tx, doctor_report(state)),

        // ─── model / provider / catalogue ───────────────────────────
        SlashCommand::Providers => {
            let current = state.config.detect_provider_kind().ok();
            let mut out = String::from("Providers:\n");
            for kind in crate::providers::ProviderKind::ALL {
                let marker = if Some(*kind) == current { "*" } else { " " };
                out.push_str(&format!(
                    "  {marker} {:<12} → {}\n",
                    kind.name(),
                    kind.default_model(),
                ));
            }
            emit(events_tx, out);
        }
        SlashCommand::ModelsSetContext { key, size, project } => {
            let scope = if project {
                crate::model_catalogue::OverrideScope::Project
            } else {
                crate::model_catalogue::OverrideScope::User
            };
            let entry = crate::model_catalogue::ModelEntry {
                context: Some(size),
                max_output: None,
                source: Some("override".into()),
                verified_at: None,
            };
            let cat = crate::model_catalogue::EffectiveCatalogue::load();
            let warn = cat.lookup_exact(&key).map(|n| size > n).unwrap_or(false);
            match crate::model_catalogue::save_override(&key, Some(entry), scope) {
                Ok(path) => {
                    emit(
                        events_tx,
                        format!(
                            "override → {key}: {size} tokens (saved to {})",
                            path.display()
                        ),
                    );
                    if warn {
                        emit(
                            events_tx,
                            "warning: override exceeds catalogue value — provider may reject"
                                .into(),
                        );
                    }
                }
                Err(e) => emit(events_tx, format!("set-context failed: {e}")),
            }
        }
        SlashCommand::ModelsUnsetContext { key, project } => {
            let scope = if project {
                crate::model_catalogue::OverrideScope::Project
            } else {
                crate::model_catalogue::OverrideScope::User
            };
            match crate::model_catalogue::save_override(&key, None, scope) {
                Ok(path) => emit(
                    events_tx,
                    format!("override removed for {key} (in {})", path.display()),
                ),
                Err(e) => emit(events_tx, format!("unset-context failed: {e}")),
            }
        }
        SlashCommand::ModelsRefresh => {
            emit(events_tx, "refreshing model catalogue…".into());
            match crate::model_catalogue::refresh_from_remote().await {
                Ok(out) => emit(
                    events_tx,
                    format!(
                        "catalogue refreshed: {} models (source: {})",
                        out.model_count,
                        if out.source.is_empty() {
                            "unspecified".into()
                        } else {
                            out.source
                        }
                    ),
                ),
                Err(e) => emit(
                    events_tx,
                    format!(
                        "catalogue refresh failed: {e} (keeping existing {})",
                        if crate::model_catalogue::cache_path()
                            .map(|p| p.exists())
                            .unwrap_or(false)
                        {
                            "cache"
                        } else {
                            "embedded baseline"
                        }
                    ),
                ),
            }
        }
        SlashCommand::Models => {
            fn format_tokens(n: u32) -> String {
                if n >= 1_000_000 {
                    let m = n as f64 / 1_000_000.0;
                    if (m - m.round()).abs() < 0.05 {
                        format!("{:.0}M", m)
                    } else {
                        format!("{:.1}M", m)
                    }
                } else if n >= 1_000 {
                    format!("{}K", n / 1_000)
                } else {
                    n.to_string()
                }
            }
            let kind = match state.config.detect_provider_kind() {
                Ok(k) => k,
                Err(e) => {
                    emit(events_tx, format!("provider error: {e}"));
                    return;
                }
            };
            let cat = crate::model_catalogue::EffectiveCatalogue::load();
            let provider_name = crate::model_catalogue::provider_kind_name(kind);

            // Collect ids from the catalogue (baseline ∪ user cache, with
            // cache winning on metadata). This is the list we render for
            // every non-Ollama provider.
            let mut rows = cat.list_models_for_provider(provider_name);

            // Ollama is per-machine, so the catalogue alone can't know what
            // the user has pulled — hit `/api/tags` too and union any new
            // ids (without context until `/model <id>` auto-scans them).
            let is_ollama = matches!(
                kind,
                crate::providers::ProviderKind::Ollama
                    | crate::providers::ProviderKind::OllamaAnthropic,
            );
            let mut live_note: Option<String> = None;
            if is_ollama {
                if let Ok(p) = crate::repl::build_provider(&state.config) {
                    match p.list_models().await {
                        Ok(live) => {
                            let have: std::collections::HashSet<String> =
                                rows.iter().map(|(id, _)| id.clone()).collect();
                            for m in live {
                                if !have.contains(&m.id) {
                                    rows.push((
                                        m.id,
                                        crate::model_catalogue::ModelEntry {
                                            context: None,
                                            max_output: None,
                                            source: None,
                                            verified_at: None,
                                        },
                                    ));
                                }
                            }
                            rows.sort_by(|a, b| a.0.cmp(&b.0));
                        }
                        Err(e) => {
                            live_note = Some(format!(
                                "(could not reach Ollama /api/tags: {e}; showing catalogue only)"
                            ));
                        }
                    }
                }
            }

            if rows.is_empty() {
                emit(
                    events_tx,
                    format!("no models catalogued for '{provider_name}'. Run /models refresh."),
                );
                return;
            }

            let mut out = format!(
                "models — {provider_name} ({} entries, from catalogue{})\n",
                rows.len(),
                if is_ollama { " + /api/tags" } else { "" }
            );
            for (id, entry) in &rows {
                let ctx = entry
                    .context
                    .map(format_tokens)
                    .unwrap_or_else(|| "—".to_string());
                out.push_str(&format!("  {:<40} {:>6}\n", id, ctx));
            }
            if let Some(note) = live_note {
                out.push_str(&format!("\n{note}\n"));
            }
            out.push_str("\ntype /models refresh to re-seed from openrouter/vendor lists\n");
            emit(events_tx, out);
        }
        SlashCommand::Model(arg) => {
            let arg = arg.trim();
            if arg.is_empty() {
                let prov = state.config.detect_provider().unwrap_or("unknown");
                // Always print the current model — keeps `/model` useful
                // as an introspection command and degrades gracefully on
                // CLI where the picker isn't (yet) rendered.
                emit(
                    events_tx,
                    format!("model: {} (provider: {})", state.config.model, prov),
                );
                // GUI side: also broadcast a model_picker_open event so
                // the existing ModelPickerModal opens with the active
                // provider's catalogue. Skipped for tiny catalogues
                // (<3 entries — no choice to make) and runtime-loaded
                // backends (Ollama / LMStudio) whose model lists come
                // from the live runtime, not the catalogue. Closes #25.
                let runtime_loaded = matches!(prov, "ollama" | "ollama-anthropic" | "lmstudio");
                if !runtime_loaded {
                    let cat = crate::model_catalogue::EffectiveCatalogue::load();
                    let models = cat.list_models_for_provider(prov);
                    if models.len() >= 3 {
                        let model_rows: Vec<serde_json::Value> = models
                            .iter()
                            .map(|(id, e)| {
                                serde_json::json!({
                                    "id": id,
                                    "context": e.context,
                                    "max_output": e.max_output,
                                })
                            })
                            .collect();
                        let payload = serde_json::json!({
                            "type": "model_picker_open",
                            "provider": prov,
                            "current": state.config.model,
                            "models": model_rows,
                        });
                        let _ = events_tx.send(ViewEvent::ModelPickerOpen(payload.to_string()));
                    }
                }
            } else {
                // Strict mode: user named a specific model. A typo
                // should abort so they don't end up on the wrong one.
                switch_model(state, arg, events_tx, /* fallback_to_first */ false).await;
            }
        }
        SlashCommand::Provider(arg) => {
            let arg = arg.trim();
            if arg.is_empty() {
                let prov = state.config.detect_provider().unwrap_or("unknown");
                emit(
                    events_tx,
                    format!("current provider: {prov} (model: {})", state.config.model),
                );
            } else {
                match default_model_for_provider(arg) {
                    // Permissive mode: user picked a provider, not a
                    // specific model. If the hardcoded default isn't
                    // in the live catalogue (which drifts as providers
                    // ship/retire models), fall back to the first
                    // available model rather than aborting.
                    Some(m) => {
                        switch_model(state, m, events_tx, /* fallback_to_first */ true).await
                    }
                    None => emit(
                        events_tx,
                        format!("unknown provider: {arg} (try: anthropic, openai, gemini, ollama)"),
                    ),
                }
            }
        }

        // ─── session ─────────────────────────────────────────────────
        SlashCommand::Clear => {
            state.agent.clear_history();
            state.session = Session::new(&state.config.model, state.cwd.to_string_lossy());
            let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));
            emit(events_tx, "(history cleared)".into());
        }
        SlashCommand::Save => match &state.session_store {
            Some(store) => {
                let history = state.agent.history_snapshot();
                if history.is_empty() {
                    emit(events_tx, "(nothing to save — empty history)".into());
                } else {
                    state.session.sync(history);
                    match store.save(&mut state.session) {
                        Ok(p) => emit(events_tx, format!("session saved → {}", p.display())),
                        Err(e) => emit(events_tx, format!("save failed: {e}")),
                    }
                }
            }
            None => emit(events_tx, "no session store available".into()),
        },
        SlashCommand::Load(id_or_name) => match &state.session_store {
            Some(store) => {
                let id = id_or_name.trim();
                let resolved = if id.eq_ignore_ascii_case("last")
                    || id.eq_ignore_ascii_case("latest")
                    || id.is_empty()
                {
                    store.list().ok().and_then(|mut list| {
                        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                        list.into_iter().next().map(|m| m.id)
                    })
                } else {
                    Some(id.to_string())
                };
                let Some(target) = resolved else {
                    emit(events_tx, "no sessions to load".into());
                    return;
                };
                let result = store
                    .load_by_name_or_id(&target)
                    .or_else(|_| store.load(&target));
                match result {
                    Ok(loaded) => {
                        state.agent.set_history(loaded.messages.clone());
                        state.session = loaded;
                        let display = DisplayMessage::from_messages(&state.session.messages);
                        let _ = events_tx.send(ViewEvent::HistoryReplaced(display));
                        emit(events_tx, format!("loaded session: {}", state.session.id));
                    }
                    Err(e) => emit(events_tx, format!("load failed: {e}")),
                }
            }
            None => emit(events_tx, "no session store available".into()),
        },
        SlashCommand::Sessions => match &state.session_store {
            Some(store) => match store.list() {
                Ok(mut list) => {
                    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                    if list.is_empty() {
                        emit(events_tx, "(no saved sessions)".into());
                    } else {
                        let mut out = String::from("Sessions:\n");
                        for m in list.iter().take(20) {
                            let title = m.title.as_deref().unwrap_or(&m.id);
                            out.push_str(&format!(
                                "  {} — {} ({} msgs, model={})\n",
                                m.id, title, m.message_count, m.model
                            ));
                        }
                        emit(events_tx, out);
                    }
                }
                Err(e) => emit(events_tx, format!("list failed: {e}")),
            },
            None => emit(events_tx, "no session store available".into()),
        },
        SlashCommand::Rename(title) => {
            let title = title.trim();
            if title.is_empty() {
                emit(events_tx, "usage: /rename <title>".into());
            } else {
                state.session.title = Some(title.to_string());
                if let Some(store) = &state.session_store {
                    let history = state.agent.history_snapshot();
                    if !history.is_empty() {
                        state.session.sync(history);
                    }
                    let _ = store.save(&mut state.session);
                }
                emit(events_tx, format!("session renamed → {title}"));
            }
        }

        // ─── runtime knobs ──────────────────────────────────────────
        SlashCommand::Permissions(mode) => {
            if mode.trim().is_empty() {
                let cur = match crate::permissions::current_mode() {
                    crate::permissions::PermissionMode::Auto => "auto",
                    crate::permissions::PermissionMode::Ask => "ask",
                    crate::permissions::PermissionMode::Plan => "plan",
                };
                emit(
                    events_tx,
                    format!(
                        "permissions: {cur} (auto = never prompt, ask = prompt on mutating tools, \
                         plan = read-only exploration; mutating tools blocked)"
                    ),
                );
            } else {
                let persisted = match mode.as_str() {
                    "auto" | "yolo" => {
                        state.agent.permission_mode = crate::permissions::PermissionMode::Auto;
                        crate::permissions::set_current_mode_and_broadcast(
                            crate::permissions::PermissionMode::Auto,
                        );
                        state.config.permissions = "auto".into();
                        Some("auto")
                    }
                    "ask" | "default" => {
                        state.agent.permission_mode = crate::permissions::PermissionMode::Ask;
                        crate::permissions::set_current_mode_and_broadcast(
                            crate::permissions::PermissionMode::Ask,
                        );
                        state.config.permissions = "ask".into();
                        Some("ask")
                    }
                    _ => {
                        emit(events_tx, "usage: /permissions auto|ask".into());
                        None
                    }
                };
                if let Some(m) = persisted {
                    // Persist so a restart lands on the same policy.
                    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
                    project.set_permissions_mode(m);
                    let save_note = match project.save() {
                        Ok(()) => "saved to .thclaws/settings.json",
                        Err(_) => "warning: could not save to .thclaws/settings.json",
                    };
                    let label = if m == "auto" {
                        "permissions → auto (no prompts)"
                    } else {
                        "permissions → ask"
                    };
                    emit(events_tx, format!("{label} ({save_note})"));
                }
            }
        }
        SlashCommand::Plan(arg) => {
            // /plan        → enter plan mode (mutating tools blocked)
            // /plan exit   → restore the prior mode (clears any plan)
            // /plan cancel → alias for /plan exit
            // /plan status → just print current mode + plan summary
            let cur = crate::permissions::current_mode();
            let arg = arg.trim().to_lowercase();
            match arg.as_str() {
                "" | "on" | "enter" | "start" => {
                    if matches!(cur, crate::permissions::PermissionMode::Plan) {
                        emit(events_tx, "Already in plan mode.".into());
                    } else {
                        crate::permissions::stash_pre_plan_mode(cur);
                        crate::permissions::set_current_mode_and_broadcast(
                            crate::permissions::PermissionMode::Plan,
                        );
                        emit(
                            events_tx,
                            "Plan mode active. Mutating tools are blocked — \
                             use Read / Grep / Glob / Ls to explore. When ready, \
                             ask the model to call SubmitPlan."
                                .into(),
                        );
                    }
                }
                "exit" | "off" | "cancel" | "stop" | "abort" => {
                    let restored = crate::permissions::take_pre_plan_mode()
                        .unwrap_or(crate::permissions::PermissionMode::Ask);
                    crate::permissions::set_current_mode_and_broadcast(restored);
                    crate::tools::plan_state::clear();
                    emit(
                        events_tx,
                        format!("Plan mode cleared. Permission mode restored to {restored:?}."),
                    );
                }
                "status" | "show" => {
                    let plan = crate::tools::plan_state::get();
                    let plan_summary = match plan {
                        Some(p) => format!(" — active plan {} ({} step(s))", p.id, p.steps.len()),
                        None => String::new(),
                    };
                    emit(events_tx, format!("permission mode: {cur:?}{plan_summary}"));
                }
                _ => {
                    emit(events_tx, "usage: /plan [enter | exit | status]".into());
                }
            }
        }
        SlashCommand::Thinking(arg) => {
            let arg = arg.trim();
            if arg.is_empty() {
                let budget = state.agent.thinking_budget.unwrap_or(0);
                emit(
                    events_tx,
                    format!("thinking budget: {budget} tokens (0 = off)"),
                );
            } else {
                match arg.parse::<u32>() {
                    Ok(0) => {
                        state.agent.thinking_budget = None;
                        state.config.thinking_budget = None;
                        emit(events_tx, "thinking disabled".into());
                    }
                    Ok(n) => {
                        state.agent.thinking_budget = Some(n);
                        state.config.thinking_budget = Some(n);
                        emit(events_tx, format!("thinking budget → {n} tokens"));
                    }
                    Err(_) => emit(events_tx, "usage: /thinking BUDGET (integer)".into()),
                }
            }
        }
        SlashCommand::Config { key, value } => {
            emit(
                events_tx,
                format!("(session-only) {key} = {value} — applied to runtime only; edit .thclaws/settings.json for persistence"),
            );
        }
        SlashCommand::Compact => {
            let history = state.agent.history_snapshot();
            let compacted = crate::compaction::compact(&history, state.agent.budget_tokens / 2);
            state.agent.set_history(compacted.clone());
            // Persist a checkpoint so the next `/load` starts from the
            // compacted view instead of replaying the full history.
            let persist_note = match (&state.session_store, compacted.len() < history.len()) {
                (Some(store), true) => {
                    let path = store.path_for(&state.session.id);
                    match state.session.append_compaction_to(&path, &compacted) {
                        Ok(()) => " (checkpoint saved)".to_string(),
                        Err(e) => format!(" (checkpoint save failed: {e})"),
                    }
                }
                _ => String::new(),
            };
            emit(
                events_tx,
                format!(
                    "compacted: {} → {} messages{persist_note}",
                    history.len(),
                    compacted.len()
                ),
            );
        }
        SlashCommand::Fork => {
            // Flush the current session to disk so the archive reflects
            // everything up to this moment, then build an LLM-summary
            // of the history and seed a fresh session with it so the
            // next turn starts in a clean file with compact context.
            save_history(&state.agent, &mut state.session, &state.session_store);
            let history = state.agent.history_snapshot();
            if history.is_empty() {
                emit(
                    events_tx,
                    "/fork: nothing to summarize — history is empty".into(),
                );
                return;
            }
            let provider = match crate::repl::build_provider(&state.config) {
                Ok(p) => p,
                Err(e) => {
                    emit(events_tx, format!("/fork: can't build provider: {e}"));
                    return;
                }
            };
            // Aim for roughly half of budget_tokens so the new session
            // has room to grow before the next auto-compact kicks in.
            let target = state.agent.budget_tokens / 2;
            let summary_history = crate::compaction::compact_with_summary(
                &history,
                target,
                provider.as_ref(),
                &state.config.model,
            )
            .await;
            let fallback_note = if summary_history.len() < history.len()
                && summary_history
                    .first()
                    .map(|m| match m.content.first() {
                        Some(crate::types::ContentBlock::Text { text }) => {
                            text.starts_with("[Conversation summary")
                        }
                        _ => false,
                    })
                    .unwrap_or(false)
            {
                ""
            } else {
                " (summary unavailable — used drop-oldest)"
            };
            // New session, seeded with the summary + recent turns.
            let old_id = state.session.id.clone();
            state.session =
                crate::session::Session::new(&state.config.model, state.session.cwd.clone());
            state.warned_file_size = false;
            state.agent.clear_history();
            state.agent.set_history(summary_history.clone());
            state.session.messages = summary_history.clone();
            // Persist the new session with its seeded history.
            if let Some(store) = &state.session_store {
                let _ = store.save(&mut state.session);
            }
            let display = crate::shared_session::DisplayMessage::from_messages(&summary_history);
            let _ = events_tx.send(crate::shared_session::ViewEvent::HistoryReplaced(display));
            let _ = events_tx.send(crate::shared_session::ViewEvent::SessionListRefresh(
                build_session_list(&state.session_store, &state.session.id),
            ));
            emit(
                events_tx,
                format!(
                    "/fork: forked {old_id} → {} ({} → {} messages){fallback_note}",
                    state.session.id,
                    history.len(),
                    summary_history.len()
                ),
            );
        }

        // ─── memory ─────────────────────────────────────────────────
        SlashCommand::MemoryList => {
            let store = match crate::memory::MemoryStore::default_path()
                .map(crate::memory::MemoryStore::new)
            {
                Some(s) => s,
                None => {
                    emit(events_tx, "no memory store".into());
                    return;
                }
            };
            match store.list() {
                Ok(entries) if entries.is_empty() => {
                    emit(events_tx, "(no memory entries)".into());
                }
                Ok(entries) => {
                    let mut out = String::from("Memory:\n");
                    for e in entries {
                        let kind = e.memory_type.unwrap_or_default();
                        let kind_label = if kind.is_empty() {
                            String::new()
                        } else {
                            format!(" ({kind})")
                        };
                        out.push_str(&format!("  {}{kind_label} — {}\n", e.name, e.description));
                    }
                    emit(events_tx, out);
                }
                Err(e) => emit(events_tx, format!("memory list failed: {e}")),
            }
        }
        SlashCommand::MemoryRead(name) => {
            let store = match crate::memory::MemoryStore::default_path()
                .map(crate::memory::MemoryStore::new)
            {
                Some(s) => s,
                None => {
                    emit(events_tx, "no memory store".into());
                    return;
                }
            };
            match store.get(&name) {
                Some(entry) => emit(events_tx, entry.body),
                None => emit(events_tx, format!("memory entry '{name}' not found")),
            }
        }
        // M6.26 BUG #2: GUI-side write/append/edit/delete dispatch.
        // Editor flow isn't supported in the GUI (no terminal); user
        // must pass --body. The agent-side MemoryWrite tool covers
        // the "ask the model to author" use case.
        SlashCommand::MemoryWrite {
            name,
            body,
            type_,
            description,
        } => {
            let Some(store) =
                crate::memory::MemoryStore::default_path().map(crate::memory::MemoryStore::new)
            else {
                emit(events_tx, "no memory store".into());
                return;
            };
            let Some(body_str) = body else {
                emit(
                    events_tx,
                    "GUI /memory write requires --body \"...\". (Editor flow is CLI-only — \
                     ask the agent to write a memory via the chat for free-form authoring.)"
                        .into(),
                );
                return;
            };
            let final_content =
                if (type_.is_some() || description.is_some()) && !body_str.starts_with("---") {
                    let mut fm = std::collections::HashMap::new();
                    if let Some(t) = type_ {
                        fm.insert("type".to_string(), t);
                    }
                    if let Some(d) = description {
                        fm.insert("description".to_string(), d);
                    }
                    crate::memory::write_frontmatter_map(&fm, &body_str)
                } else {
                    body_str
                };
            match crate::memory::write_entry(&store, &name, &final_content) {
                Ok(path) => emit(
                    events_tx,
                    format!("wrote {} ({} bytes)", path.display(), final_content.len()),
                ),
                Err(e) => emit(events_tx, format!("write failed: {e}")),
            }
        }
        SlashCommand::MemoryAppend { name, body } => {
            let Some(store) =
                crate::memory::MemoryStore::default_path().map(crate::memory::MemoryStore::new)
            else {
                emit(events_tx, "no memory store".into());
                return;
            };
            match crate::memory::append_to_entry(&store, &name, &body) {
                Ok(path) => emit(
                    events_tx,
                    format!("appended {} bytes → {}", body.len(), path.display()),
                ),
                Err(e) => emit(events_tx, format!("append failed: {e}")),
            }
        }
        SlashCommand::MemoryEdit(_name) => {
            // GUI doesn't have an editor surface yet; punt to a
            // helpful error rather than silently no-op.
            emit(
                events_tx,
                "GUI /memory edit isn't implemented yet (CLI-only). \
                 Use /memory write <name> --body \"...\" to overwrite \
                 with new content, or ask the agent to update via chat."
                    .into(),
            );
        }
        SlashCommand::MemoryDelete { name, yes: _yes } => {
            // GUI: no interactive confirm — the slash command itself
            // is treated as the confirm gesture (user typed it). If
            // we want a modal later, route through ViewEvent.
            let Some(store) =
                crate::memory::MemoryStore::default_path().map(crate::memory::MemoryStore::new)
            else {
                emit(events_tx, "no memory store".into());
                return;
            };
            match crate::memory::delete_entry(&store, &name) {
                Ok(path) => emit(events_tx, format!("deleted {}", path.display())),
                Err(e) => emit(events_tx, format!("delete failed: {e}")),
            }
        }

        // ─── /loop + /goal (M6.29) ──────────────────────────────────
        SlashCommand::Loop {
            interval_secs,
            body,
        } => {
            if state.active_loop.is_some() {
                emit(
                    events_tx,
                    "loop already running — `/loop stop` first, then start a new one".into(),
                );
                return;
            }
            let interval = std::time::Duration::from_secs(interval_secs.unwrap_or(300));
            let body_for_task = body.clone();
            let input_tx_for_task = input_tx.clone();
            let label_interval = interval_secs
                .map(|s| format!("every {s}s"))
                .unwrap_or_else(|| "self-paced (default 5min)".to_string());
            let handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    if input_tx_for_task
                        .send(crate::shared_session::ShellInput::Line(
                            body_for_task.clone(),
                        ))
                        .is_err()
                    {
                        // Channel closed (worker shut down); exit task.
                        break;
                    }
                }
            });
            state.active_loop = Some(crate::shared_session::ActiveLoop {
                interval_secs,
                body: body.clone(),
                started_at: now_secs(),
                iterations_fired: 0,
                abort: handle.abort_handle(),
            });
            emit(
                events_tx,
                format!("loop started ({label_interval}): {body}"),
            );
        }
        SlashCommand::LoopStop => match state.active_loop.take() {
            Some(loop_state) => {
                loop_state.abort.abort();
                emit(
                    events_tx,
                    format!(
                        "loop stopped (ran {} iteration(s) firing `{}`)",
                        loop_state.iterations_fired, loop_state.body,
                    ),
                );
            }
            None => emit(events_tx, "no active loop".into()),
        },
        SlashCommand::LoopStatus => match &state.active_loop {
            Some(l) => emit(
                events_tx,
                format!(
                    "loop active: body=`{}` interval={} iterations_fired={} started_at={}s ago",
                    l.body,
                    l.interval_secs
                        .map(|s| format!("{s}s"))
                        .unwrap_or_else(|| "self-paced".into()),
                    l.iterations_fired,
                    now_secs().saturating_sub(l.started_at),
                ),
            ),
            None => emit(events_tx, "no active loop".into()),
        },
        SlashCommand::GoalStart {
            objective,
            budget_tokens,
            budget_time_secs,
            auto_continue,
        } => {
            let new_goal = crate::goal_state::GoalState::new(
                objective.clone(),
                budget_tokens,
                budget_time_secs,
                auto_continue,
            );
            crate::goal_state::set(Some(new_goal));
            // Live-register the three goal-lifecycle tools (Phase C1):
            //   RecordGoalProgress  — mid-loop audit checkpoint, status stays Active
            //   MarkGoalComplete    — terminal Complete (audit required)
            //   MarkGoalBlocked     — terminal Blocked (reason required)
            // Authority is split so the model can't slip into "mark
            // complete to escape the loop" — terminal transitions are
            // distinct tools with required justification fields.
            state
                .tool_registry
                .register(std::sync::Arc::new(crate::tools::RecordGoalProgressTool));
            state
                .tool_registry
                .register(std::sync::Arc::new(crate::tools::MarkGoalCompleteTool));
            state
                .tool_registry
                .register(std::sync::Arc::new(crate::tools::MarkGoalBlockedTool));
            state.rebuild_system_prompt();
            if let Err(e) = state.rebuild_agent(true) {
                emit(events_tx, format!("rebuild failed: {e}"));
                return;
            }
            emit(
                events_tx,
                format!(
                    "goal started: \"{}\" (budget_tokens={}, budget_time={}s, auto={})",
                    objective,
                    budget_tokens
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unlimited".into()),
                    budget_time_secs
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unlimited".into()),
                    auto_continue,
                ),
            );
            // Phase D1: when --auto is set, kick off the first /goal
            // continue immediately so users don't have to type it
            // themselves. Subsequent iterations chain via the post-turn
            // logic in handle_line. Without this, --auto would only
            // affect what happens AFTER the first manual /goal continue.
            if auto_continue {
                let _ = input_tx.send(crate::shared_session::ShellInput::Line(
                    "/goal continue".into(),
                ));
            }
        }
        SlashCommand::GoalStatus => match crate::goal_state::current() {
            Some(g) => emit(events_tx, format_goal_status(&g)),
            None => emit(
                events_tx,
                "no active goal — try /goal start \"<objective>\"".into(),
            ),
        },
        SlashCommand::GoalShow => match crate::goal_state::current() {
            Some(g) => emit(events_tx, format_goal_show(&g)),
            None => emit(events_tx, "no active goal".into()),
        },
        SlashCommand::GoalContinue => {
            // Handled in shared_session.rs::handle_line as a turn-rewrite
            // BEFORE this dispatch is called (mirrors the
            // KmsIngestSession pattern). If we reach here, the
            // intercept missed for some reason — surface a clear error.
            emit(
                events_tx,
                "/goal continue requires the agent loop — invoke from chat / CLI, \
                 not via shell_dispatch directly."
                    .into(),
            );
        }
        SlashCommand::GoalComplete { reason } => {
            if crate::goal_state::current().is_none() {
                emit(events_tx, "no active goal".into());
                return;
            }
            let r = reason.clone();
            crate::goal_state::apply(|g| {
                g.status = crate::goal_state::GoalStatus::Complete;
                if let Some(r) = &r {
                    g.last_message = Some(r.clone());
                }
                g.completed_at = Some(now_secs());
                true
            });
            emit(events_tx, "goal marked complete".into());
        }
        SlashCommand::GoalAbandon { reason } => {
            if crate::goal_state::current().is_none() {
                emit(events_tx, "no active goal".into());
                return;
            }
            let r = reason.clone();
            crate::goal_state::apply(|g| {
                g.status = crate::goal_state::GoalStatus::Abandoned;
                if let Some(r) = &r {
                    g.last_message = Some(r.clone());
                }
                g.completed_at = Some(now_secs());
                true
            });
            emit(events_tx, "goal abandoned".into());
        }

        // ─── sso (EE Phase 4) ───────────────────────────────────────
        SlashCommand::Sso { sub } => {
            let policy = crate::policy::active()
                .and_then(|a| a.policy.policies.sso.as_ref())
                .cloned();
            let policy = match policy {
                Some(p) if p.enabled => p,
                Some(_) => {
                    emit(
                        events_tx,
                        "policies.sso.enabled is false — nothing to do".into(),
                    );
                    return;
                }
                None => {
                    emit(
                        events_tx,
                        "no SSO policy active — /sso requires policies.sso.enabled in the org policy".into(),
                    );
                    return;
                }
            };
            match sub {
                crate::repl::SsoSubcommand::Status => {
                    emit(events_tx, crate::sso::status(&policy));
                }
                crate::repl::SsoSubcommand::Login => match crate::sso::login(&policy).await {
                    Ok(s) => {
                        let who = s
                            .email
                            .clone()
                            .or(s.name.clone())
                            .or(s.sub.clone())
                            .unwrap_or_else(|| "(no identity claim)".into());
                        emit(
                            events_tx,
                            format!("✓ signed in as {who} (issuer: {})", s.issuer),
                        );
                    }
                    Err(e) => emit(events_tx, format!("/sso login failed: {e}")),
                },
                crate::repl::SsoSubcommand::Logout => match crate::sso::logout(&policy) {
                    Ok(()) => emit(events_tx, "signed out (cached tokens cleared)".into()),
                    Err(e) => emit(events_tx, format!("/sso logout failed: {e}")),
                },
            }
        }

        // ─── skills ─────────────────────────────────────────────────
        SlashCommand::Skills => {
            let s = crate::skills::SkillStore::discover();
            if s.skills.is_empty() {
                emit(events_tx, "(no skills installed)".into());
            } else {
                let mut entries: Vec<&crate::skills::SkillDef> = s.skills.values().collect();
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                let mut out = String::from("Skills:\n");
                for s in entries {
                    out.push_str(&format!("  {} — {}\n", s.name, s.description));
                }
                emit(events_tx, out);
            }
        }
        SlashCommand::SkillShow(name) => {
            let store = crate::skills::SkillStore::discover();
            match store.get(&name) {
                Some(skill) => {
                    let mut out = format!("{} — {}\n", skill.name, skill.description);
                    if !skill.when_to_use.is_empty() {
                        out.push_str(&format!("when to use: {}\n", skill.when_to_use));
                    }
                    out.push_str(&format!("path: {}\n", skill.dir.display()));
                    emit(events_tx, out);
                }
                None => emit(
                    events_tx,
                    format!("unknown skill: '{name}' — /skills to list"),
                ),
            }
        }
        SlashCommand::SkillInstall {
            git_url,
            name,
            project,
        } => {
            // Resolve marketplace name → install_url (or fall through
            // for a URL). See `repl::resolve_skill_install_target` for
            // the same logic in the CLI surface.
            let (effective_url, effective_name, abort_msg) =
                resolve_skill_install_target_gui(&git_url, name.as_deref());
            if let Some(msg) = abort_msg {
                emit(events_tx, msg);
                return;
            }
            match crate::skills::install_from_url(
                &effective_url,
                effective_name.as_deref(),
                project,
            )
            .await
            {
                Ok(report) => {
                    // Live refresh: replace the SkillTool's store
                    // contents + recompute the system prompt so the
                    // new skill is listed in `# Available skills`.
                    let refreshed = crate::skills::SkillStore::discover();
                    if let Ok(mut store) = state.skill_store.lock() {
                        *store = refreshed;
                    }
                    state.rebuild_system_prompt();
                    if let Err(e) = state.rebuild_agent(true) {
                        emit(events_tx, format!("rebuild failed: {e}"));
                        return;
                    }
                    let mut out = report.join("\n");
                    out.push_str("\n(skill available in this session — no restart needed)");
                    emit(events_tx, out);
                }
                Err(e) => emit(events_tx, format!("skill install failed: {e}")),
            }
        }
        SlashCommand::SkillMarketplace { refresh } => {
            if refresh {
                match crate::marketplace::refresh_from_remote().await {
                    Ok(out) => emit(
                        events_tx,
                        format!(
                            "refreshed marketplace from {} — {} skill(s)",
                            crate::marketplace::REMOTE_URL,
                            out.skill_count
                        ),
                    ),
                    Err(e) => emit(
                        events_tx,
                        format!("refresh failed ({e}); using cached/baseline catalogue"),
                    ),
                }
            }
            let mp = crate::marketplace::load();
            // M6.11 (H2): include cache freshness in the header so
            // users see how old their snapshot is at a glance.
            let age_suffix = match crate::marketplace::cache_age_label() {
                Some(label) => format!(", {label}"),
                None => String::new(),
            };
            let mut out = format!(
                "marketplace ({}, {} skill(s){age_suffix})\n",
                mp.source,
                mp.skills.len(),
            );
            let mut by_cat: std::collections::BTreeMap<
                String,
                Vec<&crate::marketplace::MarketplaceSkill>,
            > = std::collections::BTreeMap::new();
            for s in &mp.skills {
                let cat = if s.category.is_empty() {
                    "other".to_string()
                } else {
                    s.category.clone()
                };
                by_cat.entry(cat).or_default().push(s);
            }
            for (cat, skills) in by_cat {
                out.push_str(&format!("── {cat} ──\n"));
                for s in skills {
                    let tags = crate::marketplace::entry_tags(s);
                    out.push_str(&format!("  {:<24}{tags} — {}\n", s.name, s.short_line()));
                }
            }
            out.push_str("install with: /skill install <name>   |   detail: /skill info <name>");
            emit(events_tx, out);
        }
        SlashCommand::SkillSearch(query) => {
            let mp = crate::marketplace::load();
            let hits = mp.search(&query);
            if hits.is_empty() {
                emit(
                    events_tx,
                    format!("no matches for '{query}' — try /skill marketplace"),
                );
            } else {
                let mut out = format!("{} match(es) for '{query}':\n", hits.len());
                for s in hits {
                    out.push_str(&format!("  {:<24} — {}\n", s.name, s.short_line()));
                }
                emit(events_tx, out);
            }
        }
        SlashCommand::SkillInfo(name) => {
            let mp = crate::marketplace::load();
            match mp.find(&name) {
                Some(s) => {
                    let mut out = format!("name:        {}\n", s.name);
                    out.push_str(&format!("description: {}\n", s.description));
                    if !s.category.is_empty() {
                        out.push_str(&format!("category:    {}\n", s.category));
                    }
                    out.push_str(&format!(
                        "license:     {} ({})\n",
                        s.license, s.license_tier
                    ));
                    if !s.homepage.is_empty() {
                        out.push_str(&format!("homepage:    {}\n", s.homepage));
                    }
                    match (s.license_tier.as_str(), s.install_url.as_ref()) {
                        ("linked-only", _) => out.push_str(&format!(
                            "install:     not redistributable — install from {}",
                            if s.homepage.is_empty() {
                                "the upstream repo"
                            } else {
                                &s.homepage
                            }
                        )),
                        (_, Some(url)) => out.push_str(&format!(
                            "install:     /skill install {} (resolves to {url})",
                            s.name
                        )),
                        (_, None) => out.push_str("install:     no install_url in catalogue"),
                    }
                    emit(events_tx, out);
                }
                None => emit(
                    events_tx,
                    format!("no skill named '{name}' in marketplace — try /skill search <query>"),
                ),
            }
        }
        SlashCommand::McpMarketplace { refresh } => {
            if refresh {
                if let Err(e) = crate::marketplace::refresh_from_remote().await {
                    emit(events_tx, format!("refresh failed: {e}"));
                }
            }
            let mp = crate::marketplace::load();
            let age_suffix = match crate::marketplace::cache_age_label() {
                Some(label) => format!(", {label}"),
                None => String::new(),
            };
            let mut out = format!(
                "MCP marketplace ({}, {} server(s){age_suffix})\n",
                mp.source,
                mp.mcp_servers.len(),
            );
            let mut by_cat: std::collections::BTreeMap<
                String,
                Vec<&crate::marketplace::MarketplaceMcpServer>,
            > = std::collections::BTreeMap::new();
            for s in &mp.mcp_servers {
                let cat = if s.category.is_empty() {
                    "other".into()
                } else {
                    s.category.clone()
                };
                by_cat.entry(cat).or_default().push(s);
            }
            for (cat, servers) in by_cat {
                out.push_str(&format!("── {cat} ──\n"));
                for s in servers {
                    let tport = if s.transport == "sse" {
                        " [hosted]"
                    } else {
                        ""
                    };
                    let tags = crate::marketplace::entry_tags(s);
                    out.push_str(&format!(
                        "  {:<24}{tport}{tags} — {}\n",
                        s.name,
                        s.short_line()
                    ));
                }
            }
            out.push_str("install with: /mcp install <name>   |   detail: /mcp info <name>");
            emit(events_tx, out);
        }
        SlashCommand::McpSearch(query) => {
            let mp = crate::marketplace::load();
            let hits = mp.search_mcp(&query);
            if hits.is_empty() {
                emit(
                    events_tx,
                    format!("no matches for '{query}' — try /mcp marketplace"),
                );
            } else {
                let mut out = format!("{} match(es) for '{query}':\n", hits.len());
                for s in hits {
                    out.push_str(&format!("  {:<24} — {}\n", s.name, s.short_line()));
                }
                emit(events_tx, out);
            }
        }
        SlashCommand::McpInfo(name) => {
            let mp = crate::marketplace::load();
            match mp.find_mcp(&name) {
                Some(s) => {
                    let mut out = format!("name:         {}\n", s.name);
                    out.push_str(&format!("description:  {}\n", s.description));
                    if !s.category.is_empty() {
                        out.push_str(&format!("category:     {}\n", s.category));
                    }
                    out.push_str(&format!(
                        "license:      {} ({})\n",
                        s.license, s.license_tier
                    ));
                    out.push_str(&format!("transport:    {}\n", s.transport));
                    if s.transport == "stdio" && !s.command.is_empty() {
                        let argv = if s.args.is_empty() {
                            s.command.clone()
                        } else {
                            format!("{} {}", s.command, s.args.join(" "))
                        };
                        out.push_str(&format!("command:      {argv}\n"));
                    }
                    if s.transport == "sse" && !s.url.is_empty() {
                        out.push_str(&format!("url:          {}\n", s.url));
                    }
                    if let Some(src) = &s.install_url {
                        out.push_str(&format!("source:       {src}\n"));
                    }
                    if !s.homepage.is_empty() {
                        out.push_str(&format!("homepage:     {}\n", s.homepage));
                    }
                    if let Some(msg) = &s.post_install_message {
                        out.push_str(&format!("note:         {msg}\n"));
                    }
                    out.push_str(&format!("install with: /mcp install {}", s.name));
                    emit(events_tx, out);
                }
                None => emit(
                    events_tx,
                    format!("no MCP named '{name}' in marketplace — try /mcp search <query>"),
                ),
            }
        }
        SlashCommand::McpInstall { name, user } => {
            match crate::repl::install_mcp_from_marketplace(&name, user).await {
                Ok(report) => {
                    emit(events_tx, report.join("\n"));
                    broadcast_mcp_update(events_tx);
                }
                Err(e) => emit(events_tx, format!("mcp install failed: {e}")),
            }
        }
        SlashCommand::PluginMarketplace { refresh } => {
            if refresh {
                if let Err(e) = crate::marketplace::refresh_from_remote().await {
                    emit(events_tx, format!("refresh failed: {e}"));
                }
            }
            let mp = crate::marketplace::load();
            let age_suffix = match crate::marketplace::cache_age_label() {
                Some(label) => format!(", {label}"),
                None => String::new(),
            };
            let mut out = format!(
                "plugin marketplace ({}, {} plugin(s){age_suffix})\n",
                mp.source,
                mp.plugins.len(),
            );
            let mut by_cat: std::collections::BTreeMap<
                String,
                Vec<&crate::marketplace::MarketplacePlugin>,
            > = std::collections::BTreeMap::new();
            for p in &mp.plugins {
                let cat = if p.category.is_empty() {
                    "other".into()
                } else {
                    p.category.clone()
                };
                by_cat.entry(cat).or_default().push(p);
            }
            for (cat, plugins) in by_cat {
                out.push_str(&format!("── {cat} ──\n"));
                for p in plugins {
                    let tags = crate::marketplace::entry_tags(p);
                    out.push_str(&format!("  {:<24}{tags} — {}\n", p.name, p.short_line()));
                }
            }
            out.push_str("install with: /plugin install <name>   |   detail: /plugin info <name>");
            emit(events_tx, out);
        }
        SlashCommand::PluginSearch(query) => {
            let mp = crate::marketplace::load();
            let hits = mp.search_plugin(&query);
            if hits.is_empty() {
                emit(
                    events_tx,
                    format!("no matches for '{query}' — try /plugin marketplace"),
                );
            } else {
                let mut out = format!("{} match(es) for '{query}':\n", hits.len());
                for p in hits {
                    out.push_str(&format!("  {:<24} — {}\n", p.name, p.short_line()));
                }
                emit(events_tx, out);
            }
        }
        SlashCommand::PluginInfo(name) => {
            let mp = crate::marketplace::load();
            match mp.find_plugin(&name) {
                Some(p) => {
                    let mut out = format!("name:         {}\n", p.name);
                    out.push_str(&format!("description:  {}\n", p.description));
                    if !p.category.is_empty() {
                        out.push_str(&format!("category:     {}\n", p.category));
                    }
                    out.push_str(&format!(
                        "license:      {} ({})\n",
                        p.license, p.license_tier
                    ));
                    if !p.homepage.is_empty() {
                        out.push_str(&format!("homepage:     {}\n", p.homepage));
                    }
                    out.push_str(&format!(
                        "install with: /plugin install {} (resolves to {})",
                        p.name, p.install_url
                    ));
                    emit(events_tx, out);
                }
                None => emit(
                    events_tx,
                    format!("no plugin named '{name}' in marketplace — try /plugin search <query>"),
                ),
            }
        }

        // ─── knowledge bases ────────────────────────────────────────
        SlashCommand::Kms => {
            let all = crate::kms::list_all();
            if all.is_empty() {
                emit(
                    events_tx,
                    "no knowledge bases yet — try: /kms new default".into(),
                );
            } else {
                let active: std::collections::HashSet<&String> =
                    state.config.kms_active.iter().collect();
                let mut out = String::from("Knowledge bases:\n");
                for k in &all {
                    let marker = if active.contains(&k.name) { "*" } else { " " };
                    out.push_str(&format!(
                        "  {marker} {:<16} ({})\n",
                        k.name,
                        k.scope.as_str()
                    ));
                }
                out.push_str("(* = attached to this project; toggle with /kms use | /kms off)");
                emit(events_tx, out);
            }
        }
        SlashCommand::KmsNew { name, project } => {
            let scope = if project {
                crate::kms::KmsScope::Project
            } else {
                crate::kms::KmsScope::User
            };
            match crate::kms::create(&name, scope) {
                Ok(k) => {
                    emit(
                        events_tx,
                        format!(
                            "created KMS '{}' ({}) → {}",
                            k.name,
                            k.scope.as_str(),
                            k.root.display()
                        ),
                    );
                    broadcast_kms_update(events_tx);
                }
                Err(e) => emit(events_tx, format!("create failed: {e}")),
            }
        }
        SlashCommand::KmsUse(name) => {
            if crate::kms::resolve(&name).is_none() {
                emit(
                    events_tx,
                    format!("no KMS named '{name}' (try /kms list or /kms new {name})"),
                );
            } else if state.config.kms_active.iter().any(|n| n == &name) {
                emit(events_tx, format!("KMS '{name}' already attached"));
            } else {
                state.config.kms_active.push(name.clone());
                if let Err(e) =
                    crate::config::ProjectConfig::set_active_kms(state.config.kms_active.clone())
                {
                    emit(events_tx, format!("save failed: {e}"));
                    return;
                }
                // Live register: ensure KMS tools are in the registry
                // (first KMS activation; repeated ones are idempotent
                // since register() is insert-overwrite).
                state
                    .tool_registry
                    .register(std::sync::Arc::new(crate::tools::KmsReadTool));
                state
                    .tool_registry
                    .register(std::sync::Arc::new(crate::tools::KmsSearchTool));
                // M6.25 BUG #1: write tools alongside read tools.
                state
                    .tool_registry
                    .register(std::sync::Arc::new(crate::tools::KmsWriteTool));
                state
                    .tool_registry
                    .register(std::sync::Arc::new(crate::tools::KmsAppendTool));
                state.rebuild_system_prompt();
                if let Err(e) = state.rebuild_agent(true) {
                    emit(events_tx, format!("rebuild failed: {e}"));
                    return;
                }
                emit(
                    events_tx,
                    format!("KMS '{name}' attached (tools registered; available this turn)"),
                );
                broadcast_kms_update(events_tx);
            }
        }
        SlashCommand::KmsOff(name) => {
            let before = state.config.kms_active.len();
            state.config.kms_active.retain(|n| n != &name);
            if state.config.kms_active.len() == before {
                emit(events_tx, format!("KMS '{name}' was not attached"));
            } else {
                if let Err(e) =
                    crate::config::ProjectConfig::set_active_kms(state.config.kms_active.clone())
                {
                    emit(events_tx, format!("save failed: {e}"));
                    return;
                }
                // If no KMS is attached anymore, drop the tools so
                // the model doesn't see stale affordances.
                if state.config.kms_active.is_empty() {
                    state.tool_registry.remove("KmsRead");
                    state.tool_registry.remove("KmsSearch");
                    state.tool_registry.remove("KmsWrite");
                    state.tool_registry.remove("KmsAppend");
                }
                state.rebuild_system_prompt();
                if let Err(e) = state.rebuild_agent(true) {
                    emit(events_tx, format!("rebuild failed: {e}"));
                    return;
                }
                emit(
                    events_tx,
                    format!("KMS '{name}' detached (system prompt updated)"),
                );
                broadcast_kms_update(events_tx);
            }
        }
        SlashCommand::KmsShow(name) => match crate::kms::resolve(&name) {
            Some(k) => {
                let active = state.config.kms_active.iter().any(|n| n == &k.name);
                let mark = if active { "attached" } else { "not attached" };
                emit(
                    events_tx,
                    format!(
                        "{} ({}) — {mark}\npath: {}",
                        k.name,
                        k.scope.as_str(),
                        k.root.display()
                    ),
                );
            }
            None => emit(events_tx, format!("no KMS named '{name}'")),
        },
        SlashCommand::KmsIngest {
            name,
            file,
            alias,
            force,
        } => {
            let Some(k) = crate::kms::resolve(&name) else {
                emit(
                    events_tx,
                    format!("no KMS named '{name}' (try /kms list or /kms new {name})"),
                );
                return;
            };
            let source = std::path::PathBuf::from(&file);
            let source = if source.is_absolute() {
                source
            } else {
                state.cwd.join(&source)
            };
            match crate::kms::ingest(&k, &source, alias.as_deref(), force) {
                Ok(r) => {
                    let verb = if r.overwrote { "replaced" } else { "ingested" };
                    let cascade = if r.cascaded > 0 {
                        format!(" (marked {} dependent page(s) stale)", r.cascaded)
                    } else {
                        String::new()
                    };
                    emit(
                        events_tx,
                        format!("{verb} → {} — {}{cascade}", r.target.display(), r.summary),
                    );
                }
                Err(e) => emit(events_tx, format!("ingest failed: {e}")),
            }
        }
        SlashCommand::KmsIngestUrl {
            name,
            url,
            alias,
            force,
        } => {
            // M6.25 BUG #8: URL ingest dispatches via async ingest_url.
            let Some(k) = crate::kms::resolve(&name) else {
                emit(events_tx, format!("no KMS named '{name}'"));
                return;
            };
            match crate::kms::ingest_url(&k, &url, alias.as_deref(), force).await {
                Ok(r) => {
                    let verb = if r.overwrote { "replaced" } else { "ingested" };
                    let cascade = if r.cascaded > 0 {
                        format!(" (marked {} dependent page(s) stale)", r.cascaded)
                    } else {
                        String::new()
                    };
                    emit(
                        events_tx,
                        format!(
                            "{verb} {url} → {} — {}{cascade}",
                            r.target.display(),
                            r.summary
                        ),
                    );
                }
                Err(e) => emit(events_tx, format!("url ingest failed: {e}")),
            }
        }
        SlashCommand::KmsIngestPdf {
            name,
            file,
            alias,
            force,
        } => {
            // M6.25 BUG #8: PDF ingest via pdftotext.
            let Some(k) = crate::kms::resolve(&name) else {
                emit(events_tx, format!("no KMS named '{name}'"));
                return;
            };
            let source = std::path::PathBuf::from(&file);
            let source = if source.is_absolute() {
                source
            } else {
                state.cwd.join(&source)
            };
            match crate::kms::ingest_pdf(&k, &source, alias.as_deref(), force).await {
                Ok(r) => {
                    let verb = if r.overwrote { "replaced" } else { "ingested" };
                    let cascade = if r.cascaded > 0 {
                        format!(" (marked {} dependent page(s) stale)", r.cascaded)
                    } else {
                        String::new()
                    };
                    emit(
                        events_tx,
                        format!(
                            "{verb} {} → {} — {}{cascade}",
                            source.display(),
                            r.target.display(),
                            r.summary
                        ),
                    );
                }
                Err(e) => emit(events_tx, format!("pdf ingest failed: {e}")),
            }
        }
        SlashCommand::KmsIngestSession { name, .. } => {
            // M6.28: handled in shared_session.rs::handle_line as a
            // turn-rewrite BEFORE this dispatch is called, so this
            // arm should be unreachable in normal flow. If it fires
            // (e.g. somebody calls dispatch directly bypassing
            // handle_line), surface a clear error rather than
            // silently no-opping.
            emit(
                events_tx,
                format!(
                    "/kms ingest {name} $ requires the agent loop — invoke from chat / CLI, \
                     not via shell_dispatch::dispatch directly."
                ),
            );
        }
        SlashCommand::KmsLint(name) => {
            // M6.25 BUG #3: pure-read health check.
            let Some(k) = crate::kms::resolve(&name) else {
                emit(events_tx, format!("no KMS named '{name}'"));
                return;
            };
            match crate::kms::lint(&k) {
                Ok(report) => {
                    emit(events_tx, format_lint_report(&name, &report));
                }
                Err(e) => emit(events_tx, format!("lint failed: {e}")),
            }
        }
        SlashCommand::KmsFileAnswer { name, title } => {
            // M6.25 BUG #4: file the latest assistant message into a KMS
            // as a new page. Reads from the live session.
            let Some(k) = crate::kms::resolve(&name) else {
                emit(events_tx, format!("no KMS named '{name}'"));
                return;
            };
            let answer = state
                .session
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, crate::types::Role::Assistant))
                .and_then(|m| {
                    Some(
                        m.content
                            .iter()
                            .filter_map(|b| match b {
                                crate::types::ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n"),
                    )
                });
            let Some(answer_text) = answer.filter(|s| !s.trim().is_empty()) else {
                emit(
                    events_tx,
                    "no assistant message in session yet — nothing to file".into(),
                );
                return;
            };
            let stem = sanitize_alias_for_dispatch(&title);
            if stem.is_empty() {
                emit(events_tx, "title sanitises to empty — pick another".into());
                return;
            }
            let body = format!("# {title}\n\n{answer_text}\n");
            let mut fm = std::collections::BTreeMap::new();
            fm.insert("category".into(), "answer".into());
            fm.insert("filed_from".into(), "chat".into());
            let serialized = crate::kms::write_frontmatter(&fm, &body);
            match crate::kms::write_page(&k, &stem, &serialized) {
                Ok(path) => emit(
                    events_tx,
                    format!(
                        "filed answer → {} ({} bytes)",
                        path.display(),
                        serialized.len()
                    ),
                ),
                Err(e) => emit(events_tx, format!("file-answer failed: {e}")),
            }
        }

        // ─── MCP servers ────────────────────────────────────────────
        SlashCommand::Mcp => {
            let servers = crate::config::AppConfig::load()
                .map(|c| c.mcp_servers)
                .unwrap_or_default();
            if servers.is_empty() {
                emit(events_tx, "no MCP servers configured".into());
            } else {
                let mut out = String::from("MCP servers:\n");
                for s in servers {
                    let kind = if s.transport == "http" {
                        "http"
                    } else {
                        "stdio"
                    };
                    out.push_str(&format!("  {} ({kind})\n", s.name));
                }
                emit(events_tx, out);
            }
        }
        SlashCommand::McpAdd { name, url, user } => {
            // Hand-add is untrusted by default; enabling widget UI
            // requires the user to edit mcp.json and set
            // `"trusted": true` explicitly.
            let cfg = crate::mcp::McpServerConfig {
                name: name.clone(),
                transport: "http".into(),
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
                url,
                headers: Default::default(),
                trusted: false,
            };
            // 1. Persist to disk (so restarts keep the server).
            let saved_to = match crate::config::save_mcp_server(&cfg, user) {
                Ok(p) => p,
                Err(e) => {
                    emit(events_tx, format!("write failed: {e}"));
                    return;
                }
            };
            // 2. Spawn the client + list tools + register them.
            match crate::mcp::McpClient::spawn_with_approver(
                cfg.clone(),
                Some(state.approver.clone()),
            )
            .await
            {
                Ok(client) => match client.list_tools().await {
                    Ok(tool_infos) => {
                        let names: Vec<String> =
                            tool_infos.iter().map(|t| t.name.clone()).collect();
                        for info in tool_infos {
                            let tool = crate::mcp::McpTool::new(client.clone(), info);
                            state.tool_registry.register(std::sync::Arc::new(tool));
                        }
                        state.mcp_clients.push(client);
                        if let Err(e) = state.rebuild_agent(true) {
                            emit(events_tx, format!("rebuild failed: {e}"));
                            return;
                        }
                        emit(
                            events_tx,
                            format!(
                                "mcp '{name}' added ({}, {} tool(s)) → {}\nTools: {}",
                                if user { "user" } else { "project" },
                                names.len(),
                                saved_to.display(),
                                names.join(", "),
                            ),
                        );
                        broadcast_mcp_update(events_tx);
                    }
                    Err(e) => emit(
                        events_tx,
                        format!(
                            "saved '{name}' to {} but list_tools failed: {e}",
                            saved_to.display()
                        ),
                    ),
                },
                Err(e) => emit(
                    events_tx,
                    format!(
                        "saved '{name}' to {} but connect failed: {e}",
                        saved_to.display()
                    ),
                ),
            }
        }
        SlashCommand::McpRemove { name, user } => {
            match crate::config::remove_mcp_server(&name, user) {
                Ok((true, p)) => {
                    // We can't cleanly remove just this server's tools
                    // from the live registry (they're interleaved with
                    // other MCP tools by name and we don't track the
                    // mapping). Persist + advise restart; the config
                    // will be clean on next launch.
                    emit(
                        events_tx,
                        format!(
                            "mcp '{name}' removed from {} (tools active in this session will be dropped on restart)",
                            p.display()
                        ),
                    );
                    // Sidebar shows the new (shorter) list immediately —
                    // the dropped tools won't disappear until restart but
                    // at least the entry doesn't linger after the user
                    // explicitly removed it.
                    broadcast_mcp_update(events_tx);
                }
                Ok((false, p)) => emit(
                    events_tx,
                    format!("no server named '{name}' in {}", p.display()),
                ),
                Err(e) => emit(events_tx, format!("remove failed: {e}")),
            }
        }

        // ─── plugins ────────────────────────────────────────────────
        SlashCommand::Plugins => {
            let plugins = crate::plugins::all_plugins_all_scopes();
            if plugins.is_empty() {
                emit(
                    events_tx,
                    "no plugins installed (try /plugin install <url>)".into(),
                );
            } else {
                let mut out = String::from("Plugins:\n");
                for p in plugins {
                    let status = if p.enabled { "enabled" } else { "disabled" };
                    let version = if p.version.is_empty() {
                        String::new()
                    } else {
                        format!(" v{}", p.version)
                    };
                    out.push_str(&format!(
                        "  {}{version} ({status}) → {}\n",
                        p.name,
                        p.path.display(),
                    ));
                }
                emit(events_tx, out);
            }
        }
        SlashCommand::PluginInstall { url, user } => {
            // Resolve marketplace slug → install_url (no-op for URLs).
            let (effective_url, abort_msg) = crate::repl::resolve_plugin_install_target(&url);
            if let Some(msg) = abort_msg {
                emit(events_tx, msg);
                return;
            }
            match crate::plugins::install(&effective_url, user).await {
                Ok(plugin) => {
                    // Refresh the SkillTool store so plugin-contributed
                    // skills are callable in this session. Plugin-
                    // contributed MCP servers still need a restart
                    // (auto-spawning them here would need to detect
                    // which ones are new vs. already running).
                    let refreshed = crate::skills::SkillStore::discover();
                    if let Ok(mut store) = state.skill_store.lock() {
                        *store = refreshed;
                    }
                    state.rebuild_system_prompt();
                    if let Err(e) = state.rebuild_agent(true) {
                        emit(events_tx, format!("rebuild failed: {e}"));
                        return;
                    }
                    // Skills + commands are live in this session
                    // (skill store refreshed; commands re-discover per
                    // /-resolution). MCP servers still need a restart
                    // — explain prominently with the server names so
                    // the user knows exactly what's coming after
                    // relaunch. M6.16.1 follow-up.
                    let mut note = format!(
                        "plugin '{}' installed ({}) → {}\nSkills + commands callable this session.",
                        plugin.name,
                        if user { "user" } else { "project" },
                        plugin.path.display(),
                    );
                    if let Ok(m) = plugin.manifest() {
                        if !m.mcp_servers.is_empty() {
                            let mut names: Vec<&str> =
                                m.mcp_servers.keys().map(String::as_str).collect();
                            names.sort();
                            note.push_str(&format!(
                                "\n\n⚠  restart thClaws (/quit then relaunch) to spawn {} new MCP server(s): {}",
                                names.len(),
                                names.join(", ")
                            ));
                        }
                    }
                    emit(events_tx, note);
                }
                Err(e) => emit(events_tx, format!("plugin install failed: {e}")),
            }
        }
        SlashCommand::PluginRemove { name, user } => {
            // Capture MCP names BEFORE remove() — once the manifest
            // file is gone, find_installed returns None.
            let mcp_to_drop = mcp_server_names(&name);
            match crate::plugins::remove(&name, user) {
                Ok(true) => {
                    // M6.16 BUG H1: refresh skill store + rebuild agent
                    // so the removed plugin's skill contributions stop
                    // being callable in this session. Without this, the
                    // model could still invoke a removed skill and get
                    // an empty body (the lazy read fails after the file
                    // is gone, OnceLock caches the empty result). MCP
                    // subprocess teardown is still restart-bound — see
                    // the trailing note in the message.
                    refresh_after_plugin_change(state, events_tx);
                    let mut note = format!("plugin '{name}' removed");
                    if let Some(names) = mcp_to_drop {
                        note.push_str(&format!(
                            "\n\n⚠  restart thClaws (/quit then relaunch) to fully drop {} MCP server(s) it was running: {}",
                            names.len(),
                            names.join(", ")
                        ));
                    }
                    emit(events_tx, note);
                }
                Ok(false) => emit(events_tx, format!("no plugin named '{name}' in that scope")),
                Err(e) => emit(events_tx, format!("remove failed: {e}")),
            }
        }
        SlashCommand::PluginEnable { name, user } => {
            match crate::plugins::set_enabled(&name, user, true) {
                Ok(true) => {
                    refresh_after_plugin_change(state, events_tx);
                    let mut note = format!("plugin '{name}' enabled");
                    if let Some(names) = mcp_server_names(&name) {
                        note.push_str(&format!(
                            "\n\n⚠  restart thClaws (/quit then relaunch) to spawn {} MCP server(s): {}",
                            names.len(),
                            names.join(", ")
                        ));
                    }
                    emit(events_tx, note);
                }
                Ok(false) => emit(events_tx, format!("no plugin named '{name}' in that scope")),
                Err(e) => emit(events_tx, format!("enable failed: {e}")),
            }
        }
        SlashCommand::PluginDisable { name, user } => {
            // Capture MCP names BEFORE disabling so the message can
            // list them; set_enabled doesn't delete the manifest, but
            // keeping the lookup symmetric with PluginRemove makes
            // the helper reusable.
            let mcp_to_drop = mcp_server_names(&name);
            match crate::plugins::set_enabled(&name, user, false) {
                Ok(true) => {
                    refresh_after_plugin_change(state, events_tx);
                    let mut note = format!("plugin '{name}' disabled");
                    if let Some(names) = mcp_to_drop {
                        note.push_str(&format!(
                            "\n\n⚠  restart thClaws (/quit then relaunch) to drop {} MCP server(s) it contributed: {}",
                            names.len(),
                            names.join(", ")
                        ));
                    }
                    emit(events_tx, note);
                }
                Ok(false) => emit(events_tx, format!("no plugin named '{name}' in that scope")),
                Err(e) => emit(events_tx, format!("disable failed: {e}")),
            }
        }
        SlashCommand::PluginShow { name } => match crate::plugins::find_installed_with_scope(&name)
        {
            Some((p, is_user)) => {
                let status = if p.enabled { "enabled" } else { "disabled" };
                let scope = if is_user { "user" } else { "project" };
                let version = if p.version.is_empty() {
                    "-"
                } else {
                    &p.version
                };
                // M6.16.1 BUG L3: include scope in the output so the
                // user knows which `--user` flag to pass to subsequent
                // /plugin disable / enable / remove. Pre-fix the user
                // had to inspect the path string to figure it out.
                let mut out = format!(
                    "{} v{version} ({status}, {scope})\npath: {}\n",
                    p.name,
                    p.path.display()
                );
                if !p.source.is_empty() {
                    out.push_str(&format!("source: {}\n", p.source));
                }
                emit(events_tx, out);
            }
            None => emit(events_tx, format!("no plugin named '{name}'")),
        },
        SlashCommand::PluginGc => match crate::plugins::gc() {
            Ok((proj, user)) => {
                if proj.is_empty() && user.is_empty() {
                    emit(events_tx, "no zombie entries — registry is clean".into());
                } else {
                    let mut out = String::from("removed zombie entries:\n");
                    for n in &proj {
                        out.push_str(&format!("  - {n} (project)\n"));
                    }
                    for n in &user {
                        out.push_str(&format!("  - {n} (user)\n"));
                    }
                    // Refresh in case any zombie was contributing
                    // skills cached in the worker.
                    refresh_after_plugin_change(state, events_tx);
                    emit(events_tx, out);
                }
            }
            Err(e) => emit(events_tx, format!("gc failed: {e}")),
        },

        // ─── team ───────────────────────────────────────────────────
        SlashCommand::Team => {
            let team_dir = crate::team::Mailbox::default_dir();
            let mailbox = crate::team::Mailbox::new(team_dir);
            match mailbox.all_status() {
                Ok(agents) if agents.is_empty() => {
                    emit(events_tx, "no team agents found".into());
                }
                Ok(agents) => {
                    let mut out = String::from("Team:\n");
                    for a in &agents {
                        let task = a.current_task.as_deref().unwrap_or("-");
                        out.push_str(&format!("  {} — {} (task: {})\n", a.agent, a.status, task));
                    }
                    emit(events_tx, out);
                }
                Err(_) => emit(events_tx, "no team configured".into()),
            }
        }

        SlashCommand::Unknown(detail) => {
            emit(events_tx, format!("unknown command: {detail}"));
        }
    }
}

/// Switch to a new model. If the provider supports listing, validate
/// the target exists in the catalogue.
///
/// `fallback_to_first` controls what happens when validation fails:
///   - `false` (used by `/model X`): abort with an error message.
///     The user named a specific model — a typo should fail loud.
///   - `true` (used by `/provider X`): pick the first available model
///     from the catalogue. The user named a provider, not a model;
///     the hardcoded default may have drifted as the provider ships
///     or retires models.
///
/// Persists to `.thclaws/settings.json` and rebuilds the agent with
/// the new provider — clearing history so conversation pieces built
/// for a different provider's schema don't confuse the new one.
async fn switch_model(
    state: &mut WorkerState,
    new_model: &str,
    events_tx: &broadcast::Sender<ViewEvent>,
    fallback_to_first: bool,
) {
    let resolved_initial = crate::providers::ProviderKind::resolve_alias(new_model);
    if resolved_initial != new_model {
        emit(
            events_tx,
            format!("(alias '{new_model}' → '{resolved_initial}')"),
        );
    }
    let mut candidate = state.config.clone();
    candidate.model = resolved_initial.clone();
    let new_provider = match crate::repl::build_provider(&candidate) {
        Ok(p) => p,
        Err(e) => {
            emit(events_tx, format!("{e}"));
            return;
        }
    };

    // Catalogue validation. If the provider supports listing and the
    // requested model isn't there, either abort (strict, /model X)
    // or fall back to the first available model (permissive,
    // /provider X). Empty list / unsupported listing accepts the
    // requested model optimistically.
    let mut resolved = resolved_initial.clone();
    if let Ok(models) = new_provider.list_models().await {
        if !models.is_empty() && !models.iter().any(|m| m.id == resolved) {
            if fallback_to_first {
                let first = models[0].id.clone();
                emit(
                    events_tx,
                    format!(
                        "default model '{resolved}' not in {} catalogue — falling back to first available: {first}",
                        candidate.detect_provider().unwrap_or("provider"),
                    ),
                );
                resolved = first;
                candidate.model = resolved.clone();
            } else {
                emit(
                    events_tx,
                    format!(
                        "model '{resolved}' not found in {} catalogue — aborting switch (try /models to see what's available)",
                        candidate.detect_provider().unwrap_or("provider"),
                    ),
                );
                return;
            }
        }
    }

    // Intra-family swap (e.g. sonnet → opus, both Anthropic) keeps the
    // same message/tool-call schema on the wire, so the existing
    // conversation replays cleanly against the new model. Cross-family
    // swaps (Anthropic → OpenAI → Gemini) change the wire shape and
    // would either hard-error or silently corrupt context — fork to a
    // fresh session instead.
    let old_kind = crate::providers::ProviderKind::detect(&state.config.model);
    let new_kind = crate::providers::ProviderKind::detect(&resolved);
    let same_family = old_kind.is_some() && old_kind == new_kind;

    // Flush prior session before swapping. We always want the on-disk
    // copy up-to-date regardless of which branch we take next.
    save_history(&state.agent, &mut state.session, &state.session_store);

    state.config = candidate;
    if same_family {
        // Preserve history across the model swap. `rebuild_agent(true)`
        // carries the existing message list into the fresh Agent; the
        // session itself keeps its id and accumulated messages, we just
        // update the `model` label so the header reflects the new model.
        if let Err(e) = state.rebuild_agent(true) {
            emit(events_tx, format!("rebuild failed: {e}"));
            return;
        }
        state.session.model = state.config.model.clone();
    } else {
        if let Err(e) = state.rebuild_agent(false) {
            emit(events_tx, format!("rebuild failed: {e}"));
            return;
        }
        state.agent.clear_history();
        state.session = Session::new(&state.config.model, state.session.cwd.clone());
    }

    // Persist the model choice to project settings so a restart lands
    // on the same provider/model.
    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
    project.set_model(&state.config.model);
    let _ = project.save();

    let provider = state.config.detect_provider().unwrap_or("unknown");
    let session_note = if same_family {
        "conversation preserved".to_string()
    } else {
        format!("new session {}", state.session.id)
    };
    emit(
        events_tx,
        format!(
            "model → {} (provider: {provider}; saved to .thclaws/settings.json; {session_note})",
            state.config.model
        ),
    );
    // Catalogue hint: if we don't have an exact context-window entry
    // for this model, try to discover it at the source.
    // - Ollama models: hit `POST /api/show` for the chosen context
    //   (prefers `num_ctx` over native `context_length`) and write
    //   the result into the user cache so it sticks.
    // - Everyone else: emit the "run /models refresh" nudge.
    let cat = crate::model_catalogue::EffectiveCatalogue::load();
    let (ctx, src) =
        crate::model_catalogue::effective_context_window_with(&cat, &state.config.model);
    if !src.is_known() {
        let is_ollama = matches!(
            new_kind,
            Some(crate::providers::ProviderKind::Ollama)
                | Some(crate::providers::ProviderKind::OllamaAnthropic)
        );
        let mut resolved_via_ollama = false;
        if is_ollama {
            let base = std::env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| crate::providers::ollama::DEFAULT_BASE_URL.to_string());
            let ollama =
                crate::providers::ollama::OllamaProvider::new().with_base_url(base.clone());
            let model_id = state.config.model.clone();
            match ollama.show(&model_id).await {
                Ok((n, which)) => {
                    let provider_key = match new_kind {
                        Some(crate::providers::ProviderKind::OllamaAnthropic) => "ollama-anthropic",
                        _ => "ollama",
                    };
                    let entry = crate::model_catalogue::ModelEntry {
                        context: Some(n),
                        max_output: None,
                        source: Some(format!("ollama://{base}/api/show ({which})")),
                        verified_at: Some(crate::model_catalogue::today_iso()),
                    };
                    match crate::model_catalogue::upsert_cache_entry(provider_key, &model_id, entry)
                    {
                        Ok(()) => {
                            emit(
                                events_tx,
                                format!(
                                    "auto-scanned '{model_id}' via Ollama /api/show → {n} tokens ({which}); cached for next time"
                                ),
                            );
                            resolved_via_ollama = true;
                        }
                        Err(e) => emit(
                            events_tx,
                            format!(
                                "scanned Ollama context ({n} tokens) but cache write failed: {e}"
                            ),
                        ),
                    }
                }
                Err(e) => emit(
                    events_tx,
                    format!("could not scan Ollama context for '{model_id}': {e}"),
                ),
            }
        }
        if !resolved_via_ollama {
            emit(
                events_tx,
                format!(
                    "⚠ no catalogue entry for '{}' — using {} ({} tokens). Run /models refresh to pick up newer entries.",
                    state.config.model,
                    provider,
                    ctx
                ),
            );
        }
    }
    // Only reset the view's history when we actually forked. On a same-
    // family swap the bubbles / terminal replay stays as-is.
    if !same_family {
        let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));
    }
    let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
        &state.session_store,
        &state.session.id,
    )));
    // Push the sidebar's Provider/Model section immediately so it
    // doesn't lag behind until the 5 s config_poll fires.
    let payload = serde_json::json!({
        "type": "provider_update",
        "provider": provider,
        "model": state.config.model,
        "provider_ready": true,
    });
    let _ = events_tx.send(crate::shared_session::ViewEvent::ProviderUpdate(
        payload.to_string(),
    ));
}

fn doctor_report(state: &WorkerState) -> String {
    let v = crate::version::info();
    let dirty = if v.git_dirty { "+dirty" } else { "" };
    let api_key = if state.config.api_key_from_env().is_some() {
        "set ✓"
    } else {
        "MISSING ✗"
    };
    let sandbox = crate::sandbox::Sandbox::root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "disabled".into());
    let sessions = crate::session::SessionStore::default_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".into());
    let memory = crate::memory::MemoryStore::default_path()
        .map(|p| {
            if p.exists() {
                format!("{} ✓", p.display())
            } else {
                format!("{} (empty)", p.display())
            }
        })
        .unwrap_or_else(|| "none".into());
    let tmux = if crate::team::has_tmux() {
        "available ✓"
    } else {
        "not found"
    };

    format!(
        "── thClaws diagnostics ──\n\
         version:    {}\n\
         revision:   {}{dirty} ({})\n\
         built:      {} ({})\n\
         model:      {}\n\
         provider:   {}\n\
         api key:    {api_key}\n\
         sandbox:    {sandbox}\n\
         sessions:   {sessions}\n\
         memory:     {memory}\n\
         tmux:       {tmux}\n\
         tools:      {} registered\n\
         history:    {} messages\n",
        v.version,
        v.git_sha,
        v.git_branch,
        v.build_time,
        v.build_profile,
        state.config.model,
        state.config.detect_provider().unwrap_or("unknown"),
        state.tool_registry.names().len(),
        state.agent.history_snapshot().len(),
    )
}

fn emit(events_tx: &broadcast::Sender<ViewEvent>, text: String) {
    let _ = events_tx.send(ViewEvent::SlashOutput(text));
}

/// GUI-side mirror of `repl::resolve_skill_install_target` so the Chat
/// tab's `/skill install <name>` resolves a marketplace slug the same
/// way the CLI does. Inlined here (rather than reaching into `repl::`)
/// to keep the GUI's shell_dispatch module self-contained.
fn resolve_skill_install_target_gui(
    arg: &str,
    explicit_name: Option<&str>,
) -> (String, Option<String>, Option<String>) {
    let looks_like_url = arg.contains("://")
        || arg.starts_with("git@")
        || arg.starts_with('/')
        || arg.starts_with("./")
        || arg.starts_with("../")
        || arg.to_ascii_lowercase().ends_with(".zip");
    if looks_like_url {
        return (arg.to_string(), explicit_name.map(String::from), None);
    }
    let mp = crate::marketplace::load();
    match mp.find(arg) {
        Some(entry) if entry.license_tier == "linked-only" => {
            let homepage = if entry.homepage.is_empty() {
                "the upstream repo".to_string()
            } else {
                entry.homepage.clone()
            };
            (
                String::new(),
                None,
                Some(format!(
                    "'{}' is source-available and cannot be redistributed — install directly from {}",
                    entry.name, homepage
                )),
            )
        }
        Some(entry) => match &entry.install_url {
            Some(url) => (
                url.clone(),
                Some(
                    explicit_name
                        .map(String::from)
                        .unwrap_or_else(|| entry.name.clone()),
                ),
                None,
            ),
            None => (
                String::new(),
                None,
                Some(format!(
                    "'{}' has no install_url in the marketplace catalogue",
                    entry.name
                )),
            ),
        },
        None => (
            String::new(),
            None,
            Some(format!(
                "no skill named '{arg}' in marketplace and not a URL — try /skill search <query> or pass a git URL"
            )),
        ),
    }
}

/// Push the latest KMS list to the sidebar after a /kms mutation so
/// the sidebar's list, active-marker, and scope tags reflect the new
/// state without waiting for a full session_update tick.
fn broadcast_kms_update(events_tx: &broadcast::Sender<ViewEvent>) {
    let payload = crate::gui::build_kms_update_payload();
    let _ = events_tx.send(ViewEvent::KmsUpdate(payload.to_string()));
}

/// M6.25: human-readable lint summary for `/kms lint`.
/// M6.29: helper for the `/loop` and `/goal` arms.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// M6.29: short status line for `/goal status`.
fn format_goal_status(g: &crate::goal_state::GoalState) -> String {
    let elapsed = g.time_used_secs();
    let token_budget = g
        .budget_tokens
        .map(|n| format!("{}/{}", g.tokens_used, n))
        .unwrap_or_else(|| format!("{} (unlimited)", g.tokens_used));
    let last = g
        .last_message
        .as_deref()
        .or(g.last_audit.as_deref())
        .unwrap_or("(none)");
    format!(
        "goal status: {}\n  elapsed: {}s, iterations: {}, tokens: {}\n  objective: {}\n  last: {}",
        g.status.as_str(),
        elapsed,
        g.iterations_done,
        token_budget,
        truncate_inline(&g.objective, 100),
        truncate_inline(last, 200),
    )
}

/// M6.29: full goal contents for `/goal show`.
fn format_goal_show(g: &crate::goal_state::GoalState) -> String {
    let mut out = format!(
        "goal: {}\nstatus: {}\nstarted_at: {}\niterations_done: {}\ntokens_used: {}\n",
        g.objective,
        g.status.as_str(),
        g.started_at,
        g.iterations_done,
        g.tokens_used,
    );
    if let Some(b) = g.budget_tokens {
        out.push_str(&format!("budget_tokens: {b}\n"));
    }
    if let Some(b) = g.budget_time_secs {
        out.push_str(&format!("budget_time_secs: {b}\n"));
    }
    if let Some(a) = &g.last_audit {
        out.push_str(&format!("last_audit: {a}\n"));
    }
    if let Some(m) = &g.last_message {
        out.push_str(&format!("last_message: {m}\n"));
    }
    if let Some(c) = g.completed_at {
        out.push_str(&format!("completed_at: {c}\n"));
    }
    out
}

fn truncate_inline(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out.replace('\n', " ")
    }
}

fn format_lint_report(name: &str, report: &crate::kms::LintReport) -> String {
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
    out
}

/// M6.25 BUG #4: alias-sanitizer used by /kms file-answer. Same rules
/// as `kms::sanitize_alias` (which is private to that module).
fn sanitize_alias_for_dispatch(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.trim_matches('_').to_string()
}

/// Same shape as [`broadcast_kms_update`], for the MCP-server list. Read
/// fresh from disk by `build_mcp_update_payload` so user-scope removals
/// (which the live tool registry can't surgically reflect) at least
/// disappear from the sidebar immediately.
fn broadcast_mcp_update(events_tx: &broadcast::Sender<ViewEvent>) {
    let payload = crate::gui::build_mcp_update_payload();
    let _ = events_tx.send(ViewEvent::McpUpdate(payload.to_string()));
}

/// M6.16 BUG H1 helper: refresh skill_store + rebuild system prompt
/// + rebuild agent so plugin contributions stop / start being callable
/// in this session without a restart. Mirrors the install path's
/// refresh block; called from PluginRemove / PluginEnable / PluginDisable.
/// MCP subprocess teardown is NOT handled here — the live tool registry
/// can't surgically detach an already-spawned client. Callers append a
/// hint to their user-facing message when the plugin contributed MCP
/// servers (see has_running_mcp_contributions).
fn refresh_after_plugin_change(
    state: &mut crate::shared_session::WorkerState,
    events_tx: &broadcast::Sender<crate::shared_session::ViewEvent>,
) {
    let refreshed = crate::skills::SkillStore::discover();
    if let Ok(mut store) = state.skill_store.lock() {
        *store = refreshed;
    }
    state.rebuild_system_prompt();
    if let Err(e) = state.rebuild_agent(true) {
        emit(events_tx, format!("[plugin] agent rebuild failed: {e}"));
    }
}

/// Sorted list of MCP server names a plugin contributes (or `None`
/// if the plugin isn't found / has no MCP servers / manifest unread).
/// Used by /plugin install / enable / disable / remove to render an
/// emphasized "⚠  restart to spawn / drop server(s): a, b, c" hint
/// — the user gets the actual names so they know what's coming
/// after relaunch, not just a generic "restart needed" note.
/// M6.16.1 — replaces the older boolean `has_running_mcp_contributions`.
fn mcp_server_names(name: &str) -> Option<Vec<String>> {
    let plugin = crate::plugins::find_installed(name)?;
    let manifest = plugin.manifest().ok()?;
    if manifest.mcp_servers.is_empty() {
        return None;
    }
    let mut names: Vec<String> = manifest.mcp_servers.keys().cloned().collect();
    names.sort();
    Some(names)
}
