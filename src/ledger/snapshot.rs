//! Month-end snapshots in DuckDB: `billing_snapshot` and its lines.
//!
//! A freeze copies the period's rows out of `fct_charge`, which whole-period
//! replacement would otherwise delete. Nothing here updates a snapshot. The
//! only deletes are of a whole account, with the rest of its history.
//!
//! The arithmetic is [`crate::analytics::snapshot`].

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use duckdb::{params, Connection, Row};

use super::schema::{NORMALIZED_VIEW, TIMESTAMP_FORMAT};
use super::{parse_timestamp, with_connection, with_connection_ref};
use crate::analytics::snapshot::{self, BatchSource};
use crate::model::{
    BillingSnapshot, Channel, Charge, ChargeCategory, CostBasis, FreezeOutcome, PeriodKey,
    SnapshotLine,
};

/// The columns a line keeps, as `fct_charge` and the snapshot table both name
/// them. `fx_rate` is the view's and is added where a query needs it.
const LINE_COLUMNS: &str = "charge_id, billing_account_id, charge_period_start, charge_period_end,
     charge_category, charge_description, service_name, service_category, resource_id,
     resource_name, region_id, billed_cost, effective_cost, list_cost, billing_currency,
     cost_basis, pricing_quantity, pricing_unit, tags, sub_account_id, sub_account_name,
     service_subcategory, x_service_code, x_model, consumed_quantity, consumed_unit, sku_meter,
     sku_price_details, principal_id, credential_id, requester_details";

/// [`LINE_COLUMNS`] as [`line_from`] reads them.
const LINE_SELECT: &str = "charge_id, billing_account_id, CAST(charge_period_start AS VARCHAR),
     CAST(charge_period_end AS VARCHAR), charge_category, charge_description, service_name,
     service_category, resource_id, resource_name, region_id, billed_cost, effective_cost,
     list_cost, billing_currency, cost_basis, pricing_quantity, pricing_unit, tags,
     sub_account_id, sub_account_name, service_subcategory, x_service_code, x_model,
     consumed_quantity, consumed_unit, sku_meter, sku_price_details, principal_id, credential_id,
     requester_details, fx_rate";

const HEADER_SELECT: &str = "snapshot_id, provider, account_id, billing_period, revision,
     CAST(frozen_at AS VARCHAR), batch_id, channel, source_ref,
     CAST(batch_completed_at AS VARCHAR), row_count, checksum, reporting_currency, period_open,
     totals, fx_rates";

/// Freeze a period's current rows as a new revision.
pub fn freeze_period(key: &PeriodKey) -> Result<FreezeOutcome> {
    with_connection(|conn| freeze_of(conn, key, Utc::now()))
}

/// A period's revisions, newest first.
pub fn snapshots_of(key: &PeriodKey) -> Result<Vec<BillingSnapshot>> {
    with_connection_ref(|conn| snapshots_of_key(conn, key))
}

/// The lines frozen under one snapshot.
pub fn snapshot_lines(snapshot_id: &str) -> Result<Vec<SnapshotLine>> {
    with_connection_ref(|conn| stored_lines(conn, snapshot_id))
}

/// The lines the ledger holds for a period now.
pub fn current_lines(key: &PeriodKey) -> Result<Vec<SnapshotLine>> {
    with_connection_ref(|conn| read_current(conn, key))
}

/// Where the ledger's current rows for a period came from.
pub fn current_source(key: &PeriodKey) -> Result<BatchSource> {
    with_connection_ref(|conn| batch_source(conn, key))
}

/// The periods an account has rows or a snapshot for, newest first.
pub fn periods_of(account_id: &str) -> Result<Vec<PeriodKey>> {
    with_connection_ref(|conn| periods_of_account(conn, account_id))
}

