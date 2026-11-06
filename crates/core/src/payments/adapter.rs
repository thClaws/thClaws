//! Rails behind the payment core. A licensed provider is a new
//! [`PaymentAdapter`] (and, for its wallet, a [`FundingAdapter`]); the gate,
//! mandates, prompt and ledger never change per rail.

use super::{Quote, Receipt};
use async_trait::async_trait;
use serde_json::Value;

/// What the core needs to know about a rail to adapt without code changes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AdapterCaps {
    /// Payees must come from the provider's verified directory.
    pub payee_must_be_kyc: bool,
    pub no_cross_chain: bool,
    pub supports_refund: bool,
}

/// What a tool asks a rail to pay for. `challenge` is the rail's own price
/// statement (an x402 402 body/header); `hint` is anything else the tool knows.
#[derive(Debug, Clone, Default)]
pub struct PaymentRequest {
    pub resource: Option<String>,
    pub challenge: Value,
    pub hint: Value,
}

/// Identity the provider has bound to the paying wallet (KYC'd org/person).
#[derive(Debug, Clone, PartialEq)]
pub struct KycBinding {
    pub subject: String,
    pub provider: String,
}

#[async_trait]
pub trait PaymentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn caps(&self) -> AdapterCaps;
    async fn quote(&self, req: &PaymentRequest) -> Result<Quote, String>;
    /// Called only after the core authorised `quote`.
    async fn pay(&self, quote: &Quote, mandate_id: Option<&str>) -> Result<Receipt, String>;
    async fn refund(&self, _receipt: &Receipt) -> Result<(), String> {
        Err(format!("{} does not support refunds", self.id()))
    }
    fn identity(&self) -> Option<KycBinding> {
        None
    }
    /// For `payee_must_be_kyc` rails: is this payee in the directory?
    async fn payee_verified(&self, _payee: &str) -> bool {
        true
    }
}

/// The wallet side, implemented by whoever holds the money. thClaws never
/// custodies or converts funds.
#[async_trait]
pub trait FundingAdapter: Send + Sync {
    async fn balance(&self, asset: &str, network: &str) -> Result<u128, String>;
    fn top_up_url(&self) -> Option<String> {
        None
    }
}

/// In-process stand-in for a licensed THB provider: KYC'd payee directory,
/// refunds, no chain. Proves a new rail is only an adapter. Carries no value.
pub struct MockThbAdapter {
    pub directory: Vec<(String, String)>,
    pub paid: std::sync::Mutex<Vec<Receipt>>,
}

impl MockThbAdapter {
    pub fn new(directory: &[(&str, &str)]) -> Self {
        Self {
            directory: directory
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
            paid: Default::default(),
        }
    }
}

#[async_trait]
impl PaymentAdapter for MockThbAdapter {
    fn id(&self) -> &'static str {
        "mock-thb"
    }
    fn caps(&self) -> AdapterCaps {
        AdapterCaps {
            payee_must_be_kyc: true,
            no_cross_chain: true,
            supports_refund: true,
        }
    }
    async fn quote(&self, req: &PaymentRequest) -> Result<Quote, String> {
        let c = &req.challenge;
        let payee = c["payee"]
            .as_str()
            .ok_or("challenge has no payee")?
            .to_string();
        let amount = c["amount"]
            .as_str()
            .and_then(|a| super::parse_units(a, 2))
            .ok_or("challenge has no amount")?;
        Ok(Quote {
            adapter: self.id().into(),
            amount: Some(amount),
            decimals: 2,
            asset: "THB".into(),
            asset_id: "THB".into(),
            payee_name: self
                .directory
                .iter()
                .find(|(id, _)| *id == payee)
                .map(|(_, n)| n.clone()),
            payee,
            network: "mock-thb".into(),
            resource: req.resource.clone(),
            description: c["description"].as_str().map(str::to_string),
        })
    }
    async fn pay(&self, quote: &Quote, _mandate_id: Option<&str>) -> Result<Receipt, String> {
        let r = Receipt {
            id: format!("thb-{}", uuid::Uuid::new_v4()),
            network: quote.network.clone(),
            amount: quote.amount,
            payee: quote.payee.clone(),
            raw: Value::Null,
            fee: None,
        };
        self.paid.lock().unwrap().push(r.clone());
        Ok(r)
    }
    async fn refund(&self, receipt: &Receipt) -> Result<(), String> {
        let mut paid = self.paid.lock().unwrap();
        let before = paid.len();
        paid.retain(|r| r.id != receipt.id);
        if paid.len() == before {
            return Err("no such receipt".into());
        }
        Ok(())
    }
    fn identity(&self) -> Option<KycBinding> {
        Some(KycBinding {
            subject: "test-org".into(),
            provider: "mock-thb".into(),
        })
    }
    async fn payee_verified(&self, payee: &str) -> bool {
        self.directory.iter().any(|(id, _)| id == payee)
    }
}
