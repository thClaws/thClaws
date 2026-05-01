/**
 * Inline + lifted MCP-Apps widget host.
 *
 * MCP-Apps (text/html;profile=mcp-app resources at ui:// URIs) speak MCP
 * over postMessage: the widget acts as an MCP client, this component as
 * the MCP server. The widget loads the @modelcontextprotocol/ext-apps
 * SDK, calls `app.connect()` (sends a `ui/initialize` request), and
 * registers `app.ontoolresult = …`. Once init is acknowledged, we push
 * the tool result via `ui/notifications/tool-result` so the widget's
 * callback fires.
 *
 * Display modes:
 *   inline      — embedded in the chat bubble (default)
 *   fullscreen  — full-viewport overlay with persistent top toolbar
 *   pip         — floating draggable panel; chat stays interactive
 *
 * Mode changes are SYMMETRIC: the user clicking our toolbar and the
 * widget calling `app.requestDisplayMode({mode})` go through the same
 * `setMode` path, so the widget always sees a `host-context-changed`
 * notification with the new mode regardless of who initiated.
 *
 * ## DOM stability across mode changes
 *
 * Naively re-rendering the iframe in a different parent (via
 * `createPortal` whose target changes) would tear the iframe out and
 * re-mount it — re-running the SDK handshake, re-fetching unpkg, and
 * losing the tool-result push. To keep the iframe alive across mode
 * lifts we render it once into a stable detached `<div>` and use
 * `appendChild` to MOVE that div between mount points (inline slot in
 * the bubble vs lifted slot in the fullscreen overlay or PIP panel).
 * `appendChild` of an attached node moves it without recreating the
 * underlying element, so the iframe's `contentWindow` and message
 * listeners stay intact.
 *
 * Protocol = JSON-RPC 2.0:
 *   request:      {jsonrpc:"2.0", id, method, params}
 *   response:     {jsonrpc:"2.0", id, result|error}
 *   notification: {jsonrpc:"2.0", method, params}            (no id)
 */

import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import {
  Maximize2,
  Minimize2,
  PictureInPicture2,
  X,
  GripHorizontal,
} from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";

type ToolResultContent = {
  content: unknown[];
  isError?: boolean;
  _meta?: Record<string, unknown>;
};

type DisplayMode = "inline" | "fullscreen" | "pip";

type Props = {
  /// `ui://server/widget` URI — used as a sessionStorage key for PIP
  /// position so multiple widgets remember their own placement.
  uri: string;
  /// The widget HTML returned by `resources/read`. Mounted via `srcdoc`.
  html: string;
  /// Qualified name of the parent tool whose result loaded this
  /// widget (e.g. `pinn_ai__text2image`). The server prefix
  /// (everything before the first `__`) identifies the originating
  /// MCP server — widget→host `tools/call` requests are constrained
  /// to tools on that same server, so we extract the prefix here
  /// and rebuild qualified names from `<prefix>__<bare-name>`.
  parentToolName: string;
  /// The tool's result content blocks (text, image, etc.). Pushed to
  /// the widget after init handshake completes so its `ontoolresult`
  /// fires.
  toolResult: ToolResultContent;
};

type JsonRpcMessage = {
  jsonrpc?: "2.0";
  id?: number | string | null;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: { code: number; message: string };
};

const HOST_INFO = { name: "thClaws", version: "0.7.1" };
const PROTOCOL_VERSION = "0.4.0";
const AVAILABLE_MODES: DisplayMode[] = ["inline", "fullscreen", "pip"];
/// Fixed inline-mode iframe height. Honouring widget-driven size
/// changes turned out fragile in practice — pinn.ai's image viewer
/// reports its initial spinner-state size before the image has
/// loaded and the post-load update arrives unreliably across mode
/// lifts. A fixed 480px gives every widget a predictable canvas;
/// content that needs more room can lift to fullscreen / PIP.
const INLINE_HEIGHT = 480;

const PIP_DEFAULT_W = 360;
const PIP_DEFAULT_H = 260;
const PIP_MARGIN = 16;

type PipRect = { x: number; y: number; w: number; h: number };

