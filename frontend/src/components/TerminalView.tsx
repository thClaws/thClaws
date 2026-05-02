import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { send, subscribe } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";
import {
  SlashCommandPopup,
  filterCommands,
  type SlashCommandInfo,
} from "./SlashCommandPopup";
import bannerText from "../../../banner.txt?raw";

type SlashView = {
  open: boolean;
  query: string;
  index: number;
  filtered: SlashCommandInfo[];
};

const SLASH_VIEW_CLOSED: SlashView = {
  open: false,
  query: "",
  index: 0,
  filtered: [],
};

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
  modalOpen: boolean;
}

function b64decode(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function TerminalView({ active, modalOpen }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const { resolved: themeMode } = useTheme();
  const themeModeRef = useRef(themeMode);
  useEffect(() => { themeModeRef.current = themeMode; }, [themeMode]);

  // Slash-command catalogue mirrored from the backend, plus the live
  // popup state. The xterm closure keeps the line buffer locally; we
  // bridge to React via refs so `attachCustomKeyEventHandler` and
  // `onData` can read/update the popup view without re-mounting xterm.
  const [slashCommands, setSlashCommands] = useState<SlashCommandInfo[]>([]);
  const slashCommandsRef = useRef<SlashCommandInfo[]>([]);
  const [slashView, setSlashView] = useState<SlashView>(SLASH_VIEW_CLOSED);
  const slashViewRef = useRef(slashView);
  // Mirror of the agent's streaming state, used to render the Stop
  // button overlay only while a turn is in flight. Tracked the same
  // way ChatView does (true on chat_text_delta / chat_tool_call,
  // false on chat_done) — backend events arrive in both views since
  // they share the same SharedSession.
  const [streaming, setStreaming] = useState(false);
  // Bridge for the xterm onData closure (which is set up in the mount
  // useEffect and can't see React state updates). Used by the Enter
  // handler to suppress the misleading "fresh prompt" rendering while
  // a turn is in flight.
  const streamingRef = useRef(false);
  useEffect(() => {
    streamingRef.current = streaming;
  }, [streaming]);
  useEffect(() => { slashViewRef.current = slashView; }, [slashView]);
  // Bridge React clicks back into the xterm closure where lineBuffer
  // lives. Set inside the mount effect; called from the popup's onSelect.
  const acceptSlashRef = useRef<(cmd: SlashCommandInfo) => void>(() => {});

  useEffect(() => {
    send({ type: "slash_commands_list" });
    const unsub = subscribe((msg) => {
      if (msg.type === "slash_commands" && Array.isArray(msg.commands)) {
        const list = msg.commands as SlashCommandInfo[];
        slashCommandsRef.current = list;
        setSlashCommands(list);
      }
    });
    return unsub;
  }, []);

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
    // When the model invokes AskUserQuestion, the gui forwarder posts
    // an `ask_user_question` IPC envelope with `{id, question}`. We
    // capture the id here so the next submission goes back as
    // `ask_user_response` (resolves the agent's blocking oneshot)
    // instead of being treated as a new chat prompt. Cleared on
    // submit, on `chat_done`, on session swap.
    let pendingAskId: number | null = null;
    // Index into `lineBuffer` where the next character would be
    // inserted. 0 = before first char, lineBuffer.length = after last
    // char (the standard "end of line" position). Left/Right arrows,
    // Home/End, and Ctrl+A/E move this; insert / backspace mutate
    // around it. Reset to 0 whenever the buffer is cleared, and to
    // `next.length` whenever we replace the whole buffer (history
    // recall, slash accept, etc.) so the caret lands at the end of the
    // restored text.
    let cursorPos = 0;
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

    // Erase the current line and rewrite "❯ <lineBuffer>", then move
    // the visible cursor back to `cursorPos`. Called after any mid-line
    // mutation so the screen stays in sync with the buffer + caret.
    // Cheap enough to do on every keystroke since lines are short.
    const redrawLine = () => {
      term.write("\x1b[2K\r");
      writePrompt();
      if (lineBuffer.length > 0) term.write(lineBuffer);
      const back = lineBuffer.length - cursorPos;
      if (back > 0) term.write(`\x1b[${back}D`);
    };

    const replaceLineBuffer = (next: string) => {
      lineBuffer = next;
      cursorPos = next.length;
      redrawLine();
      recomputeSlash();
    };

    // Recompute the slash-popup state from the current `lineBuffer`.
    // The popup is open iff the buffer starts with `/` AND the user
    // hasn't typed a space yet (once we hit "/model gpt-5", we're past
    // composing the name). Index is preserved across keystrokes so the
    // user's selection doesn't jump back to the top on every char.
    const recomputeSlash = () => {
      const open = lineBuffer.startsWith("/") && !lineBuffer.includes(" ");
      if (!open) {
        if (slashViewRef.current.open) setSlashView(SLASH_VIEW_CLOSED);
        return;
      }
      const query = lineBuffer.slice(1);
      const filtered = filterCommands(slashCommandsRef.current, query);
      const prev = slashViewRef.current;
      let index = prev.open && prev.query === query ? prev.index : 0;
      if (index >= filtered.length) index = 0;
      setSlashView({ open: true, query, index, filtered });
    };

    // Accept a command into the line buffer + visible terminal. Always
    // appends a trailing space so the popup closes and the user can
    // immediately type args or press Enter.
    const acceptSlashCommand = (cmd: SlashCommandInfo) => {
      const next = `/${cmd.name} `;
      term.write("\x1b[2K\r");
      writePrompt();
      lineBuffer = next;
      cursorPos = next.length;
      term.write(next);
      setSlashView(SLASH_VIEW_CLOSED);
    };
    acceptSlashRef.current = acceptSlashCommand;

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

      // Slash-command popup: when open, intercept navigation/accept/
      // dismiss keys so xterm doesn't also process them as input. Only
      // fires on keydown and only when no modifiers are held — Cmd+↑
      // / Ctrl+Tab / etc. should pass through unchanged.
      if (
        e.type === "keydown" &&
        !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey
      ) {
        const sv = slashViewRef.current;
        if (sv.open && sv.filtered.length > 0) {
          if (e.key === "ArrowDown") {
            const next = (sv.index + 1) % sv.filtered.length;
            setSlashView({ ...sv, index: next });
            return false;
          }
          if (e.key === "ArrowUp") {
            const next = (sv.index - 1 + sv.filtered.length) % sv.filtered.length;
            setSlashView({ ...sv, index: next });
            return false;
          }
          if (e.key === "Tab") {
            e.preventDefault();
            const cmd = sv.filtered[sv.index];
            if (cmd) acceptSlashCommand(cmd);
            return false;
          }
          if (e.key === "Escape") {
            term.write("\x1b[2K\r");
            writePrompt();
            lineBuffer = "";
            recomputeSlash();
            return false;
          }
          if (e.key === "Enter") {
            // While the user is still composing the name (no space yet),
            // Enter accepts the highlighted item — same UX as the chat
            // tab. Once they've typed past the name into args, Enter
            // falls through and the existing onData path submits.
            if (!lineBuffer.includes(" ")) {
              const cmd = sv.filtered[sv.index];
              if (cmd) acceptSlashCommand(cmd);
              return false;
            }
          }
        }
      }

      // Shift+Enter inserts a literal newline into the input buffer
      // instead of submitting. Same UX as the chat tab's textarea —
      // lets the user compose multi-line prompts (and especially
      // multi-line answers to AskUserQuestion) without resorting to
      // paste tricks. xterm's onData would otherwise see this as a
      // bare `\r` indistinguishable from plain Enter, so we have to
      // catch it here at the keyboard-event layer and short-circuit.
      if (
        e.type === "keydown" &&
        e.key === "Enter" &&
        e.shiftKey &&
        !e.metaKey && !e.ctrlKey && !e.altKey
      ) {
        lineBuffer += "\n";
        cursorPos = lineBuffer.length;
        // `\r\n  ` = move to next line, indent two spaces so the
        // continuation visually aligns past the `❯ ` prompt glyph.
        // (We don't try to track caret movement across multi-line
        // buffers — readline-style multi-line editing in xterm is a
        // bigger project. For now: append-only continuation, plain
        // Enter submits the whole buffer.)
        term.write("\r\n  ");
        return false;
      }

      // Plain Ctrl+C: ALWAYS request abort of the in-flight turn via
      // the shell_cancel IPC. If the line buffer has content, also
      // clear it so input state matches "cancelled" (don't leave the
      // user staring at half-typed text after they aborted).
      //
      // Earlier versions only fired shell_cancel when the line was
      // empty (bash convention: Ctrl+C with text just clears the
      // line). That conflicted with the agent-cancel intent — users
      // pressing Ctrl+C while a turn was running and they had typed
      // something would only see the line clear, not the agent stop.
      // For an agent terminal, "stop the work" wins over "shell-style
      // line edit." Cancel is idempotent on the backend (no-op when
      // nothing is running) so always firing is safe.
      if (
        e.type === "keydown" &&
        e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey &&
        (e.key === "c" || e.key === "C")
      ) {
        if (lineBuffer.length > 0) {
          term.write("\x1b[2K\r");
          lineBuffer = "";
          cursorPos = 0;
          writePrompt(); // also flips promptShowing back on
          recomputeSlash();
        }
        send({ type: "shell_cancel" });
        return false;
      }

      // Cmd/Ctrl+L: clear screen
      if (
        e.type === "keydown" &&
        ((isMac && e.metaKey) || (!isMac && e.ctrlKey)) &&
        (e.key === "l" || e.key === "L")
      ) {
        term.write("\x1b[2J\x1b[H");
        // redrawLine writes the prompt, the buffer, and re-positions
        // the caret — handles the case where the user cleared screen
        // mid-edit (cursorPos < lineBuffer.length).
        redrawLine();
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
            // Cap on paste size — atob() and TextDecoder are sync and
            // freeze the main thread on multi-MB inputs. 1 MB binary
            // is ~1.33 MB base64; round up the b64 ceiling for safety.
            const MAX_PASTE_BYTES = 1 * 1024 * 1024;
            const MAX_PASTE_B64 = Math.ceil((MAX_PASTE_BYTES * 4) / 3);
            let text = "";
            if (typeof msg.text_b64 === "string") {
              if (msg.text_b64.length > MAX_PASTE_B64) {
                console.warn(
                  `[paste] clipboard too large (${msg.text_b64.length} b64 bytes); ignoring`,
                );
                return;
              }
              const bin = atob(msg.text_b64 as string);
              const bytes = new Uint8Array(bin.length);
              for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
              text = new TextDecoder("utf-8").decode(bytes);
            } else if (typeof msg.text === "string") {
              if (msg.text.length > MAX_PASTE_BYTES) {
                console.warn(
                  `[paste] clipboard too large (${msg.text.length} bytes); ignoring`,
                );
                return;
              }
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
                // Single-line paste: insert at the current caret
                // position so pasting into the middle of an in-progress
                // edit works the same as typing there. After the
                // buffer change, recompute the slash-popup state in
                // case the paste extended a `/<word>` query.
                lineBuffer =
                  lineBuffer.slice(0, cursorPos) +
                  text +
                  lineBuffer.slice(cursorPos);
                cursorPos += text.length;
                redrawLine();
                recomputeSlash();
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
      // Left / Right arrows: walk the caret one column. We forward the
      // same escape sequence to xterm so the visible cursor moves —
      // we just track the new logical position so subsequent inserts /
      // deletes know where to act.
      if (data === "\x1b[D") {
        if (cursorPos > 0) {
          cursorPos -= 1;
          term.write("\x1b[D");
        }
        return;
      }
      if (data === "\x1b[C") {
        if (cursorPos < lineBuffer.length) {
          cursorPos += 1;
          term.write("\x1b[C");
        }
        return;
      }
      // Home / End: jump caret to start / end of line. xterm sends one
      // of two encodings depending on application-keypad mode (`\x1bO*`
      // vs `\x1b[*~`); accept both. Ctrl-A / Ctrl-E are the readline
      // equivalents — surfaced here as plain control bytes (`\x01`,
      // `\x05`) which would otherwise be dropped by the control-byte
      // filter below.
      if (
        data === "\x1bOH" || data === "\x1b[H" || data === "\x1b[1~" ||
        data === "\x01"
      ) {
        if (cursorPos > 0) {
          cursorPos = 0;
          redrawLine();
        }
        return;
      }
      if (
        data === "\x1bOF" || data === "\x1b[F" || data === "\x1b[4~" ||
        data === "\x05"
      ) {
        if (cursorPos < lineBuffer.length) {
          cursorPos = lineBuffer.length;
          redrawLine();
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
        cursorPos = 0;
        if (trimmed.length > 0) {
          // Erase whatever the user had locally echoed — the canonical
          // UserPrompt event from the shared session will re-render
          // the whole paste in one block.
          term.write("\x1b[2K\r");
          promptShowing = false;
          if (pendingAskId !== null) {
            // Multi-line paste while answering an AskUserQuestion —
            // route as the answer instead of a new prompt.
            send({ type: "ask_user_response", id: pendingAskId, text: trimmed });
            pendingAskId = null;
          } else {
            pushHistory(trimmed);
            send({ type: "shell_input", text: trimmed });
            // Mid-turn paste — see the keystroke Enter branch for why
            // we surface a queued indicator.
            if (streamingRef.current) {
              term.write(
                "\x1b[2m[queued — will run after current turn]\x1b[0m\r\n",
              );
            }
          }
        } else if (!streamingRef.current) {
          // Empty paste while idle: redraw prompt. While streaming,
          // suppress (chat_done will render the real prompt).
          term.write("\r\n");
          writePrompt();
        }
        return;
      }

      // xterm hands us each keystroke as a string; classify and either
      // mutate the buffer + echo, or submit on Enter.
      let needsRedraw = false;
      let bufferMutated = false;
      for (const ch of data) {
        if (ch === "\r" || ch === "\n") {
          if (lineBuffer.trim().length > 0) {
            // Erase the locally-echoed `❯ <typing>` so the canonical
            // UserPrompt event (`> text\r\n`) coming back from the
            // shared session is the visible representation. For
            // multi-line buffers we also have to clear the
            // continuation rows the Shift+Enter handler painted —
            // each embedded `\n` is one row above us.
            const newlinesInBuffer = (lineBuffer.match(/\n/g) ?? []).length;
            for (let i = 0; i < newlinesInBuffer; i += 1) {
              term.write("\x1b[A\x1b[2K");
            }
            term.write("\x1b[2K\r");
            promptShowing = false;
            const submitted = lineBuffer;
            if (pendingAskId !== null) {
              // Answer mode: route to the AskUserQuestion oneshot
              // instead of treating this as a new chat prompt.
              send({ type: "ask_user_response", id: pendingAskId, text: submitted });
              pendingAskId = null;
            } else {
              pushHistory(submitted);
              send({ type: "shell_input", text: submitted });
              // Mid-turn submit: input gets queued in the worker's
              // input_rx and runs after chat_done. Without a visible
              // cue the user thinks the text vanished — surface a dim
              // "[queued]" hint so they know it'll fire when the
              // current turn ends.
              if (streamingRef.current) {
                term.write(
                  "\x1b[2m[queued — will run after current turn]\x1b[0m\r\n",
                );
              }
            }
          } else if (streamingRef.current) {
            // Mid-turn empty Enter: do NOT redraw the chevron prompt.
            // A fresh `❯` would look like the agent is ready for
            // input, but the worker is still inside drive_turn_stream
            // and any typing would just sit in the queue. Stay silent;
            // the chat_done handler renders the real prompt when the
            // turn finishes.
          } else {
            // Idle empty line: newline + redraw prompt for the next try.
            term.write("\r\n");
            writePrompt();
          }
          lineBuffer = "";
          cursorPos = 0;
          needsRedraw = false;
          bufferMutated = true;
        } else if (ch === "\x7f" || ch === "\b") {
          // Backspace deletes the char *before* the caret. At end of
          // line we can use the cheap `\b \b` trick; mid-line we have
          // to redraw because the tail shifts left.
          if (cursorPos > 0) {
            const atEnd = cursorPos === lineBuffer.length;
            lineBuffer =
              lineBuffer.slice(0, cursorPos - 1) + lineBuffer.slice(cursorPos);
            cursorPos -= 1;
            if (atEnd) {
              term.write("\b \b");
            } else {
              needsRedraw = true;
            }
            bufferMutated = true;
          }
        } else if (ch >= " " && ch !== "\x7f") {
          // Insert at caret. End-of-line is the fast path (just echo);
          // mid-line requires a redraw so the existing tail shifts
          // right and the caret lands one column further along.
          const atEnd = cursorPos === lineBuffer.length;
          lineBuffer =
            lineBuffer.slice(0, cursorPos) + ch + lineBuffer.slice(cursorPos);
          cursorPos += 1;
          if (atEnd) {
            term.write(ch);
          } else {
            needsRedraw = true;
          }
          bufferMutated = true;
        }
        // Other control bytes are dropped.
      }
      if (needsRedraw) redrawLine();
      // Slash-popup state must follow buffer mutations regardless of
      // whether the visible line redrew (end-of-line edits use the
      // fast `\b \b` / direct echo path but still change the buffer).
      if (bufferMutated) recomputeSlash();
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
        // Stale askPromptId can't outlive a turn boundary; if the
        // model exited the loop without resolving the ask, the
        // backend's oneshot already returned an empty answer.
        pendingAskId = null;
        setStreaming(false);
        term.write("\r\n");
        writePrompt();
        term.write(lineBuffer);
      } else if (
        msg.type === "chat_text_delta" ||
        msg.type === "chat_tool_call"
      ) {
        // Agent is actively producing output — flip the streaming
        // flag so the Stop overlay shows. Idempotent on the same
        // turn (setState bailouts on equal value), so the per-event
        // hot path doesn't re-render needlessly.
        setStreaming(true);
      } else if (msg.type === "ask_user_question") {
        // Mark the next submit as an answer rather than a new prompt.
        // The cyan-bordered question block already rendered via
        // `terminal_data`; the only state we need locally is the id.
        if (typeof msg.id === "number") {
          pendingAskId = msg.id;
          // Visual hint that the input is now expected to be an
          // answer — green chevron + "answer" badge instead of the
          // usual prompt. (Lightweight; full-fledged "answer mode"
          // UI is M5 polish.)
          term.write("\r\n\x1b[32m❯ \x1b[2m(answer — Shift+Enter for newline)\x1b[0m ");
          promptShowing = true;
        }
      } else if (msg.type === "terminal_clear") {
        term.reset();
        term.clear();
        writePrompt();
        lineBuffer = "";
        pendingAskId = null;
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

  // Refit + focus when the tab becomes active or a modal closes.
  useEffect(() => {
    if (!active || modalOpen) return;
    const t = termRef.current;
    const f = fitRef.current;
    if (!t) return;
    requestAnimationFrame(() => {
      try { f?.fit(); } catch { /* fit() may throw on zero-size or disposed container */ }
      t.focus();
    });
  }, [active, modalOpen]);

  // Live theme swap.
  useEffect(() => {
    const t = termRef.current;
    if (!t) return;
    t.options.theme = TERMINAL_PALETTES[themeMode];
  }, [themeMode]);

  return (
    <div
      className="relative h-full w-full"
      style={{ background: "var(--terminal-bg)" }}
    >
      <div ref={ref} className="h-full w-full p-1.5" />
      {streaming && (
        // Floating Stop button overlay — sits top-right while the
        // agent is generating, fires shell_cancel on click. Mirrors
        // the chat-side Stop button so terminal users have a
        // discoverable abort affordance even when their fingers
        // aren't on the keyboard. Hotkeys still work too:
        // Ctrl+C (any line state) / Cmd+. / Ctrl+. / Esc.
        <button
          type="button"
          onClick={() => send({ type: "shell_cancel" })}
          className="absolute right-3 top-3 px-2.5 py-1 rounded text-xs font-medium inline-flex items-center gap-1.5 transition-colors"
          style={{
            background: "var(--danger, #c0392b)",
            color: "#fff",
            cursor: "pointer",
            boxShadow: "0 2px 6px rgba(0,0,0,0.3)",
            zIndex: 10,
          }}
          title="Stop the agent (Esc / Cmd+. / Ctrl+. / Ctrl+C)"
          aria-label="Stop"
        >
          <span
            aria-hidden="true"
            style={{
              display: "inline-block",
              width: 9,
              height: 9,
              background: "#fff",
              borderRadius: 1,
            }}
          />
          Stop
        </button>
      )}
      {slashView.open && slashView.filtered.length > 0 && (
        <div
          className="absolute left-3 right-3 bottom-3"
          style={{ pointerEvents: "auto" }}
        >
          <SlashCommandPopup
            query={slashView.query}
            commands={slashCommands}
            selectedIndex={slashView.index}
            onHoverIndex={(i) =>
              setSlashView((prev) => ({ ...prev, index: i }))
            }
            onSelect={(cmd) => acceptSlashRef.current(cmd)}
          />
        </div>
      )}
    </div>
  );
}
