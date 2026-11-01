//! The film job worker: one OS thread per job (the `ipc.rs` invoke
//! pattern) with its own current-thread tokio runtime, walking shots in
//! dependency order and writing `job.json` atomically after every
//! transition — `FilmJobStatus` only ever reads.
//!
//! Nothing is lost on app quit: Kie tasks keep running server-side and
//! `FilmGenerate {resume:true}` re-attaches — completed work is skipped
//! through the **shot-result cache** (key = sha256 of the exact Kie
//! payload → `{task_id, clip_url, credits}`), which also makes
//! edited-script re-renders incremental (only changed shots re-fire)
//! and powers the Review pane's per-shot re-roll. The job id itself is
//! the script hash, so "same script" and "same job" coincide by
//! construction.
//!
//! One active job per workspace: the in-process registry rejects a
//! second `FilmGenerate` (unless it resumes the same id), which also
//! keeps the shared caches single-writer.

use super::super::{
    compile_phase1, compile_phase2, AssetRequest, PartialShot, Phase1Result, ResolvedAsset,
};
use super::kie::KieClient;
use super::upload::KieUploader;
use super::{atomic_write_json, cache_dir, ffprobe_duration_ms, job_dir, sha256_hex};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const STATE_RUNNING: &str = "running";
pub const STATE_DONE: &str = "done";
pub const STATE_FAILED: &str = "failed";
pub const STATE_CANCELLED: &str = "cancelled";

/// Kie credit → USD (kie.ai: 1 credit = $0.005). Same basis the phase-1
/// estimate uses (300 credits = $1.50), so the running budget ceiling
/// and the up-front estimate speak the same currency.
const USD_PER_KIE_CREDIT: f64 = 0.005;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShotState {
    pub id: String,
    /// pending | assets | generating | polling | done | failed | skipped
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobState {
    pub job_id: String,
    pub state: String,
    pub budget_usd: f64,
    pub estimate_usd: f64,
    pub spent_credits: f64,
    pub shots: Vec<ShotState>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Set once assembly completes: relative paths of final.mp4 /
    /// final.srt / manifest.json under the job dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl JobState {
    fn save(&self, job_id: &str) -> Result<()> {
        atomic_write_json(&job_dir(job_id).join("job.json"), self)
    }

    pub fn load(job_id: &str) -> Result<Self> {
        let p = job_dir(job_id).join("job.json");
        let s = std::fs::read_to_string(&p)
            .map_err(|_| Error::Tool(format!("no job '{job_id}' (missing {})", p.display())))?;
        serde_json::from_str(&s).map_err(|e| Error::Tool(format!("job.json parse: {e}")))
    }

    fn shot_mut(&mut self, id: &str) -> &mut ShotState {
        self.shots
            .iter_mut()
            .find(|s| s.id == id)
            .expect("shot exists")
    }
}

/// In-process active-job registry — "running" in job.json alone may be
/// a stale artifact of a killed process; this is the live truth.
fn active() -> &'static Mutex<Option<(String, Arc<AtomicBool>)>> {
    static ACTIVE: OnceLock<Mutex<Option<(String, Arc<AtomicBool>)>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(None))
}

pub fn active_job_id() -> Option<String> {
    active().lock().unwrap().as_ref().map(|(id, _)| id.clone())
}