fn periods_of_account(conn: &Connection, account_id: &str) -> Result<Vec<PeriodKey>> {
    let mut stmt = conn.prepare(
        "SELECT provider, account_id, billing_period FROM ingest_batch
         WHERE account_id = ? AND status = 'complete' AND row_count > 0
         UNION
         SELECT provider, account_id, billing_period FROM billing_snapshot
         WHERE account_id = ?
         ORDER BY billing_period DESC",
    )?;
    let keys = stmt
        .query_map(params![account_id, account_id], |row| {
            Ok(PeriodKey::new(
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(keys)
}

pub(crate) fn freeze_of(
    conn: &mut Connection,
    key: &PeriodKey,
    now: DateTime<Utc>,
) -> Result<FreezeOutcome> {
    let lines = read_current(conn, key)?;
    if lines.is_empty() {
        return Ok(FreezeOutcome::Empty);
    }

    let revisions = snapshots_of_key(conn, key)?;
    if let Some(latest) = revisions.first() {
        if latest.checksum == snapshot::checksum(&lines) {
            return Ok(FreezeOutcome::Unchanged(latest.clone()));
        }
    }

    let reporting_currency: String = conn.query_row(
        &format!(
            "SELECT any_value(reporting_currency) FROM {NORMALIZED_VIEW}
             WHERE provider = ? AND account_id = ? AND billing_period = ?"
        ),
        params![key.provider, key.account_id, key.billing_period],
        |row| row.get(0),
    )?;
    let revision = revisions.first().map_or(1, |latest| latest.revision + 1);
    let frozen = snapshot::build(
        key,
        revision,
        &lines,
        batch_source(conn, key)?,
        &reporting_currency,
        now,
    )?;

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO billing_snapshot
         (snapshot_id, provider, account_id, billing_period, revision, frozen_at, batch_id,
          channel, source_ref, batch_completed_at, row_count, checksum, reporting_currency,
          period_open, totals, fx_rates)
         VALUES (?, ?, ?, ?, ?, CAST(? AS TIMESTAMP), ?, ?, ?, CAST(? AS TIMESTAMP),
                 ?, ?, ?, ?, ?, ?)",
        params![
            frozen.snapshot_id,
            key.provider,
            key.account_id,
            key.billing_period,
            frozen.revision,
            frozen.frozen_at.format(TIMESTAMP_FORMAT).to_string(),
            frozen.batch_id,
            frozen.channel.as_str(),
            frozen.source_ref,
            frozen
                .batch_completed_at
                .map(|at| at.format(TIMESTAMP_FORMAT).to_string()),
            frozen.row_count as i64,
            frozen.checksum,
            frozen.reporting_currency,
            frozen.period_open,
            serde_json::to_string(&frozen.totals)?,
            serde_json::to_string(&frozen.rates)?,
        ],
    )?;
    let copied = tx.execute(
        &format!(
            "INSERT INTO billing_snapshot_charge (snapshot_id, {LINE_COLUMNS}, fx_rate)
             SELECT ?, {LINE_COLUMNS}, fx_rate FROM {NORMALIZED_VIEW}
             WHERE provider = ? AND account_id = ? AND billing_period = ?"
        ),
        params![
            frozen.snapshot_id,
            key.provider,
            key.account_id,
            key.billing_period
        ],
    )?;
    if copied != lines.len() {
        // Dropping `tx` rolls the header back with the lines.
        return Err(anyhow!(
            "Copied {} of {} charges of {}; nothing was frozen",
            copied,
            lines.len(),
            key.billing_period
        ));
    }
    tx.commit()?;

    tracing::info!(
        "Ledger: froze {}/{} {} as revision {} ({} charges)",
        key.provider,
        key.account_id,
        key.billing_period,
        frozen.revision,
        copied
    );
    Ok(FreezeOutcome::Frozen(frozen))
}

fn batch_source(conn: &Connection, key: &PeriodKey) -> Result<BatchSource> {
    let mut stmt = conn.prepare(
        "SELECT batch_id, channel, source_ref, CAST(completed_at AS VARCHAR) FROM ingest_batch
         WHERE provider = ? AND account_id = ? AND billing_period = ? AND status = 'complete'
         LIMIT 1",
    )?;
    let found = stmt
        .query_map(
            params![key.provider, key.account_id, key.billing_period],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )?
        .next()
        .transpose()?;
    match found {
        Some((batch_id, channel, source_ref, completed_at)) => Ok(BatchSource {
            batch_id: Some(batch_id),
            channel: Channel::from_stored(channel.as_deref()),
            source_ref,
            completed_at: completed_at.as_deref().map(parse_timestamp).transpose()?,
        }),
        None => Ok(BatchSource::unknown()),
    }
}

fn snapshots_of_key(conn: &Connection, key: &PeriodKey) -> Result<Vec<BillingSnapshot>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {HEADER_SELECT} FROM billing_snapshot
         WHERE provider = ? AND account_id = ? AND billing_period = ?
         ORDER BY revision DESC"
    ))?;
    let mut rows = stmt.query(params![key.provider, key.account_id, key.billing_period])?;
    let mut snapshots = Vec::new();
    while let Some(row) = rows.next()? {
        snapshots.push(header_from(row)?);
    }
    Ok(snapshots)
}

