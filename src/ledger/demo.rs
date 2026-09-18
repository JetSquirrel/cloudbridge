//! Writing the demo bill into the ledger.
//!
//! The rows themselves — the accounts, the services, the twelve months, the
//! spike and the jitter — are [`crate::demo_data`], shared with the browser
//! build so both show the same numbers. What is here is the desktop's half:
//! the transactions that put them in DuckDB and take them out again.
//!
//! Everything demo is keyed under the `demo-` prefix — batch ids and account
//! ids — so clearing is a prefix delete and re-seeding first replaces every
//! period wholesale, making the result deterministic. Demo accounts hold no
//! credentials and are skipped by refresh, so no demo row ever reaches a real
//! API.

use anyhow::Result;
use chrono::Utc;

use super::{record_balance, replace_period, rollup, with_connection, Channel, PeriodKey};
use crate::db;
use crate::demo_data::{self, DEMO_SOURCES};

pub use crate::demo_data::DEMO_PREFIX;

/// Fill the ledger with the demo accounts, charges, and balances, replacing
/// any demo data already present. Blocking; wrap in `smol::unblock`. Returns
/// a one-line summary for the UI.
pub fn seed_demo() -> Result<String> {
    let now = Utc::now();
    let periods = demo_data::periods(now);

    for (provider, account_id, name, _currency) in DEMO_SOURCES {
        db::save_account(&demo_data::account(provider, account_id, name, now), "", "")?;
    }

    let mut charge_count = 0usize;
    for (provider, account_id, _, currency) in DEMO_SOURCES {
        let services = demo_data::services_of(provider);
        for (index, period) in periods.iter().enumerate() {
            let charges =
                demo_data::period_charges(provider, services, currency, *period, index, now);
            charge_count += charges.len();
            replace_period(
                &PeriodKey::new(*provider, *account_id, period.label()),
                &demo_data::batch_id(provider, *period),
                &charges,
                None,
                Channel::Api,
            )?;
        }
    }

    for snapshot in demo_data::balance_ladder(&periods) {
        record_balance(&snapshot)?;
    }

    for (_, account_id, _, _) in DEMO_SOURCES {
        db::mark_account_synced(account_id, now)?;
    }

    // `replace_period` bypasses the ingest hook that refreshes the rollup, so
    // recompute it in full once the seed is in. As in ingest, a failure reads
    // as stale and self-repairs on the next start.
    if let Err(e) = rollup::rebuild_all() {
        tracing::warn!("Daily rollup rebuild failed after demo seed: {}", e);
    }

    Ok(demo_data::summary(periods.len(), charge_count))
}

/// Remove every demo row and demo account. Blocking. Returns a one-line
/// summary for the UI.
pub fn clear_demo() -> Result<String> {
    let accounts = db::get_all_accounts()?
        .into_iter()
        .filter(|account| account.id.starts_with(DEMO_PREFIX))
        .collect::<Vec<_>>();
    let removed = accounts.len();
    for account in accounts {
        db::delete_account(&account.id)?;
    }

    with_connection(|conn| {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM fct_charge WHERE batch_id LIKE 'demo-%'", [])?;
        tx.execute("DELETE FROM ingest_batch WHERE batch_id LIKE 'demo-%'", [])?;
        tx.execute(
            "DELETE FROM fct_balance_snapshot WHERE account_id LIKE 'demo-%'",
            [],
        )?;
        tx.commit()?;
        Ok(())
    })?;

    // The raw deletes above bypass the ingest hook, so the rollup still holds
    // the demo rows; rebuild it from what is left.
    if let Err(e) = rollup::rebuild_all() {
        tracing::warn!("Daily rollup rebuild failed after demo clear: {}", e);
    }

    Ok(format!("Removed {removed} demo account(s) and their data"))
}