pub fn cancel(job_id: &str) -> Result<bool> {
    let guard = active().lock().unwrap();
    match guard.as_ref() {
        Some((id, flag)) if id == job_id => {
            flag.store(true, Ordering::Relaxed);
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub fn job_id_for_script(script: &str) -> String {
    format!("film-{}", &sha256_hex(script.as_bytes())[..12])
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ShotCacheEntry {
    task_id: String,
    clip_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    credits: Option<f64>,
}

fn shot_cache_path() -> PathBuf {
    cache_dir().join("shots.json")
}

fn load_shot_cache() -> BTreeMap<String, ShotCacheEntry> {
    std::fs::read_to_string(shot_cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Start (or resume) the job for `script`. Pre-flights are the caller's
/// UX (FilmGenerate re-checks everything anyway): compile-clean, budget
/// covers the estimate, ffmpeg present, Kie key present, no other job.
pub fn start(script: &str, budget_usd: f64, resume: bool) -> Result<String> {
    let p1 = compile_phase1(script);
    if p1.has_errors() {
        return Err(Error::Tool(format!(
            "script has compile errors — fix them first: {}",
            serde_json::to_string(&p1.errors).unwrap_or_default()
        )));
    }
    if p1.cost.total_usd > budget_usd {
        return Err(Error::Tool(format!(
            "estimate ${:.2} exceeds the confirmed budget ${budget_usd:.2} — raise budgetUsd or trim the film",
            p1.cost.total_usd
        )));
    }
    super::check_av_tools()?;
    // Pre-flight the Kie endpoint (BYOK key or gateway access) before
    // any state is created — run_job re-resolves inside the worker.
    let _ = KieClient::resolve()?;

    let job_id = job_id_for_script(script);
    {
        let mut guard = active().lock().unwrap();
        if let Some((running, _)) = guard.as_ref() {
            return Err(Error::Tool(format!(
                "job {running} is already running — cancel it or wait (one film job per workspace)"
            )));
        }
        let existing = JobState::load(&job_id).ok();
        if existing.is_some() && !resume {
            return Err(Error::Tool(format!(
                "job {job_id} already exists for this exact script — pass resume:true to continue it"
            )));
        }
        let flag = Arc::new(AtomicBool::new(false));
        *guard = Some((job_id.clone(), flag.clone()));

        let dir = job_dir(&job_id);
        std::fs::create_dir_all(dir.join("out"))?;
        std::fs::write(dir.join("script.film"), script)?;

        let mut state = existing.unwrap_or_else(|| JobState {
            job_id: job_id.clone(),
            state: STATE_RUNNING.into(),
            budget_usd,
            estimate_usd: p1.cost.total_usd,
            spent_credits: 0.0,
            shots: p1
                .shots
                .iter()
                .map(|s| ShotState {
                    id: s.id().to_string(),
                    state: "pending".into(),
                    task_id: None,
                    clip: None,
                    credits: None,
                    error: None,
                })
                .collect(),
            warnings: chain_warnings(&p1),
            artifacts: None,
            error: None,
        });
        state.state = STATE_RUNNING.into();
        state.budget_usd = budget_usd;
        state.error = None;
        state.save(&job_id)?;

        let script = script.to_string();
        let jid = job_id.clone();
        // dev-plan/45 A2: the MEMBER_ID task-local does NOT cross this
        // thread + fresh-runtime boundary. Capture it in the turn context
        // (where scope_member set it) and re-scope inside the job so every
        // gateway call in run_job attaches X-Thclaws-Member — otherwise
        // the most expensive media path (FilmGenerate) spends entirely
        // unattributed and escapes the per-member daily cap.
        // The workdir scope is lost the same way, and without it every file
        // the job writes would resolve against the shared pod root.
        let scope = crate::workdir::Scope::capture();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let outcome = rt.block_on(scope.run(run_job(&jid, &script, &flag)));
            let mut st = JobState::load(&jid).unwrap_or(state);
            match outcome {
                Ok(()) => st.state = STATE_DONE.into(),
                Err(e) if flag.load(Ordering::Relaxed) => {
                    st.state = STATE_CANCELLED.into();
                    st.error = Some(e.to_string());
                }
                Err(e) => {
                    st.state = STATE_FAILED.into();
                    st.error = Some(e.to_string());
                }
            }
            let _ = st.save(&jid);
            *active().lock().unwrap() = None;
        });
    }
    Ok(job_id)
}

/// Appendix-A drift rule: re-anchor to original refs every ~4–5 chained
/// segments — warn on longer `@continue_from` chains.
fn chain_warnings(p1: &Phase1Result) -> Vec<String> {
    let depends: BTreeMap<&str, &str> = p1
        .shots
        .iter()
        .filter_map(|s| s.depends_on().map(|d| (s.id(), d)))
        .collect();
    let mut warnings = Vec::new();
    for shot in &p1.shots {
        let mut depth = 0usize;
        let mut cur = shot.id();
        while let Some(prev) = depends.get(cur) {
            depth += 1;
            cur = prev;
            if depth > p1.shots.len() {
                break;
            }
        }
        if depth >= 4 {
            warnings.push(format!(
                "W_LONG_CHAIN shot {}: {depth} chained segments — re-anchor to original character refs every ~4 (identity drifts)",
                shot.id()
            ));
        }
    }
    warnings
}

async fn run_job(job_id: &str, script: &str, cancel: &AtomicBool) -> Result<()> {
    let p1 = compile_phase1(script);
    let uploader = KieUploader::resolve()?;
    let kie = KieClient::resolve()?;
    let mut assets: Vec<ResolvedAsset> = Vec::new();
    let mut state = JobState::load(job_id)?;

    // Upfront assets: files + TTS. Video/frame requests resolve later,
    // per shot, once their source clip exists.
    for req in &p1.asset_requests {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Tool("job cancelled".into()));
        }
        match req {
            AssetRequest::File { id, path, .. } => {
                let url = uploader.upload(Path::new(path)).await?;
                assets.push(ResolvedAsset {
                    id: id.clone(),
                    url,
                    duration_ms: None,
                });
            }
            AssetRequest::Tts {
                id,
                text,
                voice,
                lang,
                tone_hint,
            } => {
                let (path, ms) =
                    super::tts::synthesize(id, text, voice, lang, tone_hint.as_deref()).await?;
                let url = uploader.upload(&path).await?;
                assets.push(ResolvedAsset {
                    id: id.clone(),
                    url,
                    duration_ms: Some(ms),
                });
            }
            _ => {}
        }
    }

    // Dialogue-length errors (E_AUDIO_OVERRUN) surface now, before any
    // paid createTask — real TTS durations are known.
    let pre = compile_phase2(&p1, &assets);
    if let Some(fatal) = pre
        .errors
        .iter()
        .find(|e| e.code == "E_AUDIO_OVERRUN" || e.code == "E_REF_AUDIO_ONLY")
    {
        return Err(Error::Tool(
            serde_json::to_string(fatal).unwrap_or_default(),
        ));
    }

    let mut shot_cache = load_shot_cache();
    let order = topo_order(&p1)?;
    for shot_id in order {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Tool("job cancelled".into()));
        }
        let shot = p1.shots.iter().find(|s| s.id() == shot_id).expect("shot");
        if state.shot_mut(&shot_id).state == "done" {
            continue;
        }

        // Running budget ceiling. The estimate is confirmed once at start
        // (`total_usd <= budget_usd`), but per-shot ACTUALS can drift above
        // it — stop before spending the next paid shot once accumulated
        // spend passes the confirmed budget. Kie bills $0.005/credit, the
        // same basis as the phase-1 estimate (300 credits = $1.50), so both
        // speak the same currency. Rendered shots are already saved, so the
        // user can raise budgetUsd and resume.
        if state.budget_usd > 0.0 {
            let spent_usd = state.spent_credits * USD_PER_KIE_CREDIT;
            if spent_usd > state.budget_usd {
                return Err(Error::Tool(format!(
                    "budget ceiling reached: spent ~${spent_usd:.2} of the ${:.2} confirmed budget \
                     — raise budgetUsd and resume to continue (already-rendered shots are kept)",
                    state.budget_usd
                )));
            }
        }

        state.shot_mut(&shot_id).state = "assets".into();
        state.save(job_id)?;

        if let Some(dep) = shot.depends_on() {
            let dep_clip = state
                .shots
                .iter()
                .find(|s| s.id == dep)
                .and_then(|s| s.clip.clone())
                .ok_or_else(|| {
                    Error::Tool(format!("shot {shot_id}: source shot {dep} has no clip"))
                })?;
            let dep_path = job_dir(job_id).join(&dep_clip);
            if let Some(video_id) = &shot.video_asset {
                if !assets.iter().any(|a| &a.id == video_id) {
                    let prepped = prepare_ref_video(job_id, &dep_path)?;
                    let url = uploader.upload(&prepped).await?;
                    assets.push(ResolvedAsset {
                        id: video_id.clone(),
                        url,
                        duration_ms: None,
                    });
                }
            }
            if let Some(frame_id) = &shot.frame_asset {
                if !assets.iter().any(|a| &a.id == frame_id) {
                    let frame = capture_last_frame(job_id, &dep_path)?;
                    let url = uploader.upload(&frame).await?;
                    assets.push(ResolvedAsset {
                        id: frame_id.clone(),
                        url,
                        duration_ms: None,
                    });
                }
            }
        }

        let p2 = compile_phase2(&p1, &assets);
        let payload = p2
            .payloads
            .iter()
            .find(|p| p.shot_id == shot_id)
            .ok_or_else(|| {
                Error::Tool(format!(
                    "shot {shot_id}: no payload — {}",
                    serde_json::to_string(&p2.errors).unwrap_or_default()
                ))
            })?
            .clone();
        let payload_hash = sha256_hex(payload.payload.to_string().as_bytes());
        let clip_rel = format!("out/shot-{shot_id}.mp4");
        let clip_abs = job_dir(job_id).join(&clip_rel);
        // Raw Seedance clip stays separate; the delivered clip is the
        // dialogue-overlaid version (produced idempotently from raw).
        let raw_abs = job_dir(job_id).join(format!("out/shot-{shot_id}.raw.mp4"));

        if let Some(hit) = shot_cache.get(&payload_hash) {
            if !raw_abs.exists() {
                if hit.clip_url.is_empty() {
                    // sync backend (LTX) has no re-downloadable URL — regenerate.
                    super::dispatch::generate_clip(
                        payload.backend,
                        &payload.payload,
                        &kie,
                        &raw_abs,
                        cancel,
                        |_| {},
                    )
                    .await?;
                } else {
                    kie.download(&hit.clip_url, &raw_abs).await?;
                }
            }
            finalize_dialogue_clip(job_id, shot, &raw_abs, &clip_abs)?;
            let s = state.shot_mut(&shot_id);
            s.state = "done".into();
            s.task_id = Some(hit.task_id.clone());
            s.clip = Some(clip_rel);
            s.credits = hit.credits;
            state.save(job_id)?;
            continue;
        }

        state.shot_mut(&shot_id).state = "generating".into();
        state.save(job_id)?;
        // Dispatch to the shot's backend (D5b): Grok/Seedance/Veo on Kie,
        // LTX native-sync, Happy Horse on DashScope. The task id is persisted
        // as soon as submit returns so a crash mid-poll resumes rather than
        // re-spends.
        let mut submitted_id: Option<String> = None;
        let (task_id, result) = super::dispatch::generate_clip(
            payload.backend,
            &payload.payload,
            &kie,
            &raw_abs,
            cancel,
            |tid| submitted_id = Some(tid.to_string()),
        )
        .await
        .inspect_err(|_| {
            if let Some(tid) = &submitted_id {
                let s = state.shot_mut(&shot_id);
                s.task_id = Some(tid.clone());
                s.state = "polling".into();
                let _ = state.save(job_id);
            }
        })?;
        let _ = ffprobe_duration_ms(&raw_abs)?;
        // Swap Seedance's regenerated timbre for the real Gemini TTS,
        // aligned to the on-screen speech onset (see finalize fn).
        finalize_dialogue_clip(job_id, shot, &raw_abs, &clip_abs)?;

        shot_cache.insert(
            payload_hash,
            ShotCacheEntry {
                task_id: task_id.clone(),
                clip_url: result.clip_url.clone(),
                credits: result.credits,
            },
        );
        atomic_write_json(&shot_cache_path(), &shot_cache)?;

        let s = state.shot_mut(&shot_id);
        s.state = "done".into();
        s.clip = Some(clip_rel);
        s.credits = result.credits;
        state.spent_credits += result.credits.unwrap_or(0.0);
        state.save(job_id)?;
    }

    // Tier 4: all clips in hand → assemble the film.
    let assembled = super::assemble::assemble(job_id, &p1.assembly_plan, &state)?;
    let rel = |p: &Path| {
        p.strip_prefix(job_dir(job_id))
            .unwrap_or(p)
            .to_string_lossy()
            .to_string()
    };
    state.artifacts = Some(serde_json::json!({
        "mp4": rel(&assembled.mp4),
        "srt": rel(&assembled.srt),
        "manifest": rel(&assembled.manifest),
    }));
    state.save(job_id)?;

    Ok(())
}

