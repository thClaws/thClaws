//! Tool trait + registry.
//!
//! Tools are named, described, and hand a JSON schema for their input.
//! The agent loop (Phase 9) picks a tool from the registry by name after
//! the provider emits a `ContentBlock::ToolUse`, invokes `call()`, and feeds
//! the returned string back as a `ContentBlock::ToolResult`.

use crate::error::{Error, Result};
use crate::types::{ToolDef, ToolResultContent};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

pub mod ask;
pub mod bash;
pub mod docx_create;
pub mod docx_edit;
pub mod docx_read;
pub mod edit;
pub mod epub_create;
pub mod fetch_images;
pub mod filmscript;
pub mod folder_index;
pub mod glob;
pub mod grep;
pub mod gui_shell;
pub mod hal;
pub mod image_gen;
pub mod kms;
pub mod ls;
pub mod md_tables;
pub mod memory;
pub mod pdf_create;
pub mod pdf_read;
pub mod plan;
pub mod plan_state;
pub mod pptx_create;
pub mod pptx_edit;
pub mod pptx_read;
pub mod quiz_render;
pub mod read;
pub mod search;
pub mod session_rename;
pub mod slide_render;
pub mod speech_gen;
pub mod tasks;
pub mod todo;
pub mod todo_state;
pub mod update_goal;
pub mod video_gen;
pub mod watch_video;
pub mod web;
pub mod workflow_run;
pub mod write;
pub mod xlsx_create;
pub mod xlsx_edit;
pub mod xlsx_read;

pub use ask::{set_gui_ask_sender, set_line_driven_turn, AskUserRequest, AskUserTool};
pub use bash::BashTool;
pub use docx_create::DocxCreateTool;
pub use docx_edit::DocxEditTool;
pub use docx_read::DocxReadTool;
pub use edit::EditTool;
pub use epub_create::EpubCreateTool;
pub use fetch_images::FetchImagesTool;
pub use folder_index::FolderIndexTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use hal::{WebScrapeTool, YouTubeTranscriptTool};
pub use image_gen::{ImageToImageTool, TextToImageTool};
pub use kms::{
    KmsAppendTool, KmsCreateTool, KmsDeleteTool, KmsReadTool, KmsSearchTool, KmsWriteSourceTool,
    KmsWriteTool,
};
pub use ls::LsTool;
pub use memory::{MemoryAppendTool, MemoryReadTool, MemoryWriteTool};
pub use pdf_create::PdfCreateTool;
pub use pdf_read::PdfReadTool;
pub use plan::{EnterPlanModeTool, ExitPlanModeTool, SubmitPlanTool, UpdatePlanStepTool};
pub use pptx_create::PptxCreateTool;
pub use pptx_edit::PptxEditTool;
pub use pptx_read::PptxReadTool;
pub use quiz_render::QuizRenderTool;
pub use read::ReadTool;
pub use search::WebSearchTool;
pub use session_rename::SessionRenameTool;
pub use slide_render::RenderSlidesTool;
pub use speech_gen::TextToSpeechTool;
pub use todo::TodoWriteTool;
pub use update_goal::{MarkGoalBlockedTool, MarkGoalCompleteTool, RecordGoalProgressTool};
pub use video_gen::{ImageToVideoTool, MediaJobStatusTool, TextToVideoTool};
pub use watch_video::WatchVideoTool;
pub use web::WebFetchTool;
pub use workflow_run::WorkflowRunTool;
pub use write::WriteTool;
pub use xlsx_create::XlsxCreateTool;
pub use xlsx_edit::XlsxEditTool;
pub use xlsx_read::XlsxReadTool;

/// What a tool reports about one call for the audit record. `targets`
/// are workspace-relative paths; `summary` is clamped to 256 bytes and
/// its first line by the audit layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditSummary {
    pub targets: Vec<String>,
    pub summary: Option<String>,
}

