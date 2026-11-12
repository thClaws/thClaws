//! Agent payments (dev-plan/74): a call that moves money is gated apart
//! from every other tool.
//!
//! Order for every payment: the adapter **quotes**, the network must be on
//! the testnet allow-list, a **mandate** must cover the quote or a human must
//! approve it (once, or by granting a budget), and only then does the
//! adapter **pay**. Under `-p`, team agents and multiuser pods there is no
//! human, so a payment with no covering mandate is denied — the generic
//! "auto-approve everything" sink never reaches this gate.
//!
//! The core never names a rail; rails live behind [`adapter::PaymentAdapter`].

pub mod adapter;
pub mod mpp;
pub mod x402;

use crate::permissions::{AgentOrigin, ApprovalSink};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use adapter::{AdapterCaps, PaymentAdapter, PaymentRequest};

/// Networks a payment may settle on. Testnets only, by owner decision
/// (2026-11-03): no real money moves through thClaws until the Thai legal
/// position is clear or a licensed provider exists. `mock-thb` is the
/// in-process stand-in for a licensed THB provider and carries no value.
const ALLOWED_NETWORKS: &[&str] = &["base-sepolia", "eip155:84532", "mock-thb"];

/// Internal POC only: real-money networks enabled by env for one process
/// (e.g. `eip155:17` for ThaiFi). Never read from settings, so no workspace
/// can opt itself in.
pub const LIVE_NETWORKS_ENV: &str = "THCLAWS_PAYMENT_LIVE_NETWORKS";

pub fn live_network_enabled(network: &str) -> bool {
    std::env::var(LIVE_NETWORKS_ENV).is_ok_and(|v| {
        v.split(',')
            .any(|n| !n.trim().is_empty() && n.trim().eq_ignore_ascii_case(network.trim()))
    })
}

pub fn network_allowed(network: &str) -> bool {
    is_testnet(network) || live_network_enabled(network)
}

