//! Invoice evidence in DuckDB. Kept apart from `fct_charge`, which nothing
//! here reads from or writes to. The comparison is
//! [`crate::analytics::reconciliation`].

use anyhow::{anyhow, Result};
use duckdb::{params, Connection, Row};

use super::schema::{NORMALIZED_VIEW, TIMESTAMP_FORMAT};
use super::{parse_timestamp, with_connection, with_connection_ref};
use crate::model::{Invoice, InvoiceSource, PeriodKey};

const INVOICE_SELECT: &str = "invoice_id, provider, account_id, billing_period, invoice_ref,
     currency, amount, tax, credits, adjustments, billing_account_id, source, source_file,
     CAST(entered_at AS VARCHAR)";

/// Store invoices. One carrying the same reference for the same month and
/// account replaces the earlier entry, so importing a corrected file does not
/// count a bill twice. All or nothing. Returns how many were stored.
pub fn save_invoices(invoices: &[Invoice]) -> Result<usize> {
    with_connection(|conn| save_invoices_in(conn, invoices))
}

/// Every invoice on record.
pub fn all_invoices() -> Result<Vec<Invoice>> {
    with_connection_ref(all_invoices_in)
}

/// Remove one invoice from the evidence.
pub fn delete_invoice(invoice_id: &str) -> Result<()> {
    with_connection_ref(|conn| {
        conn.execute(
            "DELETE FROM invoice_evidence WHERE invoice_id = ?",
            params![invoice_id],
        )?;
        Ok(())
    })
}

/// A period's authoritative billed total per currency, without reading its
/// lines.
pub fn billed_totals(key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| billed_totals_in(conn, key))
}

pub(crate) fn save_invoices_in(conn: &mut Connection, invoices: &[Invoice]) -> Result<usize> {
    let tx = conn.transaction()?;
    for invoice in invoices {
        tx.execute(
            "DELETE FROM invoice_evidence
             WHERE provider = ? AND account_id = ? AND billing_period = ? AND invoice_ref = ?",
            params![
                invoice.key.provider,
                invoice.key.account_id,
                invoice.key.billing_period,
                invoice.invoice_ref
            ],
        )?;
        tx.execute(
            "INSERT INTO invoice_evidence
             (invoice_id, provider, account_id, billing_period, invoice_ref, currency, amount,
              tax, credits, adjustments, billing_account_id, source, source_file, entered_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CAST(? AS TIMESTAMP))",
            params![
                invoice.invoice_id,
                invoice.key.provider,
                invoice.key.account_id,
                invoice.key.billing_period,
                invoice.invoice_ref,
                invoice.currency,
                invoice.amount,
                invoice.tax,
                invoice.credits,
                invoice.adjustments,
                invoice.billing_account_id,
                invoice.source.as_str(),
                invoice.source_file,
                invoice.entered_at.format(TIMESTAMP_FORMAT).to_string(),
            ],
        )?;
    }
    tx.commit()?;
    Ok(invoices.len())
}

pub(crate) fn all_invoices_in(conn: &Connection) -> Result<Vec<Invoice>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INVOICE_SELECT} FROM invoice_evidence
         ORDER BY billing_period DESC, provider, account_id, invoice_ref"
    ))?;
    let mut rows = stmt.query([])?;
    let mut invoices = Vec::new();
    while let Some(row) = rows.next()? {
        invoices.push(invoice_from(row)?);
    }
    Ok(invoices)
}

pub(crate) fn billed_totals_in(conn: &Connection, key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT billing_currency, sum(billed_cost) FROM {NORMALIZED_VIEW}
         WHERE provider = ? AND account_id = ? AND billing_period = ?
           AND cost_basis = 'authoritative' AND billed_cost IS NOT NULL
         GROUP BY billing_currency ORDER BY billing_currency"
    ))?;
    let totals = stmt
        .query_map(
            params![key.provider, key.account_id, key.billing_period],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(totals)
}

