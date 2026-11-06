use super::adapter::MockThbAdapter;
use super::x402::{self, Authorization};
use super::*;
use crate::permissions::{AutoApprover, ScriptedApprover};
use async_trait::async_trait;
use base64::Engine as _;
use k256::ecdsa::SigningKey;
use serde_json::json;

fn test_key() -> SigningKey {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).unwrap();
    SigningKey::from_slice(&b).unwrap()
}

const USDC_SEPOLIA: &str = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";

#[test]
fn units_round_trip() {
    assert_eq!(parse_units("12.5", 6), Some(12_500_000));
    assert_eq!(parse_units("0.01", 2), Some(1));
    assert_eq!(parse_units("3", 0), Some(3));
    assert_eq!(parse_units("1.234", 2), None);
    assert_eq!(parse_units("-1", 2), None);
    assert_eq!(parse_units("abc", 2), None);
    assert_eq!(format_units(12_500_000, 6), "12.50");
    assert_eq!(format_units(8_500_000, 6), "8.50");
    assert_eq!(format_units(1_234_567, 6), "1.234567");
}

#[test]
fn only_testnets_are_allowed() {
    assert!(network_allowed("base-sepolia"));
    assert!(network_allowed("eip155:84532"));
    assert!(!network_allowed("base"));
    assert!(!network_allowed("eip155:8453"));
    assert!(!network_allowed("unknown"));
    assert_eq!(x402::chain_id("eip155:8453"), None);
}

#[test]
fn eip712_signature_recovers_the_signer() {
    let key = test_key();
    let from = x402::address_of(key.verifying_key());
    let a = Authorization {
        from: from.clone(),
        to: "0x1111111111111111111111111111111111111111".into(),
        value: 2_500_000,
        valid_after: 0,
        valid_before: 9_999_999_999,
        nonce: [7u8; 32],
    };
    let d = x402::transfer_digest("USDC", "2", 84532, USDC_SEPOLIA, &a).unwrap();
    let sig = x402::sign_digest(&key, &d).unwrap();
    assert_eq!(x402::from_hex(&sig).unwrap().len(), 65);
    assert_eq!(x402::recover(&d, &sig).as_deref(), Some(from.as_str()));
    let other = x402::transfer_digest("USDC", "2", 8453, USDC_SEPOLIA, &a).unwrap();
    assert_ne!(x402::recover(&other, &sig).as_deref(), Some(from.as_str()));
    assert_eq!(Authorization::from_json(&a.to_json()), Some(a));
}

#[test]
fn eip712_type_hash_matches_eip3009() {
    // keccak256("TransferWithAuthorization(...)") as published in EIP-3009.
    let h = x402::keccak(
        b"TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
    );
    assert_eq!(
        x402::to_hex(&h),
        "0x7c7c6cdb67a18743f49ec6fa9b35f50d52ed05cbed4cc592e13b44501c1a2267"
    );
}

#[test]
fn cli_answers() {
    let q = Quote {
        adapter: "x".into(),
        amount: Some(1),
        decimals: 6,
        asset: "USDC".into(),
        asset_id: "u".into(),
        payee: "p".into(),
        payee_name: None,
        network: "base-sepolia".into(),
        resource: None,
        description: None,
    };
    assert_eq!(parse_cli_answer("o\n", &q), PaymentDecision::ApproveOnce);
    assert_eq!(parse_cli_answer("", &q), PaymentDecision::Deny);
    assert_eq!(parse_cli_answer("b", &q), PaymentDecision::Deny);
    assert_eq!(
        parse_cli_answer("b 35 10", &q),
        PaymentDecision::Budget(BudgetGrant {
            total: 35_000_000,
            per_tx: Some(10_000_000),
            payees: None,
            expires: None
        })
    );
}

// ── the trip acceptance test, run on two rails ──────────────────────────

/// One rail as the trip test sees it: book an item by name, through the core.
#[async_trait]
trait Rail: Send + Sync {
    fn asset(&self) -> &'static str;
    async fn book(&self, ctx: &PaymentContext, item: &str) -> Result<Paid, String>;
    fn receipts(&self) -> usize;
}

const ITEMS: &[(&str, &str, &str)] = &[
    ("hotel", "12.00", "Hotel Mock"),
    ("air", "9.50", "Airline Mock"),
    ("transfer", "2.50", "Transfer Mock"),
    ("transfer2", "2.50", "Transfer Mock"),
    ("pricey", "12.00", "Hotel Mock"),
];

