//! The domain types both data backends are written against.
//!
//! The desktop reads charges out of DuckDB and the web demo reads them out of
//! memory, but a charge is a charge either way. Keeping the shapes here — and
//! not inside either backend — is what lets the pages, the view models and
//! the alerting rules be compiled for both targets without knowing which
//! storage is underneath them.
//!
//! The same goes for what a read hands back: the summaries, forecasts and
//! findings a query returns belong to the question, not to the store that
//! answered it, so the result types live here too.

// The types mirror the FOCUS columns a bill is stored in, so they carry the
// whole vocabulary — a charge category, a cost basis — whether or not every
// path through the application constructs each variant.
#![allow(dead_code)]

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// What kind of charge a row is, in FOCUS terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeCategory {
    Usage,
    Purchase,
    Credit,
    Tax,
    Adjustment,
}

impl ChargeCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "Usage",
            Self::Purchase => "Purchase",
            Self::Credit => "Credit",
            Self::Tax => "Tax",
            Self::Adjustment => "Adjustment",
        }
    }
}

/// How much weight the amount on a row carries.
///
/// Keeps authoritative bills, unit-price-derived amounts and pure usage
/// records in one table without anyone mistaking a shadow cost for money
/// actually spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostBasis {
    /// Straight from the provider's bill.
    Authoritative,
    /// Computed from usage and a unit price.
    Derived,
    /// A projection or an allocation.
    Estimated,
    /// Usage with no amount attached; `billed_cost` is NULL.
    Absent,
}

impl CostBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Derived => "derived",
            Self::Estimated => "estimated",
            Self::Absent => "absent",
        }
    }
}

/// How a period's rows reached the ledger.
///
/// Recorded because the two channels are not interchangeable. A bill export
/// the user imported is the provider's own bill at instance level; an
/// automatic API refresh of the same month is coarser, and would replace it
/// with less.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Fetched from the provider's billing API.
    Api,
    /// Imported from a bill export the user downloaded.
    File,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::File => "file",
        }
    }

    /// The channel a stored value names.
    ///
    /// Anything unrecognized — including the NULL a row written before the
    /// column existed reads as — is an API fetch, which is what those rows
    /// were.
    pub fn from_stored(value: Option<&str>) -> Self {
        match value {
            Some("file") => Self::File,
            _ => Self::Api,
        }
    }
}

/// The unit of replacement: one account's charges for one billing period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodKey {
    /// Registry `SourceId`, stored verbatim.
    pub provider: String,
    /// Our `cloud_accounts.id`, not the provider-side account number.
    pub account_id: String,
    /// `YYYY-MM`.
    pub billing_period: String,
}

impl PeriodKey {
    pub fn new(
        provider: impl Into<String>,
        account_id: impl Into<String>,
        billing_period: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            account_id: account_id.into(),
            billing_period: billing_period.into(),
        }
    }
}

/// One row of `fct_charge`, minus the columns that come from the
/// [`PeriodKey`] it is written under.
#[derive(Debug, Clone)]
pub struct Charge {
    pub charge_period_start: DateTime<Utc>,
    pub charge_period_end: DateTime<Utc>,
    pub charge_category: ChargeCategory,
    pub cost_basis: CostBasis,
    pub billing_currency: String,
    /// Provider-side account, when it differs from the credential's own
    /// (an AWS payer account reports its linked accounts).
    pub billing_account_id: Option<String>,
    pub charge_description: Option<String>,
    pub service_name: Option<String>,
    pub service_category: Option<String>,
    pub resource_id: Option<String>,
    pub resource_name: Option<String>,
    pub region_id: Option<String>,
    pub billed_cost: Option<f64>,
    pub effective_cost: Option<f64>,
    pub list_cost: Option<f64>,
    pub pricing_quantity: Option<f64>,
    pub pricing_unit: Option<String>,
    /// JSON object text, or `None` when the source reports no tags.
    pub tags: Option<String>,
}

impl Charge {
    /// An authoritative usage charge with everything optional left unset.
    /// Fill the rest in with struct update syntax.
    pub fn new(
        charge_period_start: DateTime<Utc>,
        charge_period_end: DateTime<Utc>,
        billing_currency: impl Into<String>,
    ) -> Self {
        Self {
            charge_period_start,
            charge_period_end,
            charge_category: ChargeCategory::Usage,
            cost_basis: CostBasis::Authoritative,
            billing_currency: billing_currency.into(),
            billing_account_id: None,
            charge_description: None,
            service_name: None,
            service_category: None,
            resource_id: None,
            resource_name: None,
            region_id: None,
            billed_cost: None,
            effective_cost: None,
            list_cost: None,
            pricing_quantity: None,
            pricing_unit: None,
            tags: None,
        }
    }
}

