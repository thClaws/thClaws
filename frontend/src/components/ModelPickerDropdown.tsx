import { useEffect, useMemo, useRef, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

/// One model row from the backend's `all_models_list` response.
type ModelRow = {
  id: string;
  context?: number | null;
};

/// One provider group as the backend ships it. `provider` is the
/// lowercase short name (`anthropic`, `openai`, …); `models` is sorted
/// by id ascending.
type Group = {
  provider: string;
  models: ModelRow[];
};

type Props = {
  current: string;
  onClose: () => void;
};

function formatCtx(n: number | null | undefined): string {
  if (!n || n <= 0) return "";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  if (n >= 1_000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

/// Drop the first `<segment>/` from a model id when that segment is the
/// known prefix for its group. The full id is still sent on selection;
/// only the display label is shortened to fit the narrow sidebar.
/// Examples (group → input → output):
///   ollama        → ollama/llama3.2          → llama3.2
///   thaillm       → thaillm/OpenThaiGPT-…    → OpenThaiGPT-…
///   ollama-cloud  → ollama-cloud/kimi-k2.5   → kimi-k2.5
///   openrouter    → openrouter/anthropic/X   → anthropic/X (still informative)
///   anthropic     → claude-sonnet-4-6        → claude-sonnet-4-6 (no slash)
function stripProviderPrefix(id: string, provider: string): string {
  // Special-cased shortcuts where the model prefix and provider name diverge.
  // Falls through to the general "strip any leading <prefix>/" otherwise.
  const aliases: Record<string, string> = {
    "agentic-press": "ap",
    "ollama-anthropic": "oa",
    "openai-compat": "oai",
  };
  const candidates = [provider, aliases[provider]].filter(
    (p): p is string => Boolean(p),
  );
  for (const p of candidates) {
    const prefix = `${p}/`;
    if (id.startsWith(prefix)) return id.slice(prefix.length);
  }
  return id;
}

/// Sidebar inline model picker (issue #49). Asks the backend for the
/// cross-provider model list when mounted, renders a search box plus
/// grouped list, and dispatches `model_set` on selection. Anchored
/// absolute under the sidebar's Provider section by the parent.
export function ModelPickerDropdown({ current, onClose }: Props) {
  const [groups, setGroups] = useState<Group[]>([]);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  // Fetch on mount.
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "all_models_list") {
        setGroups((msg.groups as Group[]) ?? []);
        setLoading(false);
      }
    });
    send({ type: "request_all_models" });
    return unsub;
  }, []);

  // Esc closes; click outside closes.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    const onClick = (e: MouseEvent) => {
      const target = e.target as Node;
      if (containerRef.current && !containerRef.current.contains(target)) {
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    // Defer one tick so the click that opened us doesn't immediately close.
    const t = setTimeout(
      () => window.addEventListener("mousedown", onClick),
      0,
    );
    return () => {
      window.removeEventListener("keydown", onKey);
      clearTimeout(t);
      window.removeEventListener("mousedown", onClick);
    };
  }, [onClose]);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const filtered = useMemo<Group[]>(() => {
    const q = query.trim().toLowerCase();
    if (!q) return groups;
    return groups
      .map((g) => ({
        provider: g.provider,
        models: g.models.filter(
          (m) =>
            m.id.toLowerCase().includes(q) ||
            g.provider.toLowerCase().includes(q),
        ),
      }))
      .filter((g) => g.models.length > 0);
  }, [groups, query]);

  const totalCount = useMemo(
    () => groups.reduce((acc, g) => acc + g.models.length, 0),
    [groups],
  );

  const pick = (id: string) => {
    send({ type: "model_set", model: id });
    onClose();
  };

  return (
    <div
      ref={containerRef}
      className="absolute left-2 right-2 mt-1 rounded shadow-lg z-50 flex flex-col"
      style={{
        top: "100%",
        background: "var(--bg-secondary)",
        border: "1px solid var(--border)",
        maxHeight: "60vh",
      }}
    >
      <div
        className="px-2 py-2 border-b"
        style={{ borderColor: "var(--border)" }}
      >
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={
            loading ? "Loading…" : `Search ${totalCount} model${totalCount === 1 ? "" : "s"}…`
          }
          className="w-full px-2 py-1 rounded text-xs outline-none"
          style={{
            background: "var(--bg-tertiary)",
            border: "1px solid var(--border)",
            color: "var(--text-primary)",
          }}
        />
      </div>

      <div className="flex-1 overflow-y-auto py-1">
        {loading ? (
          <div
            className="px-2 py-3 text-xs text-center"
            style={{ color: "var(--text-secondary)" }}
          >
            Loading models…
          </div>
        ) : filtered.length === 0 ? (
          <div
            className="px-2 py-3 text-xs text-center"
            style={{ color: "var(--text-secondary)" }}
          >
            No models match "{query}".
          </div>
        ) : (
          filtered.map((g) => (
            <div key={g.provider}>
              <div
                className="px-2 py-1 text-[10px] uppercase tracking-wider sticky top-0"
                style={{
                  color: "var(--text-secondary)",
                  background: "var(--bg-secondary)",
                }}
              >
                {g.provider}
              </div>
              {g.models.map((m) => {
                const isCurrent = m.id === current;
                const ctx = formatCtx(m.context);
                return (
                  <button
                    key={m.id}
                    type="button"
                    onClick={() => pick(m.id)}
                    className="w-full px-2 py-1 text-left text-xs flex items-center justify-between"
                    style={{
                      background: isCurrent
                        ? "var(--bg-tertiary)"
                        : "transparent",
                      color: "var(--text-primary)",
                      cursor: "pointer",
                      borderLeft: isCurrent
                        ? "2px solid var(--accent)"
                        : "2px solid transparent",
                    }}
                    onMouseEnter={(e) =>
                      (e.currentTarget.style.background = "var(--bg-tertiary)")
                    }
                    onMouseLeave={(e) =>
                      (e.currentTarget.style.background = isCurrent
                        ? "var(--bg-tertiary)"
                        : "transparent")
                    }
                  >
                    <span className="font-mono truncate">
                      {stripProviderPrefix(m.id, g.provider)}
                    </span>
                    {ctx && (
                      <span
                        className="ml-2 shrink-0"
                        style={{
                          color: "var(--text-secondary)",
                          fontSize: "10px",
                        }}
                      >
                        {ctx}
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          ))
        )}
      </div>
    </div>
  );
}
