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
 * ## Iframe stability across mode changes
 *
 * Earlier versions of this component moved the iframe between three
 * DOM slots (inline / fullscreen / pip) via `appendChild`. WebKit (and
 * wry's WKWebView) reloads any iframe whose DOM ancestry changes, even
 * when the move is a no-op `appendChild` of an already-attached node.
 * That meant every Fullscreen / PIP / "Back to chat" click reloaded
 * the widget — fine for stateless image viewers (pinn.ai re-renders
 * idempotently from `tool-result`), fatal for stateful widgets like a
 * running game.
 *
 * The fix: render the iframe exactly ONCE inside a wrapper portaled to
 * `document.body`, and switch modes purely via CSS:
 *   inline      → position:fixed pinned to a placeholder's rect in the
 *                 chat bubble, tracked via ResizeObserver + capturing
 *                 scroll listener
 *   fullscreen  → position:fixed inset:0
 *   pip         → position:fixed at the user-dragged rect
 * The iframe never moves in the DOM tree, so WebKit never reloads it.
 *
 * Protocol = JSON-RPC 2.0:
 *   request:      {jsonrpc:"2.0", id, method, params}
 *   response:     {jsonrpc:"2.0", id, result|error}
 *   notification: {jsonrpc:"2.0", method, params}            (no id)
 */

import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
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
  /// Add `allow-same-origin` to the iframe sandbox. Opt-in for
  /// first-party tools that need to load `<script src>` / images
  /// from a localhost preview server (`GamedevPreview`). Untrusted
  /// MCP-server widgets leave this `false` so they stay on an opaque
  /// origin and can't reach back into host state.
  allowSameOrigin?: boolean;
  /// Per-widget opt-in for content-driven inline iframe height. When
  /// `true`, `ui/notifications/size-changed` messages from the widget
  /// resize the inline surface (capped at 85% of viewport). When
  /// `false` (default), all such messages are silently dropped and
  /// the iframe stays at the fixed `INLINE_HEIGHT`. Trust gate is
  /// orthogonal to `allowSameOrigin` — a trusted widget can grant
  /// same-origin without unlocking resize, and vice versa.
  autoSize?: boolean;
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
/// Bounds for honored `ui/notifications/size-changed` requests. The
/// floor avoids accidental collapse if a widget reports 0; the cap is
/// fractional-of-viewport so an over-eager widget can't push the chat
/// surface off-screen on a small window. Only applied when the widget
/// opted in via `autoSize=true`.
const AUTOSIZE_MIN = 200;
const AUTOSIZE_MAX_FRAC = 0.85;

const PIP_DEFAULT_W = 360;
const PIP_DEFAULT_H = 260;
const PIP_MARGIN = 16;

type PipRect = { x: number; y: number; w: number; h: number };

type Rect = { x: number; y: number; w: number; h: number };

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
  allowSameOrigin = false,
  autoSize = false,
}: Props) {
  const [mode, setMode] = useState<DisplayMode>("inline");
  const [pipRect, setPipRect] = useState<PipRect>(() => loadPipRect(uri));
  // Honored only when autoSize === true. Falls back to the fixed
  // INLINE_HEIGHT when null (widget didn't report, isn't opted in, or
  // reported out-of-bounds). Persists across mode toggles so a lift
  // to fullscreen + back lands the inline surface at the same height
  // it last measured.
  const [measuredHeight, setMeasuredHeight] = useState<number | null>(null);
  // Inline placeholder's viewport rect, tracked via RAF-throttled
  // ResizeObserver + capturing scroll listener. Drives the inner
  // iframe wrapper's position in inline mode. `null` until the first
  // measurement lands (one layout pass after mount).
  const [placeholderRect, setPlaceholderRect] = useState<Rect | null>(null);
  // The chat scroll container's viewport rect. Used as the outer
  // wrapper's bounds in inline mode so `overflow:hidden` clips the
  // iframe to the visible chat area — without this the position:fixed
  // wrapper bleeds over chat headers / the input bar.
  const [containerRect, setContainerRect] = useState<Rect | null>(null);
  const { resolved: themeMode } = useTheme();

  const iframeRef = useRef<HTMLIFrameElement | null>(null);
  const placeholderRef = useRef<HTMLDivElement | null>(null);
  // Cached scroll-ancestor of the placeholder. Found once on the first
  // inline-mode layout pass; reused for every subsequent measurement.
  // Falls back to `document.documentElement` when the chat scrolls at
  // window level rather than in a bounded container.
  const scrollParentRef = useRef<HTMLElement | null>(null);
  // Mirror mode/theme into refs so the message handler reads the
  // latest values without re-binding. Re-binding the listener on
  // every mode change would create a window where iframe→host
  // messages get dropped.
  const modeRef = useRef<DisplayMode>("inline");
  const themeRef = useRef(themeMode);

  const stableResult = useMemo(() => toolResult, [toolResult]);
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

  // Track inline placeholder + chat scroll container rects. The outer
  // wrapper (overflow:hidden, position:fixed) is sized to the scroll
  // container so the iframe never bleeds past the chat viewport; the
  // inner wrapper is offset to the placeholder so the iframe scrolls
  // with chat content. useLayoutEffect runs before paint to avoid a
  // one-frame flash. Capturing scroll listener catches every scrollable
  // ancestor — ChatView's list, document, anything in between.
  useLayoutEffect(() => {
    if (mode !== "inline") return;
    const el = placeholderRef.current;
    if (!el) return;

    // Find (and cache) the scroll-bounded ancestor. Falling back to
    // `documentElement` keeps inline mode functional when the chat
    // scrolls at window level rather than in a bounded container.
    if (!scrollParentRef.current) {
      let p: HTMLElement | null = el.parentElement;
      while (p && p !== document.body) {
        const style = window.getComputedStyle(p);
        if (style.overflowY === "auto" || style.overflowY === "scroll") {
          scrollParentRef.current = p;
          break;
        }
        p = p.parentElement;
      }
      if (!scrollParentRef.current) {
        scrollParentRef.current = document.documentElement;
      }
    }

    let raf = 0;
    const measure = () => {
      raf = 0;
      const node = placeholderRef.current;
      if (!node) return;
      const pr = node.getBoundingClientRect();
      setPlaceholderRect({ x: pr.left, y: pr.top, w: pr.width, h: pr.height });
      const sp = scrollParentRef.current;
      if (sp) {
        const cr = sp.getBoundingClientRect();
        setContainerRect({ x: cr.left, y: cr.top, w: cr.width, h: cr.height });
      }
    };
    const schedule = () => {
      if (raf) return;
      raf = requestAnimationFrame(measure);
    };

    // Synchronous initial measure before paint.
    measure();

    const ro = new ResizeObserver(schedule);
    ro.observe(el);
    if (scrollParentRef.current && scrollParentRef.current !== document.documentElement) {
      ro.observe(scrollParentRef.current);
    }
    window.addEventListener("scroll", schedule, true);
    window.addEventListener("resize", schedule);

    return () => {
      if (raf) cancelAnimationFrame(raf);
      ro.disconnect();
      window.removeEventListener("scroll", schedule, true);
      window.removeEventListener("resize", schedule);
    };
  }, [mode]);

  // Persist PIP rect on every change so a re-render or remount
  // doesn't snap the panel back to the default corner.
  useEffect(() => {
    if (mode === "pip") savePipRect(uri, pipRect);
  }, [mode, pipRect, uri]);

  // Mode change → notify widget so it can re-layout. We post even if
  // the widget hasn't finished init yet; if it's not listening yet
  // the next `initialize` response carries the new mode in
  // hostContext.displayMode and the widget catches up that way.
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
            // Read mode/theme through refs so the init response
            // reflects the current state, not whatever was captured
            // when this listener was bound.
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
            // Extract text from content blocks and route through the
            // same `shell_input` IPC the chat composer uses. Multi-
            // block / image content blocks are flattened to text —
            // image attachment via this path can be added later if a
            // widget actually needs it.
            const params = msg.params as
              | { role?: string; content?: Array<{ type?: string; text?: string }> }
              | undefined;
            const blocks = params?.content ?? [];
            const text = blocks
              .filter((b) => b?.type === "text")
              .map((b) => b?.text ?? "")
              .join("");
            if (text.trim()) {
              send({ type: "shell_input", text });
              // Include `content: []` so the response is a valid
              // CallToolResult — the SDK's Zod validator on the
              // widget side rejects `{isError}` alone.
              respond(id, { content: [], isError: false });
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
            // Re-push tool-result on every `initialized` notification.
            // After the CSS-positioning rewrite the iframe survives
            // mode changes, so this normally fires exactly once per
            // widget load — but keeping the push idempotent here
            // means a widget that voluntarily reloads itself (e.g. an
            // in-widget "reset" button) gets its state back.
            sendNotification("ui/notifications/tool-result", {
              content: stableResult.content,
              isError: stableResult.isError ?? false,
              _meta: stableResult._meta,
            });
            break;
          }
          case "ui/notifications/size-changed": {
            // Per-widget opt-in. Untrusted / unannotated widgets keep
            // the fixed INLINE_HEIGHT — this is the safety-by-default
            // the autoSize flag is layered against. The pinn.ai
            // image-viewer bug (reports spinner-state size before the
            // image loads) is filtered out here because pinn.ai's
            // widget meta doesn't carry autoSize.
            if (!autoSize) break;
            // Only honor size changes while inline. In fullscreen / PIP
            // a widget may re-layout to its larger viewport and report
            // a different height — but `measuredHeight` drives the
            // INLINE bubble's placeholder, so accepting a lifted-mode
            // report would shift the chat layout under the user's
            // scroll position and snap it back on Back-to-chat. When
            // the widget returns to inline it re-emits with the inline
            // size (via host-context-changed → its own re-layout).
            if (modeRef.current !== "inline") break;
            const params = msg.params as { height?: number } | undefined;
            const h = params?.height;
            if (typeof h !== "number" || !Number.isFinite(h)) break;
            const cap = Math.floor(window.innerHeight * AUTOSIZE_MAX_FRAC);
            const bounded = Math.max(AUTOSIZE_MIN, Math.min(cap, Math.ceil(h)));
            setMeasuredHeight(bounded);
            break;
          }
          default:
            break;
        }
      }
    };

    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [stableResult, themeMode, autoSize, serverPrefix]);

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

  // Esc → back to inline from fullscreen. Bound only while fullscreen
  // so we don't steal the key from other modals when the widget is
  // inline or PIP.
  useEffect(() => {
    if (mode !== "fullscreen") return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMode("inline");
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [mode]);

  const inlineHeight = measuredHeight ?? INLINE_HEIGHT;

  // Compute the outer + inner wrapper styles. The DOM shape is the
  // same for every mode (outer → inner → iframe) so React never
  // re-mounts the iframe across mode lifts; only the styles change.
  //
  // inline      — outer = chat container's rect (overflow:hidden so the
  //               iframe is clipped to the visible chat viewport and
  //               doesn't bleed over the header / input bar). Inner is
  //               absolute-positioned at the placeholder's offset
  //               inside outer, sized to the placeholder. Negative
  //               offsets are valid when the bubble scrolls past the
  //               top — the iframe simply moves up under the clip.
  // fullscreen  — outer = full viewport, inner = 100%.
  // pip         — outer = pipRect, inner = 100%.
  const baseInner: CSSProperties = {
    display: "flex",
    flexDirection: "column",
    background: "var(--bg-primary)",
    minHeight: 0,
  };
  let outerStyle: CSSProperties;
  let innerStyle: CSSProperties;
  if (mode === "inline") {
    if (!placeholderRect || !containerRect) {
      outerStyle = { display: "none" };
      innerStyle = baseInner;
    } else {
      outerStyle = {
        position: "fixed",
        left: containerRect.x,
        top: containerRect.y,
        width: containerRect.w,
        height: containerRect.h,
        overflow: "hidden",
        // Let chat scroll / clicks pass through wherever the wrapper
        // covers non-iframe space. The inner div re-enables pointer
        // events for the iframe area itself.
        pointerEvents: "none",
        zIndex: 10,
      };
      innerStyle = {
        ...baseInner,
        position: "absolute",
        left: placeholderRect.x - containerRect.x,
        top: placeholderRect.y - containerRect.y,
        width: placeholderRect.w,
        height: placeholderRect.h,
        pointerEvents: "auto",
        borderRadius: 6,
        border: "1px solid var(--border)",
        overflow: "hidden",
      };
    }
  } else if (mode === "fullscreen") {
    outerStyle = {
      position: "fixed",
      inset: 0,
      zIndex: 55,
    };
    innerStyle = {
      ...baseInner,
      width: "100%",
      height: "100%",
    };
  } else {
    // pip
    outerStyle = {
      position: "fixed",
      left: pipRect.x,
      top: pipRect.y,
      width: pipRect.w,
      height: pipRect.h,
      zIndex: 45,
    };
    innerStyle = {
      ...baseInner,
      width: "100%",
      height: "100%",
      borderRadius: 8,
      border: "1px solid var(--border)",
      boxShadow: "0 12px 36px rgba(0,0,0,0.45)",
      overflow: "hidden",
    };
  }

  // PIP drag — pointer events so trackpad / stylus work the same as
  // mouse. Captured on the header to leave the iframe body free to
  // receive widget interactions.
  const onPipHeaderPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if ((e.target as HTMLElement).closest("button")) return;
    e.preventDefault();
    const startX = e.clientX;
    const startY = e.clientY;
    const start = pipRect;

    const onMove = (ev: PointerEvent) => {
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      const nx = clamp(
        start.x + (ev.clientX - startX),
        PIP_MARGIN - start.w + 80,
        vw - 80,
      );
      const ny = clamp(start.y + (ev.clientY - startY), 0, vh - 40);
      setPipRect({ ...start, x: nx, y: ny });
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  };

  return (
    <>
      {/* Inline placeholder. Always rendered at the full inline height
          so the chat bubble's geometry stays constant across mode
          changes — collapsing this when the iframe lifts to Fullscreen
          / PIP would shift everything below by 480px, and the
          subsequent return-to-inline would shift it back, producing
          the "chat jumped down on Back-to-chat" behavior.
          In inline mode this div is invisible (the portaled wrapper
          overlays it). In fullscreen/PIP it shows a centered stub so
          the user has a visible affordance to come back. */}
      <div
        ref={placeholderRef}
        style={{
          marginTop: 8,
          height: inlineHeight,
          borderRadius: 6,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          ...(mode !== "inline"
            ? {
                background: "var(--bg-secondary)",
                border: "1px dashed var(--border)",
              }
            : {}),
        }}
      >
        {mode !== "inline" && (
          <BubbleStub mode={mode} onRestore={() => setMode("inline")} />
        )}
      </div>

      {/* The single, permanent iframe + chrome. Portaled to body and
          positioned with CSS only — never re-parented across mode
          changes. This is the whole point of the rewrite: WebKit/wry
          reloads an iframe whose DOM ancestry changes, and keeping
          this wrapper put is what preserves widget state across
          Fullscreen / PIP / Back-to-chat clicks. */}
      {createPortal(
        <div style={outerStyle}>
          <div className="group" style={innerStyle}>
            {mode === "inline" && (
              <FloatingInlineToolbar
                onFullscreen={() => setMode("fullscreen")}
                onPip={() => setMode("pip")}
              />
            )}
            {mode === "fullscreen" && (
              <FullscreenHeader
                onInline={() => setMode("inline")}
                onPip={() => setMode("pip")}
              />
            )}
            {mode === "pip" && (
              <PipHeader
                onPointerDown={onPipHeaderPointerDown}
                onInline={() => setMode("inline")}
                onFullscreen={() => setMode("fullscreen")}
              />
            )}

            <iframe
              ref={iframeRef}
              srcDoc={html}
              title={`MCP App: ${uri}`}
              // `allow-scripts` is required for the SDK to run;
              // combining it with `allow-same-origin` would defeat the
              // srcdoc origin isolation, so we only add it when the
              // caller has explicitly opted in.
              sandbox={
                allowSameOrigin
                  ? "allow-scripts allow-popups allow-forms allow-same-origin"
                  : "allow-scripts allow-popups allow-forms"
              }
              style={{
                display: "block",
                flex: "1 1 auto",
                width: "100%",
                border: "none",
                background: "transparent",
                minHeight: 0,
              }}
            />
          </div>
        </div>,
        document.body,
      )}
    </>
  );
}