/// Dependency-respecting shot order. `depends_on` targets are already
/// validated (E_CONTINUE_UNKNOWN forces earlier shots), so file order
/// with a sanity check suffices — a cycle can't parse.
fn topo_order(p1: &Phase1Result) -> Result<Vec<String>> {
    let mut done: Vec<String> = Vec::new();
    for shot in &p1.shots {
        if let Some(dep) = shot.depends_on() {
            if !done.iter().any(|d| d == dep) {
                return Err(Error::Tool(format!(
                    "shot {}: dependency {dep} not scheduled earlier",
                    shot.id()
                )));
            }
        }
        done.push(shot.id().to_string());
    }
    Ok(done)
}

/// Compensation added to the detected onset before the TTS lands. Zero:
/// Seedance's own generated audio starts exactly where the mouth voices,
/// so the (silence-trimmed) TTS placed at that onset is already aligned.
/// A first tuning pass found +0.75 "perfect" — but that was compensating
/// for one clip set whose quiet pre-speech tripped silencedetect early;
/// with the −30dB threshold below (which locks onto real speech, not
/// breath) the onset is clean and a fixed comp over-delays other clips
/// (jimmy, 2026-07-02: "0 is perfect").
const DIALOGUE_ONSET_COMP: f64 = 0.0;

