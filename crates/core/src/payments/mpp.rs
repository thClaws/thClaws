//! MPP rail, `tempo` method, `charge` intent, push mode (ThaiFi chain 17 is
//! the first seller). A 402 carries `WWW-Authenticate: Payment id=…,
//! realm=…, method="tempo", intent="charge", request=<b64url JSON>`; the
//! client broadcasts a TIP-20 transfer with a memo bound to the challenge,
//! then retries with `Authorization: Payment <b64url credential>`.
//!
//! Signing a Tempo transaction with a P256 access key is delegated to a small
//! Node helper (`pay.mjs`, viem/tempo) in [`payer_dir`]; it transfers exactly
//! the authorised amount to the quoted payee and prints the tx hash. The key
//! stays in the ThaiFi wallet's own store; thClaws never reads it.

use super::adapter::{AdapterCaps, PaymentAdapter, PaymentRequest};
use super::x402::{keccak, to_hex};
use super::{Fee, Quote, Receipt};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

pub const PAYER_DIR_ENV: &str = "THCLAWS_MPP_TEMPO_DIR";

const TOKENS: &[(&str, &str)] = &[
    ("0x20c0000000000000000000000000000000000000", "pathTHB"),
    ("0x20c000000000000000000000c82102ffe7064362", "THCFI"),
    ("0x20c0000000000000000000007c24a0c628e8a940", "THCOC"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Challenge {
    pub id: String,
    pub realm: String,
    pub method: String,
    pub intent: String,
    pub request_raw: String,
    pub request: Value,
    pub description: Option<String>,
    pub expires: Option<String>,
    pub opaque: Option<String>,
}

fn b64u_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

pub fn b64u_encode(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Parses an MPP `Payment` (or `MPP`) challenge. `None` for other schemes.
pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let h = header.trim();
    let (scheme, rest) = h.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("payment") && !scheme.eq_ignore_ascii_case("mpp") {
        return None;
    }
    let mut params = std::collections::HashMap::new();
    let mut it = rest.trim().chars().peekable();
    loop {
        while it.peek().is_some_and(|c| c.is_whitespace() || *c == ',') {
            it.next();
        }
        let key: String =
            std::iter::from_fn(|| it.next_if(|c| c.is_ascii_alphanumeric() || *c == '_')).collect();
        if key.is_empty() {
            break;
        }
        if it.next() != Some('=') {
            break;
        }
        let mut val = String::new();
        if it.peek() == Some(&'"') {
            it.next();
            while let Some(c) = it.next() {
                match c {
                    '\\' => match it.next() {
                        Some('u') => {
                            let hex: String = (0..4).filter_map(|_| it.next()).collect();
                            match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                                Some(c) => val.push(c),
                                None => val.push_str(&hex),
                            }
                        }
                        Some(n) => val.push(n),
                        None => {}
                    },
                    '"' => break,
                    c => val.push(c),
                }
            }
        } else {
            val = std::iter::from_fn(|| it.next_if(|c| !c.is_whitespace() && *c != ',')).collect();
        }
        params.insert(key.to_ascii_lowercase(), val);
    }
    let request_raw = params.get("request")?.clone();
    let request: Value = serde_json::from_slice(&b64u_decode(&request_raw)?).ok()?;
    Some(Challenge {
        id: params.get("id").cloned().unwrap_or_default(),
        realm: params.get("realm")?.clone(),
        method: params
            .get("method")
            .cloned()
            .unwrap_or_else(|| "tempo".into()),
        intent: params
            .get("intent")
            .cloned()
            .unwrap_or_else(|| "charge".into()),
        request_raw,
        request,
        description: params.get("description").cloned(),
        expires: params.get("expires").cloned(),
        opaque: params.get("opaque").cloned(),
    })
}

/// 32-byte MPP attribution memo (mppx encoding): keccak("mpp")[..4] ‖ 0x01 ‖
/// keccak(realm)[..10] ‖ keccak(client)[..10] ‖ keccak(challenge id)[..7].
pub fn attribution_memo(realm: &str, challenge_id: &str) -> String {
    let mut m = Vec::with_capacity(32);
    m.extend_from_slice(&keccak(b"mpp")[..4]);
    m.push(0x01);
    m.extend_from_slice(&keccak(realm.as_bytes())[..10]);
    m.extend_from_slice(&keccak(b"thclaws")[..10]);
    m.extend_from_slice(&keccak(challenge_id.as_bytes())[..7]);
    to_hex(&m)
}

pub fn network_of(c: &Challenge) -> String {
    match c.request["methodDetails"]["chainId"].as_u64() {
        Some(id) => format!("eip155:{id}"),
        None => "tempo".into(),
    }
}

pub fn payer_dir() -> PathBuf {
    std::env::var_os(PAYER_DIR_ENV)
        .map(PathBuf::from)
        .or_else(|| crate::util::home_dir().map(|h| h.join(".thclaws").join("mpp-tempo")))
        .unwrap_or_else(|| PathBuf::from("mpp-tempo"))
}

pub struct MppAdapter {
    client: reqwest::Client,
    method: reqwest::Method,
    url: String,
    body: Option<String>,
    content_type: Option<String>,
    pub challenge: Challenge,
    pub response: Mutex<Option<(u16, String)>>,
}

impl MppAdapter {
    pub fn new(
        client: reqwest::Client,
        method: reqwest::Method,
        url: &str,
        body: Option<String>,
        content_type: Option<String>,
        challenge: Challenge,
    ) -> Self {
        Self {
            client,
            method,
            url: url.to_string(),
            body,
            content_type,
            challenge,
            response: Mutex::new(None),
        }
    }

    fn credential(&self, tx_hash: &str, from: &str, chain: &str) -> String {
        let c = &self.challenge;
        let mut ch = json!({
            "id": c.id, "realm": c.realm, "method": c.method,
            "intent": c.intent, "request": c.request_raw,
        });
        if let Some(o) = &c.opaque {
            ch["opaque"] = json!(o);
        }
        if let Some(e) = &c.expires {
            ch["expires"] = json!(e);
        }
        let doc = json!({
            "challenge": ch,
            "payload": {"type": "hash", "hash": tx_hash},
            "source": format!("did:pkh:{chain}:{from}"),
        });
        format!("Payment {}", b64u_encode(doc.to_string().as_bytes()))
    }
}

/// The gas the payer reports, `{amount, token, symbol?, decimals}`.
fn fee_of(paid: &Value) -> Option<Fee> {
    let f = &paid["fee"];
    let amount = f["amount"].as_str()?.parse().ok()?;
    let asset_id = f["token"].as_str()?.to_string();
    let asset = f["symbol"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            TOKENS
                .iter()
                .find(|(a, _)| a.eq_ignore_ascii_case(&asset_id))
                .map(|(_, s)| s.to_string())
        })
        .unwrap_or_else(|| asset_id.clone());
    Some(Fee {
        amount,
        decimals: f["decimals"]
            .as_u64()
            .and_then(|d| u8::try_from(d).ok())
            .unwrap_or(6),
        asset,
        asset_id,
        paid_by: None,
    })
}

