import { SessionNavigation } from "./sessionNavigation";
/**
 * IPC bridge between React frontend and Rust backend.
 *
 * Two transports — same protocol, same `send()` / `subscribe()` API.
 * The transport is chosen at module-load time by sniffing
 * `window.ipc`:
 *
 * - Desktop GUI (wry): `window.ipc.postMessage(json)` → Rust;
 *   `window.__thclaws_dispatch(json)` ← Rust (called via evaluate_script).
 * - Webapp (M6.36 `--serve` mode): WebSocket at `ws[s]://<host>/ws`.
 *   Inbound frames are dispatched to subscribers exactly like wry's
 *   `__thclaws_dispatch`; outbound `send()` calls map to `ws.send`.
 *
 * dev-plan/59 §6.3: under a workspace host the webapp transport holds ONE
 * SOCKET PER BOT (`/ws?bot=<slug>`) rather than one socket carrying tagged
 * frames — N sockets need no multiplex protocol on either end, and a
 * background bot's own socket is what lights its dot in the rail.
 * `send()` goes to the ACTIVE bot; inbound frames reach `subscribe()` only
 * from the active bot's socket, so a component never sees another bot's
 * stream. That is safe only because hidden bot subtrees run no effects
 * (React `<Activity>`), so a background bot never calls `send()` at all —
 * see `BotShell`. Everything else — `send`, `subscribe`, all 236 call
 * sites — is unchanged.
 *
 * The webapp transport reconnects automatically with exponential
 * backoff capped at 5s. While disconnected, the singleton dispatches
 * `{type: "ws_status", status: "disconnected"|"connecting"|"connected"}`
 * envelopes so any banner component can render a "reconnecting…" UI.
 * On reconnect, the frontend re-sends `frontend_ready` so the server
 * pushes a fresh initial-state snapshot — Phase 1A semantics from the
 * M6.36 design (snapshot-then-stream).
 */

export type IPCMessage = {
  type: string;
  [key: string]: unknown;
};

type Handler = (msg: IPCMessage) => void;
const handlers = new Set<Handler>();

/** Shell-level listeners: they see every bot's frames, tagged with its slug. */
type AnyHandler = (slug: string | null, msg: IPCMessage) => void;
const anyHandlers = new Set<AnyHandler>();

function emitToSubscribers(msg: IPCMessage) {
  handlers.forEach((h) => {
    try {
      h(msg);
    } catch (e) {
      console.error("[ipc] handler error:", e);
    }
  });
}

const navigation = new Map<string | null, SessionNavigation>();
function sessionNavigation() {
  let state = navigation.get(activeSlug);
  if (!state) {
    state = new SessionNavigation(emitToSubscribers, transmit);
    navigation.set(activeSlug, state);
  }
  return state;
}
function dispatchToSubscribers(msg: IPCMessage) { sessionNavigation().receive(msg); }
export function sessionActionError() { return sessionNavigation().actionError(); }
export function viewedSessionId() { return sessionNavigation().viewed; }


function dispatchFromBot(slug: string | null, msg: IPCMessage) {
  anyHandlers.forEach((h) => {
    try {
      h(slug, msg);
    } catch (e) {
      console.error("[ipc] shell handler error:", e);
    }
  });
  // Only the focused bot's frames reach the component tree. A hidden tree
  // has no live subscribers anyway; this is the belt to that braces.
  if (slug === activeSlug) dispatchToSubscribers(msg);
}

// ── Wry desktop transport (existing) ─────────────────────────────────

let wrySend: ((msg: IPCMessage) => void) | null = null;

if (typeof window !== "undefined" && window.ipc) {
  wrySend = (msg) => window.ipc!.postMessage(JSON.stringify(msg));
  window.__thclaws_dispatch = (json: string) => {
    try {
      const msg: IPCMessage = JSON.parse(json);
      // dev-plan/59 §7.6: on the desktop every bot's frames arrive on one
      // bridge, stamped `_bot` by the window. A frame still in flight from
      // the bot being switched away from must not land in the new bot's
      // tree; frames the window itself sends carry no stamp and pass.
      const from = typeof msg._bot === "string" ? (msg._bot as string) : null;
      if (from !== null) {
        anyHandlers.forEach((h) => {
          try {
            h(from, msg);
          } catch (e) {
            console.error("[ipc] shell handler error:", e);
          }
        });
        if (activeSlug !== null && from !== activeSlug) return;
      }
      dispatchToSubscribers(msg);
    } catch (e) {
      console.error("[ipc] wry dispatch parse error:", e);
    }
  };
}

// ── Webapp WebSocket transport (M6.36 SERVE4) ───────────────────────

/** One connection. `slug === null` is the pre-host / single-bot socket. */
type BotConn = {
  slug: string | null;
  ws: WebSocket | null;
  /** Sends issued while CONNECTING, flushed on open. */
  queue: IPCMessage[];
  backoffMs: number;
  /** Set when the shell retires this connection, so it stops reconnecting. */
  closed: boolean;
};

const conns = new Map<string | null, BotConn>();
let activeSlug: string | null = null;

/**
 * Path prefix the frontend lives under, with a trailing slash.
 *
 * Local + self-hosted deploys serve at `/` so this returns `/`. Hosted
 * agents on thclaws.cloud serve under `/u/<handle>/<slug>/` — every
 * runtime URL (WebSocket, /upload, etc.) needs to include that prefix
 * so requests reach the right pod after Caddy strips it. Recomputed
 * lazily at call time so we pick up the actual page location, not a
 * cached value from another tab.
 */
