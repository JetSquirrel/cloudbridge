//! Reading the ledger.
//!
//! Everything the UI shows comes through [`schema::NORMALIZED_VIEW`], so
//! amounts arrive already expressed in the reporting currency. Nothing in
//! here adds up two currencies.

use anyhow::Result;
use chrono::{DateTime, Utc};
use duckdb::{params, Connection};

use super::schema::{NORMALIZED_VIEW, TIMESTAMP_FORMAT};
use super::{with_connection_ref, PeriodKey};

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

/// Total charged in one billing period, in the reporting currency.
pub fn period_total(key: &PeriodKey) -> Result<f64> {
    with_connection_ref(|conn| period_total_of(conn, key))
}

/// Total charged across every account in a billing period.
///
/// This is the cross-cloud, cross-currency number: one query, one currency
/// out, no summing of amounts that were never comparable.
pub fn total_for_period(billing_period: &str) -> Result<f64> {
    with_connection_ref(|conn| total_for_period_of(conn, billing_period))
}

/// Charges of one period grouped by service, largest first.
pub fn service_breakdown(key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| service_breakdown_of(conn, key))
}

/// Daily charge totals for an account since an instant, oldest first.
pub fn daily_totals(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    with_connection_ref(|conn| daily_totals_of(conn, provider, account_id, since))
}

/// The newest balance snapshot for an account, if it reports one.
pub fn latest_balance(provider: &str, account_id: &str) -> Result<Option<Balance>> {
    with_connection_ref(|conn| latest_balance_of(conn, provider, account_id))
}

/// When a period was last ingested, or `None` if it never was.
///
/// This is what a refresh checks against its cache window: the ledger
/// records when it was written, so nothing else has to.
pub fn last_ingest(key: &PeriodKey) -> Result<Option<DateTime<Utc>>> {
    with_connection_ref(|conn| last_ingest_of(conn, key))
}

/// How many charges could not be converted, because no rate covers their
/// currency. They are missing from every converted total.
pub fn unconverted_charges(billing_period: &str) -> Result<i64> {
    with_connection_ref(|conn| unconverted_charges_of(conn, billing_period))
}

fn period_total_of(conn: &Connection, key: &PeriodKey) -> Result<f64> {
    let total: Option<f64> = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base) FROM {NORMALIZED_VIEW}
             WHERE provider = ? AND account_id = ? AND billing_period = ?"
        ),
        params![key.provider, key.account_id, key.billing_period],
        |row| row.get(0),
    )?;

    Ok(total.unwrap_or(0.0))
}

fn total_for_period_of(conn: &Connection, billing_period: &str) -> Result<f64> {
    let total: Option<f64> = conn.query_row(
        &format!("SELECT sum(billed_cost_base) FROM {NORMALIZED_VIEW} WHERE billing_period = ?"),
        params![billing_period],
        |row| row.get(0),
    )?;

    Ok(total.unwrap_or(0.0))
}

fn service_breakdown_of(conn: &Connection, key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT coalesce(service_name, 'Other') AS service, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE provider = ? AND account_id = ? AND billing_period = ?
         GROUP BY service
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let rows = stmt
        .query_map(
            params![key.provider, key.account_id, key.billing_period],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

fn daily_totals_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT strftime(charge_period_start, '%Y-%m-%d') AS day, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE provider = ? AND account_id = ? AND charge_period_start >= CAST(? AS TIMESTAMP)
         GROUP BY day
         ORDER BY day"
    ))?;

    let rows = stmt
        .query_map(
            params![
                provider,
                account_id,
                since.format(TIMESTAMP_FORMAT).to_string()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(day, amount)| (day, amount.unwrap_or(0.0)))
        .collect())
}