fn invoice_from(row: &Row<'_>) -> Result<Invoice> {
    let source: String = row.get(11)?;
    Ok(Invoice {
        invoice_id: row.get(0)?,
        key: PeriodKey::new(
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ),
        invoice_ref: row.get(4)?,
        currency: row.get(5)?,
        amount: row.get(6)?,
        tax: row.get(7)?,
        credits: row.get(8)?,
        adjustments: row.get(9)?,
        billing_account_id: row.get(10)?,
        source: InvoiceSource::from_stored(&source)
            .ok_or_else(|| anyhow!("Unknown invoice source {source:?} in the ledger"))?,
        source_file: row.get(12)?,
        entered_at: parse_timestamp(&row.get::<_, String>(13)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::{freeze_of, read_current_for_tests, stored_lines_for_tests};
    use super::super::{delete_account_history_of, schema, write_period};
    use super::*;
    use crate::analytics::reconciliation::{reconcile, Evidence};
    use crate::model::{
        Channel, Charge, ChargeCategory, FreezeOutcome, InvoiceSource, ReconcileStatus,
    };
    use chrono::{DateTime, TimeZone, Utc};

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

    fn invoice(reference: &str, amount: f64) -> Invoice {
        Invoice {
            invoice_id: uuid::Uuid::new_v4().to_string(),
            key: key(),
            invoice_ref: reference.to_string(),
            currency: "USD".to_string(),
            amount,
            tax: Some(1.5),
            credits: None,
            adjustments: None,
            billing_account_id: Some("payer".to_string()),
            source: InvoiceSource::Csv,
            source_file: Some("bills.csv".to_string()),
            entered_at: at(31),
        }
    }

    fn usage(day: u32, amount: f64) -> Charge {
        Charge {
            service_name: Some("EC2".to_string()),
            billed_cost: Some(amount),
            effective_cost: Some(amount),
            ..Charge::new(at(day), at(day + 1), "USD")
        }
    }

    fn charge_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM fct_charge", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn an_invoice_reads_back_as_it_was_stored() {
        let mut conn = conn();
        let stored = invoice("INV-1", 100.25);
        save_invoices_in(&mut conn, std::slice::from_ref(&stored)).unwrap();
        assert_eq!(all_invoices_in(&conn).unwrap(), vec![stored]);
    }

    #[test]
    fn the_same_reference_replaces_and_a_new_one_adds() {
        let mut conn = conn();
        save_invoices_in(&mut conn, &[invoice("INV-1", 100.0)]).unwrap();
        save_invoices_in(&mut conn, &[invoice("INV-1", 110.0)]).unwrap();
        save_invoices_in(&mut conn, &[invoice("INV-2", 5.0)]).unwrap();
        let stored = all_invoices_in(&conn).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].amount, 110.0);
    }

    #[test]
    fn evidence_never_touches_charges_and_charges_never_touch_evidence() {
        let mut conn = conn();
        write_period(
            &mut conn,
            &key(),
            "b-1",
            &[usage(1, 10.0)],
            None,
            Channel::Api,
        )
        .unwrap();
        let before = read_current_for_tests(&conn, &key());
        save_invoices_in(&mut conn, &[invoice("INV-1", 999.0)]).unwrap();
        assert_eq!(charge_count(&conn), 1);
        assert_eq!(
            crate::analytics::snapshot::checksum(&before),
            crate::analytics::snapshot::checksum(&read_current_for_tests(&conn, &key()))
        );

        write_period(
            &mut conn,
            &key(),
            "b-2",
            &[usage(1, 20.0)],
            None,
            Channel::Api,
        )
        .unwrap();
        assert_eq!(all_invoices_in(&conn).unwrap().len(), 1);
    }

    #[test]
    fn deleting_one_invoice_and_deleting_an_account_remove_the_evidence() {
        let mut conn = conn();
        let first = invoice("INV-1", 1.0);
        save_invoices_in(&mut conn, &[first.clone(), invoice("INV-2", 2.0)]).unwrap();
        conn.execute(
            "DELETE FROM invoice_evidence WHERE invoice_id = ?",
            params![first.invoice_id],
        )
        .unwrap();
        assert_eq!(all_invoices_in(&conn).unwrap().len(), 1);

        delete_account_history_of(&mut conn, "acct-1").unwrap();
        assert!(all_invoices_in(&conn).unwrap().is_empty());
    }

    #[test]
    fn billed_totals_hold_only_authoritative_amounts_per_currency() {
        let mut conn = conn();
        let estimated = Charge {
            cost_basis: crate::model::CostBasis::Estimated,
            ..usage(2, 100.0)
        };
        let credit = Charge {
            charge_category: ChargeCategory::Credit,
            ..usage(3, -4.0)
        };
        let eur = Charge {
            billing_currency: "EUR".to_string(),
            ..usage(4, 7.0)
        };
        write_period(
            &mut conn,
            &key(),
            "b-1",
            &[usage(1, 10.0), estimated, credit, eur],
            None,
            Channel::Api,
        )
        .unwrap();
        assert_eq!(
            billed_totals_in(&conn, &key()).unwrap(),
            vec![("EUR".to_string(), 7.0), ("USD".to_string(), 6.0)]
        );
    }

    #[test]
    fn the_demo_invoices_show_a_match_a_difference_and_a_currency_gap() {
        use crate::analytics::reconciliation::reconcile;
        use crate::demo_data;

        let now = Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap();
        let periods = demo_data::periods(now);
        let mut conn = conn();
        for (provider, account, _, currency) in demo_data::DEMO_SOURCES {
            for (index, period) in periods.iter().enumerate() {
                let charges = demo_data::period_charges(
                    provider,
                    demo_data::services_of(provider),
                    currency,
                    *period,
                    index,
                    now,
                );
                let key = PeriodKey::new(*provider, *account, period.label());
                let batch = format!("{account}-{}", period.label());
                write_period(&mut conn, &key, &batch, &charges, None, Channel::Api).unwrap();
            }
        }

        let invoices = demo_data::invoices(&periods, now);
        let status: Vec<_> = invoices
            .iter()
            .map(|invoice| {
                let lines = read_current_for_tests(&conn, &invoice.key);
                let evidence = Evidence {
                    lines: &lines,
                    period_open: false,
                    neighbours: &[],
                };
                reconcile(std::slice::from_ref(invoice), &evidence).status
            })
            .collect();
        assert_eq!(
            status,
            vec![
                ReconcileStatus::Matched,
                ReconcileStatus::Difference,
                ReconcileStatus::CannotVerify,
            ]
        );
    }

    #[test]
    fn a_revised_bill_is_compared_with_the_ledger_or_the_frozen_copy() {
        let mut conn = conn();
        let bill = [usage(1, 50.0), usage(29, 50.0)];
        write_period(&mut conn, &key(), "b-1", &bill, None, Channel::File).unwrap();
        let later = Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap();
        let FreezeOutcome::Frozen(frozen) = freeze_of(&mut conn, &key(), later).unwrap() else {
            panic!("expected a new revision");
        };
        let revised = [usage(1, 50.0), usage(29, 40.0)];
        write_period(&mut conn, &key(), "b-2", &revised, None, Channel::File).unwrap();

        let invoices = [Invoice {
            billing_account_id: None,
            tax: None,
            ..invoice("INV-1", 100.0)
        }];
        let against = |lines: &[crate::model::SnapshotLine]| {
            reconcile(
                &invoices,
                &Evidence {
                    lines,
                    period_open: false,
                    neighbours: &[],
                },
            )
        };
        let current = against(&read_current_for_tests(&conn, &key()));
        assert_eq!(current.status, ReconcileStatus::Difference);
        assert_eq!(current.difference, Some(10.0));
        let snapshot = against(&stored_lines_for_tests(&conn, &frozen.snapshot_id));
        assert_eq!(snapshot.status, ReconcileStatus::Matched);
    }
}