fn price(item: &str) -> &'static str {
    ITEMS.iter().find(|i| i.0 == item).unwrap().1
}

fn payee_id(item: &str) -> String {
    let name = ITEMS.iter().find(|i| i.0 == item).unwrap().2;
    match name {
        "Hotel Mock" => "0x1000000000000000000000000000000000000001".into(),
        "Airline Mock" => "0x2000000000000000000000000000000000000002".into(),
        _ => "0x3000000000000000000000000000000000000003".into(),
    }
}

struct ThbRail(MockThbAdapter);

impl ThbRail {
    fn new() -> Self {
        Self(MockThbAdapter::new(&[
            ("0x1000000000000000000000000000000000000001", "Hotel Mock"),
            ("0x2000000000000000000000000000000000000002", "Airline Mock"),
            (
                "0x3000000000000000000000000000000000000003",
                "Transfer Mock",
            ),
        ]))
    }
}

#[async_trait]
impl Rail for ThbRail {
    fn asset(&self) -> &'static str {
        "THB"
    }
    async fn book(&self, ctx: &PaymentContext, item: &str) -> Result<Paid, String> {
        let req = PaymentRequest {
            resource: Some(format!("mock://{item}")),
            challenge: json!({"payee": payee_id(item), "amount": price(item), "description": item}),
            hint: Value::Null,
        };
        pay_in(ctx, &self.0, &req).await
    }
    fn receipts(&self) -> usize {
        self.0.paid.lock().unwrap().len()
    }
}

/// Mock x402 sellers on 127.0.0.1: a real 402 challenge, and a real
/// EIP-712 check of the payment before the 200 — offline, no chain.
struct X402Rail {
    base: String,
    key: SigningKey,
    settled: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn seller(
    axum::extract::Path(item): axum::extract::Path<String>,
    axum::extract::State(st): axum::extract::State<(String, Arc<std::sync::Mutex<Vec<String>>>)>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (network, settled) = st;
    let network = if item == "mainnet" {
        "base".to_string()
    } else {
        network
    };
    let item_key = if item == "mainnet" {
        "hotel".to_string()
    } else {
        item.clone()
    };
    let amount = parse_units(price(&item_key), 6).unwrap();
    let pay_to = payee_id(&item_key);
    let challenge = json!({
        "x402Version": 1,
        "error": "payment required",
        "accepts": [{
            "scheme": "exact", "network": network,
            "maxAmountRequired": amount.to_string(),
            "resource": format!("/{item}/book"), "description": format!("book {item}"),
            "mimeType": "application/json", "payTo": pay_to, "maxTimeoutSeconds": 60,
            "asset": USDC_SEPOLIA,
            "extra": {"name": "USDC", "version": "2"}
        }]
    });
    let reject = || {
        (
            axum::http::StatusCode::PAYMENT_REQUIRED,
            axum::Json(challenge.clone()),
        )
            .into_response()
    };
    let Some(h) = headers.get("X-PAYMENT").and_then(|v| v.to_str().ok()) else {
        return reject();
    };
    let Some(doc) = base64::engine::general_purpose::STANDARD
        .decode(h)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return reject();
    };
    let Some(a) = Authorization::from_json(&doc["payload"]["authorization"]) else {
        return reject();
    };
    let sig = doc["payload"]["signature"].as_str().unwrap_or_default();
    let d = x402::transfer_digest("USDC", "2", 84532, USDC_SEPOLIA, &a).unwrap();
    let ok = x402::recover(&d, sig).as_deref() == Some(a.from.as_str())
        && a.to.eq_ignore_ascii_case(&pay_to)
        && a.value >= amount;
    if !ok {
        return reject();
    }
    let tx = format!("0xmock{}", settled.lock().unwrap().len());
    settled.lock().unwrap().push(tx.clone());
    let settle = base64::engine::general_purpose::STANDARD
        .encode(json!({"success": true, "transaction": tx, "network": "base-sepolia"}).to_string());
    (
        [("X-PAYMENT-RESPONSE", settle)],
        axum::Json(json!({"booked": item})),
    )
        .into_response()
}

