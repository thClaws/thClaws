//! Capture build-time metadata so released binaries can identify themselves.
//!
//! Sets the following `cargo:rustc-env` variables which `src/version.rs`
//! reads via `env!()`:
//!   THCLAWS_GIT_SHA       — short commit hash of HEAD, or "unknown"
//!   THCLAWS_GIT_DIRTY     — "1" if the working tree had uncommitted changes
//!                           at build time, "0" otherwise
//!   THCLAWS_GIT_BRANCH    — current branch name, or "unknown"
//!   THCLAWS_BUILD_TIME    — ISO-8601 UTC timestamp of the build
//!   THCLAWS_BUILD_PROFILE — "debug" / "release"
//!
//! The build script intentionally doesn't fail if git is missing (source
//! tarball installs, Docker without git, etc.) — it just reports "unknown".

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Re-run when git HEAD moves (covers most branch switches and commits).
    println!("cargo:rerun-if-changed=../../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../../.git/index");
    // Always re-run when build.rs itself changes.
    println!("cargo:rerun-if-changed=build.rs");

    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = match git(&["status", "--porcelain"]) {
        Some(s) if !s.trim().is_empty() => "1",
        Some(_) => "0",
        None => "0",
    };

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    let build_time = iso8601_utc_now();

    println!("cargo:rustc-env=THCLAWS_GIT_SHA={sha}");
    println!("cargo:rustc-env=THCLAWS_GIT_BRANCH={branch}");
    println!("cargo:rustc-env=THCLAWS_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=THCLAWS_BUILD_TIME={build_time}");
    println!("cargo:rustc-env=THCLAWS_BUILD_PROFILE={profile}");

    // Optional: enterprise-build embeds a customer-specific Ed25519 public key
    // used to verify org policy files. Source-of-truth resolution at build time:
    //
    //   1. `THCLAWS_POLICY_PUBKEY_PATH` env var — explicit override, used by
    //      per-customer CI builds to point at the right pubkey for each
    //      enterprise SKU.
    //   2. `~/.config/thclaws/policy.pub` — conventional default path. Solo
    //      operators just drop the pubkey here and run `cargo build`; no env
    //      var to remember. Same dir the runtime loader looks for `policy.json`.
    //
    // If neither is found, the build proceeds with no embedded key and the
    // open-core binary ships unchanged. Runtime can still pick up a key via
    // env var or `~/.config/thclaws/policy.pub` for testing / self-locking.
    println!("cargo:rerun-if-env-changed=THCLAWS_POLICY_PUBKEY_PATH");
    let pubkey_path: Option<String> = match std::env::var("THCLAWS_POLICY_PUBKEY_PATH") {
        Ok(p) if !p.trim().is_empty() => Some(p),
        _ => default_pubkey_path().filter(|p| std::path::Path::new(p).exists()),
    };
    let embedded_pubkey = match pubkey_path {
        Some(path) => {
            println!("cargo:rerun-if-changed={path}");
            match std::fs::read(&path) {
                Ok(bytes) => Some(decode_pubkey_bytes(&bytes, &path)),
                Err(e) => panic!("policy pubkey at {path:?} unreadable: {e}"),
            }
        }
        None => None,
    };
    let embedded_b64 = embedded_pubkey
        .map(|b| base64_encode(&b))
        .unwrap_or_default();
    println!("cargo:rustc-env=THCLAWS_EMBEDDED_POLICY_PUBKEY={embedded_b64}");

    embed_windows_icon();
}

/// Embed `resources/thclaws.ico` into the Windows PE so Explorer, the
/// taskbar, alt-tab, and file properties pick up the icon (issue #53).
/// No-op on non-Windows targets — `winresource` is gated behind a
/// `cfg(windows)` build-dependency so this whole call disappears at
/// compile time on Linux/macOS.
#[cfg(windows)]
fn embed_windows_icon() {
    println!("cargo:rerun-if-changed=resources/thclaws.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("resources/thclaws.ico");
    if let Err(e) = res.compile() {
        // Don't fail the build over an icon — Windows users without the
        // toolchain pieces (rc.exe / windres) can still ship a working
        // binary, just without the embedded icon. Surface the warning
        // so CI release builds catch a misconfiguration.
        println!("cargo:warning=winresource compile failed: {e}");
    }
}

#[cfg(not(windows))]
fn embed_windows_icon() {}

/// Conventional path used at build time when `THCLAWS_POLICY_PUBKEY_PATH`
/// isn't explicitly set. Mirrors the runtime loader's fallback so the
/// solo-operator workflow is "drop the file once, both build and runtime
/// pick it up."
fn default_pubkey_path() -> Option<String> {
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())?;
    Some(format!("{home}/.config/thclaws/policy.pub"))
}

/// Accept either raw 32-byte Ed25519 key material or a PEM/base64-encoded
/// equivalent. Anything else fails the build hard — a malformed pubkey at
/// build time would silently break verification at runtime.
fn decode_pubkey_bytes(raw: &[u8], path: &str) -> Vec<u8> {
    if raw.len() == 32 {
        return raw.to_vec();
    }
    let text = std::str::from_utf8(raw).unwrap_or_else(|_| {
        panic!("policy pubkey at {path:?} is {} bytes and not valid UTF-8 — expected 32 raw bytes or base64/PEM text", raw.len())
    });
    let trimmed = text.trim();
    // Strip PEM-style header/footer if present.
    let inner: String = if trimmed.starts_with("-----BEGIN") {
        trimmed
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("")
    } else {
        trimmed.replace('\n', "").replace('\r', "")
    };
    let decoded = base64_decode(&inner)
        .unwrap_or_else(|e| panic!("policy pubkey at {path:?} is not valid base64: {e}"));
    if decoded.len() != 32 {
        panic!(
            "policy pubkey at {path:?} decoded to {} bytes; expected 32 (raw Ed25519 public key)",
            decoded.len()
        );
    }
    decoded
}

/// Tiny self-contained base64 encoder so build.rs has no extra deps.
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = input.bytes().filter(|b| *b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut buf = [0u8; 4];
        for (i, b) in chunk.iter().enumerate() {
            buf[i] = val(*b).ok_or_else(|| format!("invalid base64 char {:?}", *b as char))?;
        }
        out.push((buf[0] << 2) | (buf[1] >> 4));
        if chunk.len() > 2 {
            out.push((buf[1] << 4) | (buf[2] >> 2));
        }
        if chunk.len() > 3 {
            out.push((buf[2] << 6) | buf[3]);
        }
    }
    Ok(out)
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Render a best-effort ISO-8601 UTC timestamp without pulling in `chrono`.
/// Good enough for human-readable build metadata; don't parse it.
fn iso8601_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since 1970-01-01 → civil date (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let rem = secs % 86_400;
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}