export function basePath(): string {
  const p = window.location.pathname;
  return p.endsWith("/") ? p : p + "/";
}

/**
 * Bearer for a host that was started with one (dev-plan/59 Step 2). It rides
 * in the page URL. A plain `--serve` has none and this returns "".
 */
export function serveToken(): string {
  try {
    return new URLSearchParams(window.location.search).get("token") ?? "";
  } catch {
    return "";
  }
}

/** Query string carrying the bot and token, for WS URLs and fetches. */
export function botQuery(slug?: string | null): string {
  const p = new URLSearchParams();
  const s = slug === undefined ? activeSlug : slug;
  if (s) p.set("bot", s);
  const t = serveToken();
  if (t) p.set("token", t);
  const q = p.toString();
  return q ? `?${q}` : "";
}

function wsUrl(slug: string | null): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}${basePath()}ws${botQuery(slug)}`;
}

function emitStatus(
  slug: string | null,
  status: "disconnected" | "connecting" | "connected",
) {
  // Synthetic event the React banner can subscribe to. Doesn't go
  // through the WS — purely a frontend-side signal.
  dispatchFromBot(slug, { type: "ws_status", status });
}

function connect(conn: BotConn) {
  if (conn.closed) return;
  emitStatus(conn.slug, "connecting");
  const ws = new WebSocket(wsUrl(conn.slug));
  conn.ws = ws;
  ws.onopen = () => {
    conn.backoffMs = 250; // reset backoff
    emitStatus(conn.slug, "connected");
    // M6.36 Phase 1A: re-send frontend_ready on every (re)connect so
    // the server pushes the latest snapshot. Each bot's socket gets its
    // own handshake, so each answers with its own state.
    rawSend(conn, { type: "frontend_ready" });
    const queued = conn.queue;
    conn.queue = [];
    queued.forEach((m) => rawSend(conn, m));
  };
  ws.onmessage = (ev) => {
    try {
      dispatchFromBot(conn.slug, JSON.parse(ev.data) as IPCMessage);
    } catch (e) {
      console.error("[ipc] ws dispatch parse error:", e);
    }
  };
  ws.onclose = () => {
    emitStatus(conn.slug, "disconnected");
    if (conn.closed) return;
    // Exponential backoff up to RECONNECT_MAX_MS.
    setTimeout(() => {
      conn.backoffMs = Math.min(conn.backoffMs * 2, RECONNECT_MAX_MS);
      connect(conn);
    }, conn.backoffMs);
  };
  ws.onerror = () => {
    // onclose fires after onerror — let onclose handle reconnect.
  };
}

const RECONNECT_MAX_MS = 5000;

function rawSend(conn: BotConn, msg: IPCMessage) {
  const ws = conn.ws;
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(msg));
  } else if (ws && ws.readyState === WebSocket.CONNECTING) {
    conn.queue.push(msg);
  } else {
    console.warn("[ipc] ws not open, dropped:", msg);
  }
}

function openConn(slug: string | null): BotConn {
  const existing = conns.get(slug);
  if (existing) return existing;
  const conn: BotConn = {
    slug,
    ws: null,
    queue: [],
    backoffMs: 250,
    closed: false,
  };
  conns.set(slug, conn);
  connect(conn);
  return conn;
}

if (typeof window !== "undefined" && !window.ipc) {
  // No wry — assume webapp transport. One connection until the shell
  // discovers this is a host and asks for one per bot.
  openConn(null);
}

// ── Bot routing (dev-plan/59 §6.3) ───────────────────────────────────

/**
 * Open a socket per bot and focus one. Called by `BotShell` once it knows
 * the workspace is a host.
 *
 * The pre-host socket is ADOPTED as `defaultSlug`'s rather than torn down
 * and replaced: it is already connected to that exact bot (the host routes
 * a socket with no `?bot=` to the first entry in bots.json), so reconnecting
 * would cost a round-trip and a second snapshot to arrive at the same place.
 */
export function configureBots(slugs: string[], active: string, defaultSlug: string) {
  const pre = conns.get(null);
  if (pre && !conns.has(defaultSlug)) {
    conns.delete(null);
    pre.slug = defaultSlug;
    conns.set(defaultSlug, pre);
  }
  for (const slug of slugs) openConn(slug);
  for (const [slug, conn] of [...conns]) {
    if (slug !== null && !slugs.includes(slug)) {
      conn.closed = true;
      conn.ws?.close();
      conns.delete(slug);
    }
  }
  setActiveBot(active);
}

export function setActiveBot(slug: string | null) {
  activeSlug = slug;
}

export function activeBot(): string | null {
  return activeSlug;
}

/** Every bot's frames, tagged. For the rail and the approval interrupt. */
export function subscribeAny(handler: AnyHandler): () => void {
  anyHandlers.add(handler);
  return () => {
    anyHandlers.delete(handler);
  };
}

// ── Public API ──────────────────────────────────────────────────────

export function send(msg: IPCMessage) { return sessionNavigation().send(msg); }

function transmit(msg: IPCMessage) {
  if (wrySend) {
    wrySend(msg);
    return;
  }
  // The ACTIVE bot's socket. Safe because a hidden bot subtree runs no
  // effects, so nothing outside the focused tree calls this.
  const conn = conns.get(activeSlug);
  if (conn) {
    rawSend(conn, msg);
  } else {
    console.warn("[ipc] no backend — running in browser dev mode?", msg);
  }
}

export function subscribe(handler: Handler): () => void {
  handlers.add(handler);
  return () => {
    handlers.delete(handler);
  };
}
