//! The daily cost rollup: a derived read-model over `fct_charge`, so the
//! hot analytics reads do not rescan raw line items on every page load.
//!
//! Modeled on Wealthfolio's `daily_account_valuation`: writes are
//! incremental — a completed ingest batch recomputes only the day range it
//! touched (delete + re-aggregate), never the whole table — and a full
//! [`rebuild_all`] covers what incrementality cannot.
//!
//! Amounts are stored twice, as the reading view exposes them: summed in
//! the currency the provider billed (`billed_cost`, ...) and converted to
//! the reporting currency (`billed_cost_base`, ...), which the
//! `reporting_currency` column records. Conversion still happens through
//! [`NORMALIZED_VIEW`] and its ASOF fx join — the rollup aggregates the
//! view, never the raw table — so a change of reporting currency
//! invalidates every stored `*_base` amount. That is the one drift
//! [`is_current`] checks for, and `schema::apply_reporting_currency`
//! rebuilds on it.

use anyhow::{anyhow, Result};
use chrono::{NaiveDate, Utc};
use duckdb::{params, Connection};

use super::schema::{NORMALIZED_VIEW, TIMESTAMP_FORMAT};
use super::{with_connection_ref, PeriodKey};

/// Recompute the rollup for the day range one completed batch covered: the
/// account's billing period, since a period is the unit the ledger
/// replaces as a whole.
///
/// Rows outside the range — other periods, other accounts — are untouched,
/// which is what makes this cheap enough to run on every ingest. Returns
/// the number of rollup rows written.
pub fn refresh_for_period(key: &PeriodKey) -> Result<usize> {
    with_connection_ref(|conn| refresh_for_period_of(conn, key))
}

/// Drop and re-aggregate every rollup row. The expensive path: taken when
/// the reporting currency changes (every stored `*_base` amount is then
/// wrong) and as the repair for anything the incremental path missed.
pub fn rebuild_all() -> Result<usize> {
    with_connection_ref(rebuild_all_of)
}

/// Whether the rollup reflects the ledger as `reporting_currency` reads it.
///
/// False when any row was built for another currency, and when the ledger
/// holds charges the rollup has none of — a ledger that predates the
/// rollup table, or one whose refreshes never ran, reads as stale rather
/// than as an honest zero. What this does not track is finer drift inside
/// an otherwise current rollup; the per-batch refresh and
/// [`rebuild_all`] are the answer to that.
pub fn is_current(reporting_currency: &str) -> Result<bool> {
    with_connection_ref(|conn| is_current_of(conn, reporting_currency))
}

/// Rebuild the rollup when [`is_current_of`] says it has drifted from the
/// ledger. Called from `schema::apply_reporting_currency` — after the view
/// has been repointed, so the rebuild converts at the currency just
/// applied — which makes both a currency change and a pre-rollup ledger
/// self-repairing on the next start.
pub(crate) fn rebuild_if_stale(conn: &Connection, reporting_currency: &str) -> Result<()> {
    if is_current_of(conn, reporting_currency)? {
        return Ok(());
    }

    tracing::info!("Daily rollup is stale for {reporting_currency}; rebuilding");
    rebuild_all_of(conn)?;
    Ok(())
}

pub(crate) fn refresh_for_period_of(conn: &Connection, key: &PeriodKey) -> Result<usize> {
    let (first_day, end_day) = period_day_range(&key.billing_period)?;

    in_transaction(conn, |conn| {
        conn.execute(
            "DELETE FROM daily_cost_rollup
             WHERE provider = ? AND account_id = ?
               AND day >= CAST(? AS DATE) AND day < CAST(? AS DATE)",
            params![
                key.provider,
                key.account_id,
                first_day.to_string(),
                end_day.to_string()
            ],
        )?;
        aggregate_into_rollup(conn, Some((key, first_day, end_day)))
    })
}

pub(crate) fn rebuild_all_of(conn: &Connection) -> Result<usize> {
    in_transaction(conn, |conn| {
        conn.execute("DELETE FROM daily_cost_rollup", [])?;
        aggregate_into_rollup(conn, None)
    })
}

pub(crate) fn is_current_of(conn: &Connection, reporting_currency: &str) -> Result<bool> {
    let foreign_rows: i64 = conn.query_row(
        "SELECT count(*) FROM daily_cost_rollup WHERE reporting_currency <> ?",
        params![reporting_currency],
        |row| row.get(0),
    )?;
    if foreign_rows > 0 {
        return Ok(false);
    }

    let (rolled_up, stored): (i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM daily_cost_rollup),
                (SELECT count(*) FROM fct_charge)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // An empty rollup over a non-empty ledger was never built, or was lost.
    Ok(rolled_up > 0 || stored == 0)
}

