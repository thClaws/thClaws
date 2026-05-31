import { useEffect, useState } from "react";
import { RefreshCw, Sparkles } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";

// dev-plan/33 Tier 2 — picker modal listing every installed shell:
// built-ins (embedded), user (~/.config/thclaws/gui-shell/), and
// project (./.thclaws/gui-shell/). Click a card → opens that shell
// in the parent Shell tab.

export interface ShellInfo {
  id: string;
  name: string;
  version: string;
  description: string;
  icon: string | null;
  source: "builtin" | "user" | "project";
  permissions: string[];
}

interface ShellPickerProps {
  onSelect: (shellId: string) => void;
  /**
   * When true (the default), the picker honours `settings.json::guiShell
   * .tabDefault` from the first list reply and immediately calls
   * onSelect with that id, skipping the grid. Set false to force the
   * grid (used by the breadcrumb "shells" button — once the user has
   * gone back to the picker, they explicitly want the grid even if a
   * tabDefault is set).
   */
  honourDefault?: boolean;
}

let nextRequestId = 1;

export function ShellPicker({ onSelect, honourDefault = true }: ShellPickerProps) {
  const [shells, setShells] = useState<ShellInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const requestList = () => {
    const id = nextRequestId++;
    setShells(null);
    setError(null);
    send({ type: "gui_shell_list", id });
    // Safety timeout so the user isn't stuck on "loading…" if the
    // backend never replies (broken IPC bridge, etc.).
    setTimeout(() => {
      setShells((current) => {
        if (current === null) {
          setError("Timed out waiting for shell list.");
          return [];
        }
        return current;
      });
    }, 5000);
  };

  useEffect(() => {
    const unsub = subscribe((msg: any) => {
      if (msg?.type !== "gui_shell_list_result") return;
      if (!Array.isArray(msg.shells)) return;
      setShells(msg.shells as ShellInfo[]);
      // settings.json::guiShell tabDefault — auto-open without showing
      // the grid when the user has pinned a preferred shell. Only fires
      // when the resolved id matches a shell that actually exists in
      // the registry (avoids navigating to a stale config entry).
      if (honourDefault && typeof msg.tabDefault === "string" && msg.tabDefault) {
        const exists = (msg.shells as ShellInfo[]).some((s) => s.id === msg.tabDefault);
        if (exists) onSelect(msg.tabDefault);
      }
    });
    requestList();
    return unsub;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="w-full h-full overflow-y-auto p-6"
         style={{ background: "var(--bg-primary)", color: "var(--text-primary)" }}>
      <div className="flex items-center justify-between mb-4">
        <div>
          <h2 className="text-base font-semibold flex items-center gap-2">
            <Sparkles size={16} /> GUI Shell
          </h2>
          <p className="text-xs mt-1" style={{ color: "var(--text-secondary)" }}>
            Pick a domain-specific frontend for this tab. Drop a folder in
            <code className="mx-1 px-1 rounded" style={{ background: "var(--bg-secondary)" }}>
              ~/.config/thclaws/gui-shell/
            </code>
            or
            <code className="mx-1 px-1 rounded" style={{ background: "var(--bg-secondary)" }}>
              ./.thclaws/gui-shell/
            </code>
            and click Refresh.
          </p>
        </div>
        <button
          onClick={requestList}
          className="flex items-center gap-1.5 text-xs px-2.5 py-1.5 rounded border"
          style={{
            background: "var(--bg-secondary)",
            borderColor: "var(--border)",
            color: "var(--text-primary)",
          }}
          title="Rescan ~/.config/thclaws/gui-shell/ and ./.thclaws/gui-shell/"
        >
          <RefreshCw size={12} /> Refresh
        </button>
      </div>

      {error && (
        <div className="text-xs p-3 mb-3 rounded border"
             style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}>
          {error}
        </div>
      )}

      {shells === null && (
        <div className="text-xs py-12 text-center" style={{ color: "var(--text-secondary)" }}>
          loading shells…
        </div>
      )}

      {shells !== null && shells.length === 0 && (
        <div className="text-xs py-12 text-center" style={{ color: "var(--text-secondary)" }}>
          No shells found. (You should at least see the built-in Session
          Explorer — if not, IPC may be broken.)
        </div>
      )}

      {shells !== null && shells.length > 0 && (
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
          {shells.map((s) => (
            <ShellCard key={`${s.source}:${s.id}`} shell={s} onSelect={onSelect} />
          ))}
        </div>
      )}
    </div>
  );
}

