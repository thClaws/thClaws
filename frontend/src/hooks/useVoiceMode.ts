/**
 * Voice mode for the chat composer on an AIServer Suite (local AI box or
 * EE Pack): hold the mic to talk (clip → Suite ASR → text), and with the
 * speaker on, the assistant's reply is read aloud when it is complete
 * (Suite TTS), and the model is asked for a short spoken answer. The engine makes both calls (`voice_*` frames),
 * so no key reaches the page. Off a Suite, `available` stays false and the
 * composer shows nothing.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { send, subscribe } from "./useIPC";

/** `loaded` and `base` (Suite ≥ 0.2.183): on the GPU, and its model size; absent = unknown. */
export type VoiceInfo = {
  id: string;
  type?: string;
  label?: string;
  loaded?: boolean;
  base?: string;
  gender?: string;
};

/** Thai is spoken differently by a woman and a man (ค่ะ / ครับ), so the
 *  model has to know whose voice will read the reply. "" = not known. */
export type VoiceGender = "female" | "male" | "";

/** From the Suite's `gender` when it gives one, else from the label
 *  ("ตัวอย่าง 1 · หญิง"). A voice someone trained has neither. */
function genderOf(v: VoiceInfo | undefined): VoiceGender {
  const t = `${v?.gender || ""} ${v?.label || ""}`.toLowerCase();
  if (/หญิง|female|woman/.test(t)) return "female";
  if (/ชาย|\bmale\b|\bman\b/.test(t)) return "male";
  return "";
}

const SPEAK_KEY = "thclaws.voice.speak";
const VOICE_KEY = "thclaws.voice.voice";
const MAX_CLIP_MS = 60_000;
const MIN_CLIP_MS = 400;
const GENDER_KEY = "thclaws.voice.gender.";
const WARMUP: Record<VoiceGender, string> = { female: "สวัสดีค่ะ", male: "สวัสดีครับ", "": "สวัสดี" };

function load(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function save(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* private mode: per-session only */
  }
}

let seq = 0;
const nextId = () => `v${Date.now().toString(36)}-${++seq}`;

/** Waits for the one reply frame carrying `id`. */
function request<T>(
  type: string,
  body: Record<string, unknown>,
  timeoutMs = 0,
  lenient = false,
): Promise<T> {
  const id = nextId();
  return new Promise<T>((resolve, reject) => {
    let timer: number | null = null;
    const unsub = subscribe((msg) => {
      if (msg.type !== `${type}_result` || msg.id !== id) return;
      unsub();
      if (timer !== null) window.clearTimeout(timer);
      if (!lenient && typeof msg.error === "string" && msg.error) reject(new Error(msg.error));
      else resolve(msg as unknown as T);
    });
    if (timeoutMs > 0)
      timer = window.setTimeout(() => {
        unsub();
        reject(new Error("timeout"));
      }, timeoutMs);
    send({ type, id, ...body });
  });
}

