//! Fetching a bill, in a browser that cannot fetch one.
//!
//! Refreshing means signing a request with a credential from the keyring;
//! importing means reading a file the user picked. The web demo has neither,
//! and its accounts are all demo accounts — the same ones the desktop
//! deliberately skips — so there is nothing here for a refresh to do.
//!
//! Every function therefore answers in the terms the pages already
//! understand: a refresh that lands nothing, a replay with nothing to replay,
//! and an import that says why it cannot.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

use crate::model::{BillingPeriod, CloudAccount, PeriodKey};

/// The ledger key an account's period is stored under.
pub fn period_key(account: &CloudAccount, period: &BillingPeriod) -> PeriodKey {
    PeriodKey::new(
        account.source_id.as_str().to_string(),
        account.id.clone(),
        period.label(),
    )
}

/// What one ingest did, for logging and for the UI to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub batch_id: String,
    pub charges: usize,
    pub balances: usize,
    /// Where the raw payloads were written. Nothing is written here, so this
    /// is empty rather than a path that does not exist.
    pub raw_path: PathBuf,
}

/// What one refresh of an account did.
#[derive(Debug, Default)]
pub struct RefreshOutcome {
    /// Each period fetched and landed, as `(period label, outcome)`.
    pub ingested: Vec<(String, IngestOutcome)>,
    /// Periods left alone because they were ingested recently enough.
    pub skipped_fresh: Vec<String>,
}

/// What re-normalizing every stored payload did.
#[derive(Debug, Default)]
pub struct ReplayOutcome {
    /// Periods re-normalized.
    pub periods: usize,
    /// Charges written across them.
    pub charges: usize,
}

/// What importing a bill file did.
#[derive(Debug, Default)]
pub struct ImportOutcome {
    /// The format the file was read as, for the message the UI shows.
    pub format: &'static str,
    /// Each billing period the file covered, oldest first.
    pub periods: Vec<(String, IngestOutcome)>,
}

impl ImportOutcome {
    /// Total rows written across every period the file covered.
    pub fn charges(&self) -> usize {
        self.periods
            .iter()
            .map(|(_, outcome)| outcome.charges)
            .sum()
    }

    /// The months the file covered, as the import message lists them.
    pub fn period_labels(&self) -> String {
        self.periods
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Bring an account's ledger up to date.
///
/// Nothing to bring: the demo's ledger is seeded, not fetched. Reported the
/// way the desktop reports a demo account — a refresh that ingested no
/// periods and skipped none — so the button behaves identically in both
/// builds instead of erroring in one of them.
pub fn refresh_account(_account: &CloudAccount, _force: bool) -> Result<RefreshOutcome> {
    Ok(RefreshOutcome::default())
}

/// Re-normalize every stored payload without fetching anything.
///
/// There are no stored payloads: nothing was ever fetched or imported, so
/// there is nothing to re-read.
pub fn replay_all() -> Result<ReplayOutcome> {
    Ok(ReplayOutcome::default())
}

/// Read a bill export the user picked.
///
/// Refused rather than faked: the demo's ledger already holds the twelve
/// months it is meant to show, and a browser has no file to pick anyway.
pub fn import_bill_file(_account: &CloudAccount, _path: &Path) -> Result<ImportOutcome> {
    Err(anyhow!(
        "The web demo reads no bill files — it ships with demo data already loaded"
    ))
}
