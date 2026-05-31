// thClaws GUI Shell — bridge runtime, Tier 1.
//
// Injected automatically into every shell's <head> at HTML serve time.
// Exposes window.thclaws.* for the shell's code to call. Marshals
// JSON over postMessage to the parent React app (Mode A) or directly
// over WebSocket (Mode B, Tier 2 — not implemented yet).
//
// Tier 1 surface:
//   thclaws.shell.id          — string, this shell's id
//   thclaws.shell.sessionId   — string, the session this tab is bound to
//   thclaws.transport         — "tauri" | "ws"
//   thclaws.run(prompt, opts?) -> Promise<{ runId }>
//   thclaws.cancel(runId?)    -> void
//   thclaws.on(event, cb)     -> unsubscribe()
//       events: "text" | "done" | "error" | "ready"
//
// Tier 2 additions:
//   thclaws.storage.get(key)         -> Promise<any>     // file-backed
//   thclaws.storage.set(key, value)  -> Promise<void>    // <shell-root>/state/<sessionId>.json
//   thclaws.on(event, cb) events:    + "tool_call" + "tool_result"
//   thclaws.tools.invoke(name, args) -> Promise<string>  // Task 18 (separate)

(() => {
  // Mode A URL: thclaws://localhost/gui-shell/<id>/<path>?session=<sid>
  // Mode B URL: https://host/t/<token>/<path>?session=<sid> — the
  // serve handler sets window.__thclaws_shell_mode = "ws" before this
  // runs, plus window.__thclaws_shell_ws_url for the WS endpoint.
  const url = new URL(location.href);
  const parts = url.pathname.split("/").filter(Boolean);
  const isModeB = window.__thclaws_shell_mode === "ws";
  // Identifier resolution:
  //   Mode B — the serve handler injects window.__thclaws_shell_id +
  //            window.__thclaws_shell_session_id at HTML render time
  //            (the URL `/t/<token>/` carries neither).
  //   Mode A — fall back to URL parts: /gui-shell/<id>/... + ?session=<id>
  const shellId =
    (typeof window.__thclaws_shell_id === "string" && window.__thclaws_shell_id) ||
    (parts[0] === "gui-shell" ? parts[1] : null);
  const sessionId =
    (typeof window.__thclaws_shell_session_id === "string" &&
      window.__thclaws_shell_session_id) ||
    url.searchParams.get("session");
  const transport = isModeB ? "ws" : "tauri";

  const pending = new Map();     // requestId -> {resolve, reject}
  const subscribers = new Map(); // eventName -> Set<callback>
  let nextRequestId = 1;

  // Mode B WebSocket transport — opened lazily on first send. The
  // bridge auto-reconnects with exponential backoff if the socket
  // drops mid-session (Risk 13 in dev-plan/33).
  let ws = null;
  let wsQueue = [];
  let wsBackoffMs = 500;
  function ensureWs() {
    if (!isModeB) return null;
    if (ws && ws.readyState === WebSocket.OPEN) return ws;
    if (ws && ws.readyState === WebSocket.CONNECTING) return ws;
    const wsUrl = (() => {
      const path = window.__thclaws_shell_ws_url || "/__ws";
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      return `${proto}//${location.host}${path}`;
    })();
    ws = new WebSocket(wsUrl);
    ws.addEventListener("open", () => {
      wsBackoffMs = 500;
      while (wsQueue.length) ws.send(wsQueue.shift());
    });
    ws.addEventListener("message", (evt) => {
      try {
        const obj = typeof evt.data === "string" ? JSON.parse(evt.data) : null;
        if (!obj) return;
        // Backend dispatches arrive as flat {type, ...} JSON. Convert
        // shell-relevant types into the bridge's ns="thclaws-shell-event"
        // envelope so the existing event-loop handler does the
        // routing.
        if (obj.type === "gui_shell_event") {
          handleShellEvent(obj);
        }
      } catch {}
    });
    ws.addEventListener("close", () => {
      const wait = Math.min(wsBackoffMs, 10_000);
      wsBackoffMs = Math.min(wsBackoffMs * 2, 30_000);
      setTimeout(ensureWs, wait);
    });
    return ws;
  }

  // Single point where backend gui_shell_event envelopes get fanned
  // out to bridge subscribers or resolve a pending request — shared
  // between Mode A (parent postMessage) and Mode B (WS).
  function handleShellEvent(data) {
    if (data.replyTo != null && pending.has(data.replyTo)) {
      const slot = pending.get(data.replyTo);
      pending.delete(data.replyTo);
      if (data.error) slot.reject(new Error(data.error));
      else slot.resolve(data.result);
      return;
    }
    if (data.event) {
      const set = subscribers.get(data.event);
      if (set) {
        for (const cb of set) {
          try { cb(data.payload); } catch (err) {
            // eslint-disable-next-line no-console
            console.error("thclaws shell subscriber threw:", err);
          }
        }
      }
    }
  }

  function ensureSub(event) {
    let set = subscribers.get(event);
    if (!set) {
      set = new Set();
      subscribers.set(event, set);
    }
    return set;
  }

  function send(type, payload) {
    return new Promise((resolve, reject) => {
      const requestId = nextRequestId++;
      pending.set(requestId, { resolve, reject });
      if (isModeB) {
        // Mode B: write directly to WS, queuing until open.
        const frame = JSON.stringify({
          type: `gui_shell_${type}`,
          id: requestId,
          sessionId,
          shellId,
          ...payload,
        });
        const sock = ensureWs();
        if (sock && sock.readyState === WebSocket.OPEN) {
          sock.send(frame);
        } else {
          wsQueue.push(frame);
        }
        return;
      }
      // Mode A: parent React app marshals between window.ipc and us.
      parent.postMessage(
        {
          ns: "thclaws-shell",
          requestId,
          type,
          payload,
          shellId,
          sessionId,
        },
        "*",
      );
    });
  }

  // Mode A only: parent React app forwards backend dispatches to us
  // via postMessage. Mode B receives them directly on the WS, handled
  // in the ensureWs() message handler above.
  if (!isModeB) {
    window.addEventListener("message", (e) => {
      const data = e.data;
      if (!data || data.ns !== "thclaws-shell-event") return;
      handleShellEvent(data);
    });
  }

  window.thclaws = {
    shell: { id: shellId, sessionId },
    transport,

    run(prompt, opts) {
      if (typeof prompt !== "string") {
        return Promise.reject(new TypeError("thclaws.run: prompt must be a string"));
      }
      return send("run", { prompt, ...(opts || {}) });
    },

    cancel(runId) {
      // Fire-and-forget — cancel doesn't acknowledge.
      if (isModeB) {
        const frame = JSON.stringify({
          type: "gui_shell_cancel",
          id: nextRequestId++,
          sessionId,
          shellId,
          runId: runId || null,
        });
        const sock = ensureWs();
        if (sock && sock.readyState === WebSocket.OPEN) sock.send(frame);
        else wsQueue.push(frame);
        return;
      }
      parent.postMessage(
        {
          ns: "thclaws-shell",
          requestId: nextRequestId++,
          type: "cancel",
          payload: { runId: runId || null },
          shellId,
          sessionId,
        },
        "*",
      );
    },

    on(event, callback) {
      if (typeof callback !== "function") {
        throw new TypeError("thclaws.on: callback must be a function");
      }
      const set = ensureSub(event);
      set.add(callback);
      return () => set.delete(callback);
    },

    // Tier 2: resolve a path the agent produced (in `./output/...` or
    // similar) to a URL the browser can fetch — e.g. for
    //   <img src={thclaws.fileUrl(payload.file)}>
    //
    // Mode B: the bound shell's project root IS the cwd, so a relative
    // path like "output/abc.svg" maps to /t/<token>/file-asset/output/
    // abc.svg.
    //
    // Mode A: cwd is the launch dir (Tier 2.x — Task 21 adds CWD
    // switching). For now the shell author should ensure the agent
    // returns an absolute path in Mode A; relative paths return null.
    fileUrl(path) {
      if (typeof path !== "string" || !path) return null;
      if (isModeB) {
        const wsUrl = window.__thclaws_shell_ws_url || "";
        const prefix = wsUrl.endsWith("/__ws") ? wsUrl.slice(0, -5) : wsUrl;
        const tail = path.startsWith("/") ? path : "/" + path;
        return `${prefix}/file-asset${tail}`;
      }
      if (path.startsWith("/")) {
        return `thclaws://localhost/file-asset${path}`;
      }
      return null;
    },

    // Tier 2: direct tool invocation, bypasses the agent loop. Use
    // this for deterministic actions in a shell's UI ("Generate"
    // button calls image_gen, no model round-trip). Returns the
    // tool's raw string output.
    //
    // Read-only tools (ls / read / glob / grep / web_fetch / web_search
    // / kms_read / kms_search / docx_read / pdf_read / xlsx_read /
    // youtube_transcript / web_scrape / etc.) work directly.
    //
    // Mutating tools (Bash / Write / Edit / DocxCreate / etc.) reject
    // with "requires approval" — the approval flow lands in Tier 3.
    //
    // MCP-contributed tools aren't reachable here in Tier 2 (the IPC
    // arm builds a fresh built-ins-only ToolRegistry). Tier 3 routes
    // through the worker's registry so MCP tools work too.
    tools: {
      invoke(name, args) {
        if (typeof name !== "string" || !name) {
          return Promise.reject(
            new TypeError("thclaws.tools.invoke: name must be a non-empty string"),
          );
        }
        return send("tool_invoke", { name, args: args ?? null });
      },
    },

    // Tier 2: per-shell, per-session storage. Backed by a single JSON
    // file at <shell-root>/state/<sessionId>.json — atomic per-set,
    // namespaced by shell id (two shells with different ids cannot
    // read each other's storage even if they happen to share a session).
    storage: {
      get(key) {
        if (typeof key !== "string") {
          return Promise.reject(
            new TypeError("thclaws.storage.get: key must be a string"),
          );
        }
        return send("storage_get", { key });
      },
      set(key, value) {
        if (typeof key !== "string") {
          return Promise.reject(
            new TypeError("thclaws.storage.set: key must be a string"),
          );
        }
        return send("storage_set", { key, value });
      },
    },
  };

  if (isModeB) {
    // Open the WS proactively so the first send doesn't pay the
    // connection setup latency.
    ensureWs();
  } else {
    // Mode A only — signal to the parent React app.
    parent.postMessage(
      { ns: "thclaws-shell", type: "ready", shellId, sessionId },
      "*",
    );
  }
})();