/** Markdown → plain text worth saying: no code, links read as their label. */
export function speakable(md: string): string {
  return md
    .replace(/```[\s\S]*?(```|$)/g, " ")
    .replace(/`[^`]*`/g, " ")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/https?:\/\/\S+/g, " ")
    .replace(/^\s{0,3}(#{1,6}|>|[-*+]|\d+\.)\s+/gm, "")
    .replace(/[*_~|#]+/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

export function useVoiceMode(onTranscript: (text: string) => void) {
  const [available, setAvailable] = useState(false);
  const [asrUp, setAsrUp] = useState(false);
  const [ttsUp, setTtsUp] = useState(false);
  const [voices, setVoices] = useState<VoiceInfo[]>([]);
  const [voice, setVoiceState] = useState<string>(load(VOICE_KEY) || "");
  const [speakOn, setSpeakOn] = useState(load(SPEAK_KEY) === "1");
  // The user's own answer for a voice wins over what its label says.
  const [genderPicks, setGenderPicks] = useState(0);
  const gender = useMemo<VoiceGender>(() => {
    void genderPicks;
    const saved = load(GENDER_KEY + voice);
    if (saved === "female" || saved === "male") return saved;
    return genderOf(voices.find((v) => v.id === voice));
  }, [voice, voices, genderPicks]);
  const genderRef = useRef<VoiceGender>(gender);
  genderRef.current = gender;
  const setGender = useCallback(
    (g: VoiceGender) => {
      save(GENDER_KEY + voice, g);
      setGenderPicks((n) => n + 1);
    },
    [voice],
  );
  const [recording, setRecording] = useState(false);
  const [transcribing, setTranscribing] = useState(false);
  const [speaking, setSpeaking] = useState(false);
  // Waiting on the box for audio with nothing playing: a voice that is not
  // on the GPU takes from half a minute to a few minutes to load.
  const [preparing, setPreparing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const micSupported =
    typeof navigator !== "undefined" &&
    !!navigator.mediaDevices?.getUserMedia &&
    typeof MediaRecorder !== "undefined";

  const onTranscriptRef = useRef(onTranscript);
  onTranscriptRef.current = onTranscript;
  const speakOnRef = useRef(speakOn);
  speakOnRef.current = speakOn;
  const voiceRef = useRef(voice);
  voiceRef.current = voice;
  const runningRef = useRef(false);

  useEffect(() => {
    let live = true;
    type Status = { available: boolean; asr: boolean; tts: boolean; voices: VoiceInfo[]; defaultVoice: string };
    // The socket may not be up when the composer mounts (the host is still
    // starting the agent), and a frame sent then is dropped: ask again.
    const ask = async (): Promise<Status> => {
      for (let i = 0; ; i++) {
        try {
          // Lenient: a status with `error` still says which half is up.
          return await request<Status>("voice_status", {}, 4000, true);
        } catch (e) {
          if ((e as Error).message !== "timeout" || i >= 14 || !live) throw e;
        }
      }
    };
    ask()
      .then((r) => {
        if (!live) return;
        setAvailable(!!r.available);
        setAsrUp(!!r.asr);
        setTtsUp(!!r.tts);
        // `reference` entries are raw clips uploaded to clone from, not voices.
        const list = (Array.isArray(r.voices) ? r.voices : []).filter((v) => v.type !== "reference");
        setVoices(list);
        setVoiceState((cur) => {
          if (cur && list.some((v) => v.id === cur)) return cur;
          // A 1.5b voice is generated at about real time; a 7b one at 2 to 3
          // times slower, which stalls a stream.
          const fast = list.filter((v) => v.base === "1.5b");
          const pick = fast.find((v) => v.loaded === true) ?? fast[0] ?? list.find((v) => v.loaded === true);
          if (pick) return pick.id;
          if (list.some((v) => v.id === r.defaultVoice)) return r.defaultVoice;
          return list[0]?.id || "";
        });
      })
      .catch(() => live && setAvailable(false));
    return () => {
      live = false;
    };
  }, []);

  // The box keeps one voice on the GPU and anyone can swap it, so `loaded`
  // from the status goes stale: warm the chosen voice whenever it is picked.
  const warmUp = useCallback((v: string) => {
    if (!v) return;
    setPreparing(true);
    request("voice_speak", { text: WARMUP[genderRef.current], voice: v })
      .catch((e) => setError(voiceError(e)))
      .finally(() => voiceRef.current === v && !runningRef.current && setPreparing(false));
  }, []);

  const setVoice = useCallback(
    (v: string) => {
      setVoiceState(v);
      voiceRef.current = v;
      save(VOICE_KEY, v);
      if (speakOnRef.current) warmUp(v);
    },
    [warmUp],
  );

  // ── speaking ─────────────────────────────────────────────────────────
  // One TTS worker per box and requests queue there, so the next answer is
  // asked for only while the current one plays. `gen` drops anything in
  // flight after stop().
  const bufRef = useRef("");
  const queueRef = useRef<string[]>([]);
  const genRef = useRef(0);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const ctxRef = useRef<AudioContext | null>(null);
  const sourcesRef = useRef(new Set<AudioBufferSourceNode>());
  const hangUpRef = useRef<(() => void) | null>(null);

  const stopSpeaking = useCallback(() => {
    genRef.current++;
    bufRef.current = "";
    queueRef.current = [];
    runningRef.current = false;
    hangUpRef.current?.();
    sourcesRef.current.forEach((src) => {
      try {
        src.stop();
      } catch {
        /* already ended */
      }
    });
    sourcesRef.current.clear();
    audioRef.current?.pause();
    audioRef.current = null;
    setSpeaking(false);
    setPreparing(false);
  }, []);

  /** The answer as it is generated: PCM frames, played as they arrive. */
  const playStream = useCallback(
    (text: string, gen: number) =>
      new Promise<void>((resolve, reject) => {
        const ctx = audioContext(ctxRef);
        const id = nextId();
        let rate = 24_000;
        let started = false;
        let waiting: Float32Array[] = [];
        let waitingSecs = 0;
        let nextAt = 0;
        let frames = 0;
        let timer: number | null = null;
        const schedule = (pcm: Float32Array) => {
          const buf = ctx.createBuffer(1, pcm.length, rate);
          buf.getChannelData(0).set(pcm);
          const src = ctx.createBufferSource();
          src.buffer = buf;
          src.connect(ctx.destination);
          // The box fell behind the playhead: wait a beat, don't stutter.
          if (nextAt < ctx.currentTime + 0.02) nextAt = ctx.currentTime + (frames > 8 ? 0.3 : 0.05);
          src.start(nextAt);
          nextAt += buf.duration;
          sourcesRef.current.add(src);
          src.onended = () => sourcesRef.current.delete(src);
        };
        const start = () => {
          started = true;
          setPreparing(false);
          waiting.forEach(schedule);
          waiting = [];
        };
        const done = (err?: Error) => {
          unsub();
          if (timer !== null) window.clearTimeout(timer);
          hangUpRef.current = null;
          if (err) reject(err);
          else resolve();
        };
        const unsub = subscribe((msg) => {
          if (msg.id !== id || gen !== genRef.current) return;
          if (msg.type === "voice_audio") {
            rate = Number(msg.rate) || rate;
            const pcm = pcmToFloat(String(msg.data || ""));
            frames++;
            if (started) schedule(pcm);
            else {
              waiting.push(pcm);
              waitingSecs += pcm.length / rate;
              if (waitingSecs >= START_BUFFER_SECS) start();
            }
          } else if (msg.type === "voice_speak_stream_result") {
            if (typeof msg.error === "string" && msg.error && !frames) {
              done(new Error(msg.error));
              return;
            }
            if (!started) start();
            unsub();
            timer = window.setTimeout(() => done(), Math.max(0, nextAt - ctx.currentTime) * 1000 + 80);
          }
        });
        hangUpRef.current = () => {
          send({ type: "voice_speak_cancel", id });
          done();
        };
        send({ type: "voice_speak_stream", id, text, voice: voiceRef.current });
      }),
    [],
  );

  /** A Suite without streaming: the whole clip, then play. */
  const playClip = useCallback(async (text: string, gen: number) => {
    const r = await request<{ data: string; mime: string }>("voice_speak", { text, voice: voiceRef.current });
    if (gen !== genRef.current) return;
    setPreparing(false);
    const a = new Audio(`data:${r.mime || "audio/mpeg"};base64,${r.data}`);
    audioRef.current = a;
    await new Promise<void>((done) => {
      a.onended = () => done();
      a.onerror = () => done();
      a.play().catch(() => done());
    });
  }, []);

  const pump = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    const gen = genRef.current;
    setSpeaking(true);
    try {
      while (gen === genRef.current && queueRef.current.length) {
        const text = queueRef.current.shift()!;
        setPreparing(true);
        try {
          await playStream(text, gen);
        } catch (e) {
          // Busy or out of memory is the box's answer; anything else is a
          // Suite that does not stream yet.
          if (/^(busy|memory short)/.test(String((e as Error).message))) throw e;
          await playClip(text, gen);
        }
      }
    } catch (e) {
      if (gen === genRef.current) {
        setError(voiceError(e));
        queueRef.current = [];
      }
    } finally {
      if (gen === genRef.current) {
        runningRef.current = false;
        setSpeaking(false);
        setPreparing(false);
      }
    }
  }, [playStream, playClip]);

  const enqueue = useCallback(
    (answer: string) => {
      const text = speakable(answer);
      if (!text) return;
      queueRef.current.push(text);
      void pump();
    },
    [pump],
  );

  // The whole answer goes to the box in one call, when it is complete:
  // a call per sentence left a silence before every sentence.
  /** A streamed piece of the assistant's reply. */
  const feed = useCallback(
    (delta: string) => {
      if (!speakOnRef.current || !ttsUp) return;
      bufRef.current += delta;
    },
    [ttsUp],
  );

  /** A tool call follows: what was said so far was a preamble, not the answer. */
  const discard = useCallback(() => {
    bufRef.current = "";
  }, []);

  /** The turn ended: say the answer. */
  const flush = useCallback(() => {
    const text = bufRef.current;
    bufRef.current = "";
    if (!speakOnRef.current || !ttsUp) return;
    enqueue(text);
  }, [ttsUp, enqueue]);

  const toggleSpeak = useCallback(() => {
    setSpeakOn((on) => {
      const next = !on;
      save(SPEAK_KEY, next ? "1" : "0");
      if (!next) stopSpeaking();
      else {
        // A click is the browser's licence to play sound later.
        audioContext(ctxRef);
        warmUp(voiceRef.current);
      }
      return next;
    });
  }, [stopSpeaking, warmUp]);

  // ── listening ────────────────────────────────────────────────────────
  const recRef = useRef<MediaRecorder | null>(null);
  const startedRef = useRef(0);
  const capRef = useRef<number | null>(null);

  const startRecording = useCallback(async () => {
    if (!asrUp || !micSupported || recRef.current) return;
    setError(null);
    stopSpeaking();
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch (e) {
      setError(`Microphone: ${(e as Error).message || e}`);
      return;
    }
    const mime = ["audio/webm;codecs=opus", "audio/webm", "audio/mp4", "audio/ogg"].find(
      (m) => MediaRecorder.isTypeSupported?.(m),
    );
    const rec = mime ? new MediaRecorder(stream, { mimeType: mime }) : new MediaRecorder(stream);
    const chunks: Blob[] = [];
    rec.ondataavailable = (ev) => ev.data.size && chunks.push(ev.data);
    rec.onstop = async () => {
      stream.getTracks().forEach((t) => t.stop());
      recRef.current = null;
      setRecording(false);
      if (capRef.current !== null) window.clearTimeout(capRef.current);
      if (Date.now() - startedRef.current < MIN_CLIP_MS || !chunks.length) return;
      const type = rec.mimeType || mime || "audio/webm";
      const blob = new Blob(chunks, { type });
      setTranscribing(true);
      try {
        const data = await blobToBase64(blob);
        const r = await request<{ text: string }>("voice_transcribe", { data, mime: type });
        const text = (r.text || "").trim();
        if (text) onTranscriptRef.current(text);
        else setError("No speech heard");
      } catch (e) {
        setError(voiceError(e));
      } finally {
        setTranscribing(false);
      }
    };
    recRef.current = rec;
    startedRef.current = Date.now();
    rec.start();
    setRecording(true);
    capRef.current = window.setTimeout(() => rec.state !== "inactive" && rec.stop(), MAX_CLIP_MS);
  }, [asrUp, micSupported, stopSpeaking]);

  const stopRecording = useCallback(() => {
    const rec = recRef.current;
    if (rec && rec.state !== "inactive") rec.stop();
  }, []);

  useEffect(() => () => {
    stopRecording();
    stopSpeaking();
  }, [stopRecording, stopSpeaking]);

  return {
    available,
    asrUp,
    ttsUp,
    micSupported,
    voices,
    voice,
    setVoice,
    gender,
    setGender,
    speakOn,
    toggleSpeak,
    recording,
    transcribing,
    speaking,
    preparing,
    error,
    clearError: () => setError(null),
    startRecording,
    stopRecording,
    feed,
    discard,
    flush,
    stopSpeaking,
  };
}

const START_BUFFER_SECS = 1;

function audioContext(ref: { current: AudioContext | null }): AudioContext {
  if (!ref.current) ref.current = new AudioContext();
  if (ref.current.state === "suspended") void ref.current.resume();
  return ref.current;
}

/** base64 PCM s16le → samples in -1..1. */
function pcmToFloat(b64: string): Float32Array {
  const bin = atob(b64);
  const out = new Float32Array(bin.length >> 1);
  for (let i = 0; i < out.length; i++) {
    const v = bin.charCodeAt(2 * i) | (bin.charCodeAt(2 * i + 1) << 8);
    out[i] = (v >= 0x8000 ? v - 0x10000 : v) / 0x8000;
  }
  return out;
}

function voiceError(e: unknown): string {
  const m = String((e as Error)?.message || e);
  // Keep the Suite's own status and message: the Thai line alone hid which
  // call failed and why.
  if (m.startsWith("memory short")) return `เครื่องหน่วยความจำไม่พอ [${m}]`;
  if (m.startsWith("busy")) return `เครื่องกำลังทำงานอื่นอยู่ ลองใหม่อีกครั้ง [${m}]`;
  return m;
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => {
      const s = String(r.result || "");
      resolve(s.slice(s.indexOf(",") + 1));
    };
    r.onerror = () => reject(r.error);
    r.readAsDataURL(blob);
  });
}
