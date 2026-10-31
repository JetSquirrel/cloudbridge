//! The resource inventory: a corkscrew scan copied into the ledger.
//!
//! corkscrew (github.com/jlgore/corkscrew, MIT) is an external tool the
//! user installs and runs; it scans an AWS or Cloudflare account into a
//! DuckDB file of its own. An import reads that file through a separate
//! read-only connection — nothing attaches it to the ledger's — takes the
//! newest complete scan of each, and replaces `dim_resource` and
//! `inventory_scan` with them in one transaction. Nothing reads corkscrew's file afterwards:
//! its schema is its own to change, and it writes with a newer DuckDB.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use duckdb::{params, AccessMode, Config, Connection};

use super::schema::TIMESTAMP_FORMAT;
use super::with_connection;
use crate::cloud::corkscrew;
use crate::model::{InventoryResource, InventoryScope};

/// Replace the inventory with the newest complete scan in each of the
/// corkscrew databases at `paths` — one per scanned account, of whichever
/// provider scanned it — merged into one inventory.
pub fn import_scans(paths: &[PathBuf]) -> Result<InventoryScope> {
    let mut scans = Vec::with_capacity(paths.len());
    let mut resources = Vec::new();
    for path in paths {
        let config = Config::default().access_mode(AccessMode::ReadOnly)?;
        let corkscrew = Connection::open_with_flags(path, config)
            .map_err(|e| anyhow!("Cannot open the scan at {}: {}", path.display(), e))?;
        let (scan, found) = read_scan(&corkscrew)?;
        scans.push(scan);
        resources.extend(found);
    }
    let scope = merged_scope(&scans, &resources, paths)
        .ok_or_else(|| anyhow!("There was no scan to import"))?;
    with_connection(|conn| write_inventory(conn, &scope, &resources))?;

    tracing::info!(
        "Inventory: imported {} resources from {} scan(s)",
        scope.resource_count,
        scans.len()
    );
    Ok(scope)
}

/// One scope for scans of several accounts: the earliest scan time, since
/// the inventory is no newer than its oldest part, and every region any
/// of them covered.
fn merged_scope(
    scans: &[Scan],
    resources: &[InventoryResource],
    paths: &[PathBuf],
) -> Option<InventoryScope> {
    let first = scans.first()?;
    let mut regions: Vec<String> = scans.iter().flat_map(|s| s.regions.clone()).collect();
    regions.sort();
    regions.dedup();
    Some(InventoryScope {
        scan_id: scans
            .iter()
            .map(|s| s.scan_id.as_str())
            .collect::<Vec<_>>()
            .join("+"),
        scanned_at: scans
            .iter()
            .map(|s| s.scanned_at)
            .min()
            .unwrap_or(first.scanned_at),
        imported_at: Utc::now(),
        source_path: paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        regions,
        resource_count: resources.len() as i64,
        // The scans an import reads are the ones CloudBridge just ran, with
        // the release it installs.
        scanner: Some(format!("corkscrew {}", crate::cloud::corkscrew::RELEASE)),
        complete: scans.iter().all(|s| s.complete),
    })
}

/// The scan an import takes, before it is stamped with when and where from.
#[derive(Debug, PartialEq)]
pub(crate) struct Scan {
    pub scan_id: String,
    pub scanned_at: DateTime<Utc>,
    pub regions: Vec<String>,
    /// `snapshot_complete`: every service and type was observed.
    pub complete: bool,
}

