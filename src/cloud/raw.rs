//! Raw payload store.
//!
//! `fetch` writes what a provider actually returned, byte for byte, before
//! anything interprets it. `normalize` then reads from here rather than
//! from the network. Cost Explorer bills per request, so this is what makes
//! a schema change cheap: re-normalizing a corrected mapping over payloads
//! already on disk costs nothing.
//!
//! The layout is Hive-partitioned:
//!
//! ```text
//! raw/provider=<p>/account=<a>/billing_period=<YYYY-MM>/batch=<id>/part-0.parquet
//! ```
//!
//! The same path semantics work for a local directory and for an object
//! store, so P1's S3/OSS export channel replaces the `fetch` implementation
//! and leaves everything downstream alone.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use duckdb::{params, Connection};
use std::path::{Path, PathBuf};

use super::BillingPeriod;
use crate::ledger::schema::TIMESTAMP_FORMAT;

/// One response, exactly as the provider sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPart {
    /// Logical name of the call, unique within a batch — this is what
    /// `normalize` looks the payload up by.
    pub name: String,
    /// The request that produced it, for reproducing the call later.
    pub request: String,
    /// The response body, unchanged. Never parsed on the way in.
    pub body: String,
}

impl RawPart {
    pub fn new(
        name: impl Into<String>,
        request: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            request: request.into(),
            body: body.into(),
        }
    }
}

/// Everything one fetch of one account's billing period returned.
///
/// This is the whole input to [`super::BillingSource::normalize`]: keeping
/// `fetched_at` on the batch rather than reading the clock inside a
/// normalizer is what lets a normalizer stay a pure function.
#[derive(Debug, Clone)]
pub struct RawBatch {
    pub provider: String,
    pub account_id: String,
    pub period: BillingPeriod,
    pub batch_id: String,
    pub fetched_at: DateTime<Utc>,
    pub parts: Vec<RawPart>,
}

impl RawBatch {
    /// The payload stored under `name`.
    pub fn part(&self, name: &str) -> Option<&RawPart> {
        self.parts.iter().find(|part| part.name == name)
    }

    /// Directory this batch is stored under, below `root`.
    pub fn directory(&self, root: &Path) -> PathBuf {
        batch_directory(
            root,
            &self.provider,
            &self.account_id,
            &self.period,
            &self.batch_id,
        )
    }
}

/// `<root>/provider=<p>/account=<a>/billing_period=<YYYY-MM>/batch=<id>`
pub fn batch_directory(
    root: &Path,
    provider: &str,
    account_id: &str,
    period: &BillingPeriod,
    batch_id: &str,
) -> PathBuf {
    root.join(format!("provider={}", provider))
        .join(format!("account={}", account_id))
        .join(format!("billing_period={}", period.label()))
        .join(format!("batch={}", batch_id))
}

/// Write a batch as a single Parquet part. Returns the file's path, which
/// is what the ledger records as the batch's `source_ref`.
pub fn write(root: &Path, batch: &RawBatch) -> Result<PathBuf> {
    let directory = batch.directory(root);
    std::fs::create_dir_all(&directory)?;
    let file = directory.join("part-0.parquet");

    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "CREATE TABLE part (
             part_name  VARCHAR NOT NULL,
             request    VARCHAR NOT NULL,
             body       VARCHAR NOT NULL,
             fetched_at TIMESTAMP NOT NULL
         )",
    )?;

    let fetched_at = batch.fetched_at.format(TIMESTAMP_FORMAT).to_string();
    for part in &batch.parts {
        conn.execute(
            "INSERT INTO part VALUES (?, ?, ?, CAST(? AS TIMESTAMP))",
            params![part.name, part.request, part.body, fetched_at],
        )?;
    }

    conn.execute_batch(&format!(
        "COPY part TO '{}' (FORMAT PARQUET)",
        sql_literal(&file.to_string_lossy())
    ))?;

    tracing::info!("Raw: wrote {} payload(s) to {:?}", batch.parts.len(), file);
    Ok(file)
}

/// Read the payloads of a batch back, in the order they were written,
/// with the instant they were fetched at.
pub fn read(file: &Path) -> Result<(Vec<RawPart>, DateTime<Utc>)> {
    let conn = Connection::open_in_memory()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT part_name, request, body, CAST(fetched_at AS VARCHAR) FROM read_parquet('{}')",
        sql_literal(&file.to_string_lossy())
    ))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                RawPart {
                    name: row.get(0)?,
                    request: row.get(1)?,
                    body: row.get(2)?,
                },
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let fetched_at = rows
        .first()
        .map(|(_, stamp)| parse_timestamp(stamp))
        .transpose()?
        .unwrap_or_else(Utc::now);

    Ok((rows.into_iter().map(|(part, _)| part).collect(), fetched_at))
}

/// Read a stored batch back in full, so it can be normalized again without
/// paying for another fetch.
pub fn read_batch(
    root: &Path,
    provider: &str,
    account_id: &str,
    period: &BillingPeriod,
    batch_id: &str,
) -> Result<RawBatch> {
    let file = batch_directory(root, provider, account_id, period, batch_id).join("part-0.parquet");
    let (parts, fetched_at) = read(&file)?;

    Ok(RawBatch {
        provider: provider.to_string(),
        account_id: account_id.to_string(),
        period: *period,
        batch_id: batch_id.to_string(),
        fetched_at,
        parts,
    })
}

