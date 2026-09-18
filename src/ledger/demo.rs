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

    Ok(format!(
        "{} accounts, {} periods, {} charges",
        DEMO_SOURCES.len(),
        DEMO_SOURCES.len() * periods.len(),
        charge_count
    ))
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

    Ok(format!("Removed {removed} demo account(s) and their data"))
}

/// One period's charges for one source: a monthly row per service for
/// settled periods, daily rows for the current and previous period so the
/// 30-day and MTD charts have points to draw.
fn period_charges(
    provider: &str,
    services: &[(&str, f64, Option<&str>)],
    currency: &str,
    period: BillingPeriod,
    index: usize,
    now: DateTime<Utc>,
) -> Vec<Charge> {
    let current = BillingPeriod::containing(now);
    let recent = period.label() == current.label() || period.label() == current.previous().label();

    // Growth over the year, then a spike month for the model services.
    let growth = 0.62 + 0.08 * index as f64;
    let mut charges = Vec::new();

    for (service, base, line) in services {
        let mut amount = base * growth;
        if index == SPIKE_MONTH && matches!(*service, "Bedrock" | "deepseek-reasoner") {
            amount *= 2.6;
        }

        if recent {
            let mut day = period.start();
            while day < period.end_exclusive() {
                let start = day_start(day);
                if start > now {
                    break;
                }
                let mut daily = amount / 30.0 * (1.0 + jitter(service, day));
                // Three consecutive hot days right before now, so the
                // cost-anomaly rule (daily > 7-day baseline × 2.5) fires
                // against the demo data.
                if *service == "deepseek-reasoner" && (now - start).num_days() < 3 {
                    daily *= 3.4;
                }
                let Some(next) = day.succ_opt() else { break };
                charges.push(usage_charge(
                    provider,
                    service,
                    *line,
                    currency,
                    daily,
                    start,
                    day_start(next),
                ));
                day = next;
            }
        } else {
            charges.push(usage_charge(
                provider,
                service,
                *line,
                currency,
                amount,
                day_start(period.start()),
                day_start(period.end_exclusive()),
            ));
        }
    }

    Ok(format!("Removed {removed} demo account(s) and their data"))
}
