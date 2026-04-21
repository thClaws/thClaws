import { useEditor, EditorContent } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import { Markdown } from "tiptap-markdown";
import { useEffect, useRef } from "react";

interface Props {
  source: string;
  onChange: (markdown: string) => void;
}

// WYSIWYG markdown editor built on TipTap. We keep `tiptap-markdown`
// for round-trip — the editor state is a ProseMirror document, but
// `storage.markdown.getMarkdown()` serialises back to GFM-compatible
// markdown on every change. StarterKit covers headings, bold/italic,
// code, code-block, lists, blockquotes, horizontal-rule, history.
export function MarkdownEditor({ source, onChange }: Props) {
  // Track the last value we pushed through `onChange` so we can tell
  // whether an incoming `source` prop is the user's own edit echoed
  // back by the parent (skip) or an external change that should
  // replace editor content (like switching files).
  const lastEmittedRef = useRef<string | null>(null);

  const editor = useEditor({
    extensions: [
      StarterKit.configure({ codeBlock: {} }),
      Markdown.configure({
        html: false,
        tightLists: true,
        linkify: true,
        breaks: false,
        transformPastedText: true,
        transformCopiedText: true,
      }),
    ],
    content: source,
    onUpdate: ({ editor }) => {
      // tiptap-markdown attaches a `markdown` extension-storage slot.
      // The runtime property exists; its type isn't declared via
      // module augmentation, so we narrow via unknown → structural.
      const md = (
        editor.storage as unknown as {
          markdown?: { getMarkdown: () => string };
        }
      ).markdown?.getMarkdown();
      if (typeof md === "string") {
        lastEmittedRef.current = md;
        onChange(md);
      }
    },
    editorProps: {
      attributes: {
        class:
          "tiptap-prose prose prose-sm prose-invert max-w-none focus:outline-none px-4 py-3",
        spellcheck: "false",
      },
    },
  });

  // Replace content when `source` changes and it's not the value we
  // just emitted. Prevents caret-jump on every keystroke.
  useEffect(() => {
    if (!editor) return;
    if (lastEmittedRef.current === source) return;
    editor.commands.setContent(source, { emitUpdate: false });
    lastEmittedRef.current = source;
  }, [source, editor]);

  return (
    <div
      className="flex-1 min-h-0 overflow-auto rounded border"
      style={{
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      <EditorContent editor={editor} className="h-full" />
    </div>
  );
}