/// Read the newest usable scan out of a corkscrew database, by a plugin
/// CloudBridge knows ([`corkscrew::PROVIDERS`]); its resources are filed
/// under that plugin's source.
///
/// A complete scan (`snapshot_complete`) is taken, and so is a `partial`
/// one: since corkscrew reports a failed describe as an error, one
/// throttled call makes a scan partial although it listed everything. A
/// partial scan comes back with `complete: false`, and Insights then calls
/// nothing missing from it. A failed scan, or one still running, is not
/// taken.
pub(crate) fn read_scan(corkscrew: &Connection) -> Result<(Scan, Vec<InventoryResource>)> {
    let has_tables: i64 = corkscrew.query_row(
        "SELECT count(*) FROM duckdb_tables()
         WHERE table_name IN ('scan_metadata', 'resource_observations')",
        [],
        |row| row.get(0),
    )?;
    if has_tables < 2 {
        return Err(anyhow!(
            "Not a corkscrew database: it has no scan_metadata and resource_observations tables"
        ));
    }

    // `status` arrived in corkscrew with partial scans; a database without
    // it has only complete ones to offer.
    let has_status = has_column(corkscrew, "scan_metadata", "status")?;
    let usable = if has_status {
        "(coalesce(snapshot_complete, false) OR status = 'partial')"
    } else {
        "coalesce(snapshot_complete, false)"
    };
    let known = corkscrew::PROVIDERS
        .iter()
        .map(|provider| format!("'{}'", provider.plugin))
        .collect::<Vec<_>>()
        .join(", ");
    let scan = corkscrew
        .query_row(
            &format!(
                "SELECT id,
                        CAST(coalesce(scan_end_time, scan_start_time) AS VARCHAR),
                        CAST(regions AS VARCHAR),
                        lower(provider),
                        coalesce(snapshot_complete, false)
                 FROM scan_metadata
                 WHERE lower(provider) IN ({known}) AND {usable}
                 ORDER BY coalesce(scan_end_time, scan_start_time) DESC
                 LIMIT 1"
            ),
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )
        .map_err(|e| match e {
            duckdb::Error::QueryReturnedNoRows => {
                anyhow!("The corkscrew database has no finished scan to import")
            }
            other => other.into(),
        })?;
    let (scan_id, scanned_at, regions, plugin, complete) = scan;
    let provider =
        corkscrew::provider_for_plugin(&plugin).expect("the query only returns a known plugin");
    let regions: Vec<String> = regions
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(|e| anyhow!("Scan {} lists its regions unreadably: {}", scan_id, e))?
        .unwrap_or_default();

    // corkscrew records resources' own times since its schema 7.
    let times = if has_column(corkscrew, "resource_observations", "created_at")? {
        "CAST(created_at AS VARCHAR), CAST(modified_at AS VARCHAR)"
    } else {
        "NULL::VARCHAR, NULL::VARCHAR"
    };
    let mut stmt = corkscrew.prepare(&format!(
        "SELECT DISTINCT ON (resource_id)
                account_id, resource_id, arn, type, location, name,
                CAST(tags AS VARCHAR), CAST(raw_data AS VARCHAR), {times}
         FROM resource_observations
         WHERE scan_id = ? AND lower(provider) = ?
         ORDER BY resource_id"
    ))?;
    let resources = stmt
        .query_map(params![scan_id, provider.plugin], |row| {
            Ok(InventoryResource {
                provider: provider.source.to_string(),
                cloud_account_id: present(row.get(0)?),
                resource_id: row.get(1)?,
                // corkscrew's arn column falls back to the id when a
                // scanner had no ARN; only a real one is kept as such.
                arn: present(row.get(2)?).filter(|arn: &String| arn.starts_with("arn:")),
                resource_type: row.get(3)?,
                region: present(row.get(4)?),
                name: present(row.get(5)?),
                tags: json_object(row.get(6)?),
                properties: json_object(row.get(7)?),
                created_at: timestamp(row.get(8)?),
                modified_at: timestamp(row.get(9)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok((
        Scan {
            scan_id,
            scanned_at: super::parse_timestamp(&scanned_at)?,
            regions,
            complete,
        },
        resources,
    ))
}

/// Replace the inventory and its scan record in one transaction.
pub(crate) fn write_inventory(
    conn: &mut Connection,
    scope: &InventoryScope,
    resources: &[InventoryResource],
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM dim_resource", [])?;
    tx.execute("DELETE FROM inventory_scan", [])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO dim_resource
             (provider, resource_id, cloud_account_id, arn, resource_type, region, name,
              tags, properties, created_at, modified_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP))",
        )?;
        for resource in resources {
            stmt.execute(params![
                resource.provider,
                resource.resource_id,
                resource.cloud_account_id,
                resource.arn,
                resource.resource_type,
                resource.region,
                resource.name,
                resource.tags,
                resource.properties,
                resource
                    .created_at
                    .map(|t| t.format(TIMESTAMP_FORMAT).to_string()),
                resource
                    .modified_at
                    .map(|t| t.format(TIMESTAMP_FORMAT).to_string()),
            ])?;
        }
    }
    tx.execute(
        "INSERT INTO inventory_scan
         (scan_id, scanned_at, imported_at, source_path, regions, resource_count, scanner,
          complete)
         VALUES (?, CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP), ?, ?, ?, ?, ?)",
        params![
            scope.scan_id,
            scope.scanned_at.format(TIMESTAMP_FORMAT).to_string(),
            scope.imported_at.format(TIMESTAMP_FORMAT).to_string(),
            scope.source_path,
            serde_json::to_string(&scope.regions)?,
            scope.resource_count,
            scope.scanner,
            scope.complete,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// A corkscrew timestamp as text, or nothing when it is absent or does
/// not parse: a resource's own time is a detail, not a reason to refuse
/// the scan.
fn timestamp(value: Option<String>) -> Option<DateTime<Utc>> {
    present(value).and_then(|v| super::parse_timestamp(&v).ok())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM duckdb_columns() WHERE table_name = ? AND column_name = ?",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Text that says something, or nothing.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// JSON object text, or nothing for JSON `null` and an empty object.
fn json_object(value: Option<String>) -> Option<String> {
    present(value).filter(|v| v != "null" && v != "{}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// corkscrew's two tables, as much of them as an import reads.
    fn corkscrew_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE scan_metadata (
                   id VARCHAR PRIMARY KEY, provider VARCHAR, regions JSON,
                   scan_start_time TIMESTAMP, scan_end_time TIMESTAMP,
                   snapshot_complete BOOLEAN);
               CREATE TABLE resource_observations (
                   scan_id VARCHAR, provider VARCHAR, resource_id VARCHAR, name VARCHAR,
                   type VARCHAR, service VARCHAR, location VARCHAR, account_id VARCHAR,
                   arn VARCHAR, parent_id VARCHAR, tags JSON, attributes JSON,
                   raw_data JSON, semantic_hash VARCHAR, observed_at TIMESTAMP);
               INSERT INTO scan_metadata VALUES
                   ('old', 'aws', '["ap-east-1"]', '2026-09-01 00:00:00', '2026-09-01 00:05:00', true),
                   ('new', 'aws', '["ap-east-1","us-east-1"]', '2026-09-30 18:13:00', '2026-09-30 18:14:19.36241', true),
                   ('partial', 'aws', '["ap-east-1"]', '2026-10-01 00:00:00', NULL, false);
               INSERT INTO resource_observations VALUES
                   ('new', 'aws', 'arn:aws:ec2:us-east-1:123456789012:instance/i-0abc', 'web',
                    'ec2:instance', 'ec2', 'us-east-1', '123456789012',
                    'arn:aws:ec2:us-east-1:123456789012:instance/i-0abc', NULL,
                    '{"owner":"platform"}', NULL, '{"State":{"Name":"stopped"}}', 'h1',
                    '2026-09-30 18:14:00'),
                   ('new', 'aws', 'my-bucket', 'my-bucket', 's3:bucket', 's3', 'global',
                    '123456789012', 'my-bucket', NULL, '{}', NULL, 'null', 'h2',
                    '2026-09-30 18:14:00'),
                   ('old', 'aws', 'arn:aws:kms:ap-east-1:123456789012:key/gone', NULL,
                    'kms:key', 'kms', 'ap-east-1', '123456789012', NULL, NULL, NULL, NULL,
                    NULL, 'h3', '2026-09-01 00:04:00');"#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn the_newest_complete_scan_is_read() {
        let (scan, resources) = read_scan(&corkscrew_fixture()).unwrap();
        assert_eq!(scan.scan_id, "new");
        assert_eq!(scan.regions, ["ap-east-1", "us-east-1"]);
        assert_eq!(
            scan.scanned_at.format(TIMESTAMP_FORMAT).to_string(),
            "2026-09-30 18:14:19"
        );
        // The old scan's KMS key is not carried over.
        assert_eq!(resources.len(), 2);

        let bucket = resources
            .iter()
            .find(|r| r.resource_id == "my-bucket")
            .unwrap();
        // corkscrew's arn falls back to the id: not an ARN, so not kept.
        assert_eq!(bucket.arn, None);
        assert_eq!(bucket.tags, None);
        assert_eq!(bucket.properties, None);
        assert_eq!(bucket.provider, "AWS");

        let instance = resources
            .iter()
            .find(|r| r.resource_type == "ec2:instance")
            .unwrap();
        assert_eq!(instance.cloud_account_id.as_deref(), Some("123456789012"));
        assert_eq!(instance.tags.as_deref(), Some(r#"{"owner":"platform"}"#));
        assert!(instance.properties.as_deref().unwrap().contains("stopped"));
    }

    #[test]
    fn a_database_that_is_not_corkscrews_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        let error = read_scan(&conn).unwrap_err().to_string();
        assert!(error.contains("Not a corkscrew database"), "{error}");
    }

    #[test]
    fn a_database_without_a_finished_scan_is_refused() {
        let conn = corkscrew_fixture();
        conn.execute_batch("UPDATE scan_metadata SET snapshot_complete = false")
            .unwrap();
        let error = read_scan(&conn).unwrap_err().to_string();
        assert!(error.contains("no finished scan"), "{error}");
    }

    /// corkscrew since its schema 7: a `status`, so a partial scan can be
    /// told from a failed one, and each resource's own times.
    fn current_corkscrew() -> Connection {
        let conn = corkscrew_fixture();
        conn.execute_batch(
            "ALTER TABLE scan_metadata ADD COLUMN status VARCHAR;
             UPDATE scan_metadata SET status = CASE WHEN snapshot_complete THEN 'completed'
                                                     ELSE 'running' END;
             ALTER TABLE resource_observations ADD COLUMN created_at TIMESTAMP;
             ALTER TABLE resource_observations ADD COLUMN modified_at TIMESTAMP;
             UPDATE resource_observations
                SET created_at = '2025-03-04 05:06:07', modified_at = '2026-09-01 00:00:00'
              WHERE type = 'ec2:instance';",
        )
        .unwrap();
        conn
    }

    #[test]
    fn a_resource_s_own_times_are_read_when_the_scan_has_them() {
        let (_, resources) = read_scan(&current_corkscrew()).unwrap();
        let instance = resources
            .iter()
            .find(|r| r.resource_type == "ec2:instance")
            .unwrap();
        assert_eq!(
            instance
                .created_at
                .unwrap()
                .format(TIMESTAMP_FORMAT)
                .to_string(),
            "2025-03-04 05:06:07"
        );
        assert!(instance.modified_at.is_some());
        let bucket = resources
            .iter()
            .find(|r| r.resource_id == "my-bucket")
            .unwrap();
        assert_eq!(bucket.created_at, None);
        // An older database has no such columns, and no times.
        let (_, old) = read_scan(&corkscrew_fixture()).unwrap();
        assert!(old.iter().all(|r| r.created_at.is_none()));
    }

    /// A partial scan is taken, as incomplete; a failed or running one
    /// is not.
    #[test]
    fn a_partial_scan_is_taken_as_incomplete() {
        let conn = current_corkscrew();
        conn.execute_batch(
            "INSERT INTO scan_metadata VALUES
                 ('throttled', 'aws', '[\"us-east-1\"]', '2026-10-02 00:00:00',
                  '2026-10-02 00:04:00', false, 'partial'),
                 ('failed', 'aws', '[\"us-east-1\"]', '2026-10-03 00:00:00',
                  '2026-10-03 00:01:00', false, 'failed');",
        )
        .unwrap();
        let (scan, _) = read_scan(&conn).unwrap();
        assert_eq!(scan.scan_id, "throttled");
        assert!(!scan.complete);

        let (scan, _) = read_scan(&current_corkscrew()).unwrap();
        assert_eq!(scan.scan_id, "new");
        assert!(scan.complete);
    }

    /// The Cloudflare plugin records its scan and resources under its own
    /// name; they are filed under the Cloudflare source, account-wide.
    #[test]
    fn a_cloudflare_scan_is_read_as_cloudflare() {
        let conn = corkscrew_fixture();
        conn.execute_batch(
            r#"INSERT INTO scan_metadata VALUES
                   ('cf', 'cloudflare', '["global"]', '2026-10-08 06:00:00', '2026-10-08 06:01:00', true);
               INSERT INTO resource_observations VALUES
                   ('cf', 'cloudflare', 'do-namespace-1', 'alarm-loop', 'durable_object_namespace',
                    'data', 'global', '023e105f4ecef8ad9ca31a8372d0c353', NULL, NULL, NULL, NULL,
                    '{"script":"ticker"}', 'h4', '2026-10-08 06:00:30');"#,
        )
        .unwrap();

        let (scan, resources) = read_scan(&conn).unwrap();
        assert_eq!(scan.scan_id, "cf");
        assert_eq!(scan.regions, ["global"]);
        assert_eq!(resources.len(), 1, "the AWS scan's resources stay out");
        let namespace = &resources[0];
        assert_eq!(namespace.provider, "Cloudflare");
        assert_eq!(namespace.resource_type, "durable_object_namespace");
        assert_eq!(
            namespace.cloud_account_id.as_deref(),
            Some("023e105f4ecef8ad9ca31a8372d0c353")
        );
    }

    /// A scan by a plugin CloudBridge does not import is no scan at all.
    #[test]
    fn a_scan_by_an_unknown_plugin_is_not_taken() {
        let conn = corkscrew_fixture();
        conn.execute_batch(
            r#"DELETE FROM scan_metadata;
               INSERT INTO scan_metadata VALUES
                   ('gcp', 'gcp', '["us-central1"]', '2026-10-08 06:00:00', '2026-10-08 06:01:00', true);"#,
        )
        .unwrap();
        assert!(read_scan(&conn).is_err());
    }

    #[test]
    fn an_import_replaces_the_inventory_whole() {
        let mut ledger = Connection::open_in_memory().unwrap();
        super::super::schema::apply(&ledger).unwrap();
        let (scan, resources) = read_scan(&corkscrew_fixture()).unwrap();
        let scope = InventoryScope {
            scan_id: scan.scan_id,
            scanned_at: scan.scanned_at,
            imported_at: scan.scanned_at,
            source_path: "/tmp/corkscrew.duckdb".to_string(),
            regions: scan.regions,
            resource_count: resources.len() as i64,
            scanner: Some("corkscrew test".to_string()),
            complete: false,
        };
        write_inventory(&mut ledger, &scope, &resources).unwrap();
        write_inventory(&mut ledger, &scope, &resources[..1]).unwrap();

        let count: i64 = ledger
            .query_row("SELECT count(*) FROM dim_resource", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let scans: i64 = ledger
            .query_row("SELECT count(*) FROM inventory_scan", [], |row| row.get(0))
            .unwrap();
        assert_eq!(scans, 1);
        let complete: bool = ledger
            .query_row("SELECT complete FROM inventory_scan", [], |row| row.get(0))
            .unwrap();
        assert!(!complete);
    }
}