impl AuditSummary {
    pub fn targets(paths: impl IntoIterator<Item = String>) -> Self {
        Self {
            targets: paths.into_iter().collect(),
            summary: None,
        }
    }
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            targets: Vec::new(),
            summary: Some(s.into()),
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    async fn call(&self, input: Value) -> Result<String>;

    /// Multimodal variant. Override for tools that produce non-text
    /// artifacts (Read on image files, future image-generation tools,
    /// etc.). The default impl wraps `call()`'s string output as Text,
    /// so existing tools need no changes.
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        self.call(input).await.map(ToolResultContent::Text)
    }

    /// Whether this tool requires user approval before execution when the
    /// permission mode is `Ask`. Default: false (read-only). Override for
    /// tools that mutate filesystem or system state.
    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    /// Audit classification (RFC 0001). Builtins are the default; MCP
    /// and workflow tools override.
    fn audit_kind(&self) -> crate::audit::AuditToolKind {
        crate::audit::AuditToolKind::Builtin
    }

    /// MCP server name for `audit_kind() == Mcp`.
    fn audit_mcp_server(&self) -> Option<&str> {
        None
    }

    /// Payload-free description of a call for the audit record: paths a
    /// file tool touches, the first line of a shell command. Tools decide
    /// what is safe to write; the audit layer only clamps length. Default:
    /// nothing.
    fn audit_summary(&self, _input: &Value) -> Option<AuditSummary> {
        None
    }

    /// Whether this tool is safe to run **concurrently** with other
    /// parallelizable calls in the same turn — read-only, no approval, no
    /// shared-state mutation. When a turn emits ≥2 tool calls that are ALL
    /// parallelizable, the agent loop dispatches them in one concurrent
    /// batch instead of awaiting them one-by-one (a big win for fan-out:
    /// parallel file reads, parallel `Task` subagents). Default: false —
    /// mutating tools (Write/Edit/Bash/…) stay strictly sequential.
    fn parallelizable(&self) -> bool {
        false
    }

    /// MCP-Apps widget the chat surface should embed inline alongside
    /// this tool's results. Returns `(uri, html, mime)` where `html` is
    /// the resource body to mount in an iframe and `mime` is the
    /// declared resource MIME (typically `text/html;profile=mcp-app`).
    /// Default: no widget. Only [`crate::mcp::McpTool`] overrides this
    /// today — a non-MCP tool has nothing to fetch.
    async fn fetch_ui_resource(&self) -> Option<UiResource> {
        None
    }

    /// Env vars this tool needs at runtime (API keys for upstream
    /// services). When **any** listed var is unset or empty, the tool
    /// is hidden from [`ToolRegistry::tool_defs`] (the model never
    /// sees its name) and [`ToolRegistry::call`] rejects invocation
    /// (defense in depth).
    ///
    /// Default: `&[]` (always available — covers Read, Bash, etc.).
    /// Tools that wrap a keyed upstream return their env var names
    /// (e.g. `&["HAL_API_KEY"]`). Multiple entries are AND-ed: the
    /// tool is available only when *every* listed var is present.
    fn requires_env(&self) -> &'static [&'static str] {
        &[]
    }

    /// Optional gate this tool belongs to. `None` (default) → always
    /// visible (subject to `requires_env`). `Some("gui-shell")` → hidden
    /// from [`ToolRegistry::tool_defs`] and rejected by
    /// [`ToolRegistry::call`] until something opens that gate via
    /// [`activate_gate`]. Lets a whole group of tools (e.g. GUI-Shell
    /// authoring) be lazily surfaced by a skill instead of living in the
    /// always-on system prompt — zero token cost while closed, because
    /// the model never sees a gated tool's name. Mirrors `requires_env`:
    /// the gate is re-checked every turn by the same per-turn filter, so
    /// opening it takes effect on the next request without rebuilding the
    /// registry or the agent.
    fn requires_gate(&self) -> Option<&'static str> {
        None
    }

    /// Downcast hook. Default `None`; concrete tools that need to be
    /// recovered from an `Arc<dyn Tool>` (e.g. the skill tools, so the
    /// subagent factory can rebuild allow-list-scoped copies sharing the
    /// same store handle) override this to return `Some(self)`.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }

    /// Declared output JSON Schema for a named subagent, if any (from the
    /// def's `output_schema`). Default `None`. The `Task` tool
    /// (`SubAgentTool`) overrides this so a workflow
    /// `thclaws.subagent({agent})` call that omits a per-call `schema`
    /// still gets schema validation — the schema lives in one place (the
    /// agent def) instead of being duplicated in the workflow JS.
    fn subagent_output_schema(&self, _agent: &str) -> Option<serde_json::Value> {
        None
    }
}

