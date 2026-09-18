export type Frame = { type: string; [key: string]: unknown };

const inputs = new Set([
  "shell_input",
  "chat_prompt",
  "pty_write",
  "user_input_inject",
]);
const controls = new Set([
  "shell_cancel",
  "approval_response",
  "ask_user_response",
  "workflow_decision",
  "plan_approve",
  "plan_reject",
  "plan_cancel",
  "goal_set",
  "goal_clear",
  "goal_continue",
]);

/** One presentation cursor per bot/transport. The lead has one executor; teammates have independent executors. */
export class SessionNavigation {
  viewed: string | null = null;
  execution: string | null = null;
  busy: string | null = null;
  teamAgent: string | null = null;
  private teamRunning = false;
  private teamLive = false;
  private teamOffset = 0;
  private polling = false;
  private owners = new Map<string, string>();
  private request: string | null = null;
  private sequence = 0;
  private asks = new Map<string, Frame>();

  private emit: (frame: Frame) => void;
  private transmit: (frame: Frame) => void;
  constructor(emit: (frame: Frame) => void, transmit: (frame: Frame) => void) {
    this.emit = emit;
    this.transmit = transmit;
  }

  actionError(): string | null {
    if (this.request) return "Loading session…";
    if (this.teamAgent && !this.teamLive)
      return "Archived teammate session. Open its current session from Team to send a message.";
    if (!this.teamAgent && this.busy && this.viewed !== this.busy)
      return `Session ${this.busy} is running. Wait for it to finish before sending here.`;
    return null;
  }

  private state() {
    this.emit({
      type: "session_view_state",
      session_id: this.viewed,
      team_agent: this.teamAgent,
      running: this.teamAgent
        ? this.teamRunning
        : this.busy === this.viewed && this.busy !== null,
      blocked: this.actionError(),
    });
  }

  send(frame: Frame): boolean {
    if (frame.type === "session_load" || frame.type === "new_session") {
      if (
        frame.type === "session_load" &&
        frame.id === this.viewed &&
        !this.request &&
        !frame.refresh
      ) {
        if (frame.team_agent)
          this.emit({
            type: "session_view_selected",
            session_id: this.viewed,
            team_agent: this.teamAgent,
          });
        return false;
      }
      this.request = crypto.randomUUID();
      this.transmit({
        ...frame,
        ...(frame.team_agent || !this.owners.has(String(frame.id))
          ? {}
          : { team_agent: this.owners.get(String(frame.id)) }),
        view_request: this.request,
      });
      this.state();
      return true;
    }
    if (
      this.teamAgent &&
      (inputs.has(frame.type) ||
        controls.has(frame.type) ||
        frame.type.startsWith("plan_") ||
        frame.type.startsWith("goal_"))
    ) {
      if (this.actionError()) {
        this.emit({
          type: "session_action_rejected",
          text: this.actionError(),
        });
        return false;
      }
      if (inputs.has(frame.type)) {
        if (Array.isArray(frame.attachments) && frame.attachments.length) {
          this.emit({
            type: "session_action_rejected",
            text: "Send a shared file path to this teammate; team messages currently carry text only.",
          });
          return false;
        }
        this.transmit({
          type: "team_send_message",
          to: this.teamAgent,
          text: frame.text,
          session_id: this.viewed,
        });
      } else if (frame.type === "shell_cancel") {
        this.transmit({
          type: "team_abort_turn",
          to: this.teamAgent,
          session_id: this.viewed,
        });
      } else {
        this.emit({
          type: "session_action_rejected",
          text: `Send instructions to ${this.teamAgent} through its team message box.`,
        });
        return false;
      }
      return true;
    }
    if (
      inputs.has(frame.type) ||
      controls.has(frame.type) ||
      frame.type.startsWith("plan_") ||
      frame.type.startsWith("goal_")
    ) {
      const error = this.actionError();
      if (error) {
        this.emit({ type: "session_action_rejected", text: error });
        return false;
      }
      if (frame.type === "ask_user_response" && this.viewed)
        this.asks.delete(this.viewed);
      const target = this.viewed || this.execution;
      this.transmit({ ...frame, ...(target ? { session_id: target } : {}) });
      return true;
    }
    this.transmit(frame);
    return true;
  }

  poll() {
    if (
      !this.teamAgent ||
      !this.teamLive ||
      !this.viewed ||
      this.request ||
      this.polling
    )
      return;
    this.polling = true;
    this.transmit({
      type: "team_session_poll",
      team_agent: this.teamAgent,
      session_id: this.viewed,
      offset: this.teamOffset,
    });
  }

