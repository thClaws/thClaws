//! x402 rail (Base Sepolia testnet): a 402 response states the price; the
//! client signs an EIP-3009 `transferWithAuthorization` (EIP-712) and retries
//! with it; the seller's facilitator settles. We never submit transactions,
//! never pay gas, and refuse to sign for any chain but Base Sepolia.
//!
//! Both header generations are read: v1 (JSON body with `accepts[]`, client
//! header `X-PAYMENT`, settlement in `X-PAYMENT-RESPONSE`) and v2 (base64
//! `PAYMENT-REQUIRED`, `PAYMENT-SIGNATURE`, `PAYMENT-RESPONSE`).

use super::adapter::{AdapterCaps, PaymentAdapter, PaymentRequest};
use super::{Fee, Quote, Receipt};
use async_trait::async_trait;
use base64::Engine as _;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};

pub const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;
pub const SECRET_NAME: &str = "x402-testnet";
pub const KEY_ENV: &str = "THCLAWS_X402_TEST_KEY";

pub fn chain_id(network: &str) -> Option<u64> {
    match network.trim().to_ascii_lowercase().as_str() {
        "base-sepolia" | "eip155:84532" => Some(BASE_SEPOLIA_CHAIN_ID),
        _ => None,
    }
}

pub fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

pub fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn address(s: &str) -> Result<[u8; 20], String> {
    let v = from_hex(s).ok_or_else(|| format!("bad address {s}"))?;
    v.try_into().map_err(|_| format!("bad address {s}"))
}

fn word_addr(a: &[u8; 20]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a);
    w
}

