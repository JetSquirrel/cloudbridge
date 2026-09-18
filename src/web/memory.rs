//! The web demo's data layer.
//!
//! The desktop keeps the ledger in one DuckDB file and application state in
//! another. A page in a browser has neither, so both live here as plain
//! vectors. Everything above — the query surface, the view models, the
//! alerting rules — is the desktop's own code, so what this has to reproduce
//! is the *shape* of what the two databases returned, not a database.
//!
//! The one genuinely load-bearing piece is [`normalized`]: it is the web
//! build's `v_charge_normalized`, and therefore the only place a charge is
//! converted into the reporting currency. Everything that reads an amount
//! goes through it, exactly as every read on the desktop goes through the
//! view.

use std::cell::RefCell;

use chrono::{DateTime, NaiveDate, Utc};

use crate::alerts::{AlertEvent, AlertRule};
use crate::model::{
    BalanceSnapshot, BillingPeriod, BudgetInfo, Channel, Charge, ChargeCategory, CloudAccount,
    PeriodKey,
};

/// Rates shipped with the build, as `(from, to, date, rate)`.
///
/// The same two the desktop seeds into `dim_fx_rate` — only the currencies
/// the demo's sources bill in are covered, so anything else reads as
/// unconverted, which is what the data-quality strip is for.
pub(crate) const BUILTIN_RATES: &[(&str, &str, &str, f64)] = &[
    ("USD", "CNY", "2026-01-01", 7.10),
    ("CNY", "USD", "2026-01-01", 0.1408),
];

/// One billing period's rows, as a `replace_period` left them.
///
/// Whole-period replacement is the ledger's write unit on both backends, so
/// the store keeps the same grain: replacing is dropping one entry and
/// pushing another, and nothing can leave a half-updated month behind.
pub struct StoredPeriod {
    pub key: PeriodKey,
    pub batch_id: String,
    pub channel: Channel,
    pub charges: Vec<Charge>,
    pub completed_at: DateTime<Utc>,
}

/// Everything the two desktop databases hold between them.
///
/// Each field carries its own `RefCell` rather than the store carrying one:
/// the alerting rules read the ledger and write the alert centre, and both
/// are here, so a single outer borrow would deadlock against itself.
pub struct Store {
    pub accounts: RefCell<Vec<CloudAccount>>,
    pub budgets: RefCell<Vec<BudgetInfo>>,
    pub alert_rules: RefCell<Vec<AlertRule>>,
    pub alert_events: RefCell<Vec<AlertEvent>>,
    pub dismissed_quality_keys: RefCell<Vec<String>>,
    pub periods: RefCell<Vec<StoredPeriod>>,
    pub balances: RefCell<Vec<BalanceSnapshot>>,
    pub reporting_currency: RefCell<String>,
}

impl Store {
    fn new() -> Self {
        Self {
            accounts: RefCell::new(Vec::new()),
            budgets: RefCell::new(Vec::new()),
            alert_rules: RefCell::new(Vec::new()),
            alert_events: RefCell::new(Vec::new()),
            dismissed_quality_keys: RefCell::new(Vec::new()),
            periods: RefCell::new(Vec::new()),
            balances: RefCell::new(Vec::new()),
            reporting_currency: RefCell::new(crate::config::DEFAULT_REPORTING_CURRENCY.to_string()),
        }
    }

    pub fn reporting_currency(&self) -> String {
        self.reporting_currency.borrow().clone()
    }
}

thread_local! {
    static STORE: Store = Store::new();
}

/// Run `f` against the store.
///
/// Nested calls are fine and are what the alerting path does — the rules read
/// the ledger and write events in one pass.
pub fn with_store<T>(f: impl FnOnce(&Store) -> T) -> T {
    STORE.with(f)
}

