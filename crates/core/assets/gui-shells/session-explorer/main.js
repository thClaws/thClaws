// Session Explorer — Tier 1 GUI Shell for thClaws.
//
// Demonstrates the bridge end-to-end with the Tier 1 surface only:
// thclaws.run(prompt), thclaws.cancel(), thclaws.on("text"|"done"|"error").
// No tools.invoke (Tier 2) — the agent in the shell's session does the
// FS walks itself via its existing ls/read tools.
//
// On load: ask the agent to list ./.thclaws/sessions/ → render rows.
// Click a row → ask the agent to dump the session's structure.
// "Ask" input → free-form question, response streams into the main pane.

const LIST_PROMPT = `Without commentary, list each .jsonl file in ./.thclaws/sessions/ as one line per file. Format strictly:
<filename> | <first_user_message_truncated_to_80_chars>
If a file has no user message yet, write "(no user message)" for the second field. Skip files that don't parse. Output at most 25 rows, most-recently-modified first. No headers, no JSON, no explanation — just the lines.`;

function detailPrompt(id) {
  return `Without commentary, output the first 30 lines of ./.thclaws/sessions/${id} verbatim, wrapped in a single \`\`\`jsonl\`\`\` block. No explanation before or after.`;
}

const statusEl     = document.getElementById("status");
const askInput     = document.getElementById("ask-input");
const askBtn       = document.getElementById("ask-btn");
const cancelBtn    = document.getElementById("cancel-btn");
const answerEl     = document.getElementById("answer");
const sessionList  = document.getElementById("session-list");
const sessionCount = document.getElementById("session-count");
const welcomeEl    = document.getElementById("welcome");
const detailEl     = document.getElementById("detail");
const transportEl  = document.getElementById("transport-badge");

let activeRunId = null;
let activeAccumulator = "";
let activeTarget = null;     // element receiving streamed text
let activeOnDone = null;     // callback fired on done event for this run

transportEl.textContent = `transport: ${thclaws.transport}  ·  session: ${thclaws.shell.sessionId ?? "(new)"}`;

// ---- bridge wiring ----------------------------------------------------

thclaws.on("text", (payload) => {
  const chunk = typeof payload === "string" ? payload : payload?.text ?? "";
  if (!chunk) return;
  activeAccumulator += chunk;
  if (activeTarget) activeTarget.textContent = activeAccumulator;
});

thclaws.on("done", () => {
  setRunning(false);
  const done = activeOnDone;
  const text = activeAccumulator;
  activeRunId = null;
  activeOnDone = null;
  activeAccumulator = "";
  activeTarget = null;
  if (done) done(text);
});

thclaws.on("error", (payload) => {
  setRunning(false);
  const msg = payload?.error ?? "agent error";
  if (activeTarget) activeTarget.textContent += `\n\n[error] ${msg}`;
  activeRunId = null;
  activeOnDone = null;
  activeAccumulator = "";
  activeTarget = null;
});

function setRunning(running) {
  askBtn.disabled = running;
  cancelBtn.disabled = !running;
  statusEl.textContent = running ? "running…" : "";
}

async function runPrompt({ prompt, targetEl, onDone }) {
  activeAccumulator = "";
  activeTarget = targetEl;
  activeOnDone = onDone || null;
  setRunning(true);
  if (targetEl) targetEl.textContent = "";
  try {
    const { runId } = await thclaws.run(prompt);
    activeRunId = runId;
  } catch (err) {
    setRunning(false);
    if (targetEl) targetEl.textContent = `[bridge error] ${err.message}`;
    throw err;
  }
}

// ---- UI handlers ------------------------------------------------------

askBtn.addEventListener("click", () => {
  const q = askInput.value.trim();
  if (!q) return;
  showWelcome();
  answerEl.innerHTML = "<div class='node assistant'><div class='node-header'><span class='node-kind'>answer</span></div><div class='node-body' id='answer-body'></div></div>";
  const body = document.getElementById("answer-body");
  runPrompt({ prompt: q, targetEl: body });
});

cancelBtn.addEventListener("click", () => {
  if (activeRunId != null) thclaws.cancel(activeRunId);
});

askInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    askBtn.click();
  }
});

// ---- session list (auto-loads on first paint) -------------------------

function parseSessionLines(text) {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && l.includes("|"))
    .map((l) => {
      const idx = l.indexOf("|");
      return {
        id: l.slice(0, idx).trim(),
        snippet: l.slice(idx + 1).trim(),
      };
    })
    .filter((r) => r.id.endsWith(".jsonl"));
}

function renderSessions(rows) {
  if (!rows.length) {
    sessionList.className = "empty";
    sessionList.textContent = "no sessions found in ./.thclaws/sessions/";
    return;
  }
  sessionList.className = "";
  sessionList.innerHTML = "";
  sessionCount.textContent = `(${rows.length})`;
  for (const row of rows) {
    const el = document.createElement("div");
    el.className = "session-row";
    el.dataset.sessionId = row.id;
    el.innerHTML = `<div class="id">${escapeHtml(row.id)}</div><div class="title">${escapeHtml(row.snippet || "(empty)")}</div>`;
    el.addEventListener("click", () => openSession(row.id, el));
    sessionList.appendChild(el);
  }
}

function showWelcome() {
  welcomeEl.hidden = false;
  detailEl.hidden = true;
}

function openSession(id, rowEl) {
  document.querySelectorAll(".session-row.active").forEach((e) => e.classList.remove("active"));
  rowEl.classList.add("active");
  welcomeEl.hidden = true;
  detailEl.hidden = false;
  detailEl.innerHTML = `
    <h1>${escapeHtml(id)}</h1>
    <div class="session-meta">Loading first 30 lines via agent (Tier 1 has no direct file read — Tier 2 will use <code>tools.invoke("read", …)</code>).</div>
    <pre class="node tool"><code id="detail-body">…</code></pre>
  `;
  const body = document.getElementById("detail-body");
  runPrompt({ prompt: detailPrompt(id), targetEl: body });
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[c]);
}

// Kick off the initial session list. We do this as soon as the bridge
// signals it's wired — but the bridge's "ready" message fires on load
// too, so just run it after a tick.
setTimeout(() => {
  runPrompt({
    prompt: LIST_PROMPT,
    targetEl: null,
    onDone: (text) => {
      const rows = parseSessionLines(text);
      renderSessions(rows);
    },
  }).catch(() => {
    sessionList.textContent = "(bridge unavailable)";
  });
}, 50);