function defaultPipRect(): PipRect {
  // Bottom-right of viewport with a 16px margin. Fallbacks for SSR
  // (innerWidth/Height undefined) shouldn't fire — wry is always
  // a real DOM — but keep them tidy anyway.
  const vw = typeof window !== "undefined" ? window.innerWidth : 1024;
  const vh = typeof window !== "undefined" ? window.innerHeight : 768;
  return {
    x: Math.max(PIP_MARGIN, vw - PIP_DEFAULT_W - PIP_MARGIN),
    y: Math.max(PIP_MARGIN, vh - PIP_DEFAULT_H - PIP_MARGIN),
    w: PIP_DEFAULT_W,
    h: PIP_DEFAULT_H,
  };
}

function loadPipRect(uri: string): PipRect {
  try {
    const raw = sessionStorage.getItem(`mcpapp:pip:${uri}`);
    if (!raw) return defaultPipRect();
    const parsed = JSON.parse(raw) as Partial<PipRect>;
    if (
      typeof parsed.x === "number" &&
      typeof parsed.y === "number" &&
      typeof parsed.w === "number" &&
      typeof parsed.h === "number"
    ) {
      return parsed as PipRect;
    }
  } catch {
    /* fall through */
  }
  return defaultPipRect();
}

function savePipRect(uri: string, rect: PipRect) {
  try {
    sessionStorage.setItem(`mcpapp:pip:${uri}`, JSON.stringify(rect));
  } catch {
    /* sessionStorage can throw in private mode; non-fatal */
  }
}

