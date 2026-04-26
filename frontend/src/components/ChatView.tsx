import { useState, useRef, useEffect } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { Check, Copy } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";
import logoDark from "../assets/thClaws-logo-dark.png";
import logoLight from "../assets/thClaws-logo-light.png";
import {
  SlashCommandPopup,
  filterCommands,
  type SlashCommandInfo,
} from "./SlashCommandPopup";

type ChatMessage = {
  role: "user" | "assistant" | "tool" | "system";
  content: string;
  toolName?: string;
  /// `tool` messages only — flips from false (running) to true (done)
  /// when the matching `chat_tool_result` arrives. Drives the leading
  /// glyph (▸ vs ✓) without changing the bubble's identity.
  toolDone?: boolean;
};

/// One pasted/dropped image waiting to be sent with the next chat
/// message. `data` is base64 of the raw bytes (no `data:` prefix —
/// the IPC handler doesn't want one); `previewUrl` is the full data:
/// URL we use as the <img src> for the thumbnail render.
type Attachment = {
  id: string;
  mediaType: string;
  data: string;
  previewUrl: string;
};

type AskPrompt = {
  id: number;
  question: string;
};

const SUPPORTED_IMAGE_MIME = /^image\/(png|jpeg|jpg|webp|gif)$/;
const MAX_IMAGE_BYTES = 10 * 1024 * 1024; // 10 MB per attachment

/// Pull the base64 portion out of a `data:<mime>;base64,<b64>` URL.
/// FileReader.readAsDataURL hands us the prefixed form; the backend
/// IPC contract takes raw base64.
function dataUrlToBase64(dataUrl: string): string {
  const idx = dataUrl.indexOf(",");
  return idx >= 0 ? dataUrl.slice(idx + 1) : dataUrl;
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result;
      if (typeof result === "string") resolve(dataUrlToBase64(result));
      else reject(new Error("FileReader: non-string result"));
    };
    reader.onerror = () => reject(reader.error ?? new Error("FileReader failed"));
    reader.readAsDataURL(blob);
  });
}

