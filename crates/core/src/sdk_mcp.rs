//! In-process SDK MCP server for the Anthropic Agent SDK provider.
//!
//! When thClaws runs Claude Code as a subprocess (`agent/*` model),
//! the model lives inside Claude Code and Claude Code's tool registry
//! is what the model can call — thClaws's own tools never reach it.
//! Per `ch06` of the user manual, that left KMS / Memory / MCP /
//! Agent Teams tools unreachable.
//!
//! The fix: connect an MCP server to the subprocess that wraps
//! thClaws's `ToolRegistry`. Claude Code surfaces those tools as
//! `mcp__thclaws__<name>` and routes calls back to the parent
//! (thClaws) over the Agent SDK control protocol. The parent runs
//! the tool in-process — same sandbox, same hooks, same on-disk
//! state — and writes the result back.
//!
//! ## Protocol
//!
//! Mirrors the `mcp_message` control_request shape the
//! `claude-agent-sdk-python` SDK implements (see
//! `claude-agent-sdk-python/src/claude_agent_sdk/_internal/query.py
//! :_handle_sdk_mcp_request`). Three JSON-RPC methods:
//!
//! - `initialize` → returns protocolVersion + serverInfo.
//! - `tools/list` → returns the bridged tool list.
//! - `tools/call` → dispatches to the named tool, wraps the result
//!   in `{ content: [{ type: "text", text: "..." }] }`.
//!
//! ## Tool filter
//!
//! Some tools depend on parent-process state that doesn't make sense
//! from inside Claude Code (the recursive Task spawner, Team* tools
//! that mutate `.thclaws/state/team/` for parallel teammates, the Skill
//! invoker that rewrites the next turn, plan-mode transition tools,
//! `AskUserQuestion` that needs a GUI). These are excluded from the
//! bridge — the model only sees tools that operate cleanly via the
//! shared on-disk state.

use serde_json::{json, Value};
use std::sync::Arc;

use crate::tools::ToolRegistry;

pub const SERVER_NAME: &str = "thclaws";
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Tools we deliberately don't expose to the Claude Code subprocess.
/// See module docstring for rationale.
const EXCLUDED_TOOLS: &[&str] = &[
    "Task",
    "TeamCreate",
    "SpawnTeammate",
    "SendMessage",
    "CheckInbox",
    "TeamStatus",
    "TeamTaskCreate",
    "TeamTaskList",
    "TeamTaskClaim",
    "TeamTaskComplete",
    "TeamMerge",
    "Skill",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "SubmitPlan",
    "UpdatePlanStep",
    // Goal-state mutation. `UpdateGoal` was split into these three in
    // Phase C1; the old name sat here matching nothing, which silently
    // bridged all three to the subprocess. Guarded by
    // `every_excluded_tool_name_exists` below.
    "RecordGoalProgress",
    "MarkGoalComplete",
    "MarkGoalBlocked",
];

/// Names the bridge exposes to the subprocess, sorted for stable
/// `--allowedTools` flag content. Each is the raw thClaws tool name
/// — Claude Code prefixes them with `mcp__thclaws__<name>` when the
/// model sees them.
pub fn bridged_tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|n| !EXCLUDED_TOOLS.contains(n) && crate::policy::tool_allowed(n))
        .map(String::from)
        .collect();
    names.sort();
    names
}

/// `mcp__thclaws__<tool>` names — what Claude Code's `--allowedTools`
/// flag expects to allowlist the bridged tools (and nothing else).
pub fn allowed_tool_patterns(registry: &ToolRegistry) -> Vec<String> {
    bridged_tool_names(registry)
        .into_iter()
        .map(|n| format!("mcp__{SERVER_NAME}__{n}"))
        .collect()
}

/// `--mcp-config` JSON value. Pass as the CLI flag value:
/// `--mcp-config <stringified-json>`.
pub fn mcp_config_value() -> Value {
    json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "sdk",
                "name": SERVER_NAME,
            }
        }
    })
}