impl X402Rail {
    async fn start() -> Self {
        let settled = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/{item}/book", axum::routing::any(seller))
            .with_state(("base-sepolia".to_string(), settled.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base: format!("http://{addr}"),
            key: test_key(),
            settled,
        }
    }
}

#[async_trait]
impl Rail for X402Rail {
    fn asset(&self) -> &'static str {
        "USDC"
    }
    async fn book(&self, ctx: &PaymentContext, item: &str) -> Result<Paid, String> {
        let url = format!("{}/{item}/book", self.base);
        let client = reqwest::Client::new();
        let first = client.post(&url).send().await.map_err(|e| e.to_string())?;
        assert_eq!(first.status().as_u16(), 402);
        let (version, requirement) = x402::parse_challenge(None, &first.text().await.unwrap())?;
        let mut adapter = x402::X402Adapter::new(
            self.key.clone(),
            client,
            reqwest::Method::POST,
            &url,
            Some("{}".into()),
            Some("application/json".into()),
        );
        adapter.set_requirement(version, &requirement);
        let req = PaymentRequest {
            resource: Some(url),
            challenge: requirement,
            hint: Value::Null,
        };
        let paid = pay_in(ctx, &adapter, &req).await?;
        assert!(adapter.response.lock().unwrap().is_some());
        Ok(paid)
    }
    fn receipts(&self) -> usize {
        self.settled.lock().unwrap().len()
    }
}

fn ctx(
    approver: Arc<dyn ApprovalSink>,
    dir: &tempfile::TempDir,
    mandates: Vec<Mandate>,
) -> PaymentContext {
    PaymentContext {
        approver,
        originator: AgentOrigin::Main,
        tool_name: "PaidFetch".into(),
        ledger_path: dir.path().join("ledger.jsonl"),
        mandates: Some(mandates),
        governance: Some(Governance::Open {
            allowed_sources: None,
            agents: None,
            ignore_config_mandates: false,
        }),
    }
}

fn units(rail: &dyn Rail, s: &str) -> u128 {
    parse_units(s, if rail.asset() == "THB" { 2 } else { 6 }).unwrap()
}

const TRIP: &[&str] = &["hotel", "air", "transfer", "transfer2"];

async fn trip_interactive(rail: &dyn Rail) {
    let dir = tempfile::tempdir().unwrap();
    let budget = BudgetGrant {
        total: units(rail, "35"),
        per_tx: None,
        payees: None,
        expires: None,
    };
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::Budget(budget)]);
    let c = ctx(approver.clone(), &dir, vec![]);
    let mut last = None;
    for item in TRIP {
        last = Some(rail.book(&c, item).await.expect(item));
    }
    assert_eq!(
        approver.payment_asks.lock().unwrap().len(),
        1,
        "one prompt for the whole trip"
    );
    assert_eq!(rail.receipts(), 4);
    let last = last.unwrap();
    assert_eq!(last.budget_left, Some(units(rail, "8.50")));
    assert!(last.line().contains("budget left 8.50"), "{}", last.line());

    // The 5th item overflows: exactly one re-ask, denied, nothing paid.
    let err = rail.book(&c, "pricey").await.unwrap_err();
    assert!(err.contains("payment denied"), "{err}");
    let asks = approver.payment_asks.lock().unwrap();
    assert_eq!(asks.len(), 2);
    assert_eq!(
        asks[1].reason,
        AskReason::OverBudget {
            left: units(rail, "8.50")
        }
    );
    assert_eq!(rail.receipts(), 4);

    let ledger = read_ledger(&c.ledger_path);
    assert_eq!(ledger.iter().filter(|e| e.receipt.is_some()).count(), 4);
    assert_eq!(ledger.last().unwrap().decision, Decided::Denied);
}

async fn trip_headless_no_mandate(rail: &dyn Rail) {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(Arc::new(AutoApprover), &dir, vec![]);
    let err = rail.book(&c, "hotel").await.unwrap_err();
    assert!(
        err.contains("payment denied: no mandate covers payee"),
        "{err}"
    );
    assert_eq!(rail.receipts(), 0);
}

async fn trip_headless_with_mandate(rail: &dyn Rail) {
    let dir = tempfile::tempdir().unwrap();
    let m = MandateConfig {
        id: "trip-30".into(),
        asset: rail.asset().into(),
        total: Some("30".into()),
        ..Default::default()
    }
    .to_mandate(MandateSource::Config)
    .unwrap();
    let c = ctx(Arc::new(AutoApprover), &dir, vec![m]);
    for item in TRIP {
        rail.book(&c, item).await.expect(item);
    }
    let err = rail.book(&c, "pricey").await.unwrap_err();
    assert!(err.contains("over the remaining budget"), "{err}");
    assert_eq!(rail.receipts(), 4);
}