/// Parse a timestamp as DuckDB renders it, `YYYY-MM-DD HH:MM:SS` in UTC.
fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    let stamp = value.split('.').next().unwrap_or(value);
    Ok(
        chrono::NaiveDateTime::parse_from_str(stamp, TIMESTAMP_FORMAT)
            .map_err(|e| anyhow!("Unexpected timestamp {:?} in a raw batch: {}", value, e))?
            .and_utc(),
    )
}

/// Batch ids stored for one account and period, oldest first.
///
/// Re-normalizing reads the newest of these instead of paying for another
/// fetch.
pub fn batches(
    root: &Path,
    provider: &str,
    account_id: &str,
    period: &BillingPeriod,
) -> Result<Vec<String>> {
    let period_directory = root
        .join(format!("provider={}", provider))
        .join(format!("account={}", account_id))
        .join(format!("billing_period={}", period.label()));

    if !period_directory.exists() {
        return Ok(Vec::new());
    }

    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&period_directory)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_prefix("batch=") {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    Ok(ids)
}

/// Escape a value for interpolation into a SQL string literal.
///
/// Paths cannot be bound as parameters in `COPY ... TO` or `read_parquet`,
/// so they are interpolated; a path containing a quote must not end the
/// literal.
fn sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// Reject a value that would break out of its path segment.
///
/// Provider ids come from the registry, but account ids and period labels
/// reach here from the database, so the layout is checked rather than
/// assumed.
pub fn check_path_segment(value: &str, what: &str) -> Result<()> {
    if value.is_empty() || value.contains(['/', '\\', '\0']) || value == "." || value == ".." {
        return Err(anyhow!(
            "{} is not usable as a path segment: {:?}",
            what,
            value
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("cloudbridge-raw-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn batch(batch_id: &str, parts: Vec<RawPart>) -> RawBatch {
        RawBatch {
            provider: "AWS".to_string(),
            account_id: "acct-1".to_string(),
            period: BillingPeriod::new(2026, 8),
            batch_id: batch_id.to_string(),
            fetched_at: Utc::now(),
            parts,
        }
    }

    #[test]
    fn the_partition_layout_is_the_one_a_bucket_would_use() {
        let path = batch_directory(
            Path::new("/data/raw"),
            "AWS",
            "acct-1",
            &BillingPeriod::new(2026, 8),
            "b-1",
        );
        assert!(path.ends_with("provider=AWS/account=acct-1/billing_period=2026-08/batch=b-1"));
    }

    #[test]
    fn payloads_come_back_unchanged() {
        let dir = TempDir::new();
        // A body with quotes and newlines: nothing here is escaped or
        // reformatted on the way through Parquet.
        let body = "{\"Results\": [{\"Amount\": \"1.5\"}],\n \"note\": \"it's fine\"}";
        let written = batch(
            "b-1",
            vec![
                RawPart::new("cost_and_usage", "{\"Granularity\":\"DAILY\"}", body),
                RawPart::new("second", "{}", ""),
            ],
        );

        let file = write(&dir.0, &written).unwrap();
        assert!(file.ends_with("part-0.parquet"));

        let (parts, _) = read(&file).unwrap();
        assert_eq!(parts, written.parts);
    }

    #[test]
    fn a_stored_batch_can_be_normalized_again_without_refetching() {
        let dir = TempDir::new();
        let mut written = batch(
            "b-1",
            vec![RawPart::new("cost_and_usage", "{}", "{\"a\":1}")],
        );
        written.fetched_at = "2026-08-29T09:30:00Z".parse().unwrap();

        write(&dir.0, &written).unwrap();
        let reread =
            read_batch(&dir.0, "AWS", "acct-1", &BillingPeriod::new(2026, 8), "b-1").unwrap();

        assert_eq!(reread.parts, written.parts);
        assert_eq!(reread.fetched_at, written.fetched_at);
        assert_eq!(reread.batch_id, "b-1");
    }

    #[test]
    fn batches_are_listed_oldest_first_per_period() {
        let dir = TempDir::new();
        let period = BillingPeriod::new(2026, 8);

        for id in ["b-2", "b-1"] {
            write(&dir.0, &batch(id, vec![RawPart::new("p", "", "{}")])).unwrap();
        }
        // A different period must not show up in the listing.
        let mut other = batch("b-3", vec![RawPart::new("p", "", "{}")]);
        other.period = BillingPeriod::new(2026, 7);
        write(&dir.0, &other).unwrap();

        let ids = batches(&dir.0, "AWS", "acct-1", &period).unwrap();
        assert_eq!(ids, vec!["b-1".to_string(), "b-2".to_string()]);
    }

    #[test]
    fn an_unfetched_period_lists_nothing() {
        let dir = TempDir::new();
        let ids = batches(&dir.0, "AWS", "acct-1", &BillingPeriod::new(2026, 8)).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn a_path_segment_cannot_escape_its_directory() {
        assert!(check_path_segment("acct-1", "account id").is_ok());
        assert!(check_path_segment("..", "account id").is_err());
        assert!(check_path_segment("a/b", "account id").is_err());
        assert!(check_path_segment("", "account id").is_err());
    }
}
