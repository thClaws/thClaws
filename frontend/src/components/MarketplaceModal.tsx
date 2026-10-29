import { useEffect, useMemo, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

type Entry = {
  name: string;
  short_description?: string;
  description: string;
  category?: string;
  license?: string;
  license_tier?: string;
  install_url?: string | null;
  homepage?: string;
};

type Catalog = {
  skills?: Entry[];
  mcp_servers?: Entry[];
  plugins?: Entry[];
  subagents?: Entry[];
};

type Installed = { skills?: string[]; subagents?: string[] };

type State = {
  source: string;
  cacheAge: string | null;
  catalog: Catalog;
  installed: Installed;
};

type TabKey = "skills" | "mcp_servers" | "plugins" | "subagents";

// Per-tab: catalog field, label, the slash kind used for `/<kind> install`,
// and which installed-set (if any) drives the ✓ badge.
const TABS: {
  key: TabKey;
  label: string;
  installCmd: string;
  installedKey?: keyof Installed;
}[] = [
  { key: "skills", label: "Skills", installCmd: "skill", installedKey: "skills" },
  { key: "mcp_servers", label: "MCP", installCmd: "mcp" },
  { key: "plugins", label: "Plugins", installCmd: "plugin" },
  { key: "subagents", label: "Subagents", installCmd: "subagent", installedKey: "subagents" },
];

/**
 * Unified marketplace browser, opened by `/marketplace` from the GUI
 * Chat tab. Subscribes to `marketplace_open` (carries the full catalog
 * + installed-name sets). Install and Refresh are performed by injecting
 * the existing slash commands through the `shell_input` IPC — so all the
 * existing install/refresh logic runs unchanged and results stream into
 * chat. Mirrors the self-contained, IPC-driven pattern of
 * AgentEditorModal / ScheduleAddModal.
 */
export function MarketplaceModal() {
  const [data, setData] = useState<State | null>(null);
  const [tab, setTab] = useState<TabKey>("skills");
  const [query, setQuery] = useState("");
  const [dispatched, setDispatched] = useState<Record<string, boolean>>({});

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "marketplace_open") {
        setData({
          source: String(msg.source ?? ""),
          cacheAge: (msg.cacheAge as string | null) ?? null,
          catalog: (msg.catalog as Catalog) ?? {},
          installed: (msg.installed as Installed) ?? {},
        });
        setDispatched({});
      }
    });
    return unsub;
  }, []);

  const active = TABS.find((t) => t.key === tab)!;
  const entries = useMemo<Entry[]>(() => {
    const list = (data?.catalog?.[tab] as Entry[] | undefined) ?? [];
    const q = query.trim().toLowerCase();
    if (!q) return list;
    return list.filter(
      (e) =>
        e.name.toLowerCase().includes(q) ||
        e.description.toLowerCase().includes(q) ||
        (e.category ?? "").toLowerCase().includes(q),
    );
  }, [data, tab, query]);

  if (!data) return null;

  const onClose = () => setData(null);

  const isInstalled = (name: string) =>
    active.installedKey
      ? (data.installed[active.installedKey] ?? []).includes(name)
      : false;

  const onInstall = (e: Entry) => {
    const verb = tab === "skills" && isInstalled(e.name) ? "update" : "install";
    send({ type: "shell_input", text: `/${active.installCmd} ${verb} ${e.name}` });
    setDispatched((d) => ({ ...d, [`${tab}:${e.name}`]: true }));
  };

  const onRefresh = () => send({ type: "shell_input", text: "/marketplace --refresh" });

  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center"
      style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
      onClick={onClose}
    >
      <div
        className="rounded-lg border shadow-xl w-[820px] max-w-[95vw] max-h-[92vh] flex flex-col"
        style={{
          background: "var(--bg-primary)",
          borderColor: "var(--border)",
          color: "var(--text-primary)",
        }}
        onClick={(ev) => ev.stopPropagation()}
      >
        {/* header */}
        <div
          className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <span style={{ color: "var(--accent)" }}>●</span>
          <span>Marketplace</span>
          <span className="font-mono text-xs" style={{ color: "var(--text-secondary)" }}>
            {data.source}
            {data.cacheAge ? ` · ${data.cacheAge}` : ""}
          </span>
          <button
            type="button"
            onClick={onRefresh}
            className="ml-auto px-2 py-1 rounded border text-xs"
            style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
          >
            ↻ Refresh
          </button>
        </div>

        {/* tabs + search */}
        <div
          className="px-4 py-2 border-b flex items-center gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <div className="flex gap-1">
            {TABS.map((t) => {
              const count = (data.catalog[t.key] as Entry[] | undefined)?.length ?? 0;
              const sel = t.key === tab;
              return (
                <button
                  key={t.key}
                  type="button"
                  onClick={() => {
                    setTab(t.key);
                    setQuery("");
                  }}
                  className="px-2.5 py-1 rounded text-xs font-medium"
                  style={{
                    background: sel ? "var(--accent)" : "transparent",
                    color: sel ? "var(--accent-fg, #06231a)" : "var(--text-secondary)",
                    border: `1px solid ${sel ? "var(--accent)" : "var(--border)"}`,
                  }}
                >
                  {t.label} <span style={{ opacity: 0.7 }}>{count}</span>
                </button>
              );
            })}
          </div>
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={`Search ${active.label.toLowerCase()}…`}
            className="ml-auto px-2 py-1 rounded border text-xs w-48"
            style={{
              background: "var(--bg-secondary, var(--bg-primary))",
              borderColor: "var(--border)",
              color: "var(--text-primary)",
            }}
          />
        </div>

        {/* rows */}
        <div className="px-2 py-2 overflow-auto" style={{ minHeight: "320px" }}>
          {entries.length === 0 && (
            <div className="px-3 py-6 text-xs text-center" style={{ color: "var(--text-secondary)" }}>
              nothing here
            </div>
          )}
          {entries.map((e) => {
            const linkedOnly = e.license_tier === "linked-only";
            const installed = isInstalled(e.name);
            const sent = dispatched[`${tab}:${e.name}`];
            const updatable = installed && tab === "skills";
            const idle = !sent && (!installed || updatable);
            return (
              <div
                key={e.name}
                className="px-3 py-2 rounded flex items-start gap-3"
                style={{ borderBottom: "1px solid var(--border)" }}
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="font-mono text-xs font-semibold">{e.name}</span>
                    {e.category && (
                      <span className="text-[10px]" style={{ color: "var(--text-secondary)" }}>
                        {e.category}
                      </span>
                    )}
                    {installed && (
                      <span className="text-[10px]" style={{ color: "var(--accent)" }}>
                        ✓ installed
                      </span>
                    )}
                    {linkedOnly && (
                      <span className="text-[10px]" style={{ color: "var(--text-secondary)" }}>
                        linked-only
                      </span>
                    )}
                  </div>
                  <div className="text-xs mt-0.5" style={{ color: "var(--text-secondary)" }}>
                    {e.short_description || e.description}
                  </div>
                </div>
                {linkedOnly ? (
                  <a
                    href={e.homepage || "#"}
                    target="_blank"
                    rel="noreferrer"
                    className="px-2.5 py-1 rounded border text-xs whitespace-nowrap"
                    style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
                  >
                    Upstream ↗
                  </a>
                ) : (
                  <button
                    type="button"
                    onClick={() => onInstall(e)}
                    disabled={!idle}
                    className="px-2.5 py-1 rounded text-xs font-medium whitespace-nowrap"
                    style={{
                      background: idle && !updatable ? "var(--accent)" : "transparent",
                      color:
                        idle && !updatable
                          ? "var(--accent-fg, #06231a)"
                          : idle
                            ? "var(--accent)"
                            : "var(--text-secondary)",
                      border: `1px solid ${idle ? "var(--accent)" : "var(--border)"}`,
                    }}
                    title={updatable && idle ? "Replace it with the marketplace's current copy" : undefined}
                  >
                    {sent ? "See chat…" : updatable ? "Update" : installed ? "Installed" : "Install"}
                  </button>
                )}
              </div>
            );
          })}
        </div>

        {/* footer */}
        <div
          className="px-4 py-2 border-t flex items-center justify-between text-xs"
          style={{ borderColor: "var(--border)", color: "var(--text-secondary)" }}
        >
          <span>Install runs in chat — watch the conversation for the result.</span>
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1.5 rounded border"
            style={{ borderColor: "var(--border)" }}
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
