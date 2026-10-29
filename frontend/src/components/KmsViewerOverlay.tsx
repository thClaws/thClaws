import { useEffect, useMemo, useRef, useState } from "react";
import { CtxMenuItem } from "./CtxMenuItem";
import { X, ArrowLeft, Loader2, Pencil, Save } from "lucide-react";
import { KmsTrustStrip } from "./KmsTrustStrip";
import { parsePageMeta } from "./kmsPageMeta";
import { KmsRunConfirm } from "./KmsRunConfirm";
import { type ModeCost } from "./kmsRunCost";
import { marked } from "marked";
import { send, subscribe } from "../hooks/useIPC";
import { assetUrl } from "../lib/assetUrl";
import { sanitizeHtml } from "../lib/sanitizeHtml";
import { MarkdownEditor } from "./MarkdownEditor";
import type { ViewerTarget } from "./KmsBrowserSidebar";

/// M6.39.9: KMS viewer pane. Renders a KMS file as HTML inside the
/// main content area — replaces the active tab visually, but tabs
/// stay mounted so xterm/etc don't lose state. Mounted as an
/// `absolute inset-0` sibling inside the main-pane container; close
/// returns the user to whichever tab they were on.
///
/// Markdown → HTML via `marked` (already a dep, used by
/// MarkdownEditor / InstructionsEditorModal too). Click handlers
/// rewrite links so:
///   - `[[<run-prefix>__<slug>]]` Obsidian wikilinks → load that
///     page in the same pane
///   - relative markdown links `[..](../sources/foo.md)` and
///     `[..](other-page.md)` → load that page/source in the pane
///   - http(s) links → open in external browser via `open_external`
///     IPC (delegates to the OS default browser; doesn't navigate
///     the wry webview which is single-document)
///
/// Keeps a small back-stack so the user can step backward through
/// linked pages. ESC + the X button close the pane; ArrowLeft in
/// the title bar pops the back-stack one entry.

marked.setOptions({ gfm: true, breaks: false, async: false });

interface Props {
  initial: ViewerTarget;
  onClose: () => void;
}

/// One citation marker's evidence, as `kms_citations_result` sends it.
/// `source`, `title` and `url` are exact; `claim` and `quote` are the
/// closest thing that source says to the sentence the marker sits in,
/// and are absent when nothing in it is close enough.
type CitationEvidence = {
  index: number;
  source: string;
  title?: string;
  url?: string;
  claim?: string;
  quote?: string;
  confidence?: number;
};

