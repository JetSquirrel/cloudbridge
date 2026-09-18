//! AWS Data Exports (CUR 2.0, FOCUS 1.2 with AWS columns) → ledger rows.
//!
//! A FOCUS export is Parquet, which none of the text-oriented bill-file
//! parsers can read, so this normalizer goes through an in-memory DuckDB:
//! the payload bytes are staged to a temporary directory and scanned with
//! `read_parquet`. That is still a pure function of the batch — no clock,
//! no network, no database state — and the temp files are gone when it
//! returns.
//!
//! The export's column set varies with the data the account generates, so
//! the SELECT is built from the schema the files actually have: a column
//! the export omits maps to NULL rather than failing the whole period.

use std::fmt;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use duckdb::Connection;

use super::{Normalized, RawBatch};
use crate::ledger::{Charge, ChargeCategory};

/// The export has no partition for a period yet — it lands a day or more
/// after the period starts, so the current month is routinely absent right
/// after midnight on the first. Not an error in the account, and above all
/// not an empty batch: the ingest must skip the period, never replace it
/// with zero rows.
#[derive(Debug)]
pub struct ExportNotReady {
    pub uri: String,
    pub period: String,
}

impl fmt::Display for ExportNotReady {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "No export data for {} under {} yet — the export has not delivered this period",
            self.period, self.uri
        )
    }
}

impl std::error::Error for ExportNotReady {}

/// Whether an error is the export-not-delivered case, for the ingest loop
/// to skip rather than fail on.
pub fn is_export_not_ready(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ExportNotReady>().is_some()
}

/// The ledger columns a FOCUS row maps to, in SELECT order.
///
/// `Numeric` columns are cast to DOUBLE; everything else to VARCHAR, which
/// covers the string-typed timestamps FOCUS delivers as well as real ones.
enum ColumnKind {
    Text,
    Numeric,
}

const COLUMNS: &[(&str, ColumnKind)] = &[
    ("ChargePeriodStart", ColumnKind::Text),
    ("ChargePeriodEnd", ColumnKind::Text),
    ("ChargeCategory", ColumnKind::Text),
    ("BillingAccountId", ColumnKind::Text),
    ("ChargeDescription", ColumnKind::Text),
    ("ServiceName", ColumnKind::Text),
    ("ServiceCategory", ColumnKind::Text),
    ("ResourceId", ColumnKind::Text),
    ("ResourceName", ColumnKind::Text),
    ("RegionId", ColumnKind::Text),
    ("BilledCost", ColumnKind::Numeric),
    ("EffectiveCost", ColumnKind::Numeric),
    ("ListCost", ColumnKind::Numeric),
    ("BillingCurrency", ColumnKind::Text),
    ("PricingQuantity", ColumnKind::Numeric),
    ("PricingUnit", ColumnKind::Text),
    ("Tags", ColumnKind::Text),
];

// SELECT positions, matching COLUMNS above.
const IX_START: usize = 0;
const IX_END: usize = 1;
const IX_CATEGORY: usize = 2;
const IX_BILLING_ACCOUNT: usize = 3;
const IX_DESCRIPTION: usize = 4;
const IX_SERVICE: usize = 5;
const IX_SERVICE_CATEGORY: usize = 6;
const IX_RESOURCE_ID: usize = 7;
const IX_RESOURCE_NAME: usize = 8;
const IX_REGION: usize = 9;
const IX_BILLED: usize = 10;
const IX_EFFECTIVE: usize = 11;
const IX_LIST: usize = 12;
const IX_CURRENCY: usize = 13;
const IX_QUANTITY: usize = 14;
const IX_UNIT: usize = 15;
const IX_TAGS: usize = 16;