export function McpAppIframe({
  uri,
  html,
  parentToolName,
  toolResult,
}: Props) {
  const [mode, setMode] = useState<DisplayMode>("inline");
  const [pipRect, setPipRect] = useState<PipRect>(() => loadPipRect(uri));
  const { resolved: themeMode } = useTheme();

  const iframeRef = useRef<HTMLIFrameElement | null>(null);
  // Mirror mode/theme into refs so the message handler reads the
  // latest values without re-binding. Re-binding the listener on
  // every mode change would create a window where iframe-→host
  // messages (e.g. an `initialized` notification after a WebKit
  // iframe reload) get dropped.
  const modeRef = useRef<DisplayMode>("inline");
  // One ref per surface so transitions don't fight ref semantics —
  // each surface owns its own slot div regardless of `mode`. The
  // effect below picks whichever ref matches the current mode and
  // moves iframeContainer there.
  const inlineSlotRef = useRef<HTMLDivElement | null>(null);
  const fullscreenSlotRef = useRef<HTMLDivElement | null>(null);
  const pipSlotRef = useRef<HTMLDivElement | null>(null);

  // Stable detached container that holds the iframe across mode lifts.
  // Created exactly once per component instance — `useState`'s lazy
  // initializer guarantees React doesn't churn it on re-renders.
  const [iframeContainer] = useState(() => {
    const el = document.createElement("div");
    el.style.cssText =
      "width:100%;height:100%;display:flex;flex-direction:column;min-height:0;";
    return el;
  });

  const stableResult = useMemo(() => toolResult, [toolResult]);
  const themeRef = useRef(themeMode);
  useEffect(() => {
    modeRef.current = mode;
  }, [mode]);
  useEffect(() => {
    themeRef.current = themeMode;
  }, [themeMode]);

  // Server prefix for widget→host tool-call routing. Pinn.ai's
  // `pinn_ai__text2image` → `pinn_ai`. The widget calls
  // `app.callServerTool({name: "image2image"})` and we resolve to
  // `pinn_ai__image2image` to look up in the agent's tool registry.
  // If the parent tool name doesn't have a separator we fall back to
  // the empty prefix; the call will then fail with "unknown tool"
  // server-side, which is the right error.
  const serverPrefix = useMemo(() => {
    const idx = parentToolName.indexOf("__");
    return idx > 0 ? parentToolName.slice(0, idx) : "";
  }, [parentToolName]);

  // Pending widget→host tool calls. Keyed by requestId (UUID we
  // generate), each entry resolves the JSON-RPC reply to the iframe
  // when the matching `mcp_call_tool_result` IPC arrives. Using
  // useRef so the Map identity stays stable across renders — if it
  // re-created we'd lose in-flight pending calls.
  type Pending = {
    iframeMessageId: number | string;
    timeoutId: number;
  };
  const pendingCallsRef = useRef<Map<string, Pending>>(new Map());

  // Move the iframe container into whichever slot matches the current
  // mode. `appendChild` of an already-attached node MOVES it (DOM
  // spec) without re-running the iframe's load, so the SDK handshake
  // and the iframe's contentWindow survive the lift.
  useEffect(() => {
    const target =
      mode === "inline"
        ? inlineSlotRef.current
        : mode === "fullscreen"
          ? fullscreenSlotRef.current
          : pipSlotRef.current;
    if (target && iframeContainer.parentElement !== target) {
      target.appendChild(iframeContainer);
    }
  }, [mode, iframeContainer]);

  // Persist PIP rect on every change so a re-render or remount
  // doesn't snap the panel back to the default corner.
  useEffect(() => {
    if (mode === "pip") savePipRect(uri, pipRect);
  }, [mode, pipRect, uri]);

  // Mode change → notify widget so it can re-layout. We post even if
  // the widget hasn't finished init yet; if it's not listening yet
  // (e.g. mid-reload during a mode lift), the next `initialize`
  // response carries the new mode in hostContext.displayMode and
  // the widget catches up that way.
  useEffect(() => {
    iframeRef.current?.contentWindow?.postMessage(
      {
        jsonrpc: "2.0",
        method: "ui/notifications/host-context-changed",
        params: {
          theme: themeMode,
          locale: navigator.language || "en-US",
          displayMode: mode,
          availableDisplayModes: AVAILABLE_MODES,
        },
      },
      "*",
    );
  }, [mode, themeMode]);

  // postMessage host loop. Bound once per `stableResult` change — i.e.
  // once per widget instance. The handler doesn't reference `mode` so
  // we don't have to rebind on mode changes.
  useEffect(() => {
    const iframe = iframeRef.current;
    if (!iframe) return;

    const post = (msg: object) => {
      // `*` is correct for srcdoc opaque origins — they don't have a
      // meaningful origin string the parent can match against, and
      // pinn.ai's widgets per their README do no origin validation.
      iframe.contentWindow?.postMessage(msg, "*");
    };

    const sendNotification = (method: string, params: unknown) =>
      post({ jsonrpc: "2.0", method, params });

    const respond = (id: number | string, result: unknown) =>
      post({ jsonrpc: "2.0", id, result });

    const respondError = (
      id: number | string,
      code: number,
      message: string,
    ) => post({ jsonrpc: "2.0", id, error: { code, message } });

    const onMessage = (event: MessageEvent) => {
      // Hard-bind to this iframe so a sibling McpAppIframe (or any
      // other postMessage in the page) doesn't cross-talk.
      if (event.source !== iframe.contentWindow) return;
      const msg = event.data as JsonRpcMessage | undefined;
      if (!msg || msg.jsonrpc !== "2.0") return;

      const isRequest =
        typeof msg.method === "string" &&
        msg.id !== undefined &&
        msg.id !== null;
      const isNotification =
        typeof msg.method === "string" &&
        (msg.id === undefined || msg.id === null);

      if (isRequest) {
        const id = msg.id as number | string;
        switch (msg.method) {
          case "ui/initialize": {
            // Read mode/theme through refs — the iframe may be
            // re-handshaking after a WebKit reload triggered by a
            // mode-lift parent move, in which case `mode` here
            // needs to be the *current* mode, not the one captured
            // when the listener was first bound. hostContext.
            // displayMode tells the widget which surface it's
            // rendering into without it having to track that itself.
            respond(id, {
              protocolVersion: PROTOCOL_VERSION,
              hostInfo: HOST_INFO,
              // McpUiHostCapabilities uses empty-object flags (NOT
              // booleans) — `{ serverTools: {} }` means "this host
              // implements tools/call". A truthy non-object value
              // (e.g. `true`) fails the SDK's Zod schema on the
              // widget side, causing app.connect() to throw silently
              // and stranding the widget in its spinner state.
              // openLinks is set because we honour ui/open-link via
              // the open_external IPC.
              hostCapabilities: { serverTools: {}, openLinks: {} },
              hostContext: {
                theme: themeRef.current,
                locale: navigator.language || "en-US",
                displayMode: modeRef.current,
                availableDisplayModes: AVAILABLE_MODES,
              },
            });
            break;
          }
          case "ui/open-link": {
            const params = msg.params as { url?: string } | undefined;
            const url = params?.url ?? "";
            if (url) send({ type: "open_external", url });
            respond(id, {});
            break;
          }
          case "ui/request-display-mode": {
            // Widget-initiated mode change. Symmetric with the user
            // clicking our toolbar — both routes flow through
            // setMode, which fires the host-context-changed effect
            // above. We reply with the actual mode set; for now we
            // honour every requested mode since all three are in
            // AVAILABLE_MODES, but a future host might constrain.
            const params = msg.params as { mode?: string } | undefined;
            const requested = params?.mode;
            if (
              requested === "inline" ||
              requested === "fullscreen" ||
              requested === "pip"
            ) {
              setMode(requested);
              respond(id, { mode: requested });
            } else {
              respondError(
                id,
                -32602,
                `Unsupported display mode: ${requested}`,
              );
            }
            break;
          }
          case "tools/call": {
            // Widget calling a tool on its originating MCP server
            // (app.callServerTool). Trust gate already applied at
            // widget render time — a non-trusted server would never
            // have shipped a `ui_resource` so the widget wouldn't
            // exist. Build the qualified tool name from the parent
            // tool's server prefix and the bare name the widget
            // requested, forward to Rust via IPC, register a pending
            // resolver keyed by requestId. The reply arrives via
            // the `mcp_call_tool_result` subscribe handler above.
            const params = msg.params as
              | { name?: string; arguments?: unknown }
              | undefined;
            const bareName = params?.name ?? "";
            const args = params?.arguments ?? {};
            if (!bareName) {
              respondError(id, -32602, "tools/call: missing 'name'");
              break;
            }
            if (!serverPrefix) {
              respondError(
                id,
                -32603,
                "tools/call: cannot determine originating server",
              );
              break;
            }
            const qualifiedName = `${serverPrefix}__${bareName}`;
            const requestId =
              typeof crypto?.randomUUID === "function"
                ? crypto.randomUUID()
                : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
            // 60s timeout — generative tools (image2image) routinely
            // run for tens of seconds. Anything longer is a stuck
            // call we should fail loudly rather than wait on.
            const timeoutId = window.setTimeout(() => {
              const stale = pendingCallsRef.current.get(requestId);
              if (!stale) return;
              pendingCallsRef.current.delete(requestId);
              respondError(stale.iframeMessageId, -32000, "tools/call: timed out after 60s");
            }, 60_000);
            pendingCallsRef.current.set(requestId, {
              iframeMessageId: id,
              timeoutId,
            });
            send({
              type: "mcp_call_tool",
              requestId,
              qualifiedName,
              arguments: args,
            });
            // Don't `respond` here — the resolver fires when Rust
            // dispatches `mcp_call_tool_result` back.
            break;
          }
          case "ui/message": {
            // Widget injecting a chat message (app.sendMessage).
            // Phase 1: extract text from content blocks and route
            // through the same `chat_user_message` IPC the chat
            // composer uses. Multi-block / image content blocks are
            // flattened to text — image attachment via this path
            // can be added later if a widget actually needs it.
            const params = msg.params as
              | { role?: string; content?: Array<{ type?: string; text?: string }> }
              | undefined;
            const blocks = params?.content ?? [];
            const text = blocks
              .filter((b) => b?.type === "text")
              .map((b) => b?.text ?? "")
              .join("");
            if (text.trim()) {
              send({ type: "chat_user_message", text });
              respond(id, { isError: false });
            } else {
              respond(id, {
                isError: true,
                content: [
                  { type: "text", text: "ui/message: no text content to inject" },
                ],
              });
            }
            break;
          }
          case "ui/update-model-context":
            // Not yet supported. Pinn.ai widgets don't currently
            // call this, but if a future widget does we should
            // either persist the context for the next agent turn or
            // surface it as a system message. method-not-found
            // until that design lands.
            respondError(id, -32601, `${msg.method} not supported by host`);
            break;
          default:
            respondError(id, -32601, `Unknown method: ${msg.method}`);
            break;
        }
      } else if (isNotification) {
        switch (msg.method) {
          case "ui/notifications/initialized": {
            // Always re-push tool-result on every `initialized`
            // notification — not just the first one. WebKit reloads
            // the iframe when its DOM ancestry changes (mode lift),
            // and the widget's SDK handshakes again from scratch. A
            // one-shot latch here would leave the post-reload widget
            // showing its initial "loading" state forever. pinn.ai's
            // widgets per their README are idempotent in
            // `ontoolresult` (they just set img.src etc.), so
            // re-pushing is safe.
            sendNotification("ui/notifications/tool-result", {
              content: stableResult.content,
              isError: stableResult.isError ?? false,
              _meta: stableResult._meta,
            });
            break;
          }
          // ui/notifications/size-changed deliberately not handled
          // — inline iframe height is fixed at INLINE_HEIGHT, and
          // fullscreen / PIP have their own geometry. Falling
          // through to the default arm is the JSON-RPC-correct
          // response for an unhandled notification (silent drop).
          default:
            break;
        }
      }
    };

    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
    // themeMode is in deps so the init response uses the current theme;
    // a re-bind is fine because we only re-bind when theme changes.
  }, [stableResult, themeMode]);

  // Clear any pending widget tool-call timers on unmount. Without
  // this, a 60s timeout could fire after the iframe is gone and
  // attempt to post to a freed contentWindow.
  useEffect(() => {
    const pending = pendingCallsRef.current;
    return () => {
      for (const entry of pending.values()) {
        window.clearTimeout(entry.timeoutId);
      }
      pending.clear();
    };
  }, []);

  // Subscribe to widget→host tool-call results from Rust. The IPC
  // dispatch is broadcast to all McpAppIframe instances; we match by
  // requestId and ignore anything else. Iframe message id was stored
  // in the Pending entry when the widget made the call, so we can
  // re-correlate the JSON-RPC reply to the right widget-side promise.
  useEffect(() => {
    return subscribe((msg) => {
      if (msg.type !== "mcp_call_tool_result") return;
      const requestId = msg.requestId as string | undefined;
      if (!requestId) return;
      const pending = pendingCallsRef.current.get(requestId);
      if (!pending) return;
      pendingCallsRef.current.delete(requestId);
      window.clearTimeout(pending.timeoutId);
      const result = {
        content: msg.content ?? [],
        isError: Boolean(msg.isError),
      };
      iframeRef.current?.contentWindow?.postMessage(
        {
          jsonrpc: "2.0",
          id: pending.iframeMessageId,
          result,
        },
        "*",
      );
    });
  }, []);

  // The actual iframe element — rendered ONCE into the stable
  // detached container via portal. Never re-mounted after first
  // render, regardless of mode.
  const iframeNode = createPortal(
    <iframe
      ref={iframeRef}
      srcDoc={html}
      title={`MCP App: ${uri}`}
      // `allow-scripts` is required for the SDK to run; combining it
      // with `allow-same-origin` would defeat the srcdoc origin
      // isolation, so we don't. The widget can still postMessage to
      // the parent and fetch its declared resourceDomains.
      sandbox="allow-scripts allow-popups allow-forms"
      style={{
        display: "block",
        flex: "1 1 auto",
        width: "100%",
        border: "none",
        background: "transparent",
        minHeight: 0,
      }}
    />,
    iframeContainer,
  );

  return (
    <>
      {iframeNode}

      {/* All three surfaces are ALWAYS mounted; only their visibility
          changes with the mode. WebKit reloads detached iframes when
          they re-attach (an old WK quirk shipping in wry too), so
          unmounting the lifted surfaces would kill the SDK handshake
          every time the user toggles modes. Display-none keeps them
          in the DOM at zero pixels and the iframeContainer stays put. */}
      <InlineSurface
        slotRef={inlineSlotRef}
        active={mode === "inline"}
        height={INLINE_HEIGHT}
        onFullscreen={() => setMode("fullscreen")}
        onPip={() => setMode("pip")}
      />

      {/* Both lifted surfaces portal to document.body so they escape
          the chat's overflow:auto. They stay mounted across mode
          changes and use display:none when inactive. */}
      {createPortal(
        <FullscreenSurface
          slotRef={fullscreenSlotRef}
          active={mode === "fullscreen"}
          onInline={() => setMode("inline")}
          onPip={() => setMode("pip")}
        />,
        document.body,
      )}
      {createPortal(
        <PipSurface
          slotRef={pipSlotRef}
          active={mode === "pip"}
          rect={pipRect}
          onRectChange={setPipRect}
          onInline={() => setMode("inline")}
          onFullscreen={() => setMode("fullscreen")}
        />,
        document.body,
      )}

      {/* Bubble stub — replaces the inline iframe area while the
          widget is lifted, so the user has an anchor to find their
          way back. */}
      {mode !== "inline" && (
        <BubbleStub mode={mode} onRestore={() => setMode("inline")} />
      )}
    </>
  );
}

