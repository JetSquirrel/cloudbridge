//! The billing ledger: `fct_charge` and friends, in their own DuckDB file.
//!
//! This is deliberately separate from [`crate::db`], which holds application
//! state — accounts, budgets and the response caches that still feed the
//! dashboard. Those are re-fetchable or user-entered; the ledger is the
//! thing Sankey, attribution, anomaly detection and month-end freezing will
//! be built on, so it gets its own file and its own `schema_version`.
//!
//! Writes are **whole-period replacement**: everything a provider reports
//! for one `(provider, account, billing period)` lands in one transaction
//! that first deletes what was there. Providers re-issue a bill in full
//! mid-month and retroactively correct prior months, so a row-by-row upsert
//! would leave behind entries the provider has since deleted and the total
//! would stop matching theirs.
//!
//! Nothing writes here yet — PR4 (AWS) and PR5 (Alibaba Cloud, DeepSeek)
//! do, through [`replace_period`] and [`record_balance`]. Until then the
//! module is exercised only by its tests.

// Written by PR4/PR5 and read by PR6; remove once the AWS normalizer lands.
#![allow(dead_code)]

pub mod schema;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use duckdb::{params, Connection};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::get_ledger_database_path;
use schema::TIMESTAMP_FORMAT;

lazy_static::lazy_static! {
    static ref LEDGER_CONNECTION: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(None));
}

/// What kind of charge a row is, in FOCUS terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeCategory {
    Usage,
    Purchase,
    Credit,
    Tax,
    Adjustment,
}

impl ChargeCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "Usage",
            Self::Purchase => "Purchase",
            Self::Credit => "Credit",
            Self::Tax => "Tax",
            Self::Adjustment => "Adjustment",
        }
    }
}

/// How much weight the amount on a row carries.
///
/// Keeps authoritative bills, unit-price-derived amounts and pure usage
/// records in one table without anyone mistaking a shadow cost for money
/// actually spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostBasis {
    /// Straight from the provider's bill.
    Authoritative,
    /// Computed from usage and a unit price.
    Derived,
    /// A projection or an allocation.
    Estimated,
    /// Usage with no amount attached; `billed_cost` is NULL.
    Absent,
}

impl CostBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Derived => "derived",
            Self::Estimated => "estimated",
            Self::Absent => "absent",
        }
    }
}

/// The unit of replacement: one account's charges for one billing period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodKey {
    /// Registry `SourceId`, stored verbatim.
    pub provider: String,
    /// Our `cloud_accounts.id`, not the provider-side account number.
    pub account_id: String,
    /// `YYYY-MM`.
    pub billing_period: String,
}

impl PeriodKey {
    pub fn new(
        provider: impl Into<String>,
        account_id: impl Into<String>,
        billing_period: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            account_id: account_id.into(),
            billing_period: billing_period.into(),
        }
    }
}

/// One row of `fct_charge`, minus the columns that come from the
/// [`PeriodKey`] it is written under.
#[derive(Debug, Clone)]
pub struct Charge {
    pub charge_period_start: DateTime<Utc>,
    pub charge_period_end: DateTime<Utc>,
    pub charge_category: ChargeCategory,
    pub cost_basis: CostBasis,
    pub billing_currency: String,
    /// Provider-side account, when it differs from the credential's own
    /// (an AWS payer account reports its linked accounts).
    pub billing_account_id: Option<String>,
    pub charge_description: Option<String>,
    pub service_name: Option<String>,
    pub service_category: Option<String>,
    pub resource_id: Option<String>,
    pub resource_name: Option<String>,
    pub region_id: Option<String>,
    pub billed_cost: Option<f64>,
    pub effective_cost: Option<f64>,
    pub list_cost: Option<f64>,
    pub pricing_quantity: Option<f64>,
    pub pricing_unit: Option<String>,
    /// JSON object text, or `None` when the source reports no tags.
    pub tags: Option<String>,
}

