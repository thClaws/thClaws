import { useCallback, useEffect, useRef, useState } from "react";
import {
  Terminal,
  MessageSquare,
  FolderTree,
  Users,
  FolderOpen,
  Folder,
  Settings,
  Sparkles,
  Layout,
  Maximize2,
  Globe,
  Menu,
  PanelLeftClose,
  PanelLeftOpen,
} from "lucide-react";
import { TerminalView } from "./components/TerminalView";
import { ChatView } from "./components/ChatView";
import { FilesView } from "./components/FilesView";
import { TeamView } from "./components/TeamView";
import { UITab } from "./components/UITab";
import { ShellTab } from "./components/ShellTab";
import { BrowserView } from "./components/BrowserView";
import { LoginButton } from "./components/LoginButton";
import { RunningChip } from "./components/RunningChip";
import { useBusyState } from "./hooks/useBusyState";
import { useTheme } from "./hooks/useTheme";
import { Sidebar } from "./components/Sidebar";
import { PlanSidebar } from "./components/PlanSidebar";
import { GoalSidebar } from "./components/GoalSidebar";
import { TodoSidebar } from "./components/TodoSidebar";
import { ResearchSidebar } from "./components/ResearchSidebar";
import { BackgroundAgentsSidebar } from "./components/BackgroundAgentsSidebar";
import {
  KmsBrowserSidebar,
  type ViewerTarget,
} from "./components/KmsBrowserSidebar";
import { KmsViewerOverlay } from "./components/KmsViewerOverlay";
import { KmsGraphView } from "./components/KmsGraphView";
import { SettingsModal } from "./components/SettingsModal";
import { LineConnectModal } from "./components/LineConnectModal";
import { TelegramConnectModal } from "./components/TelegramConnectModal";
import { MessengerConnectModal } from "./components/MessengerConnectModal";
import { SettingsMenu } from "./components/SettingsMenu";
import { InstructionsEditorModal } from "./components/InstructionsEditorModal";
import { SecretsBackendDialog } from "./components/SecretsBackendDialog";
import { ApprovalModal } from "./components/ApprovalModal";
import { ScheduleAddModal } from "./components/ScheduleAddModal";
import { AgentEditorModal } from "./components/AgentEditorModal";
import { MarketplaceModal } from "./components/MarketplaceModal";
import {
  ModelPickerModal,
  type PickerModel,
} from "./components/ModelPickerModal";
import { ContextWarningBanner } from "./components/ContextWarningBanner";
import { useEditingShortcuts } from "./hooks/useEditingShortcuts";
import { send, subscribe } from "./hooks/useIPC";

type Tab = "terminal" | "chat" | "files" | "team" | "ui" | "shell" | "browser";

// Fires `frontend_ready` once on mount. Mounted only after both
// startup modals (working-directory + secrets-backend) dismiss, so
// the backend can release deferred work like MCP-spawn approval
// prompts that shouldn't race the launch modals.
function FrontendReadyBeacon() {
  useEffect(() => {
    send({ type: "frontend_ready" });
  }, []);
  return null;
}

const ALL_TABS: { id: Tab; label: string; icon: React.ReactNode }[] = [
  { id: "chat", label: "Chat", icon: <MessageSquare size={14} /> },
  { id: "terminal", label: "Terminal", icon: <Terminal size={14} /> },
  { id: "files", label: "Files", icon: <FolderTree size={14} /> },
  { id: "team", label: "Team", icon: <Users size={14} /> },
  // dev-plan/33 Tier 2: GUI Shell picker (iframe-loaded installable
  // domain frontends). Renamed from "Shell" → "UI" once the new
  // PTY-backed Shell tab took the name.
  { id: "ui", label: "UI", icon: <Sparkles size={14} /> },
  // PTY-backed live shell. Spawns `$SHELL` (or fallback) and pipes
  // stdio through xterm.js.
  { id: "shell", label: "Shell", icon: <Layout size={14} /> },
  // docs/browser Phase 1: status + activity for the engine-managed
  // Playwright MCP browser. Shown only when `browserEnabled` is set.
  { id: "browser", label: "Browser", icon: <Globe size={14} /> },
];

// ── Startup modal ────────────────────────────────────────────────────
// Shown before anything else. User confirms (or changes) the working
// directory; on "Start" the backend sets cwd + re-inits sandbox, and
// only then does the PTY spawn and the tabs become active.

/**
 * Host-owned escape hatch for full-screen UI mode. The host hides all
 * its chrome in full-screen, so this guarantees the user can always
 * get back out. It is deliberately NON-OCCLUDING:
 *
 *   - On entering full-screen, a brief auto-dismissing toast names the
 *     keyboard escape (⌘⇧U / Ctrl⇧U) — discoverability without
 *     permanently covering shell content.
 *   - The clickable exit chip is hidden until the pointer enters the
 *     top-right hot corner (like full-screen video controls), so it
 *     never sits on top of the shell during normal use.
 *   - If the shell declares it renders its own exit control
 *     (`thclaws.ui.claimExitControl()` → `claimed`), the host chip is
 *     suppressed entirely; the toast + keyboard escape remain as the
 *     safety net.
 *
 * The keyboard escape lives in App's keydown handler and always works
 * regardless of this component.
 */
function FullscreenExitChrome({
  onExit,
  claimed,
}: {
  onExit: () => void;
  claimed: boolean;
}) {
  const isMac =
    typeof navigator !== "undefined" && navigator.platform.startsWith("Mac");
  const kbd = isMac ? "⌘⇧U" : "Ctrl⇧U";
  // Toast shows on mount (= on entering full-screen) and fades after a
  // few seconds.
  const [toast, setToast] = useState(true);
  // Chip only appears while the pointer is in the top-right hot corner.
  const [nearCorner, setNearCorner] = useState(false);

  useEffect(() => {
    const t = setTimeout(() => setToast(false), 4000);
    return () => clearTimeout(t);
  }, []);

  useEffect(() => {
    if (claimed) return; // shell owns the control — no hot-corner chip
    const onMove = (e: MouseEvent) => {
      const inCorner = e.clientX >= window.innerWidth - 120 && e.clientY <= 120;
      setNearCorner(inCorner);
    };
    window.addEventListener("mousemove", onMove);
    return () => window.removeEventListener("mousemove", onMove);
  }, [claimed]);

  return (
    <>
      {toast && (
        <div
          className="fixed top-3 left-1/2 -translate-x-1/2 z-50 px-3 py-1.5 rounded-full text-[11px] font-medium pointer-events-none transition-opacity duration-500"
          style={{
            background: "var(--bg-secondary)",
            color: "var(--text-secondary)",
            border: "1px solid var(--border)",
            backdropFilter: "blur(4px)",
            boxShadow: "0 2px 8px rgba(0,0,0,0.18)",
          }}
        >
          Press <span className="font-mono">{kbd}</span> to exit full screen
        </div>
      )}
      {!claimed && (
        <button
          onClick={onExit}
          title={`Exit full-screen UI (${kbd})`}
          className="fixed top-2 right-2 z-50 px-2 py-1 rounded text-[10px] font-mono transition-opacity duration-200"
          style={{
            background: "var(--bg-secondary)",
            color: "var(--text-secondary)",
            border: "1px solid var(--border)",
            backdropFilter: "blur(4px)",
            opacity: nearCorner ? 1 : 0,
            pointerEvents: nearCorner ? "auto" : "none",
          }}
        >
          {kbd}
        </button>
      )}
    </>
  );
}

