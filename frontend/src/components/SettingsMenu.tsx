import { useEffect, useRef, useState } from "react";
import { Globe, Folder, KeyRound, Sun, Moon, Monitor, Check, Users } from "lucide-react";
import { useTheme, type ThemeMode } from "../hooks/useTheme";
import { send, subscribe } from "../hooks/useIPC";

type Choice = "global-instructions" | "folder-instructions" | "api-keys";

export function SettingsMenu({
  anchorRef,
  onPick,
  onClose,
}: {
  anchorRef: React.RefObject<HTMLElement | null>;
  onPick: (choice: Choice) => void;
  onClose: () => void;
}) {
  const menuRef = useRef<HTMLDivElement | null>(null);
  const { mode, setMode } = useTheme();
  const [teamEnabled, setTeamEnabled] = useState<boolean | null>(null);
  const [teamDirty, setTeamDirty] = useState(false);
  // Persisted GUI zoom factor (multiplier, 1.0 = native). Loaded
  // once when the menu opens; updated optimistically on selection
  // so the dropdown reflects the click without a round-trip. #47.
  const [guiScale, setGuiScale] = useState<number | null>(null);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "team_enabled" && typeof msg.enabled === "boolean") {
        setTeamEnabled(msg.enabled as boolean);
      } else if (
        msg.type === "team_enabled_result" &&
        typeof msg.enabled === "boolean"
      ) {
        setTeamEnabled(msg.enabled as boolean);
        setTeamDirty(true);
      } else if (msg.type === "gui_scale_value" && typeof msg.scale === "number") {
        setGuiScale(msg.scale as number);
      }
    });
    send({ type: "team_enabled_get" });
    send({ type: "gui_scale_get" });
    return unsub;
  }, []);

  const setZoom = (scale: number) => {
    setGuiScale(scale);
    send({ type: "gui_set_zoom", scale });
  };

  const toggleTeam = () => {
    const next = !(teamEnabled ?? false);
    send({ type: "team_enabled_set", enabled: next });
  };

  // Close on click-outside (excluding the anchor so a second click on
  // the gear icon can also close the menu via its own toggle handler).
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (menuRef.current && menuRef.current.contains(target)) return;
      if (anchorRef.current && anchorRef.current.contains(target)) return;
      onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [anchorRef, onClose]);

  const items: { id: Choice; icon: React.ReactNode; label: string; hint: string }[] = [
    {
      id: "global-instructions",
      icon: <Globe size={12} />,
      label: "Global instructions",
      hint: "Edit ~/.config/thclaws/AGENTS.md",
    },
    {
      id: "folder-instructions",
      icon: <Folder size={12} />,
      label: "Folder instructions",
      hint: "Edit AGENTS.md in the current directory",
    },
    {
      id: "api-keys",
      icon: <KeyRound size={12} />,
      label: "Provider API keys",
      hint: "Manage keys stored in the OS keychain",
    },
  ];

  const themeOptions: { id: ThemeMode; icon: React.ReactNode; label: string }[] = [
    { id: "light", icon: <Sun size={12} />, label: "Light" },
    { id: "dark", icon: <Moon size={12} />, label: "Dark" },
    { id: "system", icon: <Monitor size={12} />, label: "System" },
  ];

  return (
    <div
      ref={menuRef}
      className="absolute right-2 bottom-7 rounded-md shadow-2xl py-1 z-40"
      style={{
        background: "var(--bg-secondary)",
        border: "1px solid var(--border)",
        minWidth: "220px",
      }}
    >
      {/* Accent-tinted hover + focus highlight. `hover:bg-white/5`
          alone was nearly invisible on light themes and against the
          rest of the chrome; flooding the row with the accent color
          makes the selection unambiguous and keyboard-tabbing obvious.
          Inner `.sm-subtle` spans reset to a translucent-white color
          on hover so the hint text stays readable on the accent
          background. */}
      <style>{`
        .sm-row {
          background: transparent;
          transition: background 120ms ease, color 120ms ease;
        }
        .sm-row:hover:not(:disabled),
        .sm-row:focus-visible:not(:disabled) {
          background: var(--accent);
          color: var(--accent-fg, #ffffff) !important;
          outline: none;
        }
        .sm-row:hover:not(:disabled) .sm-subtle,
        .sm-row:focus-visible:not(:disabled) .sm-subtle {
          color: rgba(255, 255, 255, 0.85) !important;
        }
      `}</style>
      {items.map((item) => (
        <button
          key={item.id}
          onClick={() => {
            onPick(item.id);
            onClose();
          }}
          className="sm-row w-full text-left px-3 py-1.5 flex items-center gap-2"
          style={{ color: "var(--text-primary)", fontSize: "12px" }}
        >
          <span
            className="sm-subtle"
            style={{ color: "var(--text-secondary)" }}
          >
            {item.icon}
          </span>
          <div>
            <div>{item.label}</div>
            <div
              className="sm-subtle"
              style={{ color: "var(--text-secondary)", fontSize: "10px" }}
            >
              {item.hint}
            </div>
          </div>
        </button>
      ))}
      <div
        className="my-1"
        style={{ borderTop: "1px solid var(--border)" }}
      />
      <div
        className="px-3 py-1 text-[10px] uppercase tracking-wider"
        style={{ color: "var(--text-secondary)" }}
      >
        Appearance
      </div>
      {themeOptions.map((opt) => {
        const active = mode === opt.id;
        return (
          <button
            key={opt.id}
            onClick={() => setMode(opt.id)}
            className="sm-row w-full text-left px-3 py-1.5 flex items-center gap-2"
            style={{ color: "var(--text-primary)", fontSize: "12px" }}
          >
            <span
              className="sm-subtle"
              style={{ color: "var(--text-secondary)" }}
            >
              {opt.icon}
            </span>
            <span className="flex-1">{opt.label}</span>
            {active && (
              <Check size={12} style={{ color: "var(--accent)" }} />
            )}
          </button>
        );
      })}
      <div
        className="px-3 py-1.5 flex items-center gap-2"
        style={{ color: "var(--text-primary)", fontSize: "12px" }}
      >
        <span style={{ color: "var(--text-secondary)" }}>GUI scale</span>
        <select
          value={guiScale ?? 1.0}
          onChange={(e) => setZoom(parseFloat(e.target.value))}
          className="ml-auto rounded px-2 py-0.5 outline-none"
          style={{
            background: "var(--bg-tertiary)",
            border: "1px solid var(--border)",
            color: "var(--text-primary)",
            fontSize: "12px",
          }}
          title="Tune GUI text size for HiDPI / 4K displays — applies live"
        >
          <option value={0.75}>75%</option>
          <option value={0.9}>90%</option>
          <option value={1.0}>100%</option>
          <option value={1.1}>110%</option>
          <option value={1.25}>125%</option>
          <option value={1.5}>150%</option>
          <option value={1.75}>175%</option>
          <option value={2.0}>200%</option>
        </select>
      </div>
      <div
        className="my-1"
        style={{ borderTop: "1px solid var(--border)" }}
      />
      <div
        className="px-3 py-1 text-[10px] uppercase tracking-wider"
        style={{ color: "var(--text-secondary)" }}
      >
        Workspace
      </div>
      <button
        onClick={toggleTeam}
        className="sm-row w-full text-left px-3 py-1.5 flex items-start gap-2"
        style={{ color: "var(--text-primary)", fontSize: "12px" }}
        disabled={teamEnabled === null}
      >
        <span
          className="sm-subtle"
          style={{ color: "var(--text-secondary)", paddingTop: "1px" }}
        >
          <Users size={12} />
        </span>
        <div className="flex-1">
          <div className="flex items-center gap-2">
            <span>Agent Teams</span>
            <span
              style={{
                fontSize: "10px",
                padding: "1px 6px",
                borderRadius: "10px",
                background:
                  teamEnabled === true
                    ? "var(--accent-dim)"
                    : "var(--bg-tertiary)",
                color:
                  teamEnabled === true
                    ? "#fff"
                    : "var(--text-secondary)",
                border:
                  teamEnabled === true
                    ? "none"
                    : "1px solid var(--border)",
              }}
            >
              {teamEnabled === null ? "…" : teamEnabled ? "on" : "off"}
            </span>
          </div>
          <div
            className="sm-subtle"
            style={{ color: "var(--text-secondary)", fontSize: "10px" }}
          >
            TeamCreate, SpawnTeammate, … (writes `.thclaws/settings.json`)
          </div>
          {teamDirty && (
            <div
              style={{
                color: "var(--warning)",
                fontSize: "10px",
                marginTop: "2px",
              }}
            >
              Restart the app for this to take effect.
            </div>
          )}
        </div>
      </button>
    </div>
  );
}
