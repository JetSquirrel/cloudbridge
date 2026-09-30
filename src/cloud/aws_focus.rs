//! AWS Data Exports (CUR 2.0, FOCUS 1.2 with AWS columns) → ledger rows.
//!
//! An export is delivered as Parquet or as gzipped CSV, whichever the
//! export was configured with. Neither suits the text-oriented bill-file
//! parsers, so this normalizer goes through an in-memory DuckDB: the
//! payload bytes are staged to a temporary directory and scanned with
//! `read_parquet` or `read_csv`. That is still a pure function of the
//! batch — no clock, no network, no database state — and the temp files
//! are gone when it returns.
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

/// The file formats an export can be delivered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Parquet,
    /// `TEXT_OR_CSV`, gzipped or not.
    Csv,
}

impl ExportFormat {
    /// The format of an export object, by its key. `None` for anything
    /// else the export writes beside the data, such as its manifests.
    pub fn of_key(key: &str) -> Option<Self> {
        let key = key.to_ascii_lowercase();
        if key.ends_with(".parquet") {
            Some(Self::Parquet)
        } else if key.ends_with(".csv.gz") || key.ends_with(".csv") {
            Some(Self::Csv)
        } else {
            None
        }
    }

    /// The suffix a staged payload of this format is named with, which is
    /// also how [`normalize`] tells the formats apart when it reads the
    /// batch back. DuckDB decompresses a `.gz` CSV by its name.
    pub fn payload_suffix(self, key: &str) -> &'static str {
        match self {
            Self::Parquet => ".parquet",
            Self::Csv if key.to_ascii_lowercase().ends_with(".gz") => ".csv.gz",
            Self::Csv => ".csv",
        }
    }
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
    ("SubAccountId", ColumnKind::Text),
    ("SubAccountName", ColumnKind::Text),
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
const IX_SUB_ACCOUNT: usize = 17;
const IX_SUB_ACCOUNT_NAME: usize = 18;