fn latest_balance_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
) -> Result<Option<Balance>> {
    let mut stmt = conn.prepare(
        "SELECT balance, granted_balance, topped_up_balance, currency,
                CAST(observed_at AS VARCHAR)
         FROM fct_balance_snapshot
         WHERE provider = ? AND account_id = ?
         ORDER BY observed_at DESC
         LIMIT 1",
    )?;

    let mut rows = stmt.query_map(params![provider, account_id], |row| {
        Ok((
            row.get::<_, f64>(0)?,
            row.get::<_, Option<f64>>(1)?,
            row.get::<_, Option<f64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;

    let Some(row) = rows.next().transpose()? else {
        return Ok(None);
    };
    let (balance, granted_balance, topped_up_balance, currency, observed_at) = row;

    Ok(Some(Balance {
        balance,
        granted_balance,
        topped_up_balance,
        currency,
        observed_at: super::parse_timestamp(&observed_at)?,
    }))
}

fn last_ingest_of(conn: &Connection, key: &PeriodKey) -> Result<Option<DateTime<Utc>>> {
    let mut stmt = conn.prepare(
        "SELECT CAST(max(completed_at) AS VARCHAR) FROM ingest_batch
         WHERE provider = ? AND account_id = ? AND billing_period = ? AND status = 'complete'",
    )?;

    let completed_at: Option<String> = stmt.query_row(
        params![key.provider, key.account_id, key.billing_period],
        |row| row.get(0),
    )?;

    completed_at
        .map(|stamp| super::parse_timestamp(&stamp))
        .transpose()
}

fn unconverted_charges_of(conn: &Connection, billing_period: &str) -> Result<i64> {
    conn.query_row(
        &format!(
            "SELECT count(*) FROM {NORMALIZED_VIEW}
             WHERE billing_period = ? AND billed_cost IS NOT NULL AND billed_cost_base IS NULL"
        ),
        params![billing_period],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::schema;
    use crate::ledger::{BalanceSnapshot, Charge, ChargeCategory};
    use chrono::TimeZone;

    fn conn(reporting_currency: &str) -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        schema::apply(&conn).expect("schema applies");
        schema::apply_reporting_currency(&conn, reporting_currency).expect("view applies");
        conn
    }

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap()
    }

    fn charge(service: &str, amount: f64, currency: &str, day: u32) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: Some(amount),
            ..Charge::new(at(day), at(day + 1), currency)
        }
    }

    fn aws() -> PeriodKey {
        PeriodKey::new("AWS", "acct-1", "2026-08")
    }

    fn aliyun() -> PeriodKey {
        PeriodKey::new("Aliyun", "acct-2", "2026-08")
    }

    fn write(conn: &mut Connection, key: &PeriodKey, charges: &[Charge]) {
        let batch_id = crate::ledger::new_batch_id();
        crate::ledger::write_period(conn, key, &batch_id, charges, None).unwrap();
    }

    #[test]
    fn a_cross_cloud_total_is_one_query_in_one_currency() {
        let mut conn = conn("USD");
        write(&mut conn, &aws(), &[charge("EC2", 12.5, "USD", 1)]);
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 1)]);

        // 710 CNY at the built-in 0.1408 is 99.968 USD.
        let total = total_for_period_of(&conn, "2026-08").unwrap();
        assert!((total - 112.468).abs() < 1e-6, "got {total}");

        // Each account still reports in the same currency as the total.
        assert!((period_total_of(&conn, &aws()).unwrap() - 12.5).abs() < 1e-9);
        assert!((period_total_of(&conn, &aliyun()).unwrap() - 99.968).abs() < 1e-6);
    }

    #[test]
    fn changing_the_reporting_currency_rereads_the_same_rows() {
        let mut conn = conn("USD");
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 1)]);

        assert!((period_total_of(&conn, &aliyun()).unwrap() - 99.968).abs() < 1e-6);

        // No rewrite of the fact table: only the view changes.
        schema::apply_reporting_currency(&conn, "CNY").unwrap();
        assert!((period_total_of(&conn, &aliyun()).unwrap() - 710.0).abs() < 1e-9);
    }

    #[test]
    fn a_charge_in_a_currency_no_rate_covers_is_reported_rather_than_counted() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 12.5, "USD", 1),
                charge("Something", 100.0, "JPY", 1),
            ],
        );

        // The unconvertible row is left out of the total...
        let total = total_for_period_of(&conn, "2026-08").unwrap();
        assert!((total - 12.5).abs() < 1e-9, "got {total}");
        // ...and is countable, so the UI can say so.
        assert_eq!(unconverted_charges_of(&conn, "2026-08").unwrap(), 1);
    }

    #[test]
    fn a_rate_is_taken_from_the_charges_own_time() {
        let mut conn = conn("USD");
        conn.execute_batch(
            "INSERT OR REPLACE INTO dim_fx_rate VALUES ('CNY', 'USD', DATE '2026-08-15', 0.2, 'test')",
        )
        .unwrap();

        write(
            &mut conn,
            &aliyun(),
            &[
                charge("ECS", 100.0, "CNY", 1),
                charge("ECS", 100.0, "CNY", 20),
            ],
        );

        // The 1 August charge predates the new rate and keeps the old one;
        // the 20 August charge takes the newer.
        let daily = daily_totals_of(&conn, "Aliyun", "acct-2", at(1)).unwrap();
        assert_eq!(daily.len(), 2);
        assert!((daily[0].1 - 14.08).abs() < 1e-9, "got {:?}", daily[0]);
        assert!((daily[1].1 - 20.0).abs() < 1e-9, "got {:?}", daily[1]);
    }

    #[test]
    fn a_breakdown_is_by_service_largest_first() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("S3", 0.75, "USD", 1),
                charge("EC2", 12.5, "USD", 1),
                charge("EC2", 4.0, "USD", 2),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-2.0),
                    ..charge("EC2", -2.0, "USD", 2)
                },
            ],
        );

        let breakdown = service_breakdown_of(&conn, &aws()).unwrap();
        assert_eq!(
            breakdown,
            vec![("EC2".to_string(), 14.5), ("S3".to_string(), 0.75)]
        );
    }

    #[test]
    fn a_period_that_was_never_ingested_has_no_ingest_time() {
        let mut conn = conn("USD");
        assert!(last_ingest_of(&conn, &aws()).unwrap().is_none());

        write(&mut conn, &aws(), &[charge("EC2", 1.0, "USD", 1)]);
        assert!(last_ingest_of(&conn, &aws()).unwrap().is_some());
    }

    #[test]
    fn the_newest_balance_is_the_one_reported() {
        let mut conn = conn("USD");
        for (day, amount) in [(1, 50.0), (3, 30.0), (2, 40.0)] {
            crate::ledger::write_balance(
                &mut conn,
                &BalanceSnapshot {
                    provider: "DeepSeek".to_string(),
                    account_id: "acct-3".to_string(),
                    observed_at: at(day),
                    balance: amount,
                    granted_balance: Some(5.0),
                    topped_up_balance: Some(amount - 5.0),
                    currency: "CNY".to_string(),
                },
            )
            .unwrap();
        }

        let balance = latest_balance_of(&conn, "DeepSeek", "acct-3")
            .unwrap()
            .expect("a balance was recorded");
        assert_eq!(balance.balance, 30.0);
        assert_eq!(balance.observed_at, at(3));
        // Not converted: a balance is what is left, not what was spent.
        assert_eq!(balance.currency, "CNY");

        assert!(latest_balance_of(&conn, "DeepSeek", "unknown")
            .unwrap()
            .is_none());
    }
}