/// The one aggregation behind both writes: sums of the reading view at the
/// rollup grain, either for one account's day range or — `None` — for the
/// whole ledger. The view's `reporting_currency` is carried into the row,
/// so what the `*_base` columns mean is recorded with them.
///
/// The DELETE predicate of [`refresh_for_period_of`] and the scope here
/// bound the same day range on the same expression, so a refresh replaces
/// exactly the rows it re-aggregates. Returns the rows written.
fn aggregate_into_rollup(
    conn: &Connection,
    scope: Option<(&PeriodKey, NaiveDate, NaiveDate)>,
) -> Result<usize> {
    let now = Utc::now().format(TIMESTAMP_FORMAT).to_string();

    let (where_sql, mut bound) = match scope {
        Some((key, first_day, end_day)) => (
            "WHERE provider = ? AND account_id = ?
               AND charge_period_start::DATE >= CAST(? AS DATE)
               AND charge_period_start::DATE <  CAST(? AS DATE)"
                .to_string(),
            vec![
                key.provider.clone(),
                key.account_id.clone(),
                first_day.to_string(),
                end_day.to_string(),
            ],
        ),
        None => (String::new(), Vec::new()),
    };
    // `built_at` binds first: it is in the SELECT list, ahead of the WHERE.
    bound.insert(0, now);

    let written = conn.execute(
        &format!(
            "INSERT INTO daily_cost_rollup
                 (provider, account_id, day, service_name, region_id, billing_currency,
                  billed_cost, effective_cost, list_cost,
                  billed_cost_base, effective_cost_base, charge_count,
                  reporting_currency, built_at)
             SELECT
                 provider,
                 account_id,
                 charge_period_start::DATE AS day,
                 service_name,
                 region_id,
                 billing_currency,
                 sum(billed_cost)         AS billed_cost,
                 sum(effective_cost)      AS effective_cost,
                 sum(list_cost)           AS list_cost,
                 sum(billed_cost_base)    AS billed_cost_base,
                 sum(effective_cost_base) AS effective_cost_base,
                 count(*)                 AS charge_count,
                 reporting_currency,
                 CAST(? AS TIMESTAMP)     AS built_at
             FROM {NORMALIZED_VIEW}
             {where_sql}
             GROUP BY provider, account_id, day, service_name, region_id,
                      billing_currency, reporting_currency"
        ),
        duckdb::params_from_iter(bound),
    )?;

    Ok(written)
}

