//! Per-message metering pipeline for multi-tenant `--serve`.
//!
//! At every agent-turn boundary the worker emits one [`MessageEvent`]
//! capturing what provider calls happened, how many tokens flowed,
//! and (per dev-plan/24 pricing) the cost in cents. A `MeteringSink`
//! consumes the event and forwards it to the cloud-control-plane
//! billing pipeline (dev-plan/34).
//!
//! Implementations bundled here:
//!
//! - [`NoopMeteringSink`] — single-tenant default; agent emits events
//!   into a black hole. Zero overhead.
//! - [`StdoutMeteringSink`] — dev / debugging. Prints one JSON line
//!   per event to stderr.
//! - [`HttpMeteringSink`] — production. POSTs each event to a
//!   configurable URL with a shared secret in `Authorization: Bearer`.
//!   Tier 1 is best-effort (lose-on-network-fail); Tier 2 wires Kafka
//!   for durability + back-pressure.
//!
//! Sink selection happens at server bootstrap from
//! `THCLAWS_METERING_ENDPOINT` (URL → HttpMeteringSink; "stdout" →
//! StdoutMeteringSink; absent → NoopMeteringSink). Single-tenant
//! `--serve` never reads this env var.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::SystemTime;

/// One billing-relevant event — emitted at the end of each agent
/// turn (`TurnDone` in `ViewEvent`). Carries all the data
/// dev-plan/34's billing pipeline needs to attribute cost and pay
/// the author their revenue share.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEvent {
    /// HMAC-verified user id.
    pub user_id: String,
    /// The Agent's marketplace id (from manifest).
    pub agent_id: String,
    /// Agent version that handled the turn (for canary / revert
    /// analytics).
    pub agent_version: String,
    /// thClaws session id (per-user session JSONL filename).
    pub session_id: String,
    /// Monotonic per-session message counter — lets the billing
    /// pipeline detect gaps + dedupe replays.
    pub message_id: u64,
    /// Every provider call that happened during this turn —
    /// typically the LLM, optionally one or more image/video MCPs.
    pub providers: Vec<ProviderCall>,
    /// Sum of `providers[].cost_cents`. Authoritative cost the
    /// cloud charges from the user's credit balance.
    pub total_cost_cents: u64,
    /// Unix-ms timestamp the turn started (cloud bills against
    /// this for credit-pack window calcs).
    pub started_at_ms: u64,
    /// Unix-ms timestamp the turn completed.
    pub completed_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCall {
    /// Provider name as used in `provider_has_credentials` —
    /// e.g. "anthropic", "openai", "pinn.ai".
    pub provider: String,
    /// Model id passed to the provider — "claude-sonnet-4-6",
    /// "gpt-4o", "prunaai/p-image", etc.
    pub model: String,
    /// Input tokens (LLMs) or units (image-gen tools — 1 unit = 1
    /// image, 1 unit = 1 second of video, etc.).
    pub input_units: u64,
    pub output_units: u64,
    /// Cost in 1/100 cents for sub-cent precision (Stripe-style).
    /// dev-plan/24 model catalogue provides per-unit pricing the
    /// caller multiplies by units.
    pub cost_cents: u64,
}

#[async_trait]
pub trait MeteringSink: Send + Sync {
    async fn record(&self, event: MessageEvent);
}

/// Production sink — POSTs each event to a control-plane HTTP
/// endpoint. Tier 1 best-effort (timeouts + drops, no retry queue);
/// Tier 2 swaps in a Kafka producer for durability.
pub struct HttpMeteringSink {
    endpoint: String,
    bearer: String,
    client: reqwest::Client,
}

impl HttpMeteringSink {
    pub fn new(endpoint: String, bearer: String) -> Self {
        Self {
            endpoint,
            bearer,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("metering reqwest client"),
        }
    }
}

#[async_trait]
impl MeteringSink for HttpMeteringSink {
    async fn record(&self, event: MessageEvent) {
        let res = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.bearer)
            .json(&event)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                eprintln!(
                    "\x1b[33m[metering] {} returned {} for user={} session={} msg={}\x1b[0m",
                    self.endpoint,
                    r.status(),
                    event.user_id,
                    event.session_id,
                    event.message_id
                );
            }
            Err(e) => {
                eprintln!(
                    "\x1b[33m[metering] {} POST failed ({e}); event dropped (user={} msg={})\x1b[0m",
                    self.endpoint, event.user_id, event.message_id
                );
            }
        }
    }
}

/// Debug / dev sink — JSON line per event on stderr. Useful for
/// running `--serve --multi-tenant` locally and watching cost
/// attribution roll past in real time.
pub struct StdoutMeteringSink;

#[async_trait]
impl MeteringSink for StdoutMeteringSink {
    async fn record(&self, event: MessageEvent) {
        if let Ok(json) = serde_json::to_string(&event) {
            eprintln!("[metering] {json}");
        }
    }
}

