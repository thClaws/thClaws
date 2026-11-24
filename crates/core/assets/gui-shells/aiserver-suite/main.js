// AI Server: the nine capabilities of the AIServer Suite API v1 on this box.
//
// Every call goes through thclaws.aiserver.request(), which the engine makes:
// on the box straight to :8100, in an EE Pack workspace through the Pack
// gateway (which holds the box key and adds the user). Binary results are
// written under output/aiserver/<cap>/ in the workspace and shown from there.
//
// Long work (image, video, 3D, music, RAG ingest) is a Suite job: submit, poll
// GET /v1/jobs/{id}, then copy every artifact into the workspace at once,
// since the box may sweep old outputs. Jobs in flight survive a reload.

const OUT = "output/aiserver";
const JOBS_KEY = "aiserver-jobs";
const GALLERY_KEY = "aiserver-gallery";
const MAX_GALLERY = 60;
const POLL_MS = 4000;

const EXT = {
  "image/png": "png", "image/jpeg": "jpg", "image/webp": "webp", "video/mp4": "mp4",
  "audio/mpeg": "mp3", "audio/mp3": "mp3", "audio/wav": "wav", "audio/x-wav": "wav",
  "audio/flac": "flac", "audio/ogg": "opus", "audio/opus": "opus", "model/gltf-binary": "glb",
};

