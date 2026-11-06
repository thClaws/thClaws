import { useEffect, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

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

/** dev-plan/74: a payment prompt. Amounts are decimal strings. */
type PaymentInfo = {
  amount: string | null;
  asset: string;
  payee: string;
  payee_name: string | null;
  network: string;
  testnet: boolean;
  purpose: string | null;
  reason: string;
};

type PendingRequest = {
  id: number;
  tool_name: string;
  input: unknown;
  summary: string | null;
  originator: AgentOrigin;
  payment: PaymentInfo | null;
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
  const [queue, setQueue] = useState<PendingRequest[]>([]);

  useEffect(() => {
    const unsub = subscribe((msg) => {
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
              tool_name: (msg.tool_name as string) ?? "?",
              input: msg.input,
              summary: (msg.summary as string | null) ?? null,
              originator,
              payment: (msg.payment as PaymentInfo | undefined) ?? null,
            },
          ];
        });
      }
    });
    return unsub;
  }, []);

  const current = queue[0];
  if (!current) return null;
  if (current.payment) {
    return (
      <PaymentPrompt
        key={current.id}
        req={current}
        payment={current.payment}
        more={queue.length - 1}
        onDone={() => setQueue((prev) => prev.slice(1))}
      />
    );
  }

  const respond = (decision: Decision) => {
    send({ type: "approval_response", id: current.id, decision });
    setQueue((prev) => prev.slice(1));
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

function PaymentPrompt({
  req,
  payment,
  more,
  onDone,
}: {
  req: PendingRequest;
  payment: PaymentInfo;
  more: number;
  onDone: () => void;
}) {
  const [budgetOpen, setBudgetOpen] = useState(false);
  const [total, setTotal] = useState("");
  const [perTx, setPerTx] = useState("");
  const valid = /^\d+(\.\d+)?$/;
  const totalOk = valid.test(total.trim()) && Number(total) > 0;
  const perTxOk = perTx.trim() === "" || valid.test(perTx.trim());
  const coversThis =
    payment.amount === null || !totalOk || Number(total) >= Number(payment.amount);

  const respond = (decision: "once" | "budget" | "deny") => {
    send({
      type: "approval_response",
      id: req.id,
      decision,
      ...(decision === "budget"
        ? { budget: { total: total.trim(), per_tx: perTx.trim() || undefined } }
        : {}),
    });
    onDone();
  };

  const amountText =
    payment.amount === null ? `unknown amount of ${payment.asset}` : `${payment.amount} ${payment.asset}`;
  const row = "flex gap-3 text-xs";
  const label = "w-20 shrink-0";

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
          <span>💳</span>
          <span>{originLabel(req.originator)} wants to pay</span>
          <code
            className="px-1.5 py-0.5 rounded text-xs font-mono"
            style={{ background: "var(--bg-secondary)" }}
          >
            {req.tool_name}
          </code>
          {payment.testnet ? (
            <span
              className="ml-auto text-[10px] px-1.5 py-0.5 rounded font-semibold"
              style={{ background: "#d97706", color: "#ffffff" }}
            >
              TESTNET
            </span>
          ) : (
            <span
              className="ml-auto text-[10px] px-1.5 py-0.5 rounded font-semibold"
              style={{ background: "#dc2626", color: "#ffffff" }}
            >
              LIVE · เงินจริง
            </span>
          )}
        </div>
        <div className="px-4 py-3 flex flex-col gap-1.5" style={{ background: "var(--bg-secondary)" }}>
          <div className="text-xl font-semibold">{amountText}</div>
          <div className={row}>
            <span className={label} style={{ color: "var(--text-secondary)" }}>Payee</span>
            <span className="break-all">
              {payment.payee_name ? `${payment.payee_name} · ` : ""}
              <span className="font-mono">{payment.payee}</span>
            </span>
          </div>
          {payment.purpose && (
            <div className={row}>
              <span className={label} style={{ color: "var(--text-secondary)" }}>For</span>
              <span className="break-all">{payment.purpose}</span>
            </div>
          )}
          <div className={row}>
            <span className={label} style={{ color: "var(--text-secondary)" }}>Network</span>
            <span>{payment.network}</span>
          </div>
          <div className="text-[11px] mt-1" style={{ color: "var(--text-secondary)" }}>
            Asking because {payment.reason}.
          </div>
        </div>
        {budgetOpen && (
          <div
            className="px-4 py-3 border-t flex flex-col gap-2 text-xs"
            style={{ borderColor: "var(--border)" }}
          >
            <div style={{ color: "var(--text-secondary)" }}>
              Payments in {payment.asset} on {payment.network} run without asking until the budget is
              used up. It lasts for this session only.
            </div>
            <label className="flex items-center gap-2">
              <span className="w-36">Total budget ({payment.asset})</span>
              <input
                autoFocus
                value={total}
                onChange={(e) => setTotal(e.target.value)}
                inputMode="decimal"
                className="flex-1 px-2 py-1 rounded border bg-transparent"
                style={{ borderColor: "var(--border)" }}
              />
            </label>
            <label className="flex items-center gap-2">
              <span className="w-36">Max per payment (optional)</span>
              <input
                value={perTx}
                onChange={(e) => setPerTx(e.target.value)}
                inputMode="decimal"
                className="flex-1 px-2 py-1 rounded border bg-transparent"
                style={{ borderColor: "var(--border)" }}
              />
            </label>
            {!coversThis && (
              <div style={{ color: "#dc2626" }}>The budget is smaller than this payment.</div>
            )}
          </div>
        )}
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
          {budgetOpen ? (
            <button
              disabled={!totalOk || !perTxOk || !coversThis}
              onClick={() => respond("budget")}
              className="text-xs px-3 py-1.5 rounded disabled:opacity-40"
              style={{ background: "var(--accent)", color: "var(--accent-fg, #ffffff)" }}
            >
              Allow within budget
            </button>
          ) : (
            payment.amount !== null && (
              <button
                onClick={() => setBudgetOpen(true)}
                className="text-xs px-3 py-1.5 rounded hover:bg-white/5"
                style={{ color: "var(--text-primary)" }}
                title="Approve a total budget so the next payments in this session run without asking"
              >
                Allow within a budget…
              </button>
            )
          )}
          {!budgetOpen && (
            <button
              onClick={() => respond("once")}
              className="text-xs px-3 py-1.5 rounded"
              style={{ background: "var(--accent)", color: "var(--accent-fg, #ffffff)" }}
              autoFocus
            >
              Approve once
            </button>
          )}
        </div>
        {more > 0 && (
          <div
            className="px-4 py-1.5 text-[10px] border-t"
            style={{
              borderColor: "var(--border)",
              color: "var(--text-secondary)",
              background: "var(--bg-secondary)",
            }}
          >
            +{more} more pending
          </div>
        )}
      </div>
    </div>
  );
}
