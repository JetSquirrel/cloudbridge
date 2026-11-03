//! Month-end snapshots, held in memory. The same surface as the desktop's
//! `ledger::snapshot`: a freeze clones the period's lines, so a later
//! `replace_period` has nothing to reach. The arithmetic is shared, in
//! [`crate::analytics::snapshot`].

use anyhow::Result;
use chrono::Utc;

use crate::analytics::snapshot::{self, BatchSource};
use crate::memory::{self, Store, StoredSnapshot};
use crate::model::{BillingSnapshot, FreezeOutcome, PeriodKey, SnapshotLine};

/// Freeze a period's current rows as a new revision.
pub fn freeze_period(key: &PeriodKey) -> Result<FreezeOutcome> {
    memory::with_store(|store| {
        let lines = lines_of(store, key);
        if lines.is_empty() {
            return Ok(FreezeOutcome::Empty);
        }

        let latest = latest_of(store, key);
        if let Some(latest) = &latest {
            if latest.checksum == snapshot::checksum(&lines) {
                return Ok(FreezeOutcome::Unchanged(latest.clone()));
            }
        }

        let source = source_of(store, key);
        let frozen = snapshot::build(
            key,
            latest.map_or(1, |latest| latest.revision + 1),
            &lines,
            source,
            &store.reporting_currency(),
            Utc::now(),
        )?;
        store.snapshots.borrow_mut().push(StoredSnapshot {
            snapshot: frozen.clone(),
            lines,
        });
        Ok(FreezeOutcome::Frozen(frozen))
    })
}

/// A period's revisions, newest first.
pub fn snapshots_of(key: &PeriodKey) -> Result<Vec<BillingSnapshot>> {
    memory::with_store(|store| {
        let mut revisions: Vec<BillingSnapshot> = store
            .snapshots
            .borrow()
            .iter()
            .filter(|stored| stored.snapshot.key == *key)
            .map(|stored| stored.snapshot.clone())
            .collect();
        revisions.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.revision));
        Ok(revisions)
    })
}

/// The lines frozen under one snapshot.
pub fn snapshot_lines(snapshot_id: &str) -> Result<Vec<SnapshotLine>> {
    memory::with_store(|store| {
        Ok(store
            .snapshots
            .borrow()
            .iter()
            .find(|stored| stored.snapshot.snapshot_id == snapshot_id)
            .map(|stored| stored.lines.clone())
            .unwrap_or_default())
    })
}

/// The lines the ledger holds for a period now.
pub fn current_lines(key: &PeriodKey) -> Result<Vec<SnapshotLine>> {
    memory::with_store(|store| Ok(lines_of(store, key)))
}

/// Where the ledger's current rows for a period came from.
pub fn current_source(key: &PeriodKey) -> Result<BatchSource> {
    memory::with_store(|store| Ok(source_of(store, key)))
}

/// The periods an account has rows or a snapshot for, newest first.
pub fn periods_of(account_id: &str) -> Result<Vec<PeriodKey>> {
    memory::with_store(|store| {
        let mut keys: Vec<PeriodKey> = store
            .periods
            .borrow()
            .iter()
            .filter(|period| period.key.account_id == account_id && !period.charges.is_empty())
            .map(|period| period.key.clone())
            .chain(
                store
                    .snapshots
                    .borrow()
                    .iter()
                    .filter(|stored| stored.snapshot.key.account_id == account_id)
                    .map(|stored| stored.snapshot.key.clone()),
            )
            .collect();
        keys.sort_by(|a, b| b.billing_period.cmp(&a.billing_period));
        keys.dedup_by(|a, b| a.billing_period == b.billing_period);
        Ok(keys)
    })
}

fn source_of(store: &Store, key: &PeriodKey) -> BatchSource {
    store
        .periods
        .borrow()
        .iter()
        .find(|period| period.key == *key)
        .map_or_else(BatchSource::unknown, |period| BatchSource {
            batch_id: Some(period.batch_id.clone()),
            channel: period.channel,
            source_ref: None,
            completed_at: Some(period.completed_at),
        })
}

fn latest_of(store: &Store, key: &PeriodKey) -> Option<BillingSnapshot> {
    store
        .snapshots
        .borrow()
        .iter()
        .filter(|stored| stored.snapshot.key == *key)
        .max_by_key(|stored| stored.snapshot.revision)
        .map(|stored| stored.snapshot.clone())
}

fn lines_of(store: &Store, key: &PeriodKey) -> Vec<SnapshotLine> {
    let reporting = store.reporting_currency();
    store
        .periods
        .borrow()
        .iter()
        .find(|period| period.key == *key)
        .map(|period| {
            snapshot::charge_ids(key, &period.charges)
                .into_iter()
                .zip(&period.charges)
                .map(|(charge_id, charge)| SnapshotLine {
                    charge_id,
                    fx_rate: memory::rate_for(charge, &reporting),
                    charge: charge.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}