/// Native lip-sync gives correct mouth *timing* but Seedance
/// regenerates the *timbre* (the voice stops sounding like the chosen
/// Gemini/MiniMax TTS). So for dialogue shots we keep the native motion
/// and swap the audio track for the real TTS, aligned to the clip's
/// speech onset — real voice + native timing, no repaint model (the
/// cheap middle path between `native` and `post`). Non-dialogue shots
/// just copy through. Idempotent: always derives `final` from `raw`.
fn finalize_dialogue_clip(job_id: &str, shot: &PartialShot, raw: &Path, out: &Path) -> Result<()> {
    let tts_file = shot
        .tts_asset
        .as_deref()
        .map(|id| super::cache_dir().join("tts").join(format!("{id}.mp3")));
    let tts_file = match tts_file {
        Some(p) if p.exists() => p,
        _ => {
            std::fs::copy(raw, out)?;
            return Ok(());
        }
    };

    // Trim the TTS's own leading silence so its first phoneme is at t=0;
    // the internal lead (~0.25s) otherwise makes alignment unpredictable.
    let trimmed = job_dir(job_id).join(format!(
        "out/{}-tts.wav",
        out.file_stem().and_then(|s| s.to_str()).unwrap_or("clip")
    ));
    let ok = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(&tts_file)
        .args([
            "-af",
            "silenceremove=start_periods=1:start_threshold=-40dB:start_silence=0.02",
        ])
        .arg(&trimmed)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        std::fs::copy(raw, out)?;
        return Ok(());
    }

    // Defensive: the overlay only aligns if the detected onset is the
    // speech start. If it lands implausibly late — past a point where the
    // trimmed TTS could still fit in the clip — silencedetect likely
    // locked onto in-clip ambience (explosion/rain) rather than the voice
    // (see W_DIALOGUE_AMBIENT). Rather than misalign, keep the native
    // audio track for this shot.
    let clip_ms = ffprobe_duration_ms(raw).unwrap_or(0);
    let tts_ms = ffprobe_duration_ms(&trimmed).unwrap_or(0);
    let onset = detect_speech_onset(raw).unwrap_or(0.3);
    let delay_ms = ((onset + DIALOGUE_ONSET_COMP) * 1000.0).max(0.0) as u64;
    if clip_ms > 0 && delay_ms + tts_ms > clip_ms + 250 {
        let _ = std::fs::remove_file(&trimmed);
        std::fs::copy(raw, out)?;
        return Ok(());
    }

    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(raw)
        .arg("-i")
        .arg(&trimmed)
        .args([
            "-filter_complex",
            &format!("[1:a]adelay={delay_ms}|{delay_ms},apad[a]"),
            "-map",
            "0:v",
            "-map",
            "[a]",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(out)
        .status()
        .map_err(|e| Error::Tool(format!("ffmpeg dialogue overlay: {e}")))?;
    let _ = std::fs::remove_file(&trimmed);
    if !status.success() {
        std::fs::copy(raw, out)?;
    }
    Ok(())
}

/// First on-screen voiced moment (seconds) via silencedetect at -35dB,
/// the threshold that tracked the mouth-start in the live tuning pass.
fn detect_speech_onset(clip: &Path) -> Option<f64> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "info", "-i"])
        .arg(clip)
        .args(["-af", "silencedetect=noise=-30dB:d=0.15", "-f", "null", "-"])
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    stderr
        .lines()
        .find_map(|l| l.split("silence_end: ").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<f64>().ok())
}

