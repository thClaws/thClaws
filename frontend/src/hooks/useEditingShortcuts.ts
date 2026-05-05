import { useEffect } from "react";
import { send, subscribe } from "./useIPC";

/**
 * Wire up macOS-style editing shortcuts (Cmd+C / Cmd+X / Cmd+V / Cmd+A /
 * Cmd+Z) for `<input>` and `<textarea>` elements inside the wry webview.
 *
 * Wry on macOS doesn't forward the OS's edit-menu shortcuts into the
 * webview, and `navigator.clipboard` is blocked in the wry security
 * context (no secure origin, focus-gated read). So we go through
 * native: `clipboard_read` / `clipboard_write` IPC messages handled by
 * the `arboard` crate in gui.rs.
 *
 * contentEditable (tiptap) elements handle their own shortcuts —
 * we skip them so their undo stack stays authoritative.
 */
export function useEditingShortcuts() {
  useEffect(() => {
    const isMac = /Mac|iPhone|iPad/.test(navigator.platform);

    const onKey = (e: KeyboardEvent) => {
      const modifier = isMac ? e.metaKey : e.ctrlKey;
      if (!modifier) return;
      if (e.altKey) return;

      const target = e.target as HTMLElement | null;
      if (!target) return;
      const tag = target.tagName;
      const isInput = tag === "INPUT" || tag === "TEXTAREA";

      const key = e.key.toLowerCase();

      // Handle Cmd+C for any text selection (not just input/textarea)
      if (key === "c" && !e.shiftKey && !isInput) {
        const sel = window.getSelection();
        if (sel && sel.toString().length > 0) {
          e.preventDefault();
          send({ type: "clipboard_write", text: sel.toString() });
        }
        return;
      }

      if (!isInput) return;
      const field = target as HTMLInputElement | HTMLTextAreaElement;
      if (field.disabled || field.readOnly) return;

      if (key === "v" && !e.shiftKey) {
        e.preventDefault();
        // Request clipboard text from the native side; subscribe for
        // exactly one response, then unsubscribe. Prefer the base64
        // payload — it sidesteps the JS-bridge escape quirks that
        // drop or corrupt long text (U+2028/U+2029, size limits).
        const unsub = subscribe((msg) => {
          if (msg.type === "clipboard_text") {
            unsub();
            if (msg.ok) {
              const text =
                typeof msg.text_b64 === "string"
                  ? decodeBase64Utf8(msg.text_b64)
                  : typeof msg.text === "string"
                    ? msg.text
                    : "";
              if (text) insertAtCursor(field, text);
            }
          }
        });
        send({ type: "clipboard_read" });
      } else if (key === "c" && !e.shiftKey) {
        const { selectionStart, selectionEnd, value } = field;
        if (
          selectionStart !== null &&
          selectionEnd !== null &&
          selectionEnd > selectionStart
        ) {
          e.preventDefault();
          send({
            type: "clipboard_write",
            text: value.slice(selectionStart, selectionEnd),
          });
        }
      } else if (key === "x" && !e.shiftKey) {
        const { selectionStart, selectionEnd, value } = field;
        if (
          selectionStart !== null &&
          selectionEnd !== null &&
          selectionEnd > selectionStart
        ) {
          e.preventDefault();
          send({
            type: "clipboard_write",
            text: value.slice(selectionStart, selectionEnd),
          });
          deleteSelection(field);
        }
      } else if (key === "a" && !e.shiftKey) {
        e.preventDefault();
        field.select();
      } else if (key === "z") {
        // Native input undo/redo via execCommand still works even
        // without the OS edit menu, call it explicitly.
        e.preventDefault();
        document.execCommand(e.shiftKey ? "redo" : "undo");
      }
    };

    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
}

// Decode a UTF-8 base64 string in a way that handles non-ASCII
// correctly. `atob` gives us Latin-1 bytes; we reinterpret them
// through TextDecoder so é / Thai / emoji survive round-trip.
function decodeBase64Utf8(b64: string): string {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder("utf-8").decode(bytes);
}

function insertAtCursor(
  field: HTMLInputElement | HTMLTextAreaElement,
  text: string,
) {
  const start = field.selectionStart ?? field.value.length;
  const end = field.selectionEnd ?? field.value.length;
  if (typeof field.setRangeText === "function") {
    field.setRangeText(text, start, end, "end");
  } else {
    field.value = field.value.slice(0, start) + text + field.value.slice(end);
    const caret = start + text.length;
    field.setSelectionRange(caret, caret);
  }
  field.dispatchEvent(new Event("input", { bubbles: true }));
}

function deleteSelection(field: HTMLInputElement | HTMLTextAreaElement) {
  const start = field.selectionStart ?? 0;
  const end = field.selectionEnd ?? 0;
  if (typeof field.setRangeText === "function") {
    field.setRangeText("", start, end, "end");
  } else {
    field.value = field.value.slice(0, start) + field.value.slice(end);
    field.setSelectionRange(start, start);
  }
  field.dispatchEvent(new Event("input", { bubbles: true }));
}