/// A point-in-time balance for a source that reports state rather than
/// charges.
#[derive(Debug, Clone)]
pub struct BalanceSnapshot {
    pub provider: String,
    pub account_id: String,
    pub observed_at: DateTime<Utc>,
    pub balance: f64,
    pub granted_balance: Option<f64>,
    pub topped_up_balance: Option<f64>,
    pub currency: String,
}

/// A calendar month of billing, the unit providers issue a bill in and the
/// unit the ledger replaces as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingPeriod {
    pub year: i32,
    pub month: u32,
}

impl BillingPeriod {
    pub fn new(year: i32, month: u32) -> Self {
        Self { year, month }
    }

    /// The period the given instant falls in.
    pub fn containing(instant: DateTime<Utc>) -> Self {
        Self::new(instant.year(), instant.month())
    }

    /// The period before this one.
    pub fn previous(&self) -> Self {
        if self.month == 1 {
            Self::new(self.year - 1, 12)
        } else {
            Self::new(self.year, self.month - 1)
        }
    }

    /// `YYYY-MM`, as stored in `billing_period` and in the raw path.
    pub fn label(&self) -> String {
        format!("{:04}-{:02}", self.year, self.month)
    }

    /// First day of the period.
    pub fn start(&self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.year, self.month, 1).expect("a valid billing period")
    }

    /// First day of the following period. Cost Explorer and the BSS API
    /// both take an exclusive end.
    pub fn end_exclusive(&self) -> NaiveDate {
        let (year, month) = if self.month == 12 {
            (self.year + 1, 1)
        } else {
            (self.year, self.month + 1)
        };
        NaiveDate::from_ymd_opt(year, month, 1).expect("a valid billing period")
    }
}

/// Identifier of a billing source.
///
/// Persisted verbatim in the `cloud_accounts` table, so these strings are
/// part of the on-disk format and must not be renamed without a migration.
///
/// [`SourceId::descriptor`] resolves against whichever source registry the
/// target compiled — see `cloud::registry` — so it is implemented there
/// rather than here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(String);

impl SourceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SourceId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

impl From<String> for SourceId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// What a source reports, and therefore what there is to refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reporting {
    /// Cost accrued over a period. Refreshing means re-fetching the current
    /// billing period and, for a while after it ends, the one before it.
    Periodic,
    /// A point-in-time balance. There is no period cost and no history to
    /// backfill, so a refresh reads the balance as it stands.
    Snapshot,
}

/// Cloud account information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudAccount {
    /// Account ID
    pub id: String,
    /// Account name (user-defined)
    pub name: String,
    /// Billing source this account belongs to; see `cloud::registry`.
    pub source_id: SourceId,
    /// Region (optional)
    pub region: Option<String>,
    /// Created time
    pub created_at: DateTime<Utc>,
    /// Last synced time
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Is enabled
    pub enabled: bool,
    /// The first characters of the access key, kept in the database so a
    /// list of accounts can be shown without reading the keyring — see
    /// [`access_key_hint`]. `None` for an account stored before the hint
    /// was recorded.
    pub access_key_hint: Option<String>,
    /// Where the provider's billing export lands (`s3://bucket/prefix` for
    /// an AWS Data Exports-backed account). `None` for an account read by
    /// its billing API, and for one stored before exports were supported.
    pub export_uri: Option<String>,
}

/// How much of an access key is kept as a hint.
const HINT_CHARS: usize = 8;

/// The part of an access key worth keeping in the clear: enough to tell two
/// accounts apart in a list, never enough to authenticate with.
///
/// The secret half is never hinted at, at any length.
pub fn access_key_hint(access_key: &str) -> String {
    access_key.chars().take(HINT_CHARS).collect()
}

impl CloudAccount {
    /// The access key as the UI shows it, or `None` for an account whose
    /// hint was never recorded.
    ///
    /// Reading the key itself would mean a keyring prompt, which is not
    /// something a list of accounts should cost; see
    /// [`crate::db::account_context`].
    pub fn masked_access_key(&self) -> Option<String> {
        self.access_key_hint
            .as_ref()
            .map(|hint| format!("{}****", hint))
    }
}

/// Budget information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetInfo {
    /// Account ID
    pub account_id: String,
    /// Monthly budget amount
    pub monthly_budget: f64,
    /// Currency
    pub currency: String,
    /// Alert threshold (percentage, e.g., 80.0 for 80%)
    pub alert_threshold: f64,
    /// Created time
    pub created_at: DateTime<Utc>,
    /// Updated time
    pub updated_at: DateTime<Utc>,
}

