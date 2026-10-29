import { useEffect, useRef, useState } from "react";
import {
  Globe,
  Folder,
  KeyRound,
  Sun,
  Moon,
  Monitor,
  Check,
  Users,
  MessageCircle,
  Send,
  RotateCw,
  MessagesSquare,
  ChevronRight,
  FileText,
  Image as ImageIcon,
  SquareTerminal,
  SlidersHorizontal,
  ShieldCheck,
} from "lucide-react";
import { useTheme, type ThemeMode } from "../hooks/useTheme";
import { send, subscribe } from "../hooks/useIPC";

type Choice =
  | "global-instructions"
  | "folder-instructions"
  | "api-keys"
  | "line-connect"
  | "telegram-connect"
  | "messenger-connect";

/// One opt-in feature-flag row (Agent Teams / Media tools / Shell tab).
/// Writes the flag to `.thclaws/settings.json` via its IPC set message.
/// `enabled === null` while the value is still loading (button disabled).
function FeatureFlagRow({
  icon,
  label,
  desc,
  enabled,
  dirty,
  onToggle,
  locked = false,
}: {
  icon: React.ReactNode;
  label: string;
  desc: string;
  enabled: boolean | null;
  dirty: boolean;
  onToggle: () => void;
  /** On by the deployment (e.g. through the organisation's gateway): shown,
   *  not switchable. */
  locked?: boolean;
}) {
  return (
    <button
      onClick={onToggle}
      className="sm-row w-full text-left px-3 py-1.5 flex items-start gap-2"
      style={{ color: "var(--text-primary)", fontSize: "12px" }}
      disabled={enabled === null || locked}
    >
      <span
        className="sm-subtle"
        style={{ color: "var(--text-secondary)", paddingTop: "1px" }}
      >
        {icon}
      </span>
      <div className="flex-1">
        <div className="flex items-center gap-2">
          <span>{label}</span>
          <span
            style={{
              fontSize: "10px",
              padding: "1px 6px",
              borderRadius: "10px",
              background:
                enabled === true ? "var(--accent-dim)" : "var(--bg-tertiary)",
              color: enabled === true ? "#fff" : "var(--text-secondary)",
              border: enabled === true ? "none" : "1px solid var(--border)",
            }}
          >
            {enabled === null ? "…" : enabled ? "on" : "off"}
          </span>
        </div>
        <div
          className="sm-subtle"
          style={{ color: "var(--text-secondary)", fontSize: "10px" }}
        >
          {desc}
        </div>
        {dirty && (
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
  );
}

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
  // Opt-in media-generation tools (`imageToolsEnabled`); the agent
  // rebuilds on flip, so no restart. Forced on where the org pays.
  const [mediaToolsEnabled, setMediaToolsEnabled] = useState<boolean | null>(
    null,
  );
  const [mediaForced, setMediaForced] = useState(false);
  // Opt-in HAL tools (`halEnabled`) — YouTubeTranscript + WebScrape.
  // Registered at agent build, so flipping needs a restart/reload.
  const [halEnabled, setHalEnabled] = useState<boolean | null>(null);
  const [halForced, setHalForced] = useState(false);
  const [halDirty, setHalDirty] = useState(false);
  // Opt-in Shell tab (`shellTabEnabled`). App.tsx swaps the tab live off
  // the same broadcast, so no restart note is needed here.
  const [shellTabEnabled, setShellTabEnabled] = useState<boolean | null>(null);
  // Browser tools (`browserEnabled`) — opt-OUT (default ON). The Playwright
  // MCP is injected at startup, so toggling needs a restart to take effect.
  const [browserEnabled, setBrowserEnabled] = useState<boolean | null>(null);
  const [browserDirty, setBrowserDirty] = useState(false);
  // Sensitive-data masking (`sensitive.enabled`, dev-plan/55). Opt-in: it
  // rewrites the prompt, so the user asks for it explicitly.
  const [sensitiveEnabled, setSensitiveEnabled] = useState<boolean | null>(
    null,
  );
  const [sensitiveDirty, setSensitiveDirty] = useState(false);
  // Persisted GUI zoom factor (multiplier, 1.0 = native). Loaded
  // once when the menu opens; updated optimistically on selection
  // so the dropdown reflects the click without a round-trip. #47.
  const [guiScale, setGuiScale] = useState<number | null>(null);
  // Side-flyout submenus — closed by default.
  const [channelsOpen, setChannelsOpen] = useState(false);
  const [appearanceOpen, setAppearanceOpen] = useState(false);
  const [instructionsOpen, setInstructionsOpen] = useState(false);
  const [featuresOpen, setFeaturesOpen] = useState(false);

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
      } else if (
        msg.type === "media_tools_enabled" &&
        typeof msg.enabled === "boolean"
      ) {
        setMediaToolsEnabled(msg.enabled as boolean);
        setMediaForced(msg.forced === true);
      } else if (
        msg.type === "media_tools_enabled_result" &&
        typeof msg.enabled === "boolean"
      ) {
        setMediaToolsEnabled(msg.enabled as boolean);
      } else if (
        msg.type === "hal_enabled" &&
        typeof msg.enabled === "boolean"
      ) {
        setHalEnabled(msg.enabled as boolean);
        setHalForced(msg.forced === true);
      } else if (
        msg.type === "hal_enabled_result" &&
        typeof msg.enabled === "boolean"
      ) {
        setHalEnabled(msg.enabled as boolean);
        setHalDirty(true);
      } else if (
        (msg.type === "shell_tab_enabled" ||
          msg.type === "shell_tab_enabled_result") &&
        typeof msg.enabled === "boolean"
      ) {
        setShellTabEnabled(msg.enabled as boolean);
      } else if (
        msg.type === "browser_enabled" &&
        typeof msg.enabled === "boolean"
      ) {
        setBrowserEnabled(msg.enabled as boolean);
      } else if (
        msg.type === "browser_enabled_result" &&
        typeof msg.enabled === "boolean"
      ) {
        setBrowserEnabled(msg.enabled as boolean);
        setBrowserDirty(true);
      } else if (
        msg.type === "sensitive_enabled" &&
        typeof msg.enabled === "boolean"
      ) {
        setSensitiveEnabled(msg.enabled as boolean);
      } else if (
        msg.type === "sensitive_enabled_result" &&
        typeof msg.enabled === "boolean"
      ) {
        setSensitiveEnabled(msg.enabled as boolean);
        setSensitiveDirty(true);
      } else if (msg.type === "gui_scale_value" && typeof msg.scale === "number") {
        setGuiScale(msg.scale as number);
      }
    });
    send({ type: "team_enabled_get" });
    send({ type: "media_tools_enabled_get" });
    send({ type: "hal_enabled_get" });
    send({ type: "shell_tab_enabled_get" });
    send({ type: "browser_enabled_get" });
    send({ type: "sensitive_enabled_get" });
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

  const toggleMedia = () => {
    const next = !(mediaToolsEnabled ?? false);
    send({ type: "media_tools_enabled_set", enabled: next });
  };

  const toggleHal = () => {
    const next = !(halEnabled ?? false);
    send({ type: "hal_enabled_set", enabled: next });
  };

  const toggleShell = () => {
    const next = !(shellTabEnabled ?? false);
    send({ type: "shell_tab_enabled_set", enabled: next });
  };

  const toggleBrowser = () => {
    // Default ON, so an unknown (null) state toggles to off.
    const next = !(browserEnabled ?? true);
    send({ type: "browser_enabled_set", enabled: next });
  };

  const toggleSensitive = () => {
    const next = !(sensitiveEnabled ?? false);
    send({ type: "sensitive_enabled_set", enabled: next });
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

  // AGENTS.md editors — grouped into one "Instructions" side flyout.
  const instructionItems: { id: Choice; icon: React.ReactNode; label: string; hint: string }[] = [
    {
      id: "global-instructions",
      icon: <Globe size={12} />,
      label: "Global",
      hint: "~/.config/thclaws/AGENTS.md",
    },
    {
      id: "folder-instructions",
      icon: <Folder size={12} />,
      label: "Folder",
      hint: "AGENTS.md in current directory",
    },
  ];

  const items: { id: Choice; icon: React.ReactNode; label: string; hint: string }[] = [
    {
      id: "api-keys",
      icon: <KeyRound size={12} />,
      label: "Settings & API keys",
      hint: "Provider keys, gateway, thClaws.cloud, auto-learn",
    },
  ];

  // Messaging connectors — collapsed under one "Connect a channel" row
  // (3 rows → 1 by default) to keep the settings menu compact.
  const channelItems: { id: Choice; icon: React.ReactNode; label: string; hint: string }[] = [
    {
      id: "line-connect",
      icon: <MessageCircle size={12} />,
      label: "LINE",
      hint: "Pair with your LINE OA",
    },
    {
      id: "telegram-connect",
      icon: <Send size={12} />,
      label: "Telegram",
      hint: "Pair with a Telegram bot",
    },
    {
      id: "messenger-connect",
      icon: <MessageCircle size={12} />,
      label: "Messenger",
      hint: "Pair with a Facebook Page",
    },
  ];

  const themeOptions: { id: ThemeMode; icon: React.ReactNode; label: string }[] = [
    { id: "light", icon: <Sun size={12} />, label: "Light" },
    { id: "dark", icon: <Moon size={12} />, label: "Dark" },
    { id: "system", icon: <Monitor size={12} />, label: "System" },
  ];
  const currentTheme = themeOptions.find((o) => o.id === mode);

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
      {/* Instructions (AGENTS.md editors) — side flyout, same pattern. */}
      <div
        className="relative"
        onMouseEnter={() => setInstructionsOpen(true)}
        onMouseLeave={() => setInstructionsOpen(false)}
      >
        <button
          className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
          style={{ color: "var(--text-primary)", fontSize: "12px" }}
          aria-haspopup="menu"
          aria-expanded={instructionsOpen}
          onClick={() => setInstructionsOpen((v) => !v)}
        >
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            <FileText size={12} />
          </span>
          <div className="flex-1">
            <div>Instructions</div>
            <div
              className="sm-subtle"
              style={{ color: "var(--text-secondary)", fontSize: "10px" }}
            >
              Global · Folder AGENTS.md
            </div>
          </div>
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            <ChevronRight size={12} />
          </span>
        </button>
        {instructionsOpen && (
          <div
            role="menu"
            className="absolute rounded-md shadow-2xl py-1 z-50"
            style={{
              right: "100%",
              top: "-1px",
              background: "var(--bg-secondary)",
              border: "1px solid var(--border)",
              minWidth: "210px",
            }}
          >
            {instructionItems.map((item) => (
              <button
                key={item.id}
                onClick={() => {
                  onPick(item.id);
                  onClose();
                }}
                className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
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
          </div>
        )}
      </div>
      {items.map((item) => (
        <button
          key={item.id}
          onClick={() => {
            onPick(item.id);
            onClose();
          }}
          className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
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
      {/* Messaging connectors — a SIDE FLYOUT (separate popup to the
          left of the menu) so this group stays one row tall and never
          grows the main menu vertically. Opens on hover; the wrapping
          container keeps it open while the cursor travels row → flyout
          (the flyout is a descendant, so it doesn't trigger mouseleave).
          Flush at right:100% (no gap) to avoid hover flicker. */}
      <div
        className="relative"
        onMouseEnter={() => setChannelsOpen(true)}
        onMouseLeave={() => setChannelsOpen(false)}
      >
        <button
          className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
          style={{ color: "var(--text-primary)", fontSize: "12px" }}
          aria-haspopup="menu"
          aria-expanded={channelsOpen}
          onClick={() => setChannelsOpen((v) => !v)}
        >
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            <MessagesSquare size={12} />
          </span>
          <div className="flex-1">
            <div>Connect a channel…</div>
            <div
              className="sm-subtle"
              style={{ color: "var(--text-secondary)", fontSize: "10px" }}
            >
              LINE, Telegram, Messenger
            </div>
          </div>
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            <ChevronRight size={12} />
          </span>
        </button>
        {channelsOpen && (
          <div
            role="menu"
            className="absolute rounded-md shadow-2xl py-1 z-50"
            style={{
              right: "100%",
              top: "-1px",
              background: "var(--bg-secondary)",
              border: "1px solid var(--border)",
              minWidth: "200px",
            }}
          >
            {channelItems.map((item) => (
              <button
                key={item.id}
                onClick={() => {
                  onPick(item.id);
                  onClose();
                }}
                className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
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
          </div>
        )}
      </div>
      <div
        className="my-1"
        style={{ borderTop: "1px solid var(--border)" }}
      />
      {/* Appearance (theme) — side flyout, same pattern as the channel
          connectors, so the three options don't take three rows. */}
      <div
        className="relative"
        onMouseEnter={() => setAppearanceOpen(true)}
        onMouseLeave={() => setAppearanceOpen(false)}
      >
        <button
          className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
          style={{ color: "var(--text-primary)", fontSize: "12px" }}
          aria-haspopup="menu"
          aria-expanded={appearanceOpen}
          onClick={() => setAppearanceOpen((v) => !v)}
        >
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            {currentTheme?.icon ?? <Monitor size={12} />}
          </span>
          <div className="flex-1">
            <div>Appearance</div>
            <div
              className="sm-subtle"
              style={{ color: "var(--text-secondary)", fontSize: "10px" }}
            >
              Theme: {currentTheme?.label ?? "System"}
            </div>
          </div>
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            <ChevronRight size={12} />
          </span>
        </button>
        {appearanceOpen && (
          <div
            role="menu"
            className="absolute rounded-md shadow-2xl py-1 z-50"
            style={{
              right: "100%",
              top: "-1px",
              background: "var(--bg-secondary)",
              border: "1px solid var(--border)",
              minWidth: "150px",
            }}
          >
            {themeOptions.map((opt) => {
              const active = mode === opt.id;
              return (
                <button
                  key={opt.id}
                  onClick={() => setMode(opt.id)}
                  className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
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
          </div>
        )}
      </div>
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
        onClick={() => send({ type: "settings_reload" })}
        className="sm-row w-full text-left px-3 py-1.5 flex items-start gap-2"
        style={{ color: "var(--text-primary)", fontSize: "12px" }}
        title="Re-read .thclaws/settings.json without restarting the engine"
      >
        <span
          className="sm-subtle"
          style={{ color: "var(--text-secondary)", paddingTop: "1px" }}
        >
          <RotateCw size={12} />
        </span>
        <div className="flex-1">
          <div>Reload settings</div>
          <div
            className="sm-subtle"
            style={{ color: "var(--text-secondary)", fontSize: "11px" }}
          >
            Pick up changes to .thclaws/settings.json (auto-applies via file
            watcher; this button is the manual fallback)
          </div>
        </div>
      </button>
      {/* Opt-in feature flags — grouped into one "Optional features"
          SIDE FLYOUT (same pattern as Instructions / Connect-a-channel)
          so the main menu stays compact. Toggling a row writes
          .thclaws/settings.json and keeps the flyout open (the rows are
          descendants, so a toggle doesn't trigger mouseleave or close). */}
      <div
        className="relative"
        onMouseEnter={() => setFeaturesOpen(true)}
        onMouseLeave={() => setFeaturesOpen(false)}
      >
        <button
          className="sm-row w-full text-left px-3 py-2.5 sm:py-1.5 flex items-center gap-2"
          style={{ color: "var(--text-primary)", fontSize: "12px" }}
          aria-haspopup="menu"
          aria-expanded={featuresOpen}
          onClick={() => setFeaturesOpen((v) => !v)}
        >
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            <SlidersHorizontal size={12} />
          </span>
          <div className="flex-1">
            <div>Optional features</div>
            <div
              className="sm-subtle"
              style={{ color: "var(--text-secondary)", fontSize: "10px" }}
            >
              Agent Teams · Media tools · HAL · Shell tab · Browser
            </div>
          </div>
          <span className="sm-subtle" style={{ color: "var(--text-secondary)" }}>
            <ChevronRight size={12} />
          </span>
        </button>
        {featuresOpen && (
          <div
            role="menu"
            className="absolute rounded-md shadow-2xl py-1 z-50"
            style={{
              right: "100%",
              // Bottom-aligned (not top): the gear menu launches from the
              // bottom edge of the screen and this is its last row, so a
              // top-anchored flyout would grow downward off-screen. Anchor
              // the flyout's bottom to the row's bottom → it grows upward.
              bottom: "-1px",
              background: "var(--bg-secondary)",
              border: "1px solid var(--border)",
              minWidth: "250px",
            }}
          >
            <FeatureFlagRow
              icon={<Users size={12} />}
              label="Agent Teams"
              desc="TeamCreate, SpawnTeammate, … (writes `.thclaws/settings.json`)"
              enabled={teamEnabled}
              dirty={teamDirty}
              onToggle={toggleTeam}
            />
            <FeatureFlagRow
              icon={<ImageIcon size={12} />}
              label="Media tools"
              desc={
                mediaForced
                  ? "TextToImage, ImageToVideo, … — on through your organisation's gateway"
                  : "TextToImage, ImageToVideo, … (writes `.thclaws/settings.json`)"
              }
              enabled={mediaToolsEnabled}
              dirty={false}
              onToggle={toggleMedia}
              locked={mediaForced}
            />
            <FeatureFlagRow
              icon={<FileText size={12} />}
              label="HAL tools"
              desc={
                halForced
                  ? "YouTubeTranscript, WebScrape — on through your organisation's gateway; no HAL key needed"
                  : "YouTubeTranscript, WebScrape — needs a HAL key/gateway (writes `.thclaws/settings.json`)"
              }
              enabled={halEnabled}
              dirty={halDirty}
              onToggle={toggleHal}
              locked={halForced}
            />
            <FeatureFlagRow
              icon={<SquareTerminal size={12} />}
              label="Shell tab"
              desc="PTY-backed terminal tab (writes `.thclaws/settings.json`)"
              enabled={shellTabEnabled}
              dirty={false}
              onToggle={toggleShell}
            />
            <FeatureFlagRow
              icon={<Globe size={12} />}
              label="Browser tools"
              desc="Playwright browser__* tools — ON by default; needs node/npx (writes `.thclaws/settings.json`)"
              enabled={browserEnabled}
              dirty={browserDirty}
              onToggle={toggleBrowser}
            />
            <FeatureFlagRow
              icon={<ShieldCheck size={12} />}
              label="Sensitive-data masking"
              desc="Thai ID / phone / plate / names → [ID_1] before text leaves the machine, restored in the reply · skipped for local models (writes `.thclaws/settings.json`)"
              enabled={sensitiveEnabled}
              dirty={sensitiveDirty}
              onToggle={toggleSensitive}
            />
          </div>
        )}
      </div>
    </div>
  );
}