const CAPS = [
  {
    id: "llm", title: "LLM", label: "แชตกับโมเดล",
    hint: "ถามโมเดลที่โหลดอยู่บนเครื่องนี้ ใช้กับงานสั้นๆ งานจริงให้ใช้แชตหลักของ thClaws",
    fields: [
      { key: "prompt", label: "คำถาม", type: "textarea", rows: 5, required: true },
      { key: "maxTokens", label: "ความยาวคำตอบสูงสุด (token)", type: "number", value: 1024, min: 16, max: 4096 },
    ],
    run: runLlm,
  },
  {
    id: "ocr", title: "OCR", label: "อ่านข้อความจากภาพ/PDF",
    hint: "ภาพหรือ PDF ไม่เกิน 200 หน้า ได้ผลเป็น Markdown (ตารางเป็น HTML) หรือข้อความล้วน",
    fields: [
      { key: "file", label: "ไฟล์", type: "file", accept: "image/*,application/pdf", required: true },
      { key: "model", label: "โมเดล", type: "select", models: true, options: [["ocr-default", "OCR Markdown (GPU)"], ["ocr-fast", "OCR Text (CPU, เร็ว)"]] },
      { key: "pages", label: "หน้า (เช่น 1-3,5 เว้นว่าง = ทั้งหมด)", type: "text" },
      { key: "output", label: "รูปแบบ", type: "select", options: [["markdown", "Markdown"], ["text", "ข้อความล้วน"]] },
    ],
    run: runOcr,
  },
  {
    id: "tts", title: "TTS", label: "แปลงข้อความเป็นเสียง",
    hint: "ต้องมีเสียงก่อน: เสียงที่เทรนไว้ หรือคลิปเสียงตัวอย่าง (≥ 3 วินาที) ที่อัปโหลดด้านล่าง",
    fields: [
      { key: "input", label: "ข้อความ", type: "textarea", rows: 5, required: true, max: 4000 },
      { key: "voice", label: "เสียง", type: "select", options: [] },
      { key: "response_format", label: "ไฟล์", type: "select", options: [["mp3", "MP3"], ["wav", "WAV"], ["flac", "FLAC"]] },
      { key: "speed", label: "ความเร็ว", type: "number", value: 1, min: 0.5, max: 2, step: 0.1 },
    ],
    run: runTts,
    extra: ttsExtra,
  },
  {
    id: "asr", title: "ASR", label: "ถอดเสียงเป็นข้อความ",
    hint: "ไฟล์เสียงหรือวิดีโอ ภาษาไทยใช้โมเดลหลัก ภาษาอื่นใช้ Whisper",
    fields: [
      { key: "file", label: "ไฟล์เสียง/วิดีโอ", type: "file", accept: "audio/*,video/*", required: true },
      { key: "model", label: "โมเดล", type: "select", models: true, options: [["asr-default", "ภาษาไทย"], ["asr-whisper", "Whisper (ทุกภาษา)"], ["asr-vibevoice", "แยกผู้พูด"]] },
      { key: "response_format", label: "รูปแบบ", type: "select", options: [["json", "ข้อความ"], ["srt", "ซับไตเติล SRT"], ["vtt", "ซับไตเติล VTT"], ["verbose_json", "ข้อความ + เวลา"]] },
    ],
    run: runAsr,
  },
  {
    id: "image", title: "Image", label: "สร้างภาพ",
    hint: "พิมพ์ไทยหรืออังกฤษก็ได้",
    fields: [
      { key: "prompt", label: "คำบรรยายภาพ", type: "textarea", rows: 4, required: true },
      { key: "model", label: "โมเดล", type: "select", models: true, options: [["image-default", "ค่าเริ่มต้นของเครื่อง"]] },
      { key: "size", label: "ขนาด", type: "select", options: [["1024x1024", "1024×1024"], ["1344x768", "1344×768 (แนวนอน)"], ["768x1344", "768×1344 (แนวตั้ง)"], ["1536x1536", "1536×1536"]] },
      { key: "n", label: "จำนวนภาพ", type: "number", value: 1, min: 1, max: 4 },
      { key: "negative_prompt", label: "สิ่งที่ไม่ต้องการ", type: "text" },
    ],
    run: runImage,
  },
  {
    id: "video", title: "Video", label: "สร้างวิดีโอ",
    hint: "ใส่ภาพเริ่มต้นได้ (ภาพ → วิดีโอ) ใช้เวลาหลายนาที ปิดหน้านี้ได้ งานยังเดินต่อ",
    fields: [
      { key: "prompt", label: "คำบรรยาย", type: "textarea", rows: 4, required: true },
      { key: "image", label: "ภาพเริ่มต้น (ไม่บังคับ)", type: "file", accept: "image/*" },
      { key: "seconds", label: "ความยาว (วินาที)", type: "number", value: 5, min: 1, max: 60 },
      { key: "resolution", label: "ความละเอียด", type: "select", options: [["720p", "720p"], ["1080p", "1080p"], ["draft", "ร่าง (เร็ว)"]] },
      { key: "aspect", label: "สัดส่วน", type: "select", options: [["16:9", "16:9"], ["9:16", "9:16"]] },
      { key: "audio", label: "สร้างเสียงประกอบ", type: "checkbox", value: true },
    ],
    run: runVideo,
  },
  {
    id: "3d", title: "3D", label: "สร้างโมเดล 3 มิติจากภาพ",
    hint: "ภาพวัตถุชิ้นเดียว พื้นหลังเรียบ ได้ไฟล์ GLB และภาพตัวอย่าง",
    fields: [
      { key: "image", label: "ภาพ", type: "file", accept: "image/*", required: true },
      { key: "model", label: "โมเดล", type: "select", models: true, options: [["3d-default", "ค่าเริ่มต้นของเครื่อง"]] },
      { key: "texture", label: "texture (px)", type: "number", value: 2048, min: 1024, max: 4096 },
    ],
    run: run3d,
  },
  {
    id: "music", title: "Music", label: "สร้างเพลง",
    hint: "เว้นเนื้อเพลงว่าง = เพลงบรรเลง ใช้ [verse] [chorus] แบ่งท่อน",
    fields: [
      { key: "prompt", label: "แนวเพลง อารมณ์ เครื่องดนตรี", type: "textarea", rows: 3, required: true },
      { key: "lyrics", label: "เนื้อเพลง", type: "textarea", rows: 5 },
      { key: "seconds", label: "ความยาว (วินาที)", type: "number", value: 30, min: 10, max: 240 },
      { key: "language", label: "ภาษาเนื้อร้อง", type: "select", options: [["th", "ไทย"], ["en", "อังกฤษ"]] },
      { key: "bpm", label: "BPM", type: "number", value: 120, min: 40, max: 240 },
      { key: "model", label: "โมเดล", type: "select", models: true, options: [["music-default", "คุณภาพ"], ["music-fast", "เร็ว"]] },
    ],
    run: runMusic,
  },
  {
    id: "rag", title: "RAG", label: "ถามจากเอกสาร",
    hint: "",
    fields: [
      { key: "query", label: "คำถาม", type: "textarea", rows: 3, required: true },
      { key: "collection", label: "คลังเอกสาร", type: "select", options: [] },
      { key: "top_k", label: "จำนวนข้อความอ้างอิง", type: "number", value: 5, min: 1, max: 20 },
    ],
    run: runRag,
    extra: ragExtra,
  },
];

