import { useEffect, useRef, useState } from "react";
import { subscribe, send } from "../hooks/useIPC";
import { useTheme, type ResolvedTheme } from "../hooks/useTheme";

// Per-theme ANSI palettes. Dark-mode uses the familiar One Dark-ish
// colors; light-mode maps the same ANSI slots to darker variants that
// stay legible on a white background (32/green → #2a7a3a rather than
// #98c379 which disappears into the bg, etc.).
const ANSI_PALETTES: Record<ResolvedTheme, Record<number, string>> = {
  dark: {
    30: "#4d4d4d", 31: "#e06c75", 32: "#98c379", 33: "#e5c07b",
    34: "#61afef", 35: "#c678dd", 36: "#56b6c2", 37: "#e6e6e6",
    90: "#888", 91: "#e06c75", 92: "#98c379", 93: "#e5c07b",
    94: "#61afef", 95: "#c678dd", 96: "#56b6c2", 97: "#ffffff",
  },
  light: {
    30: "#1a1a1a", 31: "#b22a32", 32: "#2a7a3a", 33: "#a06800",
    34: "#1f5dc0", 35: "#8b2bb2", 36: "#1f7a87", 37: "#1a1a1a",
    90: "#7a7a7a", 91: "#b22a32", 92: "#2a7a3a", 93: "#a06800",
    94: "#1f5dc0", 95: "#8b2bb2", 96: "#1f7a87", 97: "#000000",
  },
};

// Collapse runs of `[tool: X] ✓` for the same tool name into one line with
// `×N`. Otherwise long sweeps of Ls/Read calls drown out the signal in the
// pane. Blank lines between identical calls are swallowed so the collapsed
// line sits flush with its neighbours. Errors (`✗`) are never collapsed —
// they're worth seeing individually.
function collapseToolRuns(lines: string[]): string[] {
  // eslint-disable-next-line no-control-regex
  const stripAnsi = (s: string) => s.replace(/\x1b\[[0-9;]*m/g, "");
  const okRe = /\[tool:\s*(\w+)\][^\n]*?✓/;

  const out: string[] = [];
  let group: { name: string; idx: number; count: number } | null = null;

  const flush = () => {
    if (group && group.count > 1) {
      out[group.idx] =
        out[group.idx].replace(/\s×\d+\s*$/, "") + ` ×${group.count}`;
    }
    group = null;
  };

  for (const line of lines) {
    const bare = stripAnsi(line);
    const m = bare.match(okRe);
    if (m) {
      const name = m[1];
      if (group && group.name === name) {
        group.count++;
        continue;
      }
      flush();
      out.push(line);
      group = { name, idx: out.length - 1, count: 1 };
    } else if (bare.trim() === "" && group) {
      continue;
    } else {
      flush();
      out.push(line);
    }
  }
  flush();
  return out;
}

function ansiToHtml(text: string, palette: Record<number, string>): string {
  // Escape HTML entities first.
  const escaped = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");

  // Parse ANSI escape sequences into balanced <span> tags.
  let out = "";
  let open = 0;
  // eslint-disable-next-line no-control-regex
  const re = /\x1b\[([0-9;]*)m/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(escaped)) !== null) {
    out += escaped.slice(last, m.index);
    last = m.index + m[0].length;
    const codes = m[1];
    if (!codes || codes === "0") {
      if (open > 0) { out += "</span>".repeat(open); open = 0; }
      continue;
    }
    const parts = codes.split(";").map(Number);
    const styles: string[] = [];
    for (const code of parts) {
      if (code === 1) styles.push("font-weight:bold");
      else if (code === 2 || code === 90) styles.push("opacity:0.6");
      else if (palette[code]) styles.push(`color:${palette[code]}`);
    }
    if (styles.length === 0) continue;
    // Close any prior span so styles don't stack unexpectedly.
    if (open > 0) { out += "</span>".repeat(open); open = 0; }
    out += `<span style="${styles.join(";")}">`;
    open = 1;
  }
  out += escaped.slice(last);
  if (open > 0) out += "</span>".repeat(open);
  return out;
}

interface AgentInfo {
  name: string;
  status: string;
  task: string | null;
  output: string[];
}

