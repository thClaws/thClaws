// Media Studio — GUI Shell for image, video and speech generation.
//
// Drives the built-in media tools through thclaws.callTool():
//   text2image / image2image → TextToImage / ImageToImage   (sync → a path)
//   text2video / image2video → TextToVideo / ImageToVideo   (async → job_id,
//                              then MediaJobStatus until the clip lands)
//   text2speech              → TextToSpeech                 (sync → a path)
//
// The gallery is disk-backed: it lists everything under output/ via Glob, so
// media the agent produced shows up next to what was made here. Prompts and
// model choices are remembered per filename in shell storage.
//
// Every submit tool requires approval, so Generate raises the normal approval
// prompt — the engine routes gui_shell_tool_invoke through the same approver
// as the agent.

const MODELS = {
  text2image: [
    { value: "flash", label: "Gemini 3.1 Flash Image (fast)" },
    { value: "pro", label: "Gemini 3 Pro Image" },
    { value: "gpt-image-2", label: "OpenAI GPT Image 2" },
    { value: "qwen-image-3.0", label: "Qwen Image 3.0" },
    { value: "qwen-image-3.0-pro", label: "Qwen Image 3.0 Pro" },
    { value: "qwen-image-2.0", label: "Qwen Image 2.0" },
    { value: "qwen-image-2.0-pro", label: "Qwen Image 2.0 Pro" },
    { value: "iapp", label: "iApp Image — Thai text" },
  ],
  image2image: [
    { value: "flash", label: "Gemini 3.1 Flash Image (fast)" },
    { value: "pro", label: "Gemini 3 Pro Image" },
    { value: "gpt-image-2", label: "OpenAI GPT Image 2" },
    { value: "qwen-image-3.0", label: "Qwen Image 3.0" },
    { value: "qwen-image-3.0-pro", label: "Qwen Image 3.0 Pro" },
    { value: "qwen-image-2.0", label: "Qwen Image 2.0" },
    { value: "qwen-image-2.0-pro", label: "Qwen Image 2.0 Pro" },
    { value: "iapp", label: "iApp Image — Thai text" },
  ],
  text2video: [
    { value: "fast", label: "Veo 3.1 Fast" },
    { value: "quality", label: "Veo 3.1" },
    { value: "lite", label: "Veo 3.1 Lite" },
    { value: "ltx", label: "LTX-2.3 Fast" },
    { value: "ltx-pro", label: "LTX-2.3 Pro" },
    { value: "ltx-2-5-fast", label: "LTX-2.5 Fast (best speech)" },
    { value: "ltx-2-5-pro", label: "LTX-2.5 Pro" },
    { value: "happyhorse-1.0-t2v", label: "HappyHorse 1.0 (DashScope)" },
  ],
  image2video: [
    { value: "fast", label: "Veo 3.1 Fast" },
    { value: "quality", label: "Veo 3.1" },
    { value: "lite", label: "Veo 3.1 Lite" },
    { value: "ltx", label: "LTX-2.3 Fast" },
    { value: "ltx-pro", label: "LTX-2.3 Pro" },
    { value: "ltx-2-5-fast", label: "LTX-2.5 Fast (best speech)" },
    { value: "ltx-2-5-pro", label: "LTX-2.5 Pro" },
    { value: "happyhorse-1.0-i2v", label: "HappyHorse 1.0 (DashScope)" },
  ],
  text2speech: [{ value: "flash", label: "Gemini 3.1 Flash TTS" }],
};

const TOOL = {
  text2image: "TextToImage",
  image2image: "ImageToImage",
  text2video: "TextToVideo",
  image2video: "ImageToVideo",
  text2speech: "TextToSpeech",
};

const HINTS = {
  text2image: "",
  image2image: "",
  text2video: "Renders asynchronously (~30–120s) and keeps going if you leave this tab. Veo ≈ $3–6 per clip; LTX 2.3 ≈ $0.06/s, 2.5 ≈ $0.13/s. For speech: put the line in quotation marks and name the language and accent (e.g. she says in Thai, natural Bangkok accent: \"…\"), give it enough seconds to say it, and pick 48/50 fps.",
  image2video: "Renders asynchronously (~30–120s) and keeps going if you leave this tab. Veo ≈ $3–6 per clip; LTX 2.3 ≈ $0.06/s, 2.5 ≈ $0.13/s and holds a character\u2019s face from the reference frame. For speech: quote the line, name the language and accent, and pick 48/50 fps.",
  text2speech: "Speech is written to output/ as a .wav you can play here or hand to the agent.",
};