async fn run_payer(to: &str, amount: u128, token: &str, memo: &str) -> Result<Value, String> {
    let dir = payer_dir();
    let script = dir.join("pay.mjs");
    if !script.is_file() {
        return Err(format!(
            "no MPP tempo payer at {} (install it there, or set {PAYER_DIR_ENV})",
            script.display()
        ));
    }
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new("node")
            .arg(&script)
            .args([
                "--to",
                to,
                "--amount",
                &amount.to_string(),
                "--token",
                token,
                "--memo",
                memo,
            ])
            .current_dir(&dir)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "MPP payer timed out after 120 s".to_string())?
    .map_err(|e| format!("MPP payer: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or("");
    let v: Value = serde_json::from_str(line).map_err(|_| {
        format!(
            "MPP payer failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(400)
                .collect::<String>()
        )
    })?;
    if let Some(e) = v["error"].as_str() {
        return Err(format!("MPP payer: {e}"));
    }
    Ok(v)
}

#[async_trait]
impl PaymentAdapter for MppAdapter {
    fn id(&self) -> &'static str {
        "mpp-tempo"
    }

    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            no_cross_chain: true,
            ..Default::default()
        }
    }

    async fn quote(&self, req: &PaymentRequest) -> Result<Quote, String> {
        let c = &self.challenge;
        if !c.method.eq_ignore_ascii_case("tempo") || !c.intent.eq_ignore_ascii_case("charge") {
            return Err(format!(
                "unsupported MPP challenge: method {} intent {} (only tempo charge)",
                c.method, c.intent
            ));
        }
        if let Some(exp) = c.expires.as_deref() {
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(exp) {
                if t < chrono::Utc::now() {
                    return Err(format!("MPP challenge expired at {exp}"));
                }
            }
        }
        let r = &c.request;
        let amount = r["amount"]
            .as_str()
            .and_then(|a| a.parse::<u128>().ok())
            .or_else(|| r["amount"].as_u64().map(u128::from))
            .ok_or("MPP challenge has no amount")?;
        let token = r["currency"].as_str().unwrap_or(TOKENS[0].0).to_string();
        let asset = TOKENS
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(&token))
            .map(|(_, s)| s.to_string())
            .unwrap_or_else(|| token.clone());
        Ok(Quote {
            adapter: "mpp-tempo".into(),
            amount: Some(amount),
            decimals: r["decimals"].as_u64().map(|d| d as u8).unwrap_or(6),
            asset,
            asset_id: token,
            payee: r["recipient"]
                .as_str()
                .ok_or("MPP challenge has no recipient")?
                .to_string(),
            payee_name: Some(c.realm.clone()),
            network: network_of(c),
            resource: req.resource.clone().or_else(|| Some(self.url.clone())),
            description: c
                .description
                .clone()
                .or_else(|| req.hint["purpose"].as_str().map(str::to_string)),
        })
    }

    async fn pay(&self, q: &Quote, _mandate_id: Option<&str>) -> Result<Receipt, String> {
        let amount = q.amount.ok_or("refusing to pay an unknown amount")?;
        let memo = self.challenge.request["memo"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| attribution_memo(&self.challenge.realm, &self.challenge.id));
        let paid = run_payer(&q.payee, amount, &q.asset_id, &memo).await?;
        let hash = paid["hash"]
            .as_str()
            .ok_or("MPP payer returned no tx hash")?
            .to_string();
        let from = paid["from"].as_str().unwrap_or("0x0").to_string();
        let auth = self.credential(&hash, &from, &q.network);
        let mut rb = self
            .client
            .request(self.method.clone(), &self.url)
            .header(reqwest::header::AUTHORIZATION, auth);
        if let Some(b) = &self.body {
            rb = rb.body(b.clone());
            if let Some(ct) = &self.content_type {
                rb = rb.header(reqwest::header::CONTENT_TYPE, ct);
            }
        }
        let receipt = Receipt {
            id: hash.clone(),
            network: q.network.clone(),
            amount: q.amount,
            payee: q.payee.clone(),
            raw: paid.clone(),
            fee: fee_of(&paid),
        };
        // The transfer is final on chain whatever the seller answers now, so a
        // failed retry is still recorded as paid, with the seller's answer.
        match rb.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                *self.response.lock().unwrap() = Some((status, text));
            }
            Err(e) => {
                *self.response.lock().unwrap() =
                    Some((0, format!("paid (tx {hash}) but the retry failed: {e}")));
            }
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"Payment id="ehgV7LOvhNXAWCPCwaGvn5TPFQpJRFUowlUz9cni78s", realm="mpp.thaifi.com", method="tempo", intent="charge", request="eyJhbW91bnQiOiIxMDAwMDAiLCJjdXJyZW5jeSI6IjB4MjBjMDAwMDAwMDAwMDAwMDAwMDAwMDAwYzgyMTAyRkZlNzA2NDM2MiIsIm1ldGhvZERldGFpbHMiOnsiY2hhaW5JZCI6MTcsInN1cHBvcnRlZE1vZGVzIjpbInB1c2giXX0sInJlY2lwaWVudCI6IjB4NTI2NkRmYTVhZTAxMzY3NGY4RmRDODMyYjdjNjAxQjgzOEQ5NGVFNiJ9", description="Legal knowledge base search — 1 query", expires="2099-11-04T16:17:19.338Z", opaque="eyJfbXBweF9zY29wZSI6IlBPU1QgL2xlZ2FsL3NlYXJjaCJ9""#;

    #[tokio::test]
    async fn quotes_a_live_thaifi_challenge() {
        let c = parse_challenge(LIVE).expect("parses");
        assert_eq!(c.realm, "mpp.thaifi.com");
        assert_eq!(
            c.description.as_deref(),
            Some("Legal knowledge base search — 1 query")
        );
        assert_eq!(network_of(&c), "eip155:17");
        let a = MppAdapter::new(
            reqwest::Client::new(),
            reqwest::Method::POST,
            "https://mpp.thaifi.com/legal/search",
            None,
            None,
            c,
        );
        let q = a.quote(&PaymentRequest::default()).await.unwrap();
        assert_eq!(q.amount, Some(100_000));
        assert_eq!(q.asset, "THCFI");
        assert_eq!(q.payee, "0x5266Dfa5ae013674f8FdC832b7c601B838D94eE6");
        assert_eq!(q.amount_text(), "0.10 THCFI");
    }

    #[test]
    fn other_schemes_are_not_mpp() {
        assert!(parse_challenge(r#"Bearer realm="x""#).is_none());
        assert!(parse_challenge(r#"ThaiFiAgent realm="x", challenge="y""#).is_none());
    }

    #[test]
    fn payer_fee_is_read_from_its_output() {
        let paid = json!({"hash": "0x1", "fee": {"amount": "11156", "token": "0x20c000000000000000000000c82102ffe7064362", "decimals": 6, "gasUsed": 46902}});
        let f = fee_of(&paid).unwrap();
        assert_eq!(
            (f.amount, f.asset.as_str(), f.paid_by.as_deref()),
            (11156, "THCFI", None)
        );
        assert_eq!(f.text(), "gas 0.011156 THCFI");
        assert!(fee_of(&json!({"hash": "0x1"})).is_none());
    }

    #[test]
    fn memo_is_32_bytes_and_bound_to_the_challenge() {
        let a = attribution_memo("mpp.thaifi.com", "id1");
        assert_eq!(a.len(), 66);
        assert!(a.starts_with(&to_hex(&keccak(b"mpp")[..4])));
        assert_eq!(&a[10..12], "01");
        assert_ne!(a, attribution_memo("mpp.thaifi.com", "id2"));
    }

    #[test]
    fn live_networks_need_the_env_opt_in() {
        assert!(!crate::payments::network_allowed("eip155:17"));
        assert!(crate::payments::network_allowed("eip155:84532"));
    }
}
