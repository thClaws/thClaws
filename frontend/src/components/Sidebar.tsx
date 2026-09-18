import { useState, useEffect, useRef } from "react";
import { Plus } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { ModelPickerDropdown } from "./ModelPickerDropdown";
import { KmsCreateModal, type KmsCreateMode } from "./KmsCreateModal";
import { CtxMenuItem } from "./CtxMenuItem";

type SessionInfo = { id: string; model: string; messages: number; title?: string | null; owner_agent?: string | null };
function sessionLabel(session: SessionInfo): string {
  const owner = session.owner_agent?.trim();
  const title = session.title?.trim();
  if (!owner || owner === "lead") return `Lead · ${title || `Session ${session.id.slice(-6)}`}`;
  return title ? `${owner} · ${title}` : owner;
}

type KmsInfo = { name: string; scope: "user" | "project"; active: boolean };
type LineStatus = {
  state: "connected" | "disconnected";
  server_url: string;
  pending_approvals: number;
  /// LINE display name from the relay's `/pair` response. Shown
  /// next to the pill dot when present; falls back to "bridge live"
  /// when the relay didn't return one (older relay or LINE API
  /// fetch failure).
  display_name?: string;
  picture_url?: string;
};

/// dev-plan/29 Tier 1: Telegram bridge status pill state.
type TelegramStatus = {
  state: "connected" | "disconnected";
  bot_username: string | null;
  pending_approvals: number;
  pending_pairings: number;
  active_chats: number;
};

