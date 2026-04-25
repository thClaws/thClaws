import { useEffect, useState } from "react";
import { X, Save, FileText } from "lucide-react";
import { useEditor, EditorContent } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import { marked } from "marked";
import TurndownService from "turndown";
import { send, subscribe } from "../hooks/useIPC";

// Markdown ↔ HTML round-trip for the TipTap editor. The file on disk
// stays markdown (AGENTS.md / CLAUDE.md are canonically markdown and
// the LLM/Claude Code expect that format), but TipTap works natively
// in HTML — so we convert on load + on save. `marked` handles MD →
// HTML (GFM tables, task lists, fenced code). `turndown` handles HTML
// → MD on the way back; tuned to use `#` headings (ATX) and `-`
// bullets so the saved file doesn't drift styles on every round-trip.
// `async: false` forces `marked.parse()` to return a string instead of
// a Promise. Without it, any async extension would flip the whole
// instance async and TipTap would receive `[object Promise]` as its
// "HTML", which it renders as plain text — exactly what was showing up
// in the round-trip when markdown survived through unconverted.
marked.setOptions({ gfm: true, breaks: false, async: false });
const turndownService = new TurndownService({
  headingStyle: "atx",
  bulletListMarker: "-",
  codeBlockStyle: "fenced",
  emDelimiter: "_",
});

type Scope = "global" | "folder";

const SCOPE_LABEL: Record<Scope, string> = {
  global: "Global instructions",
  folder: "Folder instructions",
};

const SCOPE_HINT: Record<Scope, string> = {
  global:
    "Applies to every thClaws session on this machine. Stored at ~/.config/thclaws/AGENTS.md.",
  folder:
    "Applies only to the current project. Stored as AGENTS.md in the working directory.",
};