const META_KEY = "media-meta"; // filename → {prompt, model} for what we made
const JOBS_KEY = "media-jobs"; // in-flight video renders, so a reload resumes
const MAX_GALLERY = 300;
const POLL_INTERVAL_MS = 6000;
const POLL_MAX_MS = 15 * 60 * 1000;
// One failed MediaJobStatus call means a hiccup, not a dead render — the old
// version aborted the whole job on the first error and lost a paid clip.
const POLL_MAX_CONSECUTIVE_ERRORS = 5;

const ASSET_RE =
  /(?:^|[\s(])((?:\.?\/)?\S*output\/\S+\.(?:png|jpe?g|webp|gif|mp4|webm|mov|m4v|wav|mp3|ogg|m4a|flac))/i;
const JOBID_RE = /job_id=([A-Za-z0-9-]+)/;
const MEDIA_RE = /\.(png|jpe?g|webp|gif|mp4|webm|mov|m4v|wav|mp3|ogg|m4a|flac)$/i;
const VIDEO_RE = /\.(mp4|webm|mov|m4v)$/i;
const AUDIO_RE = /\.(wav|mp3|ogg|m4a|flac)$/i;
const TS_RE = /(\d{8}-\d{6})/; // img-YYYYMMDD-HHMMSS-… → sort key

const basename = (p) => String(p).split("/").pop();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let mode = "text2image";
let gallery = []; // [{path, type, prompt, model}] rebuilt from disk
let meta = {}; // filename → {prompt, model}
let jobs = []; // in-flight video renders (persisted)
let filter = "all";
let query = "";
let lightboxIndex = -1;

// ── elements ─────────────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const modelSel = $("model");
const inputPathField = $("input-path-field");
const inputPath = $("input-path");
const promptEl = $("prompt");
const promptLabel = $("prompt-label");
const imageParams = $("image-params");
const speechParams = $("speech-params");
const aspectSel = $("aspect");
const sizeField = $("size-field");
const durationField = $("duration-field");
const durationSel = $("duration");
const resolutionField = $("resolution-field");
const resolutionSel = $("resolution");
const fpsField = $("fps-field");
const iappTextField = $("iapp-text-field");
const iappTextEl = $("iapp-text");
const iappFontField = $("iapp-font-field");
const iappFontSel = $("iapp-font");
const fpsSel = $("fps");
const audioField = $("audio-field");
const audioEl = $("generate-audio");
const voiceSel = $("voice");
const styleInput = $("style");
const generateBtn = $("generate");
const enhanceBtn = $("enhance");
const enhancedField = $("enhanced-field");
const enhancedEl = $("enhanced");
const clearEnhancedBtn = $("clear-enhanced");
const statusEl = $("status");
const hintEl = $("hint");
const galleryEl = $("gallery");
const galleryEmpty = $("gallery-empty");
const jobsEl = $("jobs");
const jobsPill = $("jobs-pill");
const searchEl = $("search");

$("transport-badge").textContent =
  `${thclaws.transport} · ${thclaws.shell.sessionId ?? "(no session)"}`;

// ── helpers ──────────────────────────────────────────────────────────
// One resolver for every surface: the bridge maps workspace-relative and
// absolute paths to a URL this webview can actually fetch, in desktop and
// cloud alike. (It used to return null for relative paths, which is why
// this file once carried a hand-built `thclaws://` fallback — that URL is
// wrong on Windows and in the browser, so it's gone.)
const assetUrl = (path) => (path ? thclaws.fileUrl(path) : null);

const kindOf = (path) =>
  VIDEO_RE.test(path) ? "video" : AUDIO_RE.test(path) ? "audio" : "image";

function extractPath(result) {
  const m = String(result || "").match(ASSET_RE);
  return m ? m[1].replace(/^\.\//, "") : null;
}

function isVideo() {
  return mode === "text2video" || mode === "image2video";
}
function isSpeech() {
  return mode === "text2speech";
}
function needsInput() {
  return mode === "image2image" || mode === "image2video";
}
/// The picked model is an LTX one (the only backend with a 4K tier).
function isLtx() {
  return isVideo() && String(modelSel.value || "").startsWith("ltx");
}

/// LTX is the only backend with a 4K tier, an fps knob and an audio
/// switch — Veo ignores `resolution` entirely and DashScope tops out at
/// 1080P, so offering those elsewhere would bill a clip that comes back
/// wrong. Re-run on both mode and model changes.
function applyLtxOnlyFields() {
  const ltx = isLtx();
  fpsField.hidden = !ltx;
  audioField.hidden = !ltx;

  const fourK = [...resolutionSel.options].find((o) => o.value === "4K");
  if (!fourK) return;
  fourK.disabled = !ltx;
  if (fourK.disabled && resolutionSel.value === "4K") {
    resolutionSel.value = "1080P";
  }
}

/// The picked model is iApp — the only image backend that TYPESETS text
/// rather than letting the diffusion model draw it.
function isIapp() {
  return !isVideo() && !isSpeech() && String(modelSel.value || "").startsWith("iapp");
}

/// `text` + `font` exist only on iApp. Showing them for Gemini/OpenAI/Qwen
/// would promise Thai copy those models can only garble, so they follow the
/// model picker the same way the LTX-only knobs do.
function applyIappOnlyFields() {
  const iapp = isIapp();
  iappTextField.hidden = !iapp;
  iappFontField.hidden = !iapp;
}

function setStatus(text, kind) {
  statusEl.textContent = text;
  statusEl.className = "status" + (kind ? " " + kind : "");
}

function setBusy(busy) {
  generateBtn.disabled = busy;
  generateBtn.textContent = busy ? "Submitting…" : "Generate";
}

// ── mode ─────────────────────────────────────────────────────────────
function applyMode() {
  document
    .querySelectorAll(".mode-tab")
    .forEach((t) => t.classList.toggle("active", t.dataset.mode === mode));

  modelSel.innerHTML = "";
  for (const m of MODELS[mode]) {
    const o = document.createElement("option");
    o.value = m.value;
    o.textContent = m.label;
    modelSel.appendChild(o);
  }

  inputPathField.hidden = !needsInput();
  imageParams.hidden = isSpeech();
  speechParams.hidden = !isSpeech();
  sizeField.hidden = isVideo();
  durationField.hidden = !isVideo();
  resolutionField.hidden = !isVideo();

  promptLabel.textContent = isSpeech() ? "Text to speak" : "Prompt";
  promptEl.placeholder = isSpeech()
    ? "Type what should be said, in any language the voice supports…"
    : "Describe what to generate…";

  // Video providers only take 16:9 / 9:16.
  [...aspectSel.options].forEach((o) => {
    o.disabled = isVideo() && !["16:9", "9:16"].includes(o.value);
  });
  if (isVideo() && !["16:9", "9:16"].includes(aspectSel.value)) {
    aspectSel.value = "16:9";
  }

  applyLtxOnlyFields();
  applyIappOnlyFields();

  // Speech has no prompt to enhance — the text IS the deliverable.
  enhanceBtn.hidden = isSpeech();
  enhancedField.hidden = isSpeech() || !enhancedEl.value.trim();

  hintEl.textContent = HINTS[mode] || "";
  setStatus("");
}

// ── gallery ──────────────────────────────────────────────────────────
async function refreshGallery() {
  let listing = "";
  try {
    listing = await thclaws.callTool("Glob", { pattern: "**/*", path: "output" });
  } catch {
    listing = ""; // output/ doesn't exist yet — an empty gallery is correct
  }
  const paths = String(listing || "")
    .split("\n")
    .map((s) => s.trim())
    .filter((p) => p && MEDIA_RE.test(p));

  // Newest first by the timestamp the tools embed in the filename.
  paths.sort((a, b) => {
    const ka = (a.match(TS_RE) || [a])[0];
    const kb = (b.match(TS_RE) || [b])[0];
    return ka < kb ? 1 : ka > kb ? -1 : 0;
  });

  gallery = paths.slice(0, MAX_GALLERY).map((path) => {
    const m = meta[basename(path)] || {};
    return { path, type: kindOf(path), prompt: m.prompt || "", model: m.model || "" };
  });
  renderGallery();
}

function visibleItems() {
  const q = query.trim().toLowerCase();
  return gallery.filter((it) => {
    if (filter !== "all" && it.type !== filter) return false;
    if (!q) return true;
    return (
      it.path.toLowerCase().includes(q) || (it.prompt || "").toLowerCase().includes(q)
    );
  });
}

function renderGallery() {
  galleryEl.innerHTML = "";
  const items = visibleItems();
  galleryEmpty.hidden = items.length > 0;
  if (!items.length) {
    // Distinguish "nothing generated" from "nothing matches the filter" —
    // the same empty grid means two very different things.
    const filtered = gallery.length > 0;
    galleryEmpty.querySelector(".empty-title").textContent = filtered
      ? "No matches"
      : "Nothing here yet";
    galleryEmpty.querySelector(".empty-sub").textContent = filtered
      ? "Nothing under output/ matches that filter."
      : "Generated media lands in output/ and shows up here — including anything the agent made.";
    return;
  }
  items.forEach((item, i) => galleryEl.appendChild(makeCard(item, i)));
}

// Built with DOM calls, not innerHTML: a filename or prompt is user/model
// data and has no business being parsed as markup.
function makeCard(item, index) {
  const card = document.createElement("div");
  card.className = "card";

  const badge = document.createElement("span");
  badge.className = "badge";
  badge.textContent =
    item.type === "video" ? "▶ video" : item.type === "audio" ? "♪ audio" : "image";
  card.appendChild(badge);

  const url = assetUrl(item.path);
  if (item.type === "video") {
    const v = document.createElement("video");
    v.src = url;
    v.muted = true;
    v.preload = "metadata";
    // `preload="metadata"` paints nothing until a frame is decoded, which
    // left video cards as blank rectangles. Nudging past zero gives the
    // card a real still without downloading the whole clip.
    v.addEventListener(
      "loadedmetadata",
      () => {
        try {
          v.currentTime = Math.min(0.1, (v.duration || 1) / 2);
        } catch {
          /* seeking unsupported — the blank card is still clickable */
        }
      },
      { once: true },
    );
    card.appendChild(v);
  } else if (item.type === "audio") {
    const face = document.createElement("div");
    face.className = "audio-face";
    face.textContent = "♪";
    card.appendChild(face);
  } else {
    const img = document.createElement("img");
    img.src = url;
    img.alt = "";
    img.loading = "lazy";
    card.appendChild(img);
  }

  const cap = document.createElement("div");
  cap.className = "cap";
  cap.textContent = item.prompt || basename(item.path);
  cap.title = item.path;
  card.appendChild(cap);

  card.addEventListener("click", () => {
    // In a mode that needs a source frame, clicking an image picks it;
    // everything else opens the lightbox.
    if (needsInput() && item.type === "image") {
      inputPath.value = item.path;
      setStatus(`Source set: ${basename(item.path)}`, "ok");
    } else {
      openLightbox(index);
    }
  });
  return card;
}

function recordMeta(path, info) {
  meta[basename(path)] = info;
  thclaws.storage.set(META_KEY, meta).catch(() => {});
}

// ── jobs (async video renders) ───────────────────────────────────────
// Persisted so closing the tab or reloading the shell doesn't orphan a
// paid render: on load we pick every job back up and keep polling.
function saveJobs() {
  thclaws.storage.set(JOBS_KEY, jobs).catch(() => {});
}

function renderJobs() {
  jobsEl.innerHTML = "";
  for (const job of jobs) {
    const row = document.createElement("div");
    row.className = "job";

    const dot = document.createElement("span");
    dot.className = "dot";
    row.appendChild(dot);

    const text = document.createElement("span");
    text.className = "job-text";
    text.textContent = job.status || `Rendering ${job.id}…`;
    text.title = job.prompt || "";
    row.appendChild(text);

    const elapsed = document.createElement("span");
    elapsed.className = "job-elapsed";
    elapsed.textContent = fmtElapsed(Date.now() - job.startedAt);
    row.appendChild(elapsed);

    const stop = document.createElement("button");
    stop.className = "ghost-btn";
    stop.textContent = "Stop watching";
    stop.title = "Stop polling here — the render itself keeps going";
    stop.addEventListener("click", () => dropJob(job.id, "Stopped watching."));
    row.appendChild(stop);

    jobsEl.appendChild(row);
  }
  jobsPill.hidden = jobs.length === 0;
  jobsPill.textContent = jobs.length === 1 ? "1 rendering" : `${jobs.length} rendering`;
}

function fmtElapsed(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
}

function dropJob(id, note) {
  jobs = jobs.filter((j) => j.id !== id);
  saveJobs();
  renderJobs();
  if (note) setStatus(note);
}

/** Poll one job to completion. Safe to call again for the same id on reload. */
async function watchJob(job) {
  let errors = 0;
  while (jobs.some((j) => j.id === job.id)) {
    if (Date.now() - job.startedAt > POLL_MAX_MS) {
      dropJob(job.id);
      setStatus(`Gave up waiting for ${job.id} — check output/ later.`, "error");
      return;
    }
    await sleep(POLL_INTERVAL_MS);
    if (!jobs.some((j) => j.id === job.id)) return; // stopped while sleeping

    let status;
    try {
      status = String(await thclaws.callTool("MediaJobStatus", { job_id: job.id }));
      errors = 0;
    } catch (e) {
      errors += 1;
      if (errors >= POLL_MAX_CONSECUTIVE_ERRORS) {
        dropJob(job.id);
        setStatus(`Lost track of ${job.id}: ${msgOf(e)}`, "error");
        return;
      }
      continue; // transient — keep watching
    }

    if (status.startsWith("done")) {
      const path = extractPath(status);
      dropJob(job.id);
      if (path) {
        recordMeta(path, { prompt: job.prompt, model: job.model });
        await refreshGallery();
        setStatus("Video ready.", "ok");
      } else {
        setStatus(`Finished, but no path in the result: ${status}`, "error");
      }
      return;
    }
    if (status.startsWith("failed")) {
      dropJob(job.id);
      setStatus(status, "error");
      return;
    }

    job.status = status || `Rendering ${job.id}…`;
    saveJobs();
    renderJobs();
  }
}

function startJob(id, prompt, model) {
  const job = { id, prompt, model, startedAt: Date.now(), status: `Rendering ${id}…` };
  jobs.push(job);
  saveJobs();
  renderJobs();
  watchJob(job);
}

const msgOf = (e) => String(e && e.message ? e.message : e);

// ── lightbox ─────────────────────────────────────────────────────────
const lightbox = $("lightbox");
const lightboxBody = $("lightbox-body");
const lightboxCaption = $("lightbox-caption");
const lightboxMeta = $("lightbox-meta");
const togglePromptBtn = $("toggle-prompt");

// Hidden by default: the prompt used to sit over the media, where it
// covered a video's own controls. Kept as a preference for the rest of
// the session so stepping through items doesn't mean re-clicking.
let promptVisible = false;

function paintPromptToggle(hasPrompt) {
  togglePromptBtn.hidden = !hasPrompt;
  togglePromptBtn.textContent = promptVisible ? "Hide prompt" : "Show prompt";
  lightboxCaption.hidden = !(hasPrompt && promptVisible);
}

function openLightbox(index) {
  const items = visibleItems();
  if (!items.length) return;
  lightboxIndex = Math.max(0, Math.min(index, items.length - 1));
  const item = items[lightboxIndex];
  const url = assetUrl(item.path);

  lightboxBody.innerHTML = "";
  let node;
  if (item.type === "video") {
    node = document.createElement("video");
    node.controls = true;
    node.autoplay = true;
  } else if (item.type === "audio") {
    node = document.createElement("audio");
    node.controls = true;
    node.autoplay = true;
  } else {
    node = document.createElement("img");
    node.alt = "";
  }
  node.src = url;
  lightboxBody.appendChild(node);

  // The one-line identity sits in the bar; the prompt — which can run to
  // a paragraph after an enhance — gets the panel.
  lightboxMeta.textContent = [item.model, item.path].filter(Boolean).join(" · ");
  lightboxMeta.title = item.path;
  lightboxCaption.textContent = item.prompt || "";
  paintPromptToggle(Boolean(item.prompt));
  lightbox.hidden = false;
}

function stepLightbox(delta) {
  const items = visibleItems();
  if (!items.length) return;
  openLightbox((lightboxIndex + delta + items.length) % items.length);
}

function closeLightbox() {
  lightbox.hidden = true;
  lightboxBody.innerHTML = ""; // stops playback
  lightboxIndex = -1;
}

togglePromptBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  promptVisible = !promptVisible;
  paintPromptToggle(Boolean(lightboxCaption.textContent));
});
$("lightbox-close").addEventListener("click", closeLightbox);
$("lightbox-prev").addEventListener("click", (e) => {
  e.stopPropagation();
  stepLightbox(-1);
});
$("lightbox-next").addEventListener("click", (e) => {
  e.stopPropagation();
  stepLightbox(1);
});
$("copy-path").addEventListener("click", (e) => {
  e.stopPropagation();
  const item = visibleItems()[lightboxIndex];
  if (!item) return;
  // The desktop webview blocks navigator.clipboard inside a shell; the
  // bridge routes a copy through the host instead.
  if (typeof thclaws.copy === "function") thclaws.copy(item.path);
  else if (navigator.clipboard) navigator.clipboard.writeText(item.path).catch(() => {});
  setStatus(`Copied ${item.path}`, "ok");
});
lightbox.addEventListener("click", (e) => {
  if (e.target === lightbox) closeLightbox();
});