  receive(frame: Frame) {
    if (frame.type === "team_status" && Array.isArray(frame.agents)) {
      for (const agent of frame.agents as {
        name: string;
        session_id?: string;
      }[]) {
        if (agent.name !== "lead" && agent.session_id)
          this.owners.set(agent.session_id, agent.name);
      }
    }
    if (frame.type === "team_session_events") {
      if (
        frame.session_id !== this.viewed ||
        frame.team_agent !== this.teamAgent ||
        frame.offset !== this.teamOffset
      )
        return;
      this.polling = false;
      this.teamOffset = Number(frame.next_offset);
      this.teamRunning = frame.running === true;
      this.teamLive = frame.live !== false;
      for (const event of frame.events as Frame[]) this.emit(event);
      this.state();
      return;
    }
    if (
      frame.type === "session_action_rejected" ||
      frame.type === "team_message_queued"
    ) {
      if (frame.session_id && frame.session_id !== this.viewed) return;
      this.polling = false;
      if (frame.session_unavailable) {
        this.teamLive = false;
        this.state();
      }
    }
    if (frame.type === "session_execution") {
      if (typeof frame.session_id !== "string" || !frame.session_id) return;
      const previous = this.execution;
      this.execution = String(frame.session_id);
      if (!this.request && (!this.viewed || this.viewed === previous)) {
        if (this.viewed !== this.execution) {
          this.sequence = 0;
          this.emit({ type: "new_session_ack" });
          this.emit({ type: "terminal_clear" });
        }
        this.viewed = this.execution;
      }
      this.state();
      return;
    }
    if (frame.type === "gui_busy_changed" || frame.type === "gui_busy_result") {
      this.busy =
        frame.busy && typeof frame.sessionId === "string"
          ? frame.sessionId
          : null;
      if (this.busy) this.execution = this.busy;
      this.emit(frame);
      this.state();
      return;
    }
    if (frame.type === "session_view") {
      if (frame.request_id !== this.request) return;
      this.request = null;
      this.viewed = String(frame.session_id);
      this.sequence = Number(frame.sequence);
      this.teamAgent =
        typeof frame.team_agent === "string" ? frame.team_agent : null;
      this.teamRunning = frame.running === true;
      this.teamLive = frame.team_live === true;
      this.teamOffset = Number(frame.offset ?? 0);
      this.polling = false;
      if (this.teamAgent) this.owners.set(this.viewed, this.teamAgent);
      this.emit({ type: "new_session_ack" });
      this.emit({ type: "terminal_clear" });
      this.emit({ type: "chat_plan_update", plan: null });
      this.emit({ type: "chat_goal_update", goal: null });
      for (const event of frame.events as Frame[]) this.emit(event);
      const ask = this.asks.get(this.viewed);
      if (ask) this.emit(ask);
      this.emit({ type: "sessions_list", current_id: this.viewed });
      this.emit({
        type: "session_view_selected",
        session_id: this.viewed,
        team_agent: this.teamAgent,
      });
      this.state();
      return;
    }
    if (frame.type === "session_event") {
      const id = String(frame.session_id);
      if ((frame.events as Frame[]).some((event) => event.type === "chat_done"))
        this.asks.delete(id);
      if (!this.viewed && !this.request) this.viewed = id;
      if (id !== this.viewed || Number(frame.sequence) <= this.sequence) return;
      this.sequence = Number(frame.sequence);
      for (const event of frame.events as Frame[]) {
        if (event.type === "chat_done") this.asks.delete(id);
        this.emit(event);
      }
      return;
    }
    if (frame.type === "session_view_error") {
      if (frame.request_id !== this.request) return;
      this.request = null;
      this.emit({ type: "session_action_rejected", text: frame.text });
      this.state();
      return;
    }
    if (
      frame.type === "session_view_invalidated" ||
      frame.type === "initial_state"
    ) {
      if (this.viewed)
        this.send({
          type: "session_load",
          id: this.viewed,
          ...(this.teamAgent ? { team_agent: this.teamAgent } : {}),
          refresh: true,
        });
      if (frame.type === "session_view_invalidated") return;
    }
    if (
      frame.type === "terminal_data" &&
      frame.session_id &&
      frame.session_id !== this.viewed
    )
      return;
    // Legacy reconnect snapshots describe the lead, never the viewed teammate.
    if (
      frame.type.startsWith("chat_") &&
      this.viewed &&
      this.execution &&
      this.viewed !== this.execution
    )
      return;
    if (
      frame.type === "approval_request" ||
      frame.type === "ask_user_question"
    ) {
      const id =
        typeof frame.session_id === "string"
          ? frame.session_id
          : (this.busy ?? this.execution);
      frame = { ...frame, session_id: id };
      if (frame.type === "ask_user_question" && id) {
        this.asks.set(id, frame);
        if (id !== this.viewed) return;
      }
    }
    if (frame.type === "sessions_list" || frame.type === "initial_state") {
      if (
        typeof frame.current_id === "string" &&
        frame.current_id &&
        !this.execution
      )
        this.execution = frame.current_id;
      frame = {
        ...frame,
        ...(this.viewed ? { current_id: this.viewed } : {}),
        ...(frame.type === "initial_state"
          ? { agent_busy: this.busy !== null && this.busy === this.viewed }
          : {}),
      };
    }
    this.emit(frame);
  }
}