const $ = (id) => document.getElementById(id);
const state = { cap: CAPS[0], caps: {}, gallery: [], jobs: {}, busy: false };

// ── bridge helpers ──────────────────────────────────────────────────

async function api(method, path, opts = {}) {
  const r = await thclaws.aiserver.request({ method, path, ...opts });
  if (!r.ok) throw new Error(errorText(r));
  return r;
}

function errorText(r) {
  const e = r.json && r.json.error;
  const code = e && (e.code || e.type);
  if (code && KNOWN_ERRORS[code]) return KNOWN_ERRORS[code];
  const msg = (e && e.message) || r.text || (r.json && r.json.detail) || "";
  return `ผิดพลาด (${r.status})${msg ? ": " + String(msg).slice(0, 300) : ""}`;
}

const KNOWN_ERRORS = {
  model_not_loaded: "ยังไม่ได้โหลดโมเดลบนเครื่อง ให้ผู้ดูแลโหลดที่หน้า AI Server ก่อน",
  service_stopped: "บริการนี้ปิดอยู่ ให้ผู้ดูแลเปิดที่หน้า AI Server",
  model_not_installed: "ยังไม่ได้ติดตั้งโมเดลนี้บนเครื่อง",
  voice_not_found: "ไม่พบเสียงนี้ ให้เพิ่มเสียงก่อน",
  gpu_busy: "GPU ไม่ว่าง ลองใหม่อีกครั้ง",
  capacity: "เครื่องรับงานเต็ม ลองใหม่อีกครั้ง",
  file_too_large: "ไฟล์ใหญ่เกินไป",
  invalid_api_key: "คีย์ของเครื่องไม่ถูกต้อง แจ้งผู้ดูแล",
  insufficient_memory: "หน่วยความจำ GPU เต็มขณะทำงานนี้ ลองส่งใหม่อีกครั้ง หรือแจ้งผู้ดูแลให้คืนหน่วยความจำ",
};

const stamp = () => {
  const d = new Date();
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
};

function readFileB64(file) {
  return new Promise((resolve, reject) => {
    const fr = new FileReader();
    fr.onload = () => resolve(String(fr.result));
    fr.onerror = () => reject(fr.error || new Error("อ่านไฟล์ไม่ได้"));
    fr.readAsDataURL(file);
  });
}

async function filePart(name, file) {
  return { name, data: await readFileB64(file), filename: file.name || "upload" };
}

// ── form ────────────────────────────────────────────────────────────

function modelOptions(field) {
  const cap = state.caps[state.cap.id];
  const ids = cap && Array.isArray(cap.models) ? cap.models.map((m) => m.id) : [];
  const opts = field.options.slice();
  for (const id of ids) if (!opts.some(([v]) => v === id)) opts.push([id, id]);
  return opts;
}

function renderForm() {
  const form = $("form");
  form.innerHTML = "";
  for (const f of state.cap.fields) {
    const wrap = document.createElement("label");
    wrap.className = "field" + (f.type === "checkbox" ? " check" : "");
    const lab = document.createElement("span");
    lab.className = "label";
    lab.textContent = f.label;
    let input;
    if (f.type === "textarea") {
      input = document.createElement("textarea");
      input.rows = f.rows || 4;
      if (f.max) input.maxLength = f.max;
    } else if (f.type === "select") {
      input = document.createElement("select");
      for (const [v, t] of f.models ? modelOptions(f) : f.options) {
        const o = document.createElement("option");
        o.value = v;
        o.textContent = t;
        input.appendChild(o);
      }
    } else {
      input = document.createElement("input");
      input.type = f.type;
      if (f.accept) input.accept = f.accept;
      if (f.type === "checkbox") input.checked = !!f.value;
      else if (f.value != null) input.value = f.value;
      if (f.min != null) input.min = f.min;
      if (f.max != null) input.max = f.max;
      if (f.step != null) input.step = f.step;
    }
    input.id = "f-" + f.key;
    input.dataset.key = f.key;
    if (f.type === "checkbox") wrap.append(input, lab);
    else wrap.append(lab, input);
    if (f.type === "file") {
      const pv = document.createElement("div");
      pv.className = "preview";
      input.addEventListener("change", () => showPreview(pv, input.files && input.files[0]));
      wrap.append(pv);
    }
    form.appendChild(wrap);
  }
}

