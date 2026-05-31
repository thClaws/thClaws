//! Per-shell access tokens for Mode B serve-mode auth.
//!
//! When `thclaws --serve --gui-shell <id>` launches, a random 160-bit
//! token gates access at `/t/<token>/`. Tokens are persisted at
//! `~/.config/thclaws/gui-shell-tokens.json` keyed by (shellId, port)
//! so restarting `--serve` produces the same URL — sharing a URL once
//! is meaningful.
//!
//! `thclaws shell rotate-token <id>` (Task 18 CLI follow-up) wipes the
//! persisted token and forces a new one on next launch.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Base32 alphabet (Crockford-style, no ambiguous I/L/O/U). 32 chars
/// → 5 bits per char. 32 chars × 5 bits = 160 bits of entropy.
const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
const TOKEN_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellToken {
    /// The raw base32 string. Treat as a secret — the URL surface is
    /// the only auth.
    pub value: String,
    /// Unix-seconds creation time. Used for TTL expiry checks.
    pub created_at: u64,
    /// Optional TTL in seconds. `None` = never expires. Default 30d
    /// when generated unless `--gui-shell-token-ttl` overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

impl ShellToken {
    /// Whether the token has expired given the current unix time.
    pub fn is_expired(&self, now_secs: u64) -> bool {
        match self.ttl_secs {
            None => false,
            Some(ttl) => now_secs.saturating_sub(self.created_at) > ttl,
        }
    }

    /// Generate a fresh random token. Uses `getrandom` for OS-level
    /// entropy; rejection-sampled against the 32-char alphabet so the
    /// distribution stays uniform (alphabet size divides 256 evenly,
    /// so mod-bias isn't an issue here — kept the simple modulo).
    pub fn generate(ttl_secs: Option<u64>) -> Self {
        let mut bytes = [0u8; TOKEN_LEN];
        // OS RNG. getrandom failing on a desktop / server class machine
        // is catastrophic — fall back to a non-secure but deterministic
        // pad rather than panic mid-launch. Logged loudly so an
        // operator notices the degraded state.
        if let Err(e) = getrandom::getrandom(&mut bytes) {
            eprintln!(
                "[gui-shell] WARNING: getrandom failed ({e}); shell token will be predictable. Rotate immediately via `thclaws shell rotate-token`."
            );
        }
        let value: String = bytes
            .iter()
            .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
            .collect();
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            value,
            created_at,
            ttl_secs,
        }
    }
}

/// Token store on disk. JSON `{"<shell_id>:<port>": ShellToken}`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenFile {
    #[serde(default)]
    tokens: BTreeMap<String, ShellToken>,
}

fn store_path() -> Result<PathBuf> {
    let home = crate::util::home_dir()
        .ok_or_else(|| Error::Config("HOME not set; cannot resolve token store path".into()))?;
    Ok(home
        .join(".config")
        .join("thclaws")
        .join("gui-shell-tokens.json"))
}

fn load_file() -> Result<TokenFile> {
    let path = store_path()?;
    match std::fs::read_to_string(&path) {
        Ok(body) => serde_json::from_str(&body).map_err(|e| {
            Error::Tool(format!(
                "gui-shell-tokens.json corrupt at {}: {e}",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TokenFile::default()),
        Err(e) => Err(Error::Tool(format!(
            "cannot read gui-shell-tokens.json at {}: {e}",
            path.display()
        ))),
    }
}

fn save_file(file: &TokenFile) -> Result<()> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Tool(format!(
                "cannot create token store dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let body = serde_json::to_string_pretty(file)
        .map_err(|e| Error::Tool(format!("serialize gui-shell-tokens.json: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| {
        Error::Tool(format!(
            "write gui-shell-tokens.json temp {}: {e}",
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        Error::Tool(format!(
            "rename gui-shell-tokens.json temp {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

fn token_key(shell_id: &str, port: u16) -> String {
    format!("{shell_id}:{port}")
}

/// Resolve the token for `(shell_id, port)`, generating + persisting
/// a fresh one if none exists or the stored one has expired.
/// `default_ttl_secs` is applied to freshly-generated tokens.
///
/// Returns `(token, was_generated)` — `was_generated` is true when
/// the launcher should print the URL (first run or after expiry).
pub fn resolve_or_generate(
    shell_id: &str,
    port: u16,
    default_ttl_secs: Option<u64>,
) -> Result<(ShellToken, bool)> {
    let mut file = load_file()?;
    let key = token_key(shell_id, port);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(existing) = file.tokens.get(&key) {
        if !existing.is_expired(now) {
            return Ok((existing.clone(), false));
        }
    }
    let fresh = ShellToken::generate(default_ttl_secs);
    file.tokens.insert(key, fresh.clone());
    save_file(&file)?;
    Ok((fresh, true))
}

/// Override the token for `(shell_id, port)` with an explicit value
/// (the `--gui-shell-token` CLI flag path — for reproducible
/// deployments where the URL needs to be stable across redeploys).
pub fn pin(shell_id: &str, port: u16, value: String, ttl_secs: Option<u64>) -> Result<ShellToken> {
    if value.len() < 16 {
        return Err(Error::Tool(format!(
            "--gui-shell-token must be at least 16 chars (~80 bits); got {}",
            value.len()
        )));
    }
    let mut file = load_file()?;
    let key = token_key(shell_id, port);
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let token = ShellToken {
        value,
        created_at,
        ttl_secs,
    };
    file.tokens.insert(key, token.clone());
    save_file(&file)?;
    Ok(token)
}

/// Wipe the token for `(shell_id, port)` so the next launch generates
/// a fresh one. Used by `thclaws shell rotate-token <id>`. Returns
/// true if a token was actually removed.
pub fn rotate(shell_id: &str, port: u16) -> Result<bool> {
    let mut file = load_file()?;
    let removed = file.tokens.remove(&token_key(shell_id, port)).is_some();
    if removed {
        save_file(&file)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_token_of_expected_length() {
        let t = ShellToken::generate(None);
        assert_eq!(t.value.len(), TOKEN_LEN);
        // Only alphabet chars.
        assert!(t.value.bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn generate_with_ttl_marks_expiry() {
        let t = ShellToken {
            value: "x".repeat(32),
            created_at: 0,
            ttl_secs: Some(60),
        };
        assert!(!t.is_expired(30));
        assert!(t.is_expired(120));
    }

    #[test]
    fn no_ttl_never_expires() {
        let t = ShellToken {
            value: "x".repeat(32),
            created_at: 0,
            ttl_secs: None,
        };
        assert!(!t.is_expired(u64::MAX));
    }

    #[test]
    fn pin_rejects_short_tokens() {
        let err = pin("test", 9999, "tooShort".into(), None).unwrap_err();
        assert!(format!("{err}").contains("at least 16 chars"));
    }
}