/// Kie ref-video constraints (T0: 1280×720/24fps/6s passed whole).
/// Out-of-bounds clips get one normalizing transcode.
fn prepare_ref_video(job_id: &str, clip: &Path) -> Result<PathBuf> {
    // ALWAYS strip audio (`-an`): a `@continue_from` clip carries the
    // previous shot's spoken line, and Seedance lets that bleed into
    // the new shot's speech, competing with @Audio1 (docs/seedance-api.md
    // "เมื่อ audio ref กับ video ref ตีกัน" — mute the camera-ref clip).
    // Observed live: shot 2's dialogue picked up "ตั้งแต่เช้า" from shot
    // 1's ref clip. The ref is for camera/motion only.
    let within = probe_ref_ok(clip).unwrap_or(false);
    let out = job_dir(job_id).join("out").join(format!(
        "{}-ref.mp4",
        clip.file_stem().and_then(|s| s.to_str()).unwrap_or("clip")
    ));
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-y", "-v", "quiet", "-i"])
        .arg(clip)
        .args(["-an", "-t", "15"]);
    if !within {
        cmd.args([
            "-vf",
            "scale=1280:720:force_original_aspect_ratio=decrease",
            "-r",
            "24",
        ]);
    } else {
        cmd.args(["-c:v", "copy"]);
    }
    let status = cmd
        .arg(&out)
        .status()
        .map_err(|e| Error::Tool(format!("ffmpeg ref prep: {e}")))?;
    if !status.success() {
        return Err(Error::Tool(
            "ffmpeg failed preparing the reference clip".into(),
        ));
    }
    Ok(out)
}

