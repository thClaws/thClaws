import { useEffect, useRef, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

/// Modal for KMS create / rename / delete dialogs, opened from the
/// sidebar buttons + the page-row context menu. Replaces the old
/// `window.prompt()/confirm()` flow, which silently fails inside the
/// wry webview. Modes:
///   - `{ kind: "kms" }`            → new KMS base (name + scope).
///   - `{ kind: "page", kms }`      → new blank page (title/topic/…).
///   - `{ kind: "rename", kms, name }` → rename a page (new name).
///   - `{ kind: "delete", kms, name }` → confirm + delete a page.
/// Self-contained: parent renders it only when open, passes `onClose`.
/// Submits via IPC and dismisses on the matching `*_result` envelope.
export type KmsCreateMode =
  | { kind: "kms" }
  | { kind: "page"; kms: string }
  | { kind: "rename"; kms: string; name: string }
  | { kind: "delete"; kms: string; name: string };

interface Props {
  mode: KmsCreateMode;
  onClose: () => void;
}

const RESULT_EVENT: Record<KmsCreateMode["kind"], string> = {
  kms: "kms_new_result",
  page: "kms_new_page_result",
  rename: "kms_rename_page_result",
  delete: "kms_delete_page_result",
};

const inputStyle: React.CSSProperties = {
  background: "var(--bg-secondary)",
  borderColor: "var(--border)",
  color: "var(--text-primary)",
};

export function KmsCreateModal({ mode, onClose }: Props) {
  // New-KMS fields.
  const [name, setName] = useState("");
  const [scope, setScope] = useState<"project" | "user">("project");
  // New-page fields.
  const [title, setTitle] = useState("");
  const [topic, setTopic] = useState("");
  const [category, setCategory] = useState("");
  const [tags, setTags] = useState("");
  // Rename field (prefilled with the current name).
  const [newName, setNewName] = useState(
    mode.kind === "rename" ? mode.name : "",
  );

  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const firstFieldRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    firstFieldRef.current?.focus();
    firstFieldRef.current?.select();
  }, []);

  // Dismiss when the matching result arrives; surface the error inline
  // on failure so the user can correct + retry.
  useEffect(() => {
    const wanted = RESULT_EVENT[mode.kind];
    const unsub = subscribe((msg) => {
      if (msg.type !== wanted) return;
      setSubmitting(false);
      if (msg.ok) onClose();
      else setError((msg.error as string) ?? "operation failed");
    });
    return unsub;
  }, [mode.kind, onClose]);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (submitting) return;
    setError(null);
    if (mode.kind === "kms") {
      const n = name.trim();
      if (!n) return setError("name required");
      setSubmitting(true);
      send({ type: "kms_new", name: n, scope });
    } else if (mode.kind === "page") {
      const t = title.trim();
      if (!t) return setError("title required");
      setSubmitting(true);
      send({
        type: "kms_new_page",
        kms: mode.kms,
        title: t,
        topic: topic.trim(),
        category: category.trim(),
        tags: tags.trim(),
      });
    } else if (mode.kind === "rename") {
      const nn = newName.trim();
      if (!nn) return setError("new name required");
      if (nn === mode.name) return onClose(); // unchanged
      setSubmitting(true);
      send({
        type: "kms_rename_page",
        kms: mode.kms,
        name: mode.name,
        new_name: nn,
      });
    } else {
      setSubmitting(true);
      send({ type: "kms_delete_page", kms: mode.kms, name: mode.name });
    }
  };

  const heading =
    mode.kind === "kms"
      ? "New KMS"
      : mode.kind === "page"
        ? `New page · ${mode.kms}`
        : mode.kind === "rename"
          ? `Rename page · ${mode.kms}`
          : `Delete page · ${mode.kms}`;

  const isDelete = mode.kind === "delete";
  const submitLabel = submitting
    ? isDelete
      ? "Deleting…"
      : "Saving…"
    : isDelete
      ? "Delete"
      : mode.kind === "rename"
        ? "Rename"
        : "Create";

  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center"
      style={{ background: "var(--modal-backdrop, rgba(0,0,0,0.55))" }}
      onClick={onClose}
      onKeyDown={(e) => {
        if (e.key === "Escape") onClose();
      }}
    >
      <form
        className="rounded-lg border shadow-xl w-[460px] max-w-[92vw] max-h-[90vh] overflow-auto"
        style={{
          background: "var(--bg-primary)",
          borderColor: "var(--border)",
          color: "var(--text-primary)",
        }}
        onClick={(e) => e.stopPropagation()}
        onSubmit={submit}
      >
        <div
          className="px-4 py-2 border-b text-sm font-semibold flex items-center gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <span style={{ color: isDelete ? "var(--danger, #e06c75)" : "var(--accent)" }}>
            ●
          </span>
          <span>{heading}</span>
        </div>

        <div className="px-4 py-3 space-y-3 text-xs">
          {mode.kind === "kms" && (
            <>
              <Field label="Name" hint="Letters, digits, -, _">
                <input
                  ref={firstFieldRef}
                  type="text"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  required
                  placeholder="my-notes"
                  className="w-full px-2 py-1.5 rounded border font-mono text-xs"
                  style={inputStyle}
                />
              </Field>
              <Field
                label="Scope"
                hint={
                  scope === "user"
                    ? "~/.config/thclaws/kms/ (all projects)"
                    : "./.thclaws/kms/ (this project only)"
                }
              >
                <div className="flex gap-1.5">
                  {(["project", "user"] as const).map((s) => {
                    const active = scope === s;
                    return (
                      <button
                        key={s}
                        type="button"
                        onClick={() => setScope(s)}
                        className="flex-1 px-2 py-1.5 rounded border text-xs capitalize transition-colors"
                        style={{
                          background: active ? "var(--accent)" : "var(--bg-secondary)",
                          borderColor: active ? "var(--accent)" : "var(--border)",
                          color: active
                            ? "var(--accent-fg, #fff)"
                            : "var(--text-secondary)",
                        }}
                      >
                        {s}
                      </button>
                    );
                  })}
                </div>
              </Field>
            </>
          )}

          {mode.kind === "page" && (
            <>
              <Field label="Title" hint="Page heading (filename is derived from it)">
                <input
                  ref={firstFieldRef}
                  type="text"
                  value={title}
                  onChange={(e) => setTitle(e.target.value)}
                  required
                  placeholder="How retries work"
                  className="w-full px-2 py-1.5 rounded border text-xs"
                  style={inputStyle}
                />
              </Field>
              <Field label="Topic" hint="One-line description (shown under the title)">
                <input
                  type="text"
                  value={topic}
                  onChange={(e) => setTopic(e.target.value)}
                  placeholder="exponential backoff + jitter"
                  className="w-full px-2 py-1.5 rounded border text-xs"
                  style={inputStyle}
                />
              </Field>
              <Field label="Category" hint="Optional — groups the page in the index">
                <input
                  type="text"
                  value={category}
                  onChange={(e) => setCategory(e.target.value)}
                  placeholder="networking"
                  className="w-full px-2 py-1.5 rounded border text-xs"
                  style={inputStyle}
                />
              </Field>
              <Field label="Tags" hint="Optional — comma-separated">
                <input
                  type="text"
                  value={tags}
                  onChange={(e) => setTags(e.target.value)}
                  placeholder="retry, http, resilience"
                  className="w-full px-2 py-1.5 rounded border text-xs"
                  style={inputStyle}
                />
              </Field>
            </>
          )}

          {mode.kind === "rename" && (
            <Field
              label="New name"
              hint="Filename is re-slugified; inbound links + the index are updated"
            >
              <input
                ref={firstFieldRef}
                type="text"
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                required
                className="w-full px-2 py-1.5 rounded border text-xs"
                style={inputStyle}
              />
            </Field>
          )}

          {mode.kind === "delete" && (
            <div className="text-xs leading-relaxed">
              Delete page{" "}
              <code
                className="px-1 rounded"
                style={{ background: "var(--bg-secondary)" }}
              >
                {mode.name}
              </code>
              ? Its index entry is removed. Inbound links from other pages are
              left in place (they'll point at a missing page). This can't be
              undone.
            </div>
          )}

          {error && (
            <div className="text-xs" style={{ color: "var(--danger, #e06c75)" }}>
              {error}
            </div>
          )}
        </div>

        <div
          className="px-4 py-2.5 border-t flex justify-end gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1.5 rounded border text-xs"
            style={{
              background: "var(--bg-secondary)",
              borderColor: "var(--border)",
              color: "var(--text-secondary)",
            }}
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={submitting}
            className="px-3 py-1.5 rounded text-xs font-medium"
            style={{
              background: isDelete ? "var(--danger, #e06c75)" : "var(--accent)",
              color: "var(--accent-fg, #fff)",
              opacity: submitting ? 0.6 : 1,
              cursor: submitting ? "default" : "pointer",
            }}
          >
            {submitLabel}
          </button>
        </div>
      </form>
    </div>
  );
}

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span
        className="block mb-1 text-[11px] uppercase tracking-wide"
        style={{ color: "var(--text-secondary)" }}
      >
        {label}
      </span>
      {children}
      {hint && (
        <span
          className="block mt-1 text-[10px]"
          style={{ color: "var(--text-secondary)" }}
        >
          {hint}
        </span>
      )}
    </label>
  );
}
