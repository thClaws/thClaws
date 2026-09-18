import { useEffect, useRef, useState } from "react";
import { botQuery, send, subscribe } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";

// dev-plan/33 Tier 1: render a GUI Shell inside a sandboxed iframe.
// Marshals postMessage between the iframe and the IPC backend so the
// shell's `window.thclaws.*` bridge round-trips through window.ipc.
//
// Tier 1 binds every shell tab to the same session (id "tier1"); the
// shell shares Chat/Terminal's conversation. Per-shell session
// isolation lands in Tier 2 along with the picker.
//
// Tier 1 supported bridge messages from iframe:
//   { ns:"thclaws-shell", requestId, type:"run", payload:{prompt}, ... }
//   { ns:"thclaws-shell", requestId, type:"cancel", payload:{runId}, ... }
//   { ns:"thclaws-shell", type:"ready", ... }
//
// Backend dispatches forwarded to iframe:
//   gui_shell_event with replyTo  -> reply to a request
//   gui_shell_event with event    -> streamed event (text|done|error)

interface UIViewProps {
  active: boolean;
  shellId: string;
  /** Whether the host is currently showing this shell full-screen.
   * Forwarded into the iframe as a `fullscreen` bridge event so the
   * shell can render its own exit control (thclaws.ui.onFullscreen). */
  fullscreen?: boolean;
}

const TIER1_SESSION_ID = "tier1";

// Message types the bridge posts to the PARENT React app (not the
// backend): hotkey re-emissions and UI-integration signals. UIView
// must not forward these as `gui_shell_*` backend arms — App.tsx
// handles them directly on the window.
const PARENT_ONLY_TYPES = new Set(["ready", "hotkey", "ui"]);