fn header_from(row: &Row<'_>) -> Result<BillingSnapshot> {
    let totals: String = row.get(14)?;
    let rates: String = row.get(15)?;
    Ok(BillingSnapshot {
        snapshot_id: row.get(0)?,
        key: PeriodKey::new(
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ),
        revision: row.get(4)?,
        frozen_at: parse_timestamp(&row.get::<_, String>(5)?)?,
        batch_id: row.get(6)?,
        channel: Channel::from_stored(row.get::<_, Option<String>>(7)?.as_deref()),
        source_ref: row.get(8)?,
        batch_completed_at: row
            .get::<_, Option<String>>(9)?
            .as_deref()
            .map(parse_timestamp)
            .transpose()?,
        row_count: row.get::<_, i64>(10)? as u64,
        checksum: row.get(11)?,
        reporting_currency: row.get(12)?,
        period_open: row.get(13)?,
        totals: serde_json::from_str(&totals)?,
        rates: serde_json::from_str(&rates)?,
    })
}

fn read_current(conn: &Connection, key: &PeriodKey) -> Result<Vec<SnapshotLine>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {LINE_SELECT} FROM {NORMALIZED_VIEW}
         WHERE provider = ? AND account_id = ? AND billing_period = ?
         ORDER BY charge_id"
    ))?;
    let mut rows = stmt.query(params![key.provider, key.account_id, key.billing_period])?;
    collect_lines(&mut rows)
}

fn stored_lines(conn: &Connection, snapshot_id: &str) -> Result<Vec<SnapshotLine>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {LINE_SELECT} FROM billing_snapshot_charge
         WHERE snapshot_id = ? ORDER BY charge_id"
    ))?;
    let mut rows = stmt.query(params![snapshot_id])?;
    collect_lines(&mut rows)
}

fn collect_lines(rows: &mut duckdb::Rows<'_>) -> Result<Vec<SnapshotLine>> {
    let mut lines = Vec::new();
    while let Some(row) = rows.next()? {
        lines.push(line_from(row)?);
    }
    Ok(lines)
}

fn line_from(row: &Row<'_>) -> Result<SnapshotLine> {
    let start = parse_timestamp(&row.get::<_, String>(2)?)?;
    let end = parse_timestamp(&row.get::<_, String>(3)?)?;
    let category: String = row.get(4)?;
    let basis: String = row.get(15)?;
    let charge = Charge {
        charge_category: ChargeCategory::from_stored(&category)
            .ok_or_else(|| anyhow!("Unknown charge category {category:?} in the ledger"))?,
        cost_basis: CostBasis::from_stored(&basis)
            .ok_or_else(|| anyhow!("Unknown cost basis {basis:?} in the ledger"))?,
        billing_account_id: row.get(1)?,
        charge_description: row.get(5)?,
        service_name: row.get(6)?,
        service_category: row.get(7)?,
        resource_id: row.get(8)?,
        resource_name: row.get(9)?,
        region_id: row.get(10)?,
        billed_cost: row.get(11)?,
        effective_cost: row.get(12)?,
        list_cost: row.get(13)?,
        pricing_quantity: row.get(16)?,
        pricing_unit: row.get(17)?,
        tags: row.get(18)?,
        sub_account_id: row.get(19)?,
        sub_account_name: row.get(20)?,
        service_subcategory: row.get(21)?,
        x_service_code: row.get(22)?,
        x_model: row.get(23)?,
        consumed_quantity: row.get(24)?,
        consumed_unit: row.get(25)?,
        sku_meter: row.get(26)?,
        sku_price_details: row.get(27)?,
        principal_id: row.get(28)?,
        credential_id: row.get(29)?,
        requester_details: row.get(30)?,
        ..Charge::new(start, end, row.get::<_, String>(14)?)
    };
    Ok(SnapshotLine {
        charge_id: row.get(0)?,
        charge,
        fx_rate: row.get(31)?,
    })
}

