//! Writing the demo bill into the in-memory ledger.
//!
//! On the desktop the demo is a convenience laid over a real ledger; here it
//! is the ledger. A page in a browser has no database to open and no provider
//! to call, so what this seeds is what every chart, card and rule reads.
//!
//! The rows are [`crate::demo_data`], the same module the desktop seeds from
//! — same accounts, same services, same spike, same jitter — so a page
//! reviewed in the browser shows the numbers it will show on the desktop.
//! Only the writes below are this target's own.

use anyhow::Result;
use chrono::Utc;

use super::{record_balance, replace_period, with_connection, Channel, PeriodKey};
use crate::db;
use crate::demo_data::{self, DEMO_SOURCES};
use crate::memory;

pub use crate::demo_data::DEMO_PREFIX;

/// Fill the ledger with the demo accounts, charges, and balances, replacing
/// any demo data already present. Returns a one-line summary for the UI.
///
/// Every row lands in memory, so this is a loop over vectors rather than the
/// desktop's twelve transactions per account; `smol::unblock` still wraps it
/// because the call site is shared code.
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

    // The desktop rebuilds its daily rollup here, because `replace_period`
    // bypasses the ingest hook that would. Reads aggregate straight off the
    // store on this target, so there is nothing derived to refresh.

    Ok(demo_data::summary(periods.len(), charge_count))
}

/// Remove every demo row and demo account. Returns a one-line summary for the
/// UI.
pub fn clear_demo() -> Result<String> {
    let accounts = db::get_all_accounts()?
        .into_iter()
        .filter(|account| account.id.starts_with(DEMO_PREFIX))
        .collect::<Vec<_>>();
    let removed = accounts.len();
    for account in accounts {
        db::delete_account(&account.id)?;
    }

    // The desktop deletes the rows with three statements against
    // `fct_charge`, `ingest_batch` and `fct_balance_snapshot`; the same three
    // predicates are applied to the store inside the connection handle, which
    // is all that handle carries here. The prefix is what makes them prefix
    // deletes on both backends — a non-demo row added later survives this.
    with_connection(|_conn| {
        memory::with_store(|store| {
            store
                .periods
                .borrow_mut()
                .retain(|period| !period.batch_id.starts_with(DEMO_PREFIX));
            store
                .balances
                .borrow_mut()
                .retain(|snapshot| !snapshot.account_id.starts_with(DEMO_PREFIX));
        });

        Ok(())
    })?;

    Ok(format!("Removed {removed} demo account(s) and their data"))
}
