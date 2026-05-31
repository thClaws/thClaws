import { useEffect, useRef } from "react";
import { send, subscribe } from "../hooks/useIPC";

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

interface ShellViewProps {
  active: boolean;
  shellId: string;
}

const TIER1_SESSION_ID = "tier1";

export function ShellView({ active, shellId }: ShellViewProps) {
  const iframeRef = useRef<HTMLIFrameElement | null>(null);

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
        // No-op for now; Tier 2 picker uses this to dismiss the
        // "loading shell..." spinner. Logged for dev visibility.
        return;
      }
      // type is "run" / "cancel" -> backend arms are gui_shell_run /
      // gui_shell_cancel.
      const payload = data.payload || {};
      send({
        type: `gui_shell_${data.type}`,
        id: data.requestId,
        sessionId: data.sessionId ?? TIER1_SESSION_ID,
        shellId: data.shellId ?? shellId,
        ...payload,
      });
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [shellId]);

  useEffect(() => {
    // backend -> iframe: forward gui_shell_event dispatches.
    const unsub = subscribe((msg: any) => {
      if (msg?.type !== "gui_shell_event") return;
      // Tier 1: no sessionId filtering — single shared session, every
      // active shell tab gets every event. Tier 2 adds per-tab session
      // ids and we filter here.
      const target = iframeRef.current?.contentWindow;
      if (!target) return;
      target.postMessage({ ns: "thclaws-shell-event", ...msg }, "*");
    });
    return unsub;
  }, []);

  // active is unused in Tier 1 — the iframe stays mounted whether or
  // not the tab is visible (cheap) so re-activating the tab doesn't
  // re-run the shell's initial agent prompt.
  void active;

  const src =
    `thclaws://localhost/gui-shell/${encodeURIComponent(shellId)}/index.html` +
    `?session=${encodeURIComponent(TIER1_SESSION_ID)}`;

  return (
    <iframe
      ref={iframeRef}
      src={src}
      title={`GUI Shell: ${shellId}`}
      sandbox="allow-scripts allow-same-origin"
      className="w-full h-full border-0"
      style={{ display: "block", background: "transparent" }}
    />
  );
}
