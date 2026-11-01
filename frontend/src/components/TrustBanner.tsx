import { useEffect, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

type Item = { key: string; detail?: string };
type Withheld = { changed: boolean; items: Item[] };

const LABELS: Record<string, string> = {
  hooks: "hooks (คำสั่งที่รันเองอัตโนมัติ)",
  permissions: "โหมดสิทธิ์ของโฟลเดอร์ (เช่น อนุมัติอัตโนมัติ)",
  "bash.sandbox": "ผ่อน Bash sandbox ลง",
  "bash.sandbox_write_paths": "พาธนอกโฟลเดอร์ที่ Bash เขียนได้",
  remoteAgentUrl: "Remote agent URL",
  model: "โมเดล agent/* (Claude Code)",
  skill_model_fallback: "โมเดล agent/* สำหรับ skill",
  translator_subagent_model: "โมเดล agent/* สำหรับ translator",
  extract_save_skill_models: "โมเดล agent/* สำหรับบันทึก skill",
  "guiShell.tabDefault": "เปิด GUI shell เป็นแท็บเริ่มต้น",
  mcpServers: "MCP servers จาก .mcp.json",
  "bridge.line": "เชื่อมต่อ LINE อัตโนมัติ",
  "bridge.telegram": "เชื่อมต่อ Telegram อัตโนมัติ",
  "bridge.messenger": "เชื่อมต่อ Messenger อัตโนมัติ",
  "bridge.phoneHome": "เชื่อมต่อ thClaws Remote อัตโนมัติ",
  "guiShell.permissions": "สิทธิ์ของ GUI shell",
  env: "ค่าใน .env ที่ไม่ใช่ API key",
};

const label = (i: Item) => {
  const base = LABELS[i.key] ?? i.key;
  return i.detail ? `${base}: ${i.detail}` : base;
};

// Per app session, keyed by folder and state, so a later edit to a folder
// the user waved off still asks again.
const dismissed = new Set<string>();

/**
 * Asks to trust the workspace when its project settings hold something
 * `config::trust` is withholding (hooks, MCP servers, chat bindings, a
 * gui-shell's permissions, …). Non-modal: the app works without it.
 */
export function TrustBanner() {
  const [path, setPath] = useState("");
  const [withheld, setWithheld] = useState<Withheld | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [, bump] = useState(0);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "workspace_trust_status") {
        setPath(typeof msg.path === "string" ? msg.path : "");
        setWithheld((msg.withheld as Withheld | null) ?? null);
      } else if (msg.type === "workspace_trust_result") {
        setBusy(false);
        if (msg.declined === true) return;
        if (msg.ok !== true) {
          setNote(`ไว้ใจโฟลเดอร์ไม่สำเร็จ: ${String(msg.error ?? "")}`);
          return;
        }
        const later = Array.isArray(msg.restart_needed)
          ? (msg.restart_needed as string[]).map((key) => LABELS[key] ?? key)
          : [];
        setNote(
          later.length
            ? `ไว้ใจโฟลเดอร์แล้ว — ${later.join(", ")} จะมีผลเมื่อเปิดแอปใหม่`
            : "ไว้ใจโฟลเดอร์แล้ว — การตั้งค่าของโฟลเดอร์มีผลแล้ว",
        );
        send({ type: "workspace_trust_status" });
      } else if (msg.type === "settings_changed") {
        send({ type: "workspace_trust_status" });
      }
    });
    send({ type: "workspace_trust_status" });
    return unsub;
  }, []);

  const key = `${path}\u0000${withheld?.changed ? "changed" : "untrusted"}`;

  if (note && !withheld) {
    return (
      <div
        role="status"
        className="flex items-center justify-center gap-3 px-3 py-1.5 text-xs"
        style={{
          background: "var(--bg-secondary)",
          color: "var(--text-primary)",
          borderBottom: "2px solid var(--accent, #2563eb)",
        }}
      >
        <span>{note}</span>
        <button
          type="button"
          className="rounded px-2 py-0.5 opacity-60 hover:opacity-100"
          aria-label="ปิด"
          onClick={() => setNote(null)}
        >
          ✕
        </button>
      </div>
    );
  }
  if (!withheld || withheld.items.length === 0 || dismissed.has(key)) return null;

  const accent = "var(--warning, #b45309)";
  return (
    <div
      role="alert"
      className="flex flex-col gap-1 px-3 py-2 text-xs"
      style={{
        background: "var(--bg-secondary)",
        color: "var(--text-primary)",
        borderBottom: `2px solid ${accent}`,
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">
          {withheld.changed
            ? "การตั้งค่าของโฟลเดอร์นี้เปลี่ยนไปหลังจากที่คุณไว้ใจ จึงถูกพักไว้ — ไว้ใจอีกครั้งไหม"
            : "โฟลเดอร์นี้มีการตั้งค่าที่รันโค้ดหรือให้สิทธิ์เพิ่มได้ ซึ่งยังไม่เปิดใช้ — ไว้ใจโฟลเดอร์นี้ไหม"}
        </span>
        <span className="truncate opacity-70" title={path}>
          {path}
        </span>
        <span className="ml-auto flex gap-2">
          <button
            type="button"
            disabled={busy}
            className="rounded px-2 py-0.5 font-medium"
            style={{ background: accent, color: "white" }}
            onClick={() => {
              // The desktop asks again in a native dialog the page's script
              // can't press. A browser has only this, which a same-origin
              // script could still answer — best effort.
              if (
                !window.ipc &&
                !window.confirm(
                  `ไว้ใจโฟลเดอร์ ${path}?\n\nจะเปิดใช้: ${withheld.items.map(label).join(", ")}`,
                )
              ) {
                return;
              }
              setBusy(true);
              setNote(null);
              send({ type: "workspace_trust" });
            }}
          >
            ไว้ใจ
          </button>
          <button
            type="button"
            className="rounded px-2 py-0.5 opacity-70 hover:opacity-100"
            onClick={() => {
              dismissed.add(key);
              bump((n) => n + 1);
            }}
          >
            ไม่ตอนนี้
          </button>
        </span>
      </div>
      <div className="opacity-80">
        ถ้าไว้ใจ จะเปิดใช้: {withheld.items.map(label).join(" · ")}
      </div>
      {note && <div style={{ color: accent }}>{note}</div>}
    </div>
  );
}
