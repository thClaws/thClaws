//! dev-plan/45 A2: per-member attribution for gateway billing.
//!
//! In multiuser `--serve` every member's turn runs under a task-local
//! member id (established next to the workdir scope in
//! `shared_session::drive_turn_stream`). Outbound HTTP that can hit the
//! thClaws gateway attaches it as the `X-Thclaws-Member` header via
//! [`attach_member`], so the gateway can (a) record
//! `usage_events.member_id` and (b) enforce a per-(workspace, member)
//! daily cap — closing the "one guest drains the whole workspace cap"
//! fairness gap.
//!
//! The header is advisory for billing attribution, not an auth channel:
//! the gateway still authenticates the workspace by its gateway key.
//! Outside a member scope (desktop, single-tenant) requests are
//! untouched.

tokio::task_local! {
    static MEMBER_ID: String;
}

/// Wire header name (lowercase; reqwest normalises anyway).
pub const MEMBER_HEADER: &str = "x-thclaws-member";

/// Run `fut` with `member_id` as the task-local member identity.
pub async fn scope_member<F, T>(member_id: String, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    MEMBER_ID.scope(member_id, fut).await
}

pub fn sync_scope_member<F, T>(member_id: String, f: F) -> T
where
    F: FnOnce() -> T,
{
    MEMBER_ID.sync_scope(member_id, f)
}

/// The active member id, when inside a multiuser member scope.
pub fn current_member_id() -> Option<String> {
    MEMBER_ID.try_with(|m| m.clone()).ok()
}

/// Attach the member header to an outbound request when a member scope
/// is active — the single helper every gateway-capable HTTP site calls.
/// Also the turn header (`gateway_turn`), added only when the request goes
/// to the thClaws gateway.
pub fn attach_member(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let rb = match current_member_id() {
        Some(id) => rb.header(MEMBER_HEADER, id),
        None => rb,
    };
    crate::gateway_turn::tag(rb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scoped_member_is_visible_and_isolated() {
        assert!(current_member_id().is_none());
        let got = scope_member("alice".into(), async {
            tokio::task::yield_now().await;
            current_member_id()
        })
        .await;
        assert_eq!(got.as_deref(), Some("alice"));
        let got_b = scope_member("bob".into(), async { current_member_id() }).await;
        assert_eq!(got_b.as_deref(), Some("bob"));
        assert!(current_member_id().is_none());
    }
}