/// Turn a batch of FOCUS Parquet payloads into ledger rows.
///
/// Pure — every input is in `batch`; the only I/O is staging the bytes to
/// a temporary directory for `read_parquet`.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    if batch.payload_files.is_empty() {
        return Err(anyhow!("Raw batch has no FOCUS export payloads"));
    }
    let staging = StagingDir::new(&batch.payload_files)?;

    let conn = Connection::open_in_memory()?;
    let scan = format!(
        "read_parquet('{}', union_by_name = true)",
        sql_literal(&staging.glob())
    );

    let available: Vec<String> = conn
        .prepare(&format!("DESCRIBE SELECT * FROM {scan}"))?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let available: Vec<String> = available.iter().map(|name| name.to_lowercase()).collect();

    let select_list: Vec<String> = COLUMNS
        .iter()
        .enumerate()
        .map(|(ix, (name, kind))| {
            let cast = match kind {
                ColumnKind::Text => "VARCHAR",
                ColumnKind::Numeric => "DOUBLE",
            };
            if available.contains(&name.to_lowercase()) {
                format!("CAST(\"{name}\" AS {cast}) AS c{ix}")
            } else {
                format!("CAST(NULL AS {cast}) AS c{ix}")
            }
        })
        .collect();
    let sql = format!("SELECT {} FROM {scan}", select_list.join(", "));

    let mut charges = Vec::new();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let start = row.get::<_, Option<String>>(IX_START)?;
        let end = row.get::<_, Option<String>>(IX_END)?;
        Ok((
            start,
            end,
            Charge {
                charge_period_start: DateTime::default(),
                charge_period_end: DateTime::default(),
                charge_category: row
                    .get::<_, Option<String>>(IX_CATEGORY)?
                    .map_or(ChargeCategory::Usage, |c| charge_category(&c)),
                billing_account_id: row.get(IX_BILLING_ACCOUNT)?,
                charge_description: row.get(IX_DESCRIPTION)?,
                service_name: row.get(IX_SERVICE)?,
                service_category: row.get(IX_SERVICE_CATEGORY)?,
                resource_id: row.get(IX_RESOURCE_ID)?,
                resource_name: row.get(IX_RESOURCE_NAME)?,
                region_id: row.get(IX_REGION)?,
                billed_cost: row.get(IX_BILLED)?,
                effective_cost: row.get(IX_EFFECTIVE)?,
                list_cost: row.get(IX_LIST)?,
                billing_currency: row
                    .get::<_, Option<String>>(IX_CURRENCY)?
                    .unwrap_or_else(|| "USD".to_string()),
                pricing_quantity: row.get(IX_QUANTITY)?,
                pricing_unit: row.get(IX_UNIT)?,
                tags: row
                    .get::<_, Option<String>>(IX_TAGS)?
                    .filter(|tags| !tags.is_empty() && tags != "{}"),
                ..Charge::new(DateTime::default(), DateTime::default(), "USD")
            },
        ))
    })?;

    for row in rows {
        let (start, end, mut charge) = row?;
        // A row with no charge period cannot be filed anywhere sensible;
        // an export that produces one is broken, so the period fails loud.
        charge.charge_period_start = parse_instant(start.as_deref().unwrap_or_default())
            .ok_or_else(|| anyhow!("FOCUS row has no usable ChargePeriodStart"))?;
        charge.charge_period_end = parse_instant(end.as_deref().unwrap_or_default())
            .ok_or_else(|| anyhow!("FOCUS row has no usable ChargePeriodEnd"))?;
        charges.push(charge);
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

/// FOCUS category → ledger category. The vocabularies are the same five
/// values; an unrecognized one still moved money, so it is kept and
/// labelled an Adjustment rather than mistaken for consumption.
fn charge_category(value: &str) -> ChargeCategory {
    match value {
        "Usage" => ChargeCategory::Usage,
        "Purchase" => ChargeCategory::Purchase,
        "Credit" => ChargeCategory::Credit,
        "Tax" => ChargeCategory::Tax,
        "Adjustment" => ChargeCategory::Adjustment,
        other => {
            tracing::warn!(
                "Unrecognized FOCUS ChargeCategory {:?}; filed as an Adjustment",
                other
            );
            ChargeCategory::Adjustment
        }
    }
}

/// FOCUS timestamps are ISO 8601 strings (`2026-09-01T00:00:00Z`); a cast
/// DuckDB timestamp renders as `2026-09-01 00:00:00`. Accept both, with or
/// without a sub-second part.
fn parse_instant(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(parsed.and_utc());
        }
    }
    None
}

/// Escape a value for interpolation into a SQL string literal.
fn sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// Payload bytes staged to a temporary directory, removed on drop.
struct StagingDir(PathBuf);

