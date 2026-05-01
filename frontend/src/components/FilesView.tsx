import { useState, useEffect, useCallback, useRef } from "react";
import { Folder, File, ArrowUp, Pencil, Eye, Save, X } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { useTheme } from "../hooks/useTheme";
import { MarkdownEditor } from "./MarkdownEditor";
import { CodeEditor } from "./CodeEditor";

// Round-trip a native-OS confirm dialog through the Rust backend so we
// get a real modal on macOS / Linux / Windows instead of relying on
// `window.confirm` (which wry WebViews don't implement consistently).
// The backend shows the dialog on its IPC worker thread and replies
// with a `confirm_result` message keyed by `id`.
function confirmNative(opts: {
  title: string;
  message: string;
  yesLabel?: string;
  noLabel?: string;
}): Promise<boolean> {
  return new Promise((resolve) => {
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

// Build a same-origin URL for the custom protocol's file-asset handler.
// Keeping path separators unencoded lets the browser treat the URL as
// a directory structure, so relative references inside the HTML (e.g.
// `<link href="style.css">`) resolve to sibling files on disk.
function assetUrl(absPath: string): string {
  const normalized = absPath.replace(/\\/g, "/");
  const segments = normalized.split("/").map(encodeURIComponent).join("/");
  const leadingSlash = segments.startsWith("/") ? "" : "/";
  return `${window.location.origin}/file-asset${leadingSlash}${segments}`;
}

export function FilesView({ active }: Props) {
  const [currentPath, setCurrentPath] = useState(".");
  const [entries, setEntries] = useState<FileEntry[]>([]);
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
  } | null>(null);

  // Bumped on every Refresh click; used as part of iframe `key` props so
  // the iframe unmounts + re-fetches its asset (otherwise the browser
  // caches the iframe content even after the file on disk changes —
  // most visible with the productivity plugin's dashboard.html, which
  // an agent regenerates after every TASKS.md mutation).
  const [previewVersion, setPreviewVersion] = useState(0);

  const [mode, setMode] = useState<ViewMode>("preview");
  // Source-text kept separate from preview.content because the preview
  // content may be rendered HTML while the editor always operates on
  // raw text.
  const [editorSource, setEditorSource] = useState<string>("");
  const [editorDirty, setEditorDirty] = useState(false);
  const [saveToast, setSaveToast] = useState<string | null>(null);
  const pendingNavigation = useRef<{ path: string } | null>(null);

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
        const incomingReadMode: ReadMode =
          (msg.mode as ReadMode) ?? "preview";
        setPreview({
          path: incomingPath,
          content: incomingContent,
          mime: msg.mime as string,
          readMode: incomingReadMode,
        });
        if (incomingReadMode === "source") {
          setEditorSource(incomingContent);
          setEditorDirty(false);
        }
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
    send({ type: "file_list", path: "." });
    return unsub;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Auto-refresh directory listing + preview while tab active.
  // Never auto-refresh when user is editing — we'd clobber their work.
  // `themeMode` is a dep so a light/dark swap re-fetches the .md
  // preview with the fresh palette baked into its iframe HTML.
  useEffect(() => {
    if (!active) return;
    send({ type: "file_list", path: currentPath });
    if (preview && mode === "preview") {
      send({ type: "file_read", path: preview.path, mode: "preview", theme: themeMode });
    }
    const interval = setInterval(() => {
      send({ type: "file_list", path: currentPath });
      if (preview && mode === "preview") {
        send({ type: "file_read", path: preview.path, mode: "preview", theme: themeMode });
      }
    }, 2000);
    return () => clearInterval(interval);
  // `preview?.path` is intentional — using the full `preview` object
  // would re-run on every polling cycle (setPreview creates a new
  // reference each time), resetting the interval unnecessarily.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, currentPath, preview?.path, mode, themeMode]);

  const navigate = (name: string) => {
    const path = currentPath === "." ? name : `${currentPath}/${name}`;
    send({ type: "file_list", path });
  };

  const goUp = () => {
    const parent = currentPath.includes("/")
      ? currentPath.substring(0, currentPath.lastIndexOf("/"))
      : ".";
    send({ type: "file_list", path: parent || "." });
  };

  const openFile = useCallback((path: string) => {
    setMode("preview");
    send({ type: "file_read", path, mode: "preview", theme: themeMode });
  }, [themeMode]);

  const onSidebarClick = async (name: string) => {
    const path = currentPath === "." ? name : `${currentPath}/${name}`;
    if (mode === "edit" && editorDirty) {
      const ok = await confirmNative({
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
  const refreshPreview = () => {
    if (!preview) return;
    if (editorDirty) {
      const ok = window.confirm(
        "You have unsaved changes in the editor. Refresh anyway? Unsaved edits will be lost."
      );
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
      const ok = await confirmNative({
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
  // Sender origin isn't checked because the iframe runs sandboxed
  // from a `thclaws://` asset URL — the attack surface is bounded
  // to our own dashboard content.
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

      // Resolve `filename` (e.g. "TASKS.md") against the directory of
      // the currently-previewed file. So opening
      // `/proj/business-cards/dashboard.html` and asking for
      // "TASKS.md" hits `/proj/business-cards/TASKS.md`.
      const slash = preview.path.lastIndexOf("/");
      const dir = slash > 0 ? preview.path.slice(0, slash) : ".";
      const targetPath = `${dir}/${d.filename || "TASKS.md"}`;

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

  const isHtml = preview?.mime === "text/html";
  const isImage = preview?.mime.startsWith("image/");
  const isPdf = preview?.mime === "application/pdf";
  const canEdit = preview && isTextEditable(preview.path);
  const hasSyntaxPreview =
    preview && SYNTAX_PREVIEW.has(extOf(preview.path));


  return (
    <div className="flex h-full" style={{ background: "var(--bg-primary)" }}>
      {/* Tree panel */}
      <div
        className="w-64 overflow-y-auto border-r shrink-0 flex flex-col"
        style={{ borderColor: "var(--border)" }}
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
          <span className="truncate">{currentPath}</span>
        </div>

        <div className="overflow-y-auto flex-1 p-1">
          {entries.length === 0 ? (
            <div className="text-xs p-2" style={{ color: "var(--text-secondary)" }}>
              Empty directory
            </div>
          ) : (
            entries.map((entry) => (
              <button
                key={entry.name}
                className="flex items-center gap-1.5 w-full px-2 py-1 rounded text-xs hover:bg-white/5 text-left"
                style={{ color: "var(--text-primary)" }}
                onClick={() =>
                  entry.is_dir ? navigate(entry.name) : onSidebarClick(entry.name)
                }
              >
                {entry.is_dir ? (
                  <Folder size={13} style={{ color: "var(--accent)", flexShrink: 0 }} />
                ) : (
                  <File size={13} style={{ color: "var(--text-secondary)", flexShrink: 0 }} />
                )}
                <span className="truncate">{entry.name}</span>
              </button>
            ))
          )}
        </div>
      </div>

      {/* Preview / editor panel */}
      <div className="flex-1 min-w-0 min-h-0 flex flex-col p-4">
        {preview ? (
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
              </div>
            </div>

            {/* Body: preview or editor */}
            {mode === "edit" ? (
              isMarkdownPath(preview.path) ? (
                <MarkdownEditor
                  source={editorSource}
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
              <iframe
                src={`data:application/pdf;base64,${preview.content}`}
                className="w-full flex-1 min-h-0 rounded border"
                style={{ borderColor: "var(--border)", background: "#fff" }}
                title={preview.path}
              />
            ) : isHtml ? (
              isMarkdownPath(preview.path) ? (
                // Markdown preview: backend renders MD → HTML and
                // returns it in `content`. Use `srcDoc` so the iframe
                // shows that HTML directly; `src={assetUrl}` would
                // fetch the raw .md via the custom protocol and the
                // iframe would end up blank.
                <iframe
                  key={`md-${preview.path}-${previewVersion}`}
                  srcDoc={preview.content}
                  className="w-full flex-1 min-h-0 rounded border"
                  style={{ borderColor: "var(--border)", background: "var(--bg-primary)" }}
                  sandbox="allow-scripts"
                  title={preview.path}
                />
              ) : (
                <iframe
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
    </div>
  );
}