export function KmsViewerOverlay({ initial, onClose }: Props) {
  const [stack, setStack] = useState<ViewerTarget[]>([initial]);
  const [content, setContent] = useState<string | null>(null);
  // Absolute dir of the current file, from `kms_file_content`. Used to
  // resolve relative markdown image links to `/file-asset` URLs so KMS
  // source images render here the same way they do in the Files tab.
  const [assetBase, setAssetBase] = useState<string>("");
  // Pages that link here, computed by the engine on every read. A note
  // is only navigable both ways if it can show who points at it.
  const [backlinks, setBacklinks] = useState<{ slug: string; title: string }[]>([]);
  /// What each `[n]` in this page stands on, in the order the markers
  /// appear (dev-plan/64 P5.1). A citation names a source, not a
  /// sentence in it, so until now checking one meant opening the
  /// archive and searching it by hand.
  const [citations, setCitations] = useState<CitationEvidence[]>([]);
  // The engine sends only the head of a very large page, with a notice
  // prepended. Saving that back would replace the file with its first
  // 256 KB plus the notice, so such a page is read-only here.
  const [truncated, setTruncated] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  // Edit mode (pages only): YAML frontmatter edited in a modal, the
  // markdown body in a TipTap editor. `content` holds the original
  // until a successful save re-fetches it.
  const [editing, setEditing] = useState(false);
  const [editYaml, setEditYaml] = useState("");
  const [editBody, setEditBody] = useState("");
  const [showFm, setShowFm] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  // Right-click on selected text (pages, preview mode): "Create page"
  // researches the phrase into a new note linked from the selection.
  const [selMenu, setSelMenu] = useState<{ x: number; y: number; text: string } | null>(null);
  /// dev-plan/64 P5.4: nothing that spends money starts on one click.
  /// `summary` is the default because it is the cheap, reversible one —
  /// the menu offered both as equal peers and the expensive one was the
  /// same single click away.
  const [modes, setModes] = useState<Record<string, ModeCost>>({});
  const [confirmCreate, setConfirmCreate] = useState<{
    text: string;
    mode: "summary" | "atomic";
  } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const current = stack[stack.length - 1];

  // Reset stack when `initial` changes (parent opens a different file).
  // Clear `content` in the same effect so the viewer shows the spinner
  // on the very next render — otherwise the old file's HTML flashes
  // briefly under the new title before the fetch effect clears it.
  useEffect(() => {
    // Same-effect clearing is deliberate — see the note above: doing it
    // later flashes the old file's HTML under the new title.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setStack([initial]);
    setContent(null);
    setError(null);
    setEditing(false);
    setShowFm(false);
    setSaveError(null);
  }, [initial.kms, initial.kind, initial.name]);

  // Fetch content for the top-of-stack file.
  useEffect(() => {
    // Clear first so the spinner shows instead of the previous file.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setContent(null);
    setError(null);
    setBacklinks([]);
    setCitations([]);
    setTruncated(false);
    const unsub = subscribe((msg) => {
      if (
        msg.type === "kms_file_content" &&
        msg.kms === current.kms &&
        msg.kind === current.kind &&
        msg.name === current.name
      ) {
        if (msg.ok) {
          setContent(msg.content as string);
          setTruncated(msg.truncated === true);
          setAssetBase(String(msg.asset_base ?? ""));
          setBacklinks(
            Array.isArray(msg.backlinks)
              ? (msg.backlinks as { slug: string; title: string }[])
              : [],
          );
        } else {
          setError((msg.error as string) ?? "read failed");
        }
      } else if (
        msg.type === "kms_cost_result" &&
        msg.kms === current.kms
      ) {
        setModes((msg.modes as Record<string, ModeCost>) ?? {});
      } else if (
        msg.type === "kms_citations_result" &&
        msg.kms === current.kms &&
        msg.page === current.name
      ) {
        setCitations((msg.citations as CitationEvidence[]) ?? []);
      }
    });
    send({
      type: "kms_read_file",
      kms: current.kms,
      kind: current.kind,
      name: current.name,
    });
    if (current.kind === "page") {
      send({ type: "kms_citations", kms: current.kms, page: current.name });
      send({ type: "kms_cost", name: current.kms });
    }
    return unsub;
  }, [current.kms, current.kind, current.name]);

  // ESC: close the frontmatter modal first, then exit edit mode
  // (discarding unsaved edits), then close the overlay. Avoids an
  // accidental overlay-close losing in-progress edits.
  useEffect(() => {
    const close = () => setSelMenu(null);
    document.addEventListener("click", close);
    return () => document.removeEventListener("click", close);
  }, []);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type !== "kms_create_page_result" || msg.kms !== current.kms) return;
      if (msg.ok) {
        const slug = String(msg.slug ?? "");
        const linkNote = msg.linked ? "" : " (phrase not found in plain prose — link not inserted)";
        setNotice(
          msg.existed
            ? `Linked to existing page ${slug}${linkNote}`
            : `Created ${slug} — research running, see the Research sidebar${linkNote}`,
        );
        if (msg.page === current.name) {
          setContent(null);
          send({ type: "kms_read_file", kms: current.kms, kind: current.kind, name: current.name });
        }
      } else {
        setNotice(`Create page failed: ${String(msg.error ?? "unknown error")}`);
      }
      setTimeout(() => setNotice(null), 6000);
    });
    return unsub;
  }, [current.kms, current.kind, current.name]);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (selMenu) {
        setSelMenu(null);
        return;
      }
      if (showFm) setShowFm(false);
      else if (editing) setEditing(false);
      else onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [onClose, showFm, editing, selMenu]);

  // Save-result round-trip: on success, exit edit mode and re-fetch
  // (the fetch effect's subscription is still mounted — deps unchanged
  // during edit — so the re-sent kms_read_file refreshes `content`).
  useEffect(() => {
    if (!editing) return;
    const unsub = subscribe((msg) => {
      if (
        msg.type === "kms_write_page_result" &&
        msg.kms === current.kms &&
        msg.name === current.name
      ) {
        setSaving(false);
        if (msg.ok) {
          setEditing(false);
          setShowFm(false);
          setContent(null);
          send({
            type: "kms_read_file",
            kms: current.kms,
            kind: current.kind,
            name: current.name,
          });
        } else {
          setSaveError((msg.error as string) ?? "save failed");
        }
      }
    });
    return unsub;
  }, [editing, current.kms, current.kind, current.name]);

  // "Mark reviewed" (dev-plan/64 P5.2): stamp `reviewed: <today>` and write
  // the page back. Its own round-trip, because the save subscription above
  // only listens while the editor is open.
  const [marking, setMarking] = useState(false);
  useEffect(() => {
    if (!marking) return;
    const unsub = subscribe((msg) => {
      if (
        msg.type === "kms_write_page_result" &&
        msg.kms === current.kms &&
        msg.name === current.name
      ) {
        setMarking(false);
        if (msg.ok) {
          send({
            type: "kms_read_file",
            kms: current.kms,
            kind: current.kind,
            name: current.name,
          });
        } else {
          setError((msg.error as string) ?? "could not mark the page reviewed");
        }
      }
    });
    return unsub;
  }, [marking, current.kms, current.kind, current.name]);

  const markReviewed = () => {
    if (marking || content === null) return;
    const { yaml, body } = splitFrontmatter(content);
    const d = new Date();
    const today = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
    const line = `reviewed: ${today}`;
    const next = /^reviewed:.*$/m.test(yaml)
      ? yaml.replace(/^reviewed:.*$/m, line)
      : yaml.trim()
        ? `${yaml.replace(/\n+$/, "")}\n${line}`
        : line;
    setMarking(true);
    send({
      type: "kms_write_page",
      kms: current.kms,
      name: current.name,
      content: recombineFrontmatter(next, body),
    });
  };

  const startEdit = () => {
    const { yaml, body } = splitFrontmatter(content ?? "");
    setEditYaml(yaml);
    setEditBody(body);
    setSaveError(null);
    setEditing(true);
  };

  const saveEdit = () => {
    if (saving) return;
    setSaving(true);
    setSaveError(null);
    send({
      type: "kms_write_page",
      kms: current.kms,
      name: current.name,
      content: recombineFrontmatter(editYaml, editBody),
    });
  };

  const html = useMemo(() => {
    if (content === null) return "";
    // Sanitized last, after every rewrite: pages and web-ingested sources
    // are untrusted, and `marked` passes raw HTML straight through.
    return sanitizeHtml(
      markHighlight(
        rewriteImageSrcs(renderMarkdownToHtml(content, citations), assetBase),
        current.highlight,
      ),
    );
  }, [content, assetBase, citations, current.highlight]);

  // Scroll the marked passage into view once it is in the DOM. Runs on
  // `html` so a re-render (a republish, a back-and-forward) re-finds it
  // rather than leaving the reader at the top of the archive.
  useEffect(() => {
    if (!current.highlight) return;
    const el = containerRef.current?.querySelector("mark[data-kms-hl]");
    el?.scrollIntoView({ block: "center" });
  }, [html, current.highlight]);

  // Intercept clicks on rendered anchors. Resolve KMS-internal
  // targets (wikilinks, relative paths) into back-stack pushes;
  // delegate http(s) links to the OS browser.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const handler = (e: MouseEvent) => {
      const target = e.target as HTMLElement | null;
      const anchor = target?.closest("a") as HTMLAnchorElement | null;
      if (!anchor) return;
      const href = anchor.getAttribute("href");
      if (!href) return;
      e.preventDefault();
      // dev-plan/64 P5.1: a citation opens its source at the passage it
      // cites, not at the top of a 90 KB archive. The index is the one
      // the annotator stamped, so it cannot drift from the evidence.
      const cite = anchor.dataset.cite;
      if (cite !== undefined) {
        const c = citations[Number(cite)];
        if (c) {
          setStack((s) => [
            ...s,
            {
              kms: current.kms,
              kind: "source",
              name: c.source,
              highlight: c.quote || undefined,
            },
          ]);
          return;
        }
      }
      // External link: hand to OS browser. No wry navigation.
      if (/^https?:\/\//i.test(href)) {
        send({ type: "open_external", url: href });
        return;
      }
      // Wikilink converted to `wikilink:slug` href by our renderer.
      if (href.startsWith("wikilink:")) {
        // The renderer percent-encodes the slug, so it has to come back
        // out. An ASCII slug encodes to itself, which is why this was
        // invisible until a page was named in Thai: `[[มาตรฐานการครองชีพ]]`
        // asked for a page called `%E0%B8%A1...` and found nothing, while
        // the same page opened fine from the sidebar.
        const slug = decodeWikilink(href.slice("wikilink:".length));
        // Wikilinks in research output use the prefixed-filename form
        // already (rewriter applied at synth time). Treat as a page.
        setStack((s) => [...s, { kms: current.kms, kind: "page", name: slug }]);
        return;
      }
      // Relative markdown link → resolve relative to current file's
      // directory inside the KMS.
      if (href.endsWith(".md") || href.includes(".md#") || href.includes(".md?")) {
        const target = resolveRelativeLink(current, href);
        if (target) {
          setStack((s) => [...s, target]);
          return;
        }
      }
      // Other href shapes (anchor-only `#section`, mailto:, etc.) —
      // ignore the click; preventDefault stops the wry default but
      // we don't navigate anywhere either.
    };
    container.addEventListener("click", handler);
    return () => container.removeEventListener("click", handler);
  }, [current, html, citations]);

  const goBack = () => {
    setStack((s) => (s.length > 1 ? s.slice(0, -1) : s));
  };

  return (
    <div
      className="absolute inset-0 flex flex-col"
      style={{
        background: "var(--bg-primary)",
        zIndex: 30, // above the tabs, below modals (which use fixed z-50)
      }}
    >
      <div
        className="flex items-center justify-between px-4 py-2 border-b shrink-0"
        style={{
          borderColor: "var(--border)",
          background: "var(--bg-secondary)",
        }}
      >
        <div className="flex items-center gap-2 truncate">
          <button
            type="button"
            onClick={goBack}
            disabled={stack.length <= 1}
            className="p-1 rounded hover:bg-white/10"
            style={{
              color: "var(--text-secondary)",
              opacity: stack.length <= 1 ? 0.3 : 1,
              cursor: stack.length <= 1 ? "default" : "pointer",
            }}
            title="Back"
          >
            <ArrowLeft size={14} />
          </button>
          <span
            className="text-xs"
            style={{ color: "var(--text-secondary)" }}
          >
            {current.kms} / {current.kind}s /
          </span>
          <span
            className="text-sm font-semibold truncate"
            style={{ color: "var(--text-primary)" }}
          >
            {current.name}
          </span>
        </div>
        <div className="flex items-center gap-1 shrink-0">
          {editing ? (
            <>
              <button
                type="button"
                onClick={() => setShowFm(true)}
                className="px-2 py-1 rounded text-xs border hover:bg-white/10"
                style={{
                  color: "var(--text-secondary)",
                  borderColor: "var(--border)",
                }}
                title="Edit YAML frontmatter"
              >
                Frontmatter
              </button>
              <button
                type="button"
                onClick={saveEdit}
                disabled={saving}
                className="flex items-center gap-1 px-2 py-1 rounded text-xs font-medium"
                style={{
                  background: "var(--accent)",
                  color: "var(--accent-fg, #fff)",
                  opacity: saving ? 0.6 : 1,
                  cursor: saving ? "default" : "pointer",
                }}
                title="Save (writes the page)"
              >
                <Save size={12} />
                {saving ? "Saving…" : "Save"}
              </button>
              <button
                type="button"
                onClick={() => setEditing(false)}
                className="px-2 py-1 rounded text-xs border hover:bg-white/10"
                style={{
                  color: "var(--text-secondary)",
                  borderColor: "var(--border)",
                }}
                title="Discard edits (Esc)"
              >
                Cancel
              </button>
            </>
          ) : (
            <>
              {current.kind === "page" && content !== null && (
                <button
                  type="button"
                  onClick={startEdit}
                  disabled={truncated}
                  className="p-1 rounded hover:bg-white/10"
                  style={{
                    color: "var(--text-secondary)",
                    opacity: truncated ? 0.4 : 1,
                    cursor: truncated ? "not-allowed" : "pointer",
                  }}
                  title={
                    truncated
                      ? "This page is too large to edit here — only its first part is shown, and saving would cut off the rest. Edit the file directly."
                      : "Edit this page"
                  }
                >
                  <Pencil size={14} />
                </button>
              )}
              <button
                type="button"
                onClick={onClose}
                className="p-1 rounded hover:bg-white/10"
                style={{ color: "var(--text-secondary)" }}
                title="Close (Esc) — return to active tab"
              >
                <X size={14} />
              </button>
            </>
          )}
        </div>
      </div>

      <div
        ref={containerRef}
        className="flex-1 overflow-auto kms-viewer-prose"
        style={{ color: "var(--text-primary)" }}
        onContextMenu={(e) => {
          if (editing || current.kind !== "page") return;
          const text = (window.getSelection()?.toString() ?? "").replace(/\s+/g, " ").trim();
          if (text.length < 2 || text.length > 120) return;
          e.preventDefault();
          setSelMenu({ x: e.clientX, y: e.clientY, text });
        }}
      >
        {notice && (
          <div
            className="mx-auto max-w-4xl mt-2 px-3 py-1.5 rounded text-xs"
            style={{
              background: "var(--bg-secondary)",
              border: "1px solid var(--border)",
              color: "var(--text-primary)",
            }}
          >
            {notice}
          </div>
        )}
        {selMenu && (
          <div
            className="fixed z-50 rounded border shadow-lg py-1 text-xs"
            style={{
              left: selMenu.x,
              top: selMenu.y,
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
              minWidth: 200,
              maxWidth: 360,
            }}
            onClick={(e) => e.stopPropagation()}
            onContextMenu={(e) => e.preventDefault()}
          >
            <div
              className="px-3 py-0.5 truncate"
              style={{ color: "var(--text-secondary)", fontSize: "9px" }}
              title={selMenu.text}
            >
              “{selMenu.text}”
            </div>
            <CtxMenuItem
              onClick={() => {
                setConfirmCreate({ text: selMenu.text, mode: "summary" });
                setSelMenu(null);
              }}
            >
              Research this into a page…
            </CtxMenuItem>
            <CtxMenuItem muted onClick={() => setSelMenu(null)}>
              Cancel
            </CtxMenuItem>
          </div>
        )}
        {confirmCreate && (
          <KmsRunConfirm
            title="Research this into a page"
            noun="run like this"
            estimate={modes.selection}
            runLabel="Research it"
            scope={
              confirmCreate.mode === "summary"
                ? [
                    `Searches the web about “${confirmCreate.text}”.`,
                    "Writes ONE note and links this page's text to it.",
                    "Nothing already written here is replaced.",
                  ]
                : [
                    `Searches the web about “${confirmCreate.text}”.`,
                    "Writes a topic page AND one note per idea it finds — several pages, not one.",
                    "Nothing already written here is replaced.",
                  ]
            }
            onCancel={() => setConfirmCreate(null)}
            onRun={() => {
              const { text, mode } = confirmCreate;
              setConfirmCreate(null);
              setNotice(`Creating page for “${text}”…`);
              send({
                type: "kms_create_page_from_selection",
                kms: current.kms,
                page: current.name,
                text,
                mode,
              });
            }}
          >
            <div className="flex flex-col gap-1 text-xs">
              {(["summary", "atomic"] as const).map((m) => (
                <label key={m} className="flex items-start gap-2">
                  <input
                    type="radio"
                    name="kms-create-mode"
                    checked={confirmCreate.mode === m}
                    onChange={() => setConfirmCreate({ ...confirmCreate, mode: m })}
                    style={{ marginTop: 2 }}
                  />
                  <span>
                    {m === "summary" ? "One note" : "A topic page and a note per idea"}
                    <span style={{ color: "var(--text-secondary)" }}>
                      {m === "summary"
                        ? " — the cheap one, and the default"
                        : " — several runs' worth of writing"}
                    </span>
                  </span>
                </label>
              ))}
            </div>
          </KmsRunConfirm>
        )}
        <div className="max-w-4xl mx-auto px-4 sm:px-8 py-6">
          {error && (
            <div
              className="px-3 py-2 rounded"
              style={{
                background: "var(--bg-secondary)",
                color: "var(--danger, #e06c75)",
              }}
            >
              {error}
            </div>
          )}
          {content === null && !error && (
            <div
              className="px-3 py-2 italic text-sm flex items-center gap-2"
              style={{ color: "var(--text-secondary)" }}
            >
              <Loader2 size={14} className="animate-spin" />
              <span>Loading…</span>
            </div>
          )}
          {content !== null && !editing && (
            <>
              {current.kind === "page" && (
                <KmsTrustStrip
                  meta={parsePageMeta(splitFrontmatter(content).yaml)}
                  // A page that came back cut must never be written back.
                  onMarkReviewed={truncated ? undefined : markReviewed}
                  busy={marking}
                />
              )}
              <div dangerouslySetInnerHTML={{ __html: html }} />
              {backlinks.length > 0 && (
                <div
                  className="mt-8 pt-3"
                  style={{ borderTop: "1px solid var(--border)" }}
                >
                  <div
                    className="uppercase tracking-wider mb-1.5"
                    style={{ color: "var(--text-secondary)", fontSize: "10px" }}
                  >
                    Linked from ({backlinks.length})
                  </div>
                  <div className="flex flex-wrap gap-x-3 gap-y-1">
                    {backlinks.map((b) => (
                      <button
                        key={b.slug}
                        type="button"
                        className="text-left hover:underline"
                        style={{ color: "var(--accent)", fontSize: "12px" }}
                        title={b.slug}
                        onClick={() =>
                          setStack((s) => [
                            ...s,
                            { kms: current.kms, kind: "page", name: b.slug },
                          ])
                        }
                      >
                        {b.title}
                      </button>
                    ))}
                  </div>
                </div>
              )}
            </>
          )}
          {content !== null && editing && (
            <>
              {saveError && (
                <div
                  className="mb-3 px-3 py-2 rounded text-xs"
                  style={{
                    background: "var(--bg-secondary)",
                    color: "var(--danger, #e06c75)",
                  }}
                >
                  {saveError}
                </div>
              )}
              <MarkdownEditor source={editBody} onChange={setEditBody} baseDir={assetBase} />
            </>
          )}
        </div>
      </div>

      {showFm && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center"
          style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
          onClick={() => setShowFm(false)}
        >
          <div
            className="rounded-lg border shadow-xl w-[520px] max-w-[92vw] max-h-[90vh] flex flex-col"
            style={{
              background: "var(--bg-primary)",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
            onClick={(e) => e.stopPropagation()}
          >
            <div
              className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
              style={{ borderColor: "var(--border)" }}
            >
              <span style={{ color: "var(--accent)" }}>●</span>
              <span>YAML frontmatter · {current.name}</span>
            </div>
            <div className="px-4 py-3">
              <textarea
                value={editYaml}
                onChange={(e) => setEditYaml(e.target.value)}
                spellCheck={false}
                rows={12}
                className="w-full px-2 py-1.5 rounded border font-mono text-xs"
                style={{
                  background: "var(--bg-secondary)",
                  borderColor: "var(--border)",
                  color: "var(--text-primary)",
                  resize: "vertical",
                }}
                placeholder={"title: My page\ntopic: one-line description\ncategory: notes\ntags: a, b"}
              />
              <div
                className="mt-1.5 text-[10px]"
                style={{ color: "var(--text-secondary)" }}
              >
                Edits apply when you Save the page. `created:` /
                `updated:` are managed automatically.
              </div>
            </div>
            <div
              className="px-4 py-2.5 border-t flex justify-end"
              style={{ borderColor: "var(--border)" }}
            >
              <button
                type="button"
                onClick={() => setShowFm(false)}
                className="px-3 py-1.5 rounded text-xs font-medium"
                style={{
                  background: "var(--accent)",
                  color: "var(--accent-fg, #fff)",
                }}
              >
                Done
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/// Render Markdown → HTML, with two pre-processing passes for
/// KMS-specific syntax that vanilla `marked` doesn't understand:
///
/// 1. Strip the YAML frontmatter block at the very top (between
///    `---\n` and `\n---\n`) — no point rendering it as a code block.
/// 2. Drop the pre-P3.9 header remnant under the title.
/// 3. Convert Obsidian `[[slug]]` and `[[slug|display]]` wikilinks
///    into anchor tags with a custom `wikilink:` href scheme. The
///    overlay's click handler intercepts those and pushes a new
///    target onto the back-stack.
function renderMarkdownToHtml(
  markdown: string,
  citations: CitationEvidence[] = [],
): string {
  let body = stripFrontmatter(markdown);
  body = dropLegacyHeader(body);
  body = annotateCitations(body, citations);
  body = rewriteWikilinks(body);
  return marked.parse(body) as string;
}

/// dev-plan/64 P5.1: give every `[n](../sources/…)` a link title, so
/// hovering it says which source it is and what that source actually
/// says — instead of a bare number the reader has to take on faith.
///
/// Done on the markdown, not the rendered HTML, because the engine
/// counted the markers in this same text with the same pattern: the
/// nth match here is the nth entry of `citations`. A bare `[n]` with
/// no link is not a match on either side, so nothing slides.
///
/// Emitted as inline HTML rather than a markdown link so the anchor can
/// carry `data-cite`, which is what lets a click open the source at the
/// quote instead of at the top of a 90 KB archive.
const CITATION_MD_RE = /\[(\d{1,4})\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;

function annotateCitations(
  body: string,
  citations: CitationEvidence[],
): string {
  if (citations.length === 0) return body;
  let i = 0;
  return body.replace(CITATION_MD_RE, (whole, n: string, href: string) => {
    const at = i++;
    const c = citations[at];
    if (!c) return whole;
    const lines = [c.title || c.source];
    if (c.quote) {
      lines.push(`“${c.quote}”`);
      // The model's own certainty about the extraction. Shown because
      // a quote at 0.4 and a quote at 0.95 are not the same evidence.
      if (typeof c.confidence === "number") {
        lines.push(`confidence ${c.confidence.toFixed(2)}`);
      }
      lines.push("Click to open the source at this passage.");
    } else {
      // Never invent a match. The citation still names its source.
      lines.push("(nothing in this source is close enough to quote here)");
    }
    return `<a href="${esc(href)}" title="${esc(lines.join("\n"))}" data-cite="${at}">${n}</a>`;
  });
}

/// Wrap the first occurrence of `quote` in the rendered source in a
/// `<mark>` the scroll effect can find (dev-plan/64 P5.1).
///
/// Works on the rendered HTML and only outside tags, so a quote that
/// happens to contain a word from a `href` cannot corrupt the markup.
/// Whitespace is matched loosely: the digest checks its quotes against
/// a whitespace-stripped archive, so a quote can be real and still
/// differ from the file by a line break. On the owner's vault, 96 % of
/// the quotes offered are in their archive verbatim and 100 % are
/// there once whitespace is ignored. A quote that still is not found
/// is simply not marked, and the reader gets the source from the top.
function markHighlight(html: string, quote?: string): string {
  const needle = quote?.trim();
  if (!needle) return html;
  const re = new RegExp(
    needle
      .split(/\s+/)
      .map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
      .join("\\s+"),
  );
  // Text runs between tags, so the search never crosses `<`…`>`.
  let out = "";
  let i = 0;
  let done = false;
  while (i < html.length) {
    const lt = html.indexOf("<", i);
    const end = lt === -1 ? html.length : lt;
    const text = html.slice(i, end);
    const m = done ? null : re.exec(text);
    if (m) {
      out +=
        text.slice(0, m.index) +
        `<mark data-kms-hl style="background:var(--warning,#e5c07b);color:#000">` +
        m[0] +
        "</mark>" +
        text.slice(m.index + m[0].length);
      done = true;
    } else {
      out += text;
    }
    if (lt === -1) break;
    const gt = html.indexOf(">", lt);
    if (gt === -1) {
      out += html.slice(lt);
      break;
    }
    out += html.slice(lt, gt + 1);
    i = gt + 1;
  }
  return out;
}

/// Escape for an HTML attribute value.
function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/// Pages written before dev-plan/64 P3.9 carry two lines under the
/// title that were never meant to be read: a `Description:` line
/// repeating the frontmatter's `topic:`, and a bare `---`. The rule
/// renders as a second horizontal line directly under the heading's
/// own underline, with an empty band between them. `write_page` no
/// longer emits either, and takes them off the next time the page is
/// written — until then, don't render them. Display-only, like
/// `rewriteImageSrcs`: `content` stays pristine so edit + save never
/// persist a view of the page that isn't what is on disk.
function dropLegacyHeader(body: string): string {
  const lines = body.split("\n");
  let i = 0;
  while (i < lines.length && lines[i].trim() === "") i++;
  if (i >= lines.length || !lines[i].startsWith("# ")) return body;
  // Blank lines between the title and the remnant are crossed — on a
  // refreshed page the rule sits a line below the title — but the scan
  // stops at the first real content, so a rule anywhere else survives.
  let j = i + 1;
  let sawLegacy = false;
  while (j < lines.length) {
    const t = lines[j].trim();
    if (t === "") {
      j++;
    } else if (t === "---" || t.startsWith("Description:")) {
      sawLegacy = true;
      j++;
    } else {
      break;
    }
  }
  if (!sawLegacy) return body;
  lines.splice(i + 1, j - i - 1, "");
  return lines.join("\n");
}

/// Rewrite relative `<img src>` in rendered KMS HTML to `/file-asset`
/// URLs rooted at the file's directory (`assetBase`), so images ingested
/// alongside a markdown source (`sources/<alias>-assets/…`) load in the
/// viewer. Remote (`http(s):`/`data:`/`blob:`), already-absolute, and
/// already-resolved (`thclaws:`) srcs are left alone. Runs on the
/// rendered HTML only — the underlying `content` stays pristine so edit +
/// save never persist these display URLs back into the source.
function rewriteImageSrcs(html: string, assetBase: string): string {
  if (!assetBase) return html;
  const base = assetBase.replace(/\/+$/, "");
  return html.replace(
    /(<img\b[^>]*?\ssrc=)("|')(.*?)\2/gi,
    (match, pre: string, quote: string, src: string) => {
      const s = src.trim();
      if (!s || /^(https?:|data:|blob:|thclaws:|\/\/)/i.test(s) || s.startsWith("/")) {
        return match;
      }
      const rel = s.replace(/^\.\//, "");
      return `${pre}${quote}${assetUrl(`${base}/${rel}`)}${quote}`;
    },
  );
}

function stripFrontmatter(s: string): string {
  if (!s.startsWith("---\n")) return s;
  const end = s.indexOf("\n---\n", 4);
  if (end < 0) return s;
  return s.slice(end + "\n---\n".length).trimStart();
}

/// Split raw page content into its YAML frontmatter (the text between
/// the `---` fences, without them) and the markdown body. Pages with no
/// frontmatter return an empty yaml + the whole string as body.
function splitFrontmatter(raw: string): { yaml: string; body: string } {
  if (!raw.startsWith("---\n")) return { yaml: "", body: raw };
  const end = raw.indexOf("\n---\n", 4);
  if (end < 0) return { yaml: "", body: raw };
  const yaml = raw.slice(4, end);
  const body = raw.slice(end + "\n---\n".length).replace(/^\n+/, "");
  return { yaml, body };
}

/// Recombine edited YAML frontmatter + body back into page content for
/// `write_page`. Empty/blank YAML → body only (write_page will stamp a
/// minimal frontmatter). Always ends with a trailing newline.
function recombineFrontmatter(yaml: string, body: string): string {
  const y = yaml.trim();
  const b = body.endsWith("\n") ? body : `${body}\n`;
  return y ? `---\n${y}\n---\n\n${b}` : b;
}

function rewriteWikilinks(s: string): string {
  // Convert `[[slug]]` → `[slug](wikilink:slug)`
  //         `[[slug|display]]` → `[display](wikilink:slug)`
  // Markdown then lets `marked` render these as ordinary anchors.
  // Keep it simple — the rewriter runs BEFORE marked so we just
  // emit markdown link syntax.
  let out = "";
  let i = 0;
  while (i < s.length) {
    if (i + 1 < s.length && s[i] === "[" && s[i + 1] === "[") {
      const end = s.indexOf("]]", i + 2);
      if (end > 0 && end - i - 2 <= 200) {
        const inner = s.slice(i + 2, end);
        if (!inner.includes("\n")) {
          const pipe = inner.indexOf("|");
          const slug = pipe >= 0 ? inner.slice(0, pipe).trim() : inner.trim();
          const display =
            pipe >= 0 ? inner.slice(pipe + 1).trim() : inner.trim();
          if (slug.length > 0) {
            out += `[${escapeMd(display)}](wikilink:${encodeURIComponent(slug)})`;
            i = end + 2;
            continue;
          }
        }
      }
    }
    out += s[i];
    i++;
  }
  return out;
}

function escapeMd(s: string): string {
  return s.replace(/([\\[\]])/g, "\\$1");
}

/// Undo the `encodeURIComponent` the wikilink renderer applies to a slug.
///
/// A page name is free text — it can hold a literal `%` that was never an
/// escape — so a malformed sequence must give back what was there rather
/// than throw and swallow the click.
function decodeWikilink(raw: string): string {
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/// Resolve a relative markdown link from the perspective of the
/// currently-viewed file. Pages live at `<kms>/pages/`, sources at
/// `<kms>/sources/`. Common shapes our pipeline emits:
///   `[[slug]]` → handled separately as `wikilink:` scheme
///   `[T](../sources/<slug>.md)` from a page → resolves to `source`
///   `[T](other-page.md)` from a page → resolves to `page`
///   `[T](../pages/<slug>.md)` from a source → resolves to `page`
function resolveRelativeLink(
  current: ViewerTarget,
  href: string,
): ViewerTarget | null {
  // Strip query / fragment.
  const path = href.split("#")[0].split("?")[0];
  // Always lowercase the kind segment for matching.
  // The name is decoded only after the separator checks below, so a
  // `%2F` cannot slip a path separator past them by arriving encoded.
  // A Thai page or source is percent-encoded in the rendered href and
  // has to be decoded, or it names a file that does not exist.
  if (path.startsWith("../sources/") && path.endsWith(".md")) {
    const name = path.slice("../sources/".length, -3);
    if (!name.includes("/")) {
      return { kms: current.kms, kind: "source", name: decodeWikilink(name) };
    }
  }
  if (path.startsWith("../pages/") && path.endsWith(".md")) {
    const name = path.slice("../pages/".length, -3);
    if (!name.includes("/")) {
      return { kms: current.kms, kind: "page", name: decodeWikilink(name) };
    }
  }
  // Bare filename `<slug>.md` — resolve as same-kind sibling.
  if (path.endsWith(".md") && !path.includes("/")) {
    const name = path.slice(0, -3);
    return { kms: current.kms, kind: current.kind, name: decodeWikilink(name) };
  }
  return null;
}
