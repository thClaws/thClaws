import { useEffect, useRef } from "react";
import { EditorState, Compartment } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { oneDark } from "@codemirror/theme-one-dark";
import { defaultKeymap, indentWithTab } from "@codemirror/commands";
import { javascript } from "@codemirror/lang-javascript";
import { html } from "@codemirror/lang-html";
import { css } from "@codemirror/lang-css";
import { python } from "@codemirror/lang-python";
import { rust } from "@codemirror/lang-rust";
import { json } from "@codemirror/lang-json";
import { yaml } from "@codemirror/lang-yaml";
import { markdown } from "@codemirror/lang-markdown";
import { xml } from "@codemirror/lang-xml";
import { sql } from "@codemirror/lang-sql";
import type { Extension } from "@codemirror/state";

interface Props {
  source: string;
  path: string;
  onChange: (text: string) => void;
  onSave?: () => void;
}

function languageForExtension(ext: string): Extension {
  switch (ext) {
    case "js":
    case "jsx":
    case "mjs":
    case "cjs":
      return javascript({ jsx: true });
    case "ts":
      return javascript({ typescript: true });
    case "tsx":
      return javascript({ jsx: true, typescript: true });
    case "html":
    case "htm":
      return html();
    case "css":
    case "scss":
    case "sass":
    case "less":
      return css();
    case "py":
    case "pyi":
      return python();
    case "rs":
      return rust();
    case "json":
      return json();
    case "yaml":
    case "yml":
      return yaml();
    case "md":
    case "markdown":
      return markdown();
    case "xml":
    case "svg":
      return xml();
    case "sql":
      return sql();
    default:
      return [];
  }
}

// CodeMirror 6 editor wrapper. Language pack is picked from the file
// extension; theme is oneDark (dark-first, with our WebView theme in
// the light media query it still reads fine). `onSave` is bound to
// Cmd/Ctrl-S via a prepended keymap so `EditorView.defaultKeymap`
// still handles everything else.
export function CodeEditor({ source, path, onChange, onSave }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  // Latest handlers in refs so the editor-creation effect doesn't
  // have to re-run every keystroke.
  const onChangeRef = useRef(onChange);
  const onSaveRef = useRef(onSave);
  onChangeRef.current = onChange;
  onSaveRef.current = onSave;

  // Compartments let us hot-swap extensions (language pack) when the
  // path changes without recreating the whole editor state.
  const languageCompartment = useRef(new Compartment());

  useEffect(() => {
    if (!containerRef.current) return;
    const ext = path.split(".").pop()?.toLowerCase() ?? "";
    const languagePack = languageForExtension(ext);

    const saveBinding = keymap.of([
      {
        key: "Mod-s",
        preventDefault: true,
        run: () => {
          onSaveRef.current?.();
          return true;
        },
      },
      indentWithTab,
      ...defaultKeymap,
    ]);

    const state = EditorState.create({
      doc: source,
      extensions: [
        saveBinding,
        basicSetup,
        oneDark,
        languageCompartment.current.of(languagePack),
        EditorView.updateListener.of((u) => {
          if (u.docChanged) {
            onChangeRef.current(u.state.doc.toString());
          }
        }),
        EditorView.theme({
          "&": { height: "100%", fontSize: "13px" },
          ".cm-scroller": { fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace" },
        }),
      ],
    });
    const view = new EditorView({ state, parent: containerRef.current });
    viewRef.current = view;
    return () => {
      view.destroy();
      viewRef.current = null;
    };
    // Re-create when the file path changes (switching files). Source
    // is seeded on mount; subsequent source changes are handled by
    // the second effect below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path]);

  // Replace document content when the source prop changes externally
  // (file reload from disk) without losing editor state if the new
  // value equals the current one (no-op).
  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (current === source) return;
    view.dispatch({
      changes: { from: 0, to: current.length, insert: source },
    });
  }, [source]);

  return (
    <div
      ref={containerRef}
      className="flex-1 min-h-0 overflow-hidden rounded border"
      style={{ borderColor: "var(--border)" }}
    />
  );
}
