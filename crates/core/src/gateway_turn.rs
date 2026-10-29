//! The agent turn a gateway request belongs to (`X-Thclaws-Turn`).
//!
//! An install in quota mode refuses NEW turns once a user's credits run out
//! but lets a turn it already admitted finish, so the gateway has to know
//! which requests are the same turn: the first LLM call and every tool,
//! media and subagent call that follows it until the turn ends.
//!
//! One id per top-level turn, process-wide: `TurnGuard::begin` opens it at
//! the start of `Agent::run_turn*` and nested turns (a `Task` subagent inside
//! the parent's turn) reuse it. The header goes only to the thClaws gateway,
//! never to a third-party endpoint; outside a turn each request gets a fresh
//! id.
//!
//! `X-Thclaws-Session` names the chat session the call belongs to, so the
//! gateway can total a session the way the chat footer does. A multiuser
//! worker runs several sessions at once and scopes each turn
//! ([`scope_session`]); everywhere else the surface sets the process-wide
//! one before a turn ([`set_session`]).

use std::sync::Mutex;

pub const TURN_HEADER: &str = "X-Thclaws-Turn";
pub const SESSION_HEADER: &str = "X-Thclaws-Session";

tokio::task_local! {
    static SESSION: String;
}

static LAST_SESSION: Mutex<Option<String>> = Mutex::new(None);

pub fn set_session(id: &str) {
    *LAST_SESSION.lock().unwrap_or_else(|p| p.into_inner()) = Some(id.to_string());
}

pub async fn scope_session<F: std::future::Future>(id: String, fut: F) -> F::Output {
    SESSION.scope(id, fut).await
}

