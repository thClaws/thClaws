import { useState, useRef, useEffect, useMemo, useCallback } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { resolveAssetSrc } from "../lib/fileAsset";
import { Check, Copy, Paperclip } from "lucide-react";
import { basePath, send, subscribe, sessionActionError } from "../hooks/useIPC";
import { promptHistory, recordPrompt } from "../hooks/promptHistory";
import { useTheme } from "../hooks/useTheme";
import { useVersion } from "../hooks/useVersion";
import logoDark from "../assets/thClaws-logo-dark.png";
import logoLight from "../assets/thClaws-logo-light.png";
import { WorkflowReviewBubble } from "./WorkflowReviewBubble";
import {
  SlashCommandPopup,
  filterCommands,
  type SlashCommandInfo,
} from "./SlashCommandPopup";
import { McpAppIframe } from "./McpAppIframe";

type ChatMessage = {
  role: "user" | "assistant" | "tool" | "system" | "error" | "workflow_review";
  content: string;
  /// `system` messages only — marks a bubble that accumulates a slash
  /// command's streamed output (e.g. `/cloud push` progress). Consecutive
  /// `chat_slash_output` lines fold into ONE such bubble instead of a balloon
  /// per line, and progress lines (`… MB (N%)`) update its last line in place.
  slashOutput?: boolean;
  /// `workflow_review` messages only — dev-plan/32 Tier 3 GUI
  /// approval. Carries the script + id the WorkflowReviewBubble
  /// posts back when the user clicks Approve / Cancel / Re-author.
  workflowReview?: {
    id: string;
    script: string;
    prompt: string;
    model: string;
    revision: number;
  };
  /// `assistant` messages only — accumulated `reasoning_content` from
  /// thinking models (DeepSeek v4/r1, OpenAI o-series, NVIDIA NIM
  /// glm4.7, etc.). Rendered as a collapsible dimmed block above the
  /// assistant text so the user can see the model is working without
  /// the reasoning blending into the final answer.
  thinking?: string;
  /// Total accumulated thinking length. `thinking` above is capped at
  /// MAX_THINKING_CHARS (tail kept) so an hours-long reasoning stream
  /// can't grow the webview unbounded — this carries the real count
  /// for the summary line.
  thinkingChars?: number;
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
  /// Mid-turn injection state (issue #106): "queued" while the
  /// message is waiting in the agent's injection queue,
  /// "delivered" once the agent drained it at a tool_result
  /// boundary. Absent for normal user messages submitted at turn
  /// start. Drives the small badge next to the bubble.
  injectionState?: "queued" | "delivered";
  /// Local-only id used to match the optimistic queued bubble with
  /// the `user_message_injected` event that arrives once the agent
  /// drains the queue. Same value the IPC payload carries in
  /// `id`. Not persisted.
  injectionId?: string;
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
// A pasted text blob at/above this length is routed to an _uploads/ file (path
// injected) instead of dumped into the composer. ~2k chars ≈ 2-3 dense
// paragraphs — small enough to stay editable inline, large enough that a whole
// article / log / source file goes to a file so it doesn't bloat the prompt.
const PASTE_TO_FILE_THRESHOLD = 2000;
const MAX_UPLOAD_FILES = 5;

/// Cap on the thinking text a bubble HOLDS (not what the model produced —
/// the full stream persists in the session JSONL). Long agentic runs
/// accumulate megabytes of reasoning; every delta re-rendered the whole
/// open `<details>` block and the unbounded string eventually killed the
/// webview's web-content process (GUI "crash" — the CLI, which just
/// prints, was unaffected). Keep the tail: that's what the user watches
/// live. 150k chars ≈ a few thousand lines, bounded relayout cost.
const MAX_THINKING_CHARS = 150_000;

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

/// Detect bare multi-line JSON object/array blocks at line-start and
/// wrap them in ```json fences before handing to ReactMarkdown.
///
/// Without this pass, markdown collapses single newlines inside an
/// unfenced JSON block to spaces — so a tool response the model echoes
/// back as
///   {
///     "next_action": "first_greet",
///     ...
///   }
/// renders as a single-line wall instead of the indented block the
/// terminal tab shows (xterm.js just replaces \n with \r\n and the
/// monospace renderer preserves layout).
///
/// Walks the text once with a brace counter so nested braces inside
/// the JSON don't terminate early. Skips regions already inside a ```
/// fence. Only wraps a candidate if `JSON.parse` accepts it — keeps
/// false positives off (a paragraph that happens to start with `{`
/// is left alone).
function wrapBareJsonBlocks(content: string): string {
  // Map of already-fenced regions to skip.
  const fenced: [number, number][] = [];
  const fenceRe = /```[\s\S]*?```/g;
  let fm: RegExpExecArray | null;
  while ((fm = fenceRe.exec(content)) !== null) {
    fenced.push([fm.index, fm.index + fm[0].length]);
  }
  const inFence = (i: number) => fenced.some(([s, e]) => i >= s && i < e);

  const out: string[] = [];
  let i = 0;
  while (i < content.length) {
    const ch = content[i];
    const atLineStart = i === 0 || content[i - 1] === "\n";
    if (atLineStart && (ch === "{" || ch === "[") && !inFence(i)) {
      const close = ch === "{" ? "}" : "]";
      let depth = 0;
      let j = i;
      let inString = false;
      let escape = false;
      while (j < content.length) {
        const c = content[j];
        if (escape) {
          escape = false;
          j++;
          continue;
        }
        if (inString) {
          if (c === "\\") escape = true;
          else if (c === '"') inString = false;
          j++;
          continue;
        }
        if (c === '"') inString = true;
        else if (c === ch) depth++;
        else if (c === close) {
          depth--;
          if (depth === 0) {
            j++;
            break;
          }
        }
        j++;
      }
      if (depth === 0 && j > i + 1) {
        const candidate = content.substring(i, j);
        // Only wrap if it's actually valid JSON. Streaming partial
        // blocks (depth never reached 0) and prose paragraphs that
        // happen to start with `{` will fall through here.
        try {
          JSON.parse(candidate);
          out.push("```json\n", candidate, "\n```");
          i = j;
          continue;
        } catch {
          /* not valid JSON — leave as-is */
        }
      }
    }
    out.push(ch);
    i++;
  }
  return out.join("");
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result;
      if (typeof result === "string") resolve(dataUrlToBase64(result));
      else reject(new Error("FileReader: non-string result"));
    };
    reader.onerror = () =>
      reject(reader.error ?? new Error("FileReader failed"));
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
  const [sessionBlocked, setSessionBlocked] = useState<string | null>(null);
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
  // Auto-scroll only when the user is parked at the bottom. When they
  // scroll up to read history, streamed tokens must NOT yank them back
  // down (issue #170). Updated by the messages container's onScroll.
  const messagesContainerRef = useRef<HTMLDivElement>(null);
  const isAtBottomRef = useRef(true);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Bash-style prompt-history recall (shared with the Terminal tab). -1 = not
  // navigating (the textarea holds the user's own draft); otherwise an index
  // into promptHistory(). savedDraft restores the draft on Down past the newest.
  const histIndexRef = useRef(-1);
  const savedDraftRef = useRef("");
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [uploading, setUploading] = useState(false);
  // IDs of drag-drop `file_upload` requests in flight, so the shared
  // `file_upload_result` subscriber injects the path only for OUR uploads
  // (the Files tab uses the same IPC with its own ids).
  const droppedUploadIds = useRef<Set<string>>(new Set());
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
    !askPrompt &&
    !streaming &&
    input.startsWith("/") &&
    !input.slice(1).includes(" ");
  const slashQuery = slashOpen ? input.slice(1).split(/\s/)[0] : "";
  const slashFiltered = slashOpen
    ? filterCommands(slashCommands, slashQuery)
    : [];

  const showAttachmentError = (msg: string) => {
    setAttachmentError(msg);
    if (errorTimerRef.current !== null)
      window.clearTimeout(errorTimerRef.current);
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

  /// Save a dropped file to the workspace `_uploads/` dir (backend
  /// `file_upload` IPC — works desktop + `--serve`) and, on success, inject
  /// its workspace-relative path into the composer as text. This is what lets
  /// a dropped doc feed `/summarize`, `/translate`, `/extract` — the subagents
  /// Read the path, so the file's bytes never bloat the prompt. Images get
  /// this too (path injected) *and* the inline attach below.
  const uploadAndInjectPath = async (file: File) => {
    if (file.size > MAX_UPLOAD_BYTES) {
      showAttachmentError(
        `${file.name} is ${(file.size / 1024 / 1024).toFixed(1)} MB (max ${MAX_UPLOAD_BYTES / 1024 / 1024} MB)`,
      );
      return;
    }
    let data: string;
    try {
      data = await blobToBase64(file);
    } catch {
      showAttachmentError(`Couldn't read ${file.name}`);
      return;
    }
    const id = crypto.randomUUID();
    droppedUploadIds.current.add(id);
    const safeName =
      (file.name.split(/[/\\]/).pop() || "file").trim() || "file";
    setUploading(true);
    send({ type: "file_upload", id, path: `_uploads/${safeName}`, data });
  };

  const onPaste = (e: React.ClipboardEvent) => {
    if (askPrompt) return;
    const items = e.clipboardData?.items;
    if (items) {
      for (const item of Array.from(items)) {
        if (item.kind === "file" && item.type.startsWith("image/")) {
          const file = item.getAsFile();
          if (file) {
            e.preventDefault();
            void addImageBlob(file);
          }
        }
      }
    }
    // Large text paste → save it to _uploads/ and inject the path, instead of
    // stuffing the composer (and, on send, the whole prompt). Small pastes fall
    // through to the default textarea insert so short snippets stay editable.
    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (text.length >= PASTE_TO_FILE_THRESHOLD) {
      e.preventDefault();
      void uploadAndInjectPath(
        new File([text], "pasted.txt", { type: "text/plain" }),
      );
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
      // Images within the inline limit: attach (so the model SEES them) AND
      // save+inject path. Oversized images skip the inline attach (its error
      // toast would collide with the upload's) but still get the path.
      if (file.type.startsWith("image/") && file.size <= MAX_IMAGE_BYTES) {
        void addImageBlob(file);
      }
      void uploadAndInjectPath(file);
    }
  };

  const removeAttachment = (id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  };

  const onUploadButtonClick = () => {
    if (uploading || streaming) return;
    fileInputRef.current?.click();
  };

  const onUploadFilesSelected = async (
    e: React.ChangeEvent<HTMLInputElement>,
  ) => {
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
      const resp = await fetch(`${basePath()}upload`, {
        method: "POST",
        body: form,
      });
      if (!resp.ok) {
        const detail = await resp.text().catch(() => "");
        showAttachmentError(`Upload failed (${resp.status}) ${detail}`);
      }
    } catch (err) {
      showAttachmentError(
        `Upload failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    } finally {
      setUploading(false);
      e.target.value = "";
    }
  };

  useEffect(() => {
    const unsub = subscribe((msg) => {
      switch (msg.type) {
        case "file_upload_result": {
          // Only handle results for OUR drag-drop uploads (the Files tab
          // uses the same IPC with its own id-correlation).
          const rid = typeof msg.id === "string" ? (msg.id as string) : "";
          if (!droppedUploadIds.current.has(rid)) break;
          droppedUploadIds.current.delete(rid);
          // Keep the spinner up while other files in the same batch finish.
          setUploading(droppedUploadIds.current.size > 0);
          if (msg.ok && typeof msg.path === "string") {
            const path = msg.path as string;
            // Inject the saved path into the composer as text (space-padded),
            // so the user can prepend /summarize · /translate · /extract.
            setInput(
              (prev) =>
                `${prev}${prev && !prev.endsWith(" ") ? " " : ""}${path} `,
            );
            inputRef.current?.focus();
          } else {
            showAttachmentError(
              `Upload failed: ${(msg.error as string) ?? "unknown"}`,
            );
          }
          break;
        }
        case "chat_user_message": {
          // Echo of a prompt the user submitted (possibly from the
          // Terminal tab — we render it as a user bubble either way).
          //
          // Special case for mid-turn injection (issue #106): if there's
          // a local optimistic bubble in `queued` state with matching
          // text, flip it to `delivered` instead of appending a duplicate.
          // The first match wins; only the first matching queued bubble
          // is flipped.
          const incoming = msg.text as string;
          setMessages((prev) => {
            const queuedIdx = prev.findIndex(
              (m) =>
                m.role === "user" &&
                m.injectionState === "queued" &&
                m.content === incoming,
            );
            if (queuedIdx >= 0) {
              const next = prev.slice();
              next[queuedIdx] = {
                ...next[queuedIdx],
                injectionState: "delivered",
              };
              return next;
            }
            return [...prev, { role: "user", content: incoming }];
          });
          break;
        }
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
            return [
              ...prev,
              { role: "assistant", content: msg.text as string },
            ];
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
              const total =
                (last.thinkingChars ?? last.thinking?.length ?? 0) +
                chunk.length;
              let thinking = (last.thinking ?? "") + chunk;
              if (thinking.length > MAX_THINKING_CHARS) {
                thinking = thinking.slice(-MAX_THINKING_CHARS);
              }
              return [
                ...prev.slice(0, -1),
                { ...last, thinking, thinkingChars: total },
              ];
            }
            return [
              ...prev,
              {
                role: "assistant",
                content: "",
                thinking: chunk.slice(-MAX_THINKING_CHARS),
                thinkingChars: chunk.length,
              },
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
              toolKind:
                typeof msg.tool_name === "string" ? msg.tool_name : undefined,
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
        case "chat_slash_output": {
          // Fold a command's streamed output (e.g. /cloud push) into ONE
          // system bubble instead of a balloon per line. A progress line
          // (`… N/M MB (P%)`) replaces the previous progress line in place so
          // the upload counter ticks in a single spot rather than stacking.
          const line = msg.text as string;
          setMessages((prev) => {
            const last = prev[prev.length - 1];
            if (last && last.role === "system" && last.slashOutput) {
              const lines = last.content.split("\n");
              const prevLast = lines[lines.length - 1] ?? "";
              const isProgress = (s: string) =>
                /\(\s*\d+\s*%\s*\)\s*$/.test(s) ||
                /\d[\d.,]*\s*\/\s*\d[\d.,]*\s*[KMGT]?B/i.test(s);
              // Text before the first number — same for consecutive ticks of
              // the same operation ("uploading… "), so we can replace vs append.
              const stem = (s: string) => s.replace(/[\d.,].*$/s, "");
              if (
                isProgress(line) &&
                isProgress(prevLast) &&
                stem(line) === stem(prevLast)
              ) {
                lines[lines.length - 1] = line;
              } else {
                lines.push(line);
              }
              return [
                ...prev.slice(0, -1),
                { ...last, content: lines.join("\n") },
              ];
            }
            return [
              ...prev,
              { role: "system", content: line, slashOutput: true },
            ];
          });
          break;
        }
        case "chat_workflow_review": {
          // dev-plan/32 Tier 3: spawn a review bubble. We replace any
          // earlier review bubble for the same id (re-author cycle
          // emits a fresh `WorkflowReviewRequest` with revision + 1)
          // so the user sees the latest revision in place rather
          // than a growing stack.
          const id = typeof msg.id === "string" ? msg.id : "";
          const script = typeof msg.script === "string" ? msg.script : "";
          const prompt = typeof msg.prompt === "string" ? msg.prompt : "";
          const model = typeof msg.model === "string" ? msg.model : "?";
          const revision = typeof msg.revision === "number" ? msg.revision : 0;
          if (!id) break;
          setMessages((prev) => {
            const existing = prev.findIndex(
              (m) =>
                m.role === "workflow_review" && m.workflowReview?.id === id,
            );
            const next: ChatMessage = {
              role: "workflow_review",
              content: "",
              workflowReview: { id, script, prompt, model, revision },
            };
            if (existing >= 0) {
              const out = prev.slice();
              out[existing] = next;
              return out;
            }
            return [...prev, next];
          });
          break;
        }
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
        case "chat_turn_usage":
          // Per-turn token/cost footer (parity with the CLI REPL's
          // `[tokens: …in/…out · …s · $… session]`). Rendered as the same
          // muted system line as skill notes / slash output.
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
        case "session_action_rejected":
          setStreaming(false);
          setMessages((prev) => [...prev, {role: "system", content: String(msg.text)}]);
          break;
        case "session_view_state":
          setSessionBlocked(typeof msg.blocked === "string" ? msg.blocked : null);
          setStreaming(msg.running === true);
          break;
        case "initial_state":
          // (Re)connect handshake. If the worker has a turn in flight —
          // e.g. this browser detached during a long TextToSpeech/render
          // and is reconnecting mid-run — restore the "working" indicator
          // so the still-running turn doesn't look stopped. The live event
          // stream we re-subscribed to on connect delivers the eventual
          // chat_done, which clears it. Never force it false here: a
          // handshake mustn't cancel the visual state of a turn we started.
          if (msg.agent_busy === true) setStreaming(true);
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
            setMessages((prev) => {
              // A replay of the conversation we are ALREADY showing is a
              // no-op — and repainting from it would be destructive: the
              // store keeps only user/assistant/tool messages, so the
              // turn's thinking block and its `[tokens: …]` footer would
              // vanish. (They did: any redundant session_load wiped the
              // live footer, which read as "the usage line is gone".)
              // Compare against the persisted-shaped subset of what's on
              // screen and keep the richer live view when they match.
              const incoming = msg.messages as {
                role: string;
                content: string;
                thinking?: string;
              }[];
              // Compare on SPOKEN text only. The live view carries shapes
              // the store never sees — a thinking-only assistant bubble,
              // tool markers, the usage footer — so a strict element-by-
              // element match would call an identical conversation
              // "different" and repaint anyway.
              const spoken = (rows: { role: string; content: string }[]) =>
                rows
                  .filter(
                    (m) =>
                      (m.role === "user" || m.role === "assistant") &&
                      (m.content ?? "").trim() !== "",
                  )
                  .map((m) => `${m.role}:${m.content}`);
              const mine = spoken(prev);
              const theirs = spoken(incoming);
              const sameConversation =
                mine.length === theirs.length &&
                mine.every((line, i) => line === theirs[i]);
              if (sameConversation && mine.length > 0) return prev;
              return incoming.map((m) => {
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
                // Reasoning is persisted with the turn, so a reloaded
                // conversation keeps its collapsed thinking blocks
                // instead of losing them at the session boundary.
                return {
                  role,
                  content: m.content,
                  ...(m.thinking ? { thinking: m.thinking } : {}),
                } satisfies ChatMessage;
              });
            });
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
            firstLine.length > 240 ? `${firstLine.slice(0, 237)}…` : firstLine;
          const seconds = (durationMs / 1000).toFixed(1);
          const content = `✓ /${agentName} done in ${seconds}s — ${truncated}${
            id ? `  (id: ${id})` : ""
          }`;
          setMessages((prev) => [...prev, { role: "system", content }]);
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
            firstLine.length > 240 ? `${firstLine.slice(0, 237)}…` : firstLine;
          const content = `✗ /${agentName} failed — ${truncated}${
            id ? `  (id: ${id})` : ""
          }`;
          setMessages((prev) => [...prev, { role: "system", content }]);
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

  // Copy any selected page text (message bubbles, tool output, code, …) on
  // Cmd/Ctrl+C. The desktop webview blocks navigator.clipboard, so route the
  // selection through the IPC bridge (browser --serve mode uses the native
  // clipboard). Textarea/input selections report empty via window.getSelection,
  // so this never shadows normal input-field copy — only DOM text selections
  // are intercepted. Scoped to the active tab so it doesn't fire over Terminal.
  useEffect(() => {
    if (!active) return;
    const onCopyKey = (e: KeyboardEvent) => {
      const isMac = navigator.platform.startsWith("Mac");
      const mod = isMac ? e.metaKey : e.ctrlKey;
      if (!mod || e.altKey || (e.key !== "c" && e.key !== "C")) return;
      const sel = window.getSelection()?.toString() ?? "";
      if (!sel) return; // no page selection → let the input / browser handle it
      e.preventDefault();
      if (typeof window !== "undefined" && !window.ipc && navigator.clipboard) {
        navigator.clipboard.writeText(sel).catch(() => {});
      } else {
        send({ type: "clipboard_write", text: sel });
      }
    };
    document.addEventListener("keydown", onCopyKey, true);
    return () => document.removeEventListener("keydown", onCopyKey, true);
  }, [active]);

  useEffect(() => {
    // Only follow new content if the user hasn't scrolled up to read.
    if (isAtBottomRef.current) {
      bottomRef.current?.scrollIntoView({ behavior: "smooth" });
    }
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
  const handleChatLinkClick = useCallback(
    (e: React.MouseEvent<HTMLAnchorElement>, href: string) => {
      if (!href) return;
      e.preventDefault();
      send({ type: "open_external", url: href });
    },
    [],
  );

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    const blocked = sessionActionError();
    if (blocked) { setMessages((prev) => [...prev, {role: "system", content: blocked}]); return; }
    const text = input.trim();
    // Record every non-empty submission in the shared recall ring and reset
    // navigation so the next Up starts from the newest entry.
    if (text) {
      recordPrompt(text);
      histIndexRef.current = -1;
      savedDraftRef.current = "";
    }
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
    if (!text && attachments.length === 0) return;
    if (attachments.length > 10) {
      showAttachmentError("At most 10 attachments are allowed. Remove some images and send again.");
      return;
    }
    if (attachments.reduce((total, image) => total + image.data.length, 0) > 67 * 1024 * 1024) {
      showAttachmentError("Attachments exceed the 67 MiB base64-encoded payload limit. Reduce their size and send again.");
      return;
    }

    // Mid-turn injection path (issue #106): if the agent is already
    // streaming, the message goes into the agent's injection queue
    // instead of starting a new turn. The agent drains the queue at
    // the next tool_result boundary and folds the text into the
    // conversation so the user can steer mid-task. Attachments
    // can't ride along — injection only supports plain text in v1.
    if (streaming && text && !text.startsWith("/")) {
      const injectionId = `inj-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      setInput("");
      setMessages((prev) => [
        ...prev,
        {
          role: "user",
          content: text,
          injectionState: "queued",
          injectionId,
        },
      ]);
      send({ type: "user_input_inject", text, id: injectionId });
      return;
    }
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
    //
    // `/workflow run` and `/workflow resume` are the slash commands
    // that do genuinely long-running work (author + review + execute /
    // replay + execute), so flip streaming on for them too — otherwise
    // the Stop button (visible only while `streaming === true`) stays
    // hidden during the workflow lifecycle. The backend emits TurnDone
    // at the end of the workflow, which restores `streaming = false`
    // via the chat_done handler.
    const isLongRunningSlash =
      text.startsWith("/workflow run") || text.startsWith("/workflow resume");
    if (!text.startsWith("/") || isLongRunningSlash) {
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
    // Prompt-history recall (bash-style Up/Down), shared with the Terminal tab.
    // The slash popup owns the arrows when open (handled above, returns first).
    // Entering history from a fresh draft needs the caret on the edge line so
    // multi-line editing keeps working; once navigating, Up/Down always cycle.
    if (
      (e.key === "ArrowUp" || e.key === "ArrowDown") &&
      !e.nativeEvent.isComposing
    ) {
      const el = e.currentTarget;
      const caret = el.selectionStart ?? input.length;
      const navigating = histIndexRef.current !== -1;
      const hist = promptHistory();
      const caretToEnd = () =>
        requestAnimationFrame(() => {
          const t = inputRef.current;
          if (t) t.selectionStart = t.selectionEnd = t.value.length;
        });
      if (e.key === "ArrowUp") {
        const atFirstLine = !input.slice(0, caret).includes("\n");
        if ((navigating || atFirstLine) && hist.length > 0) {
          e.preventDefault();
          if (histIndexRef.current === -1) {
            savedDraftRef.current = input;
            histIndexRef.current = hist.length - 1;
          } else if (histIndexRef.current > 0) {
            histIndexRef.current -= 1;
          }
          setInput(hist[histIndexRef.current]);
          caretToEnd();
          return;
        }
      } else if (navigating) {
        // ArrowDown while navigating → newer entry, or restore the draft.
        e.preventDefault();
        if (histIndexRef.current < hist.length - 1) {
          histIndexRef.current += 1;
          setInput(hist[histIndexRef.current]);
        } else {
          histIndexRef.current = -1;
          setInput(savedDraftRef.current);
          savedDraftRef.current = "";
        }
        caretToEnd();
        return;
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

  const messageElements = useMemo(
    () =>
      messages.map((msg, i) => {
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
        // dev-plan/32 Tier 3 workflow review bubble. Renders a
        // dedicated card with Approve / Cancel / Re-author buttons;
        // each click posts a `workflow_decision` back through IPC.
        if (msg.role === "workflow_review" && msg.workflowReview) {
          const wr = msg.workflowReview;
          return (
            <WorkflowReviewBubble
              key={`${i}-${wr.id}-${wr.revision}`}
              id={wr.id}
              script={wr.script}
              prompt={wr.prompt}
              model={wr.model}
              revision={wr.revision}
            />
          );
        }
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
                className={`group flex max-w-[92%] sm:max-w-[80%] flex-col gap-1 ${widget || todos ? "w-[92%] sm:w-[80%]" : ""}`}
                style={{
                  color: "var(--text-secondary)",
                  fontFamily: "Menlo, Monaco, 'Courier New', monospace",
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
              className={`group relative max-w-[92%] sm:max-w-[80%] rounded-lg py-2 pl-3 pr-9 text-sm ${isAssistant ? "" : "whitespace-pre-wrap"}`}
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
                    ▾ Thinking (
                    {(
                      msg.thinkingChars ?? msg.thinking.length
                    ).toLocaleString()}{" "}
                    chars
                    {(msg.thinkingChars ?? 0) > msg.thinking.length
                      ? `, showing last ${msg.thinking.length.toLocaleString()}`
                      : ""}
                    )
                  </summary>
                  <div className="mt-1 whitespace-pre-wrap">
                    {
                      // While the reasoning is still streaming (no
                      // answer text yet → details forced open), render
                      // only a tail ticker: relayouting the full block
                      // on every delta is what froze the webview on
                      // hours-long runs. The full (capped) text renders
                      // once the answer starts and the block collapses.
                      msg.content || msg.thinking.length <= 4000
                        ? msg.thinking
                        : `…${msg.thinking.slice(-4000)}`
                    }
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
                          onClick={(e) => handleChatLinkClick(e, href ?? "")}
                        >
                          {children}
                        </a>
                      ),
                      // Markdown `![alt](url)` images render inline.
                      // A workspace-relative src (e.g. `output/img-….jpg`
                      // written by TextToImage) is routed through
                      // /file-asset via resolveAssetSrc — otherwise the
                      // browser resolves it against the page origin and
                      // 404s. Click-to-zoom isn't needed: MCP-Apps tools
                      // produce their own iframe widgets, and any other
                      // inline image (e.g. attached by the user) is
                      // already shown at full bubble width.
                      img: ({ src, alt, ...rest }) => (
                        <img
                          {...rest}
                          src={resolveAssetSrc(
                            typeof src === "string" ? src : undefined,
                          )}
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
                    {wrapBareJsonBlocks(stripThinkBlocks(msg.content))}
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
              {msg.injectionState && (
                // Mid-turn injection badge (issue #106). "queued"
                // → message sits in the agent's queue; "delivered"
                // → agent drained it at a tool_result boundary and
                // the LLM now sees it in the next turn's context.
                <span
                  className="ml-2 px-1.5 py-0.5 rounded text-[10px] uppercase tracking-wider align-middle"
                  style={{
                    background:
                      msg.injectionState === "queued"
                        ? "color-mix(in srgb, var(--accent) 18%, transparent)"
                        : "color-mix(in srgb, #22c55e 22%, transparent)",
                    color:
                      msg.injectionState === "queued"
                        ? "var(--accent)"
                        : "#22c55e",
                    fontWeight: 600,
                  }}
                  title={
                    msg.injectionState === "queued"
                      ? "Queued — the agent will see this at the next tool boundary"
                      : "Delivered — the agent has read this message"
                  }
                >
                  {msg.injectionState === "queued" ? "queued" : "delivered"}
                </span>
              )}
              <CopyMessageButton
                copied={copied}
                onCopy={() => copyMessage(msg, i)}
              />
            </div>
          </div>
        );
      }),
    [messages, copiedMessageIndex, copyMessage, handleChatLinkClick],
  );

  const awaitingUserAnswer = askPrompt !== null;
  // Allow typing while the agent is streaming so the user can queue
  // a mid-turn correction (issue #106). The textarea is only locked
  // when there's literally no place for input (e.g. AskUserQuestion
  // path uses a different prompt component).
  const inputDisabled = sessionBlocked !== null;
  // Submit-while-streaming is allowed for plain text (the message
  // queues into the agent's injection buffer). Slash commands and
  // file attachments don't take the inject path in v1 — keep the
  // old gate for those.
  const submitDisabled = sessionBlocked !== null || (awaitingUserAnswer
    ? !input.trim()
    : streaming
      ? !input.trim() || input.trim().startsWith("/") || attachments.length > 0
      : !input.trim() && attachments.length === 0);
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
        : "Type a message — drop a file to add its path (or paste/drop an image to attach)...";

  return (
    <div className="flex flex-col h-full">
      {/* Messages */}
      <div
        ref={messagesContainerRef}
        className="flex-1 overflow-y-auto p-4 space-y-3"
        style={{ background: "var(--bg-primary)" }}
        onScroll={() => {
          const el = messagesContainerRef.current;
          if (!el) return;
          // Within 64px of the bottom counts as "pinned" — so smooth
          // scrolls and 1px rounding don't unpin the follow behavior.
          isAtBottomRef.current =
            el.scrollHeight - el.scrollTop - el.clientHeight < 64;
        }}
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
              Waiting for first response… (some hosted models cold-start for
              30–120s before the first byte)
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
        {sessionBlocked && <div role="status" className="text-xs px-2" style={{ color: "var(--text-secondary)" }}>{sessionBlocked}</div>}
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
                title={`Upload up to ${MAX_UPLOAD_FILES} files (max ${MAX_UPLOAD_BYTES / 1024 / 1024} MB each) to _uploads/`}
                className="px-2 py-2 rounded text-sm transition-colors inline-flex items-center justify-center"
                style={{
                  background: "var(--bg-tertiary)",
                  color: uploading
                    ? "var(--text-secondary)"
                    : "var(--text-primary)",
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
            onChange={(e) => {
              setInput(e.target.value);
              // User edited the line → leave history navigation (their text is
              // the live draft again). Recall itself uses setInput, not onChange.
              histIndexRef.current = -1;
            }}
            onKeyDown={handleInputKeyDown}
            onPaste={onPaste}
            placeholder={inputPlaceholder}
            disabled={inputDisabled}
            rows={1}
            spellCheck={false}
            autoCorrect="off"
            autoCapitalize="off"
            autoComplete="off"
            data-gramm="false"
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
                background: submitDisabled
                  ? "var(--bg-tertiary)"
                  : "var(--accent)",
                color: submitDisabled
                  ? "var(--text-secondary)"
                  : "var(--accent-fg)",
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
