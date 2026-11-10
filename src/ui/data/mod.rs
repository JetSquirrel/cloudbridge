//! The real-data view-model layer behind the redesigned pages.
//!
//! Every loader is a blocking `pub fn load_*() -> Result<...>`; pages wrap
//! them in `smol::unblock` themselves (see `accounts.rs` for the pattern).
//! The structs mirror `mock.rs` field for field where the mock had the
//! right shape, with owned `String`s in place of `&'static str`.
//!
//! Empty-state semantics: on a fresh or empty ledger every loader returns
//! zeros and empty vectors, never an error. The pages render their empty
//! states from these.
//!
//! Usage vs net: headline amounts stay net (`billed_cost_base` summed
//! across charge categories), but every ranking, chart, and percentage is
//! computed on gross usage (`charge_category = 'Usage'`) through the
//! `*_usage` ledger queries — credits can net an account to ≈ $0, and UI
//! math on that base is noise.

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use gpui_kit::SharedString;
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::fmt;
use crate::alerts::{self, AlertKind, AlertStatus, AlertView, RuleView};
use crate::analytics;
use crate::cloud::registry;
use crate::cloud::{BillingPeriod, BudgetInfo, BudgetStatus, CloudAccount};
use crate::ledger::query;
use crate::{db, ingest};

/// The tag that maps a charge to a business line.
///
/// Charges carry tags as a JSON object; the value under this key is what
/// the Overview "Where it went" rows, the Attribution Sankey's last hop,
/// and the unallocated-share rule all group by. A charge without it counts
/// as "Unallocated" everywhere.
pub const BUSINESS_LINE_TAG: &str = "business_line";

/// The label every breakdown gives usage that carries no
/// [`BUSINESS_LINE_TAG`] value.
pub const UNALLOCATED: &str = "Unallocated";

/// The reporting currency every amount below is expressed in, so a page
/// can format without asking config again.
pub fn reporting_currency() -> String {
    crate::config::load_config()
        .map(|settings| settings.reporting_currency)
        .unwrap_or_else(|_| crate::config::DEFAULT_REPORTING_CURRENCY.to_string())
}

mod account_detail;
mod accounts;
mod alerting;
mod attribution;
mod budgets;
mod models;
mod overview;
mod query_templates;
mod sidebar;

pub use account_detail::*;
pub use accounts::*;
pub use alerting::*;
pub use attribution::*;
pub use budgets::*;
pub use models::*;
pub use overview::*;
pub use query_templates::*;
pub use sidebar::*;

#[cfg(test)]
mod tests;
