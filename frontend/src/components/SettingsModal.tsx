import { useEffect, useState } from "react";
import { KeyRound, X, Check, Trash2, Link as LinkIcon } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { SecretsBackendDialog } from "./SecretsBackendDialog";

type KeyStatus = {
  provider: string;
  env_var: string;
  configured_in_keychain: boolean;
  env_set: boolean;
  key_length: number;
};

type EndpointStatus = {
  provider: string;
  env_var: string;
  configured_url: string | null;
  default_url: string;
};

// Sentinel shown in the API key input when a key is already configured.
// Its length matches the actual env-var length so the user gets a visual
// cue of the key's size without ever seeing its contents. If no key is
// loaded we fall back to a short 5-char sentinel.
const FALLBACK_SENTINEL = "*****";
function sentinelFor(length: number): string {
  // Clamp so a huge key doesn't overflow the field to an absurd width.
  const clamped = Math.max(5, Math.min(length, 64));
  return "*".repeat(clamped);
}
function isSentinel(s: string): boolean {
  return s.length >= 5 && /^\*+$/.test(s);
}

const PROVIDER_LABELS: Record<string, string> = {
  "agentic-press": "Agentic Press LLM",
  anthropic: "Anthropic",
  openai: "OpenAI",
  openrouter: "OpenRouter",
  gemini: "Google Gemini",
  dashscope: "Alibaba DashScope",
  ollama: "Ollama",
  "ollama-anthropic": "Ollama (Anthropic-compatible)",
  "ollama-cloud": "Ollama Cloud",
  azure: "Azure AI Foundry",
  "openai-compat": "OpenAI-Compatible (custom endpoint)",
};

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const [keys, setKeys] = useState<KeyStatus[]>([]);
  const [endpoints, setEndpoints] = useState<EndpointStatus[]>([]);
  const [keyDrafts, setKeyDrafts] = useState<Record<string, string>>({});
  const [urlDrafts, setUrlDrafts] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [flash, setFlash] = useState<Record<string, { ok: boolean; msg: string }>>({});
  // Storage backend: null until we hear back from the backend. If the
  // backend reports `null` (user never picked), we show the chooser
  // dialog first and only render the key fields after the user picks.
  const [backend, setBackend] = useState<"keychain" | "dotenv" | null>(null);
  const [backendKnown, setBackendKnown] = useState(false);

  // Ask the backend for the stored preference first. Nothing else
  // happens until we know — no api_key_status, no keychain reads.
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "secrets_backend") {
        const value = (msg.backend as string | null) ?? null;
        setBackend(value === "keychain" || value === "dotenv" ? value : null);
        setBackendKnown(true);
      }
    });
    send({ type: "secrets_backend_get" });
    return unsub;
  }, []);

  useEffect(() => {
    // Only subscribe for key / endpoint data once the user has picked.
    if (!backend) return;
    const unsub = subscribe((msg) => {
      if (msg.type === "api_key_status" && Array.isArray(msg.keys)) {
        const next = msg.keys as KeyStatus[];
        setKeys(next);
        // Every key field starts at a sentinel string sized to the actual
        // key length — the user sees at a glance that a key is loaded and
        // roughly how long it is, without the value being exposed. Preserves
        // in-flight edits so a status refresh doesn't clobber typing.
        setKeyDrafts((prev) => {
          const out = { ...prev };
          for (const k of next) {
            const cur = out[k.provider];
            const sentinel = k.key_length > 0 ? sentinelFor(k.key_length) : FALLBACK_SENTINEL;
            if (cur === undefined || cur === "" || isSentinel(cur)) {
              out[k.provider] = sentinel;
            }
          }
          return out;
        });
      } else if (msg.type === "endpoint_status" && Array.isArray(msg.endpoints)) {
        const next = msg.endpoints as EndpointStatus[];
        setEndpoints(next);
        // Pre-fill URL drafts with the configured value so the user sees it
        // and can edit in place. Leave actively-edited drafts alone.
        setUrlDrafts((prev) => {
          const out = { ...prev };
          for (const e of next) {
            const cur = out[e.provider];
            if (cur === undefined || cur === "" || cur === e.configured_url) {
              out[e.provider] = e.configured_url ?? "";
            }
          }
          return out;
        });
      } else if (msg.type === "api_key_result" || msg.type === "endpoint_result") {
        const provider = msg.provider as string;
        const flashKey = `${provider}:${msg.type === "api_key_result" ? "key" : "url"}`;
        setBusy(null);
        setFlash((f) => ({
          ...f,
          [flashKey]: {
            ok: Boolean(msg.ok),
            msg: msg.ok
              ? msg.action === "set"
                ? msg.storage === "dotenv"
                  ? "Saved to ~/.config/thclaws/.env (keychain unavailable)"
                  : "Saved to OS keychain"
                : "Cleared"
              : String(msg.error ?? "Failed"),
          },
        }));
        if (msg.ok) {
          if (msg.type === "api_key_result") {
            // Reset the draft; the follow-up api_key_status will repopulate
            // with a sentinel sized to the new key length.
            setKeyDrafts((d) => ({ ...d, [provider]: "" }));
          }
          // URL drafts get re-synced from the follow-up endpoint_status below.
        }
        send({ type: msg.type === "api_key_result" ? "api_key_status" : "endpoint_status" });
        setTimeout(() => {
          setFlash((f) => {
            const next = { ...f };
            delete next[flashKey];
            return next;
          });
        }, 2500);
      }
    });
    send({ type: "api_key_status" });
    send({ type: "endpoint_status" });
    return unsub;
  }, [backend]);

  // Merge keys + endpoints by provider so each provider renders once.
  const providers = new Map<string, { key?: KeyStatus; endpoint?: EndpointStatus }>();
  keys.forEach((k) => {
    const entry = providers.get(k.provider) ?? {};
    entry.key = k;
    providers.set(k.provider, entry);
  });
  endpoints.forEach((e) => {
    const entry = providers.get(e.provider) ?? {};
    entry.endpoint = e;
    providers.set(e.provider, entry);
  });

  const handleSaveKey = (provider: string) => {
    const key = (keyDrafts[provider] ?? "").trim();
    // Empty or any asterisk-only sentinel → nothing to save (user didn't edit).
    if (!key || isSentinel(key)) return;
    setBusy(`${provider}:key`);
    send({ type: "api_key_set", provider, key });
  };

  const handleClearKey = (provider: string) => {
    setBusy(`${provider}:key`);
    send({ type: "api_key_clear", provider });
  };

  const handleSaveUrl = (provider: string) => {
    const url = (urlDrafts[provider] ?? "").trim();
    if (!url) return;
    // Unchanged → skip the round-trip.
    const current = endpoints.find((e) => e.provider === provider)?.configured_url ?? "";
    if (url === current) return;
    setBusy(`${provider}:url`);
    send({ type: "endpoint_set", provider, url });
  };

  const handleClearUrl = (provider: string) => {
    setBusy(`${provider}:url`);
    send({ type: "endpoint_clear", provider });
  };

  // Still waiting to hear from the backend → render nothing (a flash
  // of empty modal is worse than a tiny delay).
  if (!backendKnown) return null;

  // User hasn't picked a storage backend yet — show the chooser
  // dialog first. Once they pick, backend flips to the chosen value
  // and the real modal renders.
  if (backend === null) {
    return (
      <SecretsBackendDialog
        onPicked={(choice) => setBackend(choice)}
        onCancel={onClose}
      />
    );
  }

  return (
    <div
      className="fixed inset-0 flex items-center justify-center z-50"
      style={{ background: "var(--modal-backdrop)" }}
      // Close on backdrop mousedown only when the gesture *started* on
      // the backdrop — keeps drag-to-select inside the modal from
      // accidentally dismissing it on mouseup outside.
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="rounded-lg shadow-2xl p-5 max-w-xl w-full mx-4 max-h-[85vh] overflow-y-auto"
        style={{ background: "var(--bg-secondary)", border: "1px solid var(--border)" }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between mb-3">
          <div className="flex items-center gap-2">
            <KeyRound size={16} style={{ color: "var(--accent)" }} />
            <h2 className="text-sm font-semibold" style={{ color: "var(--text-primary)" }}>
              Providers
            </h2>
          </div>
          <button
            onClick={onClose}
            className="p-1 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Close"
          >
            <X size={14} />
          </button>
        </div>
        <p className="text-xs mb-4" style={{ color: "var(--text-secondary)" }}>
          {backend === "keychain"
            ? "API keys live in your OS keychain (encrypted, tied to your user account)."
            : (
              <>
                API keys are stored in{" "}
                <span className="font-mono">~/.config/thclaws/.env</span>{" "}
                (plain text — no keychain prompts).
              </>
            )}{" "}
          Base URLs are saved to{" "}
          <span className="font-mono">~/.config/thclaws/endpoints.json</span>.
          A shell <span className="font-mono">export</span> always wins.{" "}
          <button
            type="button"
            onClick={() => setBackend(null)}
            className="underline"
            style={{ color: "var(--text-primary)", opacity: 0.7 }}
            title="Change storage backend"
          >
            Change
          </button>
        </p>

        <div className="flex flex-col gap-3">
          {Array.from(providers.entries()).map(([provider, row]) => {
            const label = PROVIDER_LABELS[provider] ?? provider;
            return (
              <div
                key={provider}
                className="rounded p-3"
                style={{ background: "var(--bg-tertiary)", border: "1px solid var(--border)" }}
              >
                <div className="flex items-center justify-between mb-2">
                  <div className="text-xs font-semibold" style={{ color: "var(--text-primary)" }}>
                    {label}
                  </div>
                </div>

                {row.key && (
                  <KeyRow
                    status={row.key}
                    draft={
                      keyDrafts[provider] ??
                      (row.key.key_length > 0
                        ? sentinelFor(row.key.key_length)
                        : FALLBACK_SENTINEL)
                    }
                    onDraft={(v) => setKeyDrafts((d) => ({ ...d, [provider]: v }))}
                    onSave={() => handleSaveKey(provider)}
                    onClear={() => handleClearKey(provider)}
                    busy={busy === `${provider}:key`}
                    flash={flash[`${provider}:key`]}
                  />
                )}

                {row.endpoint && (
                  <UrlRow
                    status={row.endpoint}
                    draft={urlDrafts[provider] ?? (row.endpoint.configured_url ?? "")}
                    onDraft={(v) => setUrlDrafts((d) => ({ ...d, [provider]: v }))}
                    onSave={() => handleSaveUrl(provider)}
                    onClear={() => handleClearUrl(provider)}
                    busy={busy === `${provider}:url`}
                    flash={flash[`${provider}:url`]}
                    hasKeyRow={Boolean(row.key)}
                  />
                )}
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function KeyRow({
  status,
  draft,
  onDraft,
  onSave,
  onClear,
  busy,
  flash,
}: {
  status: KeyStatus;
  draft: string;
  onDraft: (v: string) => void;
  onSave: () => void;
  onClear: () => void;
  busy: boolean;
  flash?: { ok: boolean; msg: string };
}) {
  const trimmed = draft.trim();
  const showingSentinel = isSentinel(draft);
  const unchanged = trimmed === "" || showingSentinel;
  return (
    <div>
      <FieldLabel icon={<KeyRound size={11} />} text="API Key" env={status.env_var} />
      <div className="flex gap-1.5">
        <input
          // While the sentinel is showing we use `text` so the literal
          // asterisks are visible; once the user starts typing a real key
          // we flip to `password` so the characters mask.
          type={showingSentinel ? "text" : "password"}
          placeholder="Paste API key"
          className="flex-1 px-2.5 py-1.5 rounded text-xs font-mono outline-none"
          style={{
            background: "var(--bg-primary)",
            color: "var(--text-primary)",
            border: "1px solid var(--border)",
          }}
          value={draft}
          onChange={(e) => onDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onSave();
          }}
          onFocus={(e) => {
            // Clicking the sentinel selects it so typing replaces in one go.
            if (isSentinel(e.currentTarget.value)) e.currentTarget.select();
          }}
          disabled={busy}
          autoComplete="off"
          spellCheck={false}
        />
        <SaveButton onClick={onSave} disabled={unchanged || busy} />
        {/* Show Clear whenever any key is loaded — from the keychain or
            from an .env file / shell export. The backend clears the
            keychain entry (if any) and unsets the env var for the
            running process. */}
        {(status.configured_in_keychain || status.env_set) && (
          <ClearButton
            onClick={onClear}
            disabled={busy}
            title={
              status.configured_in_keychain
                ? "Remove from OS keychain"
                : "Unset for this session (edit .env to remove permanently)"
            }
          />
        )}
      </div>
      <FlashLine flash={flash} />
    </div>
  );
}

function UrlRow({
  status,
  draft,
  onDraft,
  onSave,
  onClear,
  busy,
  flash,
  hasKeyRow,
}: {
  status: EndpointStatus;
  draft: string;
  onDraft: (v: string) => void;
  onSave: () => void;
  onClear: () => void;
  busy: boolean;
  flash?: { ok: boolean; msg: string };
  hasKeyRow: boolean;
}) {
  const trimmed = draft.trim();
  const current = status.configured_url ?? "";
  const unchanged = trimmed === "" || trimmed === current;
  return (
    <div style={{ marginTop: hasKeyRow ? 10 : 0 }}>
      <FieldLabel icon={<LinkIcon size={11} />} text="Base URL" env={status.env_var} />
      <div className="flex gap-1.5">
        <input
          type="text"
          placeholder={status.default_url}
          className="flex-1 px-2.5 py-1.5 rounded text-xs font-mono outline-none"
          style={{
            background: "var(--bg-primary)",
            color: "var(--text-primary)",
            border: "1px solid var(--border)",
          }}
          value={draft}
          onChange={(e) => onDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onSave();
          }}
          disabled={busy}
          autoComplete="off"
          spellCheck={false}
        />
        <SaveButton onClick={onSave} disabled={unchanged || busy} />
        {status.configured_url && <ClearButton onClick={onClear} disabled={busy} />}
      </div>
      <FlashLine flash={flash} />
    </div>
  );
}

function FieldLabel({ icon, text, env }: { icon: React.ReactNode; text: string; env: string }) {
  return (
    <div
      className="flex items-center gap-1.5 mb-1"
      style={{ color: "var(--text-secondary)", fontSize: "10px" }}
    >
      {icon}
      <span className="uppercase tracking-wider">{text}</span>
      <span className="font-mono" style={{ opacity: 0.7 }}>
        {env}
      </span>
    </div>
  );
}

function SaveButton({ onClick, disabled }: { onClick: () => void; disabled: boolean }) {
  return (
    <button
      className="px-3 py-1.5 rounded text-xs font-medium shrink-0 flex items-center gap-1"
      style={{
        background: "var(--accent)",
        color: "#fff",
        opacity: disabled ? 0.4 : 1,
        cursor: disabled ? "not-allowed" : "pointer",
      }}
      onClick={onClick}
      disabled={disabled}
    >
      <Check size={12} /> Save
    </button>
  );
}

function ClearButton({
  onClick,
  disabled,
  title = "Remove",
}: {
  onClick: () => void;
  disabled: boolean;
  title?: string;
}) {
  return (
    <button
      className="px-2.5 py-1.5 rounded text-xs font-medium shrink-0 flex items-center gap-1"
      style={{
        background: "var(--bg-primary)",
        color: "var(--text-secondary)",
        border: "1px solid var(--border)",
        opacity: disabled ? 0.4 : 1,
        cursor: disabled ? "not-allowed" : "pointer",
      }}
      onClick={onClick}
      disabled={disabled}
      title={title}
    >
      <Trash2 size={12} />
    </button>
  );
}

function FlashLine({ flash }: { flash?: { ok: boolean; msg: string } }) {
  if (!flash) return null;
  return (
    <div
      className="mt-1 text-[10px] text-right"
      style={{ color: flash.ok ? "var(--accent)" : "var(--danger, #e06c75)" }}
    >
      {flash.msg}
    </div>
  );
}