let previewUrl = null;
function showPreview(box, file) {
  box.innerHTML = "";
  if (previewUrl) URL.revokeObjectURL(previewUrl);
  previewUrl = null;
  if (!file) return;
  const kind = (file.type || "").split("/")[0];
  if (!["image", "audio", "video"].includes(kind)) return;
  previewUrl = URL.createObjectURL(file);
  const el = document.createElement(kind === "image" ? "img" : kind);
  el.src = previewUrl;
  if (kind !== "image") el.controls = true;
  box.append(el);
}

function values() {
  const out = {};
  for (const f of state.cap.fields) {
    const el = $("f-" + f.key);
    if (!el) continue;
    if (f.type === "file") out[f.key] = el.files && el.files[0] ? el.files[0] : null;
    else if (f.type === "checkbox") out[f.key] = el.checked;
    else if (f.type === "number") out[f.key] = el.value === "" ? undefined : Number(el.value);
    else out[f.key] = el.value.trim();
    if (f.required && !out[f.key]) throw new Error(`กรุณากรอก "${f.label}"`);
  }
  return out;
}

// ── capabilities ────────────────────────────────────────────────────

async function runLlm(v) {
  const r = await thclaws.llm.complete({ prompt: v.prompt, maxTokens: v.maxTokens });
  showText(r.text, `โมเดล: ${r.model}`);
}

async function runOcr(v) {
  const form = [await filePart("file", v.file), { name: "model", value: v.model }, { name: "output", value: v.output }];
  if (v.pages) form.push({ name: "pages", value: v.pages });
  const r = await api("POST", "/v1/ocr", { form, timeoutSecs: 900 });
  const j = r.json || {};
  showText(j.text || "", `${(j.pages || []).length} หน้า · ${Math.round((j.took_ms || 0) / 100) / 10} วินาที`);
}

async function runTts(v) {
  if (!v.voice) throw new Error("ยังไม่มีเสียง ให้เพิ่มเสียงด้านล่างก่อน");
  const path = `${OUT}/tts/${stamp()}.${v.response_format}`;
  const r = await api("POST", "/v1/audio/speech", {
    json: { input: v.input, voice: v.voice, response_format: v.response_format, speed: v.speed },
    saveTo: path,
  });
  const secs = r.headers && r.headers["x-audio-seconds"];
  addGallery({ cap: "tts", files: [path], caption: v.input.slice(0, 80) + (secs ? ` · ${secs} วิ` : "") });
  showFiles([path]);
}

async function runAsr(v) {
  const form = [await filePart("file", v.file), { name: "model", value: v.model }, { name: "response_format", value: v.response_format }];
  const r = await api("POST", "/v1/audio/transcriptions", { form, timeoutSecs: 1800 });
  if (r.json && typeof r.json.text === "string") {
    const segs = Array.isArray(r.json.segments) ? r.json.segments : null;
    const text = segs
      ? segs.map((s) => `[${fmtTime(s.start)}–${fmtTime(s.end)}] ${s.text}`).join("\n")
      : r.json.text;
    const secs = r.json.usage && r.json.usage.seconds;
    showText(text, secs ? `เสียง ${Math.round(secs)} วินาที` : "");
  } else {
    showText(r.text || "", v.response_format.toUpperCase());
  }
}

async function runImage(v) {
  const body = { prompt: v.prompt, model: v.model, size: v.size, n: v.n || 1, async: true };
  if (v.negative_prompt) body.negative_prompt = v.negative_prompt;
  const r = await api("POST", "/v1/images/generations", { json: body });
  await startJob("image", r.json, v.prompt);
}

