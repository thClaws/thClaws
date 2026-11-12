import { useEffect, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

/// Agent payments panel (dev-plan/74). Shows who governs payments, what is
/// left on each mandate or granted budget, and the latest paid / refused
/// attempts from this workspace's ledger. Hidden until payments are on and
/// there is something to show. Refreshes after every tool result, since
/// that is when a payment can have happened.

type Cap = { cap: string; left: string } | null;

type MandateView = {
  id: string;
  source: "config" | "ee_policy" | "modal";
  asset: string;
  total: Cap;
  daily: Cap;
  monthly: Cap;
  per_tx: string | null;
  sources: string[] | null;
  live?: boolean;
};

type Entry = {
  ts: string;
  agent: string | null;
  amount: string;
  payee: string;
  source: string;
  testnet: boolean;
  decision: "mandate" | "approved_once" | "budget" | "denied";
  paid: boolean;
  fee?: string | null;
  error: string | null;
};

type Status = {
  enabled: boolean;
  governed_by: "settings" | "ee_policy" | "ee_refused";
  mandates: MandateView[];
  entries: Entry[];
};

const SOURCE_LABEL: Record<MandateView["source"], string> = {
  config: "settings",
  ee_policy: "company policy",
  modal: "granted budget",
};

function timeOf(ts: string): string {
  const d = new Date(ts);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function capLine(label: string, c: Cap, asset: string) {
  if (!c) return null;
  return (
    <div className="flex justify-between gap-2">
      <span style={{ color: "var(--text-secondary)" }}>{label}</span>
      <span>
        {c.left} / {c.cap} {asset}
      </span>
    </div>
  );
}

export function PaymentsSidebar() {
  const [status, setStatus] = useState<Status | null>(null);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "payments_status") {
        setStatus(msg as unknown as Status);
      } else if (msg.type === "chat_tool_result") {
        send({ type: "payments_status" });
      }
    });
    send({ type: "payments_status" });
    return unsub;
  }, []);

  if (!status || !status.enabled) return null;
  if (status.mandates.length === 0 && status.entries.length === 0) return null;

  return (
    <div
      className="flex flex-col shrink-0 border-l px-3 py-2 overflow-y-auto"
      style={{
        width: "260px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      <div
        className="text-[10px] uppercase tracking-wider flex items-center gap-2 mb-2"
        style={{ color: "var(--text-secondary)" }}
      >
        <span>💳 Payments</span>
        {status.mandates.some((m) => m.live) || status.entries.some((e) => !e.testnet) ? (
          <span
            className="px-1.5 py-px rounded font-semibold"
            style={{ fontSize: "9px", background: "#dc2626", color: "#fff" }}
          >
            LIVE · เงินจริง
          </span>
        ) : (
          <span
            className="px-1.5 py-px rounded"
            style={{ fontSize: "9px", background: "var(--warning, #d4a72c)", color: "#000" }}
          >
            TESTNET
          </span>
        )}
      </div>
      {status.governed_by === "ee_refused" && (
        <div className="text-xs mb-2" style={{ color: "var(--text-secondary)" }}>
          The company policy does not allow agent payments.
        </div>
      )}
      {status.mandates.map((m) => (
        <div
          key={m.id}
          className="text-xs mb-2 p-2 rounded"
          style={{ background: "var(--bg-tertiary)" }}
        >
          <div className="flex justify-between mb-1">
            <span className="font-medium truncate" title={m.id}>
              {m.id}
            </span>
            <span style={{ color: "var(--text-secondary)" }}>{SOURCE_LABEL[m.source]}</span>
          </div>
          {capLine("total", m.total, m.asset)}
          {capLine("today", m.daily, m.asset)}
          {capLine("this month", m.monthly, m.asset)}
          {m.per_tx && (
            <div className="flex justify-between gap-2">
              <span style={{ color: "var(--text-secondary)" }}>per payment ≤</span>
              <span>
                {m.per_tx} {m.asset}
              </span>
            </div>
          )}
          {m.sources && m.sources.length > 0 && (
            <div className="mt-1 truncate" style={{ color: "var(--text-secondary)" }} title={m.sources.join(", ")}>
              sources: {m.sources.join(", ")}
            </div>
          )}
        </div>
      ))}
      {status.entries.length > 0 && (
        <div
          className="text-[10px] uppercase tracking-wider mt-1 mb-1"
          style={{ color: "var(--text-secondary)" }}
        >
          Recent
        </div>
      )}
      {status.entries.map((e, i) => (
        <div key={i} className="text-xs py-1 border-b" style={{ borderColor: "var(--border)" }}>
          <div className="flex justify-between gap-2">
            <span>
              {e.paid ? "✅" : "⛔"} {e.amount}
              {e.fee && <span style={{ color: "var(--text-secondary)" }}> + {e.fee}</span>}
            </span>
            <span style={{ color: "var(--text-secondary)" }}>{timeOf(e.ts)}</span>
          </div>
          <div className="truncate" title={`${e.source} → ${e.payee}`}>
            {e.source}
            {e.agent && e.agent !== "main" ? ` · ${e.agent}` : ""}
          </div>
          {!e.paid && e.error && (
            <div className="truncate" style={{ color: "var(--text-secondary)" }} title={e.error}>
              {e.error}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