impl Charge {
    /// An authoritative usage charge with everything optional left unset.
    /// Fill the rest in with struct update syntax.
    pub fn new(
        charge_period_start: DateTime<Utc>,
        charge_period_end: DateTime<Utc>,
        billing_currency: impl Into<String>,
    ) -> Self {
        Self {
            charge_period_start,
            charge_period_end,
            charge_category: ChargeCategory::Usage,
            cost_basis: CostBasis::Authoritative,
            billing_currency: billing_currency.into(),
            billing_account_id: None,
            charge_description: None,
            service_name: None,
            service_category: None,
            resource_id: None,
            resource_name: None,
            region_id: None,
            billed_cost: None,
            effective_cost: None,
            list_cost: None,
            pricing_quantity: None,
            pricing_unit: None,
            tags: None,
        }
    }
}

/// A point-in-time balance for a source that reports state rather than
/// charges.
#[derive(Debug, Clone)]
pub struct BalanceSnapshot {
    pub provider: String,
    pub account_id: String,
    pub observed_at: DateTime<Utc>,
    pub balance: f64,
    pub granted_balance: Option<f64>,
    pub topped_up_balance: Option<f64>,
    pub currency: String,
}

/// Open (creating if needed) the ledger database and apply its schema.
pub fn init_ledger() -> Result<()> {
    let path = get_ledger_database_path()?;
    let conn = Connection::open(&path)?;
    schema::apply(&conn)?;

    let mut ledger = LEDGER_CONNECTION.lock().unwrap();
    *ledger = Some(conn);

    tracing::info!("Ledger initialized: {:?}", path);
    Ok(())
}

fn with_connection<T>(f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
    let mut guard = LEDGER_CONNECTION
        .lock()
        .map_err(|e| anyhow!("Failed to lock ledger connection: {}", e))?;
    let conn = guard
        .as_mut()
        .ok_or_else(|| anyhow!("Ledger not initialized"))?;
    f(conn)
}

/// Identifier for one ingest.
///
/// Minted by the caller rather than in here, because the raw payloads are
/// stored under the same id: `raw/.../batch=<id>/` is what `source_ref`
/// points at, and the two have to agree.
pub fn new_batch_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Replace everything stored for `key` with `charges`, in one transaction.
///
/// `source_ref` points at the raw payload the rows were normalized from.
pub fn replace_period(
    key: &PeriodKey,
    batch_id: &str,
    charges: &[Charge],
    source_ref: Option<&str>,
) -> Result<()> {
    with_connection(|conn| write_period(conn, key, batch_id, charges, source_ref))
}

/// Record a balance observation. Re-observing the same instant overwrites,
/// so a repeated ingest of one payload is a no-op.
pub fn record_balance(snapshot: &BalanceSnapshot) -> Result<()> {
    with_connection(|conn| write_balance(conn, snapshot))
}

