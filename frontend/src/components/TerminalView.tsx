import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { send, subscribe } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";
import bannerText from "../../../banner.txt?raw";

// xterm needs CRLF; the shared banner file uses plain LF so the Rust REPL
// can println! it unchanged.
const BANNER = bannerText.replace(/\n/g, "\r\n");

// xterm.js palettes keyed to the app's resolved theme. selectionBackground
// etc. aren't optional: if omitted, xterm's default is near-invisible
// against our dark bg (selection happens, copy works, but the user can't
// see what they've highlighted).
const TERMINAL_PALETTES = {
  dark: {
    background: "#0a0a0a",
    foreground: "#e6e6e6",
    cursor: "#e6e6e6",
    selectionBackground: "#3a4858",
    selectionInactiveBackground: "#2a3440",
  },
  light: {
    background: "#fafafa",
    foreground: "#1a1a1a",
    cursor: "#1a1a1a",
    selectionBackground: "#b4d5fe",
    selectionInactiveBackground: "#d4e4fa",
  },
} as const;

const PROMPT = "\x1b[32m❯ \x1b[0m";

interface Props {
  active: boolean;
}

function b64decode(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function TerminalView({ active }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const { resolved: themeMode } = useTheme();
  const themeModeRef = useRef(themeMode);
  useEffect(() => { themeModeRef.current = themeMode; }, [themeMode]);

  useEffect(() => {
    if (!ref.current || termRef.current) return;

    const term = new Terminal({
      fontFamily: "Menlo, Monaco, 'Courier New', monospace",
      fontSize: 13,
      cursorBlink: true,
      scrollback: 10000,
      theme: TERMINAL_PALETTES[themeModeRef.current],
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(ref.current);
    fit.fit();
    termRef.current = term;
    fitRef.current = fit;

    // Local input buffer — the shared session works in whole lines, so
    // we collect keystrokes here and submit on Enter. xterm.js gives us
    // raw input events; we echo printable chars locally so the user sees
    // what they're typing while the agent is silent (the shared session
    // doesn't echo back — it just executes lines).
    let lineBuffer = "";
    // True when the prompt (`❯ `) is the current visible line and any
    // incoming `terminal_data` should erase it before writing event
    // content. False once we're inside a turn's event stream — at
    // which point chunks must concatenate, not overwrite each other.
    // Reset to true on chat_done (next prompt is rendered) and on
    // local submit (line erased, no prompt visible until next turn).
    let promptShowing = false;
    // Prompt-history ring for Up/Down arrow recall (bash-style). The
    // array grows on every successful submit; `historyIndex === -1`
    // means "not navigating — lineBuffer is the user's own typing".
    // Anything else is an index into `history`, oldest at 0, newest
    // at `history.length - 1`.
    const history: string[] = [];
    let historyIndex = -1;
    // Snapshot of whatever the user had been typing before they
    // started pressing Up, so Down past the newest entry restores it
    // instead of clearing the line unexpectedly.
    let savedDraft = "";

    const replaceLineBuffer = (next: string) => {
      term.write("\x1b[2K\r");
      writePrompt();
      lineBuffer = next;
      if (next.length > 0) term.write(next);
    };

    // Record a successfully-submitted prompt in the recall ring.
    // Skips exact duplicates of the most recent entry (Ctrl+↑ in bash
    // etc. — no value in cycling through "ls ls ls"). Also resets the
    // navigation cursor so the next Up arrow starts from the newest.
    const HISTORY_MAX = 200;
    const pushHistory = (entry: string) => {
      const trimmed = entry.trim();
      if (trimmed.length === 0) return;
      if (history.length > 0 && history[history.length - 1] === trimmed) {
        historyIndex = -1;
        savedDraft = "";
        return;
      }
      history.push(trimmed);
      if (history.length > HISTORY_MAX) history.shift();
      historyIndex = -1;
      savedDraft = "";
    };

    const writePrompt = () => {
      term.write(PROMPT);
      promptShowing = true;
    };

    // Print a banner + first prompt so the tab doesn't open empty.
    term.write(
      "\x1b[36m" +
        BANNER +
        "\x1b[0m" +
        "\r\n" +
        "\x1b[2mthClaws — type a message, or /help for commands\x1b[0m\r\n",
    );
    writePrompt();

    // Native clipboard via the IPC bridge — wry blocks navigator.clipboard.
    term.attachCustomKeyEventHandler((e: KeyboardEvent) => {
      const isMac = navigator.platform.startsWith("Mac");
      const mod = isMac ? e.metaKey : e.ctrlKey && e.shiftKey;

      // Plain Ctrl+C:
      //  - non-empty input line → clear the line (same as bash)
      //  - empty line → request abort of the in-flight turn via the
      //    shell_cancel IPC (the equivalent of the SIGINT we used to
      //    send to the --cli PTY child)
      if (
        e.type === "keydown" &&
        e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey &&
        (e.key === "c" || e.key === "C")
      ) {
        if (lineBuffer.length > 0) {
          term.write("\x1b[2K\r");
          lineBuffer = "";
          writePrompt(); // also flips promptShowing back on
        } else {
          send({ type: "shell_cancel" });
        }
        return false;
      }

      // Cmd/Ctrl+L: clear screen
      if (
        e.type === "keydown" &&
        ((isMac && e.metaKey) || (!isMac && e.ctrlKey)) &&
        (e.key === "l" || e.key === "L")
      ) {
        term.write("\x1b[2J\x1b[H");
        writePrompt();
        term.write(lineBuffer);
        return false;
      }

      // Copy
      if (mod && e.key === "c" && e.type === "keydown") {
        const sel = term.getSelection();
        if (sel) {
          send({ type: "clipboard_write", text: sel });
          return false;
        }
        if (!isMac) return false;
      }

      // Paste
      if (mod && e.key === "v" && e.type === "keydown") {
        const unsub = subscribe((msg) => {
          if (msg.type === "clipboard_text") {
            unsub();
            if (!msg.ok) return;
            let text = "";
            if (typeof msg.text_b64 === "string") {
              const bin = atob(msg.text_b64 as string);
              const bytes = new Uint8Array(bin.length);
              for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
              text = new TextDecoder("utf-8").decode(bytes);
            } else if (typeof msg.text === "string") {
              text = msg.text as string;
            }
            if (text.length > 0) {
              if (/\r?\n/.test(text)) {
                // Multi-line paste: submit the whole thing as ONE
                // shell_input. Erase any local echo; the backend's
                // UserPrompt event will re-render the full block.
                const combined = (lineBuffer + text).replace(/\r\n/g, "\n");
                const trimmed = combined.replace(/\n+$/, "");
                lineBuffer = "";
                if (trimmed.length > 0) {
                  term.write("\x1b[2K\r");
                  send({ type: "shell_input", text: trimmed });
                }
              } else {
                // Single-line paste: insert into the line buffer and
                // echo so the user can keep editing before hitting
                // Enter.
                lineBuffer += text;
                term.write(text);
              }
            }
          }
        });
        send({ type: "clipboard_read" });
        return false;
      }

      return true;
    });

    term.onData((data) => {
      // Up / Down arrows: walk through the prompt history just like
      // bash. xterm delivers arrow keys as the escape sequences below
      // in a single onData call, so matching the raw string is enough.
      if (data === "\x1b[A") {
        if (history.length === 0) return;
        if (historyIndex === -1) {
          savedDraft = lineBuffer;
          historyIndex = history.length - 1;
        } else if (historyIndex > 0) {
          historyIndex -= 1;
        }
        replaceLineBuffer(history[historyIndex]);
        return;
      }
      if (data === "\x1b[B") {
        if (historyIndex === -1) return;
        if (historyIndex < history.length - 1) {
          historyIndex += 1;
          replaceLineBuffer(history[historyIndex]);
        } else {
          // Past the newest entry — restore whatever the user was
          // mid-typing before they started pressing Up.
          historyIndex = -1;
          replaceLineBuffer(savedDraft);
          savedDraft = "";
        }
        return;
      }
      // Any other keystroke counts as "editing the current draft" so
      // further Up/Down starts from a clean slate relative to that.
      if (historyIndex !== -1) {
        historyIndex = -1;
        savedDraft = "";
      }

      // Multi-line paste: a single onData chunk wider than one char
      // containing at least one newline is almost always a paste (or
      // bracketed-paste). Submit the WHOLE block as one shell_input so
      // the model sees it as a single prompt — not N one-line prompts
      // (which made a pasted spec echo back as "No response requested."
      // once per line, ref. dev-log audit of the ShopFlow session).
      if (data.length > 1 && /[\r\n]/.test(data)) {
        const combined = (lineBuffer + data).replace(/\r\n/g, "\n");
        const trimmed = combined.replace(/\n+$/, "");
        lineBuffer = "";
        if (trimmed.length > 0) {
          // Erase whatever the user had locally echoed — the canonical
          // UserPrompt event from the shared session will re-render
          // the whole paste in one block.
          term.write("\x1b[2K\r");
          promptShowing = false;
          pushHistory(trimmed);
          send({ type: "shell_input", text: trimmed });
        } else {
          term.write("\r\n");
          writePrompt();
        }
        return;
      }

      // xterm hands us each keystroke as a string; classify and either
      // mutate the buffer + echo, or submit on Enter.
      for (const ch of data) {
        if (ch === "\r" || ch === "\n") {
          if (lineBuffer.trim().length > 0) {
            // Erase the locally-echoed `❯ <typing>` so the canonical
            // UserPrompt event (`> text\r\n`) coming back from the
            // shared session is the visible representation.
            term.write("\x1b[2K\r");
            promptShowing = false;
            pushHistory(lineBuffer);
            send({ type: "shell_input", text: lineBuffer });
          } else {
            // Empty line: just newline + redraw prompt for the next try.
            term.write("\r\n");
            writePrompt();
          }
          lineBuffer = "";
        } else if (ch === "\x7f" || ch === "\b") {
          if (lineBuffer.length > 0) {
            lineBuffer = lineBuffer.slice(0, -1);
            // Move cursor back, overwrite with space, move back again.
            term.write("\b \b");
          }
        } else if (ch >= " " && ch !== "\x7f") {
          lineBuffer += ch;
          term.write(ch);
        }
        // Other control bytes are dropped.
      }
    });

    // Note: no resize IPC — the shared session doesn't care about
    // terminal dimensions. xterm still resizes its grid on container
    // resize via the FitAddon; we just don't forward the new size.

    // Listen for backend output. `terminal_data` carries ANSI bytes
    // already formatted for xterm; `terminal_clear` resets scrollback.
    // We also intercept assistant output to redraw the prompt after
    // each turn ends.
    const unsub = subscribe((msg) => {
      if (msg.type === "terminal_data" && typeof msg.data === "string") {
        // Erase the prompt only on the FIRST chunk after a "between
        // turns" state — once we're in a stream, subsequent chunks
        // must concatenate, not overwrite each other. Tracking
        // `promptShowing` is what differentiates "fresh prompt
        // visible, clear it before printing" from "mid-stream, just
        // append".
        const wasPrompt = promptShowing;
        if (promptShowing) {
          term.write("\x1b[2K\r");
          promptShowing = false;
        }
        const bytes = b64decode(msg.data);
        term.write(bytes);
        // Standalone info burst (prompt was up, message ends with a
        // newline — e.g. `[mcp] '…' connected`). Restore the prompt +
        // whatever the user was mid-typing so keystrokes keep their
        // chevron instead of vanishing into an invisible line.
        const endsWithNewline =
          bytes.length >= 2 &&
          bytes[bytes.length - 2] === 0x0d &&
          bytes[bytes.length - 1] === 0x0a;
        if (wasPrompt && endsWithNewline) {
          writePrompt();
          if (lineBuffer.length > 0) term.write(lineBuffer);
        }
      } else if (msg.type === "chat_done") {
        // Turn complete — newline (if needed) + fresh prompt.
        term.write("\r\n");
        writePrompt();
        term.write(lineBuffer);
      } else if (msg.type === "terminal_clear") {
        term.reset();
        term.clear();
        writePrompt();
        lineBuffer = "";
      } else if (
        msg.type === "terminal_history_replaced" &&
        typeof msg.data === "string"
      ) {
        // Session load / new session: backend already embedded clear
        // codes + (possibly empty) replayed messages. Always print a
        // fresh prompt at the end so empty histories don't leave the
        // terminal without a `❯`. Restore any in-progress typing.
        term.write(b64decode(msg.data));
        writePrompt();
        if (lineBuffer.length > 0) term.write(lineBuffer);
      }
    });

    // Trigger initial sidebar state via the legacy ack.
    send({ type: "pty_spawn" });

    const ro = new ResizeObserver((entries) => {
      const e = entries[0];
      if (!e || e.contentRect.width === 0 || e.contentRect.height === 0) return;
      try {
        fit.fit();
      } catch { /* fit() may throw on zero-size or disposed container */ }
    });
    ro.observe(ref.current);

    return () => {
      unsub();
      ro.disconnect();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, []);

  // Refit + focus when the tab becomes active.
  useEffect(() => {
    if (!active) return;
    const t = termRef.current;
    const f = fitRef.current;
    if (!t) return;
    requestAnimationFrame(() => {
      try { f?.fit(); } catch { /* fit() may throw on zero-size or disposed container */ }
      t.focus();
    });
  }, [active]);

  // Live theme swap.
  useEffect(() => {
    const t = termRef.current;
    if (!t) return;
    t.options.theme = TERMINAL_PALETTES[themeMode];
  }, [themeMode]);

  return (
    <div
      ref={ref}
      className="h-full w-full p-1.5"
      style={{ background: "var(--terminal-bg)" }}
    />
  );
}