export function UIView({ active, shellId, fullscreen = false }: UIViewProps) {
  const iframeRef = useRef<HTMLIFrameElement | null>(null);
  // Bumped by the reload button. Used as BOTH the iframe `key` and a
  // query param: the key remounts the element, and the changed URL stops
  // any cache layer between here and disk from answering with the copy
  // it already has. FilesView needed the same pair for the same reason.
  //
  // An on-disk shell is read fresh on every request (`ShellRef::OnDisk`
  // calls `std::fs::read`), so the stale content is always the webview's,
  // never the engine's — which is why this is a frontend-only fix.
  const [reloadNonce, setReloadNonce] = useState(0);
  // Resolved theme ("light" | "dark") of the main UI. Pushed into the
  // shell so it can match the app theme instead of hardcoding colors
  // (the bridge mirrors it onto the shell document's data-theme).
  const { resolved: theme } = useTheme();

  // Push the current full-screen state into the iframe. Kept in a ref-
  // free helper so both the fullscreen-change effect and the iframe's
  // "ready" handler can call it (a shell that loads while already
  // full-screen still gets its initial state).
  const sendFullscreen = (value: boolean) => {
    const target = iframeRef.current?.contentWindow;
    if (!target) return;
    target.postMessage(
      { ns: "thclaws-shell-event", event: "fullscreen", payload: { active: value } },
      "*",
    );
  };

  // Push the host theme into the iframe (same channel as fullscreen).
  const sendTheme = (value: "light" | "dark") => {
    const target = iframeRef.current?.contentWindow;
    if (!target) return;
    target.postMessage(
      { ns: "thclaws-shell-event", event: "theme", payload: { mode: value } },
      "*",
    );
  };

  useEffect(() => {
    // iframe -> parent: forward to backend.
    const onMessage = (e: MessageEvent) => {
      const data = e.data;
      if (
        !data ||
        data.ns !== "thclaws-shell" ||
        e.source !== iframeRef.current?.contentWindow
      ) {
        return;
      }
      if (data.type === "ready") {
        // Replay current full-screen state + theme to a freshly-loaded
        // shell so thclaws.ui.onFullscreen()/onTheme() fire with the
        // right initial values and the shell paints in the app theme.
        sendFullscreen(fullscreen);
        sendTheme(theme);
        return;
      }
      // Copy request from the shell (Cmd/Ctrl+C over a selection): write the
      // text via the native clipboard IPC. wry blocks navigator.clipboard
      // inside the shell iframe, so the bridge routes selection copy here.
      if (data.type === "clipboard-write") {
        const text = typeof data.text === "string" ? data.text : "";
        if (text) send({ type: "clipboard_write", text });
        return;
      }
      // Parent-only signals (hotkey / ui) are handled by App.tsx on the
      // window — never a backend arm.
      if (PARENT_ONLY_TYPES.has(data.type)) return;
      // type is "run" / "cancel" -> backend arms are gui_shell_run /
      // gui_shell_cancel.
      const payload = data.payload || {};
      send({
        type: `gui_shell_${data.type}`,
        id: data.requestId,
        sessionId: data.sessionId ?? TIER1_SESSION_ID,
        ...payload,
        // Bind privileged calls to this iframe, never its supplied identity.
        shellId,
      });
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [shellId, fullscreen, theme]);

  // Forward full-screen state changes into the iframe.
  useEffect(() => {
    sendFullscreen(fullscreen);
  }, [fullscreen]);

  // Forward theme changes into the iframe so the shell re-themes live
  // when the user switches Light/Dark/System in the main UI.
  useEffect(() => {
    sendTheme(theme);
  }, [theme]);

  useEffect(() => {
    // backend -> iframe: forward gui_shell_event dispatches.
    const unsub = subscribe((msg) => {
      const target = iframeRef.current?.contentWindow;
      if (!target) return;
      if (msg?.type === "gui_shell_event") {
        if (msg.shellId && msg.shellId !== shellId) return;
        // Tier 1: no sessionId filtering — single shared session, every
        // active shell tab gets every event. Tier 2 adds per-tab session
        // ids and we filter here.
        target.postMessage({ ns: "thclaws-shell-event", ...msg }, "*");
      } else if (msg?.type === "provider_update") {
        // Re-emit model changes (from the sidebar, /model, etc.) into the
        // shell as a `model` bridge event so thclaws.model.onChange fires.
        target.postMessage(
          {
            ns: "thclaws-shell-event",
            event: "model",
            payload: { provider: msg.provider, model: msg.model },
          },
          "*",
        );
      }
    });
    return unsub;
  }, [shellId]);

  // active is unused in Tier 1 — the iframe stays mounted whether or
  // not the tab is visible (cheap) so re-activating the tab doesn't
  // re-run the shell's initial agent prompt.
  void active;

  // Mode A (desktop wry): `thclaws://localhost/...` — the protocol
  // handler intercepts and injects the bridge script.
  // Mode C (cloud `--serve` over http(s)): browsers have no
  // `thclaws://` handler, so use a RELATIVE path. The iframe's URL
  // resolves under the same traefik-stripped prefix as the parent
  // workspace URL, so `gui-shell/<id>/...` lands on the engine's
  // `/gui-shell/<id>/...` route regardless of how many path prefixes
  // the reverse proxy peels off.
  const isHttp =
    typeof window !== "undefined" &&
    (window.location.protocol === "http:" || window.location.protocol === "https:");
  // Under a workspace host the `bot=` picks whose shell this is; the
  // shell's own sub-assets inherit it through the Referer.
  // `r` only ever changes when the reload button is pressed, so a normal
  // render keeps the URL byte-identical and the shell is not disturbed.
  const bust = reloadNonce > 0 ? `&r=${reloadNonce}` : "";
  const src = isHttp
    ? `gui-shell/${encodeURIComponent(shellId)}/?session=${encodeURIComponent(TIER1_SESSION_ID)}${botQuery().replace(/^\?/, "&")}${bust}`
    : `thclaws://localhost/gui-shell/${encodeURIComponent(shellId)}/index.html` +
      `?session=${encodeURIComponent(TIER1_SESSION_ID)}${bust}`;

  return (
    <div className="relative w-full h-full">
      <iframe
        // Remount on reload. Deliberate, and the reason this is a button
        // rather than something automatic: a remount re-runs the shell's
        // initial agent prompt, which is exactly why the iframe is
        // otherwise left mounted across tab switches (see above). That is
        // wanted when someone asks for a reload, and unwanted every other
        // time.
        key={reloadNonce}
        ref={iframeRef}
        src={src}
        title={`GUI Shell: ${shellId}`}
        sandbox="allow-scripts allow-same-origin"
        className="w-full h-full border-0"
        style={{ display: "block", background: "transparent" }}
      />
      <button
        type="button"
        onClick={() => setReloadNonce((n) => n + 1)}
        title="Reload this shell from disk"
        aria-label="Reload this shell from disk"
        // Faint until pointed at: this is a development affordance sitting
        // on top of someone's finished UI, so it should not compete with
        // the shell's own controls.
        className="absolute top-2 right-2 z-10 rounded px-2 py-1 text-xs opacity-25 hover:opacity-100 transition-opacity"
        style={{
          background: "var(--bg-secondary)",
          color: "var(--text-primary)",
          border: "1px solid var(--border)",
        }}
      >
        ⟳
      </button>
    </div>
  );
}