fn write_period(
    conn: &mut Connection,
    key: &PeriodKey,
    batch_id: &str,
    charges: &[Charge],
    source_ref: Option<&str>,
) -> Result<()> {
    let now = Utc::now().format(TIMESTAMP_FORMAT).to_string();
    let ids = charge_ids(key, charges);

    let tx = conn.transaction()?;

    // Older batches for this period stay as history — freezing (P3) needs
    // to know a period was ingested more than once — but only one of them
    // has rows in fct_charge.
    tx.execute(
        "UPDATE ingest_batch SET status = 'superseded'
         WHERE provider = ? AND account_id = ? AND billing_period = ? AND status = 'complete'",
        params![key.provider, key.account_id, key.billing_period],
    )?;
    tx.execute(
        "DELETE FROM fct_charge WHERE provider = ? AND account_id = ? AND billing_period = ?",
        params![key.provider, key.account_id, key.billing_period],
    )?;
    tx.execute(
        "INSERT INTO ingest_batch
         (batch_id, provider, account_id, billing_period, started_at, completed_at,
          status, row_count, source_ref)
         VALUES (?, ?, ?, ?, CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP), 'complete', ?, ?)",
        params![
            batch_id,
            key.provider,
            key.account_id,
            key.billing_period,
            now,
            now,
            charges.len() as i64,
            source_ref,
        ],
    )?;

    {
        let mut stmt = tx.prepare(
            "INSERT INTO fct_charge
             (charge_id, batch_id, provider, account_id, billing_account_id, billing_period,
              charge_period_start, charge_period_end, charge_category, charge_description,
              service_name, service_category, resource_id, resource_name, region_id,
              billed_cost, effective_cost, list_cost, billing_currency, cost_basis,
              pricing_quantity, pricing_unit, tags, created_at)
             VALUES (?, ?, ?, ?, ?, ?, CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP), ?, ?,
                     ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CAST(? AS TIMESTAMP))",
        )?;

        for (charge, charge_id) in charges.iter().zip(&ids) {
            stmt.execute(params![
                charge_id,
                batch_id,
                key.provider,
                key.account_id,
                charge.billing_account_id,
                key.billing_period,
                charge
                    .charge_period_start
                    .format(TIMESTAMP_FORMAT)
                    .to_string(),
                charge
                    .charge_period_end
                    .format(TIMESTAMP_FORMAT)
                    .to_string(),
                charge.charge_category.as_str(),
                charge.charge_description,
                charge.service_name,
                charge.service_category,
                charge.resource_id,
                charge.resource_name,
                charge.region_id,
                charge.billed_cost,
                charge.effective_cost,
                charge.list_cost,
                charge.billing_currency,
                charge.cost_basis.as_str(),
                charge.pricing_quantity,
                charge.pricing_unit,
                charge.tags,
                now,
            ])?;
        }
    }

    tx.commit()?;

    tracing::info!(
        "Ledger: wrote {} charges for {}/{} {} (batch {})",
        charges.len(),
        key.provider,
        key.account_id,
        key.billing_period,
        batch_id
    );
    Ok(())
}

fn write_balance(conn: &mut Connection, snapshot: &BalanceSnapshot) -> Result<()> {
    let now = Utc::now().format(TIMESTAMP_FORMAT).to_string();

    conn.execute(
        "INSERT OR REPLACE INTO fct_balance_snapshot
         (provider, account_id, observed_at, balance, granted_balance, topped_up_balance,
          currency, created_at)
         VALUES (?, ?, CAST(? AS TIMESTAMP), ?, ?, ?, ?, CAST(? AS TIMESTAMP))",
        params![
            snapshot.provider,
            snapshot.account_id,
            snapshot.observed_at.format(TIMESTAMP_FORMAT).to_string(),
            snapshot.balance,
            snapshot.granted_balance,
            snapshot.topped_up_balance,
            snapshot.currency,
            now,
        ],
    )?;

    Ok(())
}

