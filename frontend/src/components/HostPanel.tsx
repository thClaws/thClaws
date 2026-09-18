import { useState } from "react";
import { basePath, botQuery, send, subscribe, type IPCMessage } from "../hooks/useIPC";
import type { BotStatus } from "./BotRail";

// dev-plan/59 §6.3: the workspace's own surface — what belongs to no single
// bot. It hangs off the rail rather than the tab strip, because tabs belong
// to a bot.
//
// Installing a bot is a HOST action, not something an agent does for you: a
// bot's sandbox root is its own folder, so it cannot write a sibling's. That
// is the trust model working, and it is why this is a form rather than a
// chat request.

type ActionResult = { log?: string[] };

/**
 * One host mutation. In a browser it is an HTTP call to the host; on the
 * desktop it goes over the IPC bridge, because a `fetch` there would reach
 * the `thclaws://` protocol handler — which serves files, not the host — and
 * the window answers with a `bots_action_result` frame instead.
 */
async function hostAction(
  kind: "bots_add" | "bots_remove" | "bots_restart",
  slug: string,
  purge = false,
  blank = false,
): Promise<ActionResult> {
  if (typeof window !== "undefined" && window.ipc) {
    return new Promise<ActionResult>((resolve, reject) => {
      const off = subscribe((msg: IPCMessage) => {
        if (msg.type !== "bots_action_result") return;
        off();
        if (msg.ok === true) {
          resolve({ log: Array.isArray(msg.log) ? (msg.log as string[]) : [] });
        } else {
          reject(new Error(typeof msg.error === "string" ? msg.error : "failed"));
        }
      });
      send({ type: kind, slug, purge, blank });
    });
  }
  const [method, path, body] =
    kind === "bots_add"
      ? ["POST", "bots", JSON.stringify({ slug, blank })]
      : kind === "bots_remove"
        ? ["DELETE", `bots/${encodeURIComponent(slug)}`, undefined]
        : ["POST", `bots/${encodeURIComponent(slug)}/restart`, undefined];
  const res = await fetch(`${basePath()}${path}${botQuery(null)}`, {
    method,
    headers: body ? { "Content-Type": "application/json" } : undefined,
    body,
  });
  const parsed = (await res.json().catch(() => ({}))) as Record<string, unknown>;
  if (!res.ok || parsed.ok === false) {
    throw new Error(
      typeof parsed.error === "string" ? parsed.error : `HTTP ${res.status}`,
    );
  }
  return { log: Array.isArray(parsed.log) ? (parsed.log as string[]) : [] };
}

export function HostPanel({
  bots,
  onClose,
  onChanged,
}: {
  bots: BotStatus[];
  onClose: () => void;
  onChanged: () => void | Promise<unknown>;
}) {
  const [slug, setSlug] = useState("");
  const [blankSlug, setBlankSlug] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [log, setLog] = useState<string[]>([]);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
      await onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const add = () =>
    run(async () => {
      const done = await hostAction("bots_add", slug.trim());
      setLog(done.log ?? []);
      setSlug("");
    });

  // A bot with no agent — the same as opening thClaws on a new folder.
  const addBlank = () =>
    run(async () => {
      const done = await hostAction("bots_add", blankSlug.trim(), false, true);
      setLog(done.log ?? []);
      setBlankSlug("");
    });

  const remove = (s: string) => run(() => hostAction("bots_remove", s));
  const restart = (s: string) => run(() => hostAction("bots_restart", s));

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-[var(--modal-backdrop)]"
      onClick={onClose}
    >
      <div
        className="w-[520px] max-w-[92vw] max-h-[80vh] overflow-auto rounded-lg border border-[var(--border)] bg-[var(--bg-secondary)] p-4 text-sm"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between mb-3">
          <h2 className="font-semibold">Bots in this workspace</h2>
          <button
            onClick={onClose}
            className="text-[var(--text-secondary)] hover:text-[var(--text-primary)]"
          >
            ✕
          </button>
        </div>

        <p className="mb-4 text-xs text-[var(--text-secondary)]">
          Each bot has its own settings and sessions. Select a bot and open Team to see its lead and teammates.
        </p>

        <ul className="mb-4 divide-y divide-[var(--border)]">
          {bots.map((b) => (
            <li key={b.slug} className="flex items-center gap-2 py-2">
              <span className="flex-1 min-w-0 truncate">{b.name}</span>
              <span className="text-xs text-[var(--text-secondary)]">
                {b.state}
              </span>
              <button
                disabled={busy}
                onClick={() => restart(b.slug)}
                title={
                  b.state === "crash_looped"
                    ? "thClaws stopped retrying this bot; try again"
                    : "Stop and start this bot"
                }
                className="px-2 py-0.5 rounded text-xs border border-[var(--border)] disabled:opacity-40 hover:bg-[var(--bg-tertiary)]"
              >
                Restart
              </button>
              <button
                disabled={busy || bots.length < 2}
                onClick={() => remove(b.slug)}
                title={
                  bots.length < 2
                    ? "A workspace always has at least one bot"
                    : "Remove from this workspace. Its folder — sessions, logins — stays on disk."
                }
                className="px-2 py-0.5 rounded text-xs border border-[var(--border)] disabled:opacity-40 hover:bg-[var(--bg-tertiary)]"
              >
                Remove
              </button>
            </li>
          ))}
        </ul>

        <label className="block text-xs text-[var(--text-secondary)] mb-1">
          Create a bot from Agent Templates
        </label>
        <div className="flex gap-2">
          <input
            value={slug}
            onChange={(e) => setSlug(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && slug.trim() && !busy) void add();
            }}
            placeholder="template name, e.g. hello-world"
            className="flex-1 px-2 py-1 rounded border border-[var(--border)] bg-[var(--bg-primary)]"
          />
          <button
            disabled={busy || !slug.trim()}
            onClick={() => void add()}
            className="px-3 py-1 rounded bg-[var(--accent)] text-[var(--accent-fg)] disabled:opacity-40"
          >
            {busy ? "Working…" : "Get"}
          </button>
        </div>

        <label className="block text-xs text-[var(--text-secondary)] mt-4 mb-1">
          Or create a blank bot
        </label>
        <div className="flex gap-2">
          <input
            value={blankSlug}
            onChange={(e) => setBlankSlug(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && blankSlug.trim() && !busy) void addBlank();
            }}
            placeholder="bot name, e.g. scratch"
            className="flex-1 px-2 py-1 rounded border border-[var(--border)] bg-[var(--bg-primary)]"
          />
          <button
            disabled={busy || !blankSlug.trim()}
            onClick={() => void addBlank()}
            className="px-3 py-1 rounded border border-[var(--border)] disabled:opacity-40 hover:bg-[var(--bg-tertiary)]"
          >
            {busy ? "Working…" : "Create"}
          </button>
        </div>

        {error && (
          <pre className="mt-3 whitespace-pre-wrap text-xs text-[var(--danger)]">
            {error}
          </pre>
        )}
        {log.length > 0 && (
          <pre className="mt-3 whitespace-pre-wrap text-xs text-[var(--text-secondary)]">
            {log.join("\n")}
          </pre>
        )}
      </div>
    </div>
  );
}