// ── generate ─────────────────────────────────────────────────────────
async function generate() {
  const prompt = isSpeech() ? promptEl.value.trim() : effectivePrompt();
  if (!prompt) {
    setStatus(isSpeech() ? "Enter some text to speak." : "Enter a prompt first.", "error");
    promptEl.focus();
    return;
  }
  const model = modelSel.value;
  const args = isSpeech() ? { text: prompt } : { prompt, model, aspect_ratio: aspectSel.value };

  if (isSpeech()) {
    if (model) args.model = model;
    if (voiceSel.value) args.voice = voiceSel.value;
    const style = styleInput.value.trim();
    if (style) args.style = style;
  } else if (isVideo()) {
    args.duration = parseInt(durationSel.value, 10);
    args.resolution = resolutionSel.value;
    if (isLtx()) {
      // Only LTX reads these; sending them to Veo/DashScope would be noise.
      args.fps = parseInt(fpsSel.value, 10);
      args.generate_audio = audioEl.checked;
    }
  } else {
    args.size = $("size").value;
    if (isIapp()) {
      // Only iApp reads these; sending them to Gemini/OpenAI/Qwen would be noise.
      const lines = iappTextEl.value
        .split("\n")
        .map((l) => l.trim())
        .filter(Boolean);
      if (lines.length > 4) {
        setStatus("iApp renders at most 4 lines of text.", "error");
        iappTextEl.focus();
        return;
      }
      if (lines.length) {
        args.text = lines;
        args.font = iappFontSel.value;
      }
    }
  }

  if (needsInput()) {
    const p = inputPath.value.trim();
    if (!p) {
      setStatus("Pick or type a source image path.", "error");
      inputPath.focus();
      return;
    }
    args.input_path = p;
  }

  setBusy(true);
  setStatus("Submitting — approve the spend if prompted…", "busy");
  try {
    const result = await thclaws.callTool(TOOL[mode], args);
    if (isVideo()) {
      const m = String(result || "").match(JOBID_RE);
      if (!m) throw new Error(result || "no job_id in the submit result");
      startJob(m[1], prompt, model);
      // The queue owns it from here — the panel is free for the next one.
      setStatus("Queued. It keeps rendering even if you switch tabs.", "ok");
    } else {
      const path = extractPath(result);
      if (!path) throw new Error(result || "no output path in the result");
      recordMeta(path, { prompt, model });
      await refreshGallery();
      setStatus(isSpeech() ? "Speech ready." : "Done.", "ok");
    }
  } catch (e) {
    setStatus(msgOf(e), "error");
  } finally {
    setBusy(false);
  }
}


