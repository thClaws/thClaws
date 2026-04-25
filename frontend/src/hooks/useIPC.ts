/**
 * IPC bridge between React frontend and wry Rust backend.
 *
 * JS → Rust: window.ipc.postMessage(JSON.stringify({type, ...payload}))
 * Rust → JS: window.__thclaws_dispatch(JSON.stringify({type, ...payload}))
 *
 * The dispatch function is registered globally so evaluate_script from
 * Rust can call it. React components subscribe via addEventListener.
 */

export type IPCMessage = {
  type: string;
  [key: string]: unknown;
};

// Send a message to Rust
export function send(msg: IPCMessage) {
  if (window.ipc) {
    window.ipc.postMessage(JSON.stringify(msg));
  } else {
    console.warn("[ipc] no backend — running in browser dev mode?", msg);
  }
}

// Subscribe to messages from Rust
type Handler = (msg: IPCMessage) => void;
const handlers = new Set<Handler>();

export function subscribe(handler: Handler): () => void {
  handlers.add(handler);
  return () => handlers.delete(handler);
}

// Called by Rust via evaluate_script
window.__thclaws_dispatch = (json: string) => {
  try {
    const msg: IPCMessage = JSON.parse(json);
    handlers.forEach((h) => h(msg));
  } catch (e) {
    console.error("[ipc] dispatch parse error:", e);
  }
};
