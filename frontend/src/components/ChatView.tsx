import { useState, useRef, useEffect, useMemo, useCallback } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { Check, Copy, Paperclip } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";
import { useVersion } from "../hooks/useVersion";
import logoDark from "../assets/thClaws-logo-dark.png";
import logoLight from "../assets/thClaws-logo-light.png";
import {
  SlashCommandPopup,
  filterCommands,
  type SlashCommandInfo,
} from "./SlashCommandPopup";
import { McpAppIframe } from "./McpAppIframe";

type ChatMessage = {
  role: "user" | "assistant" | "tool" | "system" | "error";
  content: string;
  /// `assistant` messages only — accumulated `reasoning_content` from
  /// thinking models (DeepSeek v4/r1, OpenAI o-series, NVIDIA NIM
  /// glm4.7, etc.). Rendered as a collapsible dimmed block above the
  /// assistant text so the user can see the model is working without
  /// the reasoning blending into the final answer.
  thinking?: string;
  toolName?: string;
  /// `tool` messages only — flips from false (running) to true (done)
  /// when the matching `chat_tool_result` arrives. Drives the leading
  /// glyph (▸ vs ✓) without changing the bubble's identity.
  toolDone?: boolean;
  /// Unmangled tool name (e.g. "TodoWrite", "Bash") for tool-specific
  /// rendering. `toolName` above is the formatted label that includes
  /// arguments; this is the bare tool identifier used to route to a
  /// custom render path.
  toolKind?: string;
  /// Raw input the model passed to the tool. Stashed for tools whose
  /// input is itself the user-visible payload — currently TodoWrite,
  /// where the `todos` array drives a checklist card. Other tools
  /// ignore this.
  toolInput?: unknown;
  /// `tool` messages only — name of the upstream service that
  /// produced the result, parsed from a leading `Source: <engine>`
  /// line in the tool result body (M6.38.9). Surfaced as `(via X)`
  /// next to the ✓ glyph so the user sees the source even if the
  /// model paraphrased it away from its summary.
  toolSource?: string;
  /// MCP-Apps widget the bubble should embed inline below the tool
  /// label (e.g. pinn.ai's image viewer). Populated from the
  /// `ui_resource` field on `chat_tool_result` when the upstream MCP
  /// server declared `meta.ui.resourceUri` on the tool.
  uiResource?: {
    uri: string;
    html: string;
    mime?: string;
    allowSameOrigin?: boolean;
    /// Per-widget opt-in for content-driven inline iframe height.
    /// Mirrors `UiResource::auto_size` in Rust + `_meta.autoSize` in
    /// the MCP-Apps resource envelope. False / absent for everything
    /// except first-party widgets that explicitly opted in.
    autoSize?: boolean;
  };
};