// ── prompt enhance ───────────────────────────────────────────────────
// Rewrites the prompt for the image/video model on whatever LLM the
// session is currently using. Two rules make this safe to hand a user's
// prompt to a model:
//
//   1. Anything the user wants RENDERED as text inside the picture is
//      not the model's to translate or restyle. "ร้านกาแฟ" on a shop
//      sign must come out as those exact characters, or the render is
//      wrong in the one way the user will definitely notice.
//   2. Everything else — the description — is fair game to translate
//      into English (which every image model is strongest at) and
//      enrich with composition, lighting and lens detail.
//
// Rule 1 is also enforced here, not just asked for: any quoted run in
// the original must survive verbatim, or the rewrite is rejected.
const ENHANCE_SYSTEM = `You rewrite prompts for text-to-image and text-to-video models.

Return ONLY the rewritten prompt. No preamble, no quotes around the whole thing, no explanation, no markdown.

Rules:
1. VERBATIM TEXT — any text the user wants to appear inside the picture is sacred. That means anything in quotes ("…", '…', “…”, «…»), and anything after a label like text:, caption:, title:, sign:, label:, subtitle:. Reproduce every such run EXACTLY as given — same characters, same language, same script, same spelling, same punctuation, same capitalisation. Never translate it, never transliterate it, never correct it, never re-word it. Keep it inside the same quotes.
2. Everything else: translate the description into fluent English, then enrich it — subject, action, setting, composition, camera angle, lens, lighting, colour palette, mood, art style, level of detail.
3. Keep the user's actual intent. Do not add subjects, objects, people, brands or text that the user did not ask for.
4. One paragraph. No bullet points. Under 120 words.
5. If the prompt is already a strong English prompt, improve it lightly rather than rewriting it wholesale.`;