export function InstructionsEditorModal({
  scope,
  onClose,
}: {
  scope: Scope;
  onClose: () => void;
}) {
  const [path, setPath] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [flash, setFlash] = useState<{ ok: boolean; msg: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const editor = useEditor({
    // StarterKit gives us the ProseMirror nodes for paragraphs,
    // headings, lists, code blocks, blockquote, bold / italic / code /
    // link — everything AGENTS.md typically uses. We drop the
    // tiptap-markdown extension because marked/turndown handle the
    // round-trip at the IPC boundary; the editor itself only ever
    // sees HTML.
    extensions: [StarterKit.configure({})],
    content: "",
    editorProps: {
      attributes: {
        // `tiptap-compact` is a marker class matched by the inline
        // <style> below to dial the default `prose-sm` font sizes
        // down one notch — the old defaults felt oversized for what
        // is basically a code / settings file.
        class:
          "tiptap-compact prose prose-invert prose-sm max-w-none focus:outline-none min-h-[320px] px-4 py-3",
      },
      // Route Cmd/Ctrl+C and Cmd/Ctrl+V through the wry IPC bridge
      // because wry blocks `document.execCommand('copy')` and
      // `navigator.clipboard`. Without these, the editor silently
      // ignores both shortcuts inside the modal.
      handleKeyDown(view, event) {
        const isMac = navigator.platform.startsWith("Mac");
        const mod = isMac ? event.metaKey : event.ctrlKey;
        if (!mod || event.altKey) return false;
        if (event.key === "c" || event.key === "C") {
          const { from, to, empty } = view.state.selection;
          if (empty) return false;
          const text = view.state.doc.textBetween(from, to, "\n", " ");
          if (text.length === 0) return false;
          send({ type: "clipboard_write", text });
          event.preventDefault();
          return true;
        }
        if (event.key === "x" || event.key === "X") {
          const { from, to, empty } = view.state.selection;
          if (empty) return false;
          const text = view.state.doc.textBetween(from, to, "\n", " ");
          if (text.length === 0) return false;
          send({ type: "clipboard_write", text });
          view.dispatch(view.state.tr.deleteSelection());
          event.preventDefault();
          return true;
        }
        if (event.key === "v" || event.key === "V") {
          const unsub = subscribe((msg) => {
            if (msg.type !== "clipboard_text") return;
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
            if (text.length === 0) return;
            const tr = view.state.tr.insertText(text);
            view.dispatch(tr);
          });
          send({ type: "clipboard_read" });
          event.preventDefault();
          return true;
        }
        return false;
      },
    },
  });

  // Load content once, then subscribe for the round-trip of save results.
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "instructions_content" && msg.scope === scope) {
        if (typeof msg.path === "string") setPath(msg.path);
        if (editor) {
          const md = (msg.content as string) ?? "";
          // Convert markdown from disk to HTML for TipTap. We run the
          // conversion on a microtask delay so the editor's DOM view
          // is definitely mounted before setContent fires — otherwise
          // TipTap silently discards the update on a freshly-created
          // editor and the original raw markdown ends up in the doc
          // as plain text.
          queueMicrotask(() => {
            if (editor.isDestroyed) return;
            let html = "";
            if (md.length > 0) {
              const out = marked.parse(md, { async: false });
              html = typeof out === "string" ? out : md;
            }
            // marked already emits block-level HTML (<h1>, <p>, <ul>)
            // so we pass it straight to setContent. TipTap parses it
            // via ProseMirror's DOMParser, which walks the nodes and
            // maps them to schema-registered types (StarterKit covers
            // all the common markdown primitives).
            editor.commands.setContent(html, {
              emitUpdate: false,
              parseOptions: { preserveWhitespace: false },
            });
            editor.commands.focus("end");
          });
        }
        setLoaded(true);
      } else if (msg.type === "instructions_save_result" && msg.scope === scope) {
        setBusy(false);
        if (msg.ok) {
          // Successful save → dismiss the modal so the user returns to
          // whatever they were doing. The sidebar / system prompt picks
          // up the new instructions on the next turn.
          onClose();
        } else {
          // Keep the modal open on failure so the user can fix the
          // content or the filesystem issue and retry without losing
          // their edits.
          setFlash({
            ok: false,
            msg: `Save failed: ${msg.error ?? "unknown error"}`,
          });
          setTimeout(() => setFlash(null), 3000);
        }
      }
    });
    send({ type: "instructions_get", scope });
    return unsub;
  }, [scope, editor, onClose]);

  const handleSave = () => {
    if (!editor) return;
    setBusy(true);
    // HTML from TipTap → markdown via turndown. We strip a trailing
    // newline and add exactly one so every save lands on a tidy POSIX
    // line-ending, which `git diff` and editors prefer over "no final
    // newline".
    const html = editor.getHTML();
    const md = turndownService.turndown(html).replace(/\n+$/, "") + "\n";
    send({ type: "instructions_save", scope, content: md });
  };

  return (
    <div
      className="fixed inset-0 flex items-center justify-center z-50"
      style={{ background: "var(--modal-backdrop)" }}
      // Close on mousedown (not mouseup) AND only when the click
      // originates on the backdrop itself — so a drag-to-select that
      // begins inside the modal and ends on the backdrop doesn't
      // accidentally dismiss the dialog.
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="rounded-lg shadow-2xl max-w-3xl w-full mx-4 max-h-[85vh] flex flex-col"
        style={{ background: "var(--bg-secondary)", border: "1px solid var(--border)" }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div
          className="flex items-center justify-between p-4 border-b"
          style={{ borderColor: "var(--border)" }}
        >
          <div className="flex items-center gap-2">
            <FileText size={16} style={{ color: "var(--accent)" }} />
            <div>
              <h2
                className="text-sm font-semibold"
                style={{ color: "var(--text-primary)" }}
              >
                {SCOPE_LABEL[scope]}
              </h2>
              <div
                className="font-mono"
                style={{ color: "var(--text-secondary)", fontSize: "10px" }}
              >
                {path ?? SCOPE_HINT[scope]}
              </div>
            </div>
          </div>
          <button
            onClick={onClose}
            className="p-1 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Close"
          >
            <X size={14} />
          </button>
        </div>

        <div className="flex-1 overflow-y-auto">
          {/* Font-size overrides for the embedded TipTap editor. Prose's
              built-in `prose-sm` defaults were loud for a settings-style
              document; this keeps the proportions but scales body +
              headings down to something closer to the surrounding UI. */}
          <style>{`
            .tiptap-compact { font-size: 13px; line-height: 1.55; }
            .tiptap-compact p { font-size: 13px; margin-top: 0.35em; margin-bottom: 0.35em; }
            .tiptap-compact h1 { font-size: 1.15rem; margin-top: 0.6em; margin-bottom: 0.3em; }
            .tiptap-compact h2 { font-size: 1.0rem;  margin-top: 0.55em; margin-bottom: 0.25em; }
            .tiptap-compact h3 { font-size: 0.92rem; margin-top: 0.5em;  margin-bottom: 0.2em; }
            .tiptap-compact h4, .tiptap-compact h5, .tiptap-compact h6 { font-size: 0.85rem; }
            /* Tailwind's preflight strips list markers on ul/ol; force
               them back on inside the editor so bullets / numbers
               render for the WYSIWYG view. Bullets sit outside the
               content box so wrapped text aligns under the first
               character (the same layout GitHub / Notion use). */
            .tiptap-compact ul {
              list-style: disc;
              list-style-position: outside;
              margin-top: 0.3em;
              margin-bottom: 0.3em;
              padding-left: 1.5em;
            }
            .tiptap-compact ol {
              list-style: decimal;
              list-style-position: outside;
              margin-top: 0.3em;
              margin-bottom: 0.3em;
              padding-left: 1.5em;
            }
            .tiptap-compact ul ul { list-style: circle; }
            .tiptap-compact ul ul ul { list-style: square; }
            .tiptap-compact li {
              font-size: 13px;
              margin-top: 0.15em;
              margin-bottom: 0.15em;
              display: list-item;
            }
            .tiptap-compact li > p { margin: 0; }
            .tiptap-compact code { font-size: 12px; }
            .tiptap-compact pre { font-size: 12px; padding: 0.5em 0.7em; }
            .tiptap-compact blockquote { font-size: 13px; margin: 0.4em 0; }
          `}</style>
          {loaded ? (
            <EditorContent editor={editor} />
          ) : (
            <div
              className="px-4 py-8 text-center text-xs"
              style={{ color: "var(--text-secondary)" }}
            >
              Loading…
            </div>
          )}
        </div>

        <div
          className="flex items-center justify-between p-3 border-t"
          style={{ borderColor: "var(--border)" }}
        >
          <div
            className="text-[10px] flex-1 mr-3 truncate"
            style={{
              color: flash
                ? flash.ok
                  ? "var(--accent)"
                  : "var(--danger, #e06c75)"
                : "var(--text-secondary)",
            }}
          >
            {flash ? flash.msg : SCOPE_HINT[scope]}
          </div>
          <div className="flex gap-2 shrink-0">
            <button
              onClick={onClose}
              className="px-3 py-1.5 rounded text-xs"
              style={{
                background: "var(--bg-primary)",
                color: "var(--text-secondary)",
                border: "1px solid var(--border)",
              }}
            >
              Cancel
            </button>
            <button
              onClick={handleSave}
              disabled={!editor || busy}
              className="px-3 py-1.5 rounded text-xs font-medium flex items-center gap-1"
              style={{
                background: "var(--accent)",
                color: "#fff",
                opacity: editor && !busy ? 1 : 0.4,
                cursor: editor && !busy ? "pointer" : "not-allowed",
              }}
            >
              <Save size={12} /> Save
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
