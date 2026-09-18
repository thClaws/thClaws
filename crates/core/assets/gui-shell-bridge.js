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
//
// Host UI integration (full-screen escape hatch):
//   thclaws.ui.isFullscreen          — bool, host is showing us full-screen
//   thclaws.ui.onFullscreen(cb)      -> unsubscribe()    // fires with current state
//   thclaws.ui.exitFullscreen()      -> void             // ask host to leave full-screen
//   thclaws.ui.toggleFullscreen()    -> void             // enter/leave full-screen (⌘⇧U)
//   thclaws.ui.claimExitControl()    -> void             // hide the host's fallback chip
//   thclaws.ui.theme                 — "light" | "dark", host's resolved theme
//   thclaws.ui.onTheme(cb)           -> unsubscribe()    // fires with current theme
//   thclaws.ui.setTheme(mode)        -> void             // ask host to switch app theme
//   thclaws.ui.toggleTheme()         -> void             // flip light/dark
//                                    // (bridge also sets data-theme + color-scheme on <html>)
//
//   thclaws.model.get()              -> Promise<{provider, model, writable}>  // needs model.read
//   thclaws.model.list()             -> Promise<{current, groups:[{provider, models:[{id}]}]}>  // model.read (all providers)
//   thclaws.model.set(id)            -> Promise<{ok, model}>                   // needs model.write
//   thclaws.model.onChange(cb)       -> unsubscribe()     // fires when the active model changes
//   thclaws.model.current            — {provider, model} | null, last seen
//
//   thclaws.kms.list()               -> Promise<{kmss:[{name,scope,active}]}>   // needs kms.read
//   thclaws.kms.browse(name)         -> Promise<{kms, pages, sources}>          // needs kms.read
//   thclaws.connectors.list()        -> Promise<{connectors:[{name,transport,url,scope,tools,status,error}]}>  // connectors.read
//   thclaws.connectors.add({name,url,headers}) -> Promise<{ok,name,path}>       // connectors.write (HTTP only)
//   thclaws.connectors.remove(name)  -> Promise<{ok,name,scope}>                // connectors.write
//   thclaws.skills.install(url, {name,user}) -> Promise<{ok,report,scope}>      // needs skills.write
//   thclaws.llm.complete({prompt,system,maxTokens}) -> Promise<{text,model}>   // needs llm.complete
//   thclaws.plugins.list()           -> Promise<{plugins:[{name,version,enabled,scope,skills,commands,agents,mcpServers}]}>  // plugins.read
//   thclaws.plugins.install(url, {user}) -> Promise<{ok,name,version,skills,commands,agents,mcpServers}>  // plugins.write
//   thclaws.plugins.setEnabled(name, on) -> Promise<{ok,name,enabled,scope}>    // plugins.write
//   thclaws.plugins.remove(name)     -> Promise<{ok,name}>                      // plugins.write
//   thclaws.keys.set(provider, key)  -> Promise<{ok,provider,storage}>          // needs keys.write (write-only)
//   thclaws.research.list()          -> Promise<{jobs:[…]}>                     // needs research.read
//   thclaws.research.get(id)         -> Promise<{job|null}>                     // needs research.read

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
  // dev-plan/39 Tier 3: set once the shell registers an inline approval
  // handler (thclaws.approvals.subscribe). Makes tool invokes send
  // `preferInline` so the engine routes approve/deny to the shell's
  // widget instead of the full-screen system modal.
  let hasApprovalHandler = false;

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
    if (data.shellId && data.shellId !== shellId) return;
    if (data.replyTo != null && pending.has(data.replyTo)) {
      const slot = pending.get(data.replyTo);
      pending.delete(data.replyTo);
      if (data.error) slot.reject(new Error(data.error));
      else slot.resolve(data.result);
      return;
    }
    if (data.event) {
      // Keep the convenience flag in sync before fanning out so a
      // subscriber that reads thclaws.ui.isFullscreen sees the new
      // value.
      if (data.event === "fullscreen" && window.thclaws && window.thclaws.ui) {
        window.thclaws.ui.isFullscreen = !!(data.payload && data.payload.active);
      }
      // Theme sync — the host pushes its resolved theme ("light" |
      // "dark") so shells can match the main UI instead of hardcoding
      // colors. We set `data-theme` + `color-scheme` on the shell's
      // root document directly so a shell only needs theme-aware CSS
      // (`:root[data-theme="light"]{…}`) — no JS required. Subscribers
      // of thclaws.ui.onTheme still fire below via the normal fanout.
      if (data.event === "theme" && window.thclaws && window.thclaws.ui) {
        const mode =
          data.payload && data.payload.mode === "light" ? "light" : "dark";
        window.thclaws.ui.theme = mode;
        try {
          const de = document.documentElement;
          de.setAttribute("data-theme", mode);
          de.style.colorScheme = mode;
        } catch (e) {
          /* document not ready / sandboxed — CSS default applies */
        }
      }
      // Cache the latest model so thclaws.model.onChange can replay it
      // and thclaws.model.current is always readable.
      if (data.event === "model" && window.thclaws && window.thclaws.model) {
        window.thclaws.model.current = data.payload || null;
      }
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

  // A request whose backend arm never replies (an unimplemented /
  // renamed command) must not hang the shell's promise forever. Every
  // send() self-rejects after this long if no reply arrived. Generous
  // so a real long-running tool invoke (image/video gen) still lands —
  // those reply when done, well under this ceiling.
  const SEND_TIMEOUT_MS = 15 * 60 * 1000;

  function send(type, payload) {
    return new Promise((resolve, reject) => {
      const requestId = nextRequestId++;
      const timer = setTimeout(() => {
        if (pending.has(requestId)) {
          pending.delete(requestId);
          reject(
            new Error(
              `thclaws bridge: no reply for '${type}' after ${SEND_TIMEOUT_MS / 1000}s ` +
                `(unimplemented or dropped command?)`,
            ),
          );
        }
      }, SEND_TIMEOUT_MS);
      // Wrap so resolving/rejecting also clears the timeout.
      pending.set(requestId, {
        resolve: (v) => { clearTimeout(timer); resolve(v); },
        reject: (e) => { clearTimeout(timer); reject(e); },
      });
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

  // Fire-and-forget command (no reply awaited) — same wire shape as
  // send() but without registering a pending promise. Used by cancel-
  // style + approval-respond signals.
  function fireAndForget(type, payload) {
    if (isModeB) {
      const frame = JSON.stringify({
        type: `gui_shell_${type}`,
        id: nextRequestId++,
        sessionId,
        shellId,
        ...payload,
      });
      const sock = ensureWs();
      if (sock && sock.readyState === WebSocket.OPEN) sock.send(frame);
      else wsQueue.push(frame);
      return;
    }
    parent.postMessage(
      { ns: "thclaws-shell", requestId: nextRequestId++, type, payload, shellId, sessionId },
      "*",
    );
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
    // Mode A: the desktop `/file-asset/` route tries the path as
    // absolute and then falls back to joining it with cwd, so a
    // workspace-relative path resolves there too — an agent answering
    // with `![cat](./output/cat.jpg)` must render, not print its own
    // markdown. (This used to return null for relative paths, from
    // before the desktop route grew the cwd fallback.)
    fileUrl(path) {
      if (typeof path !== "string" || !path) return null;
      // Already fetchable — an agent that answered with a real URL, or a
      // caller passing a data:/blob: it built itself. Don't file-asset it.
      if (/^(https?|data|blob):/i.test(path)) return path;
      // `./output/x.jpg` → `output/x.jpg`; the leading dot survives the
      // URL otherwise and only the cwd-join branch would tolerate it.
      path = String(path).replace(/^\.\/+/, "");
      // Spaces and non-ASCII names must survive the trip: both routes
      // percent-decode. Pass RAW filesystem paths — `%` is escaped too
      // (a file really can be named `50%.png`), so a caller that
      // pre-encoded would double-encode. `#` and `?` would truncate the
      // path, and encodeURI leaves them alone, so escape those by hand.
      const enc = (p) => encodeURI(p).replace(/#/g, "%23").replace(/\?/g, "%3F");
      if (isModeB) {
        const wsUrl = window.__thclaws_shell_ws_url || "";
        const prefix = wsUrl.endsWith("/__ws") ? wsUrl.slice(0, -5) : wsUrl;
        const tail = path.startsWith("/") ? path : "/" + path;
        return `${prefix}/file-asset${enc(tail)}`;
      }
      // Mode A. The desktop wry webview serves the `thclaws://` custom
      // scheme (host `thclaws.localhost`, exposed as http on WebView2).
      // A real browser — the cloud webapp embeds the shell in an iframe
      // at `<workspace-root>/gui-shell/<id>/` — can't load that scheme
      // (net::ERR_UNKNOWN_URL_SCHEME). There, build an http URL relative
      // to the WORKSPACE ROOT so any reverse-proxy path prefix is kept
      // and it hits the engine's cwd-relative `/file-asset/<rel>` route.
      const wry =
        location.protocol === "thclaws:" ||
        location.hostname === "thclaws.localhost";
      if (!wry && (location.protocol === "http:" || location.protocol === "https:")) {
        const rel = path.replace(/^\/+/, "");
        // Strip back from `…/gui-shell/<id>/…` to the workspace root.
        const m = location.pathname.match(/^(.*?)\/gui-shell\/[^/]+\//);
        const root = m ? m[1] + "/" : location.pathname.replace(/[^/]*$/, "");
        return location.origin + root + "file-asset/" + enc(rel);
      }
      // Absolute and relative both go to the same route: it re-adds the
      // leading slash and tries Sandbox::check as absolute, then as
      // cwd-relative. Whichever the agent produced, the sandbox is what
      // decides whether it's readable.
      //
      // Build from the page's own scheme+host rather than hardcoding
      // `thclaws://localhost`: WebView2 exposes the custom protocol as
      // `http://thclaws.localhost/`, where a literal `thclaws://` URL is
      // an unknown scheme and the image never loads.
      const tail = path.startsWith("/") ? path : "/" + path;
      const host = location.host ? `${location.protocol}//${location.host}` : "thclaws://localhost";
      return `${host}/file-asset${enc(tail)}`;
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
        return send("tool_invoke", {
          name,
          args: args ?? null,
          preferInline: hasApprovalHandler,
        });
      },
    },

    // ── dev-plan/39 Tier 3 — inline tool approvals ────────────────
    // A shell with its own approve/deny UI subscribes here; the engine
    // then routes an mutating tool call's approval to `cb` (as an
    // `approval_request` with an `id`) instead of the full-screen system
    // modal. The shell renders its widget and calls `respond(id, ...)`.
    approvals: {
      // cb receives { id, toolName, input, summary }. Returns an
      // unsubscribe fn. Registering flags invokes as `preferInline`.
      subscribe(cb) {
        if (typeof cb !== "function") {
          throw new TypeError("thclaws.approvals.subscribe: cb must be a function");
        }
        hasApprovalHandler = true;
        const set = ensureSub("approval_request");
        const wrapped = (payload) => {
          cb({
            id: payload && payload.approvalId,
            toolName: payload && payload.toolName,
            input: payload && payload.input,
            summary: payload && payload.summary,
          });
        };
        set.add(wrapped);
        return () => {
          set.delete(wrapped);
          if (set.size === 0) hasApprovalHandler = false;
        };
      },
      // decision: "allow" | "allow_for_session" | "deny".
      respond(id, decision) {
        if (id == null) return;
        const d =
          decision === "allow" || decision === "allow_for_session"
            ? decision
            : "deny";
        fireAndForget("approval_respond", { approvalId: id, decision: d });
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
      // Tier 3: explicit delete. set(key, null) used to be the
      // convention; this is a clearer surface for shell authors.
      delete(key) {
        if (typeof key !== "string") {
          return Promise.reject(
            new TypeError("thclaws.storage.delete: key must be a string"),
          );
        }
        return send("storage_delete", { key });
      },
    },

    // ── dev-plan/39 Tier 3 — RPC + permissions surface ────────────
    //
    // Sugar over thclaws.tools.invoke that matches the dev-plan/39
    // documented contract: `await thclaws.callTool("Bash", {cmd:"ls"})`.
    // Identical wire format under the hood; the new name is what
    // marketplace shells should target going forward.
    callTool(name, args) {
      if (typeof name !== "string" || !name) {
        return Promise.reject(
          new TypeError("thclaws.callTool: name must be a non-empty string"),
        );
      }
      return send("tool_invoke", {
        name,
        args: args ?? null,
        preferInline: hasApprovalHandler,
      });
    },

    // Tier 3 stub. The shell asks for permission to take an action
    // and the user inline-approves via a custom widget (vs the full-
    // screen system modal). Engine wiring lands in Tier 3 follow-up;
    // for now, returning a clear rejection lets shells code against
    // the contract without crashing.
    // The shell asks the user to sign off on its OWN action (distinct
    // from tool-call inline approval, which is thclaws.approvals.*).
    // Routes through the session approver; resolves to { approved }.
    awaitApproval(request) {
      return send("await_approval", request ?? {});
    },

    // Run a turn and stream its events as an AsyncIterable in arrival
    // order: `{type:"text", delta}`, `{type:"tool_call", name, label,
    // input}`, `{type:"tool_result", name, output}` — terminated by the
    // turn's `done` (or surfaced as a rejection on `error`). Built on the
    // same gui_shell_event stream the backend already emits
    // (render_gui_shell_dispatch); no separate engine path needed.
    //
    //   for await (const ev of thclaws.streamTurn("draft a poem")) {
    //     if (ev.type === "text") out.append(ev.delta);
    //     else if (ev.type === "tool_call") showSpinner(ev.label);
    //   }
    streamTurn(prompt, opts) {
      const queue = [];
      const waiters = [];
      let done = false;
      let pendingError = null;
      const unsubs = [];
      const cleanup = () => {
        while (unsubs.length) {
          const u = unsubs.pop();
          try { u(); } catch (e) { /* already gone */ }
        }
      };
      const push = (value) => {
        const item = { done: false, value };
        if (waiters.length) waiters.shift()(item);
        else queue.push(item);
      };
      const finish = (err) => {
        if (done) return;
        done = true;
        pendingError = err || null;
        cleanup();
        // Wake any parked next() so it resolves/rejects instead of hanging.
        while (waiters.length) {
          if (err) waiters.shift()({ __err: err });
          else waiters.shift()({ done: true, value: undefined });
        }
      };
      unsubs.push(
        window.thclaws.on("text", (p) =>
          push({ type: "text", delta: typeof p === "string" ? p : (p && p.delta) || "" }),
        ),
      );
      unsubs.push(
        window.thclaws.on("tool_call", (p) =>
          push({ type: "tool_call", name: p && p.name, label: p && p.label, input: p && p.input }),
        ),
      );
      unsubs.push(
        window.thclaws.on("tool_result", (p) =>
          push({ type: "tool_result", name: p && p.name, output: p && p.output }),
        ),
      );
      unsubs.push(window.thclaws.on("done", () => finish(null)));
      unsubs.push(
        window.thclaws.on("error", (p) =>
          finish(new Error((p && (p.error || p.message)) || "turn error")),
        ),
      );
      window.thclaws.run(prompt, opts).catch((e) => finish(e));
      return {
        [Symbol.asyncIterator]() {
          return {
            next() {
              if (queue.length) return Promise.resolve(queue.shift());
              if (pendingError) {
                const e = pendingError;
                pendingError = null;
                return Promise.reject(e);
              }
              if (done) return Promise.resolve({ done: true, value: undefined });
              return new Promise((resolve, reject) => {
                waiters.push((item) => {
                  if (item && item.__err) reject(item.__err);
                  else resolve(item);
                });
              });
            },
            return() {
              finish(null);
              return Promise.resolve({ done: true, value: undefined });
            },
          };
        },
      };
    },

    // Upload a Blob into the workspace's `_uploads/` and resolve to a
    // servable asset URL the shell can drop into <img src>/<a href>.
    // Base64s over the IPC channel (works in desktop + serve; per-user
    // isolated in multiuser). Capped engine-side (~25 MB); reject early
    // on obviously-oversize blobs to avoid a wasted round-trip.
    uploadFile(blob, name) {
      if (!(blob instanceof Blob)) {
        return Promise.reject(new TypeError("thclaws.uploadFile: first arg must be a Blob"));
      }
      const MAX = 25 * 1024 * 1024;
      if (blob.size > MAX) {
        return Promise.reject(
          new Error(`thclaws.uploadFile: blob is ${blob.size} bytes, over the ${MAX}-byte cap`),
        );
      }
      return blob
        .arrayBuffer()
        .then((buf) => {
          // Chunked base64 — String.fromCharCode(...huge) blows the arg
          // limit, so encode in 32 KB slices.
          const u8 = new Uint8Array(buf);
          let bin = "";
          const CHUNK = 0x8000;
          for (let i = 0; i < u8.length; i += CHUNK) {
            bin += String.fromCharCode.apply(null, u8.subarray(i, i + CHUNK));
          }
          const dataB64 = btoa(bin);
          return send("upload_file", {
            name: name || blob.name || "upload.bin",
            mime: blob.type || "application/octet-stream",
            dataB64,
          });
        })
        .then((result) => (result && result.url) || result);
    },

    // Host UI integration — full-screen control. In full-screen UI
    // mode the host hides all its chrome (tab bar, sidebar, status
    // bar) so the shell owns the viewport. The host always keeps a
    // keyboard escape (⌘⇧U / Ctrl⇧U) and a fallback exit affordance,
    // but a well-built shell should render its OWN exit control as
    // part of its chrome so the host's fallback never has to occlude
    // shell content. The reference pattern:
    //
    //   thclaws.ui.onFullscreen((active) => {
    //     myExitButton.hidden = !active;       // only meaningful in FS
    //     if (active) thclaws.ui.claimExitControl();  // hide host chip
    //   });
    //   myExitButton.onclick = () => thclaws.ui.exitFullscreen();
    //
    // `claimExitControl()` tells the host to suppress its own fallback
    // chip (the keyboard escape + a brief on-entry hint stay as the
    // safety net). Only call it once you've actually rendered a
    // working exit control of your own.
    ui: {
      // True while the host is showing this shell full-screen. Updated
      // from the host's `fullscreen` events; starts false.
      isFullscreen: false,

      // The host's resolved theme ("light" | "dark"), updated from the
      // host's `theme` events. The bridge also mirrors this onto
      // `document.documentElement[data-theme]` + `color-scheme`, so a
      // shell can theme purely in CSS. Starts "dark" (the historical
      // default) until the first host event arrives.
      theme: "dark",

      // Ask the host to leave full-screen UI mode (reveals the tab bar
      // etc.). No-op in Mode B (standalone `--serve --gui-shell`, where
      // the shell already owns the whole page and there's no chrome to
      // restore).
      exitFullscreen() {
        if (isModeB) return;
        parent.postMessage(
          { ns: "thclaws-shell", type: "hotkey", key: "exit-fullscreen-ui" },
          "*",
        );
      },

      // Toggle the host's full-screen UI (enter if windowed, leave if
      // full-screen) — the same action as the ⌘⇧U / Ctrl⇧U hotkey. No-op
      // in Mode B, where the shell already owns the whole page.
      toggleFullscreen() {
        if (isModeB) return;
        parent.postMessage(
          { ns: "thclaws-shell", type: "hotkey", key: "toggle-fullscreen-ui" },
          "*",
        );
      },

      // Ask the host to switch the app theme ("light" | "dark"). The host
      // persists it (same path as Settings), applies it app-wide, and
      // echoes the resolved theme back as a `theme` event so this shell
      // re-themes too. In Mode B there's no host to ask, so we flip the
      // shell document's own `data-theme` directly.
      setTheme(mode) {
        const next = mode === "light" ? "light" : "dark";
        if (isModeB) {
          window.thclaws.ui.theme = next;
          try {
            const de = document.documentElement;
            de.setAttribute("data-theme", next);
            de.style.colorScheme = next;
          } catch (e) {
            /* document not ready */
          }
          handleShellEvent({ event: "theme", payload: { mode: next } });
          return;
        }
        parent.postMessage(
          { ns: "thclaws-shell", type: "ui", key: "set-theme", mode: next },
          "*",
        );
      },

      // Convenience: flip between light and dark from the current theme.
      toggleTheme() {
        window.thclaws.ui.setTheme(
          window.thclaws.ui.theme === "dark" ? "light" : "dark",
        );
      },

      // Tell the host this shell renders its own exit control, so the
      // host can hide its fallback chip. Safe to call repeatedly.
      claimExitControl() {
        if (isModeB) return;
        parent.postMessage(
          { ns: "thclaws-shell", type: "ui", key: "exit-control-claimed" },
          "*",
        );
      },

      // Subscribe to full-screen state changes. Fires immediately with
      // the current state so callers don't miss the initial value.
      // Returns an unsubscribe function.
      onFullscreen(callback) {
        if (typeof callback !== "function") {
          throw new TypeError("thclaws.ui.onFullscreen: callback must be a function");
        }
        const unsub = window.thclaws.on("fullscreen", (p) =>
          callback(!!(p && p.active)),
        );
        // Replay current state on the next tick so subscribers added
        // before the first host event still get an initial call.
        Promise.resolve().then(() => callback(window.thclaws.ui.isFullscreen));
        return unsub;
      },

      // Subscribe to host theme changes. Fires immediately with the
      // current theme ("light" | "dark"). Most shells won't need this —
      // theme-aware CSS keyed on `:root[data-theme]` is enough — but
      // it's here for shells that style via JS (e.g. canvas/charts).
      onTheme(callback) {
        if (typeof callback !== "function") {
          throw new TypeError("thclaws.ui.onTheme: callback must be a function");
        }
        const unsub = window.thclaws.on("theme", (p) =>
          callback(p && p.mode === "light" ? "light" : "dark"),
        );
        Promise.resolve().then(() => callback(window.thclaws.ui.theme));
        return unsub;
      },
    },

    // Active-model widget surface. Gated host-side by the shell's
    // manifest permissions: `model.read` (get/list) and `model.write`
    // (set). Without them the calls reject — <thc-model> then renders
    // nothing. `set` changes the app-wide model (same as the sidebar
    // picker), so every shell's onChange fires.
    model: {
      // Last {provider, model} seen via a "model" event; null until one
      // arrives. onChange replays this on subscribe.
      current: null,

      get() {
        return send("model_get", {});
      },
      list() {
        return send("model_list", {});
      },
      set(id) {
        if (typeof id !== "string" || !id) {
          return Promise.reject(
            new TypeError("thclaws.model.set: id must be a non-empty string"),
          );
        }
        return send("model_set", { model: id });
      },

      // Fires when the active model changes (from this shell, another
      // shell, or the main sidebar). Replays the current value on the
      // next tick. Returns an unsubscribe function.
      onChange(callback) {
        if (typeof callback !== "function") {
          throw new TypeError("thclaws.model.onChange: callback must be a function");
        }
        const unsub = window.thclaws.on("model", (p) => callback(p));
        Promise.resolve().then(() => {
          if (window.thclaws.model.current) callback(window.thclaws.model.current);
        });
        return unsub;
      },
    },

    // Sessions / chat-history API (dev-plan/33). Drives the SAME shared
    // session Chat uses: list past sessions, load one (its history is
    // returned AND the agent is primed so run() continues it), start a
    // new one, rename, delete. Needs `session.list`/`session.read`
    // (reads) and `session.write` (new/rename/delete) in the manifest.
    // Native Agent Teams: never routes through the shared run/cancel API.
    team: {
      snapshot() { return send("team", { action: "snapshot" }); },
      manage(operation) { return send("team", { action: "manage", operation }); },
      message(agent, targetSession, text) {
        return send("team", { action: "message", agent, targetSession, text });
      },
      stop(agent, targetSession) {
        return send("team", { action: "stop", agent, targetSession });
      },
      history(agent, targetSession) {
        return send("team", { action: "history", agent, targetSession });
      },
    },

    sessions: {
      // -> { sessions: [{ id, title, updatedAt, messageCount, model }] }
      list() {
        return send("session_list", {});
      },
      // -> { messages: [{ role: "user"|"bot", text }] }
      load(id) {
        if (typeof id !== "string" || !id) {
          return Promise.reject(
            new TypeError("thclaws.sessions.load: id must be a non-empty string"),
          );
        }
        return send("session_load", { loadId: id });
      },
      // Start a fresh session (becomes the active one). -> { ok: true }
      new() {
        return send("session_new", {});
      },
      rename(id, title) {
        if (typeof id !== "string" || !id) {
          return Promise.reject(
            new TypeError("thclaws.sessions.rename: id must be a non-empty string"),
          );
        }
        return send("session_rename", { renameId: id, title: String(title ?? "") });
      },
      delete(id) {
        if (typeof id !== "string" || !id) {
          return Promise.reject(
            new TypeError("thclaws.sessions.delete: id must be a non-empty string"),
          );
        }
        return send("session_delete", { deleteId: id });
      },
    },

    // Schedule API (settings "Schedule" panel). Needs `schedule.read`
    // (list) / `schedule.write` (create/delete/toggle).
    schedule: {
      // -> { schedules: [{ id, cron, runAt, prompt, enabled, lastRun }] }
      list() {
        return send("schedule_list", {});
      },
      create(prompt, cron) {
        return send("schedule_create", { prompt: String(prompt ?? ""), cron: String(cron ?? "") });
      },
      delete(id) {
        return send("schedule_delete", { scheduleId: String(id ?? "") });
      },
      setEnabled(id, enabled) {
        return send("schedule_toggle", { scheduleId: String(id ?? ""), enabled: !!enabled });
      },
    },

    // Heartbeat API (settings "Heartbeat" panel) — proactive check-ins as
    // a reserved schedule. Intervals: "off" | "30m" | "1h" | "4h" | "1d".
    // Needs `schedule.read` / `schedule.write`.
    heartbeat: {
      get() {
        return send("heartbeat_get", {});
      },
      set(interval) {
        return send("heartbeat_set", { interval: String(interval ?? "off") });
      },
    },

    // Skills API (settings "Skills" panel). list/view need `skills.read`;
    // save/delete operate on PROJECT skills and need `skills.write`.
    skills: {
      /**
       * Install a skill from an https:// git repo or .zip, the same as
       * `/skill install`. Downloads and unpacks only.
       * @param {string} url
       * @param {{name?: string, user?: boolean}} [opts] name overrides
       *   the one derived from the URL; user scope installs for every
       *   project (default is this workspace).
       */
      install(url, opts) {
        if (typeof url !== "string" || !url.trim()) {
          return Promise.reject(
            new TypeError("thclaws.skills.install: url must be a non-empty string"),
          );
        }
        const o = opts || {};
        return send("skill_install", {
          url: url.trim(),
          name: typeof o.name === "string" ? o.name.trim() : "",
          user: !!o.user,
        });
      },
      // -> { skills: [{ name, description, whenToUse, editable }] }
      list() {
        return send("skills_list", {});
      },
      // -> { content, editable }
      get(name) {
        return send("skills_get", { skillName: String(name ?? "") });
      },
      save(name, content) {
        return send("skills_save", { skillName: String(name ?? ""), content: String(content ?? "") });
      },
      delete(name) {
        return send("skills_delete", { skillName: String(name ?? "") });
      },
    },

    // Knowledge write API (settings "Knowledge" panel) — pairs with the
    // read-only thclaws.kms.*. Needs `kms.write`. Upload flow: uploadFile()
    // the document first, then ingest(kmsName, uploadedPath).
    knowledge: {
      create(name) {
        return send("kms_create", { kmsName: String(name ?? "") });
      },
      ingest(kmsName, path) {
        return send("kms_ingest", { kmsName: String(kmsName ?? ""), path: String(path ?? "") });
      },
    },

    // Composer mode selector — permission mode "auto" | "ask". get is
    // ungated; set needs `mode.write`.
    mode: {
      get() {
        return send("mode_get", {});
      },
      set(mode) {
        return send("mode_set", { mode: String(mode ?? "auto") });
      },
    },

    // Profile facts (engine-side): { memberId, workspace, multiuser }.
    // Needs `profile.read`. Email/password live in the cloud control plane.
    // Provider credentials (BYOK). Needs `keys.write` — a shell asking
    // for this is asking to hold provider secrets, so it must be
    // declared in its manifest and visible to whoever installs it.
    // Write-only by design: there is no getter, so a shell can set a key
    // it was given but can never read back one already stored.
    // Connectors = the MCP servers this workspace talks to. Reads need
    // `connectors.read`; add/remove need `connectors.write`.
    //
    // `add` is HTTP-only by design. A stdio connector names a command
    // the engine spawns, so accepting one here would let a shell run
    // arbitrary local code — those stay a desktop `/mcp add` decision.
    connectors: {
      list() {
        return send("connectors_list", {});
      },
      /**
       * @param {{name: string, url: string, headers?: Record<string,string>}} spec
       * Header values are stored verbatim in mcp.json, so prefer a
       * `${ENV_VAR}` placeholder over a literal secret — the engine
       * resolves it from the environment at connect time.
       */
      add(spec) {
        const { name, url, headers } = spec || {};
        if (typeof name !== "string" || !name.trim()) {
          return Promise.reject(
            new TypeError("thclaws.connectors.add: name must be a non-empty string"),
          );
        }
        if (typeof url !== "string" || !url.trim()) {
          return Promise.reject(
            new TypeError("thclaws.connectors.add: url must be a non-empty string"),
          );
        }
        return send("connector_add", { name: name.trim(), url: url.trim(), headers: headers || {} });
      },
      remove(name) {
        if (typeof name !== "string" || !name) {
          return Promise.reject(
            new TypeError("thclaws.connectors.remove: name must be a non-empty string"),
          );
        }
        return send("connector_remove", { name });
      },
    },

    // One-shot completion on whatever model the session is using right
    // now. For a language-model step that serves the shell's own UI —
    // rewriting a prompt, naming something — not the user's chat. Needs
    // `llm.complete`; the reply is `{text, model}`.
    llm: {
      /**
       * @param {{prompt: string, system?: string, maxTokens?: number}} spec
       */
      complete(spec) {
        const { prompt, system, maxTokens } = spec || {};
        if (typeof prompt !== "string" || !prompt.trim()) {
          return Promise.reject(
            new TypeError("thclaws.llm.complete: prompt must be a non-empty string"),
          );
        }
        return send("llm_complete", {
          prompt: prompt.trim(),
          system: typeof system === "string" ? system : "",
          maxTokens: Number.isFinite(maxTokens) ? maxTokens : undefined,
        });
      },
    },

    // Installed plugin bundles (skills + commands + agents + MCP
    // servers). Reads need `plugins.read`; enable/disable/uninstall need
    // `plugins.write`. There is no install(): installing fetches and
    // unpacks code, which stays a desktop `/plugin install` decision.
    plugins: {
      list() {
        return send("plugins_list", {});
      },
      /**
       * Install from an https:// git repo or .zip. Downloads and
       * unpacks only — nothing in the archive runs at install time.
       * @param {string} url
       * @param {{user?: boolean}} [opts] user scope = every project on
       *   this machine; default is this workspace.
       */
      install(url, opts) {
        if (typeof url !== "string" || !url.trim()) {
          return Promise.reject(
            new TypeError("thclaws.plugins.install: url must be a non-empty string"),
          );
        }
        return send("plugin_install", { url: url.trim(), user: !!(opts && opts.user) });
      },
      setEnabled(name, enabled) {
        if (typeof name !== "string" || !name) {
          return Promise.reject(
            new TypeError("thclaws.plugins.setEnabled: name must be a non-empty string"),
          );
        }
        return send("plugin_set_enabled", { name, enabled: !!enabled });
      },
      remove(name) {
        if (typeof name !== "string" || !name) {
          return Promise.reject(
            new TypeError("thclaws.plugins.remove: name must be a non-empty string"),
          );
        }
        return send("plugin_remove", { name });
      },
    },

    keys: {
      set(provider, key) {
        if (typeof provider !== "string" || !provider) {
          return Promise.reject(
            new TypeError("thclaws.keys.set: provider must be a non-empty string"),
          );
        }
        if (typeof key !== "string" || !key.trim()) {
          return Promise.reject(new TypeError("thclaws.keys.set: key must be non-empty"));
        }
        return send("key_set", { provider, key });
      },
    },

    profile: {
      // -> { memberId, displayName, workspace, multiuser }
      //
      // `displayName` is set only where there IS a login — a cloud
      // workspace. On desktop it comes back null and a shell should
      // render no name at all rather than inventing a placeholder.
      get() {
        return send("profile_get", {});
      },
    },

    // Memory API (dev-plan/33 — settings "Memory" panel). Core memory is
    // the MEMORY.md index injected into every turn. Needs `memory.read`
    // (get) / `memory.write` (set).
    memory: {
      // -> { core: "<MEMORY.md contents>" }
      getCore() {
        return send("memory_get", {});
      },
      // Overwrite core memory. -> { ok: true }
      setCore(text) {
        return send("memory_set", { core: String(text ?? "") });
      },
    },

    // Deterministic knowledge-base API (needs `kms.read`). No LLM — reads
    // the KMS store directly instead of prompting the agent.
    kms: {
      // -> { kmss: [{ name, scope, active }] }
      list() {
        return send("kms_list", {});
      },
      // -> { kms, pages:[{…}], sources:[{…}] }
      browse(name) {
        if (typeof name !== "string" || !name) {
          return Promise.reject(
            new TypeError("thclaws.kms.browse: name must be a non-empty string"),
          );
        }
        return send("kms_browse", { name: name });
      },
    },

    // Deterministic research-job API (needs `research.read`). No LLM —
    // reads the live job registry (running + recently-completed), the real
    // source of {status, score, phase, …}.
    research: {
      // -> { jobs: [{ id, query, status, phase, iterations_done,
      //              source_count, score, kms_target, result_page, error }] }
      list() {
        return send("research_list", {});
      },
      // -> { job: {…} | null }
      get(id) {
        return send("research_get", { jobId: id });
      },
    },

    // Tier 3: read-only access to the shell's declared permissions
    // (from shell.json). Lets shell code disable UI for actions the
    // user didn't grant rather than calling and getting denied.
    permissions: {
      list() {
        return send("permissions_list", {});
      },
      has(action) {
        return window.thclaws.permissions
          .list()
          .then((list) => Array.isArray(list) && list.includes(action))
          .catch(() => false);
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
    // Forward parent-app hotkeys that the iframe's focus would
    // otherwise swallow. The full-screen-UI toggle (⌘⇧U / Ctrl⇧U)
    // lives in the parent React app, so the parent's
    // window.addEventListener("keydown") never fires while the user
    // is typing inside the shell. Posting a `hotkey` envelope lets
    // the parent run its handler regardless of focus.
    window.addEventListener(
      "keydown",
      (e) => {
        const isMac =
          typeof navigator !== "undefined" &&
          navigator.platform.startsWith("Mac");
        const modOk = isMac
          ? e.metaKey && !e.ctrlKey && !e.altKey && e.shiftKey
          : e.ctrlKey && !e.metaKey && !e.altKey && e.shiftKey;
        if (!modOk) return;
        const key = (e.key || "").toLowerCase();
        if (key !== "u") return;
        e.preventDefault();
        e.stopImmediatePropagation();
        parent.postMessage(
          { ns: "thclaws-shell", type: "hotkey", key: "toggle-fullscreen-ui" },
          "*",
        );
      },
      { capture: true },
    );
  }

  // Copy selected text on Cmd/Ctrl+C from anywhere in the shell. The wry
  // desktop webview blocks navigator.clipboard inside a shell, so in Mode A
  // route the selection to the host (which writes it via the native clipboard
  // IPC); in Mode B (standalone browser) the native clipboard works directly.
  // A selection inside a native input/textarea reports empty via getSelection,
  // so a shell composer's own copy is left untouched. Runs in both modes.
  const copySelection = (text) => {
    if (!text) return;
    if (isModeB) {
      try {
        if (navigator.clipboard) navigator.clipboard.writeText(text);
      } catch (err) {
        /* clipboard unavailable */
      }
      return;
    }
    parent.postMessage(
      { ns: "thclaws-shell", type: "clipboard-write", text: text },
      "*",
    );
  };
  if (window.thclaws) window.thclaws.copy = copySelection;
  window.addEventListener(
    "keydown",
    (e) => {
      const isMac =
        typeof navigator !== "undefined" &&
        navigator.platform.startsWith("Mac");
      const mod = isMac ? e.metaKey : e.ctrlKey;
      if (!mod || e.altKey) return;
      if ((e.key || "").toLowerCase() !== "c") return;
      let sel = "";
      try {
        sel = (window.getSelection() || "").toString();
      } catch (err) {
        sel = "";
      }
      if (!sel) return;
      e.preventDefault();
      copySelection(sel);
    },
    { capture: true },
  );
})();