/// Budget status (comparison of budget vs actual)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetStatus {
    /// Account ID
    pub account_id: String,
    /// Account name
    pub account_name: String,
    /// Monthly budget
    pub monthly_budget: f64,
    /// Current month actual cost
    pub current_cost: f64,
    /// Currency
    pub currency: String,
    /// Percentage used (0-100+)
    pub percentage_used: f64,
    /// Remaining budget (can be negative if over budget)
    pub remaining: f64,
    /// Whether alert threshold is exceeded
    pub alert_triggered: bool,
}

/// The most recent balance a source reported for an account.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub balance: f64,
    pub granted_balance: Option<f64>,
    pub topped_up_balance: Option<f64>,
    /// The currency the source reports in, which is not converted: a
    /// balance is what is left in an account, not an amount spent.
    pub currency: String,
    pub observed_at: DateTime<Utc>,
}

/// One day's charges, as `(YYYY-MM-DD, amount)` in the reporting currency.
pub type DailyTotal = (String, f64);

/// One service's charges on one day, in the reporting currency.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceDailyTotal {
    pub provider: String,
    /// `coalesce(service_name, 'Other')`, as everywhere a service is grouped.
    pub service: String,
    /// `YYYY-MM-DD`.
    pub day: String,
    pub amount: f64,
}

/// One of the largest charges of a period that carries no value for a tag.
#[derive(Debug, Clone, PartialEq)]
pub struct UntaggedCharge {
    pub provider: String,
    pub service: Option<String>,
    pub description: Option<String>,
    /// In the reporting currency.
    pub amount: f64,
}

/// Untagged Usage charges of a period rolled up to one `(provider,
/// service)` row — what the Unallocated explainer card lists.
#[derive(Debug, Clone, PartialEq)]
pub struct UntaggedServiceUsage {
    pub provider: String,
    pub service: Option<String>,
    /// In the reporting currency.
    pub amount: f64,
}

/// The run-rate forecast for a billing period: what the month costs at the
/// pace seen so far (the OptScale run-rate model).
#[derive(Debug, Clone, PartialEq)]
pub struct PeriodForecast {
    /// Charged so far this period, in the reporting currency.
    pub month_to_date: f64,
    /// `month_to_date` spread over the days since the baseline.
    pub daily_rate: f64,
    /// `month_to_date + daily_rate * days remaining`. A past period
    /// forecasts its own total; a period with no charges yet forecasts 0.
    pub forecast: f64,
    pub days_elapsed: i64,
    pub days_in_month: i64,
}

/// A billing period against the one before it, from a single pass over the
/// ledger — the OptScale pattern of querying one window covering both
/// periods and splitting each bucket by which side it falls on.
#[derive(Debug, Clone, PartialEq)]
pub struct PeriodOverPeriod {
    /// Net totals across every charge category, as in
    /// [`crate::ledger::query::total_for_period`].
    pub current_total: f64,
    pub previous_total: f64,
    /// `(service, amount)` across providers, largest first; a net-negative
    /// service is dropped, as in `provider_service_totals`.
    pub current_by_service: Vec<(String, f64)>,
    pub previous_by_service: Vec<(String, f64)>,
}

/// How much of a period-over-period delta one charge category explains.
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryDelta {
    pub category: String,
    pub current: f64,
    pub previous: f64,
    pub delta: f64,
}

/// The direction one service moved between two periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovementKind {
    /// No charges in the previous period, some in the current.
    Appeared,
    /// Charges in the previous period, none in the current.
    Vanished,
    Grown,
    Shrunk,
}

/// How much of a period-over-period delta one service explains.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceMovement {
    /// `coalesce(service_name, 'Other')`, as everywhere a service is grouped.
    pub service: String,
    pub kind: MovementKind,
    pub current: f64,
    pub previous: f64,
    pub delta: f64,
}

/// A period's change against the one before it, decomposed two ways, with
/// the reconciliation check Wealthfolio applies to its attribution: the
/// components must add back up to the total they claim to explain.
#[derive(Debug, Clone, PartialEq)]
pub struct CostChangeDecomposition {
    pub billing_period: String,
    pub previous_period: String,
    pub current_total: f64,
    pub previous_total: f64,
    /// `current_total - previous_total`: the number being explained.
    pub total_delta: f64,
    /// The delta split by charge category, largest absolute first — the
    /// partition `residual` is measured against.
    pub by_category: Vec<CategoryDelta>,
    /// The same delta split by service, largest absolute first — a second,
    /// independent partition, for the "what grew" view.
    pub by_service: Vec<ServiceMovement>,
    /// `total_delta` minus what `by_category` explains. On data written by
    /// one code path this is floating-point noise; a real gap means money
    /// in a bucket the decomposition does not know about.
    pub residual: f64,
    /// `|residual|` within the tolerance of `reconcile`.
    pub reconciled: bool,
}

