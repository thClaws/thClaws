// Chatbot — minimal example GUI Shell.
//
// Demonstrates the bridge end-to-end without any tool dependency:
// thclaws.run() to send a turn, thclaws.on("text"/"done"/"error") to
// stream the reply, thclaws.storage to persist conversation history
// across reloads. Replace AGENTS.md to specialise (tutor, code
// helper, support bot, etc.) — frontend stays the same.

const STORAGE_KEY = "transcript";
const MAX_HISTORY = 200; // keep gallery cap reasonable; older trims out

const promptInput = document.getElementById("prompt-input");
const sendBtn = document.getElementById("send-btn");
const clearBtn = document.getElementById("clear-btn");
const transcriptEl = document.getElementById("transcript");
const emptyEl = document.getElementById("empty");
const statusEl = document.getElementById("status");
const transportEl = document.getElementById("transport-badge");

transportEl.textContent = `transport: ${thclaws.transport}  ·  session: ${thclaws.shell.sessionId ?? "(none)"}`;

// Conversation state: [{role: "user"|"bot"|"error", text: string, ts: number}]
let transcript = [];
let activeBotBubble = null;
let streamingText = "";

// ── render ───────────────────────────────────────────────────────────

function render() {
  emptyEl.hidden = transcript.length > 0;
  // Keep the empty placeholder in DOM but hide it; rebuild bubbles.
  [...transcriptEl.querySelectorAll(".bubble")].forEach((b) => b.remove());
  for (const msg of transcript) {
    const b = document.createElement("div");
    b.className = `bubble ${msg.role}`;
    b.textContent = msg.text;
    transcriptEl.appendChild(b);
  }
  scrollToBottom();
}

function scrollToBottom() {
  transcriptEl.scrollTop = transcriptEl.scrollHeight;
}

function setStatus(text, isError = false) {
  statusEl.textContent = text;
  statusEl.classList.toggle("error", !!isError);
}

function setSending(sending) {
  sendBtn.disabled = sending;
  sendBtn.textContent = sending ? "…" : "Send";
}

// ── bridge wiring ────────────────────────────────────────────────────

thclaws.on("text", (chunk) => {
  const text = typeof chunk === "string" ? chunk : chunk?.text ?? "";
  if (!text) return;
  streamingText += text;
  if (!activeBotBubble) {
    activeBotBubble = document.createElement("div");
    activeBotBubble.className = "bubble bot streaming";
    transcriptEl.appendChild(activeBotBubble);
    emptyEl.hidden = true;
  }
  activeBotBubble.textContent = streamingText;
  scrollToBottom();
});

thclaws.on("done", () => {
  if (activeBotBubble) {
    activeBotBubble.classList.remove("streaming");
    activeBotBubble = null;
  }
  if (streamingText.trim()) {
    transcript.push({ role: "bot", text: streamingText, ts: Date.now() });
    if (transcript.length > MAX_HISTORY) {
      transcript = transcript.slice(-MAX_HISTORY);
    }
    persist();
  }
  streamingText = "";
  setStatus("");
  setSending(false);
  promptInput.focus();
});

thclaws.on("error", (payload) => {
  const msg = payload?.error ?? "agent error";
  // Demote the streaming bubble into an error so the partial isn't lost.
  if (activeBotBubble) {
    activeBotBubble.classList.remove("streaming");
    activeBotBubble = null;
  }
  transcript.push({ role: "error", text: `[error] ${msg}`, ts: Date.now() });
  persist();
  render();
  streamingText = "";
  setStatus(msg, true);
  setSending(false);
});

// ── actions ──────────────────────────────────────────────────────────

sendBtn.addEventListener("click", send);

promptInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    send();
  }
});

clearBtn.addEventListener("click", () => {
  if (transcript.length === 0) return;
  if (!confirm(`Clear ${transcript.length} message${transcript.length === 1 ? "" : "s"}?`)) {
    return;
  }
  transcript = [];
  persist();
  render();
});

async function send() {
  const text = promptInput.value.trim();
  if (!text) return;
  transcript.push({ role: "user", text, ts: Date.now() });
  if (transcript.length > MAX_HISTORY) {
    transcript = transcript.slice(-MAX_HISTORY);
  }
  persist();
  render();
  promptInput.value = "";
  setSending(true);
  setStatus("…");
  try {
    await thclaws.run(text);
    // Stream continues via thclaws.on("text"); "done" handler closes out.
  } catch (err) {
    setStatus(`Send failed: ${err.message}`, true);
    setSending(false);
  }
}

// ── persistence ──────────────────────────────────────────────────────

async function persist() {
  try {
    await thclaws.storage.set(STORAGE_KEY, transcript);
  } catch (err) {
    setStatus(`Storage save failed: ${err.message}`, true);
  }
}

async function loadTranscript() {
  setStatus("Loading…");
  try {
    const stored = await thclaws.storage.get(STORAGE_KEY);
    const value = stored && typeof stored === "object" ? stored.value : null;
    if (Array.isArray(value)) {
      transcript = value;
    }
    setStatus("");
  } catch (err) {
    setStatus(`History load failed: ${err.message}`, true);
  }
  render();
  promptInput.focus();
}

loadTranscript();