// Confirmation dialog with two backends. Mirrors `platformConfirm`
// in FilesView. Desktop (`wry` WebView in `--gui`): the IPC bridge
// is present, so round-trip through the Rust backend for a real
// native modal. `--serve` (web browser): no `window.ipc`, fall
// back to the browser's built-in `window.confirm()`.
function platformConfirm(opts: {
  title: string;
  message: string;
  yesLabel?: string;
  noLabel?: string;
}): Promise<boolean> {
  return new Promise((resolve) => {
    const inBrowser = typeof window !== "undefined" && !window.ipc;
    if (inBrowser) {
      resolve(window.confirm(`${opts.title}\n\n${opts.message}`));
      return;
    }
    const id =
      typeof crypto !== "undefined" && "randomUUID" in crypto
        ? crypto.randomUUID()
        : `cf-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    const unsub = subscribe((msg) => {
      if (msg.type === "confirm_result" && msg.id === id) {
        unsub();
        resolve(Boolean(msg.ok));
      }
    });
    send({
      type: "confirm",
      id,
      title: opts.title,
      message: opts.message,
      yes_label: opts.yesLabel ?? "OK",
      no_label: opts.noLabel ?? "Cancel",
    });
  });
}

/// M6.39.9: parent (App) tracks which KMS the user opened the
/// browser for. The sidebar fires `onBrowseKms(name)` when the
/// user clicks a KMS title (not the checkbox); App stores that in
/// state and renders `KmsBrowserSidebar` accordingly.
interface SidebarProps {
  onBrowseKms?: (name: string) => void;
}

const SIDEBAR_WIDTH_KEY = "thclaws_sidebar_width";
const SIDEBAR_WIDTH_MIN = 160;
const SIDEBAR_WIDTH_MAX = 480;
const SIDEBAR_WIDTH_DEFAULT = 192; // matches the original Tailwind `w-48`

export function Sidebar({ onBrowseKms }: SidebarProps = {}) {
  // Persisted, user-resizable width. Replaces the previous `w-48`
  // hard-cap because model/session titles longer than ~16 chars got
  // clipped (#150). Drag the 3px gutter on the right edge to resize;
  // double-click resets to default.
  const [sidebarWidth, setSidebarWidth] = useState<number>(() => {
    if (typeof window === "undefined") return SIDEBAR_WIDTH_DEFAULT;
    const raw = localStorage.getItem(SIDEBAR_WIDTH_KEY);
    const n = raw ? Number(raw) : NaN;
    if (!Number.isFinite(n) || n < SIDEBAR_WIDTH_MIN || n > SIDEBAR_WIDTH_MAX) {
      return SIDEBAR_WIDTH_DEFAULT;
    }
    return Math.round(n);
  });
  const [resizing, setResizing] = useState(false);
  useEffect(() => {
    if (!resizing) return;
    const onMove = (e: MouseEvent) => {
      // Width = pointer X relative to the viewport's left edge (the
      // sidebar starts there). Clamp + round so we don't write
      // sub-pixel values that fight CSS rounding.
      const w = Math.max(
        SIDEBAR_WIDTH_MIN,
        Math.min(SIDEBAR_WIDTH_MAX, Math.round(e.clientX)),
      );
      setSidebarWidth(w);
    };
    const onUp = () => setResizing(false);
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [resizing]);
  useEffect(() => {
    if (typeof window !== "undefined") {
      localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth));
    }
  }, [sidebarWidth]);

  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [currentSessionId, setCurrentSessionId] = useState<string>("");
  // Start blank, not a hardcoded default: the Sidebar unmounts in
  // full-screen and remounts with empty state, so a literal default here
  // would flash the wrong provider/model until config_poll corrects it.
  // We request config_poll on mount (below) to fill these immediately.
  const [activeProvider, setActiveProvider] = useState("");
  const [activeModel, setActiveModel] = useState("");
  const [providerReady, setProviderReady] = useState(true);
  // Inline model picker dropdown anchored to the Provider section.
  // null means closed; opens on click of the active model row. #49.
  const [modelPickerOpen, setModelPickerOpen] = useState(false);
  // Thinking level 0-3, null = auto (provider default). Mirrors
  // `/thinking`; the engine broadcasts `thinking_update` after either
  // path persists, so this never drifts from the shell.
  const [thinkingLevel, setThinkingLevel] = useState<number | null>(null);
  const [mcpServers, setMcpServers] = useState<
    { name: string; tools: number }[]
  >([]);
  const [kmss, setKmss] = useState<KmsInfo[]>([]);
  // KMS create modal (new KMS base). null = closed. Replaces the old
  // window.prompt() flow that silently failed inside the webview.
  const [kmsModal, setKmsModal] = useState<KmsCreateMode | null>(null);
  // OKF import/export context menu on the "Knowledge" section header,
  // anchored to cursor coords; null when closed.
  const [kmsMenu, setKmsMenu] = useState<{ x: number; y: number } | null>(null);
  // Per-row menu on a KMS entry: rename / export / delete.
  const [kmsRowMenu, setKmsRowMenu] = useState<{ kms: KmsInfo; x: number; y: number } | null>(null);
  const [kmsRenameTarget, setKmsRenameTarget] = useState<KmsInfo | null>(null);
  const kmsRenameInputRef = useRef<HTMLInputElement | null>(null);
  // OKF import modal (collects new KMS name + scope; the backend opens
  // the native folder picker on submit). null = closed.
  const [okfImport, setOkfImport] = useState<{ scope: "user" | "project" } | null>(null);
  const okfImportNameRef = useRef<HTMLInputElement | null>(null);
  // Transient status line under the Knowledge header for OKF results.
  const [okfMsg, setOkfMsg] = useState<{ ok: boolean; text: string } | null>(null);
  // Right-click context menu anchored to the session row the user
  // right-clicked; null when closed. Click anywhere else dismisses.
  const [sessionMenu, setSessionMenu] = useState<
    { session: SessionInfo; x: number; y: number } | null
  >(null);
  // Inline rename dialog. `sessionId === null` means closed.
  const [renameTarget, setRenameTarget] = useState<
    { id: string; current: string } | null
  >(null);
  const renameInputRef = useRef<HTMLInputElement | null>(null);
  // #95(b): when empty, the sidebar shows only the top 10 most-recent
  // sessions (matches the pre-fix layout the user is used to). When
  // typing, we filter the full received list (backend caps at 200, see
  // build_session_list) by title + id substring match, case-insensitive,
  // and uncap up to 50 matches so search is usable for named sessions
  // that fall outside the top-10 default view.
  const [sessionFilter, setSessionFilter] = useState("");
  // Plan-07 Phase 2.4: LINE bridge status pill. The worker
  // broadcasts `line_status` envelopes on connect / disconnect;
  // the pill is rendered only while `state === "connected"`.
  const [lineStatus, setLineStatus] = useState<LineStatus>({
    state: "disconnected",
    server_url: "",
    pending_approvals: 0,
  });
  const [telegramStatus, setTelegramStatus] = useState<TelegramStatus>({
    state: "disconnected",
    bot_username: null,
    pending_approvals: 0,
    pending_pairings: 0,
    active_chats: 0,
  });

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "new_session_ack") {
        // Chat UI handles clearing; sessions_list arrives separately.
      } else if (msg.type === "sessions_list") {
        if (msg.sessions) {
          setSessions(msg.sessions as SessionInfo[]);
        }
        // `current_id` is only present on refreshes from the worker
        // thread (load/save/new); main-thread refreshes (config_poll,
        // rename) omit it. Preserve the last-known value in that case.
        if (typeof msg.current_id === "string") {
          setCurrentSessionId(msg.current_id as string);
        }
      } else if (msg.type === "initial_state" || msg.type === "provider_update") {
        if (msg.provider) setActiveProvider(msg.provider as string);
        if (msg.model) setActiveModel(msg.model as string);
        if (msg.thinking && typeof msg.thinking === "object") {
          const lv = (msg.thinking as { level?: number | null }).level;
          setThinkingLevel(typeof lv === "number" ? lv : null);
        }
        if (typeof msg.provider_ready === "boolean") {
          setProviderReady(msg.provider_ready);
        }
        if (msg.mcp_servers) {
          setMcpServers(msg.mcp_servers as { name: string; tools: number }[]);
        }
        if (msg.sessions) {
          setSessions(msg.sessions as SessionInfo[]);
        }
        if (msg.kmss) {
          setKmss(msg.kmss as KmsInfo[]);
        }
      } else if (msg.type === "thinking_update") {
        const lv = (msg.thinking as { level?: number | null } | undefined)?.level;
        setThinkingLevel(typeof lv === "number" ? lv : null);
      } else if (msg.type === "mcp_update") {
        setMcpServers(msg.servers as { name: string; tools: number }[]);
      } else if (msg.type === "kms_update") {
        setKmss(msg.kmss as KmsInfo[]);
      } else if (msg.type === "kms_okf_result") {
        setOkfMsg({ ok: Boolean(msg.ok), text: String(msg.message ?? "") });
      } else if (msg.type === "line_status") {
        setLineStatus({
          state: (msg.state as LineStatus["state"]) ?? "disconnected",
          server_url: (msg.server_url as string) ?? "",
          pending_approvals: (msg.pending_approvals as number) ?? 0,
          display_name: (msg.display_name as string | undefined) ?? undefined,
          picture_url: (msg.picture_url as string | undefined) ?? undefined,
        });
      } else if (msg.type === "telegram_status") {
        setTelegramStatus({
          state: (msg.state as TelegramStatus["state"]) ?? "disconnected",
          bot_username: (msg.bot_username as string | null) ?? null,
          pending_approvals: (msg.pending_approvals as number) ?? 0,
          pending_pairings: (msg.pending_pairings as number) ?? 0,
          active_chats: (msg.active_chats as number) ?? 0,
        });
      }
    });
    // Ask for current LINE state once at mount. The backend replies
    // with a `line_status` envelope the subscriber above renders.
    // (SSO state is fetched by the navbar LoginButton.)
    send({ type: "line_status" });
    send({ type: "telegram_status" });
    // The Sidebar unmounts in fullscreen (gui-shell tabs like
    // book-studio) and remounts with empty state — `initial_state`'s
    // session snapshot is long gone by then, so the history list
    // rendered blank until some worker push refired sessions_list.
    // Ask for a fresh list on every mount.
    send({ type: "sessions_request" });
    // Same remount problem for the Provider/Model section: without this
    // it would show blank (or, pre-fix, a wrong hardcoded default) until
    // the periodic 5 s config_poll fires. Ask for it immediately.
    send({ type: "config_poll" });
    // And the KMS list: `initial_state.kmss` is a one-shot fired at WS
    // connect, but the Sidebar unmounts in a fullscreen gui-shell tab
    // (e.g. research-console pinned via guiShell.tabDefault) and remounts
    // after it, missing that snapshot — the KMS section then rendered
    // "None yet" even with an active KMS on disk. `kms_list` replies with
    // a `kms_update` the subscriber above renders, so it self-heals.
    send({ type: "kms_list" });
    return unsub;
  }, []);

  // Dismiss the context menu on any outside click or Escape — standard
  // popover behavior. The menu's own buttons call setSessionMenu(null)
  // before acting so they don't self-dismiss prematurely.
  useEffect(() => {
    if (!sessionMenu) return;
    const onClick = () => {
      setSessionMenu(null);
      setKmsRowMenu(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setSessionMenu(null);
        setKmsRowMenu(null);
      }
    };
    window.addEventListener("click", onClick);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("click", onClick);
      window.removeEventListener("keydown", onKey);
    };
  }, [sessionMenu]);

  // Same dismiss behaviour for the Knowledge-header OKF menu.
  useEffect(() => {
    if (!kmsMenu) return;
    const onClick = () => setKmsMenu(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setKmsMenu(null);
    };
    window.addEventListener("click", onClick);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("click", onClick);
      window.removeEventListener("keydown", onKey);
    };
  }, [kmsMenu]);

  // Auto-dismiss the OKF status line a few seconds after it lands.
  useEffect(() => {
    if (!okfMsg) return;
    const t = setTimeout(() => setOkfMsg(null), 6000);
    return () => clearTimeout(t);
  }, [okfMsg]);

  // Focus + select the import-name field when the modal opens.
  useEffect(() => {
    if (okfImport && okfImportNameRef.current) {
      okfImportNameRef.current.focus();
      okfImportNameRef.current.select();
    }
  }, [okfImport]);

  // Focus + select-all when the rename dialog opens so the user can
  // either replace the whole title or click to keep part of it.
  useEffect(() => {
    if (kmsRenameTarget && kmsRenameInputRef.current) {
      kmsRenameInputRef.current.focus();
      kmsRenameInputRef.current.select();
    }
    if (renameTarget && renameInputRef.current) {
      renameInputRef.current.focus();
      renameInputRef.current.select();
    }
  }, [renameTarget]);

  // Poll config every 5s to pick up model/provider changes from Terminal PTY.
  useEffect(() => {
    const interval = setInterval(() => send({ type: "config_poll" }), 5000);
    return () => clearInterval(interval);
  }, []);

  return (
    <div
      className="border-r shrink-0 text-xs select-none relative flex"
      style={{
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
        width: sidebarWidth,
        // Disable pointer events inside the sidebar while dragging so
        // mid-drag mouseover doesn't accidentally fire button hovers /
        // selection — feels noticeably crisper on a fast drag.
        cursor: resizing ? "col-resize" : undefined,
      }}
    >
      <div className="flex-1 min-w-0 overflow-y-auto">
      {/* Provider */}
      <Section title="Provider">
        <div className="px-2 py-1 relative">
          <button
            type="button"
            onClick={() => setModelPickerOpen((v) => !v)}
            className="w-full text-left rounded"
            style={{
              background: modelPickerOpen ? "var(--bg-tertiary)" : "transparent",
              border: "1px solid transparent",
              cursor: "pointer",
              padding: "2px 4px",
            }}
            onMouseEnter={(e) =>
              (e.currentTarget.style.background = "var(--bg-tertiary)")
            }
            onMouseLeave={(e) =>
              (e.currentTarget.style.background = modelPickerOpen
                ? "var(--bg-tertiary)"
                : "transparent")
            }
            title="Click to switch model"
          >
            <div className="flex items-center gap-1.5">
              <span
                className="w-1.5 h-1.5 rounded-full"
                style={{
                  background: providerReady
                    ? "var(--accent)"
                    : "var(--danger, #e06c75)",
                }}
              />
              <span
                style={{
                  color: providerReady
                    ? "var(--text-primary)"
                    : "var(--text-secondary)",
                  textDecoration: providerReady ? "none" : "line-through",
                }}
              >
                {activeProvider}
              </span>
              <span
                className="ml-auto"
                style={{
                  color: "var(--text-secondary)",
                  fontSize: "10px",
                  opacity: 0.7,
                }}
              >
                ▾
              </span>
            </div>
            <div
              className="ml-3 font-mono truncate"
              style={{ color: "var(--text-secondary)", fontSize: "10px" }}
            >
              {activeModel}
            </div>
          </button>
          {!providerReady && (
            <div
              className="ml-3 mt-1"
              style={{ color: "var(--danger, #e06c75)", fontSize: "10px" }}
            >
              no API key — set one in Settings
            </div>
          )}
          {modelPickerOpen && (
            <ModelPickerDropdown
              current={activeModel}
              onClose={() => setModelPickerOpen(false)}
            />
          )}
          <ThinkingSelector
            level={thinkingLevel}
            onChange={(lv) => {
              setThinkingLevel(lv);
              send({ type: "thinking_set", level: lv === null ? "auto" : lv });
            }}
          />
        </div>
      </Section>

      {/* LINE bridge pill — visible only when the worker reports the
          bridge is connected. Mirrors the LineConnectModal's source
          of truth (`line_status` envelope). Plan-07 Phase 2.4. */}
      {lineStatus.state === "connected" && (
        <Section title="LINE">
          <div
            className="px-2 py-1 flex items-center gap-1.5"
            title={`${lineStatus.display_name ? `${lineStatus.display_name} · ` : ""}${lineStatus.server_url}${lineStatus.pending_approvals > 0 ? ` · ${lineStatus.pending_approvals} pending` : ""}`}
          >
            {lineStatus.picture_url ? (
              <img
                src={lineStatus.picture_url}
                alt=""
                className="w-4 h-4 rounded-full shrink-0"
                style={{ objectFit: "cover" }}
              />
            ) : (
              <span
                className="w-1.5 h-1.5 rounded-full"
                style={{
                  background:
                    lineStatus.pending_approvals > 0
                      ? "var(--warning, #d19a66)"
                      : "var(--accent)",
                }}
                aria-hidden
              />
            )}
            <span
              className="truncate"
              style={{ color: "var(--text-primary)" }}
            >
              {lineStatus.display_name ?? "bridge live"}
            </span>
            {lineStatus.pending_approvals > 0 && (
              <span
                className="ml-auto"
                style={{ color: "var(--warning, #d19a66)", fontSize: "10px" }}
              >
                {lineStatus.pending_approvals}
              </span>
            )}
          </div>
        </Section>
      )}

      {/* Telegram bridge pill — visible only while connected. Mirrors
          the LINE pill; a warning dot flags pending approvals or
          pairing requests waiting on the owner. dev-plan/29 Tier 1. */}
      {telegramStatus.state === "connected" && (
        <Section title="Telegram">
          <div
            className="px-2 py-1 flex items-center gap-1.5"
            title={`${telegramStatus.bot_username ?? "bot"} · ${telegramStatus.active_chats} chat(s)${
              telegramStatus.pending_approvals > 0
                ? ` · ${telegramStatus.pending_approvals} approval(s) pending`
                : ""
            }${
              telegramStatus.pending_pairings > 0
                ? ` · ${telegramStatus.pending_pairings} pairing(s) waiting`
                : ""
            }`}
          >
            <span
              className="w-1.5 h-1.5 rounded-full"
              style={{
                background:
                  telegramStatus.pending_approvals > 0 ||
                  telegramStatus.pending_pairings > 0
                    ? "var(--warning, #d19a66)"
                    : "var(--accent)",
              }}
              aria-hidden
            />
            <span className="truncate" style={{ color: "var(--text-primary)" }}>
              {telegramStatus.bot_username ?? "bridge live"}
            </span>
            {telegramStatus.pending_pairings > 0 && (
              <span
                className="ml-auto"
                style={{ color: "var(--warning, #d19a66)", fontSize: "10px" }}
              >
                {telegramStatus.pending_pairings} pair
              </span>
            )}
            {telegramStatus.pending_approvals > 0 && (
              <span
                className={telegramStatus.pending_pairings > 0 ? "" : "ml-auto"}
                style={{ color: "var(--warning, #d19a66)", fontSize: "10px" }}
              >
                {telegramStatus.pending_approvals}
              </span>
            )}
          </div>
        </Section>
      )}

      {/* Sessions */}
      <Section
        title="Sessions"
        action={
          <button
            className="p-0.5 rounded hover:bg-white/10"
            title="New lead session (keeps active task running)"
            onClick={() => {
              send({ type: "new_session" });
            }}
          >
            <Plus size={12} />
          </button>
        }
      >
        {sessions.length === 0 ? (
          <div className="px-2 py-1" style={{ color: "var(--text-secondary)" }}>
            No saved sessions
          </div>
        ) : (() => {
          const q = sessionFilter.trim().toLowerCase();
          const filtered = q.length === 0
            ? sessions.slice(0, 10)
            : sessions
                .filter((s) =>
                  sessionLabel(s).toLowerCase().includes(q) ||
                  (s.owner_agent?.toLowerCase().includes(q) ?? false) ||
                  s.id.toLowerCase().includes(q),
                )
                .slice(0, 50);
          return (
            <>
              <input
                type="text"
                value={sessionFilter}
                onChange={(e) => setSessionFilter(e.target.value)}
                placeholder={`Search ${sessions.length} session${sessions.length === 1 ? "" : "s"}…`}
                aria-label="Filter sessions"
                className="w-full mx-2 mb-1 px-1.5 py-0.5 rounded text-xs"
                style={{
                  width: "calc(100% - 1rem)",
                  background: "var(--bg-secondary, rgba(255,255,255,0.04))",
                  color: "var(--text-primary)",
                  border: "1px solid var(--border, rgba(255,255,255,0.08))",
                  outline: "none",
                }}
              />
              {filtered.length === 0 ? (
                <div className="px-2 py-1" style={{ color: "var(--text-secondary)", fontSize: "11px" }}>
                  No matches for &ldquo;{sessionFilter.trim()}&rdquo;
                </div>
              ) : (
                filtered.map((s) => {
            const name = sessionLabel(s);
            const label = sessions.filter((other) => sessionLabel(other) === name).length > 1
              ? `${name} · ${s.id.slice(-6)}`
              : name;
            const isCurrent = s.id === currentSessionId;
            return (
              <div
                key={s.id}
                className="flex items-center gap-1 px-2 py-1 rounded hover:bg-white/5"
                style={
                  isCurrent
                    ? { background: "color-mix(in srgb, var(--accent) 15%, transparent)" }
                    : undefined
                }
                onContextMenu={(e) => {
                  e.preventDefault();
                  setSessionMenu({ session: s, x: e.clientX, y: e.clientY });
                }}
              >
                <span
                  className="w-1 shrink-0"
                  style={{
                    alignSelf: "stretch",
                    background: isCurrent ? "var(--accent)" : "transparent",
                    borderRadius: "2px",
                  }}
                  aria-hidden
                />
                <button
                  className="flex-1 text-left truncate"
                  style={{
                    color: "var(--text-primary)",
                    fontWeight: isCurrent ? 600 : 400,
                  }}
                  onClick={() => {
                    if (isCurrent) return;
                    send({ type: "session_load", id: s.id });
                  }}
                  title={`${label} (${s.id})${s.owner_agent ? ` — Agent: ${s.owner_agent}` : ""} — ${s.messages} msg${isCurrent ? " — current" : ""}`}
                >
                  <span
                    style={{ fontSize: "12px" }}
                  >
                    {label}
                  </span>
                </button>
              </div>
            );
          })
              )}
            </>
          );
        })()}
      </Section>

      {/* Knowledge bases */}
      <Section
        title="Knowledge"
        onHeaderContextMenu={(e) => {
          e.preventDefault();
          setKmsMenu({ x: e.clientX, y: e.clientY });
        }}
        action={
          <button
            className="p-0.5 rounded hover:bg-white/10"
            title="New KMS (right-click header to import/export OKF bundles)"
            onClick={() => setKmsModal({ kind: "kms" })}
          >
            <Plus size={12} />
          </button>
        }
      >
        {okfMsg && (
          <div
            className="mx-2 mb-1 px-2 py-1 rounded text-xs"
            style={{
              background: "var(--bg-secondary, rgba(255,255,255,0.04))",
              color: okfMsg.ok ? "var(--text-primary)" : "var(--danger, #e06c75)",
              border: "1px solid var(--border)",
            }}
            title={okfMsg.text}
          >
            {okfMsg.text}
          </div>
        )}
        {kmss.length === 0 ? (
          <div className="px-2 py-1" style={{ color: "var(--text-secondary)" }}>
            None yet
          </div>
        ) : (
          kmss.map((k) => (
            <div
              key={`${k.scope}:${k.name}`}
              className="flex items-center gap-1.5 px-2 py-1 rounded hover:bg-white/5"
              title={`${k.scope} scope — checkbox toggles attach; click name to browse; right-click to rename / delete`}
              onContextMenu={(e) => {
                e.preventDefault();
                setKmsRowMenu({ kms: k, x: e.clientX, y: e.clientY });
              }}
            >
              <input
                type="checkbox"
                checked={k.active}
                onChange={(e) =>
                  send({
                    type: "kms_toggle",
                    name: k.name,
                    active: e.target.checked,
                  })
                }
              />
              <button
                type="button"
                onClick={() => onBrowseKms?.(k.name)}
                className="flex-1 text-left truncate hover:underline"
                style={{ color: "var(--text-primary)", cursor: "pointer" }}
                title="Browse pages + sources for this KMS"
              >
                {k.name}
              </button>
              <span style={{ color: "var(--text-secondary)", fontSize: "10px" }}>
                {k.scope === "project" ? "(proj)" : ""}
              </span>
            </div>
          ))
        )}
      </Section>

      {/* MCP */}
      <Section title="MCP Servers">
        {mcpServers.length === 0 ? (
          <div className="px-2 py-1" style={{ color: "var(--text-secondary)" }}>
            None configured
          </div>
        ) : (
          mcpServers.map((s) => (
            <div
              key={s.name}
              className="px-2 py-1"
              style={{ color: "var(--text-primary)" }}
            >
              {s.name}{" "}
              <span style={{ color: "var(--text-secondary)" }}>
                ({s.tools})
              </span>
            </div>
          ))
        )}
      </Section>

      {/* M6.39.5: Research panel moved out of left Sidebar — the
          right-edge ResearchSidebar (mounted in App.tsx alongside
          PlanSidebar / TodoSidebar) shows the active job in detail.
          Discoverability of the list is sacrificed deliberately —
          one job at a time matches how users actually use /research,
          and the verbose right panel is more informative than the
          compact left list ever was. */}
      {/* Context menu for a right-clicked session row. Absolute, pinned
          to cursor coords. The onClick={stopPropagation} prevents the
          menu's own clicks from bubbling up to the window-level click
          handler that dismisses it. */}
      {sessionMenu && (
        <div
          className="fixed z-50 rounded border shadow-lg py-1 text-xs"
          style={{
            left: sessionMenu.x,
            top: sessionMenu.y,
            background: "var(--bg-primary)",
            borderColor: "var(--border)",
            color: "var(--text-primary)",
            minWidth: 140,
          }}
          onClick={(e) => e.stopPropagation()}
          onContextMenu={(e) => e.preventDefault()}
        >
          <CtxMenuItem
            onClick={() => {
              const s = sessionMenu.session;
              setSessionMenu(null);
              setRenameTarget({ id: s.id, current: s.title ?? "" });
            }}
          >
            Rename
          </CtxMenuItem>
          <CtxMenuItem
            danger
            onClick={async () => {
              const s = sessionMenu.session;
              setSessionMenu(null);
              // Wait one frame so React commits the menu-close before
              // the native confirm dialog blocks the webview's render
              // loop — otherwise the menu stays visible *behind* the
              // OS dialog on macOS (NSAlert pauses the whole app).
              await new Promise((r) => requestAnimationFrame(() => r(undefined)));
              const label = sessionLabel(s);
              const ok = await platformConfirm({
                title: "Delete session",
                message: `Delete session "${label}"? This removes it from disk and can't be undone.`,
                yesLabel: "Delete",
                noLabel: "Cancel",
              });
              if (ok) send({ type: "session_delete", id: s.id });
            }}
          >
            Delete
          </CtxMenuItem>
        </div>
      )}
      {kmsRowMenu && (
        <div
          className="fixed z-50 rounded border shadow-lg py-1 text-xs"
          style={{
            left: kmsRowMenu.x,
            top: kmsRowMenu.y,
            background: "var(--bg-primary)",
            borderColor: "var(--border)",
            color: "var(--text-primary)",
            minWidth: 160,
          }}
          onClick={(e) => e.stopPropagation()}
          onContextMenu={(e) => e.preventDefault()}
        >
          <div
            className="px-3 py-0.5 truncate"
            style={{ color: "var(--text-secondary)", fontSize: "9px" }}
          >
            {kmsRowMenu.kms.name}
            {kmsRowMenu.kms.scope === "project" ? " (proj)" : ""}
          </div>
          <CtxMenuItem
            onClick={() => {
              const k = kmsRowMenu.kms;
              setKmsRowMenu(null);
              setKmsRenameTarget(k);
            }}
          >
            Rename…
          </CtxMenuItem>
          <CtxMenuItem
            onClick={() => {
              const k = kmsRowMenu.kms;
              setKmsRowMenu(null);
              send({ type: "kms_export_okf", name: k.name });
            }}
          >
            Export OKF bundle…
          </CtxMenuItem>
          <CtxMenuItem
            danger
            onClick={async () => {
              const k = kmsRowMenu.kms;
              setKmsRowMenu(null);
              // Same one-frame wait as the session menu: let React close
              // the menu before the native dialog blocks the webview.
              await new Promise((r) => requestAnimationFrame(() => r(undefined)));
              const ok = await platformConfirm({
                title: "Delete KMS",
                message: `Delete KMS "${k.name}"? Every page and source in it is removed from disk. This can't be undone.`,
                yesLabel: "Delete",
                noLabel: "Cancel",
              });
              if (ok) send({ type: "kms_drop", name: k.name });
            }}
          >
            Delete…
          </CtxMenuItem>
        </div>
      )}
      {kmsRenameTarget && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setKmsRenameTarget(null);
          }}
        >
          <div
            className="rounded-lg border shadow-xl w-80 max-w-[92vw]"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onMouseDown={(e) => e.stopPropagation()}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold"
              style={{ borderColor: "var(--border)" }}
            >
              Rename KMS
            </div>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const next = (kmsRenameInputRef.current?.value ?? "").trim();
                if (next && next !== kmsRenameTarget.name) {
                  send({ type: "kms_rename", name: kmsRenameTarget.name, new_name: next });
                }
                setKmsRenameTarget(null);
              }}
            >
              <div className="px-4 py-3">
                <input
                  ref={kmsRenameInputRef}
                  type="text"
                  defaultValue={kmsRenameTarget.name}
                  placeholder="new-name (no spaces or slashes)"
                  className="w-full rounded border px-2 py-1 text-xs font-mono"
                  style={{
                    background: "var(--bg-secondary)",
                    borderColor: "var(--border)",
                    color: "var(--text-primary)",
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Escape") {
                      e.preventDefault();
                      setKmsRenameTarget(null);
                    }
                  }}
                />
                <div className="mt-1" style={{ color: "var(--text-secondary)", fontSize: "10px" }}>
                  Folder is renamed in place; the attachment follows. Wikilinks are unaffected.
                </div>
              </div>
              <div
                className="px-4 py-3 border-t flex items-center justify-end gap-2"
                style={{ borderColor: "var(--border)" }}
              >
                <button
                  type="button"
                  className="text-xs px-3 py-1.5 rounded hover:bg-white/5"
                  style={{ color: "var(--text-secondary)" }}
                  onClick={() => setKmsRenameTarget(null)}
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  className="text-xs px-3 py-1.5 rounded"
                  style={{
                    background: "var(--accent)",
                    color: "var(--accent-fg, #ffffff)",
                  }}
                >
                  Rename
                </button>
              </div>
            </form>
          </div>
        </div>
      )}
      {/* OKF import/export menu for the "Knowledge" header. Export lists
          each KMS (export is per-KMS); both actions open a native folder
          picker on the backend. */}
      {kmsMenu && (
        <div
          className="fixed z-50 rounded border shadow-lg py-1 text-xs"
          style={{
            left: kmsMenu.x,
            top: kmsMenu.y,
            background: "var(--bg-primary)",
            borderColor: "var(--border)",
            color: "var(--text-primary)",
            minWidth: 180,
            maxHeight: 320,
            overflowY: "auto",
          }}
          onClick={(e) => e.stopPropagation()}
          onContextMenu={(e) => e.preventDefault()}
        >
          <CtxMenuItem
            onClick={() => {
              setKmsMenu(null);
              setOkfImport({ scope: "user" });
            }}
          >
            Import OKF bundle…
          </CtxMenuItem>
          <div
            className="my-1"
            style={{ borderTop: "1px solid var(--border)" }}
            aria-hidden
          />
          <div
            className="px-3 py-0.5 uppercase tracking-wider"
            style={{ color: "var(--text-secondary)", fontSize: "9px" }}
          >
            Export OKF bundle
          </div>
          {kmss.length === 0 ? (
            <div
              className="px-3 py-1"
              style={{ color: "var(--text-secondary)" }}
            >
              No KMS yet
            </div>
          ) : (
            kmss.map((k) => (
              <CtxMenuItem
                key={`${k.scope}:${k.name}`}
                onClick={() => {
                  setKmsMenu(null);
                  send({ type: "kms_export_okf", name: k.name });
                }}
              >
                {k.name}
                {k.scope === "project" ? " (proj)" : ""}
              </CtxMenuItem>
            ))
          )}
        </div>
      )}
      {/* OKF import: collect the new KMS name + scope, then the backend
          opens a native folder picker for the bundle directory. */}
      {okfImport && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setOkfImport(null);
          }}
        >
          <div
            className="rounded-lg border shadow-xl w-80 max-w-[92vw]"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onMouseDown={(e) => e.stopPropagation()}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold"
              style={{ borderColor: "var(--border)" }}
            >
              Import OKF bundle
            </div>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const name = (okfImportNameRef.current?.value ?? "").trim();
                if (!name) return;
                send({ type: "kms_import_okf", name, scope: okfImport.scope });
                setOkfImport(null);
              }}
            >
              <div className="px-4 py-3 flex flex-col gap-3">
                <div>
                  <label
                    className="block mb-1"
                    style={{ color: "var(--text-secondary)", fontSize: "11px" }}
                  >
                    New KMS name
                  </label>
                  <input
                    ref={okfImportNameRef}
                    type="text"
                    placeholder="e.g. partner-knowledge"
                    className="w-full rounded border px-2 py-1 text-xs"
                    style={{
                      background: "var(--bg-secondary)",
                      borderColor: "var(--border)",
                      color: "var(--text-primary)",
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Escape") {
                        e.preventDefault();
                        setOkfImport(null);
                      }
                    }}
                  />
                </div>
                <div className="flex items-center gap-3" style={{ fontSize: "11px" }}>
                  <span style={{ color: "var(--text-secondary)" }}>Scope:</span>
                  <label className="flex items-center gap-1 cursor-pointer">
                    <input
                      type="radio"
                      name="okf-scope"
                      checked={okfImport.scope === "user"}
                      onChange={() => setOkfImport({ scope: "user" })}
                    />
                    user
                  </label>
                  <label className="flex items-center gap-1 cursor-pointer">
                    <input
                      type="radio"
                      name="okf-scope"
                      checked={okfImport.scope === "project"}
                      onChange={() => setOkfImport({ scope: "project" })}
                    />
                    project
                  </label>
                </div>
                <div style={{ color: "var(--text-secondary)", fontSize: "10px" }}>
                  You&rsquo;ll pick the bundle folder next.
                </div>
              </div>
              <div
                className="px-4 py-3 border-t flex items-center justify-end gap-2"
                style={{ borderColor: "var(--border)" }}
              >
                <button
                  type="button"
                  className="text-xs px-3 py-1.5 rounded hover:bg-white/5"
                  style={{ color: "var(--text-secondary)" }}
                  onClick={() => setOkfImport(null)}
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  className="text-xs px-3 py-1.5 rounded"
                  style={{ background: "var(--accent)", color: "var(--accent-fg, #fff)" }}
                >
                  Choose folder &amp; import
                </button>
              </div>
            </form>
          </div>
        </div>
      )}
      {/* Rename dialog: simple text input in a centered modal. Replaces
          the wry-blocked window.prompt that we used to call here. */}
      {renameTarget && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          // Close on backdrop mousedown only when the click started
          // on the backdrop itself. A drag-to-select in the input
          // that ends outside the modal shouldn't dismiss.
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setRenameTarget(null);
          }}
        >
          <div
            className="rounded-lg border shadow-xl w-80 max-w-[92vw]"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onMouseDown={(e) => e.stopPropagation()}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold"
              style={{ borderColor: "var(--border)" }}
            >
              Rename session
            </div>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const next = (renameInputRef.current?.value ?? "").trim();
                send({ type: "session_rename", id: renameTarget.id, title: next });
                setRenameTarget(null);
              }}
            >
              <div className="px-4 py-3">
                <label className="block mb-3 text-xs" style={{ color: "var(--text-secondary)" }}>
                  Original session ID (unchanged)
                  <input
                    readOnly
                    value={renameTarget.id}
                    onFocus={(e) => e.currentTarget.select()}
                    className="mt-1 w-full rounded border px-2 py-1 font-mono text-xs"
                    style={{ background: "var(--bg-secondary)", borderColor: "var(--border)", color: "var(--text-primary)" }}
                  />
                </label>
                <label htmlFor="session-rename-title" className="block mb-1 text-xs">
                  Display name
                </label>
                <input
                  id="session-rename-title"
                  ref={renameInputRef}
                  type="text"
                  defaultValue={renameTarget.current}
                  placeholder="Leave empty to clear title"
                  className="w-full rounded border px-2 py-1 text-xs"
                  style={{
                    background: "var(--bg-secondary)",
                    borderColor: "var(--border)",
                    color: "var(--text-primary)",
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Escape") {
                      e.preventDefault();
                      setRenameTarget(null);
                    }
                  }}
                />
              </div>
              <div
                className="px-4 py-3 border-t flex items-center justify-end gap-2"
                style={{ borderColor: "var(--border)" }}
              >
                <button
                  type="button"
                  className="text-xs px-3 py-1.5 rounded hover:bg-white/5"
                  style={{ color: "var(--text-secondary)" }}
                  onClick={() => setRenameTarget(null)}
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  className="text-xs px-3 py-1.5 rounded"
                  style={{
                    background: "var(--accent)",
                    color: "var(--accent-fg, #ffffff)",
                  }}
                >
                  Save
                </button>
              </div>
            </form>
          </div>
        </div>
      )}
      {kmsModal && (
        <KmsCreateModal mode={kmsModal} onClose={() => setKmsModal(null)} />
      )}
      </div>
      {/* Drag handle — thin gutter on the right edge. col-resize cursor
          + hover hint. Double-click resets to default width so the
          user can recover from accidentally squishing too small. */}
      <div
        onMouseDown={(e) => {
          e.preventDefault();
          setResizing(true);
        }}
        onDoubleClick={() => setSidebarWidth(SIDEBAR_WIDTH_DEFAULT)}
        title="Drag to resize · double-click to reset"
        style={{
          width: 3,
          cursor: "col-resize",
          background: resizing ? "var(--accent)" : "transparent",
          flexShrink: 0,
          transition: resizing ? undefined : "background 0.15s",
        }}
        onMouseEnter={(e) => {
          if (!resizing) {
            (e.currentTarget as HTMLDivElement).style.background =
              "var(--border-strong, var(--border))";
          }
        }}
        onMouseLeave={(e) => {
          if (!resizing) {
            (e.currentTarget as HTMLDivElement).style.background = "transparent";
          }
        }}
      />
    </div>
  );
}