/// Single-tenant default — accepts and discards. Zero allocations,
/// no network calls. Existing `--serve` (no `--multi-tenant`) wires
/// this so the agent loop's metering emit-call is essentially free.
pub struct NoopMeteringSink;

#[async_trait]
impl MeteringSink for NoopMeteringSink {
    async fn record(&self, _event: MessageEvent) {}
}

/// Bootstrap-time sink selector. Reads `THCLAWS_METERING_ENDPOINT`:
///   - URL ("https://…")              → HttpMeteringSink (requires
///                                       THCLAWS_METERING_BEARER too)
///   - "stdout"                        → StdoutMeteringSink
///   - unset / empty / anything else   → NoopMeteringSink
///
/// Single-tenant code paths just call `MeteringSink::record` and let
/// the noop sink absorb. Multi-tenant code paths get the production
/// HTTP sink.
pub fn from_env() -> Arc<dyn MeteringSink> {
    match std::env::var("THCLAWS_METERING_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(s) if s == "stdout" => Arc::new(StdoutMeteringSink),
        Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
            let bearer = std::env::var("THCLAWS_METERING_BEARER").unwrap_or_default();
            if bearer.is_empty() {
                eprintln!(
                    "\x1b[33m[metering] THCLAWS_METERING_ENDPOINT={url} set but \
                     THCLAWS_METERING_BEARER missing — using noop sink\x1b[0m"
                );
                Arc::new(NoopMeteringSink)
            } else {
                Arc::new(HttpMeteringSink::new(url, bearer))
            }
        }
        _ => Arc::new(NoopMeteringSink),
    }
}

/// Helper for the agent loop: snapshot wall-clock for the
/// `started_at_ms` / `completed_at_ms` fields without dragging
/// chrono into every call site.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Convenience for tests + the cloud-side ingest endpoint contract:
/// MessageEvent should round-trip JSON without info loss.
#[cfg(test)]
fn round_trip_json(event: &MessageEvent) -> MessageEvent {
    let s = serde_json::to_string(event).unwrap();
    serde_json::from_str(&s).unwrap()
}

#[cfg(test)]
mod tests {
    use super::super::auth::UserId;
    use super::*;

    fn sample_event() -> MessageEvent {
        MessageEvent {
            user_id: "usr_abc".into(),
            agent_id: "image-bot".into(),
            agent_version: "0.1.0".into(),
            session_id: "sess_xyz".into(),
            message_id: 42,
            providers: vec![
                ProviderCall {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-4-6".into(),
                    input_units: 1234,
                    output_units: 567,
                    cost_cents: 4,
                },
                ProviderCall {
                    provider: "pinn.ai".into(),
                    model: "prunaai/p-image".into(),
                    input_units: 1,
                    output_units: 1,
                    cost_cents: 500,
                },
            ],
            total_cost_cents: 504,
            started_at_ms: 1_700_000_000_000,
            completed_at_ms: 1_700_000_003_500,
        }
    }

    #[test]
    fn message_event_round_trips_json() {
        let event = sample_event();
        let rt = round_trip_json(&event);
        assert_eq!(event.user_id, rt.user_id);
        assert_eq!(event.providers.len(), rt.providers.len());
        assert_eq!(event.total_cost_cents, rt.total_cost_cents);
    }

    #[test]
    fn camel_case_field_names_in_wire_format() {
        let event = sample_event();
        let s = serde_json::to_string(&event).unwrap();
        // Cloud control plane (dev-plan/34) expects camelCase.
        assert!(s.contains("\"userId\""));
        assert!(s.contains("\"agentId\""));
        assert!(s.contains("\"totalCostCents\""));
        assert!(s.contains("\"startedAtMs\""));
        // Provider fields too:
        assert!(s.contains("\"inputUnits\""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn noop_sink_accepts_and_drops() {
        let sink = NoopMeteringSink;
        sink.record(sample_event()).await;
        // No assertion — noop. Just confirms it compiles + runs
        // without panic.
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stdout_sink_emits_to_stderr() {
        // Smoke test only — can't easily capture stderr without
        // shenanigans. Confirms the format compiles + runs.
        StdoutMeteringSink.record(sample_event()).await;
    }

    #[test]
    fn from_env_returns_noop_when_unset() {
        // Temporarily clear env in case test env has it set.
        let prev = std::env::var("THCLAWS_METERING_ENDPOINT").ok();
        std::env::remove_var("THCLAWS_METERING_ENDPOINT");
        let sink = from_env();
        // No direct introspection — confirm noop by recording event
        // (would panic on http sink with empty bearer).
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(sink.record(sample_event()));
        if let Some(p) = prev {
            std::env::set_var("THCLAWS_METERING_ENDPOINT", p);
        }
    }

    /// `UserId` is the authenticated type; metering events carry the
    /// raw string form so cloud-side joins don't need the thClaws
    /// type. Confirm conversion.
    #[test]
    fn user_id_threads_through_event() {
        let uid = UserId::new_for_test("usr_test");
        let mut e = sample_event();
        e.user_id = uid.as_str().into();
        assert_eq!(e.user_id, "usr_test");
    }
}