/// The lines of a period, for another module's tests.
#[cfg(test)]
pub(crate) fn read_current_for_tests(conn: &Connection, key: &PeriodKey) -> Vec<SnapshotLine> {
    read_current(conn, key).unwrap()
}

/// The lines of a snapshot, for another module's tests.
#[cfg(test)]
pub(crate) fn stored_lines_for_tests(conn: &Connection, snapshot_id: &str) -> Vec<SnapshotLine> {
    stored_lines(conn, snapshot_id).unwrap()
}

#[cfg(test)]
mod tests {
    use super::super::{delete_account_history_of, schema, write_period};
    use super::*;
    use chrono::TimeZone;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        schema::apply_reporting_currency(&conn, "USD").unwrap();
        conn
    }

    fn key() -> PeriodKey {
        PeriodKey::new("AWS", "acct-1", "2026-08")
    }

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap()
    }

    fn after_month_end() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap()
    }

    fn usage(service: &str, amount: f64, currency: &str) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: Some(amount),
            effective_cost: Some(amount),
            ..Charge::new(at(3), at(4), currency)
        }
    }

    fn write(conn: &mut Connection, batch: &str, charges: &[Charge], channel: Channel) {
        write_period(conn, &key(), batch, charges, Some("raw/path"), channel).unwrap();
    }

    fn frozen(outcome: FreezeOutcome) -> BillingSnapshot {
        match outcome {
            FreezeOutcome::Frozen(snapshot) => snapshot,
            other => panic!("expected a new revision, got {other:?}"),
        }
    }

    fn total(snapshot: &BillingSnapshot, currency: &str) -> Option<f64> {
        snapshot::currency_totals(&snapshot.totals)
            .into_iter()
            .find(|(c, _)| c == currency)
            .and_then(|(_, total)| total)
    }

    #[test]
    fn a_stored_line_reads_back_exactly_as_the_ledger_held_it() {
        let mut conn = conn();
        let full = Charge {
            billing_account_id: Some("payer".to_string()),
            sub_account_id: Some("linked".to_string()),
            sub_account_name: Some("Linked".to_string()),
            charge_description: Some("Hours".to_string()),
            service_category: Some("Compute".to_string()),
            service_subcategory: Some("Virtual Machines".to_string()),
            x_service_code: Some("AmazonEC2".to_string()),
            resource_id: Some("i-1".to_string()),
            resource_name: Some("web".to_string()),
            region_id: Some("us-east-1".to_string()),
            list_cost: Some(12.0),
            pricing_quantity: Some(3.0),
            pricing_unit: Some("Hours".to_string()),
            tags: Some(r#"{"team":"a"}"#.to_string()),
            ..usage("EC2", 0.1 + 0.2, "USD")
        };
        write(&mut conn, "b-1", &[full], Channel::File);
        let snapshot = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());

        let current = read_current(&conn, &key()).unwrap();
        let stored = stored_lines(&conn, &snapshot.snapshot_id).unwrap();
        assert_eq!(snapshot::checksum(&current), snapshot::checksum(&stored));
        assert_eq!(stored[0].charge.tags.as_deref(), Some(r#"{"team":"a"}"#));
        assert_eq!(stored[0].charge.billed_cost, Some(0.1 + 0.2));
        assert_eq!(stored[0].fx_rate, Some(1.0));
    }

    #[test]
    fn frozen_values_survive_a_refresh_replacing_the_month() {
        let mut conn = conn();
        write(
            &mut conn,
            "b-1",
            &[usage("EC2", 10.0, "USD"), usage("S3", 2.0, "USD")],
            Channel::Api,
        );
        let snapshot = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());
        let before = stored_lines(&conn, &snapshot.snapshot_id).unwrap();

        write(&mut conn, "b-2", &[usage("EC2", 99.0, "USD")], Channel::Api);

        let after = stored_lines(&conn, &snapshot.snapshot_id).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(snapshot::checksum(&before), snapshot::checksum(&after));
        let stored = snapshots_of_key(&conn, &key()).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(total(&stored[0], "USD"), Some(12.0));
        assert!(snapshot::verify(&stored[0], &after));
        assert_eq!(stored[0].batch_id.as_deref(), Some("b-1"));
        assert_eq!(stored[0].channel, Channel::Api);
        assert_eq!(stored[0].source_ref.as_deref(), Some("raw/path"));
        assert_eq!(stored[0].row_count, 2);
    }

    #[test]
    fn frozen_values_survive_a_file_import_replacing_the_month() {
        let mut conn = conn();
        write(&mut conn, "b-1", &[usage("EC2", 10.0, "USD")], Channel::Api);
        let snapshot = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());

        write(
            &mut conn,
            "b-2",
            &[usage("EC2", 11.0, "USD")],
            Channel::File,
        );

        let lines = stored_lines(&conn, &snapshot.snapshot_id).unwrap();
        assert!(snapshot::verify(&snapshot, &lines));
        let diff = snapshot::compare(&lines, &read_current(&conn, &key()).unwrap());
        assert_eq!(diff.changed_lines, 1);
        assert!((diff.by_currency[0].delta - 1.0).abs() < 1e-9);
    }

    #[test]
    fn freezing_again_adds_a_revision_and_keeps_the_first() {
        let mut conn = conn();
        write(&mut conn, "b-1", &[usage("EC2", 10.0, "USD")], Channel::Api);
        let first = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());

        write(&mut conn, "b-2", &[usage("EC2", 12.0, "USD")], Channel::Api);
        let second = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());

        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        let revisions = snapshots_of_key(&conn, &key()).unwrap();
        assert_eq!(
            revisions.iter().map(|s| s.revision).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(total(&revisions[1], "USD"), Some(10.0));
        assert_eq!(total(&revisions[0], "USD"), Some(12.0));
    }

    #[test]
    fn freezing_an_unchanged_month_again_adds_nothing() {
        let mut conn = conn();
        write(&mut conn, "b-1", &[usage("EC2", 10.0, "USD")], Channel::Api);
        let first = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());
        // The same bill under a new batch is still the same bill.
        write(&mut conn, "b-2", &[usage("EC2", 10.0, "USD")], Channel::Api);

        match freeze_of(&mut conn, &key(), after_month_end()).unwrap() {
            FreezeOutcome::Unchanged(latest) => assert_eq!(latest.revision, first.revision),
            other => panic!("expected no new revision, got {other:?}"),
        }
        assert_eq!(snapshots_of_key(&conn, &key()).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_month_is_not_frozen_as_zero() {
        let mut conn = conn();
        assert!(matches!(
            freeze_of(&mut conn, &key(), after_month_end()).unwrap(),
            FreezeOutcome::Empty
        ));
        assert!(snapshots_of_key(&conn, &key()).unwrap().is_empty());
    }

    #[test]
    fn a_usage_only_month_freezes_with_no_total() {
        let mut conn = conn();
        let tokens = Charge {
            service_name: Some("Tokens".to_string()),
            cost_basis: CostBasis::Absent,
            pricing_quantity: Some(1200.0),
            pricing_unit: Some("Tokens".to_string()),
            ..Charge::new(at(3), at(4), "USD")
        };
        write(&mut conn, "b-1", &[tokens], Channel::File);
        let snapshot = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());
        assert_eq!(total(&snapshot, "USD"), None);
        assert_eq!(snapshot.totals[0].rows_without_amount, 1);
        let lines = stored_lines(&conn, &snapshot.snapshot_id).unwrap();
        assert_eq!(lines[0].charge.billed_cost, None);
        assert_eq!(lines[0].charge.pricing_quantity, Some(1200.0));
    }

    #[test]
    fn credits_adjustments_and_currencies_keep_their_own_totals_without_any_rate_table() {
        let mut conn = conn();
        let credit = Charge {
            charge_category: ChargeCategory::Credit,
            ..usage("EC2", -4.0, "USD")
        };
        let adjustment = Charge {
            charge_category: ChargeCategory::Adjustment,
            ..usage("EC2", 1.5, "USD")
        };
        // No rate covers EUR, and CNY's is deleted after the freeze.
        write(
            &mut conn,
            "b-1",
            &[
                usage("EC2", 10.0, "USD"),
                credit,
                adjustment,
                usage("ECS", 71.0, "CNY"),
                usage("VM", 5.0, "EUR"),
            ],
            Channel::File,
        );
        let snapshot = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());
        conn.execute_batch("DELETE FROM dim_fx_rate").unwrap();

        let stored = snapshots_of_key(&conn, &key()).unwrap().remove(0);
        assert_eq!(total(&stored, "USD"), Some(7.5));
        assert_eq!(total(&stored, "CNY"), Some(71.0));
        assert_eq!(total(&stored, "EUR"), Some(5.0));
        assert_eq!(stored.reporting_currency, "USD");
        let eur = stored.totals.iter().find(|t| t.currency == "EUR").unwrap();
        assert_eq!(eur.rows_unconverted, 1);
        let cny = stored.rates.iter().find(|r| r.currency == "CNY").unwrap();
        assert_eq!(cny.rate, Some(0.1408));

        // The lines alone reach the same figures.
        let lines = stored_lines(&conn, &snapshot.snapshot_id).unwrap();
        assert!(snapshot::verify(&stored, &lines));
    }

    #[test]
    fn a_corrected_focus_export_reads_as_a_delta_against_the_frozen_month() {
        use crate::cloud::aws_focus::tests::charges_from_csv;
        let mut conn = conn();
        let original = charges_from_csv(&[
            ("Amazon EC2", "Usage", "100.00"),
            ("Amazon S3", "Usage", "20.00"),
            ("Amazon RDS", "Usage", "30.00"),
            ("AWS Tax", "Tax", "15.00"),
        ]);
        let corrected = charges_from_csv(&[
            ("Amazon EC2", "Usage", "90.00"),
            ("Amazon S3", "Usage", "20.00"),
            ("AWS Tax", "Tax", "15.00"),
            ("AWS Support", "Usage", "7.25"),
            ("Amazon EC2", "Credit", "-5.00"),
        ]);
        write(&mut conn, "b-1", &original, Channel::File);
        let snapshot = frozen(freeze_of(&mut conn, &key(), after_month_end()).unwrap());
        write(&mut conn, "b-2", &corrected, Channel::File);

        let lines = stored_lines(&conn, &snapshot.snapshot_id).unwrap();
        let diff = snapshot::compare(&lines, &read_current(&conn, &key()).unwrap());

        assert_eq!(diff.new_lines, 2, "support and the credit");
        assert_eq!(diff.removed_lines, 1, "rds");
        assert_eq!(diff.changed_lines, 1, "ec2");
        assert_eq!(diff.unchanged_lines, 2, "s3 and tax");
        assert_eq!(diff.by_currency[0].frozen, Some(165.0));
        assert_eq!(diff.by_currency[0].current, Some(127.25));
        let moved: Vec<(&str, f64)> = diff
            .by_service
            .iter()
            .map(|d| (d.label.as_str(), d.delta))
            .collect();
        assert_eq!(
            moved,
            vec![
                ("Amazon RDS", -30.0),
                ("Amazon EC2", -15.0),
                ("AWS Support", 7.25),
            ]
        );
        assert!(diff
            .by_category
            .iter()
            .any(|d| d.label == "Credit" && d.delta == -5.0));
    }

    #[test]
    fn periods_list_what_has_rows_and_what_is_only_frozen() {
        let mut conn = conn();
        write(&mut conn, "b-1", &[usage("EC2", 10.0, "USD")], Channel::Api);
        freeze_of(&mut conn, &key(), after_month_end()).unwrap();
        // The frozen month's rows go; a later month arrives.
        write(&mut conn, "b-2", &[], Channel::Api);
        let later = PeriodKey::new("AWS", "acct-1", "2026-09");
        write_period(
            &mut conn,
            &later,
            "b-3",
            &[usage("EC2", 1.0, "USD")],
            None,
            Channel::Api,
        )
        .unwrap();

        let periods = periods_of_account(&conn, "acct-1").unwrap();
        assert_eq!(
            periods
                .iter()
                .map(|key| key.billing_period.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-09", "2026-08"]
        );
    }

    #[test]
    fn deleting_an_account_takes_its_snapshots_with_it() {
        let mut conn = conn();
        write(&mut conn, "b-1", &[usage("EC2", 10.0, "USD")], Channel::Api);
        freeze_of(&mut conn, &key(), after_month_end()).unwrap();

        delete_account_history_of(&mut conn, "acct-1").unwrap();

        assert!(snapshots_of_key(&conn, &key()).unwrap().is_empty());
        let lines: i64 = conn
            .query_row("SELECT count(*) FROM billing_snapshot_charge", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(lines, 0);
    }
}
