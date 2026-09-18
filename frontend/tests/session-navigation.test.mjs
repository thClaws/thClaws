import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

test("sidebar navigation never cancels the active turn and current session is a no-op", () => {
  const source = readFileSync(
    new URL("../src/components/Sidebar.tsx", import.meta.url),
    "utf8",
  );
  const sessions = source.slice(
    source.indexOf("{/* Sessions */}"),
    source.indexOf("{/* Knowledge bases */}"),
  );
  assert.doesNotMatch(sessions, /send\(\{ type: "shell_cancel"/);
  assert.match(sessions, /if \(isCurrent\) return/);
});

const { transpileModule, ModuleKind } = await import("typescript");
const source = readFileSync(
  new URL("../src/hooks/sessionNavigation.ts", import.meta.url),
  "utf8",
);
const compiled = transpileModule(source, {
  compilerOptions: { module: ModuleKind.ESNext },
}).outputText;
const { SessionNavigation } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`
);
function harness() {
  const received = [],
    sent = [];
  const nav = new SessionNavigation(
    (f) => received.push(f),
    (f) => sent.push(f),
  );
  const view = (id, events = [], sequence = 0) => {
    nav.send({ type: "session_load", id });
    nav.receive({
      type: "session_view",
      session_id: id,
      request_id: sent.at(-1).view_request,
      sequence,
      events,
    });
  };
  nav.receive({ type: "session_execution", session_id: "A" });
  return { nav, received, sent, view };
}

test("A → B → A preserves sequence boundaries and rejects late snapshots", () => {
  const { nav, received, sent, view } = harness();
  nav.receive({ type: "gui_busy_changed", busy: true, sessionId: "A" });
  assert.equal(nav.send({ type: "session_load", id: "A" }), false);
  assert.equal(sent.length, 0);
  view("B", [
    {
      type: "chat_history_replaced",
      messages: [{ role: "user", content: "B only" }],
    },
  ]);
  received.length = 0;
  nav.receive({
    type: "session_event",
    session_id: "A",
    sequence: 1,
    events: [{ type: "chat_text_delta", text: "A only" }],
  });
  assert.equal(received.length, 0);
  nav.send({ type: "session_load", id: "A" });
  const stale = sent.at(-1).view_request;
  nav.send({ type: "session_load", id: "B" });
  nav.receive({
    type: "session_view",
    session_id: "A",
    request_id: stale,
    sequence: 1,
    events: [],
  });
  assert.equal(nav.viewed, "B");
  nav.receive({
    type: "session_view",
    session_id: "B",
    request_id: sent.at(-1).view_request,
    sequence: 0,
    events: [],
  });
  nav.receive({ type: "gui_busy_changed", busy: false });
  view(
    "A",
    [{ type: "chat_text_delta", text: "complete A" }, { type: "chat_done" }],
    2,
  );
  received.length = 0;
  nav.receive({
    type: "session_event",
    session_id: "A",
    sequence: 2,
    events: [{ type: "chat_text_delta", text: "duplicate" }],
  });
  assert.equal(received.length, 0);
  nav.receive({
    type: "session_event",
    session_id: "A",
    sequence: 3,
    events: [{ type: "chat_text_delta", text: "next" }],
  });
  assert.equal(received[0].text, "next");
});

test("busy B blocks prompts, injection and Stop; Stop in A carries its target", () => {
  const { nav, sent, view } = harness();
  nav.receive({ type: "gui_busy_changed", busy: true, sessionId: "A" });
  view("B");
  const n = sent.length;
  for (const type of [
    "shell_input",
    "user_input_inject",
    "shell_cancel",
    "approval_response",
    "ask_user_response",
    "plan_approve",
  ]) {
    assert.equal(nav.send({ type, text: "do not inject in A" }), false);
  }
  assert.equal(sent.length, n);
  view("A");
  nav.send({ type: "shell_cancel" });
  assert.deepEqual(sent.at(-1), { type: "shell_cancel", session_id: "A" });
});

test("New session and reconnect use snapshots, never cancellation", () => {
  const { nav, sent } = harness();
  nav.receive({ type: "gui_busy_changed", busy: true, sessionId: "A" });
  nav.send({ type: "new_session" });
  nav.receive({
    type: "session_view",
    session_id: "NEW",
    request_id: sent.at(-1).view_request,
    sequence: 0,
    events: [],
  });
  assert.equal(nav.viewed, "NEW");
  assert.ok(nav.actionError().includes("A"));
  nav.receive({ type: "initial_state", agent_busy: true, current_id: "A" });
  assert.equal(sent.at(-1).id, "NEW");
  assert.equal(sent.at(-1).refresh, true);
  assert.ok(sent.every((f) => f.type !== "shell_cancel"));
});

test("ask/approval requests retain A ownership while B is visible", () => {
  const { nav, received, view } = harness();
  nav.receive({ type: "gui_busy_changed", busy: true, sessionId: "A" });
  view("B");
  received.length = 0;
  nav.receive({ type: "ask_user_question", id: 1, question: "A question" });
  assert.equal(received.length, 0);
  nav.receive({ type: "approval_request", id: 2 });
  assert.equal(received[0].session_id, "A");
  view("A");
  assert.ok(received.some((f) => f.type === "ask_user_question" && f.id === 1));
  nav.send({ type: "ask_user_response", id: 1, text: "answer" });
  view("B");
  received.length = 0;
  view("A");
  assert.ok(!received.some((f) => f.type === "ask_user_question"));
});

test("team agents run independently: messages and Stop target the selected actor, never the lead", () => {
  const { nav, sent, received } = harness();
  nav.receive({ type: "gui_busy_changed", busy: true, sessionId: "A" });
  nav.send({
    type: "session_load",
    id: "research-session",
    team_agent: "researcher",
  });
  nav.receive({
    type: "session_view",
    session_id: "research-session",
    team_agent: "researcher",
    team_live: true,
    running: true,
    offset: 100,
    sequence: 0,
    request_id: sent.at(-1).view_request,
    events: [{ type: "chat_text_delta", text: "research" }],
  });
  assert.equal(nav.actionError(), null);
  nav.send({ type: "shell_input", text: "Coordinate with the coder" });
  assert.deepEqual(sent.at(-1), {
    type: "team_send_message",
    to: "researcher",
    text: "Coordinate with the coder",
    session_id: "research-session",
  });
  nav.send({ type: "shell_cancel" });
  assert.deepEqual(sent.at(-1), {
    type: "team_abort_turn",
    to: "researcher",
    session_id: "research-session",
  });
  nav.poll();
  assert.equal(sent.at(-1).offset, 100);
  const count = sent.length;
  nav.poll();
  assert.equal(sent.length, count);
  received.length = 0;
  nav.receive({
    type: "team_session_events",
    session_id: "coder-session",
    team_agent: "coder",
    offset: 100,
    next_offset: 200,
    events: [{ type: "chat_text_delta", text: "wrong actor" }],
  });
  assert.equal(received.length, 0);
  nav.receive({
    type: "team_session_events",
    session_id: "research-session",
    team_agent: "researcher",
    offset: 100,
    next_offset: 200,
    running: false,
    events: [
      { type: "chat_text_delta", text: "finished" },
      { type: "chat_done" },
    ],
  });
  assert.equal(received[0].text, "finished");
  assert.equal(received.at(-1).running, false);
  received.length = 0;
  nav.receive({
    type: "team_session_events",
    session_id: "research-session",
    team_agent: "researcher",
    offset: 100,
    next_offset: 200,
    running: false,
    events: [{ type: "chat_text_delta", text: "duplicate" }],
  });
  assert.equal(received.length, 0);
  nav.receive({ type: "initial_state" });
  assert.equal(sent.at(-1).team_agent, "researcher");
});

test("background completion clears questions before returning to the owner", () => {
  const { nav, received, view } = harness();
  nav.receive({
    type: "ask_user_question",
    session_id: "A",
    id: 1,
    question: "old",
  });
  view("B");
  nav.receive({
    type: "session_event",
    session_id: "A",
    sequence: 1,
    events: [{ type: "chat_done" }],
  });
  received.length = 0;
  view("A");
  assert.ok(!received.some((f) => f.type === "ask_user_question"));
});

test("stopped and replaced teammates cannot receive stale commands", () => {
  const { nav, sent } = harness();
  nav.send({ type: "session_load", id: "S", team_agent: "coder" });
  nav.receive({
    type: "session_view",
    session_id: "S",
    team_agent: "coder",
    team_live: true,
    request_id: sent.at(-1).view_request,
    offset: 0,
    sequence: 0,
    events: [],
  });
  nav.receive({
    type: "team_session_events",
    session_id: "S",
    team_agent: "coder",
    offset: 0,
    next_offset: 0,
    live: false,
    running: false,
    events: [],
  });
  const count = sent.length;
  assert.equal(nav.send({ type: "shell_cancel" }), false);
  assert.equal(nav.send({ type: "shell_input", text: "stale" }), false);
  nav.poll();
  assert.equal(sent.length, count);
});

test("legacy lead snapshots cannot overwrite a different viewed session", () => {
  const { nav, received, view } = harness();
  view("B");
  received.length = 0;
  nav.receive({ type: "chat_plan_update", plan: { id: "lead-only" } });
  nav.receive({ type: "chat_goal_update", goal: { id: "lead-only" } });
  assert.equal(received.length, 0);
});

test("reopening the selected teammate switches to Chat without reloading its transcript", () => {
  const { nav, sent, received } = harness();
  nav.send({ type: "session_load", id: "S", team_agent: "coder" });
  nav.receive({
    type: "session_view",
    session_id: "S",
    team_agent: "coder",
    team_live: true,
    request_id: sent.at(-1).view_request,
    offset: 0,
    sequence: 0,
    events: [],
  });
  const count = sent.length;
  received.length = 0;
  assert.equal(
    nav.send({ type: "session_load", id: "S", team_agent: "coder" }),
    false,
  );
  assert.equal(sent.length, count);
  assert.deepEqual(received, [
    { type: "session_view_selected", session_id: "S", team_agent: "coder" },
  ]);
});

test("first connection never sends an empty session target from a sidebar refresh", () => {
  const sent = [],
    received = [];
  const nav = new SessionNavigation(
    (f) => received.push(f),
    (f) => sent.push(f),
  );
  nav.receive({ type: "initial_state", sessions: [], agent_busy: false });
  nav.receive({
    type: "sessions_list",
    current_id: "",
    sessions: [{ id: "A" }],
  });
  nav.receive({ type: "session_execution", session_id: "" });
  nav.send({ type: "shell_input", text: "first prompt" });
  assert.deepEqual(sent.at(-1), { type: "shell_input", text: "first prompt" });
  assert.equal(nav.execution, null);
  nav.receive({ type: "session_execution", session_id: "A" });
  nav.receive({ type: "gui_busy_changed", busy: false });
  nav.receive({ type: "initial_state", sessions: [{ id: "A" }] });
  assert.equal(sent.at(-1).type, "session_load");
  assert.equal(sent.at(-1).id, "A");
  nav.receive({
    type: "session_view",
    session_id: "A",
    request_id: sent.at(-1).view_request,
    sequence: 0,
    events: [],
  });
  nav.send({ type: "shell_input", text: "second prompt" });
  assert.equal(sent.at(-1).session_id, "A");
});
