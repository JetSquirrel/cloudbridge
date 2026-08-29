//! Ingest: fetch → persist raw → normalize → ledger.
//!
//! The order matters. Raw payloads are written *before* anything is
//! normalized, so a mapping bug costs a re-run of [`renormalize_period`]
//! rather than another round of paid API calls. Cost Explorer bills per
//! request; the payloads on disk do not.
//!
//! One run of [`ingest_period`] is one `ingest_batch` row, one raw
//! partition, and one whole-period replacement in `fct_charge` — all under
//! the same batch id.

// The dashboard still reads through the response caches; it starts calling
// this in PR6, when the ledger becomes the read path.
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

use crate::cloud::raw::{self, RawBatch};
use crate::cloud::{BillingPeriod, CloudAccount, Normalized};
use crate::config::get_raw_data_dir;
use crate::ledger::{self, PeriodKey};

/// What one ingest did, for logging and for the UI to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub batch_id: String,
    pub charges: usize,
    pub balances: usize,
    /// Where the raw payloads were written.
    pub raw_path: PathBuf,
}

/// Ingest the period that is currently accruing — the UI's "refresh now".
pub fn ingest_current_period(account: &CloudAccount) -> Result<IngestOutcome> {
    ingest_period(account, &BillingPeriod::containing(Utc::now()))
}

/// Fetch one account's billing period and land it in the ledger.
pub fn ingest_period(account: &CloudAccount, period: &BillingPeriod) -> Result<IngestOutcome> {
    let descriptor = account.descriptor().ok_or_else(|| {
        anyhow!(
            "No billing source registered under '{}'",
            account.source_id.as_str()
        )
    })?;

    let source = (descriptor.build)(account.context(descriptor));
    let parts = source.fetch(period)?;

    let batch = RawBatch {
        provider: descriptor.id.to_string(),
        account_id: account.id.clone(),
        period: *period,
        batch_id: ledger::new_batch_id(),
        fetched_at: Utc::now(),
        parts,
    };

    let raw_path = persist(&batch)?;
    let normalized = source.normalize(&batch)?;
    record(&batch, &normalized, &raw_path)
}

/// Normalize a period again from payloads already on disk.
///
/// This is the whole point of storing raw: correcting a mapping, or adding
/// a column, replays the newest batch at no cost. It fails rather than
/// silently fetching if nothing has been stored for the period yet.
pub fn renormalize_period(account: &CloudAccount, period: &BillingPeriod) -> Result<IngestOutcome> {
    let descriptor = account.descriptor().ok_or_else(|| {
        anyhow!(
            "No billing source registered under '{}'",
            account.source_id.as_str()
        )
    })?;

    let root = get_raw_data_dir()?;
    let batch_id = raw::batches(&root, descriptor.id, &account.id, period)?
        .pop()
        .ok_or_else(|| {
            anyhow!(
                "No raw payload stored for {} {} — fetch it first",
                account.name,
                period.label()
            )
        })?;

    let batch = raw::read_batch(&root, descriptor.id, &account.id, period, &batch_id)?;
    let raw_path = batch.directory(&root).join("part-0.parquet");

    let source = (descriptor.build)(account.context(descriptor));
    let normalized = source.normalize(&batch)?;
    record(&batch, &normalized, &raw_path)
}

/// Write the payloads under `raw/`, checking first that nothing in the path
/// came out of the database with a separator in it.
fn persist(batch: &RawBatch) -> Result<PathBuf> {
    raw::check_path_segment(&batch.provider, "source id")?;
    raw::check_path_segment(&batch.account_id, "account id")?;
    raw::check_path_segment(&batch.batch_id, "batch id")?;

    raw::write(&get_raw_data_dir()?, batch)
}

/// Replace the period in the ledger with what the normalizer produced.
///
/// Balances go in first. A source that reports only a balance reports no
/// purchases either, so its purchases are derived from the movement
/// between observations — including the one just made.
fn record(batch: &RawBatch, normalized: &Normalized, raw_path: &Path) -> Result<IngestOutcome> {
    let key = PeriodKey::new(
        batch.provider.clone(),
        batch.account_id.clone(),
        batch.period.label(),
    );

    for balance in &normalized.balances {
        ledger::record_balance(balance)?;
    }

    let mut charges = normalized.charges.clone();
    if !normalized.balances.is_empty() {
        // Recomputed on every ingest rather than written once, because
        // replacing the period clears whatever was there before.
        charges.extend(ledger::top_up_charges(&key)?);
    }

    ledger::replace_period(
        &key,
        &batch.batch_id,
        &charges,
        Some(&raw_path.to_string_lossy()),
    )?;

    tracing::info!(
        "Ingested {} {}: {} charge(s), {} balance(s)",
        batch.provider,
        batch.period.label(),
        charges.len(),
        normalized.balances.len()
    );

    Ok(IngestOutcome {
        batch_id: batch.batch_id.clone(),
        charges: charges.len(),
        balances: normalized.balances.len(),
        raw_path: raw_path.to_path_buf(),
    })
}