export function ChatView() {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [askPrompt, setAskPrompt] = useState<AskPrompt | null>(null);
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [dragActive, setDragActive] = useState(false);
  const [copiedMessageIndex, setCopiedMessageIndex] = useState<number | null>(
    null,
  );
  const [attachmentError, setAttachmentError] = useState<string | null>(null);
  const [slashCommands, setSlashCommands] = useState<SlashCommandInfo[]>([]);
  const [slashIndex, setSlashIndex] = useState(0);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const copiedTimerRef = useRef<number | null>(null);
  const errorTimerRef = useRef<number | null>(null);
  const { resolved: themeMode } = useTheme();

  // Show the slash popup whenever the input begins with `/` and the
  // user isn't mid-prompt for an `ask_user_question`. Hidden during a
  // streaming turn — slash commands fire instantly so there's nothing
  // useful to autocomplete while the model is still talking.
  const slashOpen =
    !askPrompt && !streaming && input.startsWith("/");
  const slashQuery = slashOpen ? input.slice(1).split(/\s/)[0] : "";
  const slashFiltered = slashOpen
    ? filterCommands(slashCommands, slashQuery)
    : [];

  const showAttachmentError = (msg: string) => {
    setAttachmentError(msg);
    if (errorTimerRef.current !== null) window.clearTimeout(errorTimerRef.current);
    errorTimerRef.current = window.setTimeout(() => {
      setAttachmentError(null);
      errorTimerRef.current = null;
    }, 4000);
  };

  const copyMessage = (msg: ChatMessage, index: number) => {
    if (!msg.content) return;
    send({ type: "clipboard_write", text: msg.content });
    setCopiedMessageIndex(index);
    if (copiedTimerRef.current !== null) {
      window.clearTimeout(copiedTimerRef.current);
    }
    copiedTimerRef.current = window.setTimeout(() => {
      setCopiedMessageIndex((current) => (current === index ? null : current));
      copiedTimerRef.current = null;
    }, 1200);
  };

  /// Add an image File/Blob to the pending-attachments list. Skips any
  /// MIME type the providers don't accept (anything outside
  /// png/jpeg/webp/gif) so the user gets fast feedback rather than a
  /// 400 from the model on send. Also enforces MAX_IMAGE_BYTES to
  /// avoid a multi-MB clipboard paste freezing the UI during base64
  /// encoding and ballooning the IPC payload to the backend.
  const addImageBlob = async (blob: Blob) => {
    if (!SUPPORTED_IMAGE_MIME.test(blob.type)) {
      showAttachmentError(
        `Unsupported image type: ${blob.type || "unknown"} (PNG, JPEG, WebP, GIF only)`,
      );
      return;
    }
    if (blob.size > MAX_IMAGE_BYTES) {
      const mb = (blob.size / 1024 / 1024).toFixed(1);
      const max = MAX_IMAGE_BYTES / 1024 / 1024;
      showAttachmentError(`Image too large: ${mb} MB (max ${max} MB)`);
      return;
    }
    try {
      const data = await blobToBase64(blob);
      const previewUrl = `data:${blob.type};base64,${data}`;
      setAttachments((prev) => [
        ...prev,
        { id: crypto.randomUUID(), mediaType: blob.type, data, previewUrl },
      ]);
    } catch {
      // Encoding failure is rare (only if the blob is unreadable);
      // silently drop — user can re-paste.
    }
  };

  const onPaste = (e: React.ClipboardEvent) => {
    if (askPrompt) return;
    const items = e.clipboardData?.items;
    if (!items) return;
    for (const item of Array.from(items)) {
      if (item.kind === "file" && item.type.startsWith("image/")) {
        const file = item.getAsFile();
        if (file) {
          e.preventDefault();
          void addImageBlob(file);
        }
      }
    }
  };

  const onDragOver = (e: React.DragEvent) => {
    e.preventDefault();
    if (askPrompt) return;
    if (!dragActive) setDragActive(true);
  };

  const onDragLeave = (e: React.DragEvent) => {
    e.preventDefault();
    setDragActive(false);
  };

  const onDrop = (e: React.DragEvent) => {
    e.preventDefault();
    setDragActive(false);
    if (askPrompt) return;
    const files = e.dataTransfer?.files;
    if (!files) return;
    for (const file of Array.from(files)) {
      if (file.type.startsWith("image/")) {
        void addImageBlob(file);
      }
    }
  };

  const removeAttachment = (id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  };

  useEffect(() => {
    const unsub = subscribe((msg) => {
      switch (msg.type) {
        case "chat_user_message":
          // Echo of a prompt the user submitted (possibly from the
          // Terminal tab — we render it as a user bubble either way).
          setMessages((prev) => [
            ...prev,
            { role: "user", content: msg.text as string },
          ]);
          break;
        case "chat_text_delta":
          setMessages((prev) => {
            const last = prev[prev.length - 1];
            if (last && last.role === "assistant") {
              return [
                ...prev.slice(0, -1),
                { ...last, content: last.content + (msg.text as string) },
              ];
            }
            return [...prev, { role: "assistant", content: msg.text as string }];
          });
          break;
        case "chat_tool_call":
          // Compact one-line indicator only — the actual tool output
          // is intentionally suppressed in the chat tab to keep the
          // conversation focused on user/assistant exchange. Users
          // who want raw tool stdout/stderr switch to the Terminal
          // tab, which renders the same shared session unfiltered.
          setMessages((prev) => [
            ...prev,
            {
              role: "tool",
              content: msg.name as string,
              toolName: msg.name as string,
              toolDone: false,
            },
          ]);
          break;
        case "chat_tool_result":
          // Flip the same bubble's done flag. We don't store the
          // output text here — the chat-tab UX is "the agent ran X",
          // not "X returned Y". (Errors still surface as red error
          // bubbles via chat_text_delta-like paths; that's separate
          // from normal tool completion.)
          setMessages((prev) => {
            for (let i = prev.length - 1; i >= 0; i--) {
              const candidate = prev[i];
              if (candidate.role === "tool" && !candidate.toolDone) {
                return [
                  ...prev.slice(0, i),
                  { ...candidate, toolDone: true },
                  ...prev.slice(i + 1),
                ];
              }
            }
            return prev;
          });
          break;
        case "chat_slash_output":
          setMessages((prev) => [
            ...prev,
            { role: "system", content: msg.text as string },
          ]);
          break;
        case "chat_done":
          setStreaming(false);
          setAskPrompt(null);
          break;
        case "ask_user_question": {
          const id = typeof msg.id === "number" ? msg.id : null;
          const question = typeof msg.question === "string" ? msg.question : "";
          if (id !== null) {
            setAskPrompt({ id, question });
            setStreaming(true);
            setAttachments([]);
          }
          break;
        }
        case "new_session_ack":
          setMessages([]);
          setStreaming(false);
          setAskPrompt(null);
          break;
        case "slash_commands":
          if (Array.isArray(msg.commands)) {
            setSlashCommands(msg.commands as SlashCommandInfo[]);
          }
          break;
        case "chat_history_replaced":
          if (msg.messages && Array.isArray(msg.messages)) {
            setMessages(
              (msg.messages as { role: string; content: string }[]).map(
                (m) => {
                  const role =
                    m.role === "assistant"
                      ? "assistant"
                      : m.role === "tool"
                        ? "tool"
                        : m.role === "system"
                          ? "system"
                          : "user";
                  // Restored tool entries are historical — they've
                  // already finished. Mark them done so they render
                  // with the ✓ glyph rather than the running ▸.
                  // Backend sends the bare tool name as `content`.
                  if (role === "tool") {
                    return {
                      role,
                      content: m.content,
                      toolName: m.content,
                      toolDone: true,
                    } satisfies ChatMessage;
                  }
                  return { role, content: m.content } satisfies ChatMessage;
                },
              ),
            );
            setStreaming(false);
            setAskPrompt(null);
          }
          break;
      }
    });
    // Ask the backend for the slash command catalogue once on mount.
    // The backend returns a `slash_commands` event the subscriber above
    // catches; new user commands / installed skills will only be picked
    // up on next mount, which matches the rest of the GUI's
    // discover-once-per-session behavior.
    send({ type: "slash_commands_list" });
    return unsub;
  }, []);

  useEffect(() => {
    // Reset the highlighted item whenever the filtered list changes
    // shape — keeping a stale index past the end of the new list would
    // either render off-screen or wrap unexpectedly.
    setSlashIndex(0);
  }, [slashQuery, slashOpen]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  useEffect(() => {
    return () => {
      if (copiedTimerRef.current !== null) {
        window.clearTimeout(copiedTimerRef.current);
      }
      if (errorTimerRef.current !== null) {
        window.clearTimeout(errorTimerRef.current);
      }
    };
  }, []);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    const text = input.trim();
    if (askPrompt) {
      if (!text) return;
      setInput("");
      send({ type: "ask_user_response", id: askPrompt.id, text });
      setMessages((prev) => [...prev, { role: "user", content: text }]);
      setAskPrompt(null);
      return;
    }
    // Allow send when EITHER text or attachments are present —
    // "describe this image" with no text is a valid use case.
    if ((!text && attachments.length === 0) || streaming) return;
    setInput("");
    const pendingAttachments = attachments;
    setAttachments([]);

    // /exit and /quit close the app through the backend so it can save
    // the shared session before the tao event loop exits. Everything else
    // (including /clear, /help, every other slash command) goes to the
    // shared session, which dispatches it and broadcasts the response
    // back as a `chat_slash_output` system bubble.
    const lower = text.toLowerCase();
    if (lower === "/exit" || lower === "/quit" || lower === "/q") {
      send({ type: "app_close" });
      return;
    }

    // Don't optimistically add the user bubble — the backend will echo
    // a `chat_user_message` back to us (it does so for both tabs). This
    // keeps a single source of truth about what's in the conversation.
    if (!text.startsWith("/")) setStreaming(true);
    send({
      type: "shell_input",
      text,
      attachments: pendingAttachments.map((a) => ({
        mediaType: a.mediaType,
        data: a.data,
      })),
    });
  };

  const acceptSlashCommand = (cmd: SlashCommandInfo) => {
    // Commands with required args (usage non-empty and not optional)
    // get the slash + name + trailing space inserted so the user can
    // immediately type the argument. Zero-arg commands fire on enter.
    const needsArg = cmd.usage && !cmd.usage.startsWith("[");
    setInput(`/${cmd.name}${needsArg ? " " : ""}`);
    setSlashIndex(0);
    inputRef.current?.focus();
  };

  const handleInputKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (!slashOpen || slashFiltered.length === 0) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setSlashIndex((i) => (i + 1) % slashFiltered.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSlashIndex(
        (i) => (i - 1 + slashFiltered.length) % slashFiltered.length,
      );
    } else if (e.key === "Tab") {
      e.preventDefault();
      const cmd = slashFiltered[slashIndex];
      if (cmd) acceptSlashCommand(cmd);
    } else if (e.key === "Enter") {
      // Only intercept Enter when the user is still composing the
      // command name itself ("/cl" → fill in "/clear"). Once they've
      // typed past the name into args ("/model gpt-5"), Enter should
      // submit normally so they don't have to dismiss the popup first.
      const composingName = !input.slice(1).includes(" ");
      if (composingName) {
        e.preventDefault();
        const cmd = slashFiltered[slashIndex];
        if (cmd) acceptSlashCommand(cmd);
      }
    } else if (e.key === "Escape") {
      e.preventDefault();
      setInput("");
    }
  };

  const awaitingUserAnswer = askPrompt !== null;
  const inputDisabled = streaming && !awaitingUserAnswer;
  const submitDisabled = awaitingUserAnswer
    ? !input.trim()
    : streaming || (!input.trim() && attachments.length === 0);
  const inputPlaceholder = awaitingUserAnswer
    ? askPrompt.question
      ? `Answer: ${askPrompt.question}`
      : "Answer the assistant..."
    : streaming
      ? "Waiting for response..."
      : attachments.length > 0
        ? "Add a prompt (or send as-is)..."
        : "Type a message — paste or drop an image to attach...";

  return (
    <div className="flex flex-col h-full">
      {/* Messages */}
      <div
        className="flex-1 overflow-y-auto p-4 space-y-3"
        style={{ background: "var(--bg-primary)" }}
      >
        {messages.length === 0 && (
          <div
            className="flex flex-col items-center mt-20 select-none"
            style={{ color: "var(--text-secondary)" }}
          >
            <img
              src={themeMode === "light" ? logoLight : logoDark}
              alt="thClaws"
              className="mb-4 opacity-90"
              style={{ width: 280, height: 280 }}
              draggable={false}
            />
            <div className="text-sm">Chat mode — send a message to start</div>
          </div>
        )}
        {messages.map((msg, i) => {
          // Tool calls render as a thin one-line indicator (▸ running,
          // ✓ done) rather than a full bubble — the chat tab is for
          // the user↔assistant conversation; raw tool output lives on
          // the Terminal tab.
          if (msg.role === "tool") {
            const glyph = msg.toolDone ? "✓" : "▸";
            const copied = copiedMessageIndex === i;
            return (
              <div key={i} className="flex justify-start">
                <div
                  className="group inline-flex max-w-[80%] items-center gap-1 text-xs"
                  style={{
                    color: "var(--text-secondary)",
                    fontFamily: "Menlo, Monaco, monospace",
                    paddingLeft: 2,
                    opacity: msg.toolDone ? 0.7 : 1,
                  }}
                >
                  <span className="truncate">
                    {glyph} {msg.toolName ?? msg.content}
                  </span>
                  <CopyMessageButton
                    copied={copied}
                    compact
                    onCopy={() => copyMessage(msg, i)}
                  />
                </div>
              </div>
            );
          }

          const isAssistant = msg.role === "assistant";
          const isSystem = msg.role === "system";
          const copied = copiedMessageIndex === i;
          return (
            <div
              key={i}
              className={`flex ${msg.role === "user" ? "justify-end" : isSystem ? "justify-center" : "justify-start"}`}
            >
              <div
                className={`group relative max-w-[80%] rounded-lg py-2 pl-3 pr-9 text-sm ${isAssistant ? "" : "whitespace-pre-wrap"}`}
                style={{
                  background:
                    msg.role === "user"
                      ? "var(--chat-user-bg)"
                      : isSystem
                        ? "transparent"
                        : "var(--bg-secondary)",
                  color:
                    msg.role === "user"
                      ? "var(--chat-user-fg)"
                      : isSystem
                        ? "var(--text-secondary)"
                        : "var(--text-primary)",
                  border: isSystem ? "1px solid var(--border)" : "none",
                  fontFamily: isSystem ? "Menlo, Monaco, monospace" : "inherit",
                  fontSize: isSystem ? "12px" : "14px",
                }}
              >
                {isAssistant ? (
                  // Assistant turns are rendered through react-markdown
                  // so headings/lists/code-blocks/tables come out as
                  // proper HTML rather than literal **bold** text.
                  // remark-gfm adds GitHub-flavored markdown (tables,
                  // strikethrough, task lists). rehype-highlight runs
                  // syntax highlighting against fenced code blocks —
                  // styled by the .hljs-* rules in index.css.
                  //
                  // SECURITY: msg.content is untrusted (model output).
                  // The pipeline above is the safe stack — no
                  // allowDangerousHtml, no allowSvg, no rehype-raw.
                  // rehype-highlight is a CSS-class applier (no code
                  // execution); fenced-code language IDs flow into it
                  // unchecked but are rendered as text. Don't add HTML
                  // pass-through plugins or dangerouslySetInnerHTML
                  // here without rethinking that threat model.
                  <div className="markdown-body">
                    <ReactMarkdown
                      remarkPlugins={[remarkGfm]}
                      rehypePlugins={[rehypeHighlight]}
                    >
                      {msg.content}
                    </ReactMarkdown>
                  </div>
                ) : (
                  msg.content
                )}
                <CopyMessageButton
                  copied={copied}
                  onCopy={() => copyMessage(msg, i)}
                />
              </div>
            </div>
          );
        })}
        <div ref={bottomRef} />
      </div>

      {/* Input */}
      <form
        onSubmit={handleSubmit}
        onDragOver={onDragOver}
        onDragLeave={onDragLeave}
        onDrop={onDrop}
        className="flex flex-col gap-2 p-3 border-t"
        style={{
          background: "var(--bg-secondary)",
          borderColor: dragActive ? "var(--accent)" : "var(--border)",
          borderWidth: dragActive ? 2 : 1,
          transition: "border-color 0.12s, border-width 0.12s",
        }}
      >
        {/* Attachment error banner — auto-clears after 4s */}
        {attachmentError && (
          <div
            role="alert"
            className="text-xs px-2 py-1 rounded"
            style={{
              background: "var(--bg-error, rgba(220, 38, 38, 0.12))",
              color: "var(--text-error, #f87171)",
              border: "1px solid var(--border-error, rgba(220, 38, 38, 0.3))",
            }}
          >
            {attachmentError}
          </div>
        )}

        {/* Pending image attachments */}
        {attachments.length > 0 && (
          <div className="flex flex-wrap gap-2">
            {attachments.map((a) => (
              <div
                key={a.id}
                className="relative group"
                style={{
                  width: 64,
                  height: 64,
                  borderRadius: 6,
                  overflow: "hidden",
                  border: "1px solid var(--border)",
                  background: "var(--bg-tertiary)",
                }}
              >
                <img
                  src={a.previewUrl}
                  alt="attachment"
                  style={{
                    width: "100%",
                    height: "100%",
                    objectFit: "cover",
                    display: "block",
                  }}
                />
                <button
                  type="button"
                  onClick={() => removeAttachment(a.id)}
                  aria-label="remove attachment"
                  className="absolute top-0.5 right-0.5 leading-none flex items-center justify-center"
                  style={{
                    width: 18,
                    height: 18,
                    borderRadius: 9,
                    background: "rgba(0,0,0,0.65)",
                    color: "white",
                    fontSize: 12,
                    border: "none",
                    cursor: "pointer",
                  }}
                >
                  ×
                </button>
              </div>
            ))}
          </div>
        )}
        {slashOpen && slashFiltered.length > 0 && (
          <SlashCommandPopup
            query={slashQuery}
            commands={slashCommands}
            selectedIndex={slashIndex}
            onHoverIndex={setSlashIndex}
            onSelect={acceptSlashCommand}
          />
        )}
        <div className="flex gap-2">
          <input
            ref={inputRef}
            type="text"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={handleInputKeyDown}
            onPaste={onPaste}
            placeholder={inputPlaceholder}
            disabled={inputDisabled}
            className="flex-1 px-3 py-2 rounded text-sm outline-none"
            style={{
              background: "var(--bg-tertiary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
          />
          <button
            type="submit"
            disabled={submitDisabled}
            className="px-4 py-2 rounded text-sm font-medium transition-colors"
            style={{
              background: submitDisabled ? "var(--bg-tertiary)" : "var(--accent)",
              color: submitDisabled ? "var(--text-secondary)" : "var(--accent-fg)",
              cursor: submitDisabled ? "not-allowed" : "pointer",
            }}
          >
            {awaitingUserAnswer ? "Reply" : "Send"}
          </button>
        </div>
      </form>
    </div>
  );
}

function CopyMessageButton({
  copied,
  compact,
  onCopy,
}: {
  copied: boolean;
  compact?: boolean;
  onCopy: () => void;
}) {
  const size = compact ? 20 : 24;
  const iconSize = compact ? 12 : 13;

  return (
    <button
      type="button"
      aria-label={copied ? "Message copied" : "Copy message"}
      title={copied ? "Copied" : "Copy message"}
      onClick={onCopy}
      className={`${
        compact ? "shrink-0" : "absolute right-1.5 top-1.5"
      } flex items-center justify-center rounded opacity-0 transition-opacity group-hover:opacity-100 focus:opacity-100`}
      style={{
        width: size,
        height: size,
        background: copied ? "var(--accent)" : "var(--bg-tertiary)",
        color: copied ? "var(--accent-fg)" : "var(--text-secondary)",
        border: copied ? "1px solid transparent" : "1px solid var(--border)",
        cursor: "pointer",
      }}
    >
      {copied ? <Check size={iconSize} /> : <Copy size={iconSize} />}
    </button>
  );
}
