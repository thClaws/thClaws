import { useState } from "react";
import { send } from "../hooks/useIPC";

/**
 * dev-plan/32 Tier 3 workflow review bubble. Rendered inline in the
 * chat tab when the backend emits `chat_workflow_review`. The user
 * picks Approve / Cancel / Re-author; clicking posts a
 * `workflow_decision` IPC message that the Rust side routes to the
 * matching `WorkflowApprover` oneshot. Once a decision is sent the
 * bubble freezes — the backend's next event (another review request
 * for Re-author, the script run, or "/workflow run: cancelled") tells
 * the user what happened.
 */
export type WorkflowReviewProps = {
  id: string;
  script: string;
  prompt: string;
  model: string;
  revision: number;
};

export function WorkflowReviewBubble({
  id,
  script,
  prompt,
  model,
  revision,
}: WorkflowReviewProps) {
  const [resolved, setResolved] = useState<
    | { decision: "approve" }
    | { decision: "cancel" }
    | { decision: "rework"; note: string }
    | null
  >(null);
  const [reworking, setReworking] = useState(false);
  const [reworkNote, setReworkNote] = useState("");

  const send_decision = (
    decision: "approve" | "cancel" | "rework",
    note?: string,
  ) => {
    send({
      type: "workflow_decision",
      id,
      decision,
      ...(note ? { note } : {}),
    });
    if (decision === "approve") setResolved({ decision: "approve" });
    else if (decision === "cancel") setResolved({ decision: "cancel" });
    else if (decision === "rework")
      setResolved({ decision: "rework", note: note ?? "" });
  };

  const revisionLabel = revision > 0 ? ` · Revision ${revision + 1}` : "";

  return (
    <div className="flex justify-start">
      <div
        className="group flex max-w-[80%] flex-col gap-2 w-[80%]"
        style={{
          color: "var(--text-primary)",
          fontFamily: "system-ui, sans-serif",
          fontSize: "0.875rem",
        }}
      >
        <div
          className="flex flex-col gap-1 rounded-md border px-3 py-2"
          style={{
            borderColor: "var(--accent)",
            background: "var(--surface, rgba(0,0,0,0.02))",
          }}
        >
          <div
            className="flex flex-row items-baseline justify-between gap-2"
            style={{ color: "var(--text-secondary)", fontSize: "0.75rem" }}
          >
            <span>
              <span style={{ color: "var(--accent)", fontWeight: 600 }}>
                /workflow run
              </span>{" "}
              · review · model {model}
              {revisionLabel}
            </span>
            <span style={{ fontFamily: "Menlo, monospace" }}>
              {id.slice(0, 18)}…
            </span>
          </div>
          <div
            style={{
              color: "var(--text-secondary)",
              fontSize: "0.8125rem",
              fontStyle: "italic",
              marginTop: "0.25rem",
            }}
          >
            Goal: {prompt}
          </div>
          <pre
            className="rounded mt-2 px-3 py-2 overflow-x-auto"
            style={{
              background: "var(--code-bg, rgba(0,0,0,0.06))",
              color: "var(--text-primary)",
              fontFamily: "Menlo, Monaco, 'Courier New', monospace",
              fontSize: "0.8125rem",
              lineHeight: 1.5,
              whiteSpace: "pre",
              margin: 0,
            }}
          >
            {script}
          </pre>
          {resolved ? (
            <div
              className="mt-2"
              style={{
                color: "var(--text-secondary)",
                fontSize: "0.8125rem",
                fontStyle: "italic",
              }}
            >
              {resolved.decision === "approve" &&
                "→ approved · running script…"}
              {resolved.decision === "cancel" && "→ cancelled"}
              {resolved.decision === "rework" &&
                `→ re-authoring with note: ${resolved.note || "(empty)"}`}
            </div>
          ) : reworking ? (
            <div className="mt-2 flex flex-col gap-2">
              <textarea
                value={reworkNote}
                onChange={(e) => setReworkNote(e.target.value)}
                placeholder="One-line note explaining what to fix…"
                className="rounded border px-2 py-1"
                style={{
                  borderColor: "var(--accent)",
                  background: "var(--input-bg, transparent)",
                  color: "var(--text-primary)",
                  fontSize: "0.8125rem",
                  resize: "vertical",
                }}
                rows={2}
                autoFocus
              />
              <div className="flex flex-row gap-2 justify-end">
                <button
                  onClick={() => {
                    setReworking(false);
                    setReworkNote("");
                  }}
                  className="rounded px-3 py-1"
                  style={{
                    background: "transparent",
                    color: "var(--text-secondary)",
                    border: "1px solid var(--border, currentColor)",
                    fontSize: "0.8125rem",
                  }}
                >
                  Back
                </button>
                <button
                  onClick={() => send_decision("rework", reworkNote)}
                  className="rounded px-3 py-1"
                  style={{
                    background: "var(--accent)",
                    color: "var(--accent-text, white)",
                    border: "none",
                    fontSize: "0.8125rem",
                  }}
                >
                  Submit
                </button>
              </div>
            </div>
          ) : (
            <div className="mt-2 flex flex-row gap-2 justify-end">
              <button
                onClick={() => send_decision("cancel")}
                className="rounded px-3 py-1"
                style={{
                  background: "transparent",
                  color: "var(--text-secondary)",
                  border: "1px solid var(--border, currentColor)",
                  fontSize: "0.8125rem",
                }}
              >
                Cancel
              </button>
              <button
                onClick={() => setReworking(true)}
                className="rounded px-3 py-1"
                style={{
                  background: "transparent",
                  color: "var(--text-secondary)",
                  border: "1px solid var(--border, currentColor)",
                  fontSize: "0.8125rem",
                }}
              >
                Re-author
              </button>
              <button
                onClick={() => send_decision("approve")}
                className="rounded px-3 py-1"
                style={{
                  background: "var(--accent)",
                  color: "var(--accent-text, white)",
                  border: "none",
                  fontSize: "0.8125rem",
                  fontWeight: 600,
                }}
              >
                Approve
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