// ── Inline (bubble) surface ─────────────────────────────────────────

function InlineSurface({
  slotRef,
  active,
  height,
  onFullscreen,
  onPip,
}: {
  slotRef: React.RefObject<HTMLDivElement | null>;
  active: boolean;
  height: number;
  onFullscreen: () => void;
  onPip: () => void;
}) {
  return (
    <div
      // Hidden when the iframe is lifted out, but kept mounted so the
      // slotRef stays valid for re-attach when mode flips back.
      style={{
        marginTop: 8,
        borderRadius: 6,
        overflow: "hidden",
        border: "1px solid var(--border)",
        background: "var(--bg-primary)",
        position: "relative",
        height,
        display: active ? "block" : "none",
      }}
      className="group"
    >
      <div
        ref={slotRef}
        style={{ width: "100%", height: "100%", display: "flex" }}
      />
      <ModeToolbar
        position="top-right"
        floating
        items={[
          {
            icon: <Maximize2 size={14} />,
            title: "Fullscreen",
            onClick: onFullscreen,
          },
          {
            icon: <PictureInPicture2 size={14} />,
            title: "Picture-in-picture",
            onClick: onPip,
          },
        ]}
      />
    </div>
  );
}

// ── Fullscreen surface ──────────────────────────────────────────────