/// Dispatch one JSON-RPC message from Claude Code. Returns the
/// matching JSON-RPC response (`{ jsonrpc, id, result }` or
/// `{ jsonrpc, id, error }`). The caller wraps this in the outer
/// `control_response { mcp_response: ... }` envelope.
pub async fn handle_mcp_message(registry: Arc<ToolRegistry>, message: &Value) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": crate::version::VERSION,
                },
            },
        }),
        "tools/list" => {
            let mut tools_data: Vec<Value> = Vec::new();
            for name in bridged_tool_names(&registry) {
                if let Some(tool) = registry.get(&name) {
                    tools_data.push(json!({
                        "name": tool.name(),
                        "description": tool.description(),
                        "inputSchema": tool.input_schema(),
                    }));
                }
            }
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools_data },
            })
        }
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            if EXCLUDED_TOOLS.contains(&name) {
                return jsonrpc_error(id, -32601, &format!("tool '{name}' is not bridged"));
            }
            if !crate::policy::tool_allowed(name) {
                return jsonrpc_error(id, -32601, &crate::policy::tool_denied_message(name));
            }
            let Some(tool) = registry.get(name) else {
                return jsonrpc_error(id, -32601, &format!("unknown tool '{name}'"));
            };
            match tool.call(arguments).await {
                Ok(text) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "isError": false,
                    },
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!("error: {e}") }],
                        "isError": true,
                    },
                }),
            }
        }
        // notifications/initialized arrives once after the SDK
        // `initialize` handshake. No response is expected — return a
        // bare success envelope so the caller stays happy.
        "notifications/initialized" => {
            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
        }
        _ => jsonrpc_error(id, -32601, &format!("unsupported method '{method}'")),
    }
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn registry() -> Arc<ToolRegistry> {
        let mut r = ToolRegistry::with_builtins();
        r.register(Arc::new(crate::tools::KmsReadTool));
        r.register(Arc::new(crate::tools::KmsSearchTool));
        r.register(Arc::new(crate::tools::KmsWriteTool));
        r.register(Arc::new(crate::tools::KmsWriteSourceTool));
        r.register(Arc::new(crate::tools::KmsAppendTool));
        r.register(Arc::new(crate::tools::KmsEditTool));
        r.register(Arc::new(crate::tools::KmsDeleteTool));
        r.register(Arc::new(crate::tools::KmsCreateTool));
        Arc::new(r)
    }

    /// A name in `EXCLUDED_TOOLS` that matches no registered tool
    /// excludes nothing. That is how `UpdateGoal` came to be bridged:
    /// Phase C1 split it into three tools and left the old name here,
    /// where it read as a still-active exclusion. Renaming a tool must
    /// fail this test rather than silently widen the bridge.
    #[test]
    fn every_excluded_tool_name_exists() {
        let mut r = ToolRegistry::with_builtins();
        let _mailbox = crate::team::register_team_tools(&mut r, "lead");
        r.register(Arc::new(crate::tools::RecordGoalProgressTool));
        r.register(Arc::new(crate::tools::MarkGoalCompleteTool));
        r.register(Arc::new(crate::tools::MarkGoalBlockedTool));
        let mut known: Vec<&str> = r.names();
        // Registered outside any registry we can build here: `Task` comes
        // from the REPL's agent factory and `Skill` from the skill system
        // (`skills.rs`). They are real tools, so name them explicitly
        // rather than letting the test read them as dead entries.
        known.extend(["Task", "Skill"]);
        let dead: Vec<&&str> = EXCLUDED_TOOLS
            .iter()
            .filter(|e| !known.contains(e))
            .collect();
        assert!(
            dead.is_empty(),
            "EXCLUDED_TOOLS names no longer in the registry (they exclude nothing): {dead:?}"
        );
    }

    #[test]
    fn goal_tools_are_not_bridged_to_the_subprocess() {
        let mut r = ToolRegistry::with_builtins();
        r.register(Arc::new(crate::tools::RecordGoalProgressTool));
        r.register(Arc::new(crate::tools::MarkGoalCompleteTool));
        r.register(Arc::new(crate::tools::MarkGoalBlockedTool));
        let names = bridged_tool_names(&Arc::new(r));
        for t in ["RecordGoalProgress", "MarkGoalComplete", "MarkGoalBlocked"] {
            assert!(!names.iter().any(|n| n == t), "{t} must not be bridged");
        }
    }

    #[test]
    fn bridged_tool_names_excludes_parent_state_tools() {
        let r = registry();
        let names = bridged_tool_names(&r);
        assert!(names.iter().any(|n| n == "KmsWrite"));
        assert!(names.iter().any(|n| n == "Read"));
        assert!(!names.iter().any(|n| n == "Task"));
        assert!(!names.iter().any(|n| n == "AskUserQuestion"));
        assert!(!names.iter().any(|n| n == "EnterPlanMode"));
    }

    #[test]
    fn allowed_tool_patterns_prefixes_mcp_server_name() {
        let r = registry();
        let patterns = allowed_tool_patterns(&r);
        assert!(patterns.iter().any(|p| p == "mcp__thclaws__KmsWrite"));
        assert!(patterns.iter().all(|p| p.starts_with("mcp__thclaws__")));
    }

    #[test]
    fn mcp_config_value_shape_matches_sdk_contract() {
        let v = mcp_config_value();
        assert_eq!(v["mcpServers"]["thclaws"]["type"], "sdk");
        assert_eq!(v["mcpServers"]["thclaws"]["name"], "thclaws");
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version_and_server_info() {
        let r = registry();
        let resp = handle_mcp_message(
            r,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[tokio::test]
    async fn tools_list_returns_bridged_tools_with_schemas() {
        let r = registry();
        let resp =
            handle_mcp_message(r, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        let tools = resp["result"]["tools"].as_array().expect("array");
        assert!(!tools.is_empty());
        // Spot-check: KmsWrite is present with an inputSchema.
        let kms_write = tools
            .iter()
            .find(|t| t["name"] == "KmsWrite")
            .expect("KmsWrite in list");
        assert!(kms_write["inputSchema"].is_object());
        assert!(kms_write["description"].is_string());
        // And Task isn't bridged.
        assert!(tools.iter().all(|t| t["name"] != "Task"));
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_error() {
        let r = registry();
        let resp = handle_mcp_message(
            r,
            &json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"NotARealTool","arguments":{}}
            }),
        )
        .await;
        assert!(resp.get("error").is_some());
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_call_excluded_tool_is_refused() {
        let r = registry();
        let resp = handle_mcp_message(
            r,
            &json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params":{"name":"Task","arguments":{"prompt":"x"}}
            }),
        )
        .await;
        // Task is in the registry but excluded from the bridge.
        assert!(resp.get("error").is_some());
    }

    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error() {
        let r = registry();
        let resp = handle_mcp_message(
            r,
            &json!({"jsonrpc":"2.0","id":5,"method":"resources/list"}),
        )
        .await;
        assert!(resp.get("error").is_some());
        assert_eq!(resp["error"]["code"], -32601);
    }
}