/// The run-rate forecast with a band around it, derived from the spread of
/// the daily costs seen so far — the simplified version of Wealthfolio's
/// Monte-Carlo percentile bands: one standard deviation each way instead of
/// simulated paths.
#[derive(Debug, Clone, PartialEq)]
pub struct ForecastBands {
    /// Charged so far this period, in the reporting currency.
    pub month_to_date: f64,
    /// The point forecast, as [`crate::ledger::query::forecast_for_period`]
    /// computes it.
    pub expected: f64,
    /// `month_to_date + (daily_mean + daily_stddev) * remaining days`.
    pub optimistic: f64,
    /// `month_to_date + max(daily_mean - daily_stddev, 0) * remaining days`.
    pub pessimistic: f64,
    /// Mean daily cost over the days since the baseline — the same value as
    /// [`PeriodForecast::daily_rate`].
    pub daily_mean: f64,
    /// Sample standard deviation of the daily costs; 0 with fewer than two
    /// days of data.
    pub daily_stddev: f64,
}

/// How bad a data-quality issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Info,
    Warning,
    Critical,
}

/// Which check raised an issue — stable identifiers the UI can key on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataQualityKind {
    /// Charges no FX rate covers: missing from every converted total.
    UnconvertedCharges,
    /// Usage carrying no value for the tag the attribution page groups by.
    UntaggedUsage,
    /// Usage of a service that carries no region.
    MissingRegion,
    /// `cloud::deduction`'s escape hatch: a bill line whose named
    /// deductions did not add up.
    UnreconciledAdjustment,
}

impl DataQualityKind {
    /// Stable snake_case identifier. Stored in the app-state database as
    /// part of a dismissal key, so these strings are part of the on-disk
    /// format.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnconvertedCharges => "unconverted_charges",
            Self::UntaggedUsage => "untagged_usage",
            Self::MissingRegion => "missing_region",
            Self::UnreconciledAdjustment => "unreconciled_adjustment",
        }
    }
}

/// One finding of the ledger's health check — the Wealthfolio Health
/// Center pattern: the warnings attach to the period every metric is read
/// from, rather than living in a separate report nobody opens.
#[derive(Debug, Clone, PartialEq)]
pub struct DataQualityIssue {
    pub kind: DataQualityKind,
    pub severity: IssueSeverity,
    /// User-readable, with the numbers in it.
    pub message: String,
    /// Reporting-currency amount behind the issue; `None` for unconverted
    /// charges, whose amounts are in currencies that cannot be summed.
    pub affected_amount: Option<f64>,
    pub affected_count: i64,
}

impl DataQualityIssue {
    /// The key a dismissal is stored under: `{kind}:{billing_period}`.
    /// Amounts and counts are deliberately not in it — an issue dismissed
    /// for a period stays dismissed however its numbers move, and a kind
    /// emitted per service (MissingRegion) dismisses as one finding for
    /// the period.
    pub fn dismissal_key(&self, billing_period: &str) -> String {
        format!("{}:{}", self.kind.as_str(), billing_period)
    }
}

/// A stored dimension of `fct_charge` to break a period down by. Region
/// and service category are ingested with every charge but were never
/// grouped on until now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakdownDim {
    /// `coalesce(service_name, 'Other')`, as everywhere a service is grouped.
    Service,
    Region,
    ServiceCategory,
}

/// One of a period's costliest resources.
#[derive(Debug, Clone, PartialEq)]
pub struct TopResource {
    pub resource_id: String,
    /// `any_value` of the group: name and service are stable per resource
    /// id, so grouping by id alone does not split a resource whose display
    /// name the provider reissued.
    pub resource_name: Option<String>,
    pub service: String,
    /// In the reporting currency.
    pub amount: f64,
}

/// One `(provider, service, tag_value)` usage bucket of a period.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceTagUsage {
    pub provider: String,
    /// `coalesce(service_name, 'Other')`, as everywhere a service is grouped.
    pub service: String,
    /// `'Unallocated'` for a charge with no value for the tag key, as in
    /// [`crate::ledger::query::tag_usage_breakdown`].
    pub tag_value: String,
    /// In the reporting currency.
    pub amount: f64,
}

/// The result of an ad-hoc query, with every value already rendered as
/// text — the query page shows it without touching duckdb types.
#[derive(Debug, Clone, PartialEq)]
pub struct AdhocResult {
    pub columns: Vec<String>,
    /// Per column: whether to right-align, decided by the first non-NULL
    /// value seen — a column of nothing but NULLs left-aligns.
    pub numeric: Vec<bool>,
    /// A NULL stays `None`, so the UI can grey it rather than print "NULL".
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
    pub elapsed_ms: u64,
}