export function TeamView() {
  const [agents, setAgents] = useState<AgentInfo[]>([]);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "team_status" && Array.isArray(msg.agents)) {
        setAgents(
          msg.agents.map((a: Record<string, unknown>): AgentInfo => ({
            name: String(a.name || a.agent || "?"),
            status: String(a.status || "unknown"),
            task: a.task ? String(a.task) : a.current_task ? String(a.current_task) : null,
            output: Array.isArray(a.output) ? a.output as string[] : [],
          }))
        );
      } else if (
        msg.type === "team_agent_output" &&
        typeof msg.agent === "string" &&
        typeof msg.line === "string"
      ) {
        setAgents((prev) =>
          prev.map((a) =>
            a.name === msg.agent
              ? { ...a, output: [...a.output.slice(-200), msg.line as string] }
              : a
          )
        );
      }
    });

    send({ type: "team_list" });
    const interval = setInterval(() => send({ type: "team_list" }), 3000);

    return () => {
      unsub();
      clearInterval(interval);
    };
  }, []);

  if (agents.length === 0) {
    return (
      <div
        className="flex items-center justify-center h-full"
        style={{ color: "var(--text-secondary)" }}
      >
        <div className="text-center">
          <p className="text-sm">No team agents running</p>
          <p className="text-xs mt-2">
            Ask the agent to create a team — teammates will appear here
          </p>
        </div>
      </div>
    );
  }

  const cols = agents.length <= 1 ? 1 : agents.length <= 4 ? 2 : 3;

  return (
    <div
      className="h-full w-full grid gap-px overflow-hidden"
      style={{
        gridTemplateColumns: `repeat(${cols}, 1fr)`,
        gridTemplateRows: `repeat(${Math.ceil(agents.length / cols)}, 1fr)`,
        background: "var(--border)",
      }}
    >
      {agents.map((agent) => (
        <AgentPane key={agent.name} agent={agent} />
      ))}
    </div>
  );
}

function AgentPane({ agent }: { agent: AgentInfo }) {
  const [input, setInput] = useState("");
  const endRef = useRef<HTMLDivElement>(null);
  const { resolved: themeMode } = useTheme();
  const palette = ANSI_PALETTES[themeMode];

  const lastLine = agent.output[agent.output.length - 1] ?? "";
  useEffect(() => {
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [agent.output.length, lastLine]);

  function handleSend() {
    const text = input.trim();
    if (!text) return;
    send({ type: "team_send_message", to: agent.name, text });
    setInput("");
  }

  const statusColor =
    agent.status === "working"
      ? "var(--warning)"
      : agent.status === "idle"
      ? "var(--text-secondary)"
      : "var(--accent)";

  return (
    <div
      className="flex flex-col min-h-0"
      style={{ background: "var(--terminal-bg)" }}
    >
      {/* Header */}
      <div
        className="flex items-center justify-between px-2 py-1 text-[10px] font-medium shrink-0 select-none"
        style={{
          background: "var(--bg-secondary)",
          borderBottom: "1px solid var(--border)",
        }}
      >
        <span style={{ color: "var(--accent)" }}>{agent.name}</span>
        <span style={{ color: statusColor }}>
          {agent.status}
          {agent.task ? ` · ${agent.task}` : ""}
        </span>
      </div>

      {/* Output */}
      <div
        className="flex-1 min-h-0 overflow-y-auto p-1.5 font-mono text-[11px] leading-tight"
        style={{ color: "var(--terminal-fg)" }}
      >
        {agent.output.length > 0 ? (
          <div
            className="whitespace-pre-wrap break-all"
            dangerouslySetInnerHTML={{
              __html: ansiToHtml(
                collapseToolRuns(agent.output).join("\n"),
                palette,
              ),
            }}
          />
        ) : (
          <span style={{ color: "var(--text-secondary)" }}>
            waiting for messages...
          </span>
        )}
        <div ref={endRef} />
      </div>

      {/* Input */}
      {(
        <div
          className="shrink-0 flex gap-1 p-1"
          style={{ borderTop: "1px solid var(--border)" }}
        >
          <input
            type="text"
            className="flex-1 px-1.5 py-0.5 rounded text-[11px] font-mono outline-none"
            style={{
              background: "var(--bg-tertiary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
            placeholder={`Message ${agent.name}...`}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleSend();
            }}
          />
          <button
            className="px-2 py-0.5 rounded text-[10px] font-medium"
            style={{
              background: "var(--accent-dim)",
              color: "var(--accent-fg)",
            }}
            onClick={handleSend}
          >
            Send
          </button>
        </div>
      )}
    </div>
  );
}
