import { useState, useEffect, useCallback, useRef } from "react";
import {
  Folder,
  File,
  ArrowUp,
  Pencil,
  Eye,
  EyeOff,
  Save,
  X,
  FilePlus,
  FolderPlus,
  Download,
  Trash2,
  Library,
  Languages,
  FileText,
  Code2,
  Type,
  ListTree,
  Globe,
  FileOutput,
} from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import {
  DEFAULT_INGEST_MODE,
  INGEST_MODES,
  type IngestMode,
} from "./kmsIngestModes";
import { assetUrl, workspacePrefix } from "../lib/assetUrl";
import { parentDir } from "../lib/markdownRoundTrip";
import { useTheme } from "../hooks/useTheme";
import { MarkdownEditor } from "./MarkdownEditor";
import { CodeEditor } from "./CodeEditor";
import { EpubViewer } from "./EpubViewer";
import { SlideDeckViewer } from "./SlideDeckViewer";

// Target languages for the Files-tab "Translate" action. `code` becomes the
// `<stem>-<code>.md` filename suffix; `label` is what the combobox shows.
const TRANSLATE_LANGUAGES: ReadonlyArray<{ code: string; label: string }> = [
  { code: "th", label: "Thai (ไทย)" },
  { code: "en", label: "English" },
  { code: "zh", label: "Chinese (中文)" },
  { code: "ja", label: "Japanese (日本語)" },
  { code: "ko", label: "Korean (한국어)" },
  { code: "es", label: "Spanish (Español)" },
  { code: "fr", label: "French (Français)" },
  { code: "de", label: "German (Deutsch)" },
  { code: "pt", label: "Portuguese (Português)" },
  { code: "vi", label: "Vietnamese (Tiếng Việt)" },
  { code: "id", label: "Indonesian (Bahasa)" },
  { code: "hi", label: "Hindi (हिन्दी)" },
  { code: "ar", label: "Arabic (العربية)" },
  { code: "ru", label: "Russian (Русский)" },
];