async function runVideo(v) {
  const body = { prompt: v.prompt, seconds: v.seconds, resolution: v.resolution, aspect: v.aspect, audio: v.audio };
  if (v.image) body.image = await readFileB64(v.image);
  const r = await api("POST", "/v1/videos/generations", { json: body });
  await startJob("video", r.json, v.prompt);
}

async function run3d(v) {
  const form = [await filePart("image", v.image), { name: "model", value: v.model }, { name: "texture", value: v.texture }];
  const r = await api("POST", "/v1/3d/generations", { form });
  await startJob("3d", r.json, v.image.name);
}

async function runMusic(v) {
  const body = { prompt: v.prompt, lyrics: v.lyrics || "", seconds: v.seconds, language: v.language, bpm: v.bpm, model: v.model };
  const r = await api("POST", "/v1/music/generations", { json: body });
  await startJob("music", r.json, v.prompt);
}

async function runRag(v) {
  const body = { query: v.query, top_k: v.top_k || 5 };
  if (v.collection) body.collections = [v.collection];
  const r = await api("POST", "/v1/rag/query", { json: body, timeoutSecs: 300 });
  const j = r.json || {};
  const cites = (j.citations || []).map((c) => `[${c.n}] ${c.document}${c.locator ? " · " + c.locator : ""}`);
  showText((j.answer || "") + (cites.length ? "\n\nอ้างอิง\n" + cites.join("\n") : ""), "");
}

const fmtTime = (s) => {
  const t = Math.max(0, Math.floor(Number(s) || 0));
  return `${Math.floor(t / 60)}:${String(t % 60).padStart(2, "0")}`;
};

// ── jobs ────────────────────────────────────────────────────────────

async function startJob(cap, job, caption) {
  if (!job || !job.id) throw new Error("เครื่องไม่ได้ส่งงานกลับมา");
  state.jobs[job.id] = { cap, caption: String(caption || "").slice(0, 80), at: Date.now() };
  await saveJobs();
  setStatus("ส่งงานแล้ว กำลังรอคิว…");
  pollJob(job.id);
}

async function pollJob(id) {
  while (state.jobs[id]) {
    let job;
    try {
      job = (await api("GET", `/v1/jobs/${encodeURIComponent(id)}`)).json;
    } catch (e) {
      if (/\(404\)|not_found/.test(String(e.message))) return finishJob(id, null, "ไม่พบงานนี้บนเครื่องแล้ว");
      await sleep(POLL_MS * 2);
      continue;
    }
    const meta = state.jobs[id];
    if (!meta) return;
    if (job.state === "succeeded") return finishJob(id, job);
    if (job.state === "failed" || job.state === "cancelled") {
      return finishJob(id, null, job.state === "cancelled" ? "ยกเลิกแล้ว" : jobError(job.error));
    }
    meta.progress = job.progress;
    meta.queue = job.queue_position;
    renderJobs();
    await sleep(POLL_MS);
  }
}

async function finishJob(id, job, error) {
  const meta = state.jobs[id];
  delete state.jobs[id];
  await saveJobs();
  renderJobs();
  if (!meta) return;
  if (error) {
    if (meta.cap === state.cap.id) setStatus(String(error), true);
    return;
  }
  const files = [];
  for (const a of job.artifacts || []) {
    const ext = EXT[a.type] || (String(a.type || "").split("/")[1] || "bin").replace(/[^a-z0-9]/gi, "");
    const path = `${OUT}/${meta.cap}/${id}-${a.n}.${ext}`;
    try {
      await api("GET", `/v1/jobs/${encodeURIComponent(id)}/artifacts/${a.n}`, { saveTo: path, timeoutSecs: 900 });
      files.push(path);
    } catch (e) {
      setStatus(`ดาวน์โหลดผลไม่ได้: ${e.message}`, true);
    }
  }
  if (files.length) addGallery({ cap: meta.cap, files, caption: meta.caption });
  if (meta.cap === "rag") {
    const warn = ((job.result || {}).warnings || []).join(" · ");
    if (state.cap.id === "rag") setStatus(warn ? `เพิ่มเอกสารแล้ว (${warn})` : "เพิ่มเอกสารแล้ว");
    loadRag();
    return;
  }
  if (meta.cap === state.cap.id && files.length) {
    setStatus("เสร็จแล้ว");
    showFiles(files);
  }
}