impl StagingDir {
    fn new(payloads: &[super::PayloadFile]) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("cloudbridge-focus-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        for payload in payloads {
            // Names came from `raw::write`, which rejected path escapes.
            std::fs::write(dir.join(&payload.name), &payload.bytes)?;
        }
        Ok(Self(dir))
    }

    fn glob(&self) -> String {
        self.0.join("*.parquet").to_string_lossy().into_owned()
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::raw::{PayloadFile, RawPart};
    use crate::cloud::BillingPeriod;

    /// A small FOCUS export as AWS Data Exports delivers it: ISO 8601
    /// string timestamps, JSON tags, and an `x_`-prefixed AWS column that
    /// the mapping ignores.
    fn focus_parquet(rows_sql: &str, extra_columns: &str) -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!("cloudbridge-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("focus-0.parquet");

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE focus (
                 BillingPeriodStart VARCHAR,
                 ChargePeriodStart VARCHAR,
                 ChargePeriodEnd VARCHAR,
                 ChargeCategory VARCHAR,
                 BillingAccountId VARCHAR,
                 ChargeDescription VARCHAR,
                 ServiceName VARCHAR,
                 ServiceCategory VARCHAR,
                 ResourceId VARCHAR,
                 ResourceName VARCHAR,
                 RegionId VARCHAR,
                 BilledCost DOUBLE,
                 EffectiveCost DOUBLE,
                 ListCost DOUBLE,
                 BillingCurrency VARCHAR,
                 PricingQuantity DOUBLE,
                 PricingUnit VARCHAR,
                 Tags VARCHAR{extra_columns}
             );
             {rows_sql};
             COPY focus TO '{}' (FORMAT PARQUET)",
            file.to_string_lossy()
        ))
        .unwrap();

        let bytes = std::fs::read(&file).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    }

    fn batch_with(bytes: Vec<u8>) -> RawBatch {
        RawBatch {
            provider: "AWS".to_string(),
            account_id: "acct-1".to_string(),
            period: BillingPeriod::new(2026, 9),
            batch_id: "b-1".to_string(),
            fetched_at: "2026-09-13T04:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new("export_listing", "s3://b/p", "[]")],
            payload_files: vec![PayloadFile {
                name: "focus-0.parquet".to_string(),
                bytes,
            }],
        }
    }

    #[test]
    fn focus_rows_map_to_ledger_charges() {
        let bytes = focus_parquet(
            "INSERT INTO focus VALUES
             ('2026-09-01T00:00:00Z', '2026-09-12T01:00:00Z', '2026-09-12T02:00:00Z',
              'Usage', '123456789012', 'EC2 instance hours',
              'Amazon Elastic Compute Cloud - Compute', 'Compute',
              'i-0abc123', 'web-server', 'ap-east-1',
              12.45, 10.20, 15.00, 'USD', 1.0, 'Hours',
              '{\"business_line\":\"platform\"}', 'ignored'),
             ('2026-09-01T00:00:00Z', '2026-09-12T00:00:00Z', '2026-09-13T00:00:00Z',
              'Credit', '123456789012', 'Promotional credit',
              'AWS Credits', 'Credits',
              NULL, NULL, NULL,
              -3.00, -3.00, 0.0, 'USD', NULL, NULL, NULL, 'ignored')",
            ", x_UsageAccountId VARCHAR",
        );

        let normalized = normalize(&batch_with(bytes)).unwrap();
        assert_eq!(normalized.charges.len(), 2);

        let usage = &normalized.charges[0];
        assert_eq!(usage.charge_category, ChargeCategory::Usage);
        assert_eq!(usage.billing_account_id.as_deref(), Some("123456789012"));
        assert_eq!(
            usage.service_name.as_deref(),
            Some("Amazon Elastic Compute Cloud - Compute")
        );
        assert_eq!(usage.resource_id.as_deref(), Some("i-0abc123"));
        assert_eq!(usage.region_id.as_deref(), Some("ap-east-1"));
        assert_eq!(usage.billed_cost, Some(12.45));
        assert_eq!(usage.effective_cost, Some(10.20));
        assert_eq!(usage.list_cost, Some(15.0));
        assert_eq!(usage.billing_currency, "USD");
        assert_eq!(usage.pricing_quantity, Some(1.0));
        assert_eq!(usage.pricing_unit.as_deref(), Some("Hours"));
        assert_eq!(
            usage.tags.as_deref(),
            Some("{\"business_line\":\"platform\"}")
        );
        assert_eq!(
            usage.charge_period_start.to_rfc3339(),
            "2026-09-12T01:00:00+00:00"
        );
        assert_eq!(
            usage.charge_period_end.to_rfc3339(),
            "2026-09-12T02:00:00+00:00"
        );

        let credit = &normalized.charges[1];
        assert_eq!(credit.charge_category, ChargeCategory::Credit);
        assert_eq!(credit.billed_cost, Some(-3.0));
        assert_eq!(credit.resource_id, None);
        assert_eq!(credit.tags, None);
    }

    #[test]
    fn a_column_the_export_omits_maps_to_null() {
        // No Tags, no PricingUnit, no extra AWS columns at all.
        let conn_dir =
            std::env::temp_dir().join(format!("cloudbridge-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&conn_dir).unwrap();
        let file = conn_dir.join("focus-0.parquet");
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE focus (
                 ChargePeriodStart VARCHAR, ChargePeriodEnd VARCHAR,
                 ChargeCategory VARCHAR, ServiceName VARCHAR,
                 BilledCost DOUBLE, BillingCurrency VARCHAR
             );
             INSERT INTO focus VALUES
             ('2026-09-12 01:00:00', '2026-09-12 02:00:00',
              'Tax', 'US Sales Tax', 0.75, 'USD');
             COPY focus TO '{}' (FORMAT PARQUET)",
            file.to_string_lossy()
        ))
        .unwrap();
        let bytes = std::fs::read(&file).unwrap();
        let _ = std::fs::remove_dir_all(&conn_dir);

        let normalized = normalize(&batch_with(bytes)).unwrap();
        assert_eq!(normalized.charges.len(), 1);
        let tax = &normalized.charges[0];
        assert_eq!(tax.charge_category, ChargeCategory::Tax);
        assert_eq!(tax.billed_cost, Some(0.75));
        assert_eq!(tax.tags, None);
        assert_eq!(tax.pricing_unit, None);
        assert_eq!(tax.effective_cost, None);
        // DuckDB-rendered timestamp without a 'T' also parses.
        assert_eq!(
            tax.charge_period_start.to_rfc3339(),
            "2026-09-12T01:00:00+00:00"
        );
    }

    #[test]
    fn a_batch_without_payloads_is_an_error() {
        let mut batch = batch_with(Vec::new());
        batch.payload_files.clear();
        assert!(normalize(&batch).is_err());
    }
}