function ShellCard({ shell, onSelect }: { shell: ShellInfo; onSelect: (id: string) => void }) {
  const badge = sourceBadge(shell.source);
  const iconUrl = shell.icon
    ? `thclaws://localhost/gui-shell/${encodeURIComponent(shell.id)}/${shell.icon}`
    : null;
  return (
    <button
      onClick={() => onSelect(shell.id)}
      className="text-left p-3 rounded border hover:shadow transition-shadow"
      style={{
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
        color: "var(--text-primary)",
      }}
    >
      <div className="flex items-start gap-2">
        {iconUrl ? (
          <img
            src={iconUrl}
            alt=""
            width={28}
            height={28}
            className="flex-shrink-0 mt-0.5"
            style={{ filter: "var(--icon-filter, none)" }}
            onError={(e) => { (e.target as HTMLImageElement).style.display = "none"; }}
          />
        ) : (
          <Sparkles size={20} className="flex-shrink-0 mt-1" />
        )}
        <div className="flex-1 min-w-0">
          <div className="flex items-baseline gap-2">
            <span className="font-medium text-sm truncate">{shell.name}</span>
            <span className="text-[10px] opacity-60 font-mono">v{shell.version}</span>
          </div>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              navigator.clipboard?.writeText(`--gui-shell ${shell.id}`).catch(() => {});
            }}
            className="block text-[10px] font-mono mt-0.5 truncate hover:underline text-left w-full"
            style={{ color: "var(--text-secondary)" }}
            title={`Click to copy: --gui-shell ${shell.id}\nUse with: thclaws --serve --gui-shell ${shell.id}`}
          >
            --gui-shell {shell.id}
          </button>
          <div
            className="text-xs mt-1 line-clamp-2"
            style={{ color: "var(--text-secondary)" }}
          >
            {shell.description || "—"}
          </div>
          <div className="flex items-center gap-1.5 mt-2 flex-wrap">
            <span
              className="text-[10px] px-1.5 py-0.5 rounded font-mono"
              style={{ background: badge.bg, color: badge.fg }}
              title={badge.title}
            >
              {shell.source}
            </span>
            {shell.permissions.slice(0, 3).map((p) => (
              <span
                key={p}
                className="text-[10px] px-1.5 py-0.5 rounded font-mono"
                style={{ background: "var(--bg-primary)", color: "var(--text-secondary)" }}
                title={p}
              >
                {permissionShort(p)}
              </span>
            ))}
            {shell.permissions.length > 3 && (
              <span className="text-[10px]" style={{ color: "var(--text-secondary)" }}>
                +{shell.permissions.length - 3}
              </span>
            )}
          </div>
        </div>
      </div>
    </button>
  );
}

function sourceBadge(source: ShellInfo["source"]): { bg: string; fg: string; title: string } {
  switch (source) {
    case "builtin":
      return { bg: "rgba(95, 179, 179, 0.15)", fg: "#5fb3b3", title: "Embedded in the thClaws binary." };
    case "user":
      return { bg: "rgba(110, 168, 254, 0.15)", fg: "#6ea8fe", title: "Installed at ~/.config/thclaws/gui-shell/" };
    case "project":
      return { bg: "rgba(240, 168, 48, 0.15)", fg: "#f0a830", title: "Installed at ./.thclaws/gui-shell/ — overrides user/builtin by id." };
  }
}

/// Condense a long permission string for the card. "tools.invoke:image_gen"
/// becomes "image_gen", "agent.run" stays "agent.run".
function permissionShort(p: string): string {
  const i = p.indexOf(":");
  return i >= 0 ? p.slice(i + 1) : p;
}