async function saveJobs() {
  try { await thclaws.storage.set(JOBS_KEY, state.jobs); } catch (_) {}
}

function renderJobs() {
  const n = Object.keys(state.jobs).length;
  const pill = $("jobs-pill");
  pill.hidden = n === 0;
  pill.textContent = `กำลังทำ ${n} งาน`;
  const mine = Object.values(state.jobs).filter((j) => j.cap === state.cap.id);
  if (mine.length && !state.busy) {
    const j = mine[mine.length - 1];
    const p = j.progress != null ? ` ${Math.round(j.progress * 100)}%` : j.queue ? ` (คิวที่ ${j.queue})` : "";
    setStatus(`กำลังทำ${p}…`);
  }
}

function jobError(e) {
  if (!e) return "งานล้มเหลว";
  if (typeof e === "string") return `งานล้มเหลว: ${e}`;
  if (e.code && KNOWN_ERRORS[e.code]) return KNOWN_ERRORS[e.code];
  const msg = e.message || e.detail || e.code;
  return msg ? `งานล้มเหลว: ${String(msg).slice(0, 300)}` : "งานล้มเหลว (เครื่องไม่ได้แจ้งสาเหตุ ลองใหม่อีกครั้ง หรือแจ้งผู้ดูแล)";
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── results + gallery ───────────────────────────────────────────────

function showText(text, meta) {
  const box = $("result");
  box.hidden = false;
  box.innerHTML = "";
  if (meta) {
    const m = document.createElement("div");
    m.className = "meta";
    m.textContent = meta;
    box.appendChild(m);
  }
  const pre = document.createElement("pre");
  pre.textContent = text || "(ว่าง)";
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "ghost-btn copy";
  copy.textContent = "คัดลอก";
  copy.onclick = () => navigator.clipboard && navigator.clipboard.writeText(text || "").then(() => (copy.textContent = "คัดลอกแล้ว"));
  box.append(pre, copy);
  box.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function mediaNode(path) {
  const url = thclaws.fileUrl(path);
  const ext = path.split(".").pop().toLowerCase();
  let node;
  if (["png", "jpg", "jpeg", "webp"].includes(ext)) {
    node = document.createElement("img");
    node.loading = "lazy";
  } else if (ext === "mp4") {
    node = document.createElement("video");
    node.controls = true;
    node.preload = "metadata";
  } else if (["mp3", "wav", "flac", "opus"].includes(ext)) {
    node = document.createElement("audio");
    node.controls = true;
    node.preload = "metadata";
  } else {
    node = document.createElement("a");
    node.textContent = path.split("/").pop();
    node.href = url;
    node.download = path.split("/").pop();
    return node;
  }
  node.src = url;
  return node;
}

function showFiles(files) {
  const box = $("result");
  box.hidden = false;
  box.innerHTML = "";
  const grid = document.createElement("div");
  grid.className = "files";
  for (const f of files) {
    const cell = document.createElement("figure");
    cell.append(mediaNode(f));
    const cap = document.createElement("figcaption");
    cap.textContent = f;
    cell.append(cap);
    grid.append(cell);
  }
  box.append(grid);
  box.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function addGallery(entry) {
  state.gallery.unshift({ ...entry, at: Date.now() });
  state.gallery = state.gallery.slice(0, MAX_GALLERY);
  thclaws.storage.set(GALLERY_KEY, state.gallery).catch(() => {});
  renderGallery();
}

function renderGallery() {
  const items = state.gallery.filter((g) => g.cap === state.cap.id);
  $("gallery-title").hidden = items.length === 0;
  const box = $("gallery");
  box.innerHTML = "";
  for (const g of items.slice(0, 24)) {
    const cell = document.createElement("figure");
    cell.append(mediaNode(g.files[0]));
    const cap = document.createElement("figcaption");
    cap.textContent = g.caption || g.files[0];
    cap.title = g.files.join("\n");
    cell.append(cap);
    box.append(cell);
  }
}

// ── capability extras: voices (TTS), collections (RAG) ──────────────

async function loadVoices() {
  try {
    const voices = ((await api("GET", "/v1/audio/voices")).json || {}).voices || [];
    const f = CAPS.find((c) => c.id === "tts").fields.find((x) => x.key === "voice");
    f.options = voices.map((v) => [v.id, v.type === "reference" ? `${v.id} (คลิปตัวอย่าง)` : v.id]);
    if (state.cap.id === "tts") renderForm();
    return voices;
  } catch (_) {
    return [];
  }
}

async function ttsExtra(box) {
  const voices = await loadVoices();
  const note = document.createElement("p");
  note.className = "hint";
  note.textContent = voices.length ? `มีเสียง ${voices.length} เสียง` : "เครื่องนี้ยังไม่มีเสียง เพิ่มคลิปเสียงตัวอย่างเพื่อใช้โคลนเสียงได้";
  const label = document.createElement("label");
  label.className = "field";
  label.innerHTML = '<span class="label">เพิ่มเสียงจากคลิปตัวอย่าง (≥ 3 วินาที)</span>';
  const input = document.createElement("input");
  input.type = "file";
  input.accept = "audio/*";
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "ghost-btn";
  btn.textContent = "อัปโหลดเสียง";
  btn.onclick = () => guard(async () => {
    if (!input.files || !input.files[0]) throw new Error("เลือกไฟล์เสียงก่อน");
    const r = await api("POST", "/v1/audio/references", { form: [await filePart("audio", input.files[0])] });
    setStatus(`เพิ่มเสียง ${r.json && r.json.id} แล้ว`);
    await loadVoices();
  });
  label.append(input);
  box.append(note, label, btn);
}

async function loadRag() {
  try {
    const cols = ((await api("GET", "/v1/rag/collections")).json || {}).data || [];
    const f = CAPS.find((c) => c.id === "rag").fields.find((x) => x.key === "collection");
    f.options = [["", "ทุกคลังที่เข้าถึงได้"], ...cols.map((c) => [c.id, `${c.name} (${c.documents} เอกสาร)`])];
    if (state.cap.id === "rag") {
      renderForm();
      renderRagDocs(cols);
    }
    return cols;
  } catch (e) {
    return [];
  }
}

let ragBox = null;
async function ragExtra(box) {
  ragBox = box;
  const warn = document.createElement("p");
  warn.className = "hint warn";
  warn.textContent = "คลังเอกสารบนเครื่องนี้ใช้ร่วมกัน ทุกคนที่ใช้ thClaws บนเครื่องนี้เห็นเอกสารเดียวกัน";
  box.append(warn);
  await loadRag();
}

function renderRagDocs(cols) {
  if (!ragBox) return;
  ragBox.querySelectorAll(".rag-tools").forEach((n) => n.remove());
  const tools = document.createElement("div");
  tools.className = "rag-tools";
  const name = document.createElement("input");
  name.type = "text";
  name.placeholder = "ชื่อคลังใหม่ (a-z, 0-9)";
  const mk = document.createElement("button");
  mk.type = "button";
  mk.className = "ghost-btn";
  mk.textContent = "สร้างคลัง";
  mk.onclick = () => guard(async () => {
    const n = name.value.trim();
    if (!n) throw new Error("ตั้งชื่อคลังก่อน");
    await api("POST", "/v1/rag/collections", { json: { name: n } });
    setStatus(`สร้างคลัง ${n} แล้ว`);
    await loadRag();
  });
  const files = document.createElement("input");
  files.type = "file";
  files.multiple = true;
  const up = document.createElement("button");
  up.type = "button";
  up.className = "ghost-btn";
  up.textContent = "เพิ่มเอกสารเข้าคลังที่เลือก";
  up.onclick = () => guard(async () => {
    const col = $("f-collection") && $("f-collection").value;
    if (!col) throw new Error("เลือกคลังเอกสารก่อน");
    if (!files.files || !files.files.length) throw new Error("เลือกไฟล์ก่อน");
    const form = [];
    for (const f of files.files) form.push(await filePart("files", f));
    const r = await api("POST", `/v1/rag/collections/${encodeURIComponent(col)}/documents`, { form, timeoutSecs: 900 });
    await startJob("rag", r.json, `${files.files.length} ไฟล์ → ${col}`);
  });
  const list = document.createElement("ul");
  list.className = "rag-list";
  for (const c of cols) {
    const li = document.createElement("li");
    li.textContent = `${c.name} · ${c.documents} เอกสาร · ${c.chunks} ส่วน${c.permission && c.permission !== "owner" ? " · " + c.permission : ""}`;
    list.append(li);
  }
  tools.append(name, mk, files, up, list);
  ragBox.append(tools);
}

// ── shell ───────────────────────────────────────────────────────────

function setStatus(text, isErr) {
  const s = $("status");
  s.textContent = text || "";
  s.classList.toggle("err", !!isErr);
}

async function guard(fn) {
  if (state.busy) return;
  state.busy = true;
  $("run").disabled = true;
  try {
    await fn();
  } catch (e) {
    setStatus(e && e.message ? e.message : String(e), true);
  } finally {
    state.busy = false;
    $("run").disabled = false;
  }
}

function renderCaps() {
  const nav = $("caps");
  nav.innerHTML = "";
  for (const c of CAPS) {
    const b = document.createElement("button");
    b.type = "button";
    b.role = "tab";
    b.className = "cap" + (c === state.cap ? " active" : "");
    const info = state.caps[c.id];
    const off = info && info.available === false;
    b.innerHTML = `<span class="dot ${info ? (off ? "off" : "on") : ""}"></span><span class="t"></span><span class="l"></span>`;
    b.querySelector(".t").textContent = c.title;
    b.querySelector(".l").textContent = c.label;
    if (off) b.title = info.reason || "บริการนี้ปิดอยู่";
    b.onclick = () => select(c);
    nav.append(b);
  }
}

function select(cap) {
  state.cap = cap;
  renderCaps();
  $("cap-title").textContent = `${cap.title} · ${cap.label}`;
  const info = state.caps[cap.id];
  const st = $("cap-state");
  if (!info) st.textContent = "";
  else if (info.available === false) st.textContent = "ปิดอยู่" + (info.reason ? `: ${info.reason}` : "");
  else {
    const ready = (info.models || []).filter((m) => m.ready !== false).map((m) => m.id);
    st.textContent = ready.length ? `พร้อม · ${ready.slice(0, 3).join(", ")}` : "พร้อม";
  }
  st.classList.toggle("off", !!(info && info.available === false));
  $("cap-hint").textContent = cap.hint || "";
  $("result").hidden = true;
  $("result").innerHTML = "";
  $("extra").innerHTML = "";
  ragBox = null;
  setStatus("");
  renderForm();
  renderGallery();
  renderJobs();
  if (cap.extra) cap.extra($("extra"));
}

async function loadCapabilities() {
  try {
    const health = (await thclaws.aiserver.request({ method: "GET", path: "/v1/health" })).json || {};
    $("box").textContent = health.version ? `Suite ${health.version}` : "";
    const list = ((await api("GET", "/v1/capabilities")).json || {}).data || [];
    state.caps = Object.fromEntries(list.map((c) => [c.id, c]));
  } catch (e) {
    $("box").textContent = "ติดต่อเครื่องไม่ได้";
    setStatus(e && e.message ? e.message : String(e), true);
  }
  select(state.cap);
}

async function stored(key, valid, fallback) {
  try {
    const r = await thclaws.storage.get(key);
    const v = r && typeof r === "object" && "value" in r ? r.value : r;
    return valid(v) ? v : fallback;
  } catch (_) {
    return fallback;
  }
}

async function init() {
  state.gallery = await stored(GALLERY_KEY, (v) => Array.isArray(v), []);
  state.jobs = await stored(JOBS_KEY, (v) => v && typeof v === "object" && !Array.isArray(v), {});
  $("run").onclick = () => guard(async () => {
    const v = values();
    setStatus("กำลังทำ…");
    await state.cap.run(v);
    if (!Object.values(state.jobs).some((j) => j.cap === state.cap.id)) setStatus("เสร็จแล้ว");
  });
  $("refresh").onclick = () => loadCapabilities();
  if (thclaws.ui && thclaws.ui.onFullscreen) {
    thclaws.ui.onFullscreen((fs) => ($("exit-fullscreen").hidden = !fs));
    $("exit-fullscreen").onclick = () => thclaws.ui.exitFullscreen();
  }
  renderCaps();
  await loadCapabilities();
  for (const id of Object.keys(state.jobs)) pollJob(id);
  renderJobs();
}

init();