/// Shape of a TodoWrite tool input.todos entry. Mirrors the Rust-side
/// `TodoItem` (id + content + status). Used to render the inline
/// checklist card in chat when the model calls TodoWrite.
type TodoItemInput = {
  id: string;
  content: string;
  status: "pending" | "in_progress" | "completed";
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
const MAX_UPLOAD_BYTES = 25 * 1024 * 1024; // 25 MB per uploaded file (--serve / webapp upload)
const MAX_UPLOAD_FILES = 5;

const HAS_WRY_TRANSPORT =
  typeof window !== "undefined" && typeof window.ipc !== "undefined";

/// Pull the base64 portion out of a `data:<mime>;base64,<b64>` URL.
/// FileReader.readAsDataURL hands us the prefixed form; the backend
/// IPC contract takes raw base64.
function dataUrlToBase64(dataUrl: string): string {
  const idx = dataUrl.indexOf(",");
  return idx >= 0 ? dataUrl.slice(idx + 1) : dataUrl;
}

/// Remove `<think>...</think>` blocks from rendered text. The backend's
/// assembler now routes thinking into a separate ContentBlock, but old
/// persisted sessions may still have the tags embedded — strip them here.
/// Only paired tags are removed (no lazy "swallow up to next </think>"
/// that could eat ordinary user content containing a literal tag).
const THINK_BLOCK = /<think>[\s\S]*?<\/think>\n?/gi;
const ORPHAN_CLOSE = /^[ \t\r\n]*<\/think>\n?/i;
function stripThinkBlocks(content: string): string {
  return content.replace(THINK_BLOCK, "").replace(ORPHAN_CLOSE, "");
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

type Props = {
  active: boolean;
  modalOpen: boolean;
};

export function ChatView({ active, modalOpen }: Props) {
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
  /// `true` when the model has been streaming for >5s with zero bytes
  /// arrived (text or thinking). Cold-start latency on hosted providers
  /// (NVIDIA NIM in particular — 40s+ on the first request to a model)
  /// can make the UI look frozen; this drives a subtle "Waiting…" hint
  /// so the user knows the request is in flight.
  const [waitingFirstByte, setWaitingFirstByte] = useState(false);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [uploading, setUploading] = useState(false);
  const copiedTimerRef = useRef<number | null>(null);
  const errorTimerRef = useRef<number | null>(null);
  const waitingTimerRef = useRef<number | null>(null);
  const firstByteSeenRef = useRef(false);
  const { resolved: themeMode } = useTheme();
  const version = useVersion();

  // Show the slash popup whenever the input begins with `/` and the
  // user isn't mid-prompt for an `ask_user_question`. Hidden during a
  // streaming turn — slash commands fire instantly so there's nothing
  // useful to autocomplete while the model is still talking.
  const slashOpen =
    !askPrompt && !streaming && input.startsWith("/") && !input.slice(1).includes(" ");
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

  const copyMessage = useCallback((msg: ChatMessage, index: number) => {
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
  }, []);

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

  const onUploadButtonClick = () => {
    if (uploading || streaming) return;
    fileInputRef.current?.click();
  };

  const onUploadFilesSelected = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const list = e.target.files;
    if (!list || list.length === 0) return;
    if (list.length > MAX_UPLOAD_FILES) {
      showAttachmentError(`Max ${MAX_UPLOAD_FILES} files per upload`);
      e.target.value = "";
      return;
    }
    const form = new FormData();
    for (const f of Array.from(list)) {
      if (f.size > MAX_UPLOAD_BYTES) {
        showAttachmentError(
          `${f.name} is ${(f.size / 1024 / 1024).toFixed(1)} MB (max ${MAX_UPLOAD_BYTES / 1024 / 1024} MB)`,
        );
        e.target.value = "";
        return;
      }
      form.append("file", f, f.name);
    }
    setUploading(true);
    try {
      const resp = await fetch("/upload", { method: "POST", body: form });
      if (!resp.ok) {
        const detail = await resp.text().catch(() => "");
        showAttachmentError(`Upload failed (${resp.status}) ${detail}`);
      }
    } catch (err) {
      showAttachmentError(`Upload failed: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setUploading(false);
      e.target.value = "";
    }
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
          firstByteSeenRef.current = true;
          setWaitingFirstByte(false);
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
        case "chat_error":
          // Provider / agent error surfaced as its own bubble (red
          // border, ⚠ glyph) so a 429 / auth-failure / network blow-up
          // is unambiguously an error rather than blending into the
          // assistant's reply. Pre-fix the backend folded these into
          // `chat_text_delta` and users saw a wall of provider JSON
          // appended to the last assistant bubble.
          firstByteSeenRef.current = true;
          setWaitingFirstByte(false);
          setMessages((prev) => [
            ...prev,
            { role: "error", content: msg.text as string },
          ]);
          break;
        case "chat_thinking_delta":
          firstByteSeenRef.current = true;
          setWaitingFirstByte(false);
          setMessages((prev) => {
            const last = prev[prev.length - 1];
            const chunk = msg.text as string;
            if (last && last.role === "assistant") {
              return [
                ...prev.slice(0, -1),
                { ...last, thinking: (last.thinking ?? "") + chunk },
              ];
            }
            return [
              ...prev,
              { role: "assistant", content: "", thinking: chunk },
            ];
          });
          break;
        case "chat_tool_call":
          // Compact one-line indicator only — the actual tool output
          // is intentionally suppressed in the chat tab to keep the
          // conversation focused on user/assistant exchange. Users
          // who want raw tool stdout/stderr switch to the Terminal
          // tab, which renders the same shared session unfiltered.
          //
          // Tools whose input is itself the user-visible payload
          // (e.g. TodoWrite — the todos array IS the progress
          // display) get a custom card render below. The toolKind +
          // toolInput fields carry the data; the renderer keys on
          // toolKind === "TodoWrite".
          setMessages((prev) => [
            ...prev,
            {
              role: "tool",
              content: msg.name as string,
              toolName: msg.name as string,
              toolKind: typeof msg.tool_name === "string" ? msg.tool_name : undefined,
              toolInput: msg.input,
              toolDone: false,
            },
          ]);
          break;
        case "chat_tool_result": {
          // Flip the same bubble's done flag. We don't store the
          // output text here — the chat-tab UX is "the agent ran X",
          // not "X returned Y". (Errors still surface as red error
          // bubbles via chat_text_delta-like paths; that's separate
          // from normal tool completion.)
          //
          // If the tool came back with an MCP-Apps `ui_resource`,
          // attach it to the bubble too — the render path embeds an
          // iframe widget below the tool label (pinn.ai image viewer
          // etc.). The output text is also stashed so the widget's
          // `ui/notifications/tool-result` push can carry it as a
          // standard MCP text content block.
          const ui = msg.ui_resource as
            | {
                uri: string;
                html: string;
                mime?: string;
                allow_same_origin?: boolean;
                auto_size?: boolean;
              }
            | undefined;
          const output = (msg.output as string | undefined) ?? "";
          // M6.38.9: parse `Source: <engine>` from the first line of
          // the tool result body so the bubble can render `(via X)`
          // next to the ✓ glyph. Independent of whether the model
          // surfaces the source in its summary. Strict prefix match —
          // a false positive is worse than a miss.
          const toolSource = (() => {
            const first = output.split("\n", 1)[0] ?? "";
            const rest = first.startsWith("Source: ")
              ? first.slice("Source: ".length)
              : null;
            if (!rest) return undefined;
            const cut = (() => {
              const a = rest.indexOf(" (");
              const b = rest.indexOf(" —");
              if (a < 0) return b < 0 ? rest.length : b;
              if (b < 0) return a;
              return Math.min(a, b);
            })();
            const name = rest.slice(0, cut).trim();
            return name.length > 0 ? name : undefined;
          })();
          setMessages((prev) => {
            for (let i = prev.length - 1; i >= 0; i--) {
              const candidate = prev[i];
              if (candidate.role === "tool" && !candidate.toolDone) {
                return [
                  ...prev.slice(0, i),
                  {
                    ...candidate,
                    toolDone: true,
                    content: ui ? output : candidate.content,
                    uiResource: ui
                      ? {
                          uri: ui.uri,
                          html: ui.html,
                          mime: ui.mime,
                          allowSameOrigin: ui.allow_same_origin === true,
                          autoSize: ui.auto_size === true,
                        }
                      : undefined,
                    toolSource,
                  },
                  ...prev.slice(i + 1),
                ];
              }
            }
            return prev;
          });
          break;
        }
        case "chat_slash_output":
          setMessages((prev) => [
            ...prev,
            { role: "system", content: msg.text as string },
          ]);
          break;
        case "chat_skill_model_note":
          // Skill-recommended-model swap or fallback. Renders as the
          // same muted system bubble as slash output — terse, in-line
          // with the conversation, no popup. The worker emits these
          // around skill invocation: one when the swap takes effect,
          // and a follow-up "[model → X (skill ended)]" at end of turn.
          setMessages((prev) => [
            ...prev,
            { role: "system", content: msg.text as string },
          ]);
          break;
        case "chat_done":
          setStreaming(false);
          setAskPrompt(null);
          setWaitingFirstByte(false);
          if (waitingTimerRef.current !== null) {
            window.clearTimeout(waitingTimerRef.current);
            waitingTimerRef.current = null;
          }
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
        // ─── Side-channel agent lifecycle ─────────────────────────
        // Pre-fix the chat surface pushed a full streaming bubble
        // per side-channel spawn (live tool-call markers, accumulated
        // text deltas, elapsed status header). That duplicated the
        // BackgroundAgentsSidebar's job and crowded the chat with
        // verbose runtime detail the user didn't want inline. Now
        // the sidebar is the SINGLE surface for live progress. Chat
        // gets ONE permanent audit line per spawn lifecycle —
        // the `✓ dreaming (id: …)` start text is emitted by
        // `shell_dispatch.rs::SlashCommand::Dream` directly as a
        // regular chat message; here we just push a one-line system
        // message on `done` / `error` so the chat carries a record
        // of WHAT happened (without the streaming noise).
        case "chat_side_channel_start":
        case "chat_side_channel_text_delta":
        case "chat_side_channel_tool_call":
          // Sidebar handles all of these — nothing for chat to do.
          break;
        case "chat_side_channel_done": {
          const agentName = String(msg.agent_name ?? "agent");
          const id = String(msg.id ?? "");
          const durationMs = Number(msg.duration_ms ?? 0);
          const rawResult = String(msg.result_text ?? "").trim();
          // Show only the first non-blank line of the agent's final
          // status message — that's where the dream agent puts its
          // "wrote dreams/dream-…" summary. Full text is in the KMS;
          // the chat just needs a record that the run finished.
          const firstLine =
            rawResult
              .split("\n")
              .map((s) => s.trim())
              .find((s) => s.length > 0) ?? "(no result text)";
          const truncated =
            firstLine.length > 240
              ? `${firstLine.slice(0, 237)}…`
              : firstLine;
          const seconds = (durationMs / 1000).toFixed(1);
          const content = `✓ /${agentName} done in ${seconds}s — ${truncated}${
            id ? `  (id: ${id})` : ""
          }`;
          setMessages((prev) => [
            ...prev,
            { role: "system", content },
          ]);
          break;
        }
        case "chat_side_channel_error": {
          const agentName = String(msg.agent_name ?? "agent");
          const id = String(msg.id ?? "");
          const error = String(msg.error ?? "unknown error").trim();
          const firstLine =
            error
              .split("\n")
              .map((s) => s.trim())
              .find((s) => s.length > 0) ?? "unknown error";
          const truncated =
            firstLine.length > 240
              ? `${firstLine.slice(0, 237)}…`
              : firstLine;
          const content = `✗ /${agentName} failed — ${truncated}${
            id ? `  (id: ${id})` : ""
          }`;
          setMessages((prev) => [
            ...prev,
            { role: "system", content },
          ]);
          break;
        }
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

  // Focus the input when the tab becomes active or a modal closes.
  useEffect(() => {
    if (active && !modalOpen) inputRef.current?.focus();
  }, [active, modalOpen]);

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

  // Click handler for chat-rendered links. preventDefault stops the
  // wry webview from navigating away (the webview has no browser
  // chrome to get back from), then routes the URL to the OS default
  // browser via the vetted `open_external` IPC. MCP-Apps tools render
  // their own widgets inline via `McpAppIframe`, so we don't need
  // an in-app lightbox for image previews — links can just hand off.
  const handleChatLinkClick = useCallback((
    e: React.MouseEvent<HTMLAnchorElement>,
    href: string,
  ) => {
    if (!href) return;
    e.preventDefault();
    send({ type: "open_external", url: href });
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
    if (!text.startsWith("/")) {
      setStreaming(true);
      // Arm the cold-start indicator: if no text/thinking delta has
      // arrived 5s after submit, surface a "Waiting…" hint so the user
      // knows the request is in flight (NIM cold-starts can take 40s+).
      firstByteSeenRef.current = false;
      setWaitingFirstByte(false);
      if (waitingTimerRef.current !== null) {
        window.clearTimeout(waitingTimerRef.current);
      }
      waitingTimerRef.current = window.setTimeout(() => {
        if (!firstByteSeenRef.current) setWaitingFirstByte(true);
      }, 5000);
    }
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
    // Always append a trailing space so the popup closes (slashOpen
    // checks for space) and the user can immediately type args or
    // press Enter.
    setInput(`/${cmd.name} `);
    setSlashIndex(0);
    inputRef.current?.focus();
  };

  const handleInputKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // Prevent form submit while IME is composing (Thai, Japanese, Chinese, etc.).
    // Enter during composition should commit the character, not send the message.
    if (e.key === "Enter" && e.nativeEvent.isComposing) {
      return;
    }
    // Slash-command popup navigation runs ahead of the textarea-newline
    // handling below so ArrowUp/Down still walk the menu.
    if (slashOpen && slashFiltered.length > 0) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSlashIndex((i) => (i + 1) % slashFiltered.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setSlashIndex(
          (i) => (i - 1 + slashFiltered.length) % slashFiltered.length,
        );
        return;
      }
      if (e.key === "Tab") {
        e.preventDefault();
        const cmd = slashFiltered[slashIndex];
        if (cmd) acceptSlashCommand(cmd);
        return;
      }
      if (e.key === "Enter" && !e.shiftKey) {
        // Only intercept Enter when the user is still composing the
        // command name itself ("/cl" → fill in "/clear"). Once they've
        // typed past the name into args ("/model gpt-5"), Enter should
        // submit normally so they don't have to dismiss the popup first.
        const composingName = !input.slice(1).includes(" ");
        if (composingName) {
          e.preventDefault();
          const cmd = slashFiltered[slashIndex];
          if (cmd) acceptSlashCommand(cmd);
          return;
        }
      }
    }
    // Multi-line textarea behaviour:
    //   Enter           → submit
    //   Shift+Enter     → newline
    // The form's onSubmit picks up plain Enter via this synthetic
    // submit; Shift+Enter falls through to the textarea's default
    // newline insertion.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      e.currentTarget.form?.requestSubmit();
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      // Esc while the agent is streaming → cancel the in-flight turn.
      // Esc while idle → clear the input (the original behaviour, kept
      // because clearing a long composed message is a real use case).
      // Pressing Esc twice in fast succession during streaming will
      // cancel and then clear, which matches user intent.
      if (streaming) {
        send({ type: "shell_cancel" });
      } else {
        setInput("");
      }
    }
  };

  // Auto-grow the textarea up to ~6 lines, then let it scroll. Resets
  // to one row when the input is cleared (after Send / on attachment
  // submit) so the composer doesn't stay tall after a multi-line reply.
  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    el.style.height = "auto";
    const lineHeight = 20; // matches text-sm + py-2 padding
    const maxRows = 6;
    const padding = 16; // py-2 on top + bottom
    const maxHeight = lineHeight * maxRows + padding;
    const sh = el.scrollHeight;
    el.style.height = `${Math.min(sh, maxHeight)}px`;
    el.style.overflowY = sh > maxHeight ? "auto" : "hidden";
  }, [input]);

  const messageElements = useMemo(() => messages.map((msg, i) => {
          // (Pre-fix this map opened with a side-channel bubble render
          // pulling state off `msg.sideChannel`. The bubble showed
          // live tool-call markers + streamed prose for every /dream
          // / /agent spawn, which duplicated the BackgroundAgentsSidebar
          // and crowded the chat. Live progress is sidebar-only now;
          // the chat gets a one-line system message on done/error
          // pushed by the `chat_side_channel_done` / `_error`
          // handlers above — same `role: "system"` shape as any
          // other system note, no special render branch needed.)
          // Tool calls render as a thin one-line indicator (▸ running,
          // ✓ done) rather than a full bubble — the chat tab is for
          // the user↔assistant conversation; raw tool output lives on
          // the Terminal tab.
          if (msg.role === "tool") {
            const glyph = msg.toolDone ? "✓" : "▸";
            const copied = copiedMessageIndex === i;
            // MCP-Apps tools widen the bubble so the embedded iframe
            // gets meaningful width. Plain tools keep the thin
            // one-liner indicator.
            const widget = msg.uiResource;
            // TodoWrite gets a custom card showing the rendered list
            // — the user wants to see plan-style progression even
            // though TodoWrite is the casual scratchpad. Each call
            // shows the snapshot at that point; successive cards let
            // the user see the diff over time.
            const todos = (() => {
              if (msg.toolKind !== "TodoWrite") return null;
              const inp = msg.toolInput as { todos?: unknown } | undefined;
              if (!inp || !Array.isArray(inp.todos)) return null;
              return inp.todos as TodoItemInput[];
            })();
            return (
              <div key={i} className="flex justify-start">
                <div
                  className={`group flex max-w-[80%] flex-col gap-1 ${widget || todos ? "w-[80%]" : ""}`}
                  style={{
                    color: "var(--text-secondary)",
                    fontFamily:
                      "Menlo, Monaco, 'Courier New', monospace",
                    paddingLeft: 2,
                    // The 0.7 dim signals "this tool finished" on the
                    // text-only indicator. Skip it when there's an
                    // embedded MCP-Apps widget — opacity inherits into
                    // the iframe and washes out widget content (light
                    // mode is most visible). The widget is the focus;
                    // the parent indicator above it doesn't need the
                    // dim treatment when there's actual UI to look at.
                    opacity: msg.toolDone && !widget ? 0.7 : 1,
                  }}
                >
                  <div className="inline-flex items-center gap-1 text-xs">
                    <span className="truncate">
                      {glyph} {msg.toolName ?? msg.content}
                      {msg.toolSource && msg.toolDone && (
                        <span style={{ opacity: 0.7 }}>
                          {" "}
                          (via {msg.toolSource})
                        </span>
                      )}
                    </span>
                    <CopyMessageButton
                      copied={copied}
                      compact
                      onCopy={() => copyMessage(msg, i)}
                    />
                  </div>
                  {todos && todos.length > 0 && (
                    <div
                      className="mt-1 rounded border px-2 py-1.5"
                      style={{
                        borderColor: "var(--border, #2a2a2a)",
                        background: "var(--surface-1, rgba(255,255,255,0.03))",
                        fontFamily:
                          "ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, sans-serif",
                      }}
                    >
                      {todos.map((t) => {
                        const glyphForStatus =
                          t.status === "completed"
                            ? "✓"
                            : t.status === "in_progress"
                              ? "◉"
                              : "☐";
                        const colorForStatus =
                          t.status === "completed"
                            ? "var(--success, #6cc070)"
                            : t.status === "in_progress"
                              ? "var(--warning, #d4a657)"
                              : "var(--text-secondary)";
                        return (
                          <div
                            key={t.id}
                            className="flex items-baseline gap-2"
                            style={{
                              fontSize: "11px",
                              lineHeight: "1.5",
                            }}
                          >
                            <span
                              style={{
                                color: colorForStatus,
                                fontFamily:
                                  "Menlo, Monaco, 'Courier New', monospace",
                                fontSize: "11px",
                              }}
                            >
                              {glyphForStatus}
                            </span>
                            <span
                              style={{
                                textDecoration:
                                  t.status === "completed"
                                    ? "line-through"
                                    : "none",
                                color:
                                  t.status === "pending"
                                    ? "var(--text-secondary)"
                                    : "var(--text-primary)",
                                wordBreak: "break-word",
                              }}
                            >
                              {t.content}
                            </span>
                          </div>
                        );
                      })}
                    </div>
                  )}
                  {widget && msg.toolDone && (
                    <McpAppIframe
                      uri={widget.uri}
                      html={widget.html}
                      allowSameOrigin={widget.allowSameOrigin === true}
                      autoSize={widget.autoSize === true}
                      parentToolName={msg.toolName ?? ""}
                      toolResult={{
                        content: [{ type: "text", text: msg.content }],
                        isError: false,
                      }}
                    />
                  )}
                </div>
              </div>
            );
          }

          const isAssistant = msg.role === "assistant";
          const isSystem = msg.role === "system";
          const isError = msg.role === "error";
          const copied = copiedMessageIndex === i;
          // Restored chat histories can be a wall of tool indicators
          // between user turns; an extra blank line before each user
          // message makes turn boundaries scannable. We apply it only
          // when the previous message was something other than a
          // user bubble — back-to-back user inputs (rare, but
          // possible) keep the standard `space-y-3` spacing.
          const needsTurnGap =
            msg.role === "user" && i > 0 && messages[i - 1]?.role !== "user";
          return (
            <div
              key={i}
              className={`flex ${msg.role === "user" ? "justify-end" : isSystem || isError ? "justify-center" : "justify-start"}${needsTurnGap ? " pt-4" : ""}`}
            >
              <div
                className={`group relative max-w-[80%] rounded-lg py-2 pl-3 pr-9 text-sm ${isAssistant ? "" : "whitespace-pre-wrap"}`}
                style={{
                  background:
                    msg.role === "user"
                      ? "var(--chat-user-bg)"
                      : isError
                        ? "color-mix(in srgb, #f85149 12%, transparent)"
                        : isSystem
                          ? "transparent"
                          : "var(--bg-secondary)",
                  color:
                    msg.role === "user"
                      ? "var(--chat-user-fg)"
                      : isError
                        ? "#f85149"
                        : isSystem
                          ? "var(--text-secondary)"
                          : "var(--text-primary)",
                  border: isError
                    ? "1px solid color-mix(in srgb, #f85149 50%, transparent)"
                    : isSystem
                      ? "1px solid var(--border)"
                      : "none",
                  fontFamily: isSystem
                    ? "Menlo, Monaco, 'Courier New', monospace"
                    : "inherit",
                  fontSize: isSystem ? "12px" : "14px",
                }}
              >
                {isAssistant && msg.thinking && (
                  // Reasoning models (DeepSeek v4/r1, OpenAI o-series,
                  // NVIDIA NIM glm4.7, …) emit `reasoning_content` before
                  // their final answer. Show it as a dim collapsible
                  // block above the assistant text so the user sees the
                  // model is working — but visibly distinct from its
                  // final reply.
                  <details
                    className="mb-2 rounded border px-2 py-1"
                    open={!msg.content}
                    style={{
                      borderColor: "var(--border, #2a2a2a)",
                      background: "var(--surface-1, rgba(255,255,255,0.03))",
                      fontSize: "12px",
                      color: "var(--text-secondary)",
                      fontStyle: "italic",
                    }}
                  >
                    <summary
                      className="cursor-pointer select-none text-xs"
                      style={{ fontStyle: "normal" }}
                    >
                      ▾ Thinking ({msg.thinking.length} chars)
                    </summary>
                    <div className="mt-1 whitespace-pre-wrap">
                      {msg.thinking}
                    </div>
                  </details>
                )}
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
                      components={{
                        // Intercept link clicks so the wry webview
                        // never navigates away from the chat. Image
                        // URLs open in a lightbox; everything else
                        // hands off to the OS browser.
                        a: ({ href, children, ...rest }) => (
                          <a
                            {...rest}
                            href={href}
                            onClick={(e) =>
                              handleChatLinkClick(e, href ?? "")
                            }
                          >
                            {children}
                          </a>
                        ),
                        // Markdown `![alt](url)` images render inline.
                        // Click-to-zoom isn't needed: MCP-Apps tools
                        // produce their own iframe widgets, and any
                        // other inline image (e.g. attached by the
                        // user) is already shown at full bubble width.
                        img: ({ src, alt, ...rest }) => (
                          <img
                            {...rest}
                            src={src}
                            alt={alt}
                            style={{
                              maxWidth: "100%",
                              height: "auto",
                              borderRadius: 6,
                            }}
                          />
                        ),
                      }}
                    >
                      {stripThinkBlocks(msg.content)}
                    </ReactMarkdown>
                  </div>
                ) : isError ? (
                  <span>
                    <span aria-hidden="true" style={{ marginRight: 6 }}>
                      ⚠
                    </span>
                    {msg.content}
                  </span>
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
        }), [messages, copiedMessageIndex, copyMessage, handleChatLinkClick]);

  const awaitingUserAnswer = askPrompt !== null;
  const inputDisabled = streaming && !awaitingUserAnswer;
  const submitDisabled = awaitingUserAnswer
    ? !input.trim()
    : streaming || (!input.trim() && attachments.length === 0);
  // The full question now renders as a markdown card above the input
  // (see `<AskCard>` below) — the placeholder is just a short hint
  // that points at the card. Truncating multi-line markdown into a
  // single-line placeholder was unreadable.
  const inputPlaceholder = awaitingUserAnswer
    ? "Type your reply…"
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
        {/* Empty-state hero — count only user/assistant turns. System
            bubbles (MCP "connected" notices, slash-output, skill model
            notes, etc.) can appear before the user has typed anything;
            we still want the logo + caption to greet them in that
            case. The system bubbles render normally in the .map below
            so the user sees both the hero AND the status messages. */}
        {messages.every((m) => m.role === "system") && (
          <div
            className="flex flex-col items-center mt-20 select-none"
            style={{ color: "var(--text-secondary)" }}
          >
            <img
              src={themeMode === "light" ? logoLight : logoDark}
              alt="thClaws"
              className="mb-2 opacity-90"
              style={{ width: 280, height: 280 }}
              draggable={false}
            />
            {version && (
              <div
                className="text-xs font-mono mb-2 opacity-70"
                style={{ color: "var(--text-secondary)" }}
              >
                v{version}
              </div>
            )}
            <div className="text-sm">Chat mode — send a message to start</div>
          </div>
        )}
        {messageElements}
        {streaming && waitingFirstByte && (
          <div className="flex justify-start">
            <div
              className="rounded-lg px-3 py-2 text-xs"
              style={{
                background: "var(--bg-secondary)",
                color: "var(--text-secondary)",
                fontStyle: "italic",
              }}
            >
              Waiting for first response… (some hosted models cold-start
              for 30–120s before the first byte)
            </div>
          </div>
        )}
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
        {askPrompt && askPrompt.question && (
          <div
            className="rounded p-3 max-h-64 overflow-y-auto"
            style={{
              background: "var(--bg-tertiary)",
              border: "1px solid var(--accent)",
            }}
          >
            <div
              className="text-[10px] uppercase tracking-wider mb-1.5 flex items-center gap-1.5"
              style={{ color: "var(--accent)" }}
            >
              <span>Assistant is asking</span>
            </div>
            <div className="markdown-body text-sm">
              <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                rehypePlugins={[rehypeHighlight]}
              >
                {askPrompt.question}
              </ReactMarkdown>
            </div>
          </div>
        )}
        <div className="flex gap-2 items-end">
          {!HAS_WRY_TRANSPORT && (
            <>
              <input
                ref={fileInputRef}
                type="file"
                multiple
                onChange={onUploadFilesSelected}
                style={{ display: "none" }}
              />
              <button
                type="button"
                onClick={onUploadButtonClick}
                disabled={uploading || streaming}
                aria-label="Upload files"
                title={`Upload up to ${MAX_UPLOAD_FILES} files (max ${MAX_UPLOAD_BYTES / 1024 / 1024} MB each) to ./uploads/`}
                className="px-2 py-2 rounded text-sm transition-colors inline-flex items-center justify-center"
                style={{
                  background: "var(--bg-tertiary)",
                  color: uploading ? "var(--text-secondary)" : "var(--text-primary)",
                  border: "1px solid var(--border)",
                  cursor: uploading || streaming ? "not-allowed" : "pointer",
                  minHeight: "36px",
                }}
              >
                <Paperclip size={16} />
              </button>
            </>
          )}
          <textarea
            ref={inputRef}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={handleInputKeyDown}
            onPaste={onPaste}
            placeholder={inputPlaceholder}
            disabled={inputDisabled}
            rows={1}
            className="flex-1 px-3 py-2 rounded text-sm outline-none resize-none"
            style={{
              background: "var(--bg-tertiary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
              lineHeight: "20px",
              minHeight: "36px",
              fontFamily: "inherit",
            }}
          />
          {streaming && !awaitingUserAnswer ? (
            // While the agent is generating, the Send button is
            // disabled anyway — repurpose the slot for a Stop button
            // that fires shell_cancel. Mirrors the Cmd+. / Esc
            // hotkeys with a discoverable affordance for users who
            // don't know the keyboard shortcut yet.
            <button
              type="button"
              onClick={() => send({ type: "shell_cancel" })}
              className="px-4 py-2 rounded text-sm font-medium transition-colors inline-flex items-center gap-1.5"
              style={{
                background: "var(--danger, #c0392b)",
                color: "#fff",
                cursor: "pointer",
              }}
              title="Stop the agent (Esc / Cmd+. / Ctrl+.)"
              aria-label="Stop"
            >
              <span
                aria-hidden="true"
                style={{
                  display: "inline-block",
                  width: 10,
                  height: 10,
                  background: "#fff",
                  borderRadius: 1,
                }}
              />
              Stop
            </button>
          ) : (
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
          )}
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