/// Process-global set of open tool gates. Session-sticky: once a gate is
/// opened it stays open for the process lifetime (a user mid-task keeps
/// needing the group's tools across turns). Same global-state model as
/// the env vars `tool_is_available` already reads and the
/// `skills_state` model-override slot.
fn open_gates() -> &'static Mutex<HashSet<String>> {
    static G: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Open a tool gate — every registered tool whose `requires_gate()`
/// matches `name` becomes visible to the model on the next turn. Called
/// from `SkillTool::call` when an invoked skill declares `tool-gate:`.
pub fn activate_gate(name: &str) {
    open_gates()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(name.to_string());
}

/// Whether a named gate is currently open.
pub fn gate_is_active(name: &str) -> bool {
    open_gates()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(name)
}

/// Test helper — close every gate so cases don't leak gate state.
#[cfg(test)]
pub fn reset_gates() {
    open_gates()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

/// True when the cloud gateway is the active route for keyed upstreams
/// (LLM, web search, HAL): a cloud pod (env `THCLAWS_USES_GATEWAY` or
/// multiuser) OR the desktop "gateway proxy" config toggle — the SAME
/// signal `gateway_route()` resolves a token for. A tool that
/// `requires_env` a gateway-served key (HAL) counts as available whenever
/// this is true, even with no local key (the gateway injects it).
///
/// Note this reads `AppConfig` (disk) for the desktop toggle, so callers
/// should evaluate it lazily — only when a local key is actually missing.
pub(crate) fn gateway_active() -> bool {
    std::env::var("THCLAWS_USES_GATEWAY").ok().as_deref() == Some("1")
        || crate::workdir::is_multiuser()
        || crate::config::AppConfig::load()
            .map(|c| c.gateway_proxy)
            .unwrap_or(false)
}

/// Resolved cloud-gateway route (base URL + bearer). `Some` only in
/// gateway mode with both envs present. Tools that proxy a keyed
/// upstream (HAL, web search) use this to reach `{base}/<svc>/…` with
/// the bearer; the gateway injects the real credential.
pub(crate) struct GatewayRoute {
    pub base: String,
    pub token: String,
}

pub(crate) fn gateway_route() -> Option<GatewayRoute> {
    // The gateway is active on a cloud pod (env / multiuser) OR when the
    // desktop "gateway proxy" toggle is on — the SAME signal that routes
    // LLM traffic (config.rs derives `gateway_use_for` from exactly this).
    // Resolve the base + key via the canonical resolver (env → `gateway`
    // keychain bundle → thClaws.cloud login token) so keyed services (web
    // search, HAL) reach the gateway on DESKTOP too — not only on cloud
    // pods that export THCLAWS_USES_GATEWAY. Previously this was env-only,
    // so a desktop gateway user's WebSearch silently fell back to
    // DuckDuckGo (and a scheduled run produced stale, un-searched results).
    if !gateway_active() {
        return None;
    }
    let token = crate::providers::thclaws_gateway::resolve_access_key()?;
    let base = crate::providers::thclaws_gateway::resolve_base_url();
    if base.is_empty() || token.is_empty() {
        return None;
    }
    Some(GatewayRoute { base, token })
}

/// Env vars whose backing service the cloud gateway fronts. In gateway
/// mode the runner holds no raw key for these (the gateway injects it),
/// so a tool that `requires_env` one of them is still available.
pub(crate) const GATEWAY_SERVED_ENVS: &[&str] = &["HAL_API_KEY"];

/// Whether a tool's env-var requirements are currently satisfied.
/// Reads `std::env` so live changes (`api_key_set` / `api_key_clear`
/// followed by a `rebuild_agent`) take effect on the next turn
/// without reconstructing the registry. When the gateway is active a
/// requirement on a gateway-served key counts as satisfied even with no
/// local key. `gateway_active()` is evaluated lazily (it reads config
/// from disk) — only when a local key is actually absent and the var is
/// one the gateway fronts.
fn tool_is_available(t: &dyn Tool) -> bool {
    let env_ok = t.requires_env().iter().all(|v| {
        std::env::var(v).map(|val| !val.is_empty()).unwrap_or(false)
            || (GATEWAY_SERVED_ENVS.contains(v) && gateway_active())
    });
    // A gated tool is available only once its gate has been opened.
    let gate_ok = t.requires_gate().is_none_or(gate_is_active);
    env_ok && gate_ok
}

/// A resolved MCP-Apps UI resource ready to be mounted in an iframe.
/// Produced by [`Tool::fetch_ui_resource`] after a tool call completes.
#[derive(Debug, Clone)]
pub struct UiResource {
    pub uri: String,
    pub html: String,
    pub mime: Option<String>,
    /// When true, the frontend's `McpAppIframe` mounts the widget with
    /// `sandbox="allow-scripts allow-popups allow-forms allow-same-origin"`
    /// instead of the default (`allow-same-origin` omitted). MCP-Apps
    /// widgets from arbitrary servers leave this `false` so the widget
    /// gets an opaque origin and can't reach back into thClaws state.
    /// First-party tools that need to load `<script src>` and assets
    /// from a localhost preview server (e.g. `GamedevPreview`'s game
    /// iframe) set it `true`; the trust is implicit because the tool
    /// ships inside the thClaws binary.
    pub allow_same_origin: bool,
    /// First-party opt-in for content-driven inline iframe height. When
    /// `true`, the frontend's `McpAppIframe` honours
    /// `ui/notifications/size-changed` messages from the widget
    /// (capped at 85% of viewport) instead of using the fixed
    /// `INLINE_HEIGHT`. Independent from `allow_same_origin` —
    /// sandbox policy is orthogonal to size policy, so an external
    /// trusted server can grant same-origin without unlocking
    /// content-driven resize and vice versa.
    pub auto_size: bool,
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the built-in tools (file, search, shell, web, user interaction,
    /// plan mode). Task tools require shared state and are registered separately
    /// via `tools::tasks::register_task_tools`.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(LsTool));
        r.register(Arc::new(ReadTool));
        r.register(Arc::new(WatchVideoTool));
        r.register(Arc::new(WriteTool));
        r.register(Arc::new(EditTool));
        r.register(Arc::new(GlobTool));
        r.register(Arc::new(GrepTool));
        r.register(Arc::new(BashTool::default()));
        r.register(Arc::new(DocxCreateTool));
        r.register(Arc::new(DocxEditTool));
        r.register(Arc::new(DocxReadTool));
        r.register(Arc::new(XlsxCreateTool));
        r.register(Arc::new(XlsxEditTool));
        r.register(Arc::new(XlsxReadTool));
        r.register(Arc::new(PptxCreateTool));
        r.register(Arc::new(PptxEditTool));
        r.register(Arc::new(PptxReadTool));
        r.register(Arc::new(EpubCreateTool));
        r.register(Arc::new(PdfCreateTool));
        r.register(Arc::new(PdfReadTool));
        r.register(Arc::new(WebFetchTool::new()));
        r.register(Arc::new(FetchImagesTool::new()));
        r.register(Arc::new(FolderIndexTool));
        r.register(Arc::new(WebSearchTool::default()));
        // HAL Public API tools (YouTubeTranscript, WebScrape) are NOT
        // registered here — they're opt-in via `hal_enabled` (Settings →
        // Optional features), registered per-surface like the media tools.
        // (They also still require HAL_API_KEY / gateway via requires_env.)
        r.register(Arc::new(AskUserTool));
        r.register(Arc::new(TodoWriteTool));
        r.register(Arc::new(QuizRenderTool::new()));
        r.register(Arc::new(EnterPlanModeTool));
        r.register(Arc::new(ExitPlanModeTool));
        r.register(Arc::new(SubmitPlanTool));
        r.register(Arc::new(UpdatePlanStepTool));
        // GUI-shell authoring tools — gated behind the `gui-shell` gate
        // (the `gui-shell` skill opens it), so they're invisible to the
        // model until a user asks to build a shell.
        gui_shell::register(&mut r);
        filmscript::register(&mut r);
        r
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn remove(&mut self, name: &str) {
        self.tools.remove(name);
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Build the `ToolDef` list to send to a provider.
    ///
    /// Tools whose [`Tool::requires_env`] vars aren't all present in
    /// the process env are filtered out — the model never sees their
    /// names, so it can't try to call them. Re-evaluated each turn
    /// (env reads are cheap), so live key changes flip tools in/out
    /// without restart.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut defs: Vec<ToolDef> = self
            .tools
            .values()
            .filter(|t| tool_is_available(t.as_ref()))
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Invoke a tool by name. Defense in depth: even if a tool's
    /// requires_env is currently unsatisfied (so it's hidden from
    /// [`Self::tool_defs`]), a stale provider response or hand-crafted
    /// call shouldn't be able to reach it. Reject with a clear error.
    pub async fn call(&self, name: &str, input: Value) -> Result<String> {
        let tool = self
            .get(name)
            .ok_or_else(|| Error::Tool(format!("unknown tool: {name}")))?;
        if !tool_is_available(tool.as_ref()) {
            if let Some(gate) = tool.requires_gate().filter(|g| !gate_is_active(g)) {
                return Err(Error::Tool(format!(
                    "tool '{name}' is gated behind '{gate}' — invoke the matching skill to enable it"
                )));
            }
            let needed = tool.requires_env().join(", ");
            return Err(Error::Tool(format!(
                "tool '{name}' requires env var(s) [{needed}] — set in Settings → Providers and retry"
            )));
        }
        tool.call(input).await
    }
}

/// Helper for implementations to pull a required string field from input.
pub fn req_str<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Tool(format!("missing or non-string field: {field}")))
}

/// True when any of `parts` names a hidden/dot path component (e.g.
/// `.thclaws/sessions/*.jsonl`, `.github`). The Glob/Grep read walkers
/// skip hidden + gitignored entries by default — good for clean output
/// on a plain `**/*.rs`, but it makes legitimate dot-dirs (`.thclaws/`,
/// `.github/`, `.config/`) invisible. When the caller *explicitly* asks
/// for a dot-path we must descend into it; this detects that intent.
/// `.` and `..` don't count — they're traversal, not hidden names.
pub(crate) fn targets_hidden_path<'a>(parts: impl IntoIterator<Item = &'a str>) -> bool {
    parts.into_iter().any(|s| {
        // Absolute paths point at/above the walk root; their dot-segments
        // are ancestors the walker never filters (e.g. a project under
        // `~/.config/...`), so they don't signal intent to descend into a
        // hidden child. Only relative dot-paths and glob patterns do.
        if s.starts_with('/') || s.starts_with('\\') {
            return false;
        }
        s.split(['/', '\\'])
            .any(|seg| seg.len() > 1 && seg.starts_with('.') && seg != "..")
    })
}