#[tokio::test]
async fn trip_on_mock_thb() {
    trip_interactive(&ThbRail::new()).await;
    trip_headless_no_mandate(&ThbRail::new()).await;
    trip_headless_with_mandate(&ThbRail::new()).await;
}

#[tokio::test]
async fn trip_on_x402_testnet_offline() {
    trip_interactive(&X402Rail::start().await).await;
    trip_headless_no_mandate(&X402Rail::start().await).await;
    trip_headless_with_mandate(&X402Rail::start().await).await;
}

#[tokio::test]
async fn kyc_rail_refuses_a_payee_outside_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let rail = ThbRail::new();
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce]);
    let c = ctx(approver.clone(), &dir, vec![]);
    let req = PaymentRequest {
        resource: None,
        challenge: json!({"payee": "0xdead", "amount": "1.00"}),
        hint: Value::Null,
    };
    let err = pay_in(&c, &rail.0, &req).await.unwrap_err();
    assert!(err.contains("verified directory"), "{err}");
    assert!(approver.payment_asks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mainnet_challenge_is_refused_even_with_a_mandate() {
    let rail = X402Rail::start().await;
    let body = reqwest::Client::new()
        .post(format!("{}/mainnet/book", rail.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let err = x402::parse_challenge(None, &body).unwrap_err();
    assert!(err.contains("Base Sepolia only"), "{err}");

    // And the core refuses a non-testnet quote outright, before any mandate.
    let dir = tempfile::tempdir().unwrap();
    let m = MandateConfig {
        id: "big".into(),
        asset: "USDC".into(),
        total: Some("1000".into()),
        ..Default::default()
    }
    .to_mandate(MandateSource::Config)
    .unwrap();
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce]);
    let c = ctx(approver.clone(), &dir, vec![m]);
    let mut q = external_quote(
        "t",
        &json!({"amount": "1", "payee": "0xabc", "network": "base"}),
    );
    let err = authorize_external(&c, &q).await.unwrap_err();
    assert!(err.contains("testnets only"), "{err}");
    assert!(approver.payment_asks.lock().unwrap().is_empty());
    q.network = "base-sepolia".into();
    assert!(authorize_external(&c, &q).await.is_ok());
}

#[tokio::test]
async fn unknown_amount_asks_interactively_and_is_denied_headless() {
    let dir = tempfile::tempdir().unwrap();
    let m = MandateConfig {
        id: "m".into(),
        asset: "USDC".into(),
        total: Some("100".into()),
        ..Default::default()
    }
    .to_mandate(MandateSource::Config)
    .unwrap();
    let q = external_quote(
        "mcp__shop__checkout",
        &json!({"payee": "0xabc", "network": "base-sepolia"}),
    );
    assert_eq!(q.amount, None);

    let headless = ctx(Arc::new(AutoApprover), &dir, vec![m.clone()]);
    let err = authorize_external(&headless, &q).await.unwrap_err();
    assert!(err.contains("the amount is unknown"), "{err}");

    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce]);
    let c = ctx(approver.clone(), &dir, vec![m]);
    let auth = authorize_external(&c, &q).await.unwrap();
    assert_eq!(auth.decision, Decided::ApprovedOnce);
    assert_eq!(
        approver.payment_asks.lock().unwrap()[0].reason,
        AskReason::UnknownAmount
    );
}

#[tokio::test]
async fn paid_fetch_pays_through_the_gate() {
    let rail = X402Rail::start().await;
    let dir = tempfile::tempdir().unwrap();
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce]);
    let c = ctx(approver.clone(), &dir, vec![]);
    let url = format!("{}/air/book", rail.base);
    let out = scope(
        c.clone(),
        crate::tools::paid_fetch::fetch(
            &reqwest::Client::new(),
            reqwest::Method::POST,
            &url,
            Some("{}".into()),
            Some("application/json".into()),
            Some("flight BKK→CNX"),
            Some(rail.key.clone()),
        ),
    )
    .await
    .unwrap();
    assert!(out.starts_with("💳 paid 9.50 USDC"), "{out}");
    assert!(
        out.contains("(testnet)") && out.contains("receipt 0xmock0"),
        "{out}"
    );
    assert!(out.contains("\"booked\""), "{out}");

    // Outside an agent turn there is nobody to authorise it.
    let err = crate::tools::paid_fetch::fetch(
        &reqwest::Client::new(),
        reqwest::Method::POST,
        &url,
        None,
        None,
        None,
        Some(rail.key.clone()),
    )
    .await
    .unwrap_err();
    assert!(err.contains("no agent turn"), "{err}");
    assert_eq!(rail.receipts(), 1);
}