function StartupModal({
  onStart,
}: {
  onStart: (cwd: string, initialTab?: Tab) => void;
}) {
  const [cwd, setCwd] = useState("");
  const [error, setError] = useState("");
  const [showModal, setShowModal] = useState<boolean | null>(null);
  const [picking, setPicking] = useState(false);
  const [recentDirs, setRecentDirs] = useState<string[]>([]);
  // If we never hear back from the backend, flip this to show a
  // diagnostic instead of an indefinite blank screen. Known-bad
  // situation on some macOS x86 cross-compiled builds where the
  // wry IPC bridge doesn't inject `window.ipc`.
  const [ipcDead, setIpcDead] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let gotResponse = false;
    const unsub = subscribe((msg) => {
      if (msg.type === "current_cwd" && typeof msg.path === "string") {
        // The retry loop below can put several asks in flight before the
        // first answer lands. Only the first decides: a later one saying
        // "no picker" would otherwise skip a picker the first one opened.
        if (gotResponse) return;
        gotResponse = true;
        // A slow backend is not a dead one — clear the diagnostic if the
        // answer arrives after the deadline.
        setIpcDead(false);
        setCwd(msg.path as string);
        if (Array.isArray(msg.recent_dirs)) {
          setRecentDirs(msg.recent_dirs as string[]);
        }
        // Backend resolves guiShell.tabDefault to "ui" when set so the
        // workspace lands on the GUI shell instead of always defaulting
        // to Terminal. Pass through to onStart so App.tsx can seed
        // useState<Tab> before the main UI mounts.
        const initialTab =
          typeof msg.initial_tab === "string"
            ? (msg.initial_tab as Tab)
            : undefined;
        if (msg.needs_modal === false) {
          onStart(msg.path as string, initialTab);
        } else {
          setShowModal(true);
        }
      } else if (msg.type === "directory_picked") {
        setPicking(false);
        if (typeof msg.path === "string") {
          setCwd(msg.path as string);
          setError("");
        }
      } else if (msg.type === "cwd_changed") {
        if (msg.ok) {
          onStart(msg.path as string);
        } else {
          setError(msg.error as string);
        }
      }
    });
    // Retry get_cwd on a short interval: window.ipc may not be injected
    // yet on the very first useEffect tick (wry injects it after the page
    // loads, but React's first effect can fire before that). Polling at
    // 100ms is invisible to the user and stops the moment we hear back.
    send({ type: "get_cwd" });
    const retry = setInterval(() => {
      if (!gotResponse) send({ type: "get_cwd" });
      else clearInterval(retry);
    }, 100);
    // Fallback: if we haven't heard back in 3 seconds the IPC bridge is
    // almost certainly broken — show a readable error rather than an
    // indefinite blank screen.
    const deadline = setTimeout(() => {
      if (!gotResponse) setIpcDead(true);
    }, 3000);
    return () => {
      unsub();
      clearInterval(retry);
      clearTimeout(deadline);
    };
  }, [onStart]);

  // Focus the input whenever cwd changes and the modal is visible.
  // Must be declared before any conditional return (React Rules of Hooks).
  useEffect(() => {
    if (showModal) inputRef.current?.focus();
  }, [cwd, showModal]);

  // Still waiting for backend reply — show nothing, unless we've
  // been waiting long enough to conclude the bridge is gone.
  if (showModal === null) {
    if (!ipcDead) {
      return (
        <div
          className="fixed inset-0 flex items-center justify-center"
          style={{ background: "var(--bg-primary)" }}
        />
      );
    }
    // IPC dead-air fallback — diagnostic UI so the user isn't staring
    // at a blank screen. Reachable on some macOS x86 cross-compiled
    // builds where `window.ipc` doesn't get injected by wry.
    const ipcPresent =
      typeof (window as unknown as { ipc?: unknown }).ipc !== "undefined";
    return (
      <div
        className="fixed inset-0 flex items-center justify-center p-6"
        style={{
          background: "var(--bg-primary)",
          color: "var(--text-primary)",
        }}
      >
        <div
          className="rounded-lg shadow-2xl p-6 max-w-xl w-full"
          style={{
            background: "var(--bg-secondary)",
            border: "1px solid var(--border)",
          }}
        >
          <h2 className="text-sm font-semibold mb-3">
            thClaws couldn't reach its backend
          </h2>
          <p
            className="text-xs mb-3"
            style={{ color: "var(--text-secondary)" }}
          >
            The frontend loaded, but no reply came back from the Rust side after
            3 seconds. Usually means the WebView↔Rust IPC bridge failed to
            initialise — common on older macOS x86 builds or when a dependency
            is blocked by security software.
          </p>
          <ul
            className="text-[11px] list-disc pl-5 space-y-1 mb-3"
            style={{ color: "var(--text-secondary)" }}
          >
            <li>
              <code className="font-mono">window.ipc</code> available:{" "}
              <strong>{ipcPresent ? "yes" : "no (this is the problem)"}</strong>
            </li>
            <li>
              Platform: <code className="font-mono">{navigator.platform}</code>
            </li>
            <li>
              UserAgent:{" "}
              <code className="font-mono">
                {navigator.userAgent.slice(0, 80)}…
              </code>
            </li>
          </ul>
          <p className="text-[11px]" style={{ color: "var(--text-secondary)" }}>
            Try running with{" "}
            <code className="font-mono">THCLAWS_DEVTOOLS=1 thclaws</code>, then
            right-click → Inspect to see the console. File an issue at{" "}
            <code className="font-mono">github.com/thClaws/thClaws/issues</code>{" "}
            with the console output and these details.
          </p>
        </div>
      </div>
    );
  }

  const handleStart = () => {
    setError("");
    if (!cwd.trim()) return;
    send({ type: "set_cwd", path: cwd.trim() });
  };

  return (
    <div
      className="fixed inset-0 flex items-center justify-center z-50"
      style={{ background: "var(--modal-backdrop)" }}
    >
      <div
        className="rounded-lg shadow-2xl p-6 max-w-lg w-full mx-4"
        style={{
          background: "var(--bg-secondary)",
          border: "1px solid var(--border)",
        }}
      >
        <div className="flex items-center gap-2 mb-4">
          <FolderOpen size={20} style={{ color: "var(--accent)" }} />
          <h2
            className="text-sm font-semibold"
            style={{ color: "var(--text-primary)" }}
          >
            Working Directory
          </h2>
        </div>
        <p className="text-xs mb-3" style={{ color: "var(--text-secondary)" }}>
          thClaws will operate inside this directory. All file tools are
          sandboxed to it. Change it now if needed.
        </p>
        <div className="flex gap-1.5 mb-1">
          <input
            ref={inputRef}
            type="text"
            className="flex-1 px-3 py-2 rounded text-xs font-mono outline-none"
            style={{
              background: "var(--bg-tertiary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
            value={cwd}
            onChange={(e) => {
              setCwd(e.target.value);
              setError("");
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleStart();
            }}
          />
          <button
            className="px-3 py-2 rounded text-xs font-medium shrink-0"
            style={{
              background: "var(--bg-tertiary)",
              color: "var(--text-secondary)",
              border: "1px solid var(--border)",
            }}
            onClick={() => {
              setPicking(true);
              send({ type: "pick_directory", start: cwd });
            }}
            disabled={picking}
            title="Browse for directory"
          >
            {picking ? "…" : "Browse"}
          </button>
        </div>
        {error && (
          <p
            className="text-xs mb-2"
            style={{ color: "var(--danger, #e06c75)" }}
          >
            {error}
          </p>
        )}
        {recentDirs.filter((d) => d !== cwd).length > 0 && (
          <div className="mt-3 mb-1">
            <p
              className="text-[10px] mb-1.5 uppercase tracking-wider"
              style={{ color: "var(--text-secondary)" }}
            >
              Recent
            </p>
            <div className="flex flex-col gap-1">
              {recentDirs
                .filter((d) => d !== cwd)
                .map((dir) => (
                  <button
                    key={dir}
                    className="text-left px-2.5 py-1.5 rounded text-xs font-mono truncate hover:brightness-125 transition-colors"
                    style={{
                      background: "var(--bg-tertiary)",
                      color: "var(--text-primary)",
                      border: "1px solid var(--border)",
                    }}
                    onClick={() => {
                      setCwd(dir);
                      setError("");
                    }}
                    title={dir}
                  >
                    {dir}
                  </button>
                ))}
            </div>
          </div>
        )}
        <div className="flex justify-end mt-4">
          <button
            className="px-4 py-1.5 rounded text-xs font-medium"
            style={{
              background: "var(--accent)",
              color: "#fff",
            }}
            onClick={handleStart}
          >
            Start
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Main app ─────────────────────────────────────────────────────────

export default function App() {
  // Wire up Cmd+C / Cmd+X / Cmd+V / Cmd+A / Cmd+Z for every <input>
  // and <textarea> in the app. Wry doesn't forward the macOS edit-menu
  // shortcuts by default; without this the user has to right-click
  // to paste.
  useEditingShortcuts();

  // Lets a full-screen GUI shell flip the app theme from its own navbar
  // (the host's theme switch is hidden in full-screen). Persists + syncs
  // app-wide, same as Settings → Theme.
  const { setMode } = useTheme();

  // dev-plan/36 — auto-attach to the right session on tab open. Two
  // cases, handled by a SINGLE auto-load that fires ONCE per mount:
  //
  //   1. Agent is currently busy → load the busy session so the chat
  //      view streams the live `[i/N]` progress (the original
  //      dev-plan/36 Tier 1 goal).
  //   2. Agent is idle but the user previously worked on a session
  //      (closed tab after a batch finished, came back to review) →
  //      load the most-recent non-empty session so they land in
  //      their work instead of a blank new turn.
  //
  // Loads go through the `session_load` IPC (same path the sidebar's
  // click-to-load uses) so the engine swaps `state.session`, fires a
  // `chat_history_replaced` event, and the chat view repaints.
  // `shell_input "/load <id>"` would also work but races worker
  // readiness; `session_load` is the proper typed handler.
  const busyState = useBusyState();
  const [knownSessions, setKnownSessions] = useState<
    Array<{ id: string; messages: number; title?: string | null }>
  >([]);
  // Was the agent ALREADY busy when this surface opened? `gui_busy_result`
  // is the reply to the query useBusyState fires on mount, so the first one
  // is the authoritative snapshot for "state at open". `null` until it
  // lands — busyState itself starts optimistically idle, which is not the
  // same thing.
  const busyAtOpenRef = useRef<boolean | null>(null);
  // Session the worker considers current, from `sessions_list.current_id`.
  const currentSessionIdRef = useRef<string | null>(null);
  useEffect(() => {
    const unsub = subscribe((msg: any) => {
      if (msg?.type === "initial_state" || msg?.type === "sessions_list") {
        if (Array.isArray(msg.sessions)) setKnownSessions(msg.sessions);
        // `current_id` is "" until a session is actually current — that
        // is "unknown", not "some other session", and treating it as the
        // latter is what made the guard below miss.
        if (typeof msg.current_id === "string" && msg.current_id) {
          currentSessionIdRef.current = msg.current_id;
        }
      }
      if (msg?.type === "gui_busy_result" && busyAtOpenRef.current === null) {
        busyAtOpenRef.current = !!msg.busy;
      }
    });
    // useIPC opens the WS at module-load and fires frontend_ready in
    // ws.onopen — both events can complete BEFORE this useEffect
    // runs (App may re-render mid-flow via StartupModal). If the
    // initial_state was already dispatched, our subscribe missed it
    // and `knownSessions` stays empty forever. Re-fire frontend_ready
    // here so the engine sends a fresh snapshot AFTER our subscribe
    // is in place. The engine's handler is idempotent — just rebuilds
    // the same snapshot from the current SessionStore.
    send({ type: "frontend_ready" });
    return unsub;
  }, []);
  const autoLoadedRef = useRef(false);
  // Did a turn start while this surface was already open? Then the
  // conversation on screen is ours and there is nothing to auto-resume
  // into — see the note on case 1.
  const sawOwnTurnRef = useRef(false);
  if (busyState.busy && busyAtOpenRef.current === false) {
    sawOwnTurnRef.current = true;
  }
  useEffect(() => {
    if (autoLoadedRef.current) return;
    // Case 1 — agent busy. Attach ONLY to a turn that was already running
    // when this surface opened, or one running in a different session.
    //
    // A turn that starts while we're watching is our own: the transcript
    // is live on screen and there is nothing to re-attach to. Loading it
    // anyway makes the engine replay STORED history over the top, and the
    // store holds only user/assistant/tool messages — so the streaming
    // thinking block and the per-turn `[tokens: …]` footer of that turn
    // were wiped the moment it finished. It looked like the usage footer
    // had been dropped, but only ever on the FIRST message of a session
    // (`autoLoadedRef` then disarms this for the rest of the mount).
    if (busyState.busy && busyState.sessionId) {
      if (busyAtOpenRef.current === null) return; // snapshot not in yet
      const ourOwnTurn =
        !busyAtOpenRef.current &&
        (!currentSessionIdRef.current ||
          currentSessionIdRef.current === busyState.sessionId);
      if (ourOwnTurn) {
        autoLoadedRef.current = true;
        return;
      }
      autoLoadedRef.current = true;
      send({ type: "session_load", id: busyState.sessionId });
      return;
    }
    // Case 2 — pick the most recent non-empty session from the list.
    // The engine sends sessions sorted most-recent-first (per
    // SessionStore::list ordering). Skip empty ones so a freshly-
    // spawned default session doesn't shadow a real prior session.
    //
    // Desktop is excluded: a fresh app launch should land on the clean
    // session the worker just minted, NOT silently inherit the previous
    // conversation's history (otherwise `session_load` runs the engine's
    // LoadSession → agent.set_history(previous), and the agent answers
    // "what were we talking about?" from the old chat even though the UI
    // shows a new session). Case 1 still reconnects to an actively-busy
    // agent. This auto-resume stays for the --serve/web surface, where
    // reopening a browser tab reconnects to a still-running engine.
    if (typeof window !== "undefined" && window.ipc) return;
    // Auto-resume is a TAB-OPEN behaviour. Once the user has taken a turn
    // here there is nothing to resume into, and the session list only
    // reaches us AFTER that first turn is saved — so this used to fire
    // right then, reloading the session we were already in. The reload
    // repaints from stored history, which drops the turn's thinking block
    // and its `[tokens: …]` footer: the reported "usage line disappears,
    // but only on the first message".
    if (sawOwnTurnRef.current) {
      autoLoadedRef.current = true;
      return;
    }
    if (!knownSessions.length) return;
    const target = knownSessions.find((s) => (s.messages ?? 0) > 0);
    if (!target) return;
    // Already in it — a load would only cost us the live transcript.
    if (
      currentSessionIdRef.current &&
      target.id === currentSessionIdRef.current
    ) {
      autoLoadedRef.current = true;
      return;
    }
    autoLoadedRef.current = true;
    send({ type: "session_load", id: target.id });
  }, [busyState.busy, busyState.sessionId, knownSessions]);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (!navigator.platform.startsWith("Mac")) return;
      if (!e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return;
      const key = e.key.toLowerCase();
      if (key !== "q" && key !== "w") return;
      e.preventDefault();
      e.stopImmediatePropagation();
      send({ type: "app_close" });
    };
    window.addEventListener("keydown", onKeyDown, { capture: true });
    return () =>
      window.removeEventListener("keydown", onKeyDown, { capture: true });
  }, []);

  // Global "stop the agent" hotkey: Cmd+. on macOS, Ctrl+. elsewhere.
  // Fires `shell_cancel` regardless of focus, so the user can abort a
  // running turn from Settings, the file picker, or anywhere else
  // without having to click back into Chat or Terminal first. Backend
  // request_cancel is idempotent — calling it when no turn is running
  // is a harmless no-op (cancel flag is reset before each new turn).
  // Convention borrowed from Xcode / Logic / Cursor where Cmd+. =
  // "stop whatever you're doing right now".
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const isMac = navigator.platform.startsWith("Mac");
      const modOk = isMac
        ? e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey
        : e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey;
      if (!modOk) return;
      if (e.key !== ".") return;
      e.preventDefault();
      e.stopImmediatePropagation();
      send({ type: "shell_cancel" });
    };
    window.addEventListener("keydown", onKeyDown, { capture: true });
    return () =>
      window.removeEventListener("keydown", onKeyDown, { capture: true });
  }, []);

  // ⌘⇧U / Ctrl⇧U — toggle full-screen UI tab. Mirrors the
  // `--serve --gui-shell <id>` experience (chrome-free, just the
  // shell) without restarting the server. Entering also forces the
  // active tab to "ui" so the toggle is meaningful from any tab; the
  // iframe stays mounted across the swap so the shell session
  // doesn't reset.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const isMac = navigator.platform.startsWith("Mac");
      const modOk = isMac
        ? e.metaKey && !e.ctrlKey && !e.altKey && e.shiftKey
        : e.ctrlKey && !e.metaKey && !e.altKey && e.shiftKey;
      if (!modOk) return;
      if (e.key.toLowerCase() !== "u") return;
      e.preventDefault();
      e.stopImmediatePropagation();
      setFullscreen((prev) => {
        if (!prev) setActiveTab("ui");
        return !prev;
      });
    };
    // Iframe focus would swallow the hotkey before parent sees it —
    // the gui-shell bridge re-emits matching ⌘⇧U presses as a
    // postMessage so this handler runs regardless of where focus lives.
    const onMessage = (e: MessageEvent) => {
      const data = e.data;
      if (!data || data.ns !== "thclaws-shell") return;
      // Shell declared it provides its own exit control → suppress the
      // host's fallback chip (toast + keyboard escape still apply).
      if (data.type === "ui" && data.key === "exit-control-claimed") {
        setExitControlClaimed(true);
        return;
      }
      // Shell asked to switch the app theme (its navbar theme toggle).
      if (data.type === "ui" && data.key === "set-theme") {
        if (data.mode === "light" || data.mode === "dark") setMode(data.mode);
        return;
      }
      if (data.type !== "hotkey") return;
      // Explicit exit (shell's own exit button) vs toggle (⌘⇧U).
      if (data.key === "exit-fullscreen-ui") {
        setFullscreen(false);
        return;
      }
      if (data.key !== "toggle-fullscreen-ui") return;
      setFullscreen((prev) => {
        if (!prev) setActiveTab("ui");
        return !prev;
      });
    };
    window.addEventListener("keydown", onKeyDown, { capture: true });
    window.addEventListener("message", onMessage);
    return () => {
      window.removeEventListener("keydown", onKeyDown, { capture: true });
      window.removeEventListener("message", onMessage);
    };
  }, [setMode]);

  const [started, setStarted] = useState(false);
  const [currentCwd, setCurrentCwd] = useState("");
  // Default tab is chat; backend overrides via `initial_tab` on
  // current_cwd when `guiShell.tabDefault` is set in settings.json so
  // the workspace lands on the GUI shell instead.
  const [activeTab, setActiveTab] = useState<Tab>("chat");
  useEffect(() => subscribe((msg) => {
    if (msg.type === "session_view_selected" && msg.team_agent) setActiveTab("chat");
  }), []);

  // Full-screen UI mode — hides tab strip, sidebar, status bar so the
  // GUI shell fills the viewport (the cloud equivalent of running
  // `thclaws --serve --gui-shell <id>`). Auto-enters when the backend
  // signals an initial UI tab; toggle with ⌘⇧U / Ctrl⇧U.
  const [fullscreen, setFullscreen] = useState(false);
  // Set when the active GUI shell declares (via
  // thclaws.ui.claimExitControl) that it renders its own exit control,
  // so the host suppresses its fallback chip. Reset on leaving
  // full-screen so a subsequent non-claiming shell gets the chip back.
  const [exitControlClaimed, setExitControlClaimed] = useState(false);
  // Drop the claim on leaving full-screen — the next shell (or the
  // next full-screen session) must re-declare it. The reference shell
  // re-claims in its `onFullscreen(active=true)` handler.
  useEffect(() => {
    if (!fullscreen) setExitControlClaimed(false);
  }, [fullscreen]);
  const [showSettings, setShowSettings] = useState(false);
  const [showSettingsMenu, setShowSettingsMenu] = useState(false);
  const [showLineConnect, setShowLineConnect] = useState(false);
  const [showTelegramConnect, setShowTelegramConnect] = useState(false);
  const [showMessengerConnect, setShowMessengerConnect] = useState(false);
  const [instructionsScope, setInstructionsScope] = useState<
    "global" | "folder" | null
  >(null);
  const closeInstructions = useCallback(() => setInstructionsScope(null), []);

  // M6.39.9: KMS browser + viewer state. `browsingKms` is the
  // KMS the user clicked the title of in the left sidebar — when
  // set, the right-edge `KmsBrowserSidebar` mounts. `viewerTarget`
  // is the file the user clicked inside the browser — when set,
  // `KmsViewerOverlay` mounts over the main pane. Both clear on
  // their respective close handlers.
  const [browsingKms, setBrowsingKms] = useState<string | null>(null);
  const [viewerTarget, setViewerTarget] = useState<ViewerTarget | null>(null);
  // M6.39.13: Obsidian-style graph view of the focused KMS. Mutually
  // exclusive with `viewerTarget` — opening one clears the other so
  // the main pane only ever shows one KMS surface at a time.
  const [graphKms, setGraphKms] = useState<string | null>(null);

  // Post-key-entry model picker (issue #13). Backend broadcasts
  // `model_picker_open` after a successful api_key_set when the
  // provider has a non-trivial catalogue. Clearing this state on
  // pick / Skip closes the modal.
  const [modelPicker, setModelPicker] = useState<{
    provider: string;
    current: string;
    models: PickerModel[];
  } | null>(null);
  const closeModelPicker = useCallback(() => setModelPicker(null), []);

  // A newer release exists. The backend decides that — it compares versions
  // numerically and stays quiet inside our container images — so this only
  // renders what it is told. Dismissed per version, so saying "later" once
  // doesn't hide the next release too.
  const [update, setUpdate] = useState<{ version: string; url: string } | null>(
    null,
  );
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type !== "update_available") return;
      const version = typeof msg.version === "string" ? msg.version : "";
      const url = typeof msg.url === "string" ? msg.url : "";
      if (!version || !url) return;
      let dismissed = "";
      try {
        dismissed = localStorage.getItem("thclaws.update.dismissed") ?? "";
      } catch {
        // Private window / blocked storage: show the notice rather than
        // swallow it.
      }
      if (dismissed === version) return;
      setUpdate({ version, url });
    });
    return unsub;
  }, []);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type !== "model_picker_open") return;
      const provider = typeof msg.provider === "string" ? msg.provider : "";
      const current = typeof msg.current === "string" ? msg.current : "";
      const models = Array.isArray(msg.models)
        ? (msg.models as PickerModel[])
        : [];
      if (provider && models.length > 0) {
        setModelPicker({ provider, current, models });
      }
    });
    return unsub;
  }, []);
  // Secrets-backend gate: we ask once at first launch so the app
  // never touches the OS keychain behind the user's back. `null` ==
  // not picked yet → show the chooser before the main UI.
  const [secretsBackend, setSecretsBackend] = useState<
    "keychain" | "dotenv" | "hosted" | null
  >(null);
  const [secretsBackendChecked, setSecretsBackendChecked] = useState(false);
  const settingsButtonRef = useRef<HTMLButtonElement | null>(null);

  // Ask the backend for the stored choice as soon as the app mounts.
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "secrets_backend") {
        const value = (msg.backend as string | null) ?? null;
        setSecretsBackend(
          value === "keychain" || value === "dotenv" || value === "hosted"
            ? value
            : null,
        );
        setSecretsBackendChecked(true);
      }
    });
    send({ type: "secrets_backend_get" });
    return unsub;
  }, []);

  // Desktop SSO sign-in button is gated by `ssoSignInEnabled` in
  // .thclaws/settings.json (default false → hidden). Read-only; there's no GUI
  // toggle while the sign-in feature is unfinished. Stays false until the
  // backend answers, so the button never flashes on before the gate is known.
  const [ssoSignInEnabled, setSsoSignInEnabled] = useState(false);
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "sso_sign_in_enabled") {
        setSsoSignInEnabled(msg.enabled === true);
      }
    });
    send({ type: "sso_sign_in_enabled_get" });
    return unsub;
  }, []);

  const [teamEnabled, setTeamEnabled] = useState(false);
  // Opt-in flag for the PTY-backed Shell tab. Default off — the tab
  // gives the user an unsandboxed live shell with no agent-side
  // permission gating, so it stays hidden until the project flips
  // `shellTabEnabled: true` in .thclaws/settings.json.
  const [shellTabEnabled, setShellTabEnabled] = useState(false);
  // Engine-managed Playwright browser (docs/browser Phase 1). The tab
  // only appears when `browserEnabled` is set in settings.json.
  const [browserEnabled, setBrowserEnabled] = useState(false);
  // Mobile-only: the sidebar is an off-canvas drawer below the `sm`
  // breakpoint (toggled by the hamburger in the tab bar). On `sm:`+ it's
  // the normal inline column, so this flag is ignored there.
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() => {
    try {
      return localStorage.getItem("thclaws_sidebar_collapsed") === "true";
    } catch {
      return false;
    }
  });
  useEffect(() => {
    try {
      localStorage.setItem("thclaws_sidebar_collapsed", String(sidebarCollapsed));
    } catch { /* Storage may be unavailable in a restricted webview. */ }
  }, [sidebarCollapsed]);


  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (
        (msg.type === "team_enabled" || msg.type === "team_enabled_result") &&
        typeof msg.enabled === "boolean"
      ) {
        setTeamEnabled(msg.enabled as boolean);
      } else if (
        (msg.type === "shell_tab_enabled" ||
          msg.type === "shell_tab_enabled_result") &&
        typeof msg.enabled === "boolean"
      ) {
        setShellTabEnabled(msg.enabled as boolean);
      } else if (
        msg.type === "browser_status" &&
        typeof msg.enabled === "boolean"
      ) {
        setBrowserEnabled(msg.enabled as boolean);
      } else if (msg.type === "settings_changed") {
        // Backend re-loaded .thclaws/settings.json (file watcher or
        // explicit `settings_reload` IPC). Re-fetch every settings-
        // derived flag so tab visibility + similar UI bits move
        // without a page refresh. Cheap — the responses come back
        // through this same subscribe above.
        send({ type: "team_enabled_get" });
        send({ type: "shell_tab_enabled_get" });
        send({ type: "browser_status_get" });
      } else if (msg.type === "initial_state") {
        // #95(c) + #168: the explicit `*_get` requests below race the WS
        // CONNECTING state on first mount in --serve mode — wsSend drops
        // the message if the socket isn't OPEN yet (far more likely over
        // a high-latency tunnel like ngrok, which hid these tabs there
        // but not on localhost). initial_state fires on every WS
        // (re)connect from the backend, so reading the tab-visibility
        // flags here self-heals the Team/Shell/Browser tabs regardless
        // of WS timing — no need to re-trigger the get via Settings.
        if (typeof msg.team_enabled === "boolean") {
          setTeamEnabled(msg.team_enabled as boolean);
        }
        if (typeof msg.shell_tab_enabled === "boolean") {
          setShellTabEnabled(msg.shell_tab_enabled as boolean);
        }
        if (typeof msg.browser_enabled === "boolean") {
          setBrowserEnabled(msg.browser_enabled as boolean);
        }
      }
    });
    send({ type: "team_enabled_get" });
    send({ type: "shell_tab_enabled_get" });
    send({ type: "browser_status_get" });
    return unsub;
  }, []);

  const modalOpen =
    showSettings || instructionsScope !== null || modelPicker !== null;
  const effectiveTab =
    !teamEnabled && activeTab === "team"
      ? ("chat" as Tab)
      : !shellTabEnabled && activeTab === "shell"
        ? ("chat" as Tab)
        : !browserEnabled && activeTab === "browser"
          ? ("chat" as Tab)
          : activeTab;

  let TABS = teamEnabled ? ALL_TABS : ALL_TABS.filter((t) => t.id !== "team");
  if (!shellTabEnabled) TABS = TABS.filter((t) => t.id !== "shell");
  if (!browserEnabled) TABS = TABS.filter((t) => t.id !== "browser");

  if (!started) {
    return (
      <>
        <StartupModal
          onStart={(cwd, initialTab) => {
            setCurrentCwd(cwd);
            if (initialTab) {
              setActiveTab(initialTab);
              // guiShell.tabDefault is pinned → enter full-screen UI
              // automatically so the workspace opens like a dedicated
              // gui-shell server (`thclaws --serve --gui-shell <id>`).
              // Toggle off any time with ⌘⇧U / Ctrl⇧U.
              if (initialTab === "ui") setFullscreen(true);
            }
            setStarted(true);
          }}
        />
        <ApprovalModal />
      </>
    );
  }

  // First launch only — after the user has picked a working directory
  // but before the main tabs mount, make them pick where API keys go.
  // This is the whole reason the app doesn't touch the keychain at
  // startup: no choice, no prompt.
  if (secretsBackendChecked && secretsBackend === null) {
    return (
      <>
        <SecretsBackendDialog
          onPicked={(choice) => setSecretsBackend(choice)}
        />
        <ApprovalModal />
      </>
    );
  }

  // Full-screen UI mode forces the UI tab regardless of which tab
  // the user last had active — the whole point is to hide chrome and
  // surface only the shell.
  const renderTab = fullscreen ? "ui" : effectiveTab;

  return (
    // h-[100dvh] (dynamic viewport height), not h-screen (100vh): on mobile
    // Chrome/Safari 100vh is the *large* viewport (address bar hidden), so
    // when the address bar is visible the container overshoots and the top
    // tab bar / bottom input get clipped behind the browser chrome. dvh
    // tracks the actual visible height as the bar shows/hides (issue #168;
    // Chrome 108+ / Safari 15.4+ — all current mobile devices).
    //
    // `fixed inset-x-0 top-0`: anchor to the viewport top so a stray document
    // scroll can't push the root off-screen. `overflow-clip` (NOT hidden):
    // an `overflow:hidden` box is still PROGRAMMATICALLY scrollable, so a
    // descendant `scrollIntoView` (the chat auto-scroll after a gui-shell tab
    // swap) could scroll this container down by the tab-bar height — the tab
    // bar then rendered above the viewport with an equal empty gap below
    // (navbar "gone"). `clip` makes the box unscrollable, so nothing can shift.
    // dev-plan/59 §6.3: `left` comes from `--rail-w`, which the workspace
    // shell sets when it renders the bot rail (0 when there is none). The
    // root has to stay `fixed` — see above — so it cannot simply live inside
    // the shell's flex row.
    <div className="fixed right-0 top-0 left-[var(--rail-w,0px)] flex flex-col h-[100dvh] overflow-clip">
      <FrontendReadyBeacon />
      {fullscreen && (
        <FullscreenExitChrome
          onExit={() => setFullscreen(false)}
          claimed={exitControlClaimed}
        />
      )}
      {/* Tab bar — hidden in full-screen UI mode */}
      {!fullscreen && (
        <div
          className="flex items-center gap-0 border-b select-none shrink-0"
          style={{
            background: "var(--bg-secondary)",
            borderColor: "var(--border)",
          }}
        >
          {/* Hamburger — opens the sidebar drawer on mobile only. */}
          <button
            onClick={() => setSidebarOpen((v) => !v)}
            className="sm:hidden flex items-center justify-center p-2 shrink-0"
            title="Menu"
            aria-label="Toggle sidebar"
            style={{ color: "var(--text-secondary)" }}
          >
            <Menu size={18} />
          </button>
          <button
            onClick={() => setSidebarCollapsed((collapsed) => !collapsed)}
            className="hidden sm:flex items-center justify-center p-2 shrink-0"
            title={sidebarCollapsed ? "Show sidebar" : "Hide sidebar"}
            aria-label={sidebarCollapsed ? "Show sidebar" : "Hide sidebar"}
            aria-expanded={!sidebarCollapsed}
            style={{ color: "var(--text-secondary)" }}
          >
            {sidebarCollapsed ? <PanelLeftOpen size={18} /> : <PanelLeftClose size={18} />}
          </button>
          {/* Tabs — horizontally scrollable when they don't fit (mobile);
            labels collapse to icons below `sm`. */}
          <div className="flex items-center overflow-x-auto no-scrollbar">
            {TABS.map((tab) => (
              <button
                key={tab.id}
                onClick={() => {
                  setActiveTab(tab.id);
                  // M6.39.12: switching tabs closes both the KMS viewer
                  // pane and the KMS browser sidebar — the user is moving
                  // back to "real work" (chat / terminal / files / team)
                  // and the KMS browse session is implicitly done.
                  setViewerTarget(null);
                  setBrowsingKms(null);
                  setGraphKms(null);
                }}
                className="flex items-center gap-1.5 px-3 sm:px-4 py-2.5 sm:py-2 text-xs font-medium transition-colors shrink-0"
                style={{
                  color:
                    effectiveTab === tab.id
                      ? "var(--text-primary)"
                      : "var(--text-secondary)",
                  background:
                    effectiveTab === tab.id
                      ? "var(--bg-primary)"
                      : "transparent",
                  borderBottom:
                    effectiveTab === tab.id
                      ? "2px solid var(--accent)"
                      : "2px solid transparent",
                }}
              >
                {tab.icon}
                <span className="hidden sm:inline">{tab.label}</span>
              </button>
            ))}
          </div>
          <div className="flex-1" />
          <RunningChip />
          <button
            onClick={() => {
              setActiveTab("ui");
              setFullscreen(true);
            }}
            className="flex items-center justify-center p-2 sm:p-1.5 mr-1 rounded hover:opacity-100 transition-opacity"
            title={`Full-screen UI (${navigator.platform.startsWith("Mac") ? "⌘⇧U" : "Ctrl⇧U"})`}
            style={{ color: "var(--text-secondary)", opacity: 0.7 }}
          >
            <Maximize2 size={14} />
          </button>
          {/* Sign-in button gated by `ssoSignInEnabled` in settings.json (default
            false) until the SSO feature is usable. Also stays hidden on any
            cloud-hosted workspace (gateway OR BYOK): the engine returns
            "hosted" from secrets_backend_get whenever THCLAWS_WORKSPACE_ID
            (or THCLAWS_GATEWAY_API_KEY) is set, so the visitor is already
            authenticated at the cloud-routing layer and a second SSO flow
            inside the workspace is just noise. */}
          {ssoSignInEnabled && secretsBackend !== "hosted" && <LoginButton />}
        </div>
      )}

      {/* Main content */}
      <div className="flex flex-1 min-h-0">
        {!fullscreen && (
          <>
            {/* Drawer backdrop (mobile only) — tap to dismiss. */}
            {sidebarOpen && (
              <div
                className="fixed inset-0 z-30 sm:hidden"
                style={{ background: "rgba(0,0,0,0.45)" }}
                onClick={() => setSidebarOpen(false)}
              />
            )}
            {/* Sidebar: off-canvas slide-in drawer below `sm`, normal
                inline column at `sm:`+. `flex` lets the inner Sidebar
                stretch to full height inside the fixed drawer. */}
            <div
              className={
                "flex z-40 max-sm:fixed max-sm:inset-y-0 max-sm:left-0 max-sm:shadow-2xl max-sm:transition-transform " +
                (sidebarCollapsed ? "sm:hidden " : "") +
                (sidebarOpen
                  ? "max-sm:translate-x-0"
                  : "max-sm:-translate-x-full")
              }
            >
              <Sidebar
                onBrowseKms={(name) => {
                  setBrowsingKms(name);
                  setSidebarOpen(false);
                }}
              />
            </div>
          </>
        )}
        <div className="flex-1 min-w-0 relative">
          {/* Keep every tab panel mounted AND full-sized via absolute+inset-0.
              Inactive panels get `invisible` + `pointer-events-none` so they
              don't receive input but keep their layout. This avoids
              `display: none` — which zeroes xterm's grid and kills focus,
              making the terminal un-typeable after a tab switch. */}
          {TABS.map(({ id }) => {
            const isActive = renderTab === id;
            // M6.39.9: when KMS viewer is open, hide tabs visually
            // (they stay mounted so xterm doesn't lose state) and
            // let the viewer's absolute-positioned pane cover them.
            const tabsHidden =
              !isActive || viewerTarget !== null || graphKms !== null;
            // `tab-inactive` additionally display:none's media elements —
            // WebKit's native <video>/<audio> controls (UA shadow DOM) set
            // their own visibility and pierce the ancestor `invisible`,
            // leaving a floating controls bar over other tabs (index.css).
            const cls = `absolute inset-0 ${tabsHidden ? "invisible pointer-events-none tab-inactive" : ""}`;
            return (
              <div key={id} className={cls}>
                {id === "terminal" && (
                  <TerminalView active={isActive} modalOpen={modalOpen} />
                )}
                {id === "chat" && (
                  <ChatView active={isActive} modalOpen={modalOpen} />
                )}
                {id === "files" && <FilesView active={isActive} />}
                {id === "team" && <TeamView />}
                {id === "ui" && (
                  <UITab active={isActive} fullscreen={fullscreen} />
                )}
                {id === "shell" && <ShellTab active={isActive} />}
                {id === "browser" && <BrowserView active={isActive} />}
              </div>
            );
          })}
          {/* KMS viewer pane (M6.39.9). When a file is open, mounts
              over the active tab inside the same flex-1 container so
              it feels like a tab swap rather than a modal. Tabs stay
              mounted underneath; close button returns the user to
              whichever tab they were on. */}
          {viewerTarget && (
            <KmsViewerOverlay
              initial={viewerTarget}
              onClose={() => setViewerTarget(null)}
            />
          )}
          {/* KMS graph view (M6.39.13). Obsidian-style force-directed
              visualization of pages + wikilinks. Stacks above the
              tabs; clicking a node opens the viewer overlay (which
              then sits on top of the graph). */}
          {graphKms && !viewerTarget && (
            <KmsGraphView
              kmsName={graphKms}
              onClose={() => setGraphKms(null)}
              onOpenFile={(target) => setViewerTarget(target)}
            />
          )}
        </div>
        {/* Goal-state sidebar (M6.29 Phase A). Compact 240px column
            mounted to the LEFT of the plan sidebar. Renders nothing
            when no /goal is active. Independent from plan-state — a
            session can carry both, one, or neither. */}
        <GoalSidebar />
        {/* Todo-list sidebar. Mirrors PlanSidebar's right-edge layout
            but displays the `TodoWrite` scratchpad — display-only, no
            action buttons. Hidden until the first `chat_todo_update`
            envelope lands; the worker hydrates from
            `.thclaws/todos.md` at boot so reopening a project shows
            the prior list immediately. */}
        <TodoSidebar />
        {/* Plan-mode sidebar (M1). Renders nothing when no plan is
            active — plan_state's broadcaster fires `chat_plan_update`
            with `null` to clear it on `/new` / `/load` of a plan-less
            session. Mounted on the right by design (Cowork pattern). */}
        <PlanSidebar />
        {/* Research sidebar (M6.39.5). Mirrors PlanSidebar's
            right-edge layout but shows /research pipeline progression
            verbosely — current phase, iteration progress, score
            history, phase log, accumulated source count. Renders
            nothing until at least one research job has been observed
            via `research_update`. */}
        <ResearchSidebar
          onOpenResult={(target) => {
            setGraphKms(null);
            setViewerTarget(target);
          }}
        />
        {/* Background-agents sidebar. Subscribes to
            `chat_side_channel_*` envelopes and shows currently-running
            side-channel agents (/dream, /translator, etc.) with live
            elapsed time. The inline chat bubble can scroll out of
            view during long runs; this sidebar is the persistent
            "is it still running?" answer. Renders nothing until at
            least one agent has been spawned in this session. */}
        <BackgroundAgentsSidebar />
        {/* KMS browser sidebar (M6.39.9). Activated by clicking a
            KMS row's title in the left sidebar. Lists pages +
            sources; click an entry to open the viewer overlay. */}
        {browsingKms && (
          <KmsBrowserSidebar
            kmsName={browsingKms}
            selected={viewerTarget}
            onClose={() => {
              // M6.39.12: closing the browser sidebar also closes the
              // viewer pane underneath. The user's focus has moved
              // away from this KMS — the viewer would just be
              // orphaned content with no visible browser to re-open
              // it from.
              setBrowsingKms(null);
              setViewerTarget(null);
              setGraphKms(null);
            }}
            onOpenFile={(target) => {
              setGraphKms(null);
              setViewerTarget(target);
            }}
            onOpenGraph={(name) => {
              setViewerTarget(null);
              setGraphKms((cur) => (cur === name ? null : name));
            }}
            graphActive={graphKms === browsingKms}
          />
        )}
      </div>

      {/* Status bar — hidden in full-screen UI mode */}
      {!fullscreen && (
        <div
          className="flex items-center gap-2 px-3 py-1.5 shrink-0 select-none border-t"
          style={{
            background: "var(--bg-secondary)",
            borderColor: "var(--border)",
            color: "var(--text-secondary)",
            fontSize: "12px",
            lineHeight: "16px",
          }}
        >
          <button
            onClick={() => {
              // Kill the current PTY so a fresh one spawns in the new dir.
              send({ type: "pty_kill" });
              setStarted(false);
              setCurrentCwd("");
            }}
            className="p-2 sm:p-1 rounded hover:bg-white/10 transition-colors"
            title="Change working directory"
            style={{ flexShrink: 0 }}
          >
            <Folder size={14} style={{ opacity: 0.7 }} />
          </button>
          <span className="truncate font-mono" title={currentCwd}>
            {currentCwd}
          </span>
          <div className="flex-1" />
          <div className="relative" style={{ flexShrink: 0 }}>
            <button
              ref={settingsButtonRef}
              onClick={() => setShowSettingsMenu((v) => !v)}
              className="p-2 sm:p-1 rounded hover:bg-white/10 transition-colors"
              title="Settings"
            >
              <Settings size={14} style={{ opacity: 0.7 }} />
            </button>
            {showSettingsMenu && (
              <SettingsMenu
                anchorRef={settingsButtonRef}
                onClose={() => setShowSettingsMenu(false)}
                onPick={(choice) => {
                  if (choice === "api-keys") setShowSettings(true);
                  else if (choice === "global-instructions")
                    setInstructionsScope("global");
                  else if (choice === "folder-instructions")
                    setInstructionsScope("folder");
                  else if (choice === "line-connect") setShowLineConnect(true);
                  else if (choice === "telegram-connect")
                    setShowTelegramConnect(true);
                  else if (choice === "messenger-connect")
                    setShowMessengerConnect(true);
                }}
              />
            )}
          </div>
        </div>
      )}

      {showSettings && <SettingsModal onClose={() => setShowSettings(false)} />}
      {showLineConnect && (
        <LineConnectModal onClose={() => setShowLineConnect(false)} />
      )}
      {showTelegramConnect && (
        <TelegramConnectModal onClose={() => setShowTelegramConnect(false)} />
      )}
      {showMessengerConnect && (
        <MessengerConnectModal onClose={() => setShowMessengerConnect(false)} />
      )}
      {instructionsScope && (
        <InstructionsEditorModal
          scope={instructionsScope}
          onClose={closeInstructions}
        />
      )}
      <ApprovalModal />
      <ScheduleAddModal />
      <AgentEditorModal />
      <MarketplaceModal />
      <ContextWarningBanner />
      {update && (
        <div
          className="fixed bottom-4 right-4 z-50 flex items-center gap-3 rounded-lg border px-4 py-3 text-sm shadow-lg"
          style={{
            background: "var(--bg-secondary)",
            borderColor: "var(--border)",
            color: "var(--text-primary)",
          }}
        >
          <span>
            thClaws <strong>{update.version}</strong> is available
          </span>
          <button
            type="button"
            className="rounded px-2 py-1 text-xs underline"
            // Opened through the vetted `open_external` IPC, not an <a href>:
            // a plain link navigates the webview itself, replacing the app
            // with a web page and stranding the user.
            onClick={() => send({ type: "open_external", url: update.url })}
          >
            What's new
          </button>
          <button
            type="button"
            className="rounded px-2 py-1 text-xs opacity-60 hover:opacity-100"
            aria-label="Dismiss update notice"
            onClick={() => {
              try {
                localStorage.setItem("thclaws.update.dismissed", update.version);
              } catch {
                // Storage blocked — the notice still goes away for this run,
                // it just comes back next launch. Better than not closing.
              }
              setUpdate(null);
            }}
          >
            Later
          </button>
        </div>
      )}
      {modelPicker && (
        <ModelPickerModal
          provider={modelPicker.provider}
          current={modelPicker.current}
          models={modelPicker.models}
          onClose={closeModelPicker}
        />
      )}
    </div>
  );
}