fn word_u(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

pub fn address_of(key: &VerifyingKey) -> String {
    let p = key.to_encoded_point(false);
    to_hex(&keccak(&p.as_bytes()[1..])[12..])
}

#[derive(Debug, Clone, PartialEq)]
pub struct Authorization {
    pub from: String,
    pub to: String,
    pub value: u128,
    pub valid_after: u128,
    pub valid_before: u128,
    pub nonce: [u8; 32],
}

impl Authorization {
    pub fn to_json(&self) -> Value {
        json!({
            "from": self.from,
            "to": self.to,
            "value": self.value.to_string(),
            "validAfter": self.valid_after.to_string(),
            "validBefore": self.valid_before.to_string(),
            "nonce": to_hex(&self.nonce),
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let n = |k: &str| v[k].as_str()?.parse::<u128>().ok();
        Some(Self {
            from: v["from"].as_str()?.to_string(),
            to: v["to"].as_str()?.to_string(),
            value: n("value")?,
            valid_after: n("validAfter")?,
            valid_before: n("validBefore")?,
            nonce: from_hex(v["nonce"].as_str()?)?.try_into().ok()?,
        })
    }
}

/// EIP-712 digest of an EIP-3009 `TransferWithAuthorization` for the token
/// at `contract` (domain name/version from the challenge's `extra`).
pub fn transfer_digest(
    name: &str,
    version: &str,
    chain_id: u64,
    contract: &str,
    a: &Authorization,
) -> Result<[u8; 32], String> {
    let domain_type = keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut d = Vec::with_capacity(32 * 5);
    d.extend_from_slice(&domain_type);
    d.extend_from_slice(&keccak(name.as_bytes()));
    d.extend_from_slice(&keccak(version.as_bytes()));
    d.extend_from_slice(&word_u(chain_id as u128));
    d.extend_from_slice(&word_addr(&address(contract)?));
    let domain = keccak(&d);

    let ty = keccak(
        b"TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
    );
    let mut s = Vec::with_capacity(32 * 7);
    s.extend_from_slice(&ty);
    s.extend_from_slice(&word_addr(&address(&a.from)?));
    s.extend_from_slice(&word_addr(&address(&a.to)?));
    s.extend_from_slice(&word_u(a.value));
    s.extend_from_slice(&word_u(a.valid_after));
    s.extend_from_slice(&word_u(a.valid_before));
    s.extend_from_slice(&a.nonce);
    let st = keccak(&s);

    let mut m = Vec::with_capacity(66);
    m.extend_from_slice(&[0x19, 0x01]);
    m.extend_from_slice(&domain);
    m.extend_from_slice(&st);
    Ok(keccak(&m))
}

/// 65-byte `r || s || v` (v = 27/28), the form EIP-3009 contracts accept.
pub fn sign_digest(key: &SigningKey, digest: &[u8; 32]) -> Result<String, String> {
    let (sig, rec) = key
        .sign_prehash_recoverable(digest)
        .map_err(|e| format!("sign: {e}"))?;
    let mut out = sig.to_bytes().to_vec();
    out.push(rec.to_byte() + 27);
    Ok(to_hex(&out))
}

/// Signer address for a 65-byte signature over `digest`.
pub fn recover(digest: &[u8; 32], sig_hex: &str) -> Option<String> {
    let b = from_hex(sig_hex)?;
    if b.len() != 65 {
        return None;
    }
    let sig = Signature::from_slice(&b[..64]).ok()?;
    let rec = RecoveryId::from_byte(b[64].checked_sub(27).unwrap_or(b[64]))?;
    let vk = VerifyingKey::recover_from_prehash(digest, &sig, rec).ok()?;
    Some(address_of(&vk))
}

pub fn key_from_hex(s: &str) -> Result<SigningKey, String> {
    let b = from_hex(s).ok_or("wallet key is not hex")?;
    SigningKey::from_slice(&b).map_err(|_| "wallet key is not a valid secp256k1 key".to_string())
}

/// The testnet wallet key: `THCLAWS_X402_TEST_KEY`, else the OS keychain
/// entry `x402-testnet`. Never read from settings, never logged.
pub fn load_wallet() -> Result<SigningKey, String> {
    let raw = std::env::var(KEY_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| crate::secrets::get(SECRET_NAME))
        .or_else(|| crate::secrets::keychain_get_raw(SECRET_NAME))
        .ok_or_else(|| {
            format!(
                "no x402 testnet wallet: set {KEY_ENV} (env or .env) to a testnet-only private key \
                 and fund it with test USDC at https://faucet.circle.com"
            )
        })?;
    key_from_hex(&raw)
}

/// Which wire generation a challenge came in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Version {
    V1,
    V2,
}

/// Picks the `exact` requirement on a network we can sign for from a 402.
/// `header` is the `PAYMENT-REQUIRED` value (v2), `body` the response body.
pub fn parse_challenge(header: Option<&str>, body: &str) -> Result<(Version, Value), String> {
    let (version, doc) = match header {
        Some(h) => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(h.trim())
                .map_err(|e| format!("PAYMENT-REQUIRED is not base64: {e}"))?;
            (
                Version::V2,
                serde_json::from_slice::<Value>(&raw).map_err(|e| e.to_string())?,
            )
        }
        None => (
            Version::V1,
            serde_json::from_str::<Value>(body)
                .map_err(|_| "402 without an x402 challenge".to_string())?,
        ),
    };
    let accepts = doc["accepts"]
        .as_array()
        .ok_or("challenge has no accepts[]")?;
    let networks: Vec<String> = accepts
        .iter()
        .filter_map(|a| a["network"].as_str().map(str::to_string))
        .collect();
    let pick = accepts
        .iter()
        .find(|a| a["scheme"] == "exact" && a["network"].as_str().is_some_and(|n| chain_id(n).is_some()))
        .ok_or_else(|| {
            format!(
                "payment refused: no testnet option in the challenge (offered: {}); thClaws pays on Base Sepolia only",
                networks.join(", ")
            )
        })?;
    let mut req = pick.clone();
    if version == Version::V2 {
        if let Some(r) = doc.get("resource") {
            req["_resource"] = r.clone();
        }
    }
    Ok((version, req))
}

pub struct X402Adapter {
    pub key: SigningKey,
    pub version: Version,
    pub client: reqwest::Client,
    pub url: String,
    pub method: reqwest::Method,
    pub body: Option<String>,
    pub content_type: Option<String>,
    /// The paid response: status and body, for the tool to return.
    pub response: std::sync::Mutex<Option<(u16, String)>>,
    accepted: Value,
    resource: Value,
    domain: (String, String),
}

fn requirement_amount(req: &Value) -> Option<u128> {
    req["maxAmountRequired"]
        .as_str()
        .or_else(|| req["amount"].as_str())
        .and_then(|s| s.parse().ok())
}

#[async_trait]
impl PaymentAdapter for X402Adapter {
    fn id(&self) -> &'static str {
        "x402"
    }
    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            payee_must_be_kyc: false,
            no_cross_chain: true,
            supports_refund: false,
        }
    }

    async fn quote(&self, req: &PaymentRequest) -> Result<Quote, String> {
        let r = &req.challenge;
        let extra = &r["extra"];
        let asset_name = extra["name"].as_str().unwrap_or("USDC");
        let symbol = if asset_name.to_ascii_uppercase().contains("USD") {
            "USDC"
        } else {
            asset_name
        };
        Ok(Quote {
            adapter: self.id().into(),
            amount: Some(requirement_amount(r).ok_or("challenge has no amount")?),
            decimals: extra["decimals"].as_u64().unwrap_or(6) as u8,
            asset: symbol.to_string(),
            asset_id: r["asset"]
                .as_str()
                .ok_or("challenge has no asset")?
                .to_string(),
            payee: r["payTo"]
                .as_str()
                .ok_or("challenge has no payTo")?
                .to_string(),
            payee_name: extra["payeeName"].as_str().map(str::to_string),
            network: r["network"].as_str().unwrap_or_default().to_string(),
            resource: r["resource"]
                .as_str()
                .or_else(|| r["_resource"]["url"].as_str())
                .map(str::to_string)
                .or_else(|| req.resource.clone()),
            description: r["description"]
                .as_str()
                .or_else(|| r["_resource"]["description"].as_str())
                .map(str::to_string),
        })
    }

    async fn pay(&self, q: &Quote, _mandate_id: Option<&str>) -> Result<Receipt, String> {
        let chain = chain_id(&q.network).ok_or("refusing to sign for a non-testnet chain")?;
        let amount = q.amount.ok_or("refusing to sign an unknown amount")?;
        let now = chrono::Utc::now().timestamp().max(0) as u128;
        let mut nonce = [0u8; 32];
        getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?;
        let auth = Authorization {
            from: address_of(self.key.verifying_key()),
            to: q.payee.clone(),
            value: amount,
            valid_after: now.saturating_sub(60),
            valid_before: now + 300,
            nonce,
        };
        let (name, version) = self.domain_hint();
        let digest = transfer_digest(&name, &version, chain, &q.asset_id, &auth)?;
        let signature = sign_digest(&self.key, &digest)?;
        let payload = json!({"signature": signature, "authorization": auth.to_json()});
        let (header, doc) = match self.version {
            Version::V1 => (
                "X-PAYMENT",
                json!({"x402Version": 1, "scheme": "exact", "network": q.network, "payload": payload}),
            ),
            Version::V2 => ("PAYMENT-SIGNATURE", {
                let mut d = json!({"x402Version": 2, "accepted": self.accepted.clone(), "payload": payload});
                if !self.resource.is_null() {
                    d["resource"] = self.resource.clone();
                }
                d
            }),
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(doc.to_string());
        let mut rb = self
            .client
            .request(self.method.clone(), &self.url)
            .header(header, encoded);
        if let Some(b) = &self.body {
            rb = rb.body(b.clone());
            if let Some(ct) = &self.content_type {
                rb = rb.header(reqwest::header::CONTENT_TYPE, ct);
            }
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| format!("paid request failed: {e}"))?;
        let status = resp.status().as_u16();
        let settle = resp
            .headers()
            .get("PAYMENT-RESPONSE")
            .or_else(|| resp.headers().get("X-PAYMENT-RESPONSE"))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let reason = resp
            .headers()
            .get("PAYMENT-REQUIRED")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .or_else(|| {
                settle.as_ref().and_then(|s| {
                    s["errorReason"]
                        .as_str()
                        .or_else(|| s["error"].as_str())
                        .map(str::to_string)
                })
            });
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            let why = reason.map(|r| format!(" ({r})")).unwrap_or_default();
            return Err(format!(
                "seller answered {status} after payment{why}: {}",
                text.chars().take(300).collect::<String>()
            ));
        }
        *self.response.lock().unwrap() = Some((status, text));
        let settle = settle.unwrap_or(Value::Null);
        Ok(Receipt {
            id: settle["transaction"]
                .as_str()
                .or_else(|| settle["txHash"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("nonce:{}", to_hex(&nonce))),
            network: q.network.clone(),
            amount: q.amount,
            payee: q.payee.clone(),
            raw: settle,
            // EIP-3009: the facilitator submits the transfer and pays its gas.
            fee: Some(Fee {
                amount: 0,
                decimals: q.decimals,
                asset: q.asset.clone(),
                asset_id: q.asset_id.clone(),
                paid_by: Some("facilitator".into()),
            }),
        })
    }
}

impl X402Adapter {
    pub fn new(
        key: SigningKey,
        client: reqwest::Client,
        method: reqwest::Method,
        url: &str,
        body: Option<String>,
        content_type: Option<String>,
    ) -> Self {
        Self {
            key,
            version: Version::V1,
            client,
            url: url.to_string(),
            method,
            body,
            content_type,
            response: Default::default(),
            accepted: Value::Null,
            resource: Value::Null,
            domain: ("USDC".into(), "2".into()),
        }
    }

    /// Records the requirement the quote came from (needed to sign and, in
    /// v2, echoed back in the payment header).
    pub fn set_requirement(&mut self, version: Version, req: &Value) {
        self.version = version;
        self.domain = (
            req["extra"]["name"].as_str().unwrap_or("USDC").to_string(),
            req["extra"]["version"].as_str().unwrap_or("2").to_string(),
        );
        let mut accepted = req.clone();
        if let Some(o) = accepted.as_object_mut() {
            self.resource = o.remove("_resource").unwrap_or(Value::Null);
        }
        self.accepted = accepted;
    }

    fn domain_hint(&self) -> (String, String) {
        self.domain.clone()
    }
}