// ── Inline floating toolbar ─────────────────────────────────────────

function FloatingInlineToolbar({
  onFullscreen,
  onPip,
}: {
  onFullscreen: () => void;
  onPip: () => void;
}) {
  return (
    <div
      className="absolute opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none group-hover:pointer-events-auto"
      style={{
        top: 6,
        right: 6,
        display: "flex",
        gap: 4,
        background: "var(--bg-secondary)",
        border: "1px solid var(--border)",
        borderRadius: 6,
        padding: 2,
        zIndex: 1,
      }}
    >
      <ToolbarButton
        icon={<Maximize2 size={14} />}
        title="Fullscreen"
        onClick={onFullscreen}
      />
      <ToolbarButton
        icon={<PictureInPicture2 size={14} />}
        title="Picture-in-picture"
        onClick={onPip}
      />
    </div>
  );
}

// ── Fullscreen header ───────────────────────────────────────────────

function FullscreenHeader({
  onInline,
  onPip,
}: {
  onInline: () => void;
  onPip: () => void;
}) {
  return (
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
  );
}

// ── PIP header (draggable) ──────────────────────────────────────────

function PipHeader({
  onPointerDown,
  onInline,
  onFullscreen,
}: {
  onPointerDown: (e: React.PointerEvent<HTMLDivElement>) => void;
  onInline: () => void;
  onFullscreen: () => void;
}) {
  return (
    <div
      onPointerDown={onPointerDown}
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