/// The session the current call belongs to. A multiuser worker never falls
/// back to the process-wide id: that one may be another user's session.
pub fn current_session() -> Option<String> {
    SESSION.try_with(|s| s.clone()).ok().or_else(|| {
        if crate::workdir::is_multiuser() {
            None
        } else {
            LAST_SESSION
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    })
}

struct Active {
    id: Option<String>,
    depth: usize,
}

static ACTIVE: Mutex<Active> = Mutex::new(Active { id: None, depth: 0 });

/// Held for the life of a turn; the id is cleared when the outermost one drops.
pub struct TurnGuard(());

impl TurnGuard {
    pub fn begin() -> Self {
        let mut a = ACTIVE.lock().unwrap_or_else(|p| p.into_inner());
        if a.depth == 0 {
            a.id = Some(uuid::Uuid::new_v4().to_string());
        }
        a.depth += 1;
        TurnGuard(())
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let mut a = ACTIVE.lock().unwrap_or_else(|p| p.into_inner());
        a.depth = a.depth.saturating_sub(1);
        if a.depth == 0 {
            a.id = None;
        }
    }
}

/// The open turn's id, if any.
pub fn current() -> Option<String> {
    ACTIVE.lock().unwrap_or_else(|p| p.into_inner()).id.clone()
}

/// Whether `url` points at the thClaws gateway this install uses.
pub(crate) fn is_gateway_url(url: &reqwest::Url) -> bool {
    let base = crate::providers::thclaws_gateway::resolve_base_url();
    same_origin(url, &base)
}

fn same_origin(url: &reqwest::Url, base: &str) -> bool {
    let Ok(base) = reqwest::Url::parse(base.trim()) else {
        return false;
    };
    url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
}

/// Add `X-Thclaws-Turn`, `X-Thclaws-Session` and `X-Thclaws-Client` when the
/// request goes to the thClaws gateway.
/// Called from `multi_tenant::attach_member`, the helper every gateway-capable
/// HTTP site already uses.
pub fn tag(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let (client, built) = rb.build_split();
    match built {
        Ok(mut req) => {
            if is_gateway_url(req.url()) {
                let id = current().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                if let Ok(v) = reqwest::header::HeaderValue::from_str(&id) {
                    req.headers_mut().insert(TURN_HEADER, v);
                }
                if let Some(v) =
                    current_session().and_then(|s| reqwest::header::HeaderValue::from_str(&s).ok())
                {
                    req.headers_mut().insert(SESSION_HEADER, v);
                }
                crate::desktop_update::insert_client_header(req.headers_mut());
            }
            reqwest::RequestBuilder::from_parts(client, req)
        }
        // A builder that already failed (bad header, bad URL) stays failed:
        // an unparseable URL errors the same way on send.
        Err(e) => client.get(format!("thclaws-invalid-request:{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test owns the process-wide turn state, so steps don't race.
    #[test]
    fn a_turn_id_spans_nested_turns_and_ends_with_the_outer_one() {
        assert!(current().is_none());
        let outer = TurnGuard::begin();
        let id = current().expect("open");
        {
            let _sub = TurnGuard::begin();
            assert_eq!(
                current().as_deref(),
                Some(id.as_str()),
                "a subagent reuses it"
            );
        }
        assert_eq!(
            current().as_deref(),
            Some(id.as_str()),
            "still the parent's turn"
        );
        drop(outer);
        assert!(current().is_none());
        let _next = TurnGuard::begin();
        assert_ne!(
            current().as_deref(),
            Some(id.as_str()),
            "a new turn gets a new id"
        );
    }

    #[test]
    fn only_the_gateway_origin_matches() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        let gw = "https://gateway.sis.thclaws.ai";
        assert!(same_origin(
            &u("https://gateway.sis.thclaws.ai/sis/chat/completions"),
            gw
        ));
        assert!(same_origin(
            &u("https://gateway.sis.thclaws.ai:443/hal/x"),
            gw
        ));
        assert!(!same_origin(
            &u("https://dashscope-intl.aliyuncs.com/api/v1/x"),
            gw
        ));
        assert!(!same_origin(&u("http://gateway.sis.thclaws.ai/x"), gw));
        assert!(!same_origin(
            &u("https://gateway.sis.thclaws.ai.evil.com/x"),
            gw
        ));
        assert!(same_origin(
            &u("http://gateway.thclaws-cloud.svc:8080/sis/x"),
            "http://gateway.thclaws-cloud.svc:8080"
        ));
    }

    #[test]
    fn the_header_rides_only_to_the_gateway() {
        let _env = crate::kms::test_env_lock();
        std::env::remove_var("THCLAWS_GATEWAY_BASE_URL");
        let client = reqwest::Client::new();
        let gw = crate::providers::thclaws_gateway::resolve_base_url();
        let to_gw = tag(client.post(format!("{gw}/openai/v1/chat/completions")));
        let native = tag(client.post("https://api.openai.com/v1/chat/completions"));
        assert!(to_gw.build().unwrap().headers().contains_key(TURN_HEADER));
        assert!(!native.build().unwrap().headers().contains_key(TURN_HEADER));
        let client_hdr = crate::desktop_update::CLIENT_HEADER;
        let to_gw = tag(client.post(format!("{gw}/openai/v1/chat/completions")));
        let native = tag(client.post("https://api.openai.com/v1/chat/completions"));
        assert_eq!(
            to_gw.build().unwrap().headers()[client_hdr]
                .to_str()
                .unwrap(),
            crate::desktop_update::client_value()
        );
        assert!(!native.build().unwrap().headers().contains_key(client_hdr));
        // A builder that already failed stays failed.
        assert!(tag(client.get("not a url")).build().is_err());
    }

    #[tokio::test]
    async fn a_scoped_session_wins_over_the_process_wide_one() {
        set_session("sess-process");
        assert_eq!(current_session().as_deref(), Some("sess-process"));
        let inner = scope_session("sess-scoped".into(), async { current_session() }).await;
        assert_eq!(inner.as_deref(), Some("sess-scoped"));
        assert_eq!(current_session().as_deref(), Some("sess-process"));
    }
}