// Quoted runs and label:value runs — the parts rule 1 protects.
const QUOTED_RE = /"([^"\n]{1,120})"|'([^'\n]{1,120})'|[“„]([^”\n]{1,120})[”]|«([^»\n]{1,120})»/g;

function protectedRuns(text) {
  const out = [];
  for (const m of String(text).matchAll(QUOTED_RE)) {
    const v = (m[1] ?? m[2] ?? m[3] ?? m[4] ?? "").trim();
    if (v) out.push(v);
  }
  return out;
}

/** Which protected runs the rewrite dropped or altered. */
function lostRuns(original, rewritten) {
  const hay = String(rewritten);
  return protectedRuns(original).filter((run) => !hay.includes(run));
}

/** The prompt a Generate will actually send: the enhanced one if it has
 *  text, otherwise what the user wrote. */
function effectivePrompt() {
  const enhanced = enhancedEl.value.trim();
  return enhanced || promptEl.value.trim();
}

async function enhancePrompt() {
  if (isSpeech()) return; // spoken text is the deliverable — never rewrite it
  const original = promptEl.value.trim();
  if (!original) {
    setStatus("Write a prompt first, then enhance it.", "error");
    promptEl.focus();
    return;
  }
  if (!thclaws.llm || typeof thclaws.llm.complete !== "function") {
    setStatus("This build has no llm.complete bridge.", "error");
    return;
  }

  enhanceBtn.disabled = true;
  const label = enhanceBtn.textContent;
  enhanceBtn.textContent = "✦ Enhancing…";
  setStatus("Rewriting the prompt on your active model…", "busy");
  try {
    const res = await thclaws.llm.complete({
      system: ENHANCE_SYSTEM,
      prompt: `Rewrite this ${isVideo() ? "video" : "image"} prompt:\n\n${original}`,
      maxTokens: 700,
    });
    let text = String((res && res.text) || "").trim();
    // Models like to wrap the whole answer in quotes despite being told
    // not to; stripping that is not the same as touching rule-1 text,
    // which lives INSIDE the prompt.
    if (text.length > 1 && /^["'“«]/.test(text) && /["'”»]$/.test(text)) {
      const inner = text.slice(1, -1).trim();
      if (!QUOTED_RE.test(inner)) text = inner;
      QUOTED_RE.lastIndex = 0;
    }
    if (!text) throw new Error("the model returned nothing");

    const lost = lostRuns(original, text);
    if (lost.length) {
      // Refuse rather than silently ship a poster with the wrong words on
      // it. Nothing is written to the enhanced box, so Generate still
      // sends exactly what the user wrote.
      setStatus(
        `Discarded the rewrite — it changed text meant to appear in the image: ${lost
          .map((r) => `"${r}"`)
          .join(", ")}`,
        "error",
      );
      return;
    }

    // The user's own prompt is never overwritten: the rewrite lands in
    // its own box, which they can read, edit, or discard.
    enhancedEl.value = text;
    enhancedField.hidden = false;
    setStatus(`Enhanced with ${(res && res.model) || "the active model"} — Generate will use it.`, "ok");
  } catch (e) {
    setStatus(`Enhance failed: ${msgOf(e)}`, "error");
  } finally {
    enhanceBtn.disabled = false;
    enhanceBtn.textContent = label;
  }
}

function discardEnhanced() {
  enhancedEl.value = "";
  enhancedField.hidden = true;
  setStatus("Back to your own prompt.");
}

// ── wiring ───────────────────────────────────────────────────────────
$("modes").addEventListener("click", (e) => {
  const tab = e.target.closest(".mode-tab");
  if (!tab) return;
  mode = tab.dataset.mode;
  applyMode();
});

$("filters").addEventListener("click", (e) => {
  const chip = e.target.closest(".chip");
  if (!chip) return;
  filter = chip.dataset.filter;
  document
    .querySelectorAll(".chip")
    .forEach((c) => c.classList.toggle("active", c === chip));
  renderGallery();
});

searchEl.addEventListener("input", () => {
  query = searchEl.value;
  renderGallery();
});

modelSel.addEventListener("change", () => {
  applyLtxOnlyFields();
  applyIappOnlyFields();
});
generateBtn.addEventListener("click", generate);
enhanceBtn.addEventListener("click", enhancePrompt);
clearEnhancedBtn.addEventListener("click", discardEnhanced);
promptEl.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === "Enter") generate();
  if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key.toLowerCase() === "e") {
    e.preventDefault();
    enhancePrompt();
  }
});
enhancedEl.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === "Enter") generate();
});
$("refresh").addEventListener("click", () => refreshGallery());