function FullscreenSurface({
  slotRef,
  active,
  onInline,
  onPip,
}: {
  slotRef: React.RefObject<HTMLDivElement | null>;
  active: boolean;
  onInline: () => void;
  onPip: () => void;
}) {
  // Esc → back to inline. Bound only while active so we don't steal
  // the key from other surfaces / modals when the widget isn't on
  // screen.
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onInline();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, onInline]);

  return (
    <div
      role="dialog"
      aria-modal={active}
      aria-hidden={!active}
      aria-label="MCP App fullscreen"
      // z-[55] sits above the chat scroll area but below the approval
      // modal (z-[60]) so a tool-approval prompt can still surface.
      className="fixed inset-0 z-[55] flex flex-col"
      style={{
        background: "var(--bg-primary)",
        display: active ? "flex" : "none",
      }}
    >
      <div
        className="flex items-center justify-between px-3 py-2 border-b"
        style={{
          borderColor: "var(--border)",
          background: "var(--bg-secondary)",
          color: "var(--text-primary)",
          fontSize: 13,
        }}
      >
        <button
          type="button"
          onClick={onInline}
          className="px-2 py-1 rounded inline-flex items-center gap-1.5 text-xs"
          style={{
            background: "transparent",
            color: "var(--text-primary)",
            border: "1px solid var(--border)",
          }}
        >
          ← Back to chat
        </button>
        <div className="flex items-center gap-1">
          <ToolbarButton
            icon={<PictureInPicture2 size={14} />}
            title="Picture-in-picture"
            onClick={onPip}
          />
          <ToolbarButton
            icon={<X size={14} />}
            title="Close (Esc)"
            onClick={onInline}
          />
        </div>
      </div>
      <div ref={slotRef} style={{ flex: "1 1 auto", display: "flex", minHeight: 0 }} />
    </div>
  );
}