/// Build a `WalkBuilder` for the read tools (Glob/Grep). Defaults skip
/// hidden + ignored entries. When `include_hidden` is set (the request
/// explicitly targeted a dot-path — see [`targets_hidden_path`]) all of
/// the hidden/ignore filters are disabled so the requested tree —
/// including gitignored dot-dirs like `.thclaws/` — is fully visible.
pub(crate) fn read_walker(base: &std::path::Path, include_hidden: bool) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(base);
    if include_hidden {
        b.hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
    }
    b
}

/// M6.38.9: parse a tool result body for a leading `Source: <engine>`
/// line. Returns the engine name with trailing parenthetical /
/// fallback annotations stripped. Used by the CLI + chat tool-call
/// indicator to surface the source next to the ✓ checkmark,
/// independent of whether the model carries it through into its
/// natural-language summary.
///
/// Example inputs / outputs:
///
/// - `"Source: Tavily (web search)\n\n1. ...".`     → `Some("Tavily")`
/// - `"Source: DuckDuckGo (web search) — fallback after tavily: HTTP 429\n\n..."`
///   → `Some("DuckDuckGo")`
/// - `"1. some result"` → `None`
/// - `""` → `None`
///
/// The contract is one-line + colon-prefixed, deliberately strict —
/// false positives in the indicator are worse than misses, and any
/// tool that opts in just leads its output with that line.
pub fn extract_tool_source(body: &str) -> Option<&str> {
    let first = body.lines().next()?;
    let rest = first.strip_prefix("Source: ")?;
    // Strip trailing parenthetical (`(web search)`) and/or fallback
    // annotation (`— fallback after ...`). Both are dropped so the
    // indicator stays compact: `(via Tavily)`, not
    // `(via Tavily (web search) — fallback after ...)`.
    let end = rest
        .find(" (")
        .or_else(|| rest.find(" —"))
        .unwrap_or(rest.len());
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide lock to serialize env-var manipulation across the
    /// requires_env / tool_defs filter tests. Same pattern as
    /// `search::tests::env_lock`.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// RAII guard that restores an env var to its prior value on drop.
    /// Lets tests mutate HAL_API_KEY (and others) without leaking state
    /// to other tests under `cargo test`'s parallel runner.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn new(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
        fn set(&self, val: &str) {
            std::env::set_var(self.key, val);
        }
        fn unset(&self) {
            std::env::remove_var(self.key);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test]
    async fn registry_dispatches_by_name() {
        let reg = ToolRegistry::with_builtins();
        assert!(reg.get("Read").is_some());
        assert!(reg.get("Write").is_some());
        assert!(reg.get("Edit").is_some());
        assert!(reg.get("DoesNotExist").is_none());
    }

    #[tokio::test]
    async fn registry_unknown_tool_errors() {
        let reg = ToolRegistry::with_builtins();
        let err = reg
            .call("NopeTool", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unknown tool"));
    }

    #[test]
    fn tool_defs_are_sorted_and_complete() {
        let _g = env_lock().lock().unwrap();
        // HAL tools should be filtered from the default list when
        // HAL_API_KEY is unset. Force-clear so a local export doesn't
        // make the snapshot flaky.
        let _hal = EnvGuard::new("HAL_API_KEY");
        let reg = ToolRegistry::with_builtins();
        let defs = reg.tool_defs();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "AskUserQuestion",
                "Bash",
                "DocxCreate",
                "DocxEdit",
                "DocxRead",
                "Edit",
                "EnterPlanMode",
                "EpubCreate",
                "ExitPlanMode",
                "Glob",
                "Grep",
                "Ls",
                "PdfCreate",
                "PdfRead",
                "PptxCreate",
                "PptxEdit",
                "PptxRead",
                "QuizRender",
                "Read",
                "SubmitPlan",
                "TodoWrite",
                "UpdatePlanStep",
                "WatchVideo",
                "WebFetch",
                "WebSearch",
                "Write",
                "XlsxCreate",
                "XlsxEdit",
                "XlsxRead"
            ]
        );
        for def in &defs {
            assert!(!def.description.is_empty());
            assert_eq!(def.input_schema["type"], "object");
            assert!(def.input_schema["properties"].is_object());
        }
    }

    /// Stub tool used by the filter tests below. Declares a
    /// configurable `requires_env` list; everything else is a no-op.
    struct StubTool {
        name: &'static str,
        env: &'static [&'static str],
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "stub for tests"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn call(&self, _input: Value) -> Result<String> {
            Ok("ok".into())
        }
        fn requires_env(&self) -> &'static [&'static str] {
            self.env
        }
    }

    #[test]
    fn requires_env_default_empty_means_always_visible() {
        let _g = env_lock().lock().unwrap();
        let _hal = EnvGuard::new("HAL_API_KEY");
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "AlwaysOn",
            env: &[],
        }));
        let defs = reg.tool_defs();
        assert!(defs.iter().any(|d| d.name == "AlwaysOn"));
    }

    #[test]
    fn requires_env_filter_excludes_when_unset() {
        let _g = env_lock().lock().unwrap();
        let _key = EnvGuard::new("FAKE_TEST_KEY_UNSET");
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "NeedsKey",
            env: &["FAKE_TEST_KEY_UNSET"],
        }));
        let defs = reg.tool_defs();
        assert!(
            !defs.iter().any(|d| d.name == "NeedsKey"),
            "tool should be hidden when its env var is unset"
        );
    }

    #[test]
    fn requires_env_filter_includes_when_set() {
        let _g = env_lock().lock().unwrap();
        let key = EnvGuard::new("FAKE_TEST_KEY_PRESENT");
        key.set("any-non-empty-value");
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "NeedsKey",
            env: &["FAKE_TEST_KEY_PRESENT"],
        }));
        let defs = reg.tool_defs();
        assert!(defs.iter().any(|d| d.name == "NeedsKey"));
    }

    #[test]
    fn requires_env_gateway_served_key_visible_when_gateway_active() {
        let _g = env_lock().lock().unwrap();
        // HAL_API_KEY is gateway-served: with no local key but the gateway
        // active, a tool requiring it stays visible (the gateway injects the
        // real key). Uses the env signal — THCLAWS_USES_GATEWAY short-circuits
        // gateway_active() so the assertion doesn't depend on disk config.
        let hal = EnvGuard::new("HAL_API_KEY");
        hal.unset();
        let gw = EnvGuard::new("THCLAWS_USES_GATEWAY");
        gw.set("1");
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "NeedsGatewayKey",
            env: &["HAL_API_KEY"],
        }));
        assert!(
            reg.tool_defs().iter().any(|d| d.name == "NeedsGatewayKey"),
            "gateway-served tool should be visible when the gateway is active, no local key"
        );
    }

    #[test]
    fn requires_env_treats_empty_string_as_unset() {
        let _g = env_lock().lock().unwrap();
        let key = EnvGuard::new("FAKE_TEST_KEY_EMPTY");
        key.set(""); // explicit empty — should still hide the tool
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "NeedsKey",
            env: &["FAKE_TEST_KEY_EMPTY"],
        }));
        let defs = reg.tool_defs();
        assert!(!defs.iter().any(|d| d.name == "NeedsKey"));
    }

    /// Stub tool that declares a fixed gate.
    struct GatedStub;
    #[async_trait]
    impl Tool for GatedStub {
        fn name(&self) -> &'static str {
            "GatedTool"
        }
        fn description(&self) -> &'static str {
            "gated stub"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn call(&self, _input: Value) -> Result<String> {
            Ok("ok".into())
        }
        fn requires_gate(&self) -> Option<&'static str> {
            Some("test-gate")
        }
    }

    #[test]
    fn gated_tool_hidden_until_gate_opened() {
        let _g = env_lock().lock().unwrap();
        reset_gates();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(GatedStub));
        assert!(
            !reg.tool_defs().iter().any(|d| d.name == "GatedTool"),
            "gated tool must be hidden before the gate opens"
        );
        activate_gate("test-gate");
        assert!(
            reg.tool_defs().iter().any(|d| d.name == "GatedTool"),
            "gated tool must appear once the gate opens"
        );
        reset_gates();
    }

    #[tokio::test]
    async fn gated_tool_call_rejected_until_gate_opened() {
        let _g = env_lock().lock().unwrap();
        reset_gates();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(GatedStub));
        let err = reg
            .call("GatedTool", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("gated"), "got: {err}");
        activate_gate("test-gate");
        assert!(reg.call("GatedTool", serde_json::json!({})).await.is_ok());
        reset_gates();
    }

    #[tokio::test]
    async fn requires_env_call_path_rejects_when_unset() {
        let _g = env_lock().lock().unwrap();
        let _key = EnvGuard::new("FAKE_TEST_KEY_CALL");
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "NeedsKey",
            env: &["FAKE_TEST_KEY_CALL"],
        }));
        // Even bypassing tool_defs (e.g. a stale provider response), an
        // explicit call() must refuse when the env isn't satisfied.
        let err = reg
            .call("NeedsKey", serde_json::json!({}))
            .await
            .unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("FAKE_TEST_KEY_CALL"), "got: {s}");
        assert!(s.contains("requires env var"), "got: {s}");
    }

    #[test]
    fn extract_tool_source_finds_engine_in_first_line() {
        // Happy path — WebSearch's exact M6.38.8 shape.
        assert_eq!(
            extract_tool_source("Source: Tavily (web search)\n\n1. result"),
            Some("Tavily")
        );
        assert_eq!(
            extract_tool_source("Source: Brave Search (web search)\n\n1. result"),
            Some("Brave Search")
        );
        // Fallback annotation — strip the trailing — clause.
        assert_eq!(
            extract_tool_source(
                "Source: DuckDuckGo (web search) — fallback after tavily: HTTP 429\n\n1. r"
            ),
            Some("DuckDuckGo")
        );
        // No parenthetical, no fallback — engine is the whole rest.
        assert_eq!(extract_tool_source("Source: Tavily"), Some("Tavily"));
        // Trailing — without parenthetical.
        assert_eq!(extract_tool_source("Source: Tavily — note"), Some("Tavily"));
    }

    #[test]
    fn extract_tool_source_returns_none_when_absent() {
        assert_eq!(extract_tool_source(""), None);
        assert_eq!(extract_tool_source("1. some result"), None);
        // Wrong prefix (case-sensitive on purpose — matches the
        // M6.38.8 emit format exactly).
        assert_eq!(extract_tool_source("source: Tavily"), None);
        assert_eq!(extract_tool_source("SOURCE: Tavily"), None);
        // Empty engine name → None (don't render `(via )`).
        assert_eq!(extract_tool_source("Source: "), None);
        assert_eq!(extract_tool_source("Source:  "), None);
    }

    #[test]
    fn extract_tool_source_only_inspects_first_line() {
        // A `Source:` further down in the body shouldn't match —
        // false positives are worse than misses.
        let body = "Some content\nSource: Tavily\nmore";
        assert_eq!(extract_tool_source(body), None);
    }

    #[test]
    fn hal_tools_hidden_without_key_visible_with_key() {
        let _g = env_lock().lock().unwrap();
        let key = EnvGuard::new("HAL_API_KEY");
        key.unset();
        // HAL tools are opt-in (no longer in with_builtins — they register
        // per surface when hal_enabled). Register them directly here so the
        // test exercises their requires_env gating, not registry membership.
        let gw = EnvGuard::new("THCLAWS_USES_GATEWAY");
        gw.unset();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(crate::tools::YouTubeTranscriptTool::new()));
        reg.register(Arc::new(crate::tools::WebScrapeTool::new()));

        // No key (and gateway inactive) → hidden.
        let defs = reg.tool_defs();
        assert!(!defs.iter().any(|d| d.name == "YouTubeTranscript"));
        assert!(!defs.iter().any(|d| d.name == "WebScrape"));

        // Key set → visible.
        key.set("hal_test_key");
        let defs = reg.tool_defs();
        assert!(defs.iter().any(|d| d.name == "YouTubeTranscript"));
        assert!(defs.iter().any(|d| d.name == "WebScrape"));
    }
}
