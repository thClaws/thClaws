import { useEffect, useState } from "react";
import { assetUrl } from "../lib/assetUrl";

// assetUrl already carries `?bot=<slug>` in host mode; a second `?` folds the
// version into the bot name and every slide 404s.
function withVersion(url: string, version: number | string): string {
  return `${url}${url.includes("?") ? "&" : "?"}v=${version}`;
}

// Deck preview for the Files tab: shows ONE slide at a time with
// ←/→ navigation, instead of the browser PDF viewer's endless scroll —
// a `.pptx` is a deck, not a long document. The backend already writes
// per-slide PNGs (`slide-01.png`, …) beside the rendered `deck.pdf`
// when it converts a deck, so this just pages through them; the PDF
// iframe stays available behind the footer toggle for printing and
// text selection.
type Props = {
  // Workspace-relative PNG paths, in slide order.
  slides: string[];
  // Bumped by the pane's Refresh so a re-render busts the image cache.
  version: number;
  // Rendered underneath the slide, e.g. "deck.pptx".
  label?: string;
  // Footer switch back to the PDF iframe.
  onShowPdf?: () => void;
};

export function SlideDeckViewer({ slides, version, label, onShowPdf }: Props) {
  const [index, setIndex] = useState(0);
  const count = slides.length;
  // Clamped during render, not in an effect: a re-render can shorten
  // the deck (edited while open) and the index must never point past
  // the end. A different file remounts the component (keyed on path),
  // so there's nothing to reset.
  const at = Math.min(index, Math.max(0, count - 1));

  // Warm the neighbours so stepping through doesn't flash white on
  // each large PNG. Keyed on the neighbour PATHS, not the array — the
  // Files tab re-sends the same list on every poll tick.
  const prevPath = at > 0 ? slides[at - 1] : null;
  const nextPath = at + 1 < count ? slides[at + 1] : null;
  useEffect(() => {
    for (const p of [nextPath, prevPath]) {
      if (!p) continue;
      const img = new Image();
      img.src = withVersion(assetUrl(p), version);
    }
  }, [prevPath, nextPath, version]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Don't steal arrows from the editor, the path filter, or any
      // other text entry sharing the tab.
      const t = e.target as HTMLElement | null;
      if (
        t &&
        (t.isContentEditable ||
          ["INPUT", "TEXTAREA", "SELECT"].includes(t.tagName))
      ) {
        return;
      }
      if (e.key === "ArrowRight" || e.key === "PageDown") {
        setIndex(Math.min(count - 1, at + 1));
      } else if (e.key === "ArrowLeft" || e.key === "PageUp") {
        setIndex(Math.max(0, at - 1));
      } else if (e.key === "Home") {
        setIndex(0);
      } else if (e.key === "End") {
        setIndex(Math.max(0, count - 1));
      } else {
        return;
      }
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [at, count]);

  if (count === 0) return null;
  const atFirst = at === 0;
  const atLast = at >= count - 1;

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <div
        className="relative flex-1 min-h-0 flex items-center justify-center rounded border overflow-hidden"
        style={{ borderColor: "var(--border)", background: "var(--bg-secondary)" }}
      >
        <img
          src={withVersion(assetUrl(slides[at]), version)}
          alt={`Slide ${at + 1} of ${count}`}
          className="max-w-full max-h-full object-contain"
          draggable={false}
        />
        {/* Click targets over the left/right thirds — the way every
            other deck viewer behaves. Kept transparent so they don't
            dim the slide. */}
        <button
          onClick={() => setIndex(Math.max(0, at - 1))}
          disabled={atFirst}
          aria-label="Previous slide"
          className="absolute left-0 top-0 h-full w-[15%] flex items-center justify-start pl-2 opacity-0 hover:opacity-100 disabled:pointer-events-none transition-opacity"
        >
          <span
            className="px-2 py-3 rounded text-lg"
            style={{ background: "rgba(0,0,0,0.45)", color: "#fff" }}
          >
            ‹
          </span>
        </button>
        <button
          onClick={() => setIndex(Math.min(count - 1, at + 1))}
          disabled={atLast}
          aria-label="Next slide"
          className="absolute right-0 top-0 h-full w-[15%] flex items-center justify-end pr-2 opacity-0 hover:opacity-100 disabled:pointer-events-none transition-opacity"
        >
          <span
            className="px-2 py-3 rounded text-lg"
            style={{ background: "rgba(0,0,0,0.45)", color: "#fff" }}
          >
            ›
          </span>
        </button>
      </div>
      <div
        className="flex items-center justify-between gap-2 px-2 py-1 text-[11px] font-mono shrink-0"
        style={{ color: "var(--text-secondary)" }}
      >
        <button
          onClick={() => setIndex(Math.max(0, at - 1))}
          disabled={atFirst}
          className="px-2 py-0.5 rounded hover:bg-white/10 disabled:opacity-40 shrink-0"
          title="Previous slide (←)"
        >
          ‹ Prev
        </button>
        <span className="truncate flex-1 text-center" title={label}>
          {at + 1} / {count}
          {label ? ` · ${label}` : ""}
        </span>
        <div className="flex items-center gap-1 shrink-0">
          {onShowPdf && (
            <button
              onClick={onShowPdf}
              className="px-2 py-0.5 rounded hover:bg-white/10"
              title="Show the rendered PDF instead"
            >
              PDF
            </button>
          )}
          <button
            onClick={() => setIndex(Math.min(count - 1, at + 1))}
            disabled={atLast}
            className="px-2 py-0.5 rounded hover:bg-white/10 disabled:opacity-40"
            title="Next slide (→)"
          >
            Next ›
          </button>
        </div>
      </div>
    </div>
  );
}
