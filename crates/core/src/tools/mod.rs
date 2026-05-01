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
use std::collections::HashMap;
use std::sync::Arc;

pub mod ask;
pub mod bash;
pub mod docx_create;
pub mod docx_edit;
pub mod docx_read;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod kms;
pub mod ls;
pub mod pdf_create;
pub mod pdf_read;
pub mod plan;
pub mod pptx_create;
pub mod pptx_edit;
pub mod pptx_read;
pub mod read;
pub mod search;
pub mod tasks;
pub mod todo;
pub mod web;
pub mod write;
pub mod xlsx_create;
pub mod xlsx_edit;
pub mod xlsx_read;

pub use ask::{set_gui_ask_sender, AskUserRequest, AskUserTool};
pub use bash::BashTool;
pub use docx_create::DocxCreateTool;
pub use docx_edit::DocxEditTool;
pub use docx_read::DocxReadTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use kms::{KmsReadTool, KmsSearchTool};
pub use ls::LsTool;
pub use pdf_create::PdfCreateTool;
pub use pdf_read::PdfReadTool;
pub use plan::{EnterPlanModeTool, ExitPlanModeTool};
pub use pptx_create::PptxCreateTool;
pub use pptx_edit::PptxEditTool;
pub use pptx_read::PptxReadTool;
pub use read::ReadTool;
pub use search::WebSearchTool;
pub use todo::TodoWriteTool;
pub use web::WebFetchTool;
pub use write::WriteTool;
pub use xlsx_create::XlsxCreateTool;
pub use xlsx_edit::XlsxEditTool;
pub use xlsx_read::XlsxReadTool;

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

    /// MCP-Apps widget the chat surface should embed inline alongside
    /// this tool's results. Returns `(uri, html, mime)` where `html` is
    /// the resource body to mount in an iframe and `mime` is the
    /// declared resource MIME (typically `text/html;profile=mcp-app`).
    /// Default: no widget. Only [`crate::mcp::McpTool`] overrides this
    /// today — a non-MCP tool has nothing to fetch.
    async fn fetch_ui_resource(&self) -> Option<UiResource> {
        None
    }
}

/// A resolved MCP-Apps UI resource ready to be mounted in an iframe.
/// Produced by [`Tool::fetch_ui_resource`] after a tool call completes.
#[derive(Debug, Clone)]
pub struct UiResource {
    pub uri: String,
    pub html: String,
    pub mime: Option<String>,
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
        r.register(Arc::new(WriteTool));
        r.register(Arc::new(EditTool));
        r.register(Arc::new(GlobTool));
        r.register(Arc::new(GrepTool));
        r.register(Arc::new(BashTool));
        r.register(Arc::new(DocxCreateTool));
        r.register(Arc::new(DocxEditTool));
        r.register(Arc::new(DocxReadTool));
        r.register(Arc::new(XlsxCreateTool));
        r.register(Arc::new(XlsxEditTool));
        r.register(Arc::new(XlsxReadTool));
        r.register(Arc::new(PptxCreateTool));
        r.register(Arc::new(PptxEditTool));
        r.register(Arc::new(PptxReadTool));
        r.register(Arc::new(PdfCreateTool));
        r.register(Arc::new(PdfReadTool));
        r.register(Arc::new(WebFetchTool::new()));
        r.register(Arc::new(WebSearchTool::default()));
        r.register(Arc::new(AskUserTool));
        r.register(Arc::new(TodoWriteTool));
        r.register(Arc::new(EnterPlanModeTool));
        r.register(Arc::new(ExitPlanModeTool));
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
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut defs: Vec<ToolDef> = self
            .tools
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Invoke a tool by name.
    pub async fn call(&self, name: &str, input: Value) -> Result<String> {
        let tool = self
            .get(name)
            .ok_or_else(|| Error::Tool(format!("unknown tool: {name}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
                "ExitPlanMode",
                "Glob",
                "Grep",
                "Ls",
                "PdfCreate",
                "PdfRead",
                "PptxCreate",
                "PptxEdit",
                "PptxRead",
                "Read",
                "TodoWrite",
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
}
