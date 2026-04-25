import { useState, useEffect, useRef } from "react";
import { Plus } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";

type SessionInfo = { id: string; model: string; messages: number; title?: string | null };
type KmsInfo = { name: string; scope: "user" | "project"; active: boolean };

// Wry blocks `window.confirm`, so we proxy through a native OS dialog
// via IPC. Mirrors `confirmNative` in FilesView.
function confirmNative(opts: {
  title: string;
  message: string;
  yesLabel?: string;
  noLabel?: string;
}): Promise<boolean> {
  return new Promise((resolve) => {
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

export function Sidebar() {
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [currentSessionId, setCurrentSessionId] = useState<string>("");
  const [activeProvider, setActiveProvider] = useState("anthropic");
  const [activeModel, setActiveModel] = useState("claude-sonnet-4-5");
  const [providerReady, setProviderReady] = useState(true);
  const [mcpServers, setMcpServers] = useState<
    { name: string; tools: number }[]
  >([]);
  const [kmss, setKmss] = useState<KmsInfo[]>([]);
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
      }
    });
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
        <div className="px-2 py-1">
          <div className="flex items-center gap-1.5">
            <span
              className="w-1.5 h-1.5 rounded-full"
              style={{
                background: providerReady
                  ? "var(--accent)"
                  : "var(--danger, #e06c75)",
              }}
              title={providerReady ? "Provider ready" : "No API key configured"}
            />
            <span
              style={{
                color: providerReady
                  ? "var(--text-primary)"
                  : "var(--text-secondary)",
                textDecoration: providerReady ? "none" : "line-through",
              }}
              title={providerReady ? undefined : "No API key — open the gear → Provider API keys"}
            >
              {activeProvider}
            </span>
          </div>
          <div
            className="ml-3 font-mono"
            style={{ color: "var(--text-secondary)", fontSize: "10px" }}
          >
            {activeModel}
          </div>
          {!providerReady && (
            <div
              className="ml-3 mt-1"
              style={{ color: "var(--danger, #e06c75)", fontSize: "10px" }}
            >
              no API key — set one in Settings
            </div>
          )}
        </div>
      </Section>

      {/* Sessions */}
      <Section
        title="Sessions"
        action={
          <button
            className="p-0.5 rounded hover:bg-white/10"
            title="New session (save current + clear)"
            onClick={() => send({ type: "new_session" })}
          >
            <Plus size={12} />
          </button>
        }
      >
        {sessions.length === 0 ? (
          <div className="px-2 py-1" style={{ color: "var(--text-secondary)" }}>
            No saved sessions
          </div>
        ) : (
          sessions.slice(0, 10).map((s) => {
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
                  onClick={() => send({ type: "session_load", id: s.id })}
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
      </Section>

      {/* Knowledge bases */}
      <Section
        title="Knowledge"
        action={
          <button
            className="p-0.5 rounded hover:bg-white/10"
            title="New KMS"
            onClick={() => {
              const name = window.prompt(
                "New KMS name (letters, digits, -, _):",
                "",
              );
              if (!name) return;
              const trimmed = name.trim();
              if (!trimmed) return;
              const scope = window.confirm(
                `Scope?\n\nOK = user (~/.config/thclaws/kms/${trimmed})\nCancel = project (./.thclaws/kms/${trimmed})`,
              )
                ? "user"
                : "project";
              send({ type: "kms_new", name: trimmed, scope });
            }}
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
            <label
              key={`${k.scope}:${k.name}`}
              className="flex items-center gap-1.5 px-2 py-1 rounded hover:bg-white/5 cursor-pointer"
              title={`${k.scope} scope`}
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
              <span style={{ color: "var(--text-primary)" }}>{k.name}</span>
              <span style={{ color: "var(--text-secondary)", fontSize: "10px" }}>
                {k.scope === "project" ? "(proj)" : ""}
              </span>
            </label>
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
              const ok = await confirmNative({
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