const THINKING_LEVELS: { level: number | null; label: string; hint: string }[] = [
  { level: null, label: "auto", hint: "provider default" },
  { level: 0, label: "0", hint: "off — fastest, no reasoning" },
  { level: 1, label: "1", hint: "low" },
  { level: 2, label: "2", hint: "medium" },
  { level: 3, label: "3", hint: "high — deepest reasoning" },
];

/// Thinking level pills under the model chip. Same knob as `/thinking`,
/// mapped per provider by the engine (Anthropic budget, OpenAI effort,
/// DeepSeek/Qwen switch, Gemini budget/level, Ollama think).
function ThinkingSelector({
  level,
  onChange,
}: {
  level: number | null;
  onChange: (lv: number | null) => void;
}) {
  return (
    <div
      className="ml-3 mt-1 flex items-center gap-1"
      style={{ fontSize: "10px", color: "var(--text-secondary)" }}
      title="Thinking level — how much the model reasons before answering (also: /thinking 0-3)"
    >
      <span style={{ opacity: 0.7 }}>think</span>
      {THINKING_LEVELS.map((opt) => {
        const active = opt.level === level;
        return (
          <button
            key={opt.label}
            type="button"
            onClick={() => onChange(opt.level)}
            title={opt.hint}
            className="rounded px-1"
            style={{
              fontSize: "10px",
              lineHeight: "14px",
              cursor: "pointer",
              border: "1px solid",
              borderColor: active ? "var(--accent)" : "var(--border)",
              background: active ? "var(--bg-tertiary)" : "transparent",
              color: active ? "var(--text-primary)" : "var(--text-secondary)",
              fontWeight: active ? 600 : 400,
            }}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

function Section({
  title,
  children,
  action,
  onHeaderContextMenu,
}: {
  title: string;
  children: React.ReactNode;
  action?: React.ReactNode;
  onHeaderContextMenu?: (e: React.MouseEvent) => void;
}) {
  return (
    <div className="mb-2">
      <div
        className="px-2 py-1.5 font-semibold uppercase tracking-wider flex items-center justify-between"
        style={{
          color: "var(--text-secondary)",
          fontSize: "10px",
          borderBottom: "1px solid var(--border)",
        }}
        onContextMenu={onHeaderContextMenu}
      >
        {title}
        {action}
      </div>
      <div className="py-1">{children}</div>
    </div>
  );
}
