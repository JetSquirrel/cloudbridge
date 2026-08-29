//! Billing sources: accounts, the data they report, and the client trait.

pub mod aliyun;
pub mod aws;
pub mod deepseek;
pub mod raw;
pub mod registry;

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::ledger::{BalanceSnapshot, Charge};
pub use raw::{RawBatch, RawPart};
pub use registry::{SourceDescriptor, SourceId};

/// Shown in place of a source's name when its id is not in the registry.
/// Accounts like that are filtered out on load, so this is a backstop.
const UNKNOWN_SOURCE: &str = "Unknown";

/// The credentials a [`BillingSource`] is built from.
///
/// Bundled into one struct so [`SourceDescriptor::build`] can be a plain
/// function pointer. Deliberately no account id or name: a client
/// authenticates and fetches, and which account the result is filed under
/// is the ingest's business, not its own.
pub struct SourceContext {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: Option<String>,
}

/// Cloud account information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudAccount {
    /// Account ID
    pub id: String,
    /// Account name (user-defined)
    pub name: String,
    /// Billing source this account belongs to; see [`registry`].
    pub source_id: SourceId,
    /// Access Key ID (encrypted storage)
    pub access_key_id: String,
    /// Secret Access Key (encrypted storage)
    pub secret_access_key: String,
    /// Region (optional)
    pub region: Option<String>,
    /// Created time
    pub created_at: DateTime<Utc>,
    /// Last synced time
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Is enabled
    pub enabled: bool,
}

impl CloudAccount {
    /// The descriptor for this account's source, or `None` if the stored id
    /// is not registered in this build.
    pub fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        self.source_id.descriptor()
    }

    /// Short label for the source, for badges and log lines.
    pub fn short_name(&self) -> &'static str {
        self.descriptor().map_or(UNKNOWN_SOURCE, |s| s.short_name)
    }

    /// Credentials in the shape [`SourceDescriptor::build`] expects, with the
    /// source's default region filled in when the account stored none.
    pub fn context(&self, descriptor: &SourceDescriptor) -> SourceContext {
        SourceContext {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            region: descriptor.region_or_default(self.region.clone()),
        }
    }
}

/// Budget information
// TODO(v0.2.0): drop this allow once the budget UI is wired up
#[allow(dead_code)]
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
// TODO(v0.2.0): drop this allow once the budget UI is wired up
#[allow(dead_code)]
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

/// What a normalizer produces: FOCUS rows ready for the ledger.
///
/// Charges and balances are separate because a balance is state, not a
/// charge — see `fct_balance_snapshot`.
#[derive(Debug, Default)]
pub struct Normalized {
    pub charges: Vec<Charge>,
    pub balances: Vec<BalanceSnapshot>,
}

/// A source of billing data (sync, using ureq).
///
/// [`Self::fetch`] and [`Self::normalize`] are deliberately split. `fetch`
/// touches the network and interprets nothing; `normalize` interprets and
/// touches nothing. That is what makes the billing logic testable from a
/// recorded payload, and what keeps a mapping fix from costing another
/// round of paid API calls.
pub trait BillingSource: Send + Sync {
    /// Validate credentials
    fn validate_credentials(&self) -> Result<bool>;

    /// Retrieve everything the provider reports for one billing period,
    /// unchanged. The only method here that talks to the network.
    fn fetch(&self, period: &BillingPeriod) -> Result<Vec<RawPart>>;

    /// Turn a fetched batch into ledger rows. Pure: no clock, no network,
    /// no database — everything it needs is in the batch.
    fn normalize(&self, batch: &RawBatch) -> Result<Normalized>;
}