/// One charge as the reading view exposes it: the same row, with its amount
/// also expressed in the reporting currency.
///
/// Owned rather than borrowing from the store. The rows come out of a
/// `RefCell` guard, and a guard cannot be handed back to the caller, so a row
/// that borrowed from one could not outlive the call that read it anyway. The
/// cost is a few strings per charge, which a ledger of a few thousand rows
/// does not notice.
#[derive(Debug, Clone)]
pub struct NormalizedRow {
    pub provider: String,
    pub account_id: String,
    pub billing_period: String,
    pub charge_description: Option<String>,
    pub service_name: Option<String>,
    pub service_category: Option<String>,
    pub region_id: Option<String>,
    pub resource_id: Option<String>,
    pub resource_name: Option<String>,
    pub pricing_unit: Option<String>,
    pub charge_category: ChargeCategory,
    pub tags: Option<String>,
    pub billing_currency: String,
    pub charge_period_start: DateTime<Utc>,
    /// The rate this row converts at. `None` when no rate covers it — a
    /// charge already in the reporting currency converts at `1.0`, not at
    /// nothing.
    pub fx_rate: Option<f64>,
    pub billed_cost: Option<f64>,
    pub billed_cost_base: Option<f64>,
    pub effective_cost_base: Option<f64>,
}

/// Every charge in the store, converted — the web build's
/// `v_charge_normalized`.
///
/// Sums over these rows must keep ignoring `None`, as DuckDB's `sum` ignores
/// NULL: a charge no rate covers is left out of a converted total rather than
/// counted at par.
pub fn normalized(store: &Store) -> Vec<NormalizedRow> {
    let reporting = store.reporting_currency.borrow().clone();
    let periods = store.periods.borrow();

    let mut rows = Vec::new();
    for period in periods.iter() {
        for charge in &period.charges {
            let fx_rate = rate_for(charge, &reporting);
            rows.push(NormalizedRow {
                provider: period.key.provider.clone(),
                account_id: period.key.account_id.clone(),
                billing_period: period.key.billing_period.clone(),
                charge_description: charge.charge_description.clone(),
                service_name: charge.service_name.clone(),
                service_category: charge.service_category.clone(),
                region_id: charge.region_id.clone(),
                resource_id: charge.resource_id.clone(),
                resource_name: charge.resource_name.clone(),
                pricing_unit: charge.pricing_unit.clone(),
                charge_category: charge.charge_category,
                tags: charge.tags.clone(),
                billing_currency: charge.billing_currency.clone(),
                charge_period_start: charge.charge_period_start,
                fx_rate,
                billed_cost: charge.billed_cost,
                billed_cost_base: charge.billed_cost.zip(fx_rate).map(|(v, r)| v * r),
                effective_cost_base: charge.effective_cost.zip(fx_rate).map(|(v, r)| v * r),
            });
        }
    }

    rows
}

/// The rate a charge converts at, or `None` when no rate covers it.
///
/// The view's ASOF join: a charge takes the newest rate dated on or before
/// the charge itself, never a later one. A charge already in the reporting
/// currency needs no rate at all.
fn rate_for(charge: &Charge, reporting: &str) -> Option<f64> {
    if charge.billing_currency == reporting {
        return Some(1.0);
    }

    let charged_on = charge.charge_period_start.date_naive();
    BUILTIN_RATES
        .iter()
        .filter(|(from, to, _, _)| *from == charge.billing_currency && *to == reporting)
        .filter_map(|(_, _, date, rate)| {
            let dated = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
            (dated <= charged_on).then_some((dated, *rate))
        })
        .max_by_key(|(dated, _)| *dated)
        .map(|(_, rate)| rate)
}

/// The tag value one charge reaches a business line through, or
/// `'Unallocated'`.
///
/// Mirrors the view's `coalesce(nullif(json_extract_string(tags, ?), ''),
/// 'Unallocated')`: a charge with no tags, no such key, or an empty value
/// counts toward the unallocated share.
pub fn tag_value(tags: Option<&str>, tag_key: &str) -> String {
    tags.and_then(|tags| serde_json::from_str::<serde_json::Value>(tags).ok())
        .and_then(|value| {
            value
                .get(tag_key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Unallocated".to_string())
}

/// A charge's calendar day, the grain `strftime(charge_period_start,
/// '%Y-%m-%d')` produced.
pub fn day_of(at: DateTime<Utc>) -> String {
    at.date_naive().format("%Y-%m-%d").to_string()
}

/// The billing period a charge falls in, as the ledger stores it.
pub fn period_label_of(at: DateTime<Utc>) -> String {
    BillingPeriod::containing(at).label()
}