/// Run `f` with the DELETE and the re-aggregation as one unit: a failure
/// between them must not leave a range deleted but not rebuilt.
fn in_transaction<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN TRANSACTION")?;
    match f(conn) {
        Ok(done) => {
            conn.execute_batch("COMMIT")?;
            Ok(done)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// First day of a `YYYY-MM` billing period and of the one after it.
fn period_day_range(billing_period: &str) -> Result<(NaiveDate, NaiveDate)> {
    let (year, month) = billing_period
        .split_once('-')
        .ok_or_else(|| anyhow!("Not a YYYY-MM billing period: {:?}", billing_period))?;
    let (year, month) = (year.parse()?, month.parse()?);
    if NaiveDate::from_ymd_opt(year, month, 1).is_none() {
        anyhow::bail!("Not a real billing period: {:?}", billing_period);
    }

    let period = crate::cloud::BillingPeriod::new(year, month);
    Ok((period.start(), period.end_exclusive()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::schema;
    use crate::ledger::{Channel, Charge};
    use chrono::{DateTime, TimeZone};

    fn conn(reporting_currency: &str) -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        schema::apply(&conn).expect("schema applies");
        schema::apply_reporting_currency(&conn, reporting_currency).expect("view applies");
        conn
    }

    fn at(month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, month, day, 0, 0, 0).unwrap()
    }

    fn charge(service: &str, amount: f64, currency: &str, month: u32, day: u32) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: Some(amount),
            effective_cost: Some(amount),
            ..Charge::new(
                at(month, day),
                at(month, day) + chrono::Duration::days(1),
                currency,
            )
        }
    }

    fn august() -> PeriodKey {
        PeriodKey::new("AWS", "acct-1", "2026-08")
    }

    fn write(conn: &mut Connection, key: &PeriodKey, charges: &[Charge]) {
        let batch_id = crate::ledger::new_batch_id();
        crate::ledger::write_period(conn, key, &batch_id, charges, None, Channel::Api).unwrap();
    }

    /// A rollup row, as `rows` reads it.
    #[derive(Debug, PartialEq)]
    struct RollupRow {
        provider: String,
        account_id: String,
        day: String,
        service_name: Option<String>,
        region_id: Option<String>,
        billing_currency: String,
        billed_cost: Option<f64>,
        billed_cost_base: Option<f64>,
        charge_count: i64,
        reporting_currency: String,
        built_at: String,
    }

    fn rows(conn: &Connection) -> Vec<RollupRow> {
        let mut stmt = conn
            .prepare(
                "SELECT provider, account_id, CAST(day AS VARCHAR), service_name, region_id,
                        billing_currency, billed_cost, billed_cost_base, charge_count,
                        reporting_currency, CAST(built_at AS VARCHAR)
                 FROM daily_cost_rollup
                 ORDER BY provider, account_id, day, service_name, region_id, billing_currency",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(RollupRow {
                provider: row.get(0)?,
                account_id: row.get(1)?,
                day: row.get(2)?,
                service_name: row.get(3)?,
                region_id: row.get(4)?,
                billing_currency: row.get(5)?,
                billed_cost: row.get(6)?,
                billed_cost_base: row.get(7)?,
                charge_count: row.get(8)?,
                reporting_currency: row.get(9)?,
                built_at: row.get(10)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    /// (day, service, region, currency, billed, billed_base, count) — a
    /// rollup row projected to the columns a direct group-by produces.
    type DirectRow = (
        String,
        Option<String>,
        Option<String>,
        String,
        Option<f64>,
        Option<f64>,
        i64,
    );

    #[test]
    fn the_rollup_matches_a_direct_group_by_of_the_ledger() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &august(),
            &[
                charge("EC2", 12.5, "USD", 8, 1),
                charge("EC2", 4.0, "USD", 8, 1),
                Charge {
                    region_id: Some("us-east-1".to_string()),
                    ..charge("EC2", 2.0, "USD", 8, 1)
                },
                charge("S3", 0.75, "USD", 8, 2),
            ],
        );
        write(
            &mut conn,
            &PeriodKey::new("Aliyun", "acct-2", "2026-08"),
            &[charge("ECS", 710.0, "CNY", 8, 1)],
        );

        rebuild_all_of(&conn).unwrap();

        // The same aggregation straight off the reading view, rolled into
        // (day, service, region, currency, billed, billed_base, count).
        let mut stmt = conn
            .prepare(
                "SELECT CAST(charge_period_start::DATE AS VARCHAR), service_name, region_id,
                        billing_currency, sum(billed_cost), sum(billed_cost_base), count(*)
                 FROM v_charge_normalized
                 WHERE provider = 'AWS' AND account_id = 'acct-1'
                 GROUP BY 1, 2, 3, 4
                 ORDER BY 1, 2, 3, 4",
            )
            .unwrap();
        let direct: Vec<DirectRow> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let rolled: Vec<_> = rows(&conn)
            .into_iter()
            .filter(|row| row.provider == "AWS" && row.account_id == "acct-1")
            .map(|row| {
                (
                    row.day,
                    row.service_name,
                    row.region_id,
                    row.billing_currency,
                    row.billed_cost,
                    row.billed_cost_base,
                    row.charge_count,
                )
            })
            .collect();

        assert_eq!(rolled, direct);
    }

    #[test]
    fn a_refresh_recomputes_only_the_ingested_period() {
        let mut conn = conn("USD");
        let july = PeriodKey::new("AWS", "acct-1", "2026-07");
        let other_account = PeriodKey::new("AWS", "acct-2", "2026-08");
        write(&mut conn, &july, &[charge("EC2", 9.0, "USD", 7, 15)]);
        write(
            &mut conn,
            &august(),
            &[
                charge("EC2", 12.5, "USD", 8, 1),
                charge("RDS", 3.0, "USD", 8, 2),
            ],
        );
        write(
            &mut conn,
            &other_account,
            &[charge("EC2", 5.0, "USD", 8, 1)],
        );
        rebuild_all_of(&conn).unwrap();

        let untouched_before: Vec<_> = rows(&conn)
            .into_iter()
            .filter(|row| row.account_id == "acct-2" || row.day.starts_with("2026-07"))
            .collect();

        // The provider reissues August without RDS and with EC2 corrected.
        write(&mut conn, &august(), &[charge("EC2", 11.0, "USD", 8, 1)]);
        let written = refresh_for_period_of(&conn, &august()).unwrap();

        assert_eq!(written, 1);
        let after = rows(&conn);
        let aws_august: Vec<_> = after
            .iter()
            .filter(|row| {
                row.provider == "AWS"
                    && row.account_id == "acct-1"
                    && row.day.starts_with("2026-08")
            })
            .collect();
        assert_eq!(aws_august.len(), 1);
        assert_eq!(aws_august[0].service_name.as_deref(), Some("EC2"));
        assert_eq!(aws_august[0].billed_cost, Some(11.0));

        // July and the other account are byte-for-byte what they were,
        // built_at included — the refresh did not touch them.
        let untouched_after: Vec<_> = after
            .into_iter()
            .filter(|row| row.account_id == "acct-2" || row.day.starts_with("2026-07"))
            .collect();
        assert_eq!(untouched_after, untouched_before);
    }

    #[test]
    fn a_period_replaced_with_nothing_leaves_no_rollup_rows() {
        let mut conn = conn("USD");
        write(&mut conn, &august(), &[charge("EC2", 12.5, "USD", 8, 1)]);
        refresh_for_period_of(&conn, &august()).unwrap();
        assert_eq!(rows(&conn).len(), 1);

        // The provider's corrected period holds no charges at all.
        write(&mut conn, &august(), &[]);
        let written = refresh_for_period_of(&conn, &august()).unwrap();

        assert_eq!(written, 0);
        assert!(rows(&conn).is_empty());
    }

    #[test]
    fn changing_the_reporting_currency_rebuilds_the_rollup() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &PeriodKey::new("Aliyun", "acct-2", "2026-08"),
            &[charge("ECS", 710.0, "CNY", 8, 1)],
        );

        // Charges landed after the last currency apply: stale until built.
        assert!(!is_current_of(&conn, "USD").unwrap());

        rebuild_all_of(&conn).unwrap();
        assert!(is_current_of(&conn, "USD").unwrap());
        // Built for USD, it is not current for any other currency.
        assert!(!is_current_of(&conn, "CNY").unwrap());
        assert_eq!(rows(&conn)[0].billed_cost_base, Some(710.0 * 0.1408));

        // The currency change itself triggers the rebuild.
        schema::apply_reporting_currency(&conn, "CNY").unwrap();

        assert!(is_current_of(&conn, "CNY").unwrap());
        let row = rows(&conn).into_iter().next().unwrap();
        assert_eq!(row.reporting_currency, "CNY");
        // Source sums are currency-agnostic; the converted ones are not.
        assert_eq!(row.billed_cost, Some(710.0));
        assert_eq!(row.billed_cost_base, Some(710.0));
    }

    #[test]
    fn an_empty_ledger_has_a_current_empty_rollup() {
        let conn = conn("USD");

        // Nothing to roll up: current without a build, and a build writes
        // nothing.
        assert!(is_current_of(&conn, "USD").unwrap());
        assert_eq!(rebuild_all_of(&conn).unwrap(), 0);
        assert!(rows(&conn).is_empty());
    }

    #[test]
    fn a_charge_no_rate_covers_keeps_its_source_amount() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &august(),
            &[charge("Something", 100.0, "JPY", 8, 1)],
        );

        rebuild_all_of(&conn).unwrap();

        // The converted amount is NULL — left out of a converted total, as
        // the reading view leaves it — but the source sum and the row are
        // there, and the rollup still counts as current.
        let row = rows(&conn).into_iter().next().unwrap();
        assert_eq!(row.billed_cost, Some(100.0));
        assert_eq!(row.billed_cost_base, None);
        assert!(is_current_of(&conn, "USD").unwrap());
    }

    #[test]
    fn a_refresh_scans_charges_outside_the_period_they_belong_to_nowhere() {
        let mut conn = conn("USD");
        // A charge whose day falls in July but that is stored under the
        // August period: a refresh of July must not pick it up, and a
        // refresh of August bounds by day, so neither does.
        write(&mut conn, &august(), &[charge("EC2", 50.0, "USD", 7, 31)]);

        assert_eq!(refresh_for_period_of(&conn, &august()).unwrap(), 0);
        assert!(rows(&conn).is_empty());

        // A full rebuild is the path that catches what day-bounded
        // refreshes cannot.
        assert_eq!(rebuild_all_of(&conn).unwrap(), 1);
        assert_eq!(rows(&conn)[0].day, "2026-07-31");
    }
}