// ── PIP surface ─────────────────────────────────────────────────────

function PipSurface({
  slotRef,
  active,
  rect,
  onRectChange,
  onInline,
  onFullscreen,
}: {
  slotRef: React.RefObject<HTMLDivElement | null>;
  active: boolean;
  rect: PipRect;
  onRectChange: (r: PipRect) => void;
  onInline: () => void;
  onFullscreen: () => void;
}) {
  // Pointer-driven drag from the header. Pointer events are used over
  // mouse events so a stylus/touch drag works the same on a mac
  // trackpad force-click.
  const onHeaderPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    // Don't start a drag from the toolbar buttons.
    if ((e.target as HTMLElement).closest("button")) return;
    e.preventDefault();
    const startX = e.clientX;
    const startY = e.clientY;
    const start = rect;

    const onMove = (ev: PointerEvent) => {
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      const nx = clamp(
        start.x + (ev.clientX - startX),
        PIP_MARGIN - start.w + 80,
        vw - 80,
      );
      const ny = clamp(start.y + (ev.clientY - startY), 0, vh - 40);
      onRectChange({ ...start, x: nx, y: ny });
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  };

  return (
    <div
      aria-hidden={!active}
      className="fixed z-[45] flex flex-col rounded-lg overflow-hidden"
      style={{
        left: rect.x,
        top: rect.y,
        width: rect.w,
        height: rect.h,
        background: "var(--bg-primary)",
        border: "1px solid var(--border)",
        boxShadow: "0 12px 36px rgba(0,0,0,0.45)",
        display: active ? "flex" : "none",
      }}
    >
      <div
        onPointerDown={onHeaderPointerDown}
        className="flex items-center justify-between px-2 py-1.5"
        style={{
          background: "var(--bg-secondary)",
          borderBottom: "1px solid var(--border)",
          color: "var(--text-secondary)",
          cursor: "move",
          userSelect: "none",
        }}
      >
        <div className="inline-flex items-center gap-1.5 text-xs">
          <GripHorizontal size={14} />
          <span>Picture-in-picture</span>
        </div>
        <div className="flex items-center gap-0.5">
          <ToolbarButton
            icon={<Maximize2 size={13} />}
            title="Fullscreen"
            onClick={onFullscreen}
          />
          <ToolbarButton
            icon={<Minimize2 size={13} />}
            title="Restore inline"
            onClick={onInline}
          />
          <ToolbarButton
            icon={<X size={13} />}
            title="Close"
            onClick={onInline}
          />
        </div>
      </div>
      <div ref={slotRef} style={{ flex: "1 1 auto", display: "flex", minHeight: 0 }} />
    </div>
  );
}