// Confirmation dialog with two backends.
//
// Desktop (`wry` WebView in `--gui`): the IPC bridge (`window.ipc`)
// is present, so round-trip through the Rust backend to get a real
// native modal on macOS / Linux / Windows. The backend shows the
// dialog on its IPC worker thread and replies with a
// `confirm_result` message keyed by `id`.
//
// `--serve` (web browser): no `window.ipc`, so the IPC round-trip
// would never resolve. Fall back to the browser's built-in
// `window.confirm()`.
function platformConfirm(opts: {
  title: string;
  message: string;
  yesLabel?: string;
  noLabel?: string;
}): Promise<boolean> {
  return new Promise((resolve) => {
    const inBrowser = typeof window !== "undefined" && !window.ipc;
    if (inBrowser) {
      resolve(window.confirm(`${opts.title}\n\n${opts.message}`));
      return;
    }
    const id =
      typeof crypto !== "undefined" && "randomUUID" in crypto
        ? crypto.randomUUID()
        : `cf-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    const unsub = subscribe((msg) => {
      if (msg.type === "confirm_result" && msg.id === id) {
        unsub();
        resolve(Boolean(msg.ok));
      }
    });
    send({
      type: "confirm",
      id,
      title: opts.title,
      message: opts.message,
      yes_label: opts.yesLabel ?? "OK",
      no_label: opts.noLabel ?? "Cancel",
    });
  });
}

// "Convert to…" targets that make sense per source type; the conversion
// runs on the document service (DocConvert's `convert_file`).
function convertTargets(name: string): string[] {
  const ext = name.split(".").pop()?.toLowerCase() ?? "";
  if (["docx", "doc", "odt", "rtf", "txt", "html", "htm"].includes(ext)) {
    return ext === "docx" ? ["pdf"] : ["pdf", "docx"];
  }
  if (["xlsx", "xls", "ods", "csv"].includes(ext)) {
    return ext === "xlsx" ? ["pdf"] : ["pdf", "xlsx"];
  }
  if (["pptx", "ppt", "odp"].includes(ext)) {
    return ext === "pptx" ? ["pdf"] : ["pdf", "pptx"];
  }
  return [];
}

// Files the translator / summarizer subagents can read: markdown, plus the
// office formats and PDF their reader tools cover.
const LANG_ACTION_RE = /\.(md|markdown|pdf|docx|pptx|xlsx|xls)$/i;

type FileEntry = {
  name: string;
  is_dir: boolean;
};

interface Props {
  active: boolean;
}

// UI view mode — what the user is looking at.
type ViewMode = "preview" | "edit";
// Backend read mode — what we asked the server for. "preview" returns
// pre-rendered HTML for `.md` (and raw text for everything else);
// "source" always returns raw text.
type ReadMode = "preview" | "source";

// Extensions we can open in the text editor. Binary types (image /
// pdf) stay preview-only.
const TEXT_EDITABLE = new Set([
  "md", "markdown", "html", "htm", "js", "jsx", "mjs", "cjs", "ts", "tsx",
  "css", "scss", "sass", "less", "py", "pyi", "rs", "go", "java", "kt",
  "swift", "c", "cpp", "cc", "cxx", "h", "hpp", "hh", "cs", "rb", "php",
  "sh", "bash", "zsh", "fish", "json", "jsonc", "yaml", "yml", "toml",
  "xml", "svg", "sql", "lua", "vim", "Dockerfile", "dockerfile", "ini",
  "conf", "env", "gitignore", "txt", "log",
]);

// Subset of TEXT_EDITABLE for which we actually want the preview pane
// to render through CodeMirror (syntax highlighting + line numbers)
// instead of a plain <pre>. Plain-text extensions stay in <pre> since
// CodeMirror wouldn't add anything useful there.
const SYNTAX_PREVIEW = new Set([
  "js", "jsx", "mjs", "cjs", "ts", "tsx",
  "html", "htm", "css", "scss", "sass", "less",
  "py", "pyi", "rs", "go", "java", "kt",
  "c", "cpp", "cc", "cxx", "h", "hpp", "hh",
  "php", "json", "jsonc", "yaml", "yml", "xml", "svg", "sql",
]);

function extOf(path: string): string {
  const base = path.split("/").pop() ?? "";
  if (!base.includes(".")) return base.toLowerCase();
  return (base.split(".").pop() ?? "").toLowerCase();
}

function isTextEditable(path: string): boolean {
  return TEXT_EDITABLE.has(extOf(path));
}

// Read a dropped File as base64 (without the `data:…;base64,` prefix) for
// the `file_upload` IPC, which decodes it back to raw bytes on the Rust
// side — binary-safe, unlike the text-only `file_write`.
function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const r = reader.result;
      if (typeof r !== "string")
        return reject(new Error("FileReader: non-string result"));
      const comma = r.indexOf(",");
      resolve(comma >= 0 ? r.slice(comma + 1) : r);
    };
    reader.onerror = () => reject(reader.error ?? new Error("FileReader failed"));
    reader.readAsDataURL(file);
  });
}

// A context-menu row with a clear hover highlight (accent fill) that works
// across themes — the old `hover:bg-white/10` was invisible on the light
// theme. `danger` paints the resting state red (e.g. Delete); hovering any
// row fills it with the accent and flips the text to the accent foreground.
/// A panel beside the context menu, not below it.
///
/// The menu it hangs off is `position: fixed` at the click point with no
/// edge handling of its own, so this measures the menu's box and opens
/// to the left when the right edge is too close, and lifts itself when
/// the bottom is. Absolute inside the hovered row: the row is the
/// positioning context, which keeps the panel glued to it while the
/// menu's own height changes between file types.
///
/// Placement is written onto the node from a callback ref rather than
/// held in state. Measuring needs a real layout, and `useEffect` +
/// `setState` would mean a second render every time the panel opens —
/// plus the cascading-render lint this codebase already trips ten times
/// elsewhere. The caller remounts the panel (via `key`) when its
/// contents change height, which is what re-runs this.
function SubMenu({
  anchorRef,
  children,
}: {
  anchorRef: React.RefObject<HTMLDivElement | null>;
  children: React.ReactNode;
}) {
  const place = useCallback(
    (node: HTMLDivElement | null) => {
      if (!node) return;
      const menu = anchorRef.current?.getBoundingClientRect();
      const panel = node.getBoundingClientRect();
      if (!menu) return;
      if (menu.right + panel.width > window.innerWidth - 8) {
        node.style.left = "auto";
        node.style.right = "100%";
      } else {
        node.style.right = "auto";
        node.style.left = "100%";
      }
      // `panel.bottom` is where it sits now; anything past the viewport
      // comes back as a negative offset rather than a scrollbar.
      const over = panel.bottom - (window.innerHeight - 8);
      node.style.top = over > 0 ? `${-over}px` : "0px";
    },
    [anchorRef],
  );
  return (
    <div
      ref={place}
      className="absolute rounded border shadow-lg text-xs py-1"
      style={{
        top: 0,
        left: "100%",
        minWidth: "180px",
        maxHeight: "60vh",
        overflowY: "auto",
        zIndex: 57,
        background: "var(--bg-primary)",
        borderColor: "var(--border)",
        color: "var(--text-primary)",
      }}
    >
      {children}
    </div>
  );
}

function MenuItem({
  icon,
  label,
  danger,
  onClick,
  title,
}: {
  icon: React.ReactNode;
  label: string;
  danger?: boolean;
  onClick: () => void;
  /// Hover text. The ingest modes carry what each one costs and what
  /// it does not give you, which the label has no room for.
  title?: string;
}) {
  const [hover, setHover] = useState(false);
  return (
    <button
      type="button"
      className="flex items-center gap-2 w-full text-left px-3 py-1.5"
      style={{
        background: hover ? "var(--accent)" : undefined,
        color: hover ? "var(--accent-fg, #fff)" : danger ? "#ef4444" : undefined,
      }}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      onClick={onClick}
      title={title}
    >
      {icon} {label}
    </button>
  );
}

// Compact path for the explorer header — the viewer navbar already
// shows the full path. Root stays "."; nested dirs show "../<last>".
function shortPath(p: string): string {
  if (p === "." || p === "") return ".";
  const last = p.split("/").filter(Boolean).pop() ?? p;
  return p.includes("/") ? `../${last}` : last;
}

/// A file the dashboard bridge may read or write: a plain name beside the
/// dashboard, never a path, a dotfile, the agent's own instruction files,
/// or anything under `.thclaws/`.
function isDashboardFile(dir: string, filename: string): boolean {
  if (typeof filename !== "string" || filename.length === 0 || filename.length > 255) return false;
  if (/[/\\\0]/.test(filename) || filename.startsWith(".")) return false;
  if (/^(agents|claude)\.md$/i.test(filename)) return false;
  return !dir.split(/[/\\]/).some((seg) => seg === ".thclaws");
}

function isMarkdownPath(path: string): boolean {
  // Used to gate the iframe's `srcDoc` branch (vs. the asset-URL fetch
  // branch). Backend-rendered HTML previews — Markdown source files
  // *and* the Office formats whose extracted text we render through
  // the same comrak pipeline — both want srcDoc. Adding the office
  // extensions here is what makes Files-tab previews work for them.
  const e = extOf(path);
  return (
    e === "md" ||
    e === "markdown" ||
    e === "docx" ||
    e === "xlsx" ||
    e === "xlsm" ||
    e === "xlsb" ||
    e === "xls" ||
    e === "ods" ||
    e === "pptx"
  );
}

// In cloud, every workspace is mounted under `/u/<user>/<ws>/` by
// Traefik (see infra/k3s/runner/ingressroute.yaml.j2) and the
// strip-prefix middleware peels that off before forwarding to the
// runner pod. The pod's --serve also registers the file-asset route
// PREFIXED with the same path so the Host()+PathPrefix() rule
// Inject a <base href> pointing at the markdown file's parent directory
// via the file-asset handler so relative refs in srcDoc'd HTML (e.g.
// `<img src="img/foo.png">` from `![alt](img/foo.png)`) resolve to the
// .md file's sibling assets. Without this the srcDoc iframe has an
// opaque base URL and relative paths fail silently. The asset handler
// already enforces the sandbox check, so security is unchanged.
// Resolve a markdown link (`docs/spec.md`, `../notes.md`, `images/a%20b.png`,
// `docs/` for a folder) against the file it was written in, into a workspace
// path the file IPC understands. Percent-escapes are decoded per segment —
// generated indexes encode spaces and parens so the links stay valid markdown.
// Returns null when the link resolves to nothing usable.
// Desktop GUI (wry) vs `--serve` in a browser. wry injects `window.ipc`;
// the hosted webapp has no such bridge. The two differ in how a sandboxed
// preview frame can talk to the app — see the iframe's sandbox attribute.
const isDesktopShell = typeof window !== "undefined" && !!window.ipc;

function resolveWorkspaceLink(
  fromFile: string,
  href: string,
): { path: string; isDir: boolean } | null {
  const clean = href.split(/[?#]/)[0];
  if (!clean) return null;
  const isDir = clean.endsWith("/");
  const decode = (seg: string) => {
    try {
      return decodeURIComponent(seg);
    } catch {
      return seg;
    }
  };
  // An absolute link starts from the filesystem root; anything else starts
  // from the previewed file's directory.
  const fromDir = clean.startsWith("/")
    ? ""
    : fromFile.replace(/\\/g, "/").replace(/\/[^/]*$/, "");
  const parts = clean.startsWith("/")
    ? [""]
    : fromDir && fromDir !== fromFile
      ? fromDir.split("/")
      : [];
  for (const seg of clean.split("/")) {
    if (!seg || seg === ".") continue;
    if (seg === "..") {
      if (parts.length > 1 || (parts.length === 1 && parts[0] !== "")) parts.pop();
      continue;
    }
    parts.push(decode(seg));
  }
  const path = parts.join("/");
  if (!path || path === "/") return isDir ? { path: ".", isDir } : null;
  return { path, isDir };
}

// Windows: the IPC backend echoes the requested path back with `\`
// separators, while a markdown link resolves to the same file with `/`
// (see resolveLink / injectBaseHref above). Compare a separator-normalized
// form so a valid response is not mistaken for a stale one. Reported as
// issue #197 — clicking a linked file showed nothing at all.
function samePath(a: string, b: string): boolean {
  return a.replace(/\\/g, "/") === b.replace(/\\/g, "/");
}

function injectBaseHref(html: string, filePath: string): string {
  const normalized = filePath.replace(/\\/g, "/");
  const lastSlash = normalized.lastIndexOf("/");
  const dir = lastSlash >= 0 ? normalized.slice(0, lastSlash) : "";
  const segments = dir.split("/").map(encodeURIComponent).join("/");
  const leadingSlash = segments.startsWith("/") ? "" : "/";
  const baseHref = `${window.location.origin}${workspacePrefix()}/file-asset${leadingSlash}${segments}/`;
  return html.replace(/<head>/i, `<head><base href="${baseHref}">`);
}

export function FilesView({ active }: Props) {
  const [currentPath, setCurrentPath] = useState(".");
  // Show dotfile entries (`.thclaws/`, `.claude/`, `.env`, etc.) in
  // the listing. Off by default — the agent workspace usually has
  // dozens of dot-prefixed paths the user doesn't need to see. The
  // toggle persists for the lifetime of the tab (no localStorage —
  // it's a transient view setting, not a preference).
  const [showHidden, setShowHidden] = useState(false);
  const [entries, setEntries] = useState<FileEntry[]>([]);
  // Explorer right-click context menu (null = closed).
  const [explorerMenu, setExplorerMenu] = useState<{ x: number; y: number } | null>(
    null,
  );
  // Per-entry right-click menu (Download for files, navigate for dirs).
  // Separate from `explorerMenu` (which is the empty-area "new file
  // / new folder" menu) so the two don't shadow each other.
  const [entryMenu, setEntryMenu] = useState<
    { x: number; y: number; path: string; name: string; isDir: boolean } | null
  >(null);
  // KMS list (name/scope) for the entry menu's "Add to KMS" submenu.
  // Kept in sync via `kms_update` broadcasts; requested once on mount.
  const [kmsList, setKmsList] = useState<{ name: string; scope: string }[]>([]);
  // The "Add to knowledge base" flyout. The four modes used to be four
  // flat rows in a menu that already has eleven, and picking one with
  // several bases expanded a list *below* them, so the menu grew twice
  // over on the commonest file type. One row now, one panel beside it.
  //
  // `kmsPickMode` doubles as the panel's depth: null shows the modes,
  // a mode shows the bases for it. Only ever one panel — a second
  // flyout would need its own edge-flip against the first one's box.
  const [kmsFlyout, setKmsFlyout] = useState(false);
  const [kmsPickMode, setKmsPickMode] = useState<IngestMode | null>(null);
  // Measured so the panel can open to the left when the menu was
  // right-clicked near the right edge. The menu itself has no flip.
  const entryMenuRef = useRef<HTMLDivElement | null>(null);
  // New file / folder name modal. null = closed; otherwise which kind.
  const [createKind, setCreateKind] = useState<"file" | "folder" | null>(null);
  const [createName, setCreateName] = useState("");
  const [createError, setCreateError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  // Rename modal. null = closed; otherwise the entry being renamed.
  const [renameTarget, setRenameTarget] = useState<{
    path: string;
    name: string;
    isDir: boolean;
  } | null>(null);
  const [renameName, setRenameName] = useState("");
  const [renameError, setRenameError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  // Language-action modal (markdown only): translate or summarize into a
  // chosen language. null = closed; otherwise the .md file + which action.
  // `langCode` is the selected target language code.
  const [langAction, setLangAction] = useState<{
    mode: "translate" | "summarize";
    path: string;
    name: string;
  } | null>(null);
  const [langCode, setLangCode] = useState("th");
  // Folder-index modal. null = closed; otherwise the folder being indexed.
  // `recursive` / `force` mirror the FolderIndex tool's own options.
  const [indexAction, setIndexAction] = useState<{
    path: string;
    name: string;
  } | null>(null);
  const [indexLang, setIndexLang] = useState("th");
  // How much of the tree the index *lists*. "all" = every file grouped by
  // folder; "one-level" = this folder's files + one summarizing row per
  // subfolder (everything deeper is still scanned, it just isn't listed);
  // "flat" = don't descend at all.
  const [indexScope, setIndexScope] = useState<"all" | "one-level" | "flat">(
    "all",
  );
  const [indexForce, setIndexForce] = useState(false);
  const { resolved: themeMode } = useTheme();

  // The file being displayed. `content` is what the backend returned —
  // for preview mode of a `.md` file, that's the rendered HTML; for
  // source mode it's the raw text. `mime` drives the preview renderer;
  // `mode` echoes the request so we know which we're looking at.
  const [preview, setPreview] = useState<{
    path: string;
    content: string;
    mime: string;
    readMode: ReadMode;
    /// Set when the backend rendered the file into something the
    /// browser can show — a `.pptx` converted to PDF by the
    /// slide-render service. The viewer mounts THIS path; `path`
    /// stays the file the user actually opened.
    renderPath?: string;
    /// Per-slide PNGs of a rendered deck, in order. Present ⇒ the pane
    /// pages through the deck one slide at a time instead of dropping
    /// the whole PDF into a scrolling iframe.
    slides?: string[];
  } | null>(null);

  // Bumped on every Refresh click; used as part of iframe `key` props so
  // the iframe unmounts + re-fetches its asset (otherwise the browser
  // caches the iframe content even after the file on disk changes —
  // most visible with the productivity plugin's dashboard.html, which
  // an agent regenerates after every TASKS.md mutation).
  const [previewVersion, setPreviewVersion] = useState(0);
  // Path currently being rendered backend-side (a .pptx going through
  // the slide-render service). Cleared when its file_content lands.
  // Without this the tab looks frozen for the ~13s a cold render takes.
  const [renderPending, setRenderPending] = useState<string | null>(null);
  // A rendered deck previews one slide at a time; this flips that pane
  // to the PDF iframe instead (scrolling, text selection, print).
  // Per-file — reopening a deck starts back on the slides.
  const [deckAsPdf, setDeckAsPdf] = useState(false);

  const [mode, setMode] = useState<ViewMode>("preview");
  // Source-text kept separate from preview.content because the preview
  // content may be rendered HTML while the editor always operates on
  // raw text.
  const [editorSource, setEditorSource] = useState<string>("");
  const [editorDirty, setEditorDirty] = useState(false);
  // Edit a `.md` as raw markdown instead of through the WYSIWYG. The
  // rich editor round-trips through HTML, which preserves content but
  // normalizes formatting — it reflows hard-wrapped prose, respaces list
  // markers, and rewrites raw HTML blocks as markdown. Files with a
  // machine-read format (agent definitions, `{{screenshot:…}}` book
  // chapters) want the bytes left alone. Sticks for the tab's lifetime.
  const [mdSourceEdit, setMdSourceEdit] = useState(false);
  const [saveToast, setSaveToast] = useState<string | null>(null);
  const [publishNotice, setPublishNotice] = useState<string | null>(null);
  // True while files are being dragged over the tree panel (drop-to-upload).
  const [dragActive, setDragActive] = useState(false);
  const pendingNavigation = useRef<{ path: string } | null>(null);
  // The file the preview pane is *supposed* to be showing — set the moment a
  // file is requested, before its content comes back. `file_content`
  // responses for anything else are dropped.
  //
  // Without this the pane bounces: opening a file from a link in another
  // file's preview also walks the tree to the new folder, and that
  // `currentPath` change re-runs the 2s auto-refresh effect, which re-reads
  // whatever `preview.path` still holds — the file we just left. The stale
  // response lands after the requested one and snaps the pane back, so the
  // link looks like it flips between the two files.
  const requestedPathRef = useRef<string>("");

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "file_tree") {
        setEntries(msg.entries as FileEntry[]);
        if (msg.path) setCurrentPath(msg.path as string);
      } else if (msg.type === "file_content") {
        const incomingPath = msg.path as string;
        const incomingContent = msg.content as string;
        // Dashboard host bridge: if a dashboard requested THIS
        // file via the load message, forward the content back to
        // it and DON'T touch the preview pane state — the user
        // is viewing dashboard.html, not TASKS.md.
        const pending = pendingDashboardLoad.current;
        if (pending && pending.targetPath === incomingPath) {
          pendingDashboardLoad.current = null;
          try {
            pending.source.postMessage(
              {
                type: "thclaws-dashboard-load-ack",
                reqId: pending.reqId,
                ok: true,
                content: incomingContent,
              },
              "*",
            );
          } catch {
            // iframe was torn down between request and response —
            // benign.
          }
          return;
        }
        // Late response for a file the pane has already navigated away
        // from — drop it instead of letting it overwrite the current one.
        if (
          requestedPathRef.current &&
          !samePath(incomingPath, requestedPathRef.current)
        ) {
          return;
        }
        const incomingReadMode: ReadMode =
          (msg.mode as ReadMode) ?? "preview";
        setPreview({
          path: incomingPath,
          content: incomingContent,
          mime: msg.mime as string,
          readMode: incomingReadMode,
          renderPath: (msg.render_path as string) || undefined,
          slides: Array.isArray(msg.slides)
            ? (msg.slides as string[])
            : undefined,
        });
        setRenderPending(null);
        if (incomingReadMode === "source") {
          setEditorSource(incomingContent);
          setEditorDirty(false);
        }
      } else if (msg.type === "file_render_pending") {
        setRenderPending(msg.path as string);
      } else if (msg.type === "file_written") {
        const ok = msg.ok as boolean;
        const err = msg.error as string | null | undefined;
        if (ok) {
          setEditorDirty(false);
          setSaveToast("saved");
          // If the user had queued another file to open, do it now.
          if (pendingNavigation.current) {
            const p = pendingNavigation.current.path;
            pendingNavigation.current = null;
            openFile(p);
          }
        } else {
          setSaveToast(err ? `save failed: ${err}` : "save failed");
        }
        setTimeout(() => setSaveToast(null), 2500);
      }
    });
    send({ type: "file_list", path: ".", show_hidden: showHidden });
    return unsub;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Auto-refresh directory listing + preview while tab active.
  // Never auto-refresh when user is editing — we'd clobber their work.
  // `themeMode` is a dep so a light/dark swap re-fetches the .md
  // preview with the fresh palette baked into its iframe HTML.
  useEffect(() => {
    if (!active) return;
    // Don't re-read while a backend render is in flight. The tick is 2s
    // and a cold .pptx render takes ~13s, so without this each open
    // fires six more renders of the same deck — each one a fresh upload
    // and a fresh charge — before the first one answers.
    const refresh = () => {
      send({ type: "file_list", path: currentPath, show_hidden: showHidden });
      // Poll the file the pane was last *asked* for, not the one whose
      // content happens to be rendered — between a click and its response
      // those differ, and re-reading the old one flips the pane back to it.
      const target = requestedPathRef.current || preview?.path;
      if (target && mode === "preview" && !renderPending) {
        send({ type: "file_read", path: target, mode: "preview", theme: themeMode });
      }
    };
    refresh();
    const interval = setInterval(refresh, 2000);
    return () => clearInterval(interval);
  // `preview?.path` is intentional — using the full `preview` object
  // would re-run on every polling cycle (setPreview creates a new
  // reference each time), resetting the interval unnecessarily.
  // `showHidden` triggers an immediate refresh when toggled so the
  // listing flips without waiting for the next 2s tick.
  }, [active, currentPath, preview?.path, mode, themeMode, showHidden, renderPending]);

  // Drag-and-drop upload into the directory currently shown in the tree.
  // Each dropped file is base64-encoded and written via `file_upload`
  // (binary-safe, refuses to clobber). A per-upload self-unsubscribing
  // listener captures the *current* path/showHidden so the listing
  // refreshes correctly even though the mount-time subscriber can't.
  const uploadDroppedFiles = useCallback(
    (files: FileList) => {
      for (const file of Array.from(files)) {
        const reqId = Date.now() + Math.floor(Math.random() * 100000);
        const target =
          currentPath === "." ? file.name : `${currentPath}/${file.name}`;
        const unsub = subscribe((msg) => {
          if (msg.type !== "file_upload_result" || msg.id !== reqId) return;
          unsub();
          if (msg.ok) {
            setSaveToast(`uploaded ${file.name}`);
            send({ type: "file_list", path: currentPath, show_hidden: showHidden });
          } else {
            setSaveToast(
              `upload failed: ${file.name}${msg.error ? ` — ${msg.error}` : ""}`,
            );
          }
          setTimeout(() => setSaveToast(null), 3000);
        });
        fileToBase64(file)
          .then((data) =>
            send({ type: "file_upload", id: reqId, path: target, data }),
          )
          .catch(() => {
            unsub();
            setSaveToast(`upload failed: ${file.name}`);
            setTimeout(() => setSaveToast(null), 3000);
          });
      }
    },
    [currentPath, showHidden],
  );

  const onTreeDragOver = (e: React.DragEvent) => {
    // Only react to OS file drags, not internal element drags.
    if (!Array.from(e.dataTransfer.types).includes("Files")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
    if (!dragActive) setDragActive(true);
  };
  const onTreeDragLeave = (e: React.DragEvent) => {
    // Ignore leaves that just move onto a child element.
    if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
      setDragActive(false);
    }
  };
  const onTreeDrop = (e: React.DragEvent) => {
    const files = e.dataTransfer?.files;
    if (!files || files.length === 0) return;
    e.preventDefault();
    setDragActive(false);
    uploadDroppedFiles(files);
  };

  // Keep the KMS list fresh for the entry menu's "Add to KMS" action.
  // The backend broadcasts `kms_update` on any KMS change; request it once
  // on mount so the picker is populated before the first right-click.
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type !== "kms_update") return;
      const list = Array.isArray(msg.kmss) ? msg.kmss : [];
      setKmsList(
        list
          .map((k: { name?: unknown; scope?: unknown }) => ({
            name: String(k.name ?? ""),
            scope: String(k.scope ?? ""),
          }))
          .filter((k: { name: string }) => k.name),
      );
    });
    send({ type: "kms_list" });
    return unsub;
  }, []);

  // Close the flyout whenever the entry menu opens or closes, so a
  // stale panel never carries into the next right-click.
  useEffect(() => {
    setKmsFlyout(false);
    setKmsPickMode(null);
  }, [entryMenu]);

  // Ingest a .md file into the named KMS (Files-tab "Add to KMS"). Mirrors
  // the delete/rename request-id + one-shot subscription pattern. On an
  // alias collision (`force` false) the backend flags `collision`; we then
  // confirm and re-ingest with `force: true` to replace the existing entry.
  const addToKms = useCallback(
    (
      path: string,
      name: string,
      kmsName: string,
      force = false,
      mode: IngestMode = DEFAULT_INGEST_MODE,
    ) => {
      const reqId = Date.now() + Math.floor(Math.random() * 100000);
      const unsub = subscribe((msg) => {
        if (msg.type !== "kms_ingest_result" || msg.id !== reqId) return;
        unsub();
        if (msg.ok) {
          const imgs = Number(msg.images_copied ?? 0);
          const verb = msg.overwrote ? "updated in" : "added to";
          const tail = msg.atomic
            ? msg.research_error
              ? ` — atomic notes not started: ${String(msg.research_error)}`
              : " — building atomic notes (see Research sidebar)…"
            : " — summarizing…";
          setSaveToast(
            `${verb} ${kmsName}: ${String(msg.alias ?? name)}${
              imgs > 0 ? ` (+${imgs} image${imgs === 1 ? "" : "s"})` : ""
            }${tail}`,
          );
          setTimeout(() => setSaveToast(null), 4500);
          // Auto-summary: hand the backend-composed prompt to the main
          // agent as a normal chat turn so it upgrades the stub page into
          // a curated summary (KmsRead/KmsSearch/KmsWrite). Visible in chat.
          const prompt = String(msg.summarize_prompt ?? "");
          if (prompt) send({ type: "shell_input", text: prompt });
          return;
        }
        if (msg.collision) {
          // Recoverable: an entry with this alias already exists. Ask
          // before overwriting it (and its images), then force-ingest.
          void platformConfirm({
            title: "Replace in KMS?",
            message: `"${name}" already exists in ${kmsName}. Replace the existing entry?`,
            yesLabel: "Replace",
            noLabel: "Cancel",
          }).then((confirmed) => {
            if (confirmed) addToKms(path, name, kmsName, true, mode);
          });
          return;
        }
        setSaveToast(
          `add to KMS failed: ${name}${msg.error ? ` — ${msg.error}` : ""}`,
        );
        setTimeout(() => setSaveToast(null), 3500);
      });
      send({ type: "kms_ingest", id: reqId, path, kms: kmsName, force, mode });
    },
    [],
  );

  // Translate OR summarize a .md file into the chosen language, saved beside
  // the original as `<stem>-<code>.md` (translate) or `<stem>-<code>-sum.md`
  // (summarize). Like "Add to KMS", we hand an explicit prompt to the main
  // agent as a chat turn (it has Read + Write) rather than block the UI on a
  // bespoke IPC. We route to the purpose-built `translator` / `summarizer`
  // built-in subagents via `/agent <name>` (side channel): they carry
  // Read+Write+Edit with permissionMode:auto, understand `--language=<code>`,
  // and — crucially — run INDEPENDENT of the main session, so a translation
  // doesn't bloat (or block behind) the main agent's conversation. We pass an
  // explicit output path so the written file matches the modal's preview
  // regardless of each subagent's own default suffix. Progress surfaces on the
  // Chat tab's side-channel indicator; the new file lands in the tree.
  const submitLangAction = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      if (!langAction) return;
      const { mode, path, name } = langAction;
      const lang =
        TRANSLATE_LANGUAGES.find((l) => l.code === langCode) ??
        TRANSLATE_LANGUAGES[0];
      const slash = path.lastIndexOf("/");
      const dir = slash >= 0 ? path.slice(0, slash) : "";
      const stem = name.replace(LANG_ACTION_RE, "");
      const suffix = mode === "summarize" ? `-${lang.code}-sum` : `-${lang.code}`;
      const outPath = `${dir ? `${dir}/` : ""}${stem}${suffix}.md`;
      const prompt =
        mode === "summarize"
          ? `/agent summarizer --language=${lang.code} Summarize the file ` +
            `\`${path}\` and write the ${lang.label} summary to \`${outPath}\`. ` +
            `Do not modify the original.`
          : `/agent translator --language=${lang.code} Translate the file ` +
            `\`${path}\` into ${lang.label} and write the result to \`${outPath}\`. ` +
            `Do not modify the original.`;
      send({ type: "shell_input", text: prompt });
      setSaveToast(
        `${mode === "summarize" ? "summarizing" : "translating"} ${name} → ${lang.code}…`,
      );
      setTimeout(() => setSaveToast(null), 3500);
      setLangAction(null);
    },
    [langAction, langCode],
  );

  // Index a folder: write `<folder>/index.md` — one row per file with a
  // description read from its content. Same hand-off shape as translate /
  // summarize (a `/agent` side-channel prompt), routed to the built-in
  // `folder-indexer` subagent. The heavy lifting is the deterministic
  // `FolderIndex` tool: it fingerprints every file and only hands the agent
  // what actually changed since the last run, so re-indexing a settled folder
  // reads nothing. `force` bypasses those cached fingerprints.
  const submitIndexAction = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      if (!indexAction) return;
      const { path, name } = indexAction;
      const lang =
        TRANSLATE_LANGUAGES.find((l) => l.code === indexLang) ??
        TRANSLATE_LANGUAGES[0];
      const opts = [
        `recursive: ${indexScope !== "flat"}`,
        ...(indexScope === "one-level" ? ["report_depth: 1"] : []),
        ...(indexForce ? ["force: true"] : []),
      ].join(", ");
      const shape =
        indexScope === "one-level"
          ? " List only this folder's own files, with one summarizing row per " +
            "immediate subfolder (scan everything underneath to write those rollups)."
          : "";
      const prompt =
        `/agent folder-indexer --language=${lang.code} Index the folder ` +
        `\`${path}\` into \`${path}/index.md\`, writing the summaries in ` +
        `${lang.label}.${shape} Call FolderIndex with { path: "${path}", ${opts} } ` +
        `on every call, and loop plan → read → commit until nothing is pending.`;
      send({ type: "shell_input", text: prompt });
      setSaveToast(`indexing ${name}…`);
      setTimeout(() => setSaveToast(null), 3500);
      setIndexAction(null);
    },
    [indexAction, indexLang, indexScope, indexForce],
  );

  // Delete a file or folder from the tree context menu. Confirms first
  // with the native dialog, then `file_delete` (sandbox-checked; recursive
  // for folders). On success the listing refreshes and the preview clears
  // if the deleted path was the one being viewed.
  const deleteEntry = useCallback(
    async (path: string, name: string, isDir: boolean) => {
      const confirmed = await platformConfirm({
        title: `Delete ${isDir ? "folder" : "file"}?`,
        message: isDir
          ? `"${name}" and everything inside it will be permanently deleted.`
          : `"${name}" will be permanently deleted.`,
        yesLabel: "Delete",
        noLabel: "Cancel",
      });
      if (!confirmed) return;
      const reqId = Date.now() + Math.floor(Math.random() * 100000);
      const unsub = subscribe((msg) => {
        if (msg.type !== "file_delete_result" || msg.id !== reqId) return;
        unsub();
        if (msg.ok) {
          setSaveToast(`deleted ${name}`);
          // Clear the preview if it pointed at the deleted path (or, for a
          // folder, anything inside it).
          if (
            preview &&
            (preview.path === path || preview.path.startsWith(`${path}/`))
          ) {
            setPreview(null);
            requestedPathRef.current = "";
          }
          send({ type: "file_list", path: currentPath, show_hidden: showHidden });
        } else {
          setSaveToast(
            `delete failed: ${name}${msg.error ? ` — ${msg.error}` : ""}`,
          );
        }
        setTimeout(() => setSaveToast(null), 3000);
      });
      send({ type: "file_delete", id: reqId, path });
    },
    [currentPath, showHidden, preview],
  );

  // One-shot file download — sends `file_download` with a unique
  // request id, waits for the matching `file_download_result`, then
  // converts the base64 payload into a Blob and triggers a browser
  // `<a download>` click. The subscriber unhooks itself once the
  // matching reply lands so we don't leak handlers across clicks.
  // "Convert to…": the backend replies `needs_confirm` first unless the
  // install's document service is the organisation's own (EE), so the user
  // sees where the file is going before it is uploaded.
  const convertDoc = useCallback(
    (path: string, name: string, to: string, confirmed = false) => {
      const unsub = subscribe((msg) => {
        if (
          msg.type !== "doc_convert_result" ||
          msg.path !== path ||
          msg.to !== to
        ) {
          return;
        }
        unsub();
        if (msg.needs_confirm) {
          setSaveToast(null);
          void platformConfirm({
            title: `Convert ${name} to ${to.toUpperCase()}?`,
            message:
              `The file is uploaded to ${String(msg.destination ?? "the document service")} ` +
              `for conversion; the result is saved beside it.`,
            yesLabel: "Upload and convert",
          }).then((yes) => {
            if (yes) convertDoc(path, name, to, true);
          });
          return;
        }
        if (msg.ok) {
          setSaveToast(`wrote ${String(msg.out ?? "").split(/[\\/]/).pop()}`);
          send({ type: "file_list", path: currentPath, show_hidden: showHidden });
        } else {
          setSaveToast(`convert failed: ${msg.error ?? "unknown error"}`);
        }
        setTimeout(() => setSaveToast(null), 4000);
      });
      setSaveToast(`converting ${name} to ${to.toUpperCase()}…`);
      send({ type: "doc_convert", path, to, confirmed });
    },
    [currentPath, showHidden],
  );

  const downloadFile = useCallback((path: string) => {
    const reqId = Date.now() + Math.floor(Math.random() * 1000);
    const unsub = subscribe((msg) => {
      if (msg.type !== "file_download_result" || msg.id !== reqId) return;
      unsub();
      if (!msg.ok) {
        // Surface as a toast rather than a modal — same channel as
        // save errors.
        setSaveToast(`download failed: ${(msg.error as string) || "unknown"}`);
        setTimeout(() => setSaveToast(null), 4000);
        return;
      }
      const b64 = msg.content as string;
      const mime = (msg.mime as string) || "application/octet-stream";
      const filename = (msg.filename as string) || "download";
      try {
        const bin = atob(b64);
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        const blob = new Blob([bytes], { type: mime });
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = filename;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        // Revoke after a short delay so Safari doesn't drop the
        // download mid-flight.
        setTimeout(() => URL.revokeObjectURL(url), 1500);
      } catch (e) {
        setSaveToast(`download decode failed: ${(e as Error).message}`);
        setTimeout(() => setSaveToast(null), 4000);
      }
    });
    send({ type: "file_download", id: reqId, path });
  }, []);

  const navigate = (name: string) => {
    const path = currentPath === "." ? name : `${currentPath}/${name}`;
    send({ type: "file_list", path, show_hidden: showHidden });
  };

  const goUp = () => {
    const parent = currentPath.includes("/")
      ? currentPath.substring(0, currentPath.lastIndexOf("/"))
      : ".";
    send({ type: "file_list", path: parent || ".", show_hidden: showHidden });
  };

  // Open the new file / folder modal, creating in the current directory.
  const startCreate = (kind: "file" | "folder") => {
    setExplorerMenu(null);
    setCreateName("");
    setCreateError(null);
    setCreateKind(kind);
  };

  const submitCreate = (e: React.FormEvent) => {
    e.preventDefault();
    if (creating || createKind === null) return;
    const n = createName.trim();
    if (!n) return setCreateError("name required");
    if (n.includes("/")) return setCreateError("name can't contain '/'");
    const path = currentPath === "." ? n : `${currentPath}/${n}`;
    setCreating(true);
    setCreateError(null);
    send({ type: createKind === "folder" ? "file_mkdir" : "file_create", path });
  };

  // Resolve the create round-trip; refresh the listing on success.
  useEffect(() => {
    if (createKind === null) return;
    const wanted =
      createKind === "folder" ? "file_mkdir_result" : "file_create_result";
    const unsub = subscribe((msg) => {
      if (msg.type !== wanted) return;
      setCreating(false);
      if (msg.ok) {
        setCreateKind(null);
        setCreateName("");
        send({ type: "file_list", path: currentPath, show_hidden: showHidden });
      } else {
        setCreateError((msg.error as string) ?? "create failed");
      }
    });
    return unsub;
  }, [createKind, currentPath]);

  // Open the rename modal for a tree entry, prefilled with its name.
  const startRename = (path: string, name: string, isDir: boolean) => {
    setEntryMenu(null);
    setRenameName(name);
    setRenameError(null);
    setRenameTarget({ path, name, isDir });
  };

  // Submit a rename: same parent directory, new basename. Defined as a
  // plain function (not memoized) so it always closes over the current
  // path / preview. Uses a per-request listener keyed by `id`.
  const submitRename = (e: React.FormEvent) => {
    e.preventDefault();
    if (renaming || !renameTarget) return;
    const n = renameName.trim();
    if (!n) return setRenameError("name required");
    if (n.includes("/")) return setRenameError("name can't contain '/'");
    if (n === renameTarget.name) return setRenameTarget(null);
    const target = renameTarget;
    const slash = target.path.lastIndexOf("/");
    const dir = slash >= 0 ? target.path.slice(0, slash) : "";
    const newPath = dir ? `${dir}/${n}` : n;
    const reqId = Date.now() + Math.floor(Math.random() * 100000);
    setRenaming(true);
    setRenameError(null);
    const unsub = subscribe((msg) => {
      if (msg.type !== "file_rename_result" || msg.id !== reqId) return;
      unsub();
      setRenaming(false);
      if (msg.ok) {
        // Clear the preview if it pointed at the renamed path (or inside a
        // renamed folder) — its old path no longer exists.
        if (
          preview &&
          (preview.path === target.path ||
            preview.path.startsWith(`${target.path}/`))
        ) {
          setPreview(null);
            requestedPathRef.current = "";
        }
        setRenameTarget(null);
        setRenameName("");
        setSaveToast(`renamed to ${n}`);
        setTimeout(() => setSaveToast(null), 2500);
        send({ type: "file_list", path: currentPath, show_hidden: showHidden });
      } else {
        setRenameError((msg.error as string) ?? "rename failed");
      }
    });
    send({ type: "file_rename", id: reqId, from: target.path, to: newPath });
  };

  const openFile = useCallback((path: string) => {
    setMode("preview");
    requestedPathRef.current = path;
    send({ type: "file_read", path, mode: "preview", theme: themeMode });
  }, [themeMode]);

  const onSidebarClick = async (name: string) => {
    const path = currentPath === "." ? name : `${currentPath}/${name}`;
    if (mode === "edit" && editorDirty) {
      const ok = await platformConfirm({
        title: "Unsaved changes",
        message: `You have unsaved edits to ${preview?.path ?? "this file"}. Discard them and open the new file?`,
        yesLabel: "Discard",
        noLabel: "Cancel",
      });
      if (!ok) return;
      setEditorDirty(false);
    }
    openFile(path);
  };

  // Open a file the user clicked *inside* a rendered markdown preview (an
  // `index.md` row, a relative link between chapters). Same unsaved-edit
  // guard as clicking the tree, plus it walks the tree to the file's folder
  // so the sidebar shows where you landed.
  const openLinkedFile = useCallback(
    async (path: string) => {
      if (mode === "edit" && editorDirty) {
        const ok = await platformConfirm({
          title: "Unsaved changes",
          message: `You have unsaved edits to ${preview?.path ?? "this file"}. Discard them and open the linked file?`,
          yesLabel: "Discard",
          noLabel: "Cancel",
        });
        if (!ok) return;
        setEditorDirty(false);
      }
      const slash = path.lastIndexOf("/");
      const dir = slash > 0 ? path.slice(0, slash) : ".";
      if (dir !== currentPath) {
        send({ type: "file_list", path: dir, show_hidden: showHidden });
      }
      openFile(path);
    },
    [mode, editorDirty, preview, currentPath, showHidden, openFile],
  );

  // The click handler is attached to the *iframe's* document from out here,
  // so it must not close over stale state: the iframe is bound once per
  // load, while these change with the tab's state.
  const openLinkedFileRef = useRef(openLinkedFile);
  const showHiddenRef = useRef(showHidden);
  const previewPathRef = useRef("");
  // The markdown preview frame, so the postMessage bridge below can check
  // that a link message really came from it.
  const previewFrameRef = useRef<HTMLIFrameElement | null>(null);
  // The HTML preview frame — the only sender the dashboard bridge trusts.
  const htmlFrameRef = useRef<HTMLIFrameElement | null>(null);
  useEffect(() => {
    openLinkedFileRef.current = openLinkedFile;
    showHiddenRef.current = showHidden;
    previewPathRef.current = preview?.path ?? "";
  }, [openLinkedFile, showHidden, preview?.path]);

  // Act on a link clicked inside a rendered markdown preview: a workspace
  // file opens in this tab (that's what makes `index.md`'s file rows
  // clickable), a folder link walks the tree, http(s) goes to the OS
  // browser. Shared by both delivery paths below — the href is whatever the
  // markdown actually said, already stripped of any `#anchor` handling.
  const followPreviewLink = useCallback((raw: string) => {
    if (/^[a-z][a-z0-9+.-]*:/i.test(raw)) {
      if (/^https?:/i.test(raw)) {
        // The wry webview is single-document — navigating it would blow
        // away the app. Hand external links to the OS browser.
        send({ type: "open_external", url: raw });
      }
      // mailto:, custom schemes: ignored.
      return;
    }
    // Resolve against the previewed file's own directory rather than reading
    // the anchor's resolved URL: the injected <base> maps an absolute
    // workspace path to `/file-asset/private/…`, and parsing that back loses
    // the leading slash (the backend then reads a nonexistent relative path).
    // Path arithmetic on what the markdown said can't get that wrong.
    const target = resolveWorkspaceLink(previewPathRef.current, raw);
    if (!target) return;
    // A trailing slash means a folder (index.md's section headings link one)
    // — walk the tree there instead of asking the backend to read a
    // directory as a file.
    if (target.isDir) {
      send({
        type: "file_list",
        path: target.path || ".",
        show_hidden: showHiddenRef.current,
      });
      return;
    }
    void openLinkedFileRef.current(target.path);
  }, []);
  const followPreviewLinkRef = useRef(followPreviewLink);
  followPreviewLinkRef.current = followPreviewLink;

  // Delivery path 1 — `--serve` (browser). The page and the `srcdoc` frame
  // share an http(s) origin, so the frame can keep `allow-same-origin` (its
  // <img> subresources must carry the session cookie or the cloud
  // ForwardAuth gate 302s them to login) and the parent reaches into
  // `contentDocument` to handle clicks. No `allow-scripts`: nothing runs
  // inside the frame.
  //
  // Left alone, every link resolves through the injected `<base>` to a
  // `/file-asset/` URL and the frame navigates to the raw bytes, replacing
  // the preview with a naked file and no way back.
  const bindPreviewLinks = useCallback(
    (e: React.SyntheticEvent<HTMLIFrameElement>) => {
      let doc: Document | null = null;
      try {
        doc = e.currentTarget.contentDocument;
      } catch {
        // Cross-origin (shouldn't happen with allow-same-origin): leave the
        // frame's default link behaviour alone rather than break.
        return;
      }
      if (!doc) return;
      const marker = doc as Document & { __thclawsLinksBound?: boolean };
      if (marker.__thclawsLinksBound) return;
      marker.__thclawsLinksBound = true;
      doc.addEventListener("click", (ev) => {
        const anchor = (ev.target as HTMLElement | null)?.closest?.(
          "a",
        ) as HTMLAnchorElement | null;
        const raw = anchor?.getAttribute("href");
        if (!anchor || !raw) return;
        // In-document anchor: the injected <base> would send this to a
        // different URL, so scroll to the target ourselves.
        if (raw.startsWith("#")) {
          ev.preventDefault();
          const id = raw.slice(1);
          const target =
            doc.getElementById(id) ??
            doc.getElementById(decodeURIComponent(id)) ??
            doc.querySelector(`a[name="${CSS.escape(decodeURIComponent(id))}"]`);
          target?.scrollIntoView({ behavior: "smooth", block: "start" });
          return;
        }
        ev.preventDefault();
        followPreviewLinkRef.current(raw);
      });
    },
    [],
  );

  // Delivery path 2 — desktop GUI. The app is served from a custom scheme
  // (`thclaws://localhost`), where a `srcdoc` frame does NOT hand the parent
  // access to its document, so path 1 silently binds nothing and clicks go
  // nowhere. There the frame is sandboxed with `allow-scripts` instead
  // (deliberately WITHOUT `allow-same-origin`, so it stays opaque and can't
  // reach back into the app), and the bridge script the backend appends to
  // every rendered preview reports the click by `postMessage` — which works
  // regardless of origin. Desktop assets come from the custom protocol and
  // need no cookie, so nothing is lost by dropping same-origin there.
  useEffect(() => {
    const handler = (e: MessageEvent) => {
      const d = e.data as { type?: string; href?: string } | null;
      if (!d || d.type !== "thclaws-preview-link" || typeof d.href !== "string")
        return;
      // Only the preview frame may drive the pane — an MCP-App widget or a
      // GUI Shell posting this shape must not move the user's files around.
      if (e.source !== previewFrameRef.current?.contentWindow) return;
      followPreviewLinkRef.current(d.href);
    };
    window.addEventListener("message", handler);
    return () => window.removeEventListener("message", handler);
  }, []);

  const closePreview = async () => {
    setRenderPending(null);
    if (mode === "edit" && editorDirty) {
      const ok = await platformConfirm({
        title: "Discard unsaved changes",
        message: `Discard unsaved edits to ${preview?.path ?? "this file"} and close?`,
        yesLabel: "Discard",
        noLabel: "Keep editing",
      });
      if (!ok) return;
    }
    setPreview(null);
            requestedPathRef.current = "";
    setMode("preview");
    setEditorDirty(false);
  };

  const enterEditMode = () => {
    if (!preview) return;
    setMode("edit");
    send({ type: "file_read", path: preview.path, mode: "source" });
  };

  /// Refresh the current preview — re-fetches content from disk via
  /// the backend AND forces the preview iframe (when applicable) to
  /// re-mount so it re-fetches its asset URL. Needed because:
  ///   1. iframe content is browser-cached by URL; when an agent
  ///      regenerates a file on disk, the iframe still shows the old
  ///      content until it remounts.
  ///   2. The send() re-read alone updates preview.content (used for
  ///      .md and code-mirror previews), but iframe-rendered HTML
  ///      uses src={assetUrl(path)} not srcDoc={content}, so it
  ///      doesn't notice the state change without a key bump.
  const refreshPreview = async () => {
    if (!preview) return;
    if (editorDirty) {
      // `platformConfirm`, not `window.confirm`: the desktop webview has no
      // browser dialog, so the bare call answered falsy and Refresh silently
      // did nothing whenever the editor was dirty — the one case this guard
      // exists for.
      const ok = await platformConfirm({
        title: "Unsaved changes",
        message:
          "You have unsaved changes in the editor. Refresh anyway? Unsaved edits will be lost.",
        yesLabel: "Refresh",
      });
      if (!ok) return;
      setEditorDirty(false);
    }
    setPreviewVersion((v) => v + 1);
    send({
      type: "file_read",
      path: preview.path,
      mode: mode === "preview" ? "preview" : "source",
      theme: themeMode,
    });
  };

  const exitEditMode = async () => {
    // If there are unsaved edits, surface a native OS confirm so the
    // user can abort a misclick. When the editor is already clean
    // ("Preview" button label), skip the prompt and go straight back.
    if (editorDirty) {
      const ok = await platformConfirm({
        title: "Discard unsaved changes",
        message: `Discard unsaved edits to ${preview?.path ?? "this file"} and return to preview?`,
        yesLabel: "Discard",
        noLabel: "Keep editing",
      });
      if (!ok) return;
    }
    setMode("preview");
    setEditorDirty(false);
    setEditorSource("");
    if (preview) {
      send({ type: "file_read", path: preview.path, mode: "preview", theme: themeMode });
    }
  };

  const save = useCallback(() => {
    if (!preview || !editorDirty) return;
    send({ type: "file_write", path: preview.path, content: editorSource });
  }, [preview, editorDirty, editorSource]);

  // ── thClaws → dashboard host bridge ─────────────────────────────
  //
  // Lets self-contained HTML dashboards (e.g. the productivity
  // plugin's dashboard.html, opened in an iframe via this Files
  // tab) save AND load sibling files via thClaws's IPC — without
  // ever prompting the user for a File System Access API permission
  // and without depending on agent-regenerated stale snapshots.
  //
  // Two message types from the iframe:
  //   - thclaws-dashboard-save  {filename, content}  →  file_write IPC
  //   - thclaws-dashboard-load  {filename}           →  file_read IPC
  // Each pairs with a *-ack response back to the same iframe.
  //
  // Only the HTML preview frame this pane opened may ask, and only for a
  // plain file beside it: any other frame (an MCP widget, a GUI shell, a
  // page in the lightbox) can post this shape too, and a `filename` with a
  // path in it reached every workspace file, `.thclaws/` and `.env`
  // included.
  //
  // The load path correlates async file_read responses to requesting
  // iframes via a single-slot pendingDashboardLoad ref. Concurrent
  // requests overwrite (rare in practice — one dashboard, one read).
  const pendingDashboardLoad = useRef<{
    source: Window;
    reqId: string;
    targetPath: string;
  } | null>(null);

  useEffect(() => {
    const handler = (e: MessageEvent) => {
      const d = e.data as
        | {
            type?: string;
            reqId?: string;
            filename?: string;
            content?: string;
          }
        | undefined;
      if (!d || !preview) return;
      if (d.type !== "thclaws-dashboard-save" && d.type !== "thclaws-dashboard-load") return;
      if (!e.source || e.source !== htmlFrameRef.current?.contentWindow) return;

      // Resolve `filename` (e.g. "TASKS.md") against the directory of
      // the currently-previewed file. So opening
      // `/proj/business-cards/dashboard.html` and asking for
      // "TASKS.md" hits `/proj/business-cards/TASKS.md`.
      const slash = preview.path.lastIndexOf("/");
      const dir = slash > 0 ? preview.path.slice(0, slash) : ".";
      const filename = d.filename || "TASKS.md";
      if (!isDashboardFile(dir, filename)) {
        (e.source as Window).postMessage(
          {
            type:
              d.type === "thclaws-dashboard-save"
                ? "thclaws-dashboard-save-ack"
                : "thclaws-dashboard-load-ack",
            reqId: d.reqId,
            ok: false,
            error: "not a file this dashboard may use",
          },
          "*",
        );
        return;
      }
      const targetPath = `${dir}/${filename}`;

      if (d.type === "thclaws-dashboard-save") {
        try {
          send({
            type: "file_write",
            path: targetPath,
            content: d.content || "",
          });
          if (e.source && "postMessage" in e.source) {
            (e.source as Window).postMessage(
              {
                type: "thclaws-dashboard-save-ack",
                reqId: d.reqId,
                ok: true,
              },
              "*",
            );
          }
        } catch (err) {
          if (e.source && "postMessage" in e.source) {
            (e.source as Window).postMessage(
              {
                type: "thclaws-dashboard-save-ack",
                reqId: d.reqId,
                ok: false,
                error: String(err),
              },
              "*",
            );
          }
        }
      } else if (d.type === "thclaws-dashboard-load") {
        if (!e.source || !("postMessage" in e.source) || !d.reqId) return;
        // Stash the requesting iframe + reqId so the file_content
        // subscriber below can route the response back. Single-slot
        // — concurrent requests overwrite (rare in practice).
        pendingDashboardLoad.current = {
          source: e.source as Window,
          reqId: d.reqId,
          targetPath,
        };
        send({ type: "file_read", path: targetPath, mode: "source" });
      }
    };
    window.addEventListener("message", handler);
    return () => window.removeEventListener("message", handler);
  }, [preview]);

  // Global Cmd/Ctrl-S when Files tab is active + in edit mode.
  useEffect(() => {
    if (!active || mode !== "edit") return;
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key.toLowerCase() === "s") {
        e.preventDefault();
        save();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [active, mode, save]);

  // `beforeunload` in wry WebViews is a best-effort warning; if the
  // native host ignores it, at least we're not losing data silently
  // because the Discard button and "save or discard first" toast
  // already guard the in-app flow.
  useEffect(() => {
    if (!editorDirty) return;
    const handler = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", handler);
    return () => window.removeEventListener("beforeunload", handler);
  }, [editorDirty]);

  const previewPath = preview?.path;
  useEffect(() => {
    setDeckAsPdf(false);
  }, [previewPath]);

  const isHtml = preview?.mime === "text/html";
  const isImage = preview?.mime.startsWith("image/");
  const isPdf = preview?.mime === "application/pdf";
  const isEpub = preview?.mime === "application/epub+zip";
  const isAudio = !!preview?.mime.startsWith("audio/");
  const isVideo = !!preview?.mime.startsWith("video/");
  const canEdit = preview && isTextEditable(preview.path);
  const hasSyntaxPreview =
    preview && SYNTAX_PREVIEW.has(extOf(preview.path));


  return (
    <div
      className="relative flex flex-col sm:flex-row h-full"
      style={{ background: "var(--bg-primary)" }}
    >
      {publishNotice && (
        <div
          className="absolute bottom-3 right-3 z-20 text-[11px] px-3 py-1.5 rounded shadow"
          style={{
            background: "var(--bg-secondary)",
            color: "var(--text-primary)",
            border: "1px solid var(--border)",
          }}
        >
          {publishNotice}
        </div>
      )}
      {/* Tree panel — full-width strip on top below `sm` (capped height so
          the editor stays visible), fixed-width left column at `sm:`+. */}
      <div
        className="w-full sm:w-64 max-sm:max-h-[38%] overflow-y-auto border-b sm:border-b-0 sm:border-r shrink-0 flex flex-col"
        style={{
          borderColor: dragActive ? "var(--accent)" : "var(--border)",
          ...(dragActive
            ? {
                outline: "2px dashed var(--accent)",
                outlineOffset: "-2px",
                background: "rgba(13,148,136,0.08)",
              }
            : {}),
        }}
        onDragOver={onTreeDragOver}
        onDragLeave={onTreeDragLeave}
        onDrop={onTreeDrop}
      >
        <div
          className="flex items-center gap-1 px-2 py-1.5 border-b text-[10px] font-mono shrink-0"
          style={{
            background: "var(--bg-secondary)",
            borderColor: "var(--border)",
            color: "var(--text-secondary)",
          }}
        >
          <button
            onClick={goUp}
            className="p-0.5 rounded hover:bg-white/10"
            title="Go up"
          >
            <ArrowUp size={12} />
          </button>
          <span className="truncate flex-1" title={currentPath}>
            {shortPath(currentPath)}
          </span>
          <button
            onClick={() => setShowHidden((v) => !v)}
            className="p-0.5 rounded hover:bg-white/10 shrink-0"
            title={
              showHidden
                ? "Hide dotfiles (.thclaws, .claude, .env, …)"
                : "Show dotfiles (.thclaws, .claude, .env, …)"
            }
            style={{ color: showHidden ? "var(--accent)" : "var(--text-secondary)" }}
          >
            {showHidden ? <Eye size={12} /> : <EyeOff size={12} />}
          </button>
          <button
            onClick={() => startCreate("file")}
            className="p-0.5 rounded hover:bg-white/10 shrink-0"
            title={`New file in ${currentPath}`}
          >
            <FilePlus size={12} />
          </button>
          <button
            onClick={() => startCreate("folder")}
            className="p-0.5 rounded hover:bg-white/10 shrink-0"
            title={`New folder in ${currentPath}`}
          >
            <FolderPlus size={12} />
          </button>
        </div>

        <div
          className="overflow-y-auto flex-1 p-1"
          onContextMenu={(e) => {
            e.preventDefault();
            setExplorerMenu({ x: e.clientX, y: e.clientY });
          }}
        >
          {dragActive && (
            <div
              className="text-[10px] font-mono px-2 py-1 mb-1 rounded text-center"
              style={{ background: "var(--accent)", color: "#fff" }}
            >
              Drop to upload to {shortPath(currentPath)}
            </div>
          )}
          {entries.length === 0 ? (
            <div className="text-xs p-2" style={{ color: "var(--text-secondary)" }}>
              Empty directory
            </div>
          ) : (
            entries.map((entry) => {
              // Same path composition as openFile() so the comparison
              // matches whatever the preview pane currently points at.
              // Directories never get the selection mark — they
              // navigate instead of preview, so highlighting one
              // would lie about which file is open.
              const entryPath =
                currentPath === "." ? entry.name : `${currentPath}/${entry.name}`;
              const isSelected = !entry.is_dir && preview?.path === entryPath;
              return (
                <button
                  key={entry.name}
                  aria-current={isSelected ? "true" : undefined}
                  className="flex items-center gap-1.5 w-full px-2 py-1 rounded text-xs text-left"
                  style={{
                    // Text + icon colour stay theme-default for both
                    // states — readability shouldn't depend on theme
                    // contrast against an accent fill.
                    color: "var(--text-primary)",
                    // Selection mark is just a faint background fill
                    // — same shape hover uses, slightly darker. Drops
                    // the left bar / accent text / bold from the
                    // previous pass; that combo read as a "click me"
                    // CTA rather than a passive indicator.
                    background: isSelected
                      ? "var(--bg-tertiary, rgba(255,255,255,0.06))"
                      : undefined,
                    paddingLeft: 8,
                  }}
                  onMouseEnter={(e) => {
                    if (!isSelected) e.currentTarget.style.background = "rgba(255,255,255,0.05)";
                  }}
                  onMouseLeave={(e) => {
                    if (!isSelected) e.currentTarget.style.background = "";
                  }}
                  onClick={() =>
                    entry.is_dir ? navigate(entry.name) : onSidebarClick(entry.name)
                  }
                  onContextMenu={(e) => {
                    // Per-entry menu — close the explorer-level
                    // "new file/folder" one if it was somehow open.
                    e.preventDefault();
                    e.stopPropagation();
                    setExplorerMenu(null);
                    setEntryMenu({
                      x: e.clientX,
                      y: e.clientY,
                      path: entryPath,
                      name: entry.name,
                      isDir: entry.is_dir,
                    });
                  }}
                >
                  {entry.is_dir ? (
                    <Folder size={13} style={{ color: "var(--accent)", flexShrink: 0 }} />
                  ) : (
                    <File size={13} style={{ color: "var(--text-secondary)", flexShrink: 0 }} />
                  )}
                  <span className="truncate">{entry.name}</span>
                </button>
              );
            })
          )}
        </div>
      </div>

      {/* Preview / editor panel */}
      <div className="flex-1 min-w-0 min-h-0 flex flex-col p-4">
        {renderPending ? (
          // A .pptx is being converted by the slide-render service. This
          // sits ABOVE the `preview` check on purpose: on a first open
          // there is no preview for this file yet, so a placeholder
          // gated on `preview.path` would never appear and the pane
          // would just sit there for ~13s looking hung.
          <div className="flex flex-col flex-1 min-w-0 min-h-0">
            <div
              className="text-xs font-mono truncate mb-3 shrink-0"
              style={{ color: "var(--text-secondary)" }}
            >
              {renderPending}
            </div>
            <div
              className="flex-1 min-h-0 flex flex-col items-center justify-center gap-2 text-xs"
              style={{ color: "var(--text-secondary)" }}
            >
              <div className="animate-pulse">Rendering preview…</div>
              <div className="opacity-70">
                Converting this file to PDF · cached after the first time
              </div>
            </div>
          </div>
        ) : preview ? (
          <div className="flex flex-col flex-1 min-w-0 min-h-0">
            <div className="flex items-center justify-between mb-3 shrink-0 gap-2">
              <div
                className="text-xs font-mono truncate min-w-0 flex-1"
                style={{ color: "var(--text-secondary)" }}
              >
                {preview.path}
                {editorDirty && (
                  <span style={{ color: "var(--accent)" }} title="unsaved changes">
                    {" "}●
                  </span>
                )}
              </div>
              <div className="flex items-center gap-1.5 shrink-0">
                {saveToast && (
                  <span
                    className="text-[10px] font-mono px-2 py-0.5 rounded"
                    style={{
                      background: saveToast.startsWith("save failed")
                        ? "rgba(220,80,80,0.15)"
                        : "rgba(100,180,100,0.15)",
                      color: saveToast.startsWith("save failed")
                        ? "#e06060"
                        : "#6fbf6f",
                    }}
                  >
                    {saveToast}
                  </span>
                )}
                <button
                  onClick={refreshPreview}
                  className="flex items-center gap-1 text-[11px] px-2 py-1 rounded hover:bg-white/5"
                  style={{ color: "var(--text-primary)" }}
                  title="Re-read this file from disk and re-render the preview"
                >
                  Refresh
                </button>
                {canEdit && mode === "preview" && (
                  <button
                    onClick={enterEditMode}
                    className="flex items-center gap-1 text-[11px] px-2 py-1 rounded hover:bg-white/5"
                    style={{ color: "var(--text-primary)" }}
                    title="Edit this file"
                  >
                    <Pencil size={12} />
                    Edit
                  </button>
                )}
                {mode === "edit" && isMarkdownPath(preview.path) && (
                  <button
                    onClick={() => setMdSourceEdit((v) => !v)}
                    className="flex items-center gap-1 text-[11px] px-2 py-1 rounded hover:bg-white/5"
                    style={{ color: "var(--text-secondary)" }}
                    title={
                      mdSourceEdit
                        ? "Switch to the rich-text editor (reformats markdown on save)"
                        : "Edit the raw markdown — leaves formatting exactly as written"
                    }
                  >
                    {mdSourceEdit ? <Type size={12} /> : <Code2 size={12} />}
                    {mdSourceEdit ? "Rich text" : "Source"}
                  </button>
                )}
                {mode === "edit" && (
                  <>
                    <button
                      onClick={save}
                      disabled={!editorDirty}
                      className="flex items-center gap-1 text-[11px] px-2 py-1 rounded hover:bg-white/5 disabled:opacity-40 disabled:cursor-not-allowed"
                      style={{ color: "var(--accent)" }}
                      title="Save (Cmd/Ctrl-S)"
                    >
                      <Save size={12} />
                      Save
                    </button>
                    <button
                      onClick={exitEditMode}
                      className="flex items-center gap-1 text-[11px] px-2 py-1 rounded hover:bg-white/5"
                      style={{ color: "var(--text-secondary)" }}
                      title="Back to preview"
                    >
                      {editorDirty ? <X size={12} /> : <Eye size={12} />}
                      {editorDirty ? "Discard" : "Preview"}
                    </button>
                  </>
                )}
                <button
                  onClick={closePreview}
                  className="flex items-center justify-center p-1 rounded hover:bg-white/5"
                  style={{ color: "var(--text-secondary)" }}
                  title="Close file"
                >
                  <X size={13} />
                </button>
              </div>
            </div>

            {/* Body: preview or editor */}
            {mode === "edit" ? (
              isMarkdownPath(preview.path) && !mdSourceEdit ? (
                <MarkdownEditor
                  source={editorSource}
                  baseDir={parentDir(preview.path)}
                  onChange={(md) => {
                    setEditorSource(md);
                    setEditorDirty(true);
                  }}
                />
              ) : (
                <CodeEditor
                  source={editorSource}
                  path={preview.path}
                  onChange={(text) => {
                    setEditorSource(text);
                    setEditorDirty(true);
                  }}
                  onSave={save}
                />
              )
            ) : isImage ? (
              <div className="flex-1 min-h-0 overflow-auto">
                <img
                  src={`data:${preview.mime};base64,${preview.content}`}
                  alt={preview.path}
                  className="max-w-full rounded"
                />
              </div>
            ) : isPdf ? (
              preview.slides && preview.slides.length > 0 && !deckAsPdf ? (
                // A converted `.pptx`: page through the per-slide PNGs
                // the render already produced. The PDF viewer would
                // show the same deck as one continuous scroll, which
                // reads like an export, not a presentation.
                <SlideDeckViewer
                  key={`deck-${preview.path}-${previewVersion}`}
                  slides={preview.slides}
                  version={previewVersion}
                  label={preview.path.split("/").pop()}
                  onShowPdf={() => setDeckAsPdf(true)}
                />
              ) : (
                // Stream off /file-asset/ like audio/video — Chrome
                // refuses to run its PDF viewer inside a data: iframe
                // (opaque origin ⇒ implicitly sandboxed; the viewer's
                // internal about:srcdoc frame logs "Blocked script
                // execution… 'allow-scripts'"), and a book PDF would
                // round-trip 5MB+ of base64 through the WS anyway.
                <div className="flex-1 min-h-0 flex flex-col">
                  <iframe
                    key={`pdf-${preview.renderPath ?? preview.path}-${previewVersion}`}
                    src={assetUrl(preview.renderPath ?? preview.path)}
                    className="w-full flex-1 min-h-0 rounded border"
                    style={{
                      borderColor: "var(--border)",
                      background: "#fff",
                    }}
                    title={preview.path}
                  />
                  {preview.slides && preview.slides.length > 0 && (
                    <div className="flex items-center justify-end px-2 py-1 shrink-0">
                      <button
                        onClick={() => setDeckAsPdf(false)}
                        className="px-2 py-0.5 rounded text-[11px] font-mono hover:bg-white/10"
                        style={{ color: "var(--text-secondary)" }}
                        title="Back to one slide at a time"
                      >
                        ‹ Slides
                      </button>
                    </div>
                  )}
                </div>
              )
            ) : isEpub ? (
              // EPUB renders via epub.js (see EpubViewer): it fetches
              // the bytes off /file-asset and unzips client-side, since
              // no browser renders EPUB natively the way it does PDF.
              <EpubViewer
                key={`epub-${preview.path}-${previewVersion}`}
                url={assetUrl(preview.path)}
                theme={themeMode}
              />
            ) : isAudio ? (
              // Audio + video stream off /file-asset/, not a base64
              // data URI — keeps a 50 MB clip from round-tripping
              // through IPC. `key` forces a fresh element when the
              // selected file changes so the player resets cleanly.
              <div className="flex-1 min-h-0 flex flex-col items-center justify-center gap-4 p-6">
                <audio
                  key={`audio-${preview.path}`}
                  src={assetUrl(preview.path)}
                  controls
                  preload="metadata"
                  className="w-full max-w-2xl"
                />
                <div
                  className="text-xs font-mono"
                  style={{ color: "var(--text-muted)" }}
                >
                  {preview.path.split("/").pop()} · {preview.mime}
                </div>
              </div>
            ) : isVideo ? (
              <div className="flex-1 min-h-0 overflow-auto flex items-center justify-center p-2">
                <video
                  key={`video-${preview.path}`}
                  src={assetUrl(preview.path)}
                  controls
                  preload="metadata"
                  className="max-w-full max-h-full rounded"
                  style={{ background: "#000" }}
                />
              </div>
            ) : isHtml ? (
              isMarkdownPath(preview.path) ? (
                // Markdown preview: backend renders MD → HTML and
                // returns it in `content`. Use `srcDoc` so the iframe
                // shows that HTML directly; `src={assetUrl}` would
                // fetch the raw .md via the custom protocol and the
                // iframe would end up blank. `injectBaseHref` rewrites
                // the document so relative `![alt](img/foo.png)` refs
                // resolve via /file-asset/ instead of failing against
                // srcDoc's opaque base URL.
                // sandbox: exactly one of the two grants, never both —
                // together they'd let the frame script its way back into
                // the app.
                //
                // `--serve`: `allow-same-origin` WITHOUT `allow-scripts`.
                // The rendered markdown is static HTML — no JS needed —
                // but its <img> subresources must carry the session
                // cookie or the cloud ForwardAuth gate 302s them to
                // login (sandboxed opaque origins send no credentials,
                // so chapter figures showed as broken images). Nothing
                // executes inside the frame; the parent handles link
                // clicks through `contentDocument` (bindPreviewLinks).
                //
                // Desktop: `allow-scripts` WITHOUT `allow-same-origin`.
                // The custom `thclaws://` scheme doesn't give the parent
                // that document access, so the frame stays opaque and
                // reports link clicks by postMessage instead. Its assets
                // come from the custom protocol, which needs no cookie.
                <iframe
                  ref={previewFrameRef}
                  key={`md-${preview.path}-${previewVersion}`}
                  srcDoc={injectBaseHref(preview.content, preview.path)}
                  onLoad={bindPreviewLinks}
                  className="w-full flex-1 min-h-0 rounded border"
                  style={{ borderColor: "var(--border)", background: "var(--bg-primary)" }}
                  sandbox={isDesktopShell ? "allow-scripts" : "allow-same-origin"}
                  title={preview.path}
                />
              ) : (
                <iframe
                  ref={htmlFrameRef}
                  key={`html-${preview.path}-${previewVersion}`}
                  src={assetUrl(preview.path)}
                  className="w-full flex-1 min-h-0 rounded border"
                  style={{ borderColor: "var(--border)", background: "var(--bg-primary)" }}
                  sandbox="allow-scripts"
                  title={preview.path}
                />
              )
            ) : hasSyntaxPreview ? (
              <CodeEditor
                source={preview.content}
                path={preview.path}
                readOnly
              />
            ) : (
              <pre
                className="text-xs font-mono whitespace-pre-wrap rounded p-3 flex-1 min-h-0 overflow-auto"
                style={{
                  background: "var(--bg-secondary)",
                  color: "var(--text-primary)",
                  tabSize: 4,
                }}
              >
                {preview.content}
              </pre>
            )}
          </div>
        ) : (
          <div
            className="text-sm mt-20 text-center"
            style={{ color: "var(--text-secondary)" }}
          >
            Click a file to preview
          </div>
        )}
      </div>

      {entryMenu && (
        <>
          <div
            className="fixed inset-0 z-[55]"
            onClick={() => setEntryMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setEntryMenu(null);
            }}
          />
          <div
            ref={entryMenuRef}
            className="fixed z-[56] rounded border shadow-lg text-xs py-1"
            style={{
              left: entryMenu.x,
              top: entryMenu.y,
              minWidth: "160px",
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
          >
            {/* Files-only: Download. Directories navigate; bulk
                folder download would zip on the backend — not worth
                the surface area until users ask for it. */}
            {!entryMenu.isDir && (
              <MenuItem
                icon={<Download size={13} />}
                label="Download"
                onClick={() => {
                  downloadFile(entryMenu.path);
                  setEntryMenu(null);
                }}
              />
            )}
            {/* HTML-only: hand the path to `/publish`. Deliberately a
                `shell_input` rather than a dedicated IPC — the slash
                command already validates, reads through the sandbox and
                reports refusals (no token, no credit, too big), and
                routing through it means the URL lands in the
                conversation where it can be copied, instead of in a
                toast that disappears. No backend change at all. */}
            {!entryMenu.isDir && /\.(html|htm)$/i.test(entryMenu.name) && (
              <MenuItem
                icon={<Globe size={13} />}
                label="Publish to the web…"
                onClick={() => {
                  const m = entryMenu;
                  setEntryMenu(null);
                  send({ type: "shell_input", text: `/publish ${m.path}` });
                  // The link lands in Chat, not here — without this the
                  // Files tab shows no sign the click did anything.
                  setPublishNotice(`Publishing ${m.name} — the link will appear in Chat`);
                  setTimeout(() => setPublishNotice(null), 6000);
                }}
              />
            )}
            {/* PDF-only: a markdown sibling, extracted not generated.
                No model, no cost, no waiting on a turn — the backend
                shells out to poppler and repairs Thai marks. */}
            {!entryMenu.isDir && /\.pdf$/i.test(entryMenu.name) && (
              <MenuItem
                icon={<FileText size={13} />}
                label="Convert to markdown"
                title="Extract the text beside the PDF as .md — no model for text PDFs; a scan is OCR'd on your organisation's document service, then proofread by the agent against the page images"
                onClick={() => {
                  const m = entryMenu;
                  setEntryMenu(null);
                  setSaveToast(`converting ${m.name}…`);
                  // One-shot, like the upload path: the long-lived
                  // subscription up top has `[]` deps, so `currentPath`
                  // read from inside it would be whatever it was on
                  // mount and the tree would refresh the wrong folder.
                  const unsub = subscribe((msg) => {
                    if (
                      msg.type !== "pdf_to_markdown_result" ||
                      msg.path !== m.path
                    ) {
                      return;
                    }
                    unsub();
                    if (msg.ok) {
                      const out = String(msg.out ?? "");
                      send({
                        type: "file_list",
                        path: currentPath,
                        show_hidden: showHidden,
                      });
                      if (msg.ocr) {
                        // Tesseract alone misreads Thai scans: the agent
                        // proofreads the draft against the page images.
                        setSaveToast(
                          `OCR done — the agent is proofreading ${out.split("/").pop()} against the scan…`,
                        );
                        send({
                          type: "shell_input",
                          text:
                            `/agent content-extractor Proofread \`${out}\`. It is raw OCR of the scanned PDF ` +
                            `\`${m.path}\`. Read the PDF with PdfRead (it attaches every page image), ` +
                            `correct the markdown against the images — misread words and numbers, missing ` +
                            `vowels and tone marks, junk lines from specks — keep the document's own ` +
                            `headings and order, drop the <!-- page --> comments, and overwrite ` +
                            `\`${out}\` with the corrected text. Do not modify the PDF.`,
                        });
                        setTimeout(() => setSaveToast(null), 6000);
                        return;
                      }
                      setSaveToast(`wrote ${out.split("/").pop()}`);
                    } else {
                      setSaveToast(
                        `convert failed: ${msg.error ?? "unknown error"}`,
                      );
                    }
                    setTimeout(() => setSaveToast(null), 4000);
                  });
                  send({ type: "pdf_to_markdown", path: m.path });
                }}
              />
            )}
            {/* Markdown-only: ingest into a knowledge base. One row that
                opens a panel beside the menu — the four modes, then the
                bases when there is more than one to choose from. */}
            {!entryMenu.isDir && /\.(md|markdown)$/i.test(entryMenu.name) && (
              <div
                style={{ position: "relative" }}
                onMouseEnter={() => kmsList.length > 0 && setKmsFlyout(true)}
                onMouseLeave={() => {
                  setKmsFlyout(false);
                  setKmsPickMode(null);
                }}
              >
                <MenuItem
                  icon={<Library size={13} />}
                  label="Add to knowledge base ▸"
                  title={
                    kmsList.length === 0
                      ? "No knowledge base yet — create one first"
                      : "Archive it, summarise it, or break it into cited notes"
                  }
                  onClick={() => {
                    if (kmsList.length === 0) {
                      setEntryMenu(null);
                      setSaveToast("no KMS yet — create one first");
                      setTimeout(() => setSaveToast(null), 3000);
                      return;
                    }
                    setKmsFlyout((open) => !open);
                  }}
                />
                {kmsFlyout && kmsList.length > 0 && (
                  <SubMenu
                    key={kmsPickMode ?? "modes"}
                    anchorRef={entryMenuRef}
                  >
                    {kmsPickMode === null
                      ? INGEST_MODES.map(({ id: mode, label, hint }) => (
                          <MenuItem
                            key={`add-${mode}`}
                            icon={<Library size={13} />}
                            title={hint}
                            label={
                              kmsList.length > 1 ? `${label}…` : label
                            }
                            onClick={() => {
                              const m = entryMenu;
                              // One base is the common case: no second
                              // choice to make, so do not ask for one.
                              if (kmsList.length === 1) {
                                setEntryMenu(null);
                                addToKms(
                                  m.path,
                                  m.name,
                                  kmsList[0].name,
                                  false,
                                  mode,
                                );
                              } else {
                                setKmsPickMode(mode);
                              }
                            }}
                          />
                        ))
                      : [
                          <MenuItem
                            key="back"
                            icon={<span style={{ width: 13 }}>‹</span>}
                            label="Back"
                            onClick={() => setKmsPickMode(null)}
                          />,
                          ...kmsList.map((k) => (
                            <MenuItem
                              key={`${k.scope}:${k.name}`}
                              icon={<Library size={12} />}
                              label={k.name}
                              onClick={() => {
                                const m = entryMenu;
                                setEntryMenu(null);
                                addToKms(
                                  m.path,
                                  m.name,
                                  k.name,
                                  false,
                                  kmsPickMode,
                                );
                              }}
                            />
                          )),
                        ]}
                  </SubMenu>
                )}
              </div>
            )}
            {!entryMenu.isDir &&
              convertTargets(entryMenu.name).map((to) => (
                <MenuItem
                  key={`convert-${to}`}
                  icon={<FileOutput size={13} />}
                  label={`Convert to ${to.toUpperCase()}`}
                  title="Converted on the document service and saved beside the file"
                  onClick={() => {
                    const m = entryMenu;
                    setEntryMenu(null);
                    convertDoc(m.path, m.name, to);
                  }}
                />
              ))}
            {/* Translate or summarize into a chosen language, saved as a
                `<stem>-<code>.md` / `<stem>-<code>-sum.md` sibling. */}
            {!entryMenu.isDir && LANG_ACTION_RE.test(entryMenu.name) && (
              <>
                <MenuItem
                  icon={<Languages size={13} />}
                  label="Translate…"
                  onClick={() => {
                    const m = entryMenu;
                    setEntryMenu(null);
                    // A Thai file is usually translated out of Thai.
                    setLangCode(/[\u0E00-\u0E7F]/.test(m.name) ? "en" : "th");
                    setLangAction({ mode: "translate", path: m.path, name: m.name });
                  }}
                />
                <MenuItem
                  icon={<FileText size={13} />}
                  label="Summarize…"
                  onClick={() => {
                    const m = entryMenu;
                    setEntryMenu(null);
                    setLangCode("th");
                    setLangAction({ mode: "summarize", path: m.path, name: m.name });
                  }}
                />
              </>
            )}
            {/* Folders-only: catalogue the folder into `index.md`. */}
            {entryMenu.isDir && (
              <MenuItem
                icon={<ListTree size={13} />}
                label="Index folder…"
                onClick={() => {
                  const m = entryMenu;
                  setEntryMenu(null);
                  setIndexLang("th");
                  setIndexScope("all");
                  setIndexForce(false);
                  setIndexAction({ path: m.path, name: m.name });
                }}
              />
            )}
            <MenuItem
              icon={<Pencil size={13} />}
              label="Rename"
              onClick={() => {
                const m = entryMenu;
                startRename(m.path, m.name, m.isDir);
              }}
            />
            <MenuItem
              icon={<Trash2 size={13} />}
              label="Delete"
              danger
              onClick={() => {
                const m = entryMenu;
                setEntryMenu(null);
                deleteEntry(m.path, m.name, m.isDir);
              }}
            />
          </div>
        </>
      )}

      {explorerMenu && (
        <>
          <div
            className="fixed inset-0 z-[55]"
            onClick={() => setExplorerMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setExplorerMenu(null);
            }}
          />
          <div
            className="fixed z-[56] rounded border shadow-lg text-xs py-1"
            style={{
              left: explorerMenu.x,
              top: explorerMenu.y,
              minWidth: "160px",
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
          >
            <MenuItem
              icon={<FilePlus size={13} />}
              label="New file…"
              onClick={() => startCreate("file")}
            />
            <MenuItem
              icon={<FolderPlus size={13} />}
              label="New folder…"
              onClick={() => startCreate("folder")}
            />
          </div>
        </>
      )}

      {createKind && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          onClick={() => setCreateKind(null)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setCreateKind(null);
          }}
        >
          <form
            className="rounded-lg border shadow-xl w-[420px] max-w-[92vw]"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onClick={(e) => e.stopPropagation()}
            onSubmit={submitCreate}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <span style={{ color: "var(--accent)" }}>●</span>
              <span>
                New {createKind} in {currentPath}
              </span>
            </div>
            <div className="px-4 py-3 space-y-2 text-xs">
              <input
                autoFocus
                type="text"
                value={createName}
                onChange={(e) => setCreateName(e.target.value)}
                placeholder={createKind === "folder" ? "src" : "notes.md"}
                className="w-full px-2 py-1.5 rounded border font-mono text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-primary)",
                }}
              />
              {createError && (
                <div style={{ color: "var(--danger, #e06c75)" }}>{createError}</div>
              )}
            </div>
            <div
              className="px-4 py-2.5 border-t flex justify-end gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <button
                type="button"
                onClick={() => setCreateKind(null)}
                className="px-3 py-1.5 rounded border text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-secondary)",
                }}
              >
                Cancel
              </button>
              <button
                type="submit"
                disabled={creating}
                className="px-3 py-1.5 rounded text-xs font-medium"
                style={{
                  background: "var(--accent)",
                  color: "var(--accent-fg, #fff)",
                  opacity: creating ? 0.6 : 1,
                  cursor: creating ? "default" : "pointer",
                }}
              >
                {creating ? "Creating…" : "Create"}
              </button>
            </div>
          </form>
        </div>
      )}

      {renameTarget && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          onClick={() => setRenameTarget(null)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setRenameTarget(null);
          }}
        >
          <form
            className="rounded-lg border shadow-xl w-[420px] max-w-[92vw]"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onClick={(e) => e.stopPropagation()}
            onSubmit={submitRename}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <Pencil size={14} style={{ color: "var(--accent)" }} />
              <span>Rename {renameTarget.isDir ? "folder" : "file"}</span>
            </div>
            <div className="px-4 py-3 space-y-2 text-xs">
              <input
                autoFocus
                type="text"
                value={renameName}
                onChange={(e) => setRenameName(e.target.value)}
                onFocus={(e) => e.target.select()}
                placeholder={renameTarget.name}
                className="w-full px-2 py-1.5 rounded border font-mono text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-primary)",
                }}
              />
              {renameError && (
                <div style={{ color: "var(--danger, #e06c75)" }}>{renameError}</div>
              )}
            </div>
            <div
              className="px-4 py-2.5 border-t flex justify-end gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <button
                type="button"
                onClick={() => setRenameTarget(null)}
                className="px-3 py-1.5 rounded border text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-secondary)",
                }}
              >
                Cancel
              </button>
              <button
                type="submit"
                disabled={renaming}
                className="px-3 py-1.5 rounded text-xs font-medium"
                style={{
                  background: "var(--accent)",
                  color: "var(--accent-fg, #fff)",
                  opacity: renaming ? 0.6 : 1,
                  cursor: renaming ? "default" : "pointer",
                }}
              >
                {renaming ? "Renaming…" : "Rename"}
              </button>
            </div>
          </form>
        </div>
      )}

      {langAction && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          onClick={() => setLangAction(null)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setLangAction(null);
          }}
        >
          <form
            className="rounded-lg border shadow-xl w-[420px] max-w-[92vw]"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onClick={(e) => e.stopPropagation()}
            onSubmit={submitLangAction}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              {langAction.mode === "summarize" ? (
                <FileText size={14} style={{ color: "var(--accent)" }} />
              ) : (
                <Languages size={14} style={{ color: "var(--accent)" }} />
              )}
              <span>
                {langAction.mode === "summarize" ? "Summarize" : "Translate"} file
              </span>
            </div>
            <div className="px-4 py-3 space-y-3 text-xs">
              <div style={{ color: "var(--text-secondary)" }}>
                {langAction.mode === "summarize" ? "Summarize" : "Translate"}{" "}
                <span className="font-mono" style={{ color: "var(--text-primary)" }}>
                  {langAction.name}
                </span>{" "}
                {langAction.mode === "summarize" ? "in:" : "into:"}
              </div>
              <select
                autoFocus
                value={langCode}
                onChange={(e) => setLangCode(e.target.value)}
                className="w-full px-2 py-1.5 rounded border text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-primary)",
                }}
              >
                {TRANSLATE_LANGUAGES.map((l) => (
                  <option key={l.code} value={l.code}>
                    {l.label}
                  </option>
                ))}
              </select>
              <div style={{ color: "var(--text-secondary)" }}>
                Saves to{" "}
                <span className="font-mono" style={{ color: "var(--text-primary)" }}>
                  {langAction.name.replace(LANG_ACTION_RE, "")}-{langCode}
                  {langAction.mode === "summarize" ? "-sum" : ""}.md
                </span>
              </div>
            </div>
            <div
              className="px-4 py-2.5 border-t flex justify-end gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <button
                type="button"
                onClick={() => setLangAction(null)}
                className="px-3 py-1.5 rounded border text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-secondary)",
                }}
              >
                Cancel
              </button>
              <button
                type="submit"
                className="px-3 py-1.5 rounded text-xs font-medium"
                style={{
                  background: "var(--accent)",
                  color: "var(--accent-fg, #fff)",
                }}
              >
                {langAction.mode === "summarize" ? "Summarize" : "Translate"}
              </button>
            </div>
          </form>
        </div>
      )}

      {indexAction && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          onClick={() => setIndexAction(null)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setIndexAction(null);
          }}
        >
          <form
            className="rounded-lg border shadow-xl w-[420px] max-w-[92vw]"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onClick={(e) => e.stopPropagation()}
            onSubmit={submitIndexAction}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <ListTree size={14} style={{ color: "var(--accent)" }} />
              <span>Index folder</span>
            </div>
            <div className="px-4 py-3 space-y-3 text-xs">
              <div style={{ color: "var(--text-secondary)" }}>
                Catalogue{" "}
                <span className="font-mono" style={{ color: "var(--text-primary)" }}>
                  {indexAction.name}
                </span>{" "}
                into{" "}
                <span className="font-mono" style={{ color: "var(--text-primary)" }}>
                  {indexAction.name}/index.md
                </span>
                , describing each file in:
              </div>
              <select
                autoFocus
                value={indexLang}
                onChange={(e) => setIndexLang(e.target.value)}
                className="w-full px-2 py-1.5 rounded border text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-primary)",
                }}
              >
                {TRANSLATE_LANGUAGES.map((l) => (
                  <option key={l.code} value={l.code}>
                    {l.label}
                  </option>
                ))}
              </select>
              <div className="space-y-1.5">
                <div style={{ color: "var(--text-secondary)" }}>Listing:</div>
                {(
                  [
                    ["all", "Every file, grouped by subfolder"],
                    ["one-level", "This folder + one row per subfolder"],
                    ["flat", "This folder only"],
                  ] as const
                ).map(([value, label]) => (
                  <label
                    key={value}
                    className="flex items-center gap-2 cursor-pointer"
                  >
                    <input
                      type="radio"
                      name="index-scope"
                      checked={indexScope === value}
                      onChange={() => setIndexScope(value)}
                    />
                    <span>{label}</span>
                  </label>
                ))}
                {indexScope === "one-level" && (
                  <div
                    className="pl-6"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    Subfolders are still scanned in full — each one gets a
                    sentence describing what's inside, not a file list.
                  </div>
                )}
              </div>
              <label className="flex items-center gap-2 cursor-pointer">
                <input
                  type="checkbox"
                  checked={indexForce}
                  onChange={(e) => setIndexForce(e.target.checked)}
                />
                <span>Re-read every file (ignore cache)</span>
              </label>
              <div style={{ color: "var(--text-secondary)" }}>
                Only files whose content changed since the last run are re-read —
                fingerprints live in{" "}
                <span className="font-mono" style={{ color: "var(--text-primary)" }}>
                  .thclaws-index.json
                </span>
                .
              </div>
            </div>
            <div
              className="px-4 py-2.5 border-t flex justify-end gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <button
                type="button"
                onClick={() => setIndexAction(null)}
                className="px-3 py-1.5 rounded border text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-secondary)",
                }}
              >
                Cancel
              </button>
              <button
                type="submit"
                className="px-3 py-1.5 rounded text-xs font-medium"
                style={{
                  background: "var(--accent)",
                  color: "var(--accent-fg, #fff)",
                }}
              >
                Index
              </button>
            </div>
          </form>
        </div>
      )}
    </div>
  );
}
