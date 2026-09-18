import { AlertTriangle, Plus, Settings2 } from "lucide-react";

// dev-plan/59 §6.3: the only part of the UI that knows bots exist.
//
// Everything to the right of this rail is the app as it was — one bot's
// tabs, one bot's chat. The rail lists bots in `bots.json` order (not
// alphabetically: installing `alpha` must not silently take `main`'s place)
// and carries the two signals a background bot is allowed to raise — a dot
// when it has finished something, and a warning when it is not running.

export type BotStatus = {
  slug: string;
  name: string;
  state: string;
  busy: boolean;
  unread: boolean;
};

/** Two letters is enough to tell bots apart at rail width. */
function initials(name: string): string {
  const parts = name.split(/[-_\s]+/).filter(Boolean);
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
  return name.slice(0, 2).toUpperCase();
}

export function BotRail({
  bots,
  active,
  onSelect,
  onOpenHost,
}: {
  bots: BotStatus[];
  active: string | null;
  onSelect: (slug: string) => void;
  onOpenHost: () => void;
}) {
  return (
    <div
      className="fixed left-0 top-0 bottom-0 z-40 flex flex-col items-center gap-1 py-2 px-1 border-r border-[var(--border)] bg-[var(--bg-secondary)]"
      style={{ width: "var(--rail-w)" }}
    >
      <span className="text-[9px] text-[var(--text-secondary)]">BOTS</span>
      {bots.map((b) => {
        const isActive = b.slug === active;
        const broken = b.state === "crash_looped";
        return (
          <button
            key={b.slug}
            onClick={() => onSelect(b.slug)}
            title={`Bot: ${b.name} — ${b.state}`}
            aria-label={`Bot: ${b.name}`}
            aria-current={isActive ? "true" : undefined}
            className={
              "relative w-9 h-9 rounded-lg text-[11px] font-semibold transition-colors " +
              (isActive
                ? "bg-[var(--accent)] text-[var(--accent-fg)]"
                : "text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)]")
            }
          >
            {initials(b.name)}
            {broken ? (
              <AlertTriangle
                size={10}
                className="absolute -top-0.5 -right-0.5 text-[var(--warning)]"
              />
            ) : (b.unread || b.busy) && !isActive ? (
              // One dot, two meanings, deliberately: solid = finished and
              // unread, pulsing = still working. A background bot gets no
              // louder signal than this — except an approval, which the
              // shell raises as a banner because a dot cannot say "blocked".
              <span
                className={
                  "absolute top-0.5 right-0.5 w-2 h-2 rounded-full bg-[var(--accent)] " +
                  (b.busy && !b.unread ? "animate-pulse" : "")
                }
              />
            ) : null}
          </button>
        );
      })}
      <div className="flex-1" />
      <button
        onClick={onOpenHost}
        title="Workspace — add, remove or restart bots"
        aria-label="Manage bots"
        className="w-9 h-9 rounded-lg text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] flex items-center justify-center"
      >
        <Settings2 size={16} />
      </button>
      <button
        onClick={onOpenHost}
        title="Add bot"
        aria-label="Add bot"
        className="w-9 h-9 rounded-lg text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] flex items-center justify-center"
      >
        <Plus size={16} />
      </button>
    </div>
  );
}
