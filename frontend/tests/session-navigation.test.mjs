import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

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
  assert.deepEqual(received.at(-1), { type: "session_attention", sessions: ["A"] });
  assert.ok(!received.some(f => f.type === "ask_user_question"));
  nav.receive({ type: "approval_request", id: 2 });
  assert.equal(received.find(f => f.type === "approval_request").session_id, "A");
  view("A");
  assert.ok(received.some((f) => f.type === "ask_user_question" && f.id === 1));
  nav.send({ type: "ask_user_response", id: 1, text: "answer" });
  view("B");
  received.length = 0;
  view("A");
  assert.ok(!received.some((f) => f.type === "ask_user_question"));
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

test("legacy lead snapshots cannot overwrite a different viewed session", () => {
  const { nav, received, view } = harness();
  view("B");
  received.length = 0;
  nav.receive({ type: "chat_plan_update", plan: { id: "lead-only" } });
  nav.receive({ type: "chat_goal_update", goal: { id: "lead-only" } });
  assert.equal(received.length, 0);
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


test("background attention clears on response/completion and follows the viewed session", () => {
  const { nav, received, sent, view } = harness();
  view("B");
  nav.receive({ type: "approval_request", session_id: "A", id: 42 });
  assert.deepEqual(received.findLast(f => f.type === "session_attention").sessions, ["A"]);
  view("A");
  assert.deepEqual(received.findLast(f => f.type === "session_attention").sessions, []);
  nav.send({ type: "approval_response", id: 42, decision: "allow" });
  assert.equal(sent.at(-1).session_id, "A");
  view("B");
  assert.deepEqual(received.findLast(f => f.type === "session_attention").sessions, []);
  nav.receive({ type: "approval_request", session_id: "A", id: 43 });
  nav.receive({ type: "session_event", session_id: "A", sequence: 5, events: [{type:"chat_done"}] });
  assert.deepEqual(received.findLast(f => f.type === "session_attention").sessions, []);
  assert.ok(received.some(f => f.type === "session_requests_cleared" && f.session_id === "A"));
});