pub fn is_testnet(network: &str) -> bool {
    ALLOWED_NETWORKS
        .iter()
        .any(|n| n.eq_ignore_ascii_case(network.trim()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub adapter: String,
    /// Base units (e.g. USDC has 6 decimals). `None` = the tool did not say.
    pub amount: Option<u128>,
    pub decimals: u8,
    /// Display symbol: `USDC`, `THB`.
    pub asset: String,
    /// Rail-level id: a token contract, or the symbol again.
    pub asset_id: String,
    pub payee: String,
    pub payee_name: Option<String>,
    pub network: String,
    pub resource: Option<String>,
    pub description: Option<String>,
}

impl Quote {
    pub fn amount_text(&self) -> String {
        match self.amount {
            Some(a) => format!("{} {}", format_units(a, self.decimals), self.asset),
            None => format!("unknown amount of {}", self.asset),
        }
    }

    pub fn payee_text(&self) -> String {
        match &self.payee_name {
            Some(n) => format!("{n} ({})", self.payee),
            None => self.payee.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub id: String,
    pub network: String,
    pub amount: Option<u128>,
    pub payee: String,
    #[serde(default)]
    pub raw: Value,
    /// Network fee (gas) on top of `amount`. `None` = the rail did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee: Option<Fee>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fee {
    pub amount: u128,
    pub decimals: u8,
    pub asset: String,
    pub asset_id: String,
    /// Set when someone other than our wallet paid it (x402's facilitator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paid_by: Option<String>,
}

impl Fee {
    /// What our wallet paid, in the quote's asset; `None` when the fee was
    /// someone else's or in another token.
    pub fn ours_in(&self, q: &Quote) -> Option<u128> {
        (self.paid_by.is_none() && self.asset_id.eq_ignore_ascii_case(&q.asset_id))
            .then_some(self.amount)
    }

    pub fn text(&self) -> String {
        match &self.paid_by {
            Some(who) => format!("gas paid by {who}"),
            None => format!(
                "gas {} {}",
                format_units(self.amount, self.decimals),
                self.asset
            ),
        }
    }
}

/// Why the gate is asking a human (or refusing, when none can answer).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AskReason {
    NoMandate,
    OverBudget { left: u128 },
    OverPerTx { per_tx: u128 },
    OutOfScope,
    UnknownAmount,
    AssetMismatch,
    Expired,
}

impl AskReason {
    fn describe(&self, q: &Quote) -> String {
        match self {
            AskReason::NoMandate => format!(
                "no mandate covers payee {} / amount {}",
                q.payee_text(),
                q.amount_text()
            ),
            AskReason::OverBudget { left } => format!(
                "{} is over the remaining budget ({} {})",
                q.amount_text(),
                format_units(*left, q.decimals),
                q.asset
            ),
            AskReason::OverPerTx { per_tx } => format!(
                "{} is over the per-payment cap ({} {})",
                q.amount_text(),
                format_units(*per_tx, q.decimals),
                q.asset
            ),
            AskReason::OutOfScope => format!("payee {} is outside the mandate", q.payee_text()),
            AskReason::UnknownAmount => "the amount is unknown".into(),
            AskReason::AssetMismatch => format!("no mandate in {}", q.asset),
            AskReason::Expired => "the mandate has expired".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaymentApprovalRequest {
    pub tool_name: String,
    pub quote: Quote,
    pub reason: AskReason,
    pub originator: AgentOrigin,
}

/// A budget a human grants from the approval prompt. Amounts are in the
/// quote's base units and asset.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetGrant {
    pub total: u128,
    pub per_tx: Option<u128>,
    /// `None` = any payee.
    pub payees: Option<Vec<String>>,
    pub expires: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaymentDecision {
    ApproveOnce,
    Budget(BudgetGrant),
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MandateSource {
    Config,
    EePolicy,
    Modal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mandate {
    pub id: String,
    pub source: MandateSource,
    pub asset: String,
    pub network: Option<String>,
    /// Decimal strings, interpreted in the quote's decimals so one mandate
    /// reads the same for any rail that prices in its asset.
    pub total: Option<String>,
    pub daily: Option<String>,
    /// Calendar month (UTC), computed from the ledger like `daily`.
    pub monthly: Option<String>,
    pub per_tx: Option<String>,
    pub payees: Option<Vec<String>>,
    /// Where the money may go by resource: host globs or URL prefixes.
    pub sources: Option<Vec<String>>,
    pub expires: Option<DateTime<Utc>>,
    pub allow_unknown_amount: bool,
}

/// `paymentMandates` entry in settings.json.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct MandateConfig {
    pub id: String,
    pub asset: String,
    pub network: Option<String>,
    pub total: Option<String>,
    pub daily: Option<String>,
    pub monthly: Option<String>,
    pub per_tx: Option<String>,
    pub payees: Option<Vec<String>>,
    pub sources: Option<Vec<String>>,
    pub expires: Option<String>,
    pub allow_unknown_amount: bool,
}

impl MandateConfig {
    pub fn to_mandate(&self, source: MandateSource) -> Option<Mandate> {
        let expires = match &self.expires {
            Some(s) => Some(DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Utc)),
            None => None,
        };
        if self.id.trim().is_empty() || self.asset.trim().is_empty() {
            return None;
        }
        if self.total.is_none() && self.daily.is_none() && self.monthly.is_none() {
            return None;
        }
        Some(Mandate {
            id: self.id.clone(),
            source,
            asset: self.asset.clone(),
            network: self.network.clone(),
            total: self.total.clone(),
            daily: self.daily.clone(),
            monthly: self.monthly.clone(),
            per_tx: self.per_tx.clone(),
            payees: self.payees.clone(),
            sources: self.sources.clone(),
            expires,
            allow_unknown_amount: self.allow_unknown_amount,
        })
    }
}

/// Settings read once per process: which tools count as payments and the
/// standing mandates (the only way a headless run can pay).
#[derive(Debug, Clone, Default)]
pub struct PaymentSettings {
    pub enabled: bool,
    pub payment_tools: Vec<String>,
    pub mandates: Vec<Mandate>,
    /// When set, a payment to any other source is refused before a prompt.
    pub allowed_sources: Option<Vec<String>>,
    /// When set, only these agents (`main` or an agent name) may pay.
    pub agents: Option<Vec<String>>,
}

static SETTINGS: Mutex<Option<PaymentSettings>> = Mutex::new(None);

/// Layered like the rest of settings: each file that sets a field replaces
/// what a lower layer set; a field a file leaves out is kept.
pub fn merge_settings(
    enabled: Option<bool>,
    tools: Option<&[String]>,
    mandates: Option<&[MandateConfig]>,
    allowed_sources: Option<&[String]>,
    agents: Option<&[String]>,
) {
    let mut g = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let cur = g.get_or_insert_with(PaymentSettings::default);
    if let Some(a) = allowed_sources {
        cur.allowed_sources = Some(a.to_vec());
    }
    if let Some(a) = agents {
        cur.agents = Some(a.to_vec());
    }
    if let Some(e) = enabled {
        cur.enabled = e;
    }
    if let Some(t) = tools {
        cur.payment_tools = t.to_vec();
    }
    if let Some(ms) = mandates {
        cur.mandates = ms
            .iter()
            .filter_map(|m| {
                let out = m.to_mandate(MandateSource::Config);
                if out.is_none() {
                    eprintln!(
                        "[payments] ignoring invalid paymentMandates entry {:?}",
                        m.id
                    );
                }
                out
            })
            .collect();
    }
}

/// Who decides, outside the human: the EE policy when one is loaded,
/// otherwise settings.
#[derive(Debug, Clone, PartialEq)]
pub enum Governance {
    Open {
        allowed_sources: Option<Vec<String>>,
        agents: Option<Vec<String>>,
        /// Multiuser worker: one shared settings file must not let every
        /// member spend under the same standing mandate.
        ignore_config_mandates: bool,
    },
    /// EE policy without a `payments` block.
    Refused,
    Ee(EePayments),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EePayments {
    pub asset: String,
    pub allowed_sources: Vec<String>,
    pub max_per_request: Option<String>,
    pub monthly_budget: Option<String>,
    pub agents: Vec<String>,
}

impl EePayments {
    fn mandate(&self) -> Mandate {
        Mandate {
            id: "ee-policy".into(),
            source: MandateSource::EePolicy,
            asset: self.asset.clone(),
            network: None,
            total: None,
            daily: None,
            monthly: self.monthly_budget.clone(),
            per_tx: self.max_per_request.clone(),
            payees: None,
            sources: Some(self.allowed_sources.clone()),
            expires: None,
            allow_unknown_amount: false,
        }
    }
}

pub fn governance() -> Governance {
    if crate::policy::active().is_some() {
        return match crate::policy::payments() {
            Some(p) => Governance::Ee(EePayments {
                asset: p.asset.clone().unwrap_or_else(|| "USDC".into()),
                allowed_sources: p.allowed_sources.clone(),
                max_per_request: p.max_per_request.clone(),
                monthly_budget: p.monthly_budget.clone(),
                agents: p.agents.clone(),
            }),
            None => Governance::Refused,
        };
    }
    let s = settings();
    Governance::Open {
        allowed_sources: s.allowed_sources,
        agents: s.agents,
        ignore_config_mandates: crate::workdir::is_multiuser(),
    }
}

/// `main` for the user's own agent, otherwise the agent's name.
pub fn agent_label(o: &AgentOrigin) -> String {
    match o {
        AgentOrigin::Main => "main".into(),
        AgentOrigin::SideChannel { agent_name, .. } | AgentOrigin::Subagent { agent_name, .. } => {
            agent_name.clone()
        }
    }
}

/// Host globs (`*.example.com`, `api.example.com`) match the resource's
/// host; entries with `://` match as URL prefixes.
pub fn source_allowed(list: &[String], resource: Option<&str>) -> bool {
    let Some(res) = resource else { return false };
    let host = reqwest::Url::parse(res)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    list.iter().any(|p| {
        let p = p.trim();
        if p.contains("://") {
            res.starts_with(p)
        } else {
            host.as_deref()
                .is_some_and(|h| glob_match(&p.to_ascii_lowercase(), h))
        }
    })
}

fn source_label(q: &Quote) -> String {
    q.resource
        .as_deref()
        .and_then(|r| reqwest::Url::parse(r).ok())
        .and_then(|u| u.host_str().map(str::to_string))
        .or_else(|| q.resource.clone())
        .unwrap_or_else(|| "(unknown)".into())
}

/// Whether `PaidFetch` is offered to the model.
pub fn tool_enabled() -> bool {
    match governance() {
        Governance::Refused => return false,
        Governance::Ee(_) => return true,
        Governance::Open { .. } => {}
    }
    std::env::var(x402::KEY_ENV).is_ok_and(|v| !v.trim().is_empty())
        || std::env::var("THCLAWS_PAYMENTS").is_ok_and(|v| v == "1")
        || settings().enabled
}

fn settings() -> PaymentSettings {
    SETTINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// True when settings mark `tool_name` as a payment tool (`*` globs).
pub fn is_payment_tool(tool_name: &str) -> bool {
    settings()
        .payment_tools
        .iter()
        .any(|p| glob_match(p, tool_name))
}

fn glob_match(pattern: &str, name: &str) -> bool {
    globset::Glob::new(pattern)
        .map(|g| g.compile_matcher().is_match(name))
        .unwrap_or(false)
}

/// Budgets granted from the prompt. Owned by the approver, so they die
/// with its session (`reset_session_flag`) and never reach a `-p` run.
#[derive(Default)]
pub struct Budgets {
    inner: Mutex<Vec<Mandate>>,
}

impl Budgets {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn list(&self) -> Vec<Mandate> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn add(&self, m: Mandate) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).push(m);
    }
    pub fn clear(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// What a payment needs from the agent that is running the tool. Set as a
/// task-local around each tool call so a tool can pay only inside a turn.
#[derive(Clone)]
pub struct PaymentContext {
    pub approver: Arc<dyn ApprovalSink>,
    pub originator: AgentOrigin,
    pub tool_name: String,
    pub ledger_path: PathBuf,
    /// Standing mandates; `None` reads them from settings.
    pub mandates: Option<Vec<Mandate>>,
    /// `None` reads it from the EE policy and settings.
    pub governance: Option<Governance>,
}

tokio::task_local! {
    static CONTEXT: PaymentContext;
}

pub async fn scope<F: std::future::Future>(ctx: PaymentContext, f: F) -> F::Output {
    CONTEXT.scope(ctx, f).await
}

pub fn current() -> Option<PaymentContext> {
    CONTEXT.try_with(|c| c.clone()).ok()
}

pub fn default_ledger_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".thclaws/state/payments/ledger.jsonl")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decided {
    Mandate,
    ApprovedOnce,
    Budget,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub ts: DateTime<Utc>,
    pub tool: String,
    /// `main` or the agent name that asked to pay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub quote: Quote,
    pub mandate_id: Option<String>,
    pub decision: Decided,
    pub receipt: Option<Receipt>,
    pub error: Option<String>,
}

impl LedgerEntry {
    /// Service price plus any gas our wallet paid in the same asset: what
    /// actually left the wallet, so caps count it.
    fn spent(&self) -> Option<u128> {
        let r = self.receipt.as_ref()?;
        let fee = r
            .fee
            .as_ref()
            .and_then(|f| f.ours_in(&self.quote))
            .unwrap_or(0);
        match self.quote.amount {
            Some(a) => Some(a + fee),
            None => (fee > 0).then_some(fee),
        }
    }
}

fn read_ledger(path: &std::path::Path) -> Vec<LedgerEntry> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn append_ledger(path: &std::path::Path, entry: &LedgerEntry) {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    match o.open(path) {
        Ok(mut f) => {
            let mut line = serde_json::to_string(entry).unwrap_or_default();
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
        }
        Err(e) => eprintln!("[payments] cannot write ledger {}: {e}", path.display()),
    }
    PAYMENTS_THIS_RUN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(entry.clone());
}

static PAYMENTS_THIS_RUN: Mutex<Vec<LedgerEntry>> = Mutex::new(Vec::new());

/// Every payment decision made by this process, for `-p --output-format json`.
pub fn payments_this_run() -> Vec<LedgerEntry> {
    PAYMENTS_THIS_RUN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// `"12.5"` at 6 decimals → `12_500_000`. Rejects negatives, junk and
/// more fractional digits than the asset has.
pub fn parse_units(s: &str, decimals: u8) -> Option<u128> {
    let s = s.trim();
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if frac.len() > decimals as usize {
        return None;
    }
    let scale = 10u128.checked_pow(decimals as u32)?;
    let w: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let f: u128 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<width$}", width = decimals as usize)
            .parse()
            .ok()?
    };
    w.checked_mul(scale)?.checked_add(f)
}

pub fn format_units(v: u128, decimals: u8) -> String {
    if decimals == 0 {
        return v.to_string();
    }
    let scale = 10u128.pow(decimals as u32);
    let frac = format!("{:0width$}", v % scale, width = decimals as usize);
    let frac = frac.trim_end_matches('0');
    let frac = if frac.len() < 2 {
        format!("{frac:0<2}")
    } else {
        frac.to_string()
    };
    format!("{}.{}", v / scale, frac)
}

struct Spend {
    total: u128,
    today: u128,
    month: u128,
}

fn spend_for(entries: &[LedgerEntry], mandate_id: &str, now: DateTime<Utc>) -> Spend {
    use chrono::Datelike;
    let mut s = Spend {
        total: 0,
        today: 0,
        month: 0,
    };
    for e in entries
        .iter()
        .filter(|e| e.mandate_id.as_deref() == Some(mandate_id))
    {
        if let Some(a) = e.spent() {
            s.total += a;
            if e.ts.date_naive() == now.date_naive() {
                s.today += a;
            }
            if e.ts.year() == now.year() && e.ts.month() == now.month() {
                s.month += a;
            }
        }
    }
    s
}

fn payee_matches(list: &[String], q: &Quote) -> bool {
    list.iter().any(|p| {
        p.eq_ignore_ascii_case(&q.payee)
            || q.payee_name
                .as_deref()
                .is_some_and(|n| p.eq_ignore_ascii_case(n))
    })
}

/// `Ok(())` when `m` covers `q` given what it has already spent; otherwise
/// the reason the human (if any) is being asked.
fn covers(
    m: &Mandate,
    q: &Quote,
    entries: &[LedgerEntry],
    now: DateTime<Utc>,
) -> Result<(), AskReason> {
    if !m.asset.eq_ignore_ascii_case(&q.asset) {
        return Err(AskReason::AssetMismatch);
    }
    if let Some(n) = &m.network {
        if !n.eq_ignore_ascii_case(&q.network) {
            return Err(AskReason::OutOfScope);
        }
    }
    if m.expires.is_some_and(|e| e <= now) {
        return Err(AskReason::Expired);
    }
    if let Some(list) = &m.payees {
        if !payee_matches(list, q) {
            return Err(AskReason::OutOfScope);
        }
    }
    if let Some(list) = &m.sources {
        if !source_allowed(list, q.resource.as_deref()) {
            return Err(AskReason::OutOfScope);
        }
    }
    let Some(amount) = q.amount else {
        return if m.allow_unknown_amount {
            Ok(())
        } else {
            Err(AskReason::UnknownAmount)
        };
    };
    let cap = |s: &Option<String>| s.as_deref().and_then(|v| parse_units(v, q.decimals));
    if let Some(per_tx) = cap(&m.per_tx) {
        if amount > per_tx {
            return Err(AskReason::OverPerTx { per_tx });
        }
    }
    let spent = spend_for(entries, &m.id, now);
    if let Some(total) = cap(&m.total) {
        let left = total.saturating_sub(spent.total);
        if amount > left {
            return Err(AskReason::OverBudget { left });
        }
    }
    if let Some(daily) = cap(&m.daily) {
        let left = daily.saturating_sub(spent.today);
        if amount > left {
            return Err(AskReason::OverBudget { left });
        }
    }
    if let Some(monthly) = cap(&m.monthly) {
        let left = monthly.saturating_sub(spent.month);
        if amount > left {
            return Err(AskReason::OverBudget { left });
        }
    }
    Ok(())
}

fn left_on(m: &Mandate, q: &Quote, entries: &[LedgerEntry], now: DateTime<Utc>) -> Option<u128> {
    let spent = spend_for(entries, &m.id, now);
    let cap = |s: &Option<String>| s.as_deref().and_then(|v| parse_units(v, q.decimals));
    [
        cap(&m.total).map(|c| c.saturating_sub(spent.total)),
        cap(&m.daily).map(|c| c.saturating_sub(spent.today)),
        cap(&m.monthly).map(|c| c.saturating_sub(spent.month)),
    ]
    .into_iter()
    .flatten()
    .min()
}

#[derive(Debug, Clone)]
pub struct Authorized {
    pub mandate_id: Option<String>,
    pub decision: Decided,
}

/// Serialises quote → authorise → pay → ledger across the process, so two
/// concurrent payments can't both fit under the same remaining budget.
static PAY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn grant_to_mandate(g: &BudgetGrant, q: &Quote) -> Mandate {
    Mandate {
        id: format!("budget-{}", uuid::Uuid::new_v4()),
        source: MandateSource::Modal,
        asset: q.asset.clone(),
        network: Some(q.network.clone()),
        total: Some(format_units(g.total, q.decimals)),
        daily: None,
        monthly: None,
        per_tx: g.per_tx.map(|v| format_units(v, q.decimals)),
        payees: g.payees.clone(),
        sources: None,
        expires: g.expires,
        allow_unknown_amount: false,
    }
}

async fn authorize(
    ctx: &PaymentContext,
    q: &Quote,
    entries: &[LedgerEntry],
) -> Result<Authorized, String> {
    if !network_allowed(&q.network) {
        return Err(format!(
            "payment refused: network {} is not allowed (thClaws pays on testnets only; live networks need {LIVE_NETWORKS_ENV})",
            q.network
        ));
    }
    let now = Utc::now();
    let gov = ctx.governance.clone().unwrap_or_else(governance);
    let agent = agent_label(&ctx.originator);
    let may_pay = |list: &[String]| list.iter().any(|a| a.eq_ignore_ascii_case(&agent));
    let standing = match &gov {
        Governance::Refused => {
            return Err("payment refused: the company policy does not allow agent payments".into())
        }
        Governance::Ee(p) => {
            if !p.agents.is_empty() && !may_pay(&p.agents) {
                return Err(format!(
                    "payment refused: agent {agent} may not pay under the company policy"
                ));
            }
            if !source_allowed(&p.allowed_sources, q.resource.as_deref()) {
                return Err(format!(
                    "payment refused: {} is not on the company's list of approved sources",
                    source_label(q)
                ));
            }
            let m = p.mandate();
            return match covers(&m, q, entries, now) {
                Ok(()) => Ok(Authorized {
                    mandate_id: Some(m.id),
                    decision: Decided::Mandate,
                }),
                Err(r) => Err(format!(
                    "payment refused by company policy: {}",
                    r.describe(q)
                )),
            };
        }
        Governance::Open {
            allowed_sources,
            agents,
            ignore_config_mandates,
        } => {
            if let Some(list) = agents {
                if !may_pay(list) {
                    return Err(format!(
                        "payment refused: agent {agent} is not allowed to pay (paymentAgents)"
                    ));
                }
            }
            if let Some(list) = allowed_sources {
                if !source_allowed(list, q.resource.as_deref()) {
                    return Err(format!(
                        "payment refused: {} is not on the allowed sources list (paymentAllowedSources)",
                        source_label(q)
                    ));
                }
            }
            if *ignore_config_mandates {
                Vec::new()
            } else {
                ctx.mandates.clone().unwrap_or_else(|| settings().mandates)
            }
        }
    };
    let budgets = ctx.approver.payment_budgets();
    let granted = budgets.as_ref().map(|b| b.list()).unwrap_or_default();

    let mut reason = AskReason::NoMandate;
    for m in standing.iter().chain(granted.iter()) {
        match covers(m, q, entries, now) {
            Ok(()) => {
                return Ok(Authorized {
                    mandate_id: Some(m.id.clone()),
                    decision: Decided::Mandate,
                })
            }
            Err(AskReason::AssetMismatch) => {}
            Err(r) => reason = r,
        }
    }
    if q.amount.is_none() {
        reason = AskReason::UnknownAmount;
    }
    let req = PaymentApprovalRequest {
        tool_name: ctx.tool_name.clone(),
        quote: q.clone(),
        reason: reason.clone(),
        originator: ctx.originator.clone(),
    };
    match ctx.approver.approve_payment(&req).await {
        PaymentDecision::ApproveOnce => Ok(Authorized {
            mandate_id: None,
            decision: Decided::ApprovedOnce,
        }),
        PaymentDecision::Budget(g) => {
            let Some(b) = budgets else {
                return Err("payment denied: this approver cannot hold a budget".into());
            };
            let m = grant_to_mandate(&g, q);
            if let Err(r) = covers(&m, q, entries, now) {
                return Err(format!(
                    "payment denied: the budget granted does not cover it ({})",
                    r.describe(q)
                ));
            }
            let id = m.id.clone();
            b.add(m);
            Ok(Authorized {
                mandate_id: Some(id),
                decision: Decided::Budget,
            })
        }
        PaymentDecision::Deny => Err(format!("payment denied: {}", reason.describe(q))),
    }
}

/// The result a tool shows the model (and the transcript) after paying.
#[derive(Debug, Clone)]
pub struct Paid {
    pub receipt: Receipt,
    pub quote: Quote,
    pub budget_left: Option<u128>,
}

impl Paid {
    pub fn line(&self) -> String {
        let mut s = format!(
            "💳 paid {} → {} on {}{}",
            self.quote.amount_text(),
            self.quote.payee_text(),
            self.quote.network,
            if is_testnet(&self.quote.network) {
                " (testnet)"
            } else {
                ""
            }
        );
        if let Some(f) = self
            .receipt
            .fee
            .as_ref()
            .filter(|f| f.amount > 0 || f.paid_by.is_some())
        {
            s.push_str(&format!(" + {}", f.text()));
        }
        if let Some(left) = self.budget_left {
            s.push_str(&format!(
                " · budget left {} {}",
                format_units(left, self.quote.decimals),
                self.quote.asset
            ));
        }
        s.push_str(&format!(" · receipt {}", self.receipt.id));
        s
    }
}

/// The one path money moves on: quote → network check → mandate or human →
/// pay → ledger. Refuses outside an agent turn (no context, nobody to ask).
pub async fn pay_with(adapter: &dyn PaymentAdapter, req: &PaymentRequest) -> Result<Paid, String> {
    let ctx = current().ok_or("payment refused: no agent turn to authorise it")?;
    pay_in(&ctx, adapter, req).await
}

pub async fn pay_in(
    ctx: &PaymentContext,
    adapter: &dyn PaymentAdapter,
    req: &PaymentRequest,
) -> Result<Paid, String> {
    let _guard = PAY_LOCK.lock().await;
    let quote = adapter
        .quote(req)
        .await
        .map_err(|e| format!("quote failed: {e}"))?;
    if adapter.caps().payee_must_be_kyc && !adapter.payee_verified(&quote.payee).await {
        let err = format!(
            "payment refused: payee {} is not in the provider's verified directory",
            quote.payee
        );
        record(ctx, &quote, None, Decided::Denied, None, Some(err.clone()));
        return Err(err);
    }
    let entries = read_ledger(&ctx.ledger_path);
    let auth = match authorize(ctx, &quote, &entries).await {
        Ok(a) => a,
        Err(e) => {
            record(ctx, &quote, None, Decided::Denied, None, Some(e.clone()));
            return Err(e);
        }
    };
    match adapter.pay(&quote, auth.mandate_id.as_deref()).await {
        Ok(receipt) => {
            record(
                ctx,
                &quote,
                auth.mandate_id.clone(),
                auth.decision.clone(),
                Some(receipt.clone()),
                None,
            );
            let budget_left = auth.mandate_id.as_deref().and_then(|id| {
                let entries = read_ledger(&ctx.ledger_path);
                all_mandates(ctx)
                    .into_iter()
                    .find(|m| m.id == id)
                    .and_then(|m| left_on(&m, &quote, &entries, Utc::now()))
            });
            Ok(Paid {
                receipt,
                quote,
                budget_left,
            })
        }
        Err(e) => {
            let e = format!("payment failed: {e}");
            record(
                ctx,
                &quote,
                auth.mandate_id,
                auth.decision,
                None,
                Some(e.clone()),
            );
            Err(e)
        }
    }
}

/// Gate for a tool that pays by itself (an MCP server holding its own key):
/// thClaws can't move the money, only decide whether the call may run. The
/// caller reports the outcome with [`settle_external`].
pub async fn authorize_external(ctx: &PaymentContext, quote: &Quote) -> Result<Authorized, String> {
    let entries = read_ledger(&ctx.ledger_path);
    match authorize(ctx, quote, &entries).await {
        Ok(a) => Ok(a),
        Err(e) => {
            record(ctx, quote, None, Decided::Denied, None, Some(e.clone()));
            Err(e)
        }
    }
}

pub fn settle_external(
    ctx: &PaymentContext,
    quote: &Quote,
    auth: &Authorized,
    ok: Result<(), String>,
) {
    let receipt = ok.as_ref().ok().map(|_| Receipt {
        id: format!("external:{}", ctx.tool_name),
        network: quote.network.clone(),
        amount: quote.amount,
        payee: quote.payee.clone(),
        raw: Value::Null,
        fee: None,
    });
    record(
        ctx,
        quote,
        auth.mandate_id.clone(),
        auth.decision.clone(),
        receipt,
        ok.err(),
    );
}

fn all_mandates(ctx: &PaymentContext) -> Vec<Mandate> {
    let mut v = ctx.mandates.clone().unwrap_or_else(|| settings().mandates);
    if let Governance::Ee(p) = ctx.governance.clone().unwrap_or_else(governance) {
        v.push(p.mandate());
    }
    if let Some(b) = ctx.approver.payment_budgets() {
        v.extend(b.list());
    }
    v
}

fn record(
    ctx: &PaymentContext,
    quote: &Quote,
    mandate_id: Option<String>,
    decision: Decided,
    receipt: Option<Receipt>,
    error: Option<String>,
) {
    let entry = LedgerEntry {
        ts: Utc::now(),
        tool: ctx.tool_name.clone(),
        agent: Some(agent_label(&ctx.originator)),
        quote: quote.clone(),
        mandate_id,
        decision,
        receipt,
        error,
    };
    append_ledger(&ctx.ledger_path, &entry);
}

fn mandate_view(m: &Mandate, entries: &[LedgerEntry], now: DateTime<Utc>) -> Value {
    let decimals = entries
        .iter()
        .find(|e| e.mandate_id.as_deref() == Some(m.id.as_str()))
        .map(|e| e.quote.decimals)
        .unwrap_or(
            if ["USDC", "THCFI", "THCOC", "pathTHB"]
                .iter()
                .any(|a| m.asset.eq_ignore_ascii_case(a))
            {
                6
            } else {
                2
            },
        );
    let spent = spend_for(entries, &m.id, now);
    let cap = |v: &Option<String>, used: u128| {
        v.as_deref()
            .and_then(|c| parse_units(c, decimals))
            .map(|c| {
                json!({
                    "cap": format_units(c, decimals),
                    "left": format_units(c.saturating_sub(used), decimals),
                })
            })
    };
    json!({
        "id": m.id,
        "source": m.source,
        "asset": m.asset,
        "live": m.network.as_deref().is_some_and(|n| !is_testnet(n)),
        "total": cap(&m.total, spent.total),
        "daily": cap(&m.daily, spent.today),
        "monthly": cap(&m.monthly, spent.month),
        "per_tx": m.per_tx,
        "sources": m.sources,
        "payees": m.payees,
    })
}

/// What the Payments panel shows: who governs, the mandates with what is
/// left on them, and the latest ledger entries (paid and refused).
pub fn status_frame(ledger_path: &std::path::Path, budgets: &[Mandate]) -> Value {
    let entries = read_ledger(ledger_path);
    let now = Utc::now();
    let gov = governance();
    let (governed_by, mut mandates) = match &gov {
        Governance::Refused => ("ee_refused", Vec::new()),
        Governance::Ee(p) => ("ee_policy", vec![p.mandate()]),
        Governance::Open {
            ignore_config_mandates,
            ..
        } => (
            "settings",
            if *ignore_config_mandates {
                Vec::new()
            } else {
                settings().mandates
            },
        ),
    };
    mandates.extend(budgets.iter().cloned());
    let recent: Vec<Value> = entries
        .iter()
        .rev()
        .take(50)
        .map(|e| {
            json!({
                "ts": e.ts,
                "tool": e.tool,
                "agent": e.agent,
                "amount": e.quote.amount_text(),
                "payee": e.quote.payee_text(),
                "source": source_label(&e.quote),
                "network": e.quote.network,
                "testnet": is_testnet(&e.quote.network),
                "decision": e.decision,
                "paid": e.receipt.is_some(),
                "receipt": e.receipt.as_ref().map(|r| r.id.clone()),
                "fee": e.receipt.as_ref().and_then(|r| r.fee.as_ref()).filter(|f| f.paid_by.is_none()).map(Fee::text),
                "error": e.error,
            })
        })
        .collect();
    json!({
        "type": "payments_status",
        "enabled": tool_enabled(),
        "governed_by": governed_by,
        "mandates": mandates.iter().map(|m| mandate_view(m, &entries, now)).collect::<Vec<_>>(),
        "entries": recent,
    })
}

/// Quote for a settings-tagged tool that pays through its own path. Amount
/// and payee are read from conventional input fields when present.
pub fn external_quote(tool_name: &str, input: &Value) -> Quote {
    let s = |k: &[&str]| {
        k.iter().find_map(|k| {
            input.get(*k).and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_f64().map(|f| f.to_string()))
            })
        })
    };
    let asset = s(&["asset", "currency"]).unwrap_or_else(|| "USDC".into());
    let decimals = if asset.eq_ignore_ascii_case("THB") {
        2
    } else {
        6
    };
    Quote {
        adapter: format!("external:{tool_name}"),
        amount: s(&["amount", "price", "value"]).and_then(|a| parse_units(&a, decimals)),
        decimals,
        asset_id: asset.clone(),
        asset,
        payee: s(&["payee", "payTo", "pay_to", "to", "merchant"])
            .unwrap_or_else(|| "unknown".into()),
        payee_name: None,
        network: s(&["network", "chain"]).unwrap_or_else(|| "unknown".into()),
        resource: s(&["url", "resource"]),
        description: s(&["description", "purpose"]),
    }
}

/// The terminal prompt for a payment (CLI REPL).
pub fn cli_prompt(req: &PaymentApprovalRequest) -> String {
    let q = &req.quote;
    let mut s = format!(
        "\n\x1b[35m[payment] {} wants to pay {} → {} on {}{}\x1b[0m\n",
        req.tool_name,
        q.amount_text(),
        q.payee_text(),
        q.network,
        if is_testnet(&q.network) {
            " (TESTNET)"
        } else {
            ""
        }
    );
    if let Some(d) = q.description.as_deref().or(q.resource.as_deref()) {
        s.push_str(&format!("\x1b[90m  for: {d}\x1b[0m\n"));
    }
    s.push_str(&format!(
        "\x1b[90m  asking because {}\x1b[0m\n",
        req.reason.describe(q)
    ));
    s.push_str(&format!(
        "\x1b[90m[o]nce / [b]udget <total {a}> [per-payment cap] / [n]o ▸ \x1b[0m",
        a = q.asset
    ));
    s
}

/// `o` → once; `b 35` / `b 35 10` → a session budget of 35 (cap 10 each)
/// in the quote's asset; anything else denies.
pub fn parse_cli_answer(answer: &str, q: &Quote) -> PaymentDecision {
    let mut parts = answer.split_whitespace();
    match parts.next().map(str::to_ascii_lowercase).as_deref() {
        Some("o" | "once" | "y" | "yes") => PaymentDecision::ApproveOnce,
        Some("b" | "budget") => {
            let Some(total) = parts.next().and_then(|t| parse_units(t, q.decimals)) else {
                return PaymentDecision::Deny;
            };
            let per_tx = parts.next().and_then(|t| parse_units(t, q.decimals));
            PaymentDecision::Budget(BudgetGrant {
                total,
                per_tx,
                payees: None,
                expires: None,
            })
        }
        _ => PaymentDecision::Deny,
    }
}

pub fn summary_line(req: &PaymentApprovalRequest) -> String {
    let q = &req.quote;
    format!(
        "pay {} → {} on {}{}",
        q.amount_text(),
        q.payee_text(),
        q.network,
        if is_testnet(&q.network) {
            " (testnet)"
        } else {
            ""
        }
    )
}

/// What the desktop payment modal renders.
pub fn gui_payload(req: &PaymentApprovalRequest) -> Value {
    let q = &req.quote;
    serde_json::json!({
        "amount": q.amount.map(|a| format_units(a, q.decimals)),
        "asset": q.asset,
        "decimals": q.decimals,
        "payee": q.payee,
        "payee_name": q.payee_name,
        "network": q.network,
        "testnet": is_testnet(&q.network),
        "purpose": q.description.as_deref().or(q.resource.as_deref()),
        "resource": q.resource,
        "reason": req.reason.describe(q),
    })
}

/// `once` → approve once; `budget` with `{total, per_tx?}` (decimal strings
/// in the quote's asset) → a session budget; anything else denies.
pub fn parse_gui_decision(decision: &str, budget: Option<&Value>, q: &Quote) -> PaymentDecision {
    match decision {
        "once" | "allow" => PaymentDecision::ApproveOnce,
        "budget" => {
            let field = |k: &str| {
                budget
                    .and_then(|b| b.get(k))
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .or_else(|| v.as_f64().map(|f| f.to_string()))
                    })
                    .and_then(|v| parse_units(&v, q.decimals))
            };
            match field("total") {
                Some(total) if total > 0 => PaymentDecision::Budget(BudgetGrant {
                    total,
                    per_tx: field("per_tx"),
                    payees: None,
                    expires: None,
                }),
                _ => PaymentDecision::Deny,
            }
        }
        _ => PaymentDecision::Deny,
    }
}

#[cfg(test)]
mod tests;
