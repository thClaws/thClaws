import { useEffect, useState } from "react";
import { send, subscribe, viewedSessionId } from "../hooks/useIPC";

/**
 * Identity of the agent that fired this approval request. Tagged at
 * the backend so the modal can disambiguate concurrent permission
 * asks from main + side-channel agents. Mirrors the Rust
 * `permissions::AgentOrigin` enum (serde tagged `kind`).
 */
type AgentOrigin =
  | { kind: "main" }
  | { kind: "side_channel"; id: string; agent_name: string }
  | { kind: "subagent"; agent_name: string; depth: number };

type PendingRequest = {
  id: number;
  tool_name: string;
  input: unknown;
  summary: string | null;
  originator: AgentOrigin;
};

function originLabel(o: AgentOrigin): string {
  switch (o.kind) {
    case "main":
      return "Main";
    case "side_channel":
      return `${o.agent_name} (background)`;
    case "subagent":
      return `${o.agent_name} (subagent · depth ${o.depth})`;
  }
}

function originAccent(o: AgentOrigin): string {
  // Visual distinction so concurrent requests don't all look the same.
  // Main = accent (default); side-channel = warning amber; subagent
  // = secondary blue.
  switch (o.kind) {
    case "main":
      return "var(--accent)";
    case "side_channel":
      return "#d97706";
    case "subagent":
      return "#2563eb";
  }
}

type Decision = "allow" | "allow_for_session" | "deny";

function formatValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function summarizeInput(input: unknown): string {
  if (input === null || input === undefined) return "";
  if (typeof input === "string") return input;
  // Render a flat object as multi-line "Key: value" pairs (one per
  // line) instead of raw JSON — easier to scan in the approval box.
  // Multi-line string values (e.g. an Edit's old/new text) drop to an
  // indented block; nested objects/arrays fall back to pretty JSON.
  if (typeof input === "object" && !Array.isArray(input)) {
    const entries = Object.entries(input as Record<string, unknown>);
    if (entries.length === 0) return "";
    return entries
      .map(([key, value]) => {
        const rendered = formatValue(value);
        if (rendered.includes("\n")) {
          const indented = rendered
            .split("\n")
            .map((line) => "  " + line)
            .join("\n");
          return `${key}:\n${indented}`;
        }
        return `${key}: ${rendered}`;
      })
      .join("\n");
  }
  try {
    return JSON.stringify(input, null, 2);
  } catch {
    return String(input);
  }
}

export function ApprovalModal() {
  const [queue, setQueue] = useState<(PendingRequest & { sessionId?: string })[]>([]);
  const [attention, setAttention] = useState<string[]>([]);
  const [viewed, setViewed] = useState(viewedSessionId());

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "session_attention") setAttention(msg.sessions as string[]);
      if (msg.type === "session_requests_cleared") setQueue(prev => prev.filter(r => r.sessionId !== msg.session_id));
      if (msg.type === "session_view_state") setViewed(msg.session_id as string | null);
      if (msg.type === "chat_done") setQueue((prev) => prev.filter((r) => r.sessionId !== viewedSessionId()));
      if (msg.type === "approval_request" && typeof msg.id === "number") {
        const newId = msg.id as number;
        setQueue((prev) => {
          // Backend re-dispatches every 1 s until we respond; skip
          // if we already have this id in the queue so the modal
          // doesn't spawn duplicates after a webview reload or race.
          if (prev.some((r) => r.id === newId)) return prev;
          // `originator` tagged on every request so the modal can
          // render which agent is asking when concurrent agents
          // request permissions. Default to `main` for back-compat
          // with backends that don't yet emit the field.
          const originator = (msg.originator as AgentOrigin) ?? { kind: "main" };
          return [
            ...prev,
            {
              id: newId,
              sessionId: typeof msg.session_id === "string" ? msg.session_id : undefined,
              tool_name: (msg.tool_name as string) ?? "?",
              input: msg.input,
              summary: (msg.summary as string | null) ?? null,
              originator,
            },
          ];
        });
      }
    });
    return unsub;
  }, []);

  const current = queue.find((r) => !r.sessionId || r.sessionId === viewed);
  if (!current) return attention.length ? (
    <div role="status" className="fixed bottom-4 right-4 z-[60] rounded-lg border p-3 shadow-lg"
      style={{ background: "var(--bg-primary)", color: "var(--text-primary)", borderColor: "var(--accent)" }}>
      <p className="text-sm">A background session needs your approval or answer.</p>
      {attention.map(id => <button key={id} className="block mt-2 text-sm underline"
        onClick={() => send({ type: "session_load", id })}>Return to session {id}</button>)}
    </div>
  ) : null;

  const respond = (decision: Decision) => {
    send({ type: "approval_response", id: current.id, decision });
    setQueue((prev) => prev.filter((r) => r.id !== current.id));
  };

  const preview = current.summary ?? summarizeInput(current.input);
  // MCP server spawn already persists the decision to the user-level
  // allowlist (~/.config/thclaws/mcp_allowlist.json) on Allow, so the
  // session-scoped option adds nothing. Hide it there.
  const showAllowForSession = current.tool_name !== "MCP server spawn";

  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center"
      style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
    >
      <div
        className="rounded-lg border shadow-xl w-[520px] max-w-[90vw]"
        style={{
          background: "var(--bg-primary)",
          borderColor: "var(--border)",
          color: "var(--text-primary)",
        }}
      >
        <div
          className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <span style={{ color: originAccent(current.originator) }}>●</span>
          <span>{originLabel(current.originator)} wants to run</span>
          <code
            className="px-1.5 py-0.5 rounded text-xs font-mono"
            style={{ background: "var(--bg-secondary)" }}
          >
            {current.tool_name}
          </code>
        </div>
        <pre
          className="px-4 py-3 text-xs font-mono whitespace-pre-wrap break-all max-h-[40vh] overflow-auto"
          style={{
            background: "var(--bg-secondary)",
            color: "var(--text-primary)",
          }}
        >
          {preview || "(no preview)"}
        </pre>
        <div
          className="px-4 py-3 border-t flex items-center justify-end gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <button
            onClick={() => respond("deny")}
            className="text-xs px-3 py-1.5 rounded hover:bg-white/5"
            style={{ color: "var(--text-secondary)" }}
          >
            Deny
          </button>
          {showAllowForSession && (
            <button
              onClick={() => respond("allow_for_session")}
              className="text-xs px-3 py-1.5 rounded hover:bg-white/5"
              style={{ color: "var(--text-primary)" }}
              title="Allow this and every subsequent tool call in this session"
            >
              Allow for session
            </button>
          )}
          <button
            onClick={() => respond("allow")}
            className="text-xs px-3 py-1.5 rounded"
            style={{
              background: "var(--accent)",
              color: "var(--accent-fg, #ffffff)",
            }}
            autoFocus
          >
            Allow
          </button>
        </div>
        {queue.length > 1 && (
          <div
            className="px-4 py-1.5 text-[10px] border-t"
            style={{
              borderColor: "var(--border)",
              color: "var(--text-secondary)",
              background: "var(--bg-secondary)",
            }}
          >
            +{queue.length - 1} more pending
          </div>
        )}
      </div>
    </div>
  );
}