/// Real Base Sepolia payment through the real facilitator. Needs a funded
/// testnet wallet and an x402 seller:
/// `THCLAWS_X402_LIVE_URL=http://host:4031/api/thai-word cargo test --features gui --lib live_base_sepolia -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn live_base_sepolia() {
    let url = std::env::var("THCLAWS_X402_LIVE_URL").expect("THCLAWS_X402_LIVE_URL");
    let dir = tempfile::tempdir().unwrap();
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce]);
    let c = ctx(approver.clone(), &dir, vec![]);
    let out = scope(
        c,
        crate::tools::paid_fetch::fetch(
            &reqwest::Client::new(),
            reqwest::Method::GET,
            &url,
            None,
            None,
            Some("live testnet check"),
            None,
        ),
    )
    .await
    .unwrap();
    println!("{out}");
    assert!(out.starts_with("💳 paid"), "{out}");
    assert!(out.contains("HTTP 200"), "{out}");
}

#[test]
fn ledger_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.jsonl");
    let q = external_quote(
        "t",
        &json!({"amount": "5", "payee": "p", "network": "base-sepolia"}),
    );
    let e = LedgerEntry {
        ts: Utc::now(),
        tool: "t".into(),
        agent: None,
        quote: q.clone(),
        mandate_id: Some("m".into()),
        decision: Decided::Mandate,
        receipt: Some(Receipt {
            id: "r".into(),
            network: q.network.clone(),
            amount: q.amount,
            payee: q.payee.clone(),
            raw: Value::Null,
            fee: None,
        }),
        error: None,
    };
    append_ledger(&path, &e);
    let m = MandateConfig {
        id: "m".into(),
        asset: "USDC".into(),
        total: Some("8".into()),
        ..Default::default()
    }
    .to_mandate(MandateSource::Config)
    .unwrap();
    let entries = read_ledger(&path);
    assert_eq!(
        covers(&m, &q, &entries, Utc::now()),
        Err(AskReason::OverBudget { left: 3_000_000 })
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn gas_our_wallet_paid_counts_against_caps() {
    let q = external_quote(
        "t",
        &json!({"amount": "5", "payee": "p", "network": "base-sepolia"}),
    );
    let entry = |fee: Option<Fee>| LedgerEntry {
        ts: Utc::now(),
        tool: "t".into(),
        agent: None,
        quote: q.clone(),
        mandate_id: Some("m".into()),
        decision: Decided::Mandate,
        receipt: Some(Receipt {
            id: "r".into(),
            network: q.network.clone(),
            amount: q.amount,
            payee: q.payee.clone(),
            raw: Value::Null,
            fee,
        }),
        error: None,
    };
    let gas = |asset_id: &str, paid_by: Option<&str>| Fee {
        amount: 1_000_000,
        decimals: 6,
        asset: "X".into(),
        asset_id: asset_id.into(),
        paid_by: paid_by.map(str::to_string),
    };
    let m = MandateConfig {
        id: "m".into(),
        asset: "USDC".into(),
        total: Some("8".into()),
        ..Default::default()
    }
    .to_mandate(MandateSource::Config)
    .unwrap();
    let left = |fee| covers(&m, &q, &[entry(fee)], Utc::now());
    let ours = gas(&q.asset_id.to_uppercase(), None);
    assert_eq!(
        left(Some(ours.clone())),
        Err(AskReason::OverBudget { left: 2_000_000 })
    );
    assert_eq!(
        left(Some(gas("other-token", None))),
        Err(AskReason::OverBudget { left: 3_000_000 })
    );
    assert_eq!(
        left(Some(gas(&q.asset_id, Some("facilitator")))),
        Err(AskReason::OverBudget { left: 3_000_000 })
    );

    let line = serde_json::to_string(&entry(Some(ours))).unwrap();
    assert_eq!(
        serde_json::from_str::<LedgerEntry>(&line)
            .unwrap()
            .receipt
            .unwrap()
            .fee
            .unwrap()
            .amount,
        1_000_000
    );
    let mut old: Value = serde_json::from_str(&line).unwrap();
    old["receipt"].as_object_mut().unwrap().remove("fee");
    assert!(serde_json::from_value::<LedgerEntry>(old)
        .unwrap()
        .receipt
        .unwrap()
        .fee
        .is_none());
}

fn thb() -> MockThbAdapter {
    MockThbAdapter::new(&[("0xpub", "News Publisher")])
}

fn read(resource: &str, amount: &str) -> PaymentRequest {
    PaymentRequest {
        resource: Some(resource.into()),
        challenge: json!({"payee": "0xpub", "amount": amount, "description": "article"}),
        hint: Value::Null,
    }
}

fn governed(
    approver: Arc<dyn ApprovalSink>,
    dir: &tempfile::TempDir,
    mandates: Vec<Mandate>,
    gov: Governance,
) -> PaymentContext {
    let mut c = ctx(approver, dir, mandates);
    c.governance = Some(gov);
    c
}

fn denied_in_ledger(c: &PaymentContext) -> usize {
    read_ledger(&c.ledger_path)
        .iter()
        .filter(|e| e.decision == Decided::Denied)
        .count()
}

#[test]
fn sources_match_hosts_and_prefixes() {
    let list = vec![
        "news.example".to_string(),
        "*.api.example".into(),
        "https://data.example/v1/".into(),
    ];
    assert!(source_allowed(&list, Some("https://news.example/a/1")));
    assert!(source_allowed(&list, Some("https://NEWS.example/a")));
    assert!(source_allowed(&list, Some("https://eu.api.example/q")));
    assert!(source_allowed(&list, Some("https://data.example/v1/rows")));
    assert!(!source_allowed(&list, Some("https://data.example/v2/rows")));
    assert!(!source_allowed(
        &list,
        Some("https://evil.example/news.example")
    ));
    assert!(!source_allowed(&list, None));
    assert!(!source_allowed(&[], Some("https://news.example/")));
}

#[tokio::test]
async fn off_list_source_is_refused_before_any_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let rail = thb();
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce]);
    let c = governed(
        approver.clone(),
        &dir,
        vec![],
        Governance::Open {
            allowed_sources: Some(vec!["news.example".into()]),
            agents: None,
            ignore_config_mandates: false,
        },
    );
    let err = pay_in(&c, &rail, &read("https://blocked.example/x", "0.50"))
        .await
        .unwrap_err();
    assert!(
        err.contains("blocked.example") && err.contains("allowed sources"),
        "{err}"
    );
    assert!(approver.payment_asks.lock().unwrap().is_empty());
    assert_eq!(denied_in_ledger(&c), 1);

    let ok = pay_in(&c, &rail, &read("https://news.example/a/1", "0.50")).await;
    assert!(ok.is_ok(), "{ok:?}");
    assert_eq!(approver.payment_asks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn monthly_cap_and_sources_bound_a_mandate() {
    let dir = tempfile::tempdir().unwrap();
    let rail = thb();
    let m = MandateConfig {
        id: "reads".into(),
        asset: "THB".into(),
        monthly: Some("3.00".into()),
        sources: Some(vec!["news.example".into()]),
        ..Default::default()
    }
    .to_mandate(MandateSource::Config)
    .unwrap();
    let c = ctx(Arc::new(AutoApprover), &dir, vec![m]);
    let p = pay_in(&c, &rail, &read("https://news.example/1", "2.00"))
        .await
        .unwrap();
    assert!(p.line().contains("budget left 1.00 THB"), "{}", p.line());
    let over = pay_in(&c, &rail, &read("https://news.example/2", "2.00")).await;
    assert!(over.unwrap_err().contains("over the remaining budget"));
    let elsewhere = pay_in(&c, &rail, &read("https://other.example/1", "0.10")).await;
    assert!(elsewhere.unwrap_err().contains("payment denied"));
    assert_eq!(rail.paid.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ee_policy_pays_inside_it_and_refuses_outside_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let rail = thb();
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce; 8]);
    let ee = EePayments {
        asset: "THB".into(),
        allowed_sources: vec!["news.example".into()],
        max_per_request: Some("1.00".into()),
        monthly_budget: Some("2.50".into()),
        agents: vec![],
    };
    let c = governed(approver.clone(), &dir, vec![], Governance::Ee(ee.clone()));
    for i in 0..2 {
        pay_in(
            &c,
            &rail,
            &read(&format!("https://news.example/{i}"), "1.00"),
        )
        .await
        .unwrap();
    }
    let monthly = pay_in(&c, &rail, &read("https://news.example/3", "1.00")).await;
    assert!(monthly.unwrap_err().contains("company policy"));
    let ceiling = pay_in(&c, &rail, &read("https://news.example/4", "0.40")).await;
    assert!(ceiling.is_ok(), "{ceiling:?}");
    let big = pay_in(&c, &rail, &read("https://news.example/5", "1.50")).await;
    assert!(big.unwrap_err().contains("per-payment cap"));
    let off = pay_in(&c, &rail, &read("https://shop.example/cart", "0.10")).await;
    assert!(off.unwrap_err().contains("approved sources"));
    assert!(approver.payment_asks.lock().unwrap().is_empty());
    assert_eq!(rail.paid.lock().unwrap().len(), 3);

    let only_research = governed(
        approver.clone(),
        &dir,
        vec![],
        Governance::Ee(EePayments {
            agents: vec!["researcher".into()],
            ..ee
        }),
    );
    let main = pay_in(
        &only_research,
        &rail,
        &read("https://news.example/6", "0.10"),
    )
    .await;
    assert!(main.unwrap_err().contains("agent main may not pay"));

    let none = governed(approver.clone(), &dir, vec![], Governance::Refused);
    let refused = pay_in(&none, &rail, &read("https://news.example/7", "0.10")).await;
    assert!(refused
        .unwrap_err()
        .contains("does not allow agent payments"));
    assert!(approver.payment_asks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn multiuser_ignores_config_mandates_and_agents_list_applies() {
    let dir = tempfile::tempdir().unwrap();
    let rail = thb();
    let m = MandateConfig {
        id: "shared".into(),
        asset: "THB".into(),
        total: Some("100".into()),
        ..Default::default()
    }
    .to_mandate(MandateSource::Config)
    .unwrap();
    let c = governed(
        Arc::new(AutoApprover),
        &dir,
        vec![m.clone()],
        Governance::Open {
            allowed_sources: None,
            agents: None,
            ignore_config_mandates: true,
        },
    );
    let err = pay_in(&c, &rail, &read("https://news.example/1", "1.00"))
        .await
        .unwrap_err();
    assert!(err.contains("no mandate covers"), "{err}");

    let gated = governed(
        Arc::new(AutoApprover),
        &dir,
        vec![m],
        Governance::Open {
            allowed_sources: None,
            agents: Some(vec!["researcher".into()]),
            ignore_config_mandates: false,
        },
    );
    let err = pay_in(&gated, &rail, &read("https://news.example/2", "1.00"))
        .await
        .unwrap_err();
    assert!(err.contains("paymentAgents"), "{err}");
    let entry = read_ledger(&gated.ledger_path).pop().unwrap();
    assert_eq!(entry.agent.as_deref(), Some("main"));
}

/// Real money (ThaiFi chain 17): `THCLAWS_PAYMENT_LIVE_NETWORKS=eip155:17`
/// plus a paired wallet at `THCLAWS_MPP_TEMPO_DIR`.
#[tokio::test]
#[ignore]
async fn live_thaifi_mpp() {
    let url = std::env::var("THCLAWS_MPP_LIVE_URL")
        .unwrap_or_else(|_| "https://mpp.thaifi.com/legal/search".into());
    let dir = tempfile::tempdir().unwrap();
    let approver = ScriptedApprover::with_payments(vec![], vec![PaymentDecision::ApproveOnce]);
    let c = ctx(approver.clone(), &dir, vec![]);
    let out = scope(
        c,
        crate::tools::paid_fetch::fetch(
            &reqwest::Client::new(),
            reqwest::Method::POST,
            &url,
            Some(r#"{"query":"มาตรา 26","limit":1}"#.into()),
            Some("application/json".into()),
            Some("thClaws MPP POC"),
            None,
        ),
    )
    .await
    .unwrap();
    println!("{out}");
    assert!(out.starts_with("💳 paid"), "{out}");
    assert!(out.contains("HTTP 200"), "{out}");
    let ledger = std::fs::read_to_string(dir.path().join("ledger.jsonl")).unwrap_or_default();
    println!("LEDGER {ledger}");
}
