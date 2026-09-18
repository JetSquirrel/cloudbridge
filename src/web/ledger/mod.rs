//! The ledger, held in memory.
//!
//! The same write unit as the desktop — one account's charges for one billing
//! period, replaced as a whole — and the same read surface, but no database.
//! `crate::memory` holds the rows and does the currency conversion; this
//! module is the API the pages were already written against.

pub mod demo;
pub mod query;

use anyhow::Result;
use chrono::Utc;

use crate::memory::{self, StoredPeriod};
use crate::store::Connection;

pub use crate::model::{BalanceSnapshot, Channel, Charge, ChargeCategory, CostBasis, PeriodKey};

/// Nothing to open.
///
/// The desktop opens a file, applies its schema and points the reading view
/// at the reporting currency. The store exists from the first call, so all
/// this has to do is record which currency amounts are read in.
pub fn init_ledger(reporting_currency: &str) -> Result<()> {
    set_reporting_currency(reporting_currency)
}

/// Point the reading view at a different currency.
///
/// Cheap, and nothing is rewritten: charges keep the currency they were
/// billed in and are converted as they are read, exactly as the desktop's
/// view does.
pub fn set_reporting_currency(currency: &str) -> Result<()> {
    memory::with_store(|store| {
        *store.reporting_currency.borrow_mut() = currency.to_string();
    });

    Ok(())
}

/// Identifier for one ingest. Minted by the caller, as on the desktop.
pub fn new_batch_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Run `f` with the store's handle.
///
/// The desktop's closure gets `&mut Connection` because a write needs one.
/// Here the handle carries nothing and the rows are reached through
/// `crate::memory` inside `f`, so only the shape of the call is preserved.
fn with_connection<T>(f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
    f(&mut Connection)
}

/// The same, for the reads in [`query`], which need no transaction.
pub(crate) fn with_connection_ref<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    f(&Connection)
}

/// Replace everything stored for `key` with `charges`.
///
/// Whole-period replacement is the point: a provider re-issues a bill in full
/// and retroactively corrects prior months, so a row-by-row upsert would
/// leave behind entries it has since deleted. Dropping the period and pushing
/// another is the same guarantee, and it is why re-seeding the demo data
/// replaces rather than doubles it.
///
/// `source_ref` points at the raw payload the rows came from on the desktop.
/// Nothing is ever written here, so there is no path to keep.
pub fn replace_period(
    key: &PeriodKey,
    batch_id: &str,
    charges: &[Charge],
    _source_ref: Option<&str>,
    channel: Channel,
) -> Result<()> {
    memory::with_store(|store| {
        let mut periods = store.periods.borrow_mut();
        periods.retain(|period| period.key != *key);
        periods.push(StoredPeriod {
            key: key.clone(),
            batch_id: batch_id.to_string(),
            channel,
            charges: charges.to_vec(),
            completed_at: Utc::now(),
        });
    });

    Ok(())
}

/// Record a balance observation.
///
/// Re-observing the same instant overwrites, as the desktop's primary key on
/// `(provider, account, observed_at, currency)` does, so a repeated seed of
/// one payload is a no-op rather than a second snapshot.
pub fn record_balance(snapshot: &BalanceSnapshot) -> Result<()> {
    memory::with_store(|store| {
        let mut balances = store.balances.borrow_mut();
        balances.retain(|existing| {
            !(existing.provider == snapshot.provider
                && existing.account_id == snapshot.account_id
                && existing.observed_at == snapshot.observed_at
                && existing.currency == snapshot.currency)
        });
        balances.push(snapshot.clone());
    });

    Ok(())
}

/// Purchases derived from the balance history of one period.
///
/// A source that only reports a balance never reports a purchase: a rise in
/// the topped-up balance between two observations is the only evidence of
/// one, which is why the desktop derives them rather than storing them. The
/// demo's balances only fall, so nothing is ever derived — and nothing here
/// reads them anyway, since that derivation runs on the desktop's ingest
/// path.
pub fn top_up_charges(_key: &PeriodKey) -> Result<Vec<Charge>> {
    Ok(Vec::new())
}