/// Deterministic ids for a batch of charges.
///
/// The id is a hash of the row's natural key, so re-ingesting an unchanged
/// bill produces the same `charge_id` for the same charge — that is what
/// makes "run ingest twice, get identical results" checkable, and what lets
/// a later diff say which rows the provider actually changed.
///
/// Providers do emit rows whose natural keys collide (two charges for the
/// same service, day and category, split by something we do not store). A
/// collision gets an occurrence suffix rather than being folded into one
/// row, so no money goes missing; the suffix follows the provider's own
/// ordering of the payload.
fn charge_ids(key: &PeriodKey, charges: &[Charge]) -> Vec<String> {
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut ids = Vec::with_capacity(charges.len());

    for charge in charges {
        let natural_key = [
            key.provider.as_str(),
            key.account_id.as_str(),
            key.billing_period.as_str(),
            &charge
                .charge_period_start
                .format(TIMESTAMP_FORMAT)
                .to_string(),
            &charge
                .charge_period_end
                .format(TIMESTAMP_FORMAT)
                .to_string(),
            charge.charge_category.as_str(),
            charge.billing_account_id.as_deref().unwrap_or(""),
            charge.service_name.as_deref().unwrap_or(""),
            charge.resource_id.as_deref().unwrap_or(""),
            charge.region_id.as_deref().unwrap_or(""),
            charge.charge_description.as_deref().unwrap_or(""),
            charge.pricing_unit.as_deref().unwrap_or(""),
            charge.billing_currency.as_str(),
        ]
        .join("\u{1f}");

        let occurrence = seen.entry(natural_key.clone()).or_insert(0);
        let mut hasher = Sha256::new();
        hasher.update(natural_key.as_bytes());
        hasher.update(format!("\u{1f}{}", occurrence).as_bytes());
        *occurrence += 1;

        ids.push(hex::encode(hasher.finalize())[..32].to_string());
    }

    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        schema::apply(&conn).expect("schema applies");
        conn
    }

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap()
    }

    fn usage(service: &str, amount: f64, day: u32) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: Some(amount),
            effective_cost: Some(amount),
            ..Charge::new(at(day), at(day + 1), "USD")
        }
    }

    fn key() -> PeriodKey {
        PeriodKey::new("AWS", "acct-1", "2026-08")
    }

    /// (charge_id, service, billed_cost) for a period, in id order.
    fn stored(conn: &Connection, key: &PeriodKey) -> Vec<(String, String, f64)> {
        let mut stmt = conn
            .prepare(
                "SELECT charge_id, service_name, billed_cost FROM fct_charge
                 WHERE provider = ? AND account_id = ? AND billing_period = ?
                 ORDER BY charge_id",
            )
            .unwrap();
        stmt.query_map(
            params![key.provider, key.account_id, key.billing_period],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn scalar_i64(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn ingesting_the_same_bill_twice_is_a_no_op() {
        let mut conn = conn();
        let key = key();
        let charges = vec![usage("EC2", 12.5, 1), usage("S3", 0.75, 1)];

        write_period(&mut conn, &key, "b-1", &charges, None).unwrap();
        let first = stored(&conn, &key);

        write_period(&mut conn, &key, "b-2", &charges, None).unwrap();
        let second = stored(&conn, &key);

        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
    }

    #[test]
    fn replacement_drops_rows_the_provider_no_longer_reports() {
        let mut conn = conn();
        let key = key();

        write_period(
            &mut conn,
            &key,
            "b-1",
            &[
                usage("EC2", 12.5, 1),
                usage("S3", 0.75, 1),
                usage("RDS", 3.0, 1),
            ],
            None,
        )
        .unwrap();

        // The provider reissues the period without RDS and with a corrected
        // EC2 amount.
        write_period(
            &mut conn,
            &key,
            "b-2",
            &[usage("EC2", 11.0, 1), usage("S3", 0.75, 1)],
            None,
        )
        .unwrap();

        let rows = stored(&conn, &key);
        assert_eq!(rows.len(), 2);
        let services: Vec<&str> = rows.iter().map(|(_, s, _)| s.as_str()).collect();
        assert!(!services.contains(&"RDS"));
        let ec2 = rows.iter().find(|(_, s, _)| s == "EC2").unwrap();
        assert_eq!(ec2.2, 11.0);
    }

    #[test]
    fn every_ingest_is_recorded_but_only_the_last_one_holds_rows() {
        let mut conn = conn();
        let key = key();

        write_period(&mut conn, &key, "b-1", &[usage("EC2", 12.5, 1)], None).unwrap();
        write_period(&mut conn, &key, "b-2", &[usage("EC2", 11.0, 1)], None).unwrap();

        assert_eq!(scalar_i64(&conn, "SELECT count(*) FROM ingest_batch"), 2);
        assert_eq!(
            scalar_i64(
                &conn,
                "SELECT count(*) FROM ingest_batch WHERE status = 'superseded'"
            ),
            1
        );
        assert_eq!(
            conn.query_row::<String, _, _>("SELECT batch_id FROM fct_charge", [], |r| r.get(0))
                .unwrap(),
            "b-2"
        );
    }

    #[test]
    fn replacing_one_period_leaves_the_others_alone() {
        let mut conn = conn();
        let august = key();
        let july = PeriodKey::new("AWS", "acct-1", "2026-07");
        let other_account = PeriodKey::new("AWS", "acct-2", "2026-08");

        write_period(&mut conn, &july, "b-1", &[usage("EC2", 9.0, 1)], None).unwrap();
        write_period(
            &mut conn,
            &other_account,
            "b-2",
            &[usage("EC2", 5.0, 1)],
            None,
        )
        .unwrap();
        write_period(&mut conn, &august, "b-3", &[usage("EC2", 12.5, 1)], None).unwrap();
        write_period(&mut conn, &august, "b-4", &[], None).unwrap();

        assert!(stored(&conn, &august).is_empty());
        assert_eq!(stored(&conn, &july).len(), 1);
        assert_eq!(stored(&conn, &other_account).len(), 1);
    }

    #[test]
    fn charges_that_differ_only_by_an_unstored_dimension_both_survive() {
        let mut conn = conn();
        let key = key();
        let charges = vec![usage("EC2", 12.5, 1), usage("EC2", 4.0, 1)];

        write_period(&mut conn, &key, "b-1", &charges, None).unwrap();
        let first = stored(&conn, &key);
        assert_eq!(first.len(), 2);

        write_period(&mut conn, &key, "b-2", &charges, None).unwrap();
        assert_eq!(stored(&conn, &key), first);
    }

    #[test]
    fn a_charge_without_an_amount_is_storable() {
        let mut conn = conn();
        let key = key();

        write_period(
            &mut conn,
            &key,
            "b-1",
            &[Charge {
                service_name: Some("Claude Code".to_string()),
                cost_basis: CostBasis::Absent,
                pricing_quantity: Some(18_000.0),
                pricing_unit: Some("Tokens".to_string()),
                ..Charge::new(at(1), at(2), "USD")
            }],
            None,
        )
        .unwrap();

        let (basis, unit, cost) = conn
            .query_row(
                "SELECT cost_basis, pricing_unit, billed_cost FROM fct_charge",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<f64>>(2)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(basis, "absent");
        assert_eq!(unit, "Tokens");
        assert_eq!(cost, None);
    }

    #[test]
    fn re_observing_a_balance_at_the_same_instant_overwrites() {
        let mut conn = conn();
        let snapshot = BalanceSnapshot {
            provider: "DeepSeek".to_string(),
            account_id: "acct-3".to_string(),
            observed_at: at(1),
            balance: 42.0,
            granted_balance: Some(10.0),
            topped_up_balance: Some(32.0),
            currency: "CNY".to_string(),
        };

        write_balance(&mut conn, &snapshot).unwrap();
        write_balance(
            &mut conn,
            &BalanceSnapshot {
                balance: 41.0,
                ..snapshot.clone()
            },
        )
        .unwrap();

        assert_eq!(
            scalar_i64(&conn, "SELECT count(*) FROM fct_balance_snapshot"),
            1
        );
        let balance: f64 = conn
            .query_row("SELECT balance FROM fct_balance_snapshot", [], |r| r.get(0))
            .unwrap();
        assert_eq!(balance, 41.0);
    }

    #[test]
    fn timestamps_survive_the_round_trip() {
        let mut conn = conn();
        let key = key();

        write_period(&mut conn, &key, "b-1", &[usage("EC2", 1.0, 3)], None).unwrap();

        let start: String = conn
            .query_row(
                "SELECT CAST(charge_period_start AS VARCHAR) FROM fct_charge",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(start.starts_with("2026-08-03 00:00:00"), "got {start}");

        // PR6's view casts this column to DATE for the ASOF join on rates.
        let as_date: String = conn
            .query_row(
                "SELECT CAST(charge_period_start::DATE AS VARCHAR) FROM fct_charge",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(as_date, "2026-08-03");
    }
}