// ── Bubble stub ─────────────────────────────────────────────────────

function BubbleStub({
  mode,
  onRestore,
}: {
  mode: DisplayMode;
  onRestore: () => void;
}) {
  const label =
    mode === "fullscreen" ? "in Fullscreen" : "in Picture-in-picture";
  const icon = mode === "fullscreen" ? "⛶" : "🖼️";
  return (
    <div
      className="mt-2 inline-flex items-center gap-2 rounded px-2 py-1 text-xs"
      style={{
        background: "var(--bg-secondary)",
        border: "1px dashed var(--border)",
        color: "var(--text-secondary)",
      }}
    >
      <span>
        {icon} {label}
      </span>
      <button
        type="button"
        onClick={onRestore}
        className="px-1.5 py-0.5 rounded text-[11px]"
        style={{
          background: "var(--bg-primary)",
          color: "var(--text-primary)",
          border: "1px solid var(--border)",
        }}
      >
        Restore
      </button>
    </div>
  );
}

// ── Toolbar primitives ──────────────────────────────────────────────

function ModeToolbar({
  position,
  floating,
  items,
}: {
  position: "top-right";
  floating: boolean;
  items: { icon: ReactNode; title: string; onClick: () => void }[];
}) {
  const positional =
    position === "top-right" ? { top: 6, right: 6 } : {};
  return (
    <div
      className={
        floating
          ? "absolute opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none group-hover:pointer-events-auto"
          : ""
      }
      style={{
        ...positional,
        display: "flex",
        gap: 4,
        background: "var(--bg-secondary)",
        border: "1px solid var(--border)",
        borderRadius: 6,
        padding: 2,
      }}
    >
      {items.map((it, i) => (
        <ToolbarButton
          key={i}
          icon={it.icon}
          title={it.title}
          onClick={it.onClick}
        />
      ))}
    </div>
  );
}

function ToolbarButton({
  icon,
  title,
  onClick,
}: {
  icon: ReactNode;
  title: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      aria-label={title}
      className="inline-flex items-center justify-center rounded transition-colors"
      style={{
        width: 24,
        height: 24,
        background: "transparent",
        color: "var(--text-secondary)",
        border: "none",
        cursor: "pointer",
      }}
      onMouseEnter={(e) => {
        (e.currentTarget as HTMLButtonElement).style.background =
          "var(--bg-tertiary, var(--bg-primary))";
        (e.currentTarget as HTMLButtonElement).style.color =
          "var(--text-primary)";
      }}
      onMouseLeave={(e) => {
        (e.currentTarget as HTMLButtonElement).style.background =
          "transparent";
        (e.currentTarget as HTMLButtonElement).style.color =
          "var(--text-secondary)";
      }}
    >
      {icon}
    </button>
  );
}

// ── helpers ─────────────────────────────────────────────────────────

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}
