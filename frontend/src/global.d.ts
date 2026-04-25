/** Global type augmentations for the wry WebView IPC bridge. */
declare global {
  interface Window {
    /** Injected by wry — posts JSON messages to the Rust backend. */
    ipc?: { postMessage(msg: string): void };
    /** Registered by useIPC — called by Rust via evaluate_script. */
    __thclaws_dispatch?: (json: string) => void;
  }
}

export {};