fn probe_ref_ok(clip: &Path) -> Result<bool> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate:format=duration,size",
            "-of",
            "json",
        ])
        .arg(clip)
        .output()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| Error::Tool(format!("ffprobe json: {e}")))?;
    let c = super::super::RefVideoConstraints::default();
    let stream = &v["streams"][0];
    let (w, h) = (
        stream["width"].as_u64().unwrap_or(0),
        stream["height"].as_u64().unwrap_or(0),
    );
    let pixels = w * h;
    let fps = stream["r_frame_rate"]
        .as_str()
        .and_then(|r| r.split_once('/'))
        .and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?.max(1.0)))
        .unwrap_or(0.0);
    let duration: f64 = v["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(f64::MAX);
    let size: u64 = v["format"]["size"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX);
    Ok(pixels >= c.min_total_pixels
        && pixels <= c.max_total_pixels
        && fps >= c.min_fps as f64
        && fps <= c.max_fps as f64 + 0.5
        && duration <= c.max_duration_s as f64 + 0.5
        && size <= c.max_bytes)
}

fn capture_last_frame(job_id: &str, clip: &Path) -> Result<PathBuf> {
    let out = job_dir(job_id).join("out").join(format!(
        "{}-lastframe.png",
        clip.file_stem().and_then(|s| s.to_str()).unwrap_or("clip")
    ));
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "quiet", "-sseof", "-0.1", "-i"])
        .arg(clip)
        .args(["-frames:v", "1"])
        .arg(&out)
        .status()
        .map_err(|e| Error::Tool(format!("ffmpeg frame capture: {e}")))?;
    if !status.success() {
        return Err(Error::Tool(
            "ffmpeg failed capturing the match-cut frame".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_id_is_script_hash() {
        let a = job_id_for_script("shot 1 {\nx\n}\n");
        let b = job_id_for_script("shot 1 {\nx\n}\n");
        let c = job_id_for_script("shot 1 {\ny\n}\n");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("film-"));
    }

    #[test]
    fn chain_warning_at_depth_four() {
        let src = "shot 1 {\na\n}\nshot 2 {\n@continue_from: 1\nb\n}\nshot 3 {\n@continue_from: 2\nc\n}\nshot 4 {\n@continue_from: 3\nd\n}\nshot 5 {\n@continue_from: 4\ne\n}\n";
        let p1 = compile_phase1(src);
        assert!(!p1.has_errors(), "{:?}", p1.errors);
        let w = chain_warnings(&p1);
        assert!(
            w.iter()
                .any(|w| w.contains("shot 5") && w.contains("4 chained")),
            "{w:?}"
        );
    }

    #[test]
    fn topo_order_respects_dependencies() {
        let p1 = compile_phase1("shot 1 {\na\n}\nshot 2 {\n@continue_from: 1\nb\n}\n");
        assert_eq!(topo_order(&p1).unwrap(), vec!["1", "2"]);
    }
}