document.addEventListener("keydown", (e) => {
  if (lightbox.hidden) return;
  if (e.key === "Escape") closeLightbox();
  if (e.key === "ArrowLeft") stepLightbox(-1);
  if (e.key === "ArrowRight") stepLightbox(1);
});

// Full-screen exit control (mirrors the chatbot shell).
(() => {
  const exitBtn = $("exit-fullscreen");
  if (!exitBtn || !thclaws.ui) return;
  exitBtn.addEventListener("click", () => thclaws.ui.exitFullscreen());
  thclaws.ui.onFullscreen((active) => {
    exitBtn.hidden = !active;
    if (active) thclaws.ui.claimExitControl();
  });
})();

// Keep elapsed times honest without re-polling.
setInterval(() => {
  if (jobs.length) renderJobs();
}, 1000);

// ── init ─────────────────────────────────────────────────────────────
applyMode();

// storage.get resolves to the reply body — `{value: …}` — not the bare
// value. Reading it directly is why saved prompts never came back after a
// reload (and would have quietly disabled job resume too).
async function loadStored(key) {
  try {
    const res = await thclaws.storage.get(key);
    if (res && typeof res === "object" && "value" in res) return res.value;
    return res;
  } catch {
    return null; // first run — nothing stored yet
  }
}

(async () => {
  {
    const saved = await loadStored(META_KEY);
    if (saved && typeof saved === "object") meta = saved;
  }
  {
    const saved = await loadStored(JOBS_KEY);
    if (Array.isArray(saved)) {
      // Drop anything already past the timeout rather than resuming a poll
      // that can only expire.
      jobs = saved.filter((j) => j && j.id && Date.now() - j.startedAt < POLL_MAX_MS);
      saveJobs();
      renderJobs();
      jobs.slice().forEach(watchJob);
      if (jobs.length) setStatus("Picked up a render that was still going.", "busy");
    }
  }
  await refreshGallery();
})();