/// Turn a batch of FOCUS export payloads into ledger rows.
///
/// Pure — every input is in `batch`; the only I/O is staging the bytes to
/// a temporary directory for DuckDB to read.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    if batch.payload_files.is_empty() {
        return Err(anyhow!("Raw batch has no FOCUS export payloads"));
    }
    let format = batch_format(batch)?;
    let staging = StagingDir::new(&batch.payload_files)?;
    let files = staging
        .paths()
        .iter()
        .map(|path| format!("'{}'", sql_literal(path)))
        .collect::<Vec<_>>()
        .join(", ");

    let conn = Connection::open_in_memory()?;
    // CSV is read as text throughout: a sniffed TIMESTAMPTZ would come
    // back rendered in the local zone, and the casts below turn the text
    // into numbers where the ledger wants them.
    let scan = match format {
        ExportFormat::Parquet => format!("read_parquet([{files}], union_by_name = true)"),
        ExportFormat::Csv => {
            format!("read_csv([{files}], header = true, all_varchar = true, union_by_name = true)")
        }
    };

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
            // Empty text is no value: the CSV quotes an absent RegionId
            // as "", and an empty ResourceId must not become a resource.
            if available.contains(&name.to_lowercase()) {
                format!("CAST(NULLIF(CAST(\"{name}\" AS VARCHAR), '') AS {cast}) AS c{ix}")
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
                sub_account_id: row.get(IX_SUB_ACCOUNT)?,
                sub_account_name: row.get(IX_SUB_ACCOUNT_NAME)?,
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

/// The one format every payload of a batch is in. An export has a single
/// configured format, so a batch mixing two was not assembled from one.
fn batch_format(batch: &RawBatch) -> Result<ExportFormat> {
    let mut formats = batch.payload_files.iter().map(|payload| {
        ExportFormat::of_key(&payload.name)
            .ok_or_else(|| anyhow!("Not a FOCUS export file: {}", payload.name))
    });
    let first = formats.next().expect("checked non-empty")?;
    for format in formats {
        if format? != first {
            return Err(anyhow!(
                "FOCUS export batch mixes Parquet and CSV files; an export writes one format"
            ));
        }
    }
    Ok(first)
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
struct StagingDir {
    dir: PathBuf,
    files: Vec<String>,
}

impl StagingDir {
    fn new(payloads: &[super::PayloadFile]) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("cloudbridge-focus-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let mut files = Vec::with_capacity(payloads.len());
        for payload in payloads {
            // Names came from `raw::write`, which rejected path escapes.
            let path = dir.join(&payload.name);
            std::fs::write(&path, &payload.bytes)?;
            files.push(path.to_string_lossy().into_owned());
        }
        Ok(Self { dir, files })
    }

    fn paths(&self) -> &[String] {
        &self.files
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
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
                 Tags VARCHAR,
                 SubAccountId VARCHAR,
                 SubAccountName VARCHAR{extra_columns}
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
              '{\"business_line\":\"platform\"}',
              '210987654321', 'staging', 'ignored'),
             ('2026-09-01T00:00:00Z', '2026-09-12T00:00:00Z', '2026-09-13T00:00:00Z',
              'Credit', '123456789012', 'Promotional credit',
              'AWS Credits', 'Credits',
              NULL, NULL, NULL,
              -3.00, -3.00, 0.0, 'USD', NULL, NULL, NULL, NULL, NULL, 'ignored')",
            ", x_UsageAccountId VARCHAR",
        );

        let normalized = normalize(&batch_with(bytes)).unwrap();
        assert_eq!(normalized.charges.len(), 2);

        let usage = &normalized.charges[0];
        assert_eq!(usage.charge_category, ChargeCategory::Usage);
        assert_eq!(usage.billing_account_id.as_deref(), Some("123456789012"));
        assert_eq!(usage.sub_account_id.as_deref(), Some("210987654321"));
        assert_eq!(usage.sub_account_name.as_deref(), Some("staging"));
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
        assert_eq!(credit.sub_account_id, None);
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

    /// The header of a real FOCUS 1.2 `TEXT_OR_CSV` export, column for
    /// column.
    const CSV_HEADER: &str = "AvailabilityZone,BilledCost,BillingAccountId,BillingAccountName,BillingAccountType,BillingCurrency,BillingPeriodEnd,BillingPeriodStart,CapacityReservationId,CapacityReservationStatus,ChargeCategory,ChargeClass,ChargeDescription,ChargeFrequency,ChargePeriodEnd,ChargePeriodStart,CommitmentDiscountCategory,CommitmentDiscountId,CommitmentDiscountName,CommitmentDiscountQuantity,CommitmentDiscountStatus,CommitmentDiscountType,CommitmentDiscountUnit,ConsumedQuantity,ConsumedUnit,ContractedCost,ContractedUnitPrice,EffectiveCost,InvoiceId,InvoiceIssuerName,ListCost,ListUnitPrice,PricingCategory,PricingCurrency,PricingCurrencyContractedUnitPrice,PricingCurrencyEffectiveCost,PricingCurrencyListUnitPrice,PricingQuantity,PricingUnit,ProviderName,PublisherName,RegionId,RegionName,ResourceId,ResourceName,ResourceType,ServiceCategory,ServiceName,ServiceSubcategory,SkuId,SkuMeter,SkuPriceDetails,SkuPriceId,SubAccountId,SubAccountName,SubAccountType,Tags,x_Discounts,x_Operation,x_ServiceCode";

    /// One CSV line in [`CSV_HEADER`] order. A column not named is left
    /// empty, the way the export leaves an unused one; values are written
    /// as given, so quoting is the caller's, as it is AWS's.
    fn csv_line(values: &[(&str, &str)]) -> String {
        CSV_HEADER
            .split(',')
            .map(|column| {
                values
                    .iter()
                    .find(|(name, _)| *name == column)
                    .map_or("", |(_, value)| *value)
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn gzip(text: &str) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn a_gzipped_csv_export_maps_like_parquet() {
        let usage = csv_line(&[
            ("BilledCost", "6.123E-7"),
            ("BillingAccountId", "123456789012"),
            ("BillingCurrency", "USD"),
            ("ChargeCategory", "Usage"),
            (
                "ChargeDescription",
                "First 5GB-mo per month of logs storage is free.",
            ),
            ("ChargePeriodEnd", "2026-09-01T01:00:00.000Z"),
            ("ChargePeriodStart", "2026-09-01T00:00:00.000Z"),
            ("InvoiceIssuerName", "\"Amazon Web Services, Inc.\""),
            ("PricingQuantity", "6.123E-7"),
            ("PricingUnit", "GB-Months"),
            ("RegionId", "ap-east-1"),
            (
                "ResourceId",
                "arn:aws:logs:ap-east-1:123456789012:log-group:/aws/lambda/api",
            ),
            ("ResourceType", "\"\""),
            ("ServiceName", "AmazonCloudWatch"),
            ("SubAccountId", "210987654321"),
            ("SubAccountName", "staging"),
            ("Tags", "{}"),
        ]);
        // A tax row: RegionId quoted empty, no resource at all.
        let tax = csv_line(&[
            ("BilledCost", "0.0"),
            ("BillingAccountId", "123456789012"),
            ("BillingCurrency", "USD"),
            ("ChargeCategory", "Tax"),
            ("ChargePeriodEnd", "2026-10-01T00:00:00.000Z"),
            ("ChargePeriodStart", "2026-09-01T00:00:00.000Z"),
            ("RegionId", "\"\""),
            ("RegionName", "\"\""),
            ("ServiceName", "AWS Amplify"),
            ("Tags", "{}"),
        ]);
        let text = format!("{CSV_HEADER}\n{usage}\n{tax}\n");

        let mut batch = batch_with(Vec::new());
        batch.payload_files = vec![PayloadFile {
            name: "focus-0.csv.gz".to_string(),
            bytes: gzip(&text),
        }];
        let normalized = normalize(&batch).unwrap();
        assert_eq!(normalized.charges.len(), 2);

        let usage = &normalized.charges[0];
        assert_eq!(usage.charge_category, ChargeCategory::Usage);
        assert_eq!(usage.billed_cost, Some(6.123e-7));
        assert_eq!(usage.pricing_quantity, Some(6.123e-7));
        assert_eq!(
            usage.resource_id.as_deref(),
            Some("arn:aws:logs:ap-east-1:123456789012:log-group:/aws/lambda/api")
        );
        assert_eq!(usage.sub_account_id.as_deref(), Some("210987654321"));
        assert_eq!(usage.tags, None);
        // Read as the text it is, not as a zone-shifted TIMESTAMPTZ.
        assert_eq!(
            usage.charge_period_start.to_rfc3339(),
            "2026-09-01T00:00:00+00:00"
        );

        let tax = &normalized.charges[1];
        assert_eq!(tax.charge_category, ChargeCategory::Tax);
        assert_eq!(tax.region_id, None);
        assert_eq!(tax.resource_id, None);
        assert_eq!(tax.effective_cost, None);
    }

    #[test]
    fn a_batch_mixing_formats_is_refused() {
        let mut batch = batch_with(Vec::new());
        batch.payload_files = vec![
            PayloadFile {
                name: "focus-0.csv.gz".to_string(),
                bytes: gzip(CSV_HEADER),
            },
            PayloadFile {
                name: "focus-1.parquet".to_string(),
                bytes: Vec::new(),
            },
        ];
        assert!(normalize(&batch).is_err());
    }

    #[test]
    fn export_objects_are_told_apart_by_key() {
        let data = "p/cloudbridge/data/billing_period=2026-09/cloudbridge-00001.csv.gz";
        let format = ExportFormat::of_key(data).unwrap();
        assert_eq!(format, ExportFormat::Csv);
        assert_eq!(format.payload_suffix(data), ".csv.gz");
        assert_eq!(
            ExportFormat::of_key("data/BILLING_PERIOD=2026-09/part-0.snappy.parquet"),
            Some(ExportFormat::Parquet)
        );
        assert_eq!(
            ExportFormat::of_key("p/metadata/billing_period=2026-09/cloudbridge-Manifest.json"),
            None
        );
    }
}
