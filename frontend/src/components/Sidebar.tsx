import { useState, useEffect, useRef } from "react";
import { Plus } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { ModelPickerDropdown } from "./ModelPickerDropdown";
import { KmsCreateModal, type KmsCreateMode } from "./KmsCreateModal";

type SessionInfo = { id: string; model: string; messages: number; title?: string | null };
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

export function Sidebar({ onBrowseKms }: SidebarProps = {}) {
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [currentSessionId, setCurrentSessionId] = useState<string>("");
  const [activeProvider, setActiveProvider] = useState("anthropic");
  const [activeModel, setActiveModel] = useState("claude-sonnet-4-5");
  const [providerReady, setProviderReady] = useState(true);
  // Inline model picker dropdown anchored to the Provider section.
  // null means closed; opens on click of the active model row. #49.
  const [modelPickerOpen, setModelPickerOpen] = useState(false);
  const [mcpServers, setMcpServers] = useState<
    { name: string; tools: number }[]
  >([]);
  const [kmss, setKmss] = useState<KmsInfo[]>([]);
  // KMS create modal (new KMS base). null = closed. Replaces the old
  // window.prompt() flow that silently failed inside the webview.
  const [kmsModal, setKmsModal] = useState<KmsCreateMode | null>(null);
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
      } else if (msg.type === "mcp_update") {
        setMcpServers(msg.servers as { name: string; tools: number }[]);
      } else if (msg.type === "kms_update") {
        setKmss(msg.kmss as KmsInfo[]);
      } else if (msg.type === "line_status") {
        setLineStatus({
          state: (msg.state as LineStatus["state"]) ?? "disconnected",
          server_url: (msg.server_url as string) ?? "",
          pending_approvals: (msg.pending_approvals as number) ?? 0,
          display_name: (msg.display_name as string | undefined) ?? undefined,
          picture_url: (msg.picture_url as string | undefined) ?? undefined,
        });
      }
    });
    // Ask for current LINE state once at mount. The backend replies
    // with a `line_status` envelope the subscriber above renders.
    // (SSO state is fetched by the navbar LoginButton.)
    send({ type: "line_status" });
    return unsub;
  }, []);

  // Dismiss the context menu on any outside click or Escape — standard
  // popover behavior. The menu's own buttons call setSessionMenu(null)
  // before acting so they don't self-dismiss prematurely.
  useEffect(() => {
    if (!sessionMenu) return;
    const onClick = () => setSessionMenu(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setSessionMenu(null);
    };
    window.addEventListener("click", onClick);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("click", onClick);
      window.removeEventListener("keydown", onKey);
    };
  }, [sessionMenu]);

  // Focus + select-all when the rename dialog opens so the user can
  // either replace the whole title or click to keep part of it.
  useEffect(() => {
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
      className="w-48 border-r overflow-y-auto shrink-0 text-xs select-none"
      style={{
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
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

      {/* Sessions */}
      <Section
        title="Sessions"
        action={
          <button
            className="p-0.5 rounded hover:bg-white/10"
            title="New session (cancels active task + saves current + clears)"
            onClick={() => {
              // session_load / new_session are processed by the same
              // single-threaded worker that runs agent turns; if a turn
              // is in flight the swap message sits in the input queue
              // until the turn finishes — issue #95(a): users expected
              // the click to switch sessions immediately. Always fire
              // shell_cancel first; it's idempotent on the backend
              // (no-op when nothing is running) so it's safe to send
              // even on an idle agent. Same reasoning as the Ctrl+C
              // handler in TerminalView.tsx.
              send({ type: "shell_cancel" });
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
                  (s.title?.toLowerCase().includes(q) ?? false) ||
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
            const label = s.title && s.title.trim().length > 0
              ? s.title
              : s.id;
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
                    // See "New session" button comment above for why
                    // shell_cancel goes first — issue #95(a).
                    send({ type: "shell_cancel" });
                    send({ type: "session_load", id: s.id });
                  }}
                  title={s.title ? `${s.title} (${s.id}) — ${s.messages} msg${isCurrent ? " — current" : ""}` : `${s.id} — ${s.messages} msg${isCurrent ? " — current" : ""}`}
                >
                  <span
                    className={s.title ? "" : "font-mono"}
                    style={{ fontSize: s.title ? "12px" : "10px" }}
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
        action={
          <button
            className="p-0.5 rounded hover:bg-white/10"
            title="New KMS"
            onClick={() => setKmsModal({ kind: "kms" })}
          >
            <Plus size={12} />
          </button>
        }
      >
        {kmss.length === 0 ? (
          <div className="px-2 py-1" style={{ color: "var(--text-secondary)" }}>
            None yet
          </div>
        ) : (
          kmss.map((k) => (
            <div
              key={`${k.scope}:${k.name}`}
              className="flex items-center gap-1.5 px-2 py-1 rounded hover:bg-white/5"
              title={`${k.scope} scope — checkbox toggles attach; click name to browse`}
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
              const label = s.title && s.title.trim().length > 0 ? s.title : s.id;
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
            className="rounded-lg border shadow-xl w-80"
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
                <input
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
  );
}

// Context-menu item with a solid accent-colored hover/focus highlight.
// `hover:bg-white/5` on the raw <button> is barely visible on light
// themes and under the modal backdrop, so we drive the background
// from state + pair it with a contrasting foreground colour.
function CtxMenuItem({
  onClick,
  danger,
  children,
}: {
  onClick: () => void;
  danger?: boolean;
  children: React.ReactNode;
}) {
  const [hot, setHot] = useState(false);
  const activeBg = danger
    ? "var(--danger, #e06c75)"
    : "var(--accent)";
  const activeFg = "var(--accent-fg, #ffffff)";
  const idleFg = danger ? "var(--danger, #e06c75)" : "var(--text-primary)";
  return (
    <button
      className="w-full text-left px-3 py-1 transition-colors"
      style={{
        background: hot ? activeBg : "transparent",
        color: hot ? activeFg : idleFg,
      }}
      onMouseEnter={() => setHot(true)}
      onMouseLeave={() => setHot(false)}
      onFocus={() => setHot(true)}
      onBlur={() => setHot(false)}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

function Section({
  title,
  children,
  action,
}: {
  title: string;
  children: React.ReactNode;
  action?: React.ReactNode;
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
      >
        {title}
        {action}
      </div>
      <div className="py-1">{children}</div>
    </div>
  );
}
