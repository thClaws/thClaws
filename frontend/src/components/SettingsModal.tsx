import { useEffect, useState } from "react";
import { KeyRound, X, Check, Trash2, Link as LinkIcon } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";
import { SecretsBackendDialog } from "./SecretsBackendDialog";
import {
  isOpenRouterFreeOnly,
  setOpenRouterFreeOnly,
  refreshOpenRouterFreeOnly,
} from "./ModelPickerModal";

type KeyStatus = {
  provider: string;
  env_var: string;
  configured_in_keychain: boolean;
  env_set: boolean;
  key_length: number;
  kind?: "provider" | "service";
  // Featured-tier (gateway-routable) provider — drives the modal's
  // "Featured" vs "Additional" grouping. Representative default model shown
  // as a hint, mirroring `/providers`.
  featured?: boolean;
  default_model?: string;
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
  anthropic: "Anthropic",
  openai: "OpenAI",
  openrouter: "OpenRouter",
  gemini: "Google Gemini",
  dashscope: "Alibaba DashScope",
  ollama: "Ollama",
  "ollama-anthropic": "Ollama (Anthropic-compatible)",
  "ollama-cloud": "Ollama Cloud",
  "opencode-go": "OpenCode Go",
  moonshot: "Moonshot AI (Kimi)",
  meta: "Meta AI (Muse Spark)",
  xai: "xAI (Grok)",
  groq: "Groq",
  minimax: "MiniMax",
  azure: "Azure AI Foundry",
  vllm: "vLLM (self-hosted)",
  llamacpp: "llama.cpp (self-hosted)",
  litellm: "LiteLLM Proxy (self-hosted)",
  "openai-compat": "OpenAI-Compatible (custom endpoint)",
  tavily: "Tavily Search",
  "brave-search": "Brave Search",
  serpapi: "SerpAPI (Google Search)",
  hal: "HAL Public API (YouTube transcript + Web scrape)",
  ltx: "LTX (video generation + native audio)",
  iapp: "iApp (Thai image generation)",
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
  const [backend, setBackend] = useState<"keychain" | "dotenv" | "hosted" | null>(null);
  const [backendKnown, setBackendKnown] = useState(false);

  // Ask the backend for the stored preference first. Nothing else
  // happens until we know — no api_key_status, no keychain reads.
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "secrets_backend") {
        const value = (msg.backend as string | null) ?? null;
        // "hosted" is the engine's signal that this is a cloud
        // workspace (gateway OR BYOK) — no local key storage applies.
        // Treat it as "no chooser needed" rather than null (which
        // would re-prompt the picker pointlessly).
        const recognized =
          value === "keychain" || value === "dotenv" || value === "hosted"
            ? value
            : null;
        setBackend(recognized);
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
  // Insertion order from `keys[]` is preserved by Map — backend
  // already orders providers (LLM) first, services (search) last.
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
  const llmEntries = Array.from(providers.entries()).filter(
    ([, row]) => (row.key?.kind ?? "provider") !== "service",
  );
  // Group LLM providers like `/providers`: Featured (gateway-routable) first,
  // then Additional (BYOK). Backend already returns them in display order, so
  // each filter preserves it. Endpoint-only providers (no key row) are BYOK.
  const featuredEntries = llmEntries.filter(([, row]) => row.key?.featured === true);
  const additionalEntries = llmEntries.filter(([, row]) => row.key?.featured !== true);
  const serviceEntries = Array.from(providers.entries()).filter(
    ([, row]) => row.key?.kind === "service",
  );

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
          <CloudSection />
          <AgentIdentitySection />
          <AutoLearnSection />

          {featuredEntries.length > 0 && (
            <div className="flex items-center justify-between mt-2 gap-3">
              <div
                className="text-[10px] uppercase tracking-wider"
                style={{ color: "var(--text-secondary)" }}
              >
                Featured (gateway-routable)
              </div>
              <GatewayProxyToggle />
            </div>
          )}
          {featuredEntries.map(([provider, row]) =>
            renderProviderCard(
              provider,
              row,
              keyDrafts,
              setKeyDrafts,
              urlDrafts,
              setUrlDrafts,
              handleSaveKey,
              handleClearKey,
              handleSaveUrl,
              handleClearUrl,
              busy,
              flash,
            ),
          )}

          {additionalEntries.length > 0 && (
            <div
              className="text-[10px] uppercase tracking-wider mt-2"
              style={{ color: "var(--text-secondary)" }}
            >
              Additional (bring your own key)
            </div>
          )}
          {additionalEntries.map(([provider, row]) =>
            renderProviderCard(
              provider,
              row,
              keyDrafts,
              setKeyDrafts,
              urlDrafts,
              setUrlDrafts,
              handleSaveKey,
              handleClearKey,
              handleSaveUrl,
              handleClearUrl,
              busy,
              flash,
            ),
          )}

          {serviceEntries.length > 0 && (
            <>
              <div
                className="text-[10px] uppercase tracking-wider mt-2"
                style={{ color: "var(--text-secondary)" }}
              >
                Service keys
              </div>
              {serviceEntries.map(([provider, row]) =>
                renderProviderCard(
                  provider,
                  row,
                  keyDrafts,
                  setKeyDrafts,
                  urlDrafts,
                  setUrlDrafts,
                  handleSaveKey,
                  handleClearKey,
                  handleSaveUrl,
                  handleClearUrl,
                  busy,
                  flash,
                ),
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function renderProviderCard(
  provider: string,
  row: { key?: KeyStatus; endpoint?: EndpointStatus },
  keyDrafts: Record<string, string>,
  setKeyDrafts: React.Dispatch<React.SetStateAction<Record<string, string>>>,
  urlDrafts: Record<string, string>,
  setUrlDrafts: React.Dispatch<React.SetStateAction<Record<string, string>>>,
  handleSaveKey: (p: string) => void,
  handleClearKey: (p: string) => void,
  handleSaveUrl: (p: string) => void,
  handleClearUrl: (p: string) => void,
  busy: string | null,
  flash: Record<string, { ok: boolean; msg: string }>,
) {
  const label = PROVIDER_LABELS[provider] ?? provider;
  return (
    <div
      key={provider}
      className="rounded p-3"
      style={{ background: "var(--bg-tertiary)", border: "1px solid var(--border)" }}
    >
      <div className="flex items-center justify-between mb-2">
        <div className="flex items-baseline gap-2">
          <div className="text-xs font-semibold" style={{ color: "var(--text-primary)" }}>
            {label}
          </div>
          {row.key?.default_model && (
            <span
              className="text-[10px] font-mono truncate"
              style={{ color: "var(--text-secondary)", opacity: 0.7 }}
              title={`Default model: ${row.key.default_model}`}
            >
              → {row.key.default_model}
            </span>
          )}
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

      {provider === "openrouter" && <OpenRouterFreeOnlyToggle />}
    </div>
  );
}

/// OpenRouter-only inline toggle. When on, both the model picker
/// and the `/models` slash command hide non-free rows. Persisted
/// server-side via `openrouter_free_only_set` so the slash-command
/// handler (server-side rendering) sees the same flag the UI shows.
/// localStorage is just a fast-paint cache; the on-mount IPC fetch
/// corrects drift against `.thclaws/settings.json`.
function OpenRouterFreeOnlyToggle() {
  const [on, setOn] = useState<boolean>(() => isOpenRouterFreeOnly());
  useEffect(() => refreshOpenRouterFreeOnly(setOn), []);
  return (
    <label
      className="flex items-center gap-2 mt-2 text-xs select-none cursor-pointer"
      style={{ color: "var(--text-secondary)" }}
      title="When on, the model picker and /models slash command show only OpenRouter models with $0 prompt + $0 completion pricing."
    >
      <input
        type="checkbox"
        checked={on}
        onChange={(e) => {
          setOn(e.target.checked);
          setOpenRouterFreeOnly(e.target.checked);
        }}
      />
      <span>Free only — filter the model picker and /models to $0 / $0 pricing</span>
    </label>
  );
}


type GatewaySettings = {
  base_url: string;
  // Single proxy flag (was a per-provider list). When on + a CLI token exists,
  // every gateway-routable provider routes through the thClaws Gateway.
  proxy: boolean;
  // Whether a CLI access token is present — the toggle is enabled only then.
  has_cli_token: boolean;
};

let cachedGatewaySettings: GatewaySettings | null = null;
const gatewayListeners = new Set<(s: GatewaySettings) => void>();

function applyGatewaySettings(s: GatewaySettings) {
  cachedGatewaySettings = s;
  for (const cb of gatewayListeners) cb(s);
}

function ensureGatewaySubscription() {
  if ((ensureGatewaySubscription as { inited?: boolean }).inited) return;
  (ensureGatewaySubscription as { inited?: boolean }).inited = true;
  subscribe((msg) => {
    if (msg.type === "gateway_settings" || msg.type === "gateway_settings_result") {
      applyGatewaySettings({
        base_url: String((msg as { base_url?: string }).base_url ?? ""),
        proxy: Boolean((msg as { proxy?: boolean }).proxy),
        has_cli_token: Boolean((msg as { has_cli_token?: boolean }).has_cli_token),
      });
    }
  });
  send({ type: "gateway_settings_get" });
}

function useGatewaySettings(): GatewaySettings {
  const [state, setState] = useState<GatewaySettings>(
    () => cachedGatewaySettings ?? { base_url: "", proxy: false, has_cli_token: false },
  );
  useEffect(() => {
    ensureGatewaySubscription();
    gatewayListeners.add(setState);
    if (cachedGatewaySettings) setState(cachedGatewaySettings);
    return () => {
      gatewayListeners.delete(setState);
    };
  }, []);
  return state;
}

function persistGatewaySettings(proxy: boolean) {
  applyGatewaySettings({
    base_url: cachedGatewaySettings?.base_url ?? "",
    proxy,
    has_cli_token: cachedGatewaySettings?.has_cli_token ?? false,
  });
  send({ type: "gateway_settings_set", proxy });
}

/// Single global "Use thClaws Gateway proxy" switch. Enabled only when a CLI
/// access token is present (no token, no proxy). When on, every gateway-routable
/// provider routes through the gateway for featured (priced) models; everything
/// else stays BYOK. Replaces the old per-provider checkboxes.
function GatewayProxyToggle() {
  const settings = useGatewaySettings();
  const disabled = !settings.has_cli_token;
  return (
    <label
      className="flex items-center gap-2 text-xs select-none"
      style={{
        color: "var(--text-secondary)",
        cursor: disabled ? "not-allowed" : "pointer",
        opacity: disabled ? 0.5 : 1,
      }}
      title={
        disabled
          ? "Sign in to thClaws Cloud (CLI token) to use the gateway proxy."
          : "Route featured models through the thClaws Gateway (billed to your cloud account). Other models stay BYOK."
      }
    >
      <input
        type="checkbox"
        checked={settings.proxy && !disabled}
        disabled={disabled}
        onChange={(e) => persistGatewaySettings(e.target.checked)}
      />
      <span>Use Gateway Proxy{disabled ? " (needs CLI token)" : ""}</span>
    </label>
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

// ── thClaws.cloud (dev-plan/34) ──────────────────────────────────────
// URL persists to settings.json::cloud.url; CLI token persists to the
// active secrets backend (keychain or ~/.config/thclaws/.env). The engine
// ipc handler pair is cloud_config_get / cloud_config_set.
interface CloudConfig {
  url: string | null;
  default_url: string;
  has_token: boolean;
  token_length: number;
  env_var_set: boolean;
  token_writable: boolean;
}

// dev-plan/51 P3b: runs a `/cloud push|pull` slash command in the shared
// session (output streams to Chat) — pure convenience over typing the slash.
function SyncButton({ label, cmd, enabled }: { label: string; cmd: string; enabled: boolean }) {
  return (
    <button
      onClick={() => send({ type: "shell_input", text: cmd })}
      disabled={!enabled}
      className="px-2.5 py-1.5 rounded text-xs font-medium whitespace-nowrap"
      style={{
        background: enabled ? "var(--accent)" : "var(--bg-primary)",
        color: enabled ? "#fff" : "var(--text-secondary)",
        border: "1px solid var(--border)",
        cursor: enabled ? "pointer" : "default",
        opacity: enabled ? 1 : 0.6,
      }}
      title={enabled ? `Run ${cmd} (watch output in Chat)` : "Save a thClaws.cloud CLI token above first"}
    >
      {label}
    </button>
  );
}

function CloudSection() {
  const [cfg, setCfg] = useState<CloudConfig>({
    url: null,
    default_url: "https://thclaws.cloud",
    has_token: false,
    token_length: 0,
    env_var_set: false,
    token_writable: true,
  });
  const [urlDraft, setUrlDraft] = useState("");
  const [tokenDraft, setTokenDraft] = useState("");
  const [flash, setFlash] = useState<{ ok: boolean; msg: string } | undefined>(undefined);
  // dev-plan/44: phone-home pairing status (the ack from the IPC layer;
  // the actual pair → connect runs on the worker and streams to chat).
  const [phoneHome, setPhoneHome] = useState<{ pending: boolean; msg?: string }>({
    pending: false,
  });

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "cloud_config") {
        const next = msg as unknown as CloudConfig & { type: string };
        setCfg({
          url: next.url ?? null,
          default_url: next.default_url ?? "https://thclaws.cloud",
          has_token: !!next.has_token,
          token_length: typeof next.token_length === "number" ? next.token_length : 0,
          env_var_set: !!next.env_var_set,
          token_writable: !!next.token_writable,
        });
      } else if (msg.type === "phone_home_pair_ack") {
        const r = msg as { ok?: boolean; error?: string; pending?: boolean };
        if (r.ok && r.pending) setPhoneHome({ pending: true, msg: "pairing… (see chat)" });
        else if (!r.ok) setPhoneHome({ pending: false, msg: r.error ?? "pairing failed" });
      } else if (msg.type === "cloud_config_result") {
        const r = msg as {
          url_ok?: boolean;
          url_error?: string;
          token_ok?: boolean;
          token_error?: string;
        };
        if (r.url_ok === false || r.token_ok === false) {
          const parts: string[] = [];
          if (r.url_ok === false) parts.push(`URL: ${r.url_error ?? "failed"}`);
          if (r.token_ok === false) parts.push(`Token: ${r.token_error ?? "failed"}`);
          setFlash({ ok: false, msg: parts.join(" · ") });
        } else {
          setFlash({ ok: true, msg: "saved" });
          setUrlDraft("");
          setTokenDraft("");
          setTimeout(() => setFlash(undefined), 2500);
        }
        send({ type: "cloud_config_get" });
      }
    });
    send({ type: "cloud_config_get" });
    return unsub;
  }, []);

  const onSaveUrl = () => {
    const trimmed = urlDraft.trim();
    if (!trimmed) return;
    send({ type: "cloud_config_set", url: trimmed });
  };
  const onClearUrl = () => {
    send({ type: "cloud_config_set", url: "" });
    setUrlDraft("");
  };
  const onSaveToken = () => {
    const trimmed = tokenDraft.trim();
    if (!trimmed || isSentinel(trimmed)) return;
    send({ type: "cloud_config_set", token: trimmed });
  };
  const onClearToken = () => {
    send({ type: "cloud_config_set", token: "" });
    setTokenDraft("");
  };
  const onPhoneHome = () => {
    setPhoneHome({ pending: true, msg: "pairing…" });
    send({ type: "phone_home_pair" });
  };

  const urlValue = urlDraft || cfg.url || "";
  const urlDirty = urlValue.trim() !== (cfg.url ?? "").trim() && urlValue.trim().length > 0;

  const tokenSentinel = cfg.has_token
    ? cfg.token_length > 0
      ? sentinelFor(cfg.token_length)
      : FALLBACK_SENTINEL
    : "";
  const tokenValue = tokenDraft || tokenSentinel;
  const tokenDirty = tokenDraft.trim().length > 0 && !isSentinel(tokenDraft.trim());

  return (
    <div
      className="rounded-lg p-3"
      style={{ background: "var(--bg-tertiary)", border: "1px solid var(--border)" }}
    >
      <div className="flex items-center gap-2 mb-1">
        <LinkIcon size={12} style={{ color: "var(--accent)" }} />
        <span className="text-sm font-semibold" style={{ color: "var(--text-primary)" }}>
          thClaws.cloud
        </span>
      </div>
      <p className="text-xs mb-2" style={{ color: "var(--text-secondary)" }}>
        Catalog of folder-shaped AI agents. Mint a CLI token from the
        dashboard at
        <span className="font-mono"> {(cfg.url ?? cfg.default_url) + "/dashboard"}</span>{" "}
        and paste it below — every cloud action
        (<span className="font-mono">/cloud get</span>,{" "}
        <span className="font-mono">/cloud publish</span>,{" "}
        <span className="font-mono">/cloud list</span>) uses it via the
        Authorization header, no shell-env round-trip.
      </p>

      <FieldLabel
        icon={<LinkIcon size={11} />}
        text="Catalog URL"
        env="cloud.url in settings.json"
      />
      <div className="flex gap-1.5 mb-2">
        <input
          type="text"
          placeholder={cfg.default_url}
          className="flex-1 px-2.5 py-1.5 rounded text-xs font-mono outline-none"
          style={{
            background: "var(--bg-primary)",
            color: "var(--text-primary)",
            border: "1px solid var(--border)",
          }}
          value={urlValue}
          onChange={(e) => setUrlDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onSaveUrl();
          }}
          autoComplete="off"
        />
        <SaveButton onClick={onSaveUrl} disabled={!urlDirty} />
        <ClearButton onClick={onClearUrl} disabled={!cfg.url} title="Clear configured URL" />
      </div>

      <FieldLabel
        icon={<KeyRound size={11} />}
        text="CLI token"
        env="THCLAWS_CLOUD_TOKEN"
      />
      <div className="flex gap-1.5">
        <input
          type={isSentinel(tokenValue) ? "text" : "password"}
          placeholder="Paste token from /dashboard (starts with thc_)"
          className="flex-1 px-2.5 py-1.5 rounded text-xs font-mono outline-none"
          style={{
            background: "var(--bg-primary)",
            color: "var(--text-primary)",
            border: "1px solid var(--border)",
          }}
          value={tokenValue}
          onChange={(e) => setTokenDraft(e.target.value)}
          onFocus={(e) => {
            if (isSentinel(e.currentTarget.value)) e.currentTarget.select();
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") onSaveToken();
          }}
          autoComplete="off"
          spellCheck={false}
        />
        <SaveButton onClick={onSaveToken} disabled={!tokenDirty} />
        <ClearButton
          onClick={onClearToken}
          disabled={!cfg.has_token}
          title="Clear stored token"
        />
      </div>
      <FlashLine flash={flash} />

      {/* dev-plan/44: phone home — let thClaws.cloud reach this machine's
          agent over an outbound tunnel (no public IP / inbound port). */}
      <div className="mt-3 pt-3" style={{ borderTop: "1px solid var(--border)" }}>
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <div className="text-xs font-semibold" style={{ color: "var(--text-primary)" }}>
              📡 thClaws Remote
            </div>
            <div className="text-xs" style={{ color: "var(--text-secondary)" }}>
              Let thClaws.cloud reach this machine&apos;s agent — outbound tunnel, no public
              IP or open port.
            </div>
          </div>
          <button
            onClick={onPhoneHome}
            disabled={!cfg.has_token || phoneHome.pending}
            className="px-2.5 py-1.5 rounded text-xs font-medium whitespace-nowrap"
            style={{
              background: cfg.has_token ? "var(--accent)" : "var(--bg-primary)",
              color: cfg.has_token ? "#fff" : "var(--text-secondary)",
              border: "1px solid var(--border)",
              cursor: cfg.has_token && !phoneHome.pending ? "pointer" : "default",
              opacity: cfg.has_token && !phoneHome.pending ? 1 : 0.6,
            }}
            title={
              cfg.has_token
                ? "Pair this machine with your thClaws.cloud account"
                : "Save a thClaws.cloud CLI token above first"
            }
          >
            {phoneHome.pending ? "Enabling…" : "Enable"}
          </button>
        </div>
        {phoneHome.msg && (
          <div className="text-xs mt-1" style={{ color: "var(--text-secondary)" }}>
            {phoneHome.msg}
          </div>
        )}
      </div>

      {/* dev-plan/51: workspace sync — mirror this folder to/from a hosted
          cloud workspace. Runs /cloud push|pull in the shared session; output
          streams to Chat (close Settings to watch). */}
      <div className="mt-3 pt-3" style={{ borderTop: "1px solid var(--border)" }}>
        <div className="text-xs font-semibold mb-1" style={{ color: "var(--text-primary)" }}>
          🔄 Workspace sync
        </div>
        <div className="text-xs mb-2" style={{ color: "var(--text-secondary)" }}>
          Mirror this folder to/from your hosted cloud workspace. Runs in Chat —
          close Settings to watch the result.
        </div>
        <div className="flex gap-1.5">
          <SyncButton label="⬆ Push to cloud" cmd="/cloud push" enabled={cfg.has_token} />
          <SyncButton label="⬇ Pull from cloud" cmd="/cloud pull" enabled={cfg.has_token} />
        </div>
      </div>
    </div>
  );
}

// ── Agent identity (dev-plan/34 Option A) ───────────────────────────
// settings.json::agent — this folder's authoritative agent identity.
// UUID is read-only (server-managed by `cloud publish`); the other
// three are editable. Pairs with the thClaws.cloud section above.
interface AgentConfig {
  exists: boolean;
  id: string | null;
  name: string | null;
  description: string | null;
  uuid: string | null;
}

function AgentIdentitySection() {
  const [cfg, setCfg] = useState<AgentConfig>({
    exists: false,
    id: null,
    name: null,
    description: null,
    uuid: null,
  });
  const [idDraft, setIdDraft] = useState("");
  const [nameDraft, setNameDraft] = useState("");
  const [descDraft, setDescDraft] = useState("");
  const [touched, setTouched] = useState({ id: false, name: false, description: false });
  const [flash, setFlash] = useState<{ ok: boolean; msg: string } | undefined>(undefined);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "agent_config") {
        const next = msg as unknown as AgentConfig & { type: string };
        setCfg(next);
        setIdDraft("");
        setNameDraft("");
        setDescDraft("");
        setTouched({ id: false, name: false, description: false });
      } else if (msg.type === "agent_config_result") {
        const r = msg as { ok?: boolean; error?: string };
        if (r.ok) {
          setFlash({ ok: true, msg: "saved" });
          setTimeout(() => setFlash(undefined), 2500);
          send({ type: "agent_config_get" });
        } else {
          setFlash({ ok: false, msg: r.error ?? "failed" });
        }
      } else if (msg.type === "agent_unbind_result") {
        const r = msg as { ok?: boolean; error?: string; had_uuid?: boolean };
        if (r.ok) {
          setFlash({
            ok: true,
            msg: r.had_uuid ? "unbound — next publish creates a new entry" : "already unbound",
          });
          setTimeout(() => setFlash(undefined), 3500);
          send({ type: "agent_config_get" });
        } else {
          setFlash({ ok: false, msg: r.error ?? "failed" });
        }
      }
    });
    send({ type: "agent_config_get" });
    return unsub;
  }, []);

  const idValue = touched.id ? idDraft : cfg.id ?? "";
  const nameValue = touched.name ? nameDraft : cfg.name ?? "";
  const descValue = touched.description ? descDraft : cfg.description ?? "";

  const dirty =
    (touched.id && idValue.trim() !== (cfg.id ?? "").trim()) ||
    (touched.name && nameValue.trim() !== (cfg.name ?? "").trim()) ||
    (touched.description && descValue.trim() !== (cfg.description ?? "").trim());

  const onSave = () => {
    const payload: { type: "agent_config_set"; id?: string; name?: string; description?: string } = {
      type: "agent_config_set",
    };
    if (touched.id) payload.id = idValue.trim();
    if (touched.name) payload.name = nameValue.trim();
    if (touched.description) payload.description = descValue.trim();
    send(payload);
  };

  const onUnbind = () => {
    if (!cfg.uuid) return;
    send({ type: "agent_unbind" });
  };

  const onInitialize = () => {
    setTouched({ id: true, name: true, description: true });
    setIdDraft("");
    setNameDraft("");
    setDescDraft("");
  };

  return (
    <div
      className="rounded-lg p-3"
      style={{ background: "var(--bg-tertiary)", border: "1px solid var(--border)" }}
    >
      <div className="flex items-center gap-2 mb-1">
        <KeyRound size={12} style={{ color: "var(--accent)" }} />
        <span className="text-sm font-semibold" style={{ color: "var(--text-primary)" }}>
          Agent identity
        </span>
      </div>
      <p className="text-xs mb-2" style={{ color: "var(--text-secondary)" }}>
        This folder's catalog identity, stored in{" "}
        <span className="font-mono">./.thclaws/settings.json::agent</span>.{" "}
        <span className="font-mono">cloud publish</span> reads from here; the UUID is set by the
        server on first publish.
      </p>

      {!cfg.exists && !touched.id && !touched.name && !touched.description && (
        <div className="mb-2">
          <p className="text-xs mb-2" style={{ color: "var(--text-secondary)" }}>
            No <span className="font-mono">agent</span> block yet — initialize one to publish this
            folder as an agent.
          </p>
          <button
            type="button"
            className="px-2.5 py-1.5 rounded text-xs font-medium"
            style={{
              background: "var(--bg-primary)",
              color: "var(--accent)",
              border: "1px solid var(--accent)",
            }}
            onClick={onInitialize}
          >
            Initialize agent block
          </button>
        </div>
      )}

      {(cfg.exists || touched.id || touched.name || touched.description) && (
        <>
          <FieldLabel
            icon={<KeyRound size={11} />}
            text="Slug (id)"
            env="settings.json::agent.id"
          />
          <input
            type="text"
            placeholder="my-agent"
            className="w-full px-2.5 py-1.5 rounded text-xs font-mono outline-none mb-2"
            style={{
              background: "var(--bg-primary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
            value={idValue}
            onChange={(e) => {
              setIdDraft(e.target.value);
              setTouched((t) => ({ ...t, id: true }));
            }}
            autoComplete="off"
            spellCheck={false}
          />

          <FieldLabel
            icon={<KeyRound size={11} />}
            text="Display name"
            env="settings.json::agent.name"
          />
          <input
            type="text"
            placeholder="My Agent"
            className="w-full px-2.5 py-1.5 rounded text-xs outline-none mb-2"
            style={{
              background: "var(--bg-primary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
            value={nameValue}
            onChange={(e) => {
              setNameDraft(e.target.value);
              setTouched((t) => ({ ...t, name: true }));
            }}
            autoComplete="off"
          />

          <FieldLabel
            icon={<KeyRound size={11} />}
            text="Description"
            env="settings.json::agent.description"
          />
          <textarea
            placeholder="One-line pitch shown on the catalog card."
            rows={2}
            className="w-full px-2.5 py-1.5 rounded text-xs outline-none mb-2 resize-none"
            style={{
              background: "var(--bg-primary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
            }}
            value={descValue}
            onChange={(e) => {
              setDescDraft(e.target.value);
              setTouched((t) => ({ ...t, description: true }));
            }}
            autoComplete="off"
          />

          <div className="flex justify-end mb-2">
            <SaveButton onClick={onSave} disabled={!dirty} />
          </div>

          <FieldLabel
            icon={<LinkIcon size={11} />}
            text="UUID (read-only — server-assigned)"
            env="settings.json::agent.uuid"
          />
          <div className="flex gap-1.5 items-center">
            <input
              type="text"
              readOnly
              className="flex-1 px-2.5 py-1.5 rounded text-xs font-mono outline-none"
              style={{
                background: "var(--bg-primary)",
                color: cfg.uuid ? "var(--text-primary)" : "var(--text-secondary)",
                border: "1px solid var(--border)",
              }}
              value={cfg.uuid ?? "(unbound — next publish creates new entry)"}
            />
            <button
              type="button"
              className="px-2.5 py-1.5 rounded text-xs font-medium disabled:opacity-50"
              style={{
                background: "transparent",
                color: "var(--danger, #e06c75)",
                border: "1px solid var(--danger, #e06c75)",
              }}
              disabled={!cfg.uuid}
              onClick={onUnbind}
              title="Clear the UUID so the next publish creates a new catalog entry. Use when forking."
            >
              Unbind
            </button>
          </div>

          <FlashLine flash={flash} />
        </>
      )}
    </div>
  );
}

/// Project-scope toggle for `auto_learn` (dev-plan/27). The setting
/// drives session-end ingestion into the project's KMS plus the
/// throttled reconcile pass. Reading via `auto_learn_get` returns
/// the effective AppConfig value (project overlay wins over user
/// settings); writing via `auto_learn_set` updates the project's
/// `.thclaws/settings.json`. The shell reloads its AppConfig
/// immediately so the next session-end pass sees the new state
/// without a restart.
function AutoLearnSection() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [flash, setFlash] = useState<{ ok: boolean; msg: string } | undefined>(undefined);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "auto_learn") {
        setEnabled(Boolean((msg as { enabled?: boolean }).enabled));
      } else if (msg.type === "auto_learn_result") {
        const r = msg as { ok?: boolean; error?: string; enabled?: boolean };
        if (r.ok === false) {
          setFlash({ ok: false, msg: r.error ?? "failed" });
        } else {
          setEnabled(Boolean(r.enabled));
          setFlash({ ok: true, msg: r.enabled ? "auto-learn enabled" : "auto-learn disabled" });
          setTimeout(() => setFlash(undefined), 2500);
        }
      }
    });
    send({ type: "auto_learn_get" });
    return unsub;
  }, []);

  const onToggle = (next: boolean) => {
    // Optimistic update — the `auto_learn_result` reply will
    // overwrite this with the canonical value if the save failed.
    setEnabled(next);
    send({ type: "auto_learn_set", enabled: next });
  };

  return (
    <div
      className="rounded p-3"
      style={{ background: "var(--bg-tertiary)", border: "1px solid var(--border)" }}
    >
      <div className="flex items-center gap-2 mb-1">
        <span className="text-xs font-semibold" style={{ color: "var(--text-primary)" }}>
          Auto-learn
        </span>
        <span className="text-[10px]" style={{ color: "var(--text-secondary)" }}>
          project-scope · saved to .thclaws/settings.json
        </span>
      </div>
      <p className="text-xs mb-2" style={{ color: "var(--text-secondary)" }}>
        At the end of each session, ingest the conversation into the project's KMS
        (one page per session) and reconcile the index periodically. The model
        can search and cite prior sessions on later turns. Token cost applies on
        ingest + reconcile.
      </p>
      <label
        className="flex items-center gap-2 text-xs select-none cursor-pointer"
        style={{ color: "var(--text-primary)" }}
      >
        <input
          type="checkbox"
          checked={enabled === true}
          disabled={enabled === null}
          onChange={(e) => onToggle(e.target.checked)}
        />
        <span>
          Enable auto-learn for this project
        </span>
      </label>
      <FlashLine flash={flash} />
    </div>
  );
}
