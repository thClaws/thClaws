import { useEffect, useMemo, useState } from "react";
import { ChevronRight, X } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";

/// Plan-mode sidebar (M1). Subscribes to `chat_plan_update` IPC events
/// from the worker and renders the plan as a vertical checklist on the
/// right edge of the window. Cowork-style: persistent panel that
/// updates live as the model marks step transitions, not a one-shot
/// modal.
///
/// M1 is read-only — Approve / Cancel / Edit buttons land in M3 once
/// the permission-mode integration is wired. For now the sidebar just
/// shows the model's current view of progress.

type StepStatus = "todo" | "in_progress" | "done" | "failed";

type PlanStep = {
  id: string;
  title: string;
  description: string;
  status: StepStatus;
  note?: string;
  output?: string;
};

type Plan = {
  id: string;
  steps: PlanStep[];
};

const STATUS_ICON: Record<StepStatus, string> = {
  todo: "☐",
  in_progress: "◉",
  done: "✓",
  failed: "✕",
};

const STATUS_COLOR: Record<StepStatus, string> = {
  todo: "var(--text-secondary)",
  in_progress: "var(--accent)",
  done: "var(--accent)",
  failed: "var(--danger, #e06c75)",
};

export function PlanSidebar() {
  const [plan, setPlan] = useState<Plan | null>(null);
  /// Permission mode for the badge + Approve/Cancel button visibility
  /// (M2). "plan" means the model is in the planning phase; once the
  /// user approves, mode flips back to "auto" / "ask" and execution
  /// begins. Default to null until the first `chat_permission_mode`
  /// envelope lands so we don't flash the wrong badge.
  const [mode, setMode] = useState<string | null>(null);
  /// User can dismiss the sidebar without clearing the plan; the
  /// chevron tab on the collapsed edge re-opens it. State is local
  /// to the component (resets on app restart) — that's intentional
  /// for M1, the sidebar always opens on the next plan update.
  const [dismissed, setDismissed] = useState(false);
  /// Briefly highlight a replan when SubmitPlan replaces an existing
  /// plan (M4.3). Without this, the sidebar steps just shuffle and
  /// the user has to re-read the list to figure out anything changed.
  const [replanBadge, setReplanBadge] = useState(false);
  /// Stalled-turn warning (M4.4). The shared-session worker tracks
  /// "consecutive turns without plan progress" and emits a
  /// `chat_plan_stalled` envelope past a 3-turn threshold so the user
  /// can intervene rather than watching a model loop indefinitely.
  /// Cleared on the next plan_update or by clicking Continue / Abort.
  const [stalled, setStalled] = useState<{
    stepId: string;
    stepTitle: string;
    turns: number;
  } | null>(null);

  useEffect(() => {
    let replanTimer: ReturnType<typeof setTimeout> | null = null;
    const unsub = subscribe((msg) => {
      if (msg.type === "chat_plan_update") {
        const next = (msg.plan as Plan | null) ?? null;
        // Replan detection: SubmitPlan generates a fresh plan id
        // each time, so an id change with both sides non-null is a
        // resubmission, not a routine status update.
        setPlan((prev) => {
          if (prev && next && prev.id !== next.id) {
            setReplanBadge(true);
            if (replanTimer) clearTimeout(replanTimer);
            replanTimer = setTimeout(() => setReplanBadge(false), 5000);
          }
          return next;
        });
        // A fresh plan resets dismissal — the sidebar re-opens on
        // every new submission so the user notices.
        if (next) setDismissed(false);
        // Any plan update means progress was made — clear any
        // stale "stuck" banner.
        setStalled(null);
      } else if (msg.type === "chat_permission_mode") {
        if (typeof msg.mode === "string") setMode(msg.mode as string);
      } else if (msg.type === "chat_plan_stalled") {
        setStalled({
          stepId: typeof msg.step_id === "string" ? (msg.step_id as string) : "",
          stepTitle:
            typeof msg.step_title === "string" ? (msg.step_title as string) : "",
          turns: typeof msg.turns === "number" ? (msg.turns as number) : 0,
        });
      }
    });
    return () => {
      if (replanTimer) clearTimeout(replanTimer);
      unsub();
    };
  }, []);

  const counts = useMemo(() => {
    if (!plan) return { done: 0, total: 0 };
    return {
      done: plan.steps.filter((s) => s.status === "done").length,
      total: plan.steps.length,
    };
  }, [plan]);

  /// True when every step has transitioned to Done (including any
  /// user-Skipped steps that force_step_done flipped). The footer
  /// celebrates this with an accent color + "All steps complete"
  /// text instead of the running tally — gives the user a clear
  /// "we're finished here" signal without a popup.
  const allDone =
    plan !== null &&
    plan.steps.length > 0 &&
    plan.steps.every((s) => s.status === "done");

  /// Show Approve/Cancel whenever the session is in plan mode AND a
  /// non-finished plan exists. Once the user clicks Approve, the
  /// backend flips mode to Auto and the broadcaster fires
  /// `chat_permission_mode` with `mode: "auto"` — `mode === "plan"`
  /// becomes false and the buttons hide naturally.
  ///
  /// M6.9 (Bug C1): also gate on `!allDone`. After a plan completes,
  /// the slot still holds the finished plan so the celebration footer
  /// can render. If the user then re-enters plan mode for a NEW task,
  /// the OLD all-done plan would briefly trigger Approve buttons here
  /// and a confusing "awaiting approval" system reminder. The
  /// `!allDone` guard prevents both.
  const showApprove = mode === "plan" && plan !== null && !allDone;

  if (!plan) return null;

  // Collapsed: just a chevron tab on the right edge that re-opens.
  if (dismissed) {
    return (
      <button
        type="button"
        onClick={() => setDismissed(false)}
        className="flex items-center justify-center shrink-0 border-l"
        style={{
          width: "20px",
          background: "var(--bg-secondary)",
          borderColor: "var(--border)",
          color: "var(--text-secondary)",
          cursor: "pointer",
        }}
        title={`Plan: ${counts.done}/${counts.total} steps complete`}
      >
        <ChevronRight size={14} style={{ transform: "rotate(180deg)" }} />
      </button>
    );
  }

  return (
    <div
      className="flex flex-col shrink-0 border-l"
      style={{
        width: "300px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{ borderColor: "var(--border)" }}
      >
        <div
          className="text-[10px] uppercase tracking-wider flex items-center gap-2"
          style={{ color: "var(--text-secondary)" }}
        >
          <span>Plan</span>
          {mode && (
            <span
              className="px-1.5 py-px rounded"
              style={{
                fontSize: "9px",
                background:
                  mode === "plan"
                    ? "var(--accent)"
                    : "var(--bg-tertiary)",
                color:
                  mode === "plan"
                    ? "var(--accent-fg, #fff)"
                    : "var(--text-secondary)",
                border:
                  mode === "plan"
                    ? "none"
                    : "1px solid var(--border)",
              }}
              title={
                mode === "plan"
                  ? "Plan mode — mutating tools blocked"
                  : `Permission mode: ${mode}`
              }
            >
              {mode.toUpperCase()}
            </span>
          )}
          {replanBadge && (
            // Brief 5-second highlight when SubmitPlan replaces an
            // existing plan (M4.3). Without it, step rearrangements
            // are easy to miss in the sidebar.
            <span
              className="px-1.5 py-px rounded"
              style={{
                fontSize: "9px",
                background: "var(--warning, #d4a72c)",
                color: "var(--warning-fg, #1a1a1a)",
                border: "none",
                animation: "plan-pulse 1.6s ease-in-out infinite",
              }}
              title="The model submitted a fresh plan — review the new steps"
            >
              ↻ REPLANNED
            </span>
          )}
        </div>
        <button
          type="button"
          onClick={() => setDismissed(true)}
          className="p-0.5 rounded hover:bg-white/10"
          style={{ color: "var(--text-secondary)" }}
          title="Hide sidebar (plan stays active)"
        >
          <X size={14} />
        </button>
      </div>

      {stalled && (
        // Stalled-turn warning (M4.4). The model has finished N
        // consecutive turns without progressing the plan; surface a
        // banner so the user can intervene instead of watching a
        // silent loop.
        <div
          className="px-3 py-2 border-b shrink-0"
          style={{
            borderColor: "var(--border)",
            background: "var(--warning-bg, rgba(212, 167, 44, 0.18))",
          }}
        >
          <div
            className="text-xs mb-1.5"
            style={{ color: "var(--warning, #d4a72c)", fontWeight: 600 }}
          >
            Model seems stuck
          </div>
          <div
            className="mb-2"
            style={{
              color: "var(--text-primary)",
              fontSize: "11px",
              lineHeight: "1.4",
            }}
          >
            {stalled.turns} turn{stalled.turns === 1 ? "" : "s"} without
            progress on step "{stalled.stepTitle}".
          </div>
          <div className="flex gap-1.5">
            <button
              type="button"
              onClick={() => {
                send({ type: "plan_stalled_continue" });
                setStalled(null);
              }}
              className="px-2 py-0.5 rounded transition-colors"
              style={{
                background: "var(--accent)",
                color: "var(--accent-fg, #fff)",
                border: "none",
                cursor: "pointer",
                fontSize: "10px",
                fontWeight: 500,
              }}
              title="Reset the stall counter and prompt the model to commit to a step transition"
            >
              Continue
            </button>
            <button
              type="button"
              onClick={() => {
                send({ type: "plan_cancel" });
                setStalled(null);
              }}
              className="px-2 py-0.5 rounded transition-colors"
              style={{
                background: "transparent",
                color: "var(--danger, #e06c75)",
                border: "1px solid var(--danger, #e06c75)",
                cursor: "pointer",
                fontSize: "10px",
              }}
              title="Discard the plan and exit plan mode"
            >
              Abort
            </button>
          </div>
        </div>
      )}

      {showApprove && (
        <div
          className="px-3 py-2 border-b shrink-0 flex gap-2"
          style={{ borderColor: "var(--border)" }}
        >
          <button
            type="button"
            onClick={() => send({ type: "plan_approve" })}
            className="flex-1 px-2 py-1 rounded text-xs font-medium transition-colors"
            style={{
              background: "var(--accent)",
              color: "var(--accent-fg, #fff)",
              border: "none",
              cursor: "pointer",
            }}
            title="Approve the plan and begin execution — mutating tools unblocked"
          >
            Approve & execute
          </button>
          <button
            type="button"
            onClick={() => send({ type: "plan_cancel" })}
            className="px-2 py-1 rounded text-xs transition-colors"
            style={{
              background: "transparent",
              color: "var(--text-secondary)",
              border: "1px solid var(--border)",
              cursor: "pointer",
            }}
            title="Discard the plan and exit plan mode"
          >
            Cancel
          </button>
        </div>
      )}

      <div className="flex-1 overflow-y-auto py-1">
        {plan.steps.map((step, idx) => {
          const icon = STATUS_ICON[step.status];
          const color = STATUS_COLOR[step.status];
          const dim =
            step.status === "todo" &&
            // Future steps are dimmed until the previous step is
            // done — visual hint that they're locked behind the
            // sequential gate.
            idx > 0 &&
            plan.steps[idx - 1].status !== "done";
          return (
            <div
              key={step.id}
              className="px-3 py-2 flex items-start gap-2"
              style={{ opacity: dim ? 0.55 : 1 }}
            >
              <span
                className="font-mono shrink-0"
                style={{
                  color,
                  width: "14px",
                  textAlign: "center",
                  fontSize: "13px",
                  marginTop: "1px",
                  // In-progress gets a subtle pulse so the user can
                  // see at a glance which step the model is on.
                  animation:
                    step.status === "in_progress"
                      ? "plan-pulse 1.6s ease-in-out infinite"
                      : "none",
                }}
              >
                {icon}
              </span>
              <div className="flex-1 min-w-0">
                <div
                  className="text-xs"
                  style={{
                    color: "var(--text-primary)",
                    textDecoration:
                      step.status === "done" ? "line-through" : "none",
                    textDecorationColor: "var(--text-secondary)",
                  }}
                >
                  {step.title}
                </div>
                {step.description && (
                  <div
                    className="mt-0.5"
                    style={{
                      color: "var(--text-secondary)",
                      fontSize: "10px",
                      lineHeight: "1.35",
                    }}
                  >
                    {step.description}
                  </div>
                )}
                {step.note && (
                  <div
                    className="mt-0.5 italic"
                    style={{
                      color:
                        step.status === "failed"
                          ? "var(--danger, #e06c75)"
                          : "var(--text-secondary)",
                      fontSize: "10px",
                      lineHeight: "1.35",
                    }}
                  >
                    {step.note}
                  </div>
                )}
                {step.output && step.status === "done" && (
                  // M6.3 cross-step output. Shown only on Done steps —
                  // outputs on other statuses aren't a stable contract.
                  // Monospace + arrow prefix to make it scannable as
                  // data, not prose.
                  <div
                    className="mt-0.5"
                    style={{
                      color: "var(--text-secondary)",
                      fontSize: "10px",
                      lineHeight: "1.35",
                      fontFamily:
                        "ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace",
                      wordBreak: "break-all",
                    }}
                    title={step.output}
                  >
                    → {step.output}
                  </div>
                )}
                {step.status === "failed" && (
                  // Failure recovery row (M4.2). Retry asks the model
                  // to re-run the same step (Failed → InProgress is a
                  // legal gate transition). Skip force-marks the step
                  // Done with a "skipped by user" note (audit trail).
                  // Abort clears the plan + restores prior mode —
                  // same as the cancel button on the toolbar.
                  <div className="mt-1.5 flex gap-1.5">
                    <button
                      type="button"
                      onClick={() =>
                        send({ type: "plan_retry_step", step_id: step.id })
                      }
                      className="px-2 py-0.5 rounded transition-colors"
                      style={{
                        background: "var(--accent)",
                        color: "var(--accent-fg, #fff)",
                        border: "none",
                        cursor: "pointer",
                        fontSize: "10px",
                        fontWeight: 500,
                      }}
                      title="Re-enter the step and try again"
                    >
                      Retry
                    </button>
                    <button
                      type="button"
                      onClick={() =>
                        send({ type: "plan_skip_step", step_id: step.id })
                      }
                      className="px-2 py-0.5 rounded transition-colors"
                      style={{
                        background: "transparent",
                        color: "var(--text-secondary)",
                        border: "1px solid var(--border)",
                        cursor: "pointer",
                        fontSize: "10px",
                      }}
                      title="Mark the step done (note: skipped by user) and proceed"
                    >
                      Skip
                    </button>
                    <button
                      type="button"
                      onClick={() => send({ type: "plan_cancel" })}
                      className="px-2 py-0.5 rounded transition-colors"
                      style={{
                        background: "transparent",
                        color: "var(--danger, #e06c75)",
                        border: "1px solid var(--danger, #e06c75)",
                        cursor: "pointer",
                        fontSize: "10px",
                      }}
                      title="Discard the plan and exit plan mode"
                    >
                      Abort
                    </button>
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </div>

      <div
        className="px-3 py-2 border-t shrink-0 flex items-center gap-1.5"
        style={{
          borderColor: "var(--border)",
          color: allDone ? "var(--accent)" : "var(--text-secondary)",
          fontSize: "10px",
          fontWeight: allDone ? 600 : 400,
        }}
      >
        {allDone ? (
          <>
            <span>✓</span>
            <span>All {counts.total} step{counts.total === 1 ? "" : "s"} complete</span>
          </>
        ) : (
          <span>
            {counts.done} of {counts.total} step{counts.total === 1 ? "" : "s"} complete
          </span>
        )}
      </div>

      <style>{`
        @keyframes plan-pulse {
          0%, 100% { opacity: 1; }
          50% { opacity: 0.45; }
        }
      `}</style>
    </div>
  );
}
