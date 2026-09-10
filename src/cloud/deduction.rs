//! Decomposing a "gross, deductions, net" bill line into ledger rows.
//!
//! Alibaba Cloud and Volcengine both report a line three ways at once: what
//! it would have cost, what was taken off, and what was actually charged.
//! AWS, by contrast, bills a discount as a line of its own. Putting the net
//! amount on the usage row *and* the deductions beside it would count them
//! twice, so a line is decomposed instead: the usage row carries the
//! **gross** amount and each deduction becomes a negative `Credit` next to
//! it. A product's rows then sum to what was actually charged, and a period
//! total stays a plain sum.
//!
//! Shared because the same bill arrives through two channels — the billing
//! API and the console's own export — and the two must produce rows that
//! reconcile with each other, not merely rows that each look plausible.

use crate::ledger::{Charge, ChargeCategory};

/// Description given to the row that closes the gap when the named
/// deductions do not add up to the difference between gross and net.
pub const UNRECONCILED: &str = "Unreconciled";

/// Half a fen. Below this the gap is rounding, not a missing deduction.
pub const RECONCILIATION_TOLERANCE: f64 = 0.005;

/// Append the rows one bill line decomposes into.
///
/// `template` supplies everything that identifies the line — service,
/// resource, region, currency, period — and is called once per row so each
/// carries the same identity. `what` names the line in the warning a gap
/// produces, and is the only thing here that is for a human.
///
/// Anything left between the gross amount, the deductions we know the names
/// of, and the net figure is money the bill accounts for and this parser
/// does not. Recording it as an `Adjustment` keeps the total honest and
/// makes the gap visible instead of losing it.
pub fn decompose(
    charges: &mut Vec<Charge>,
    template: impl Fn() -> Charge,
    gross: f64,
    net: f64,
    deductions: &[(&str, f64)],
    what: &str,
) {
    charges.push(Charge {
        billed_cost: Some(gross),
        list_cost: Some(gross),
        ..template()
    });

    let mut deducted = 0.0;
    for (name, amount) in deductions {
        if *amount == 0.0 {
            continue;
        }
        deducted += amount;
        charges.push(Charge {
            charge_category: ChargeCategory::Credit,
            charge_description: Some(name.to_string()),
            billed_cost: Some(-amount),
            ..template()
        });
    }

    let residual = gross - deducted - net;
    if residual.abs() > RECONCILIATION_TOLERANCE {
        tracing::warn!(
            "Bill line for {} does not reconcile: {:.4} unaccounted for",
            what,
            residual
        );
        charges.push(Charge {
            charge_category: ChargeCategory::Adjustment,
            charge_description: Some(UNRECONCILED.to_string()),
            billed_cost: Some(-residual),
            ..template()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::BillingPeriod;

    fn template() -> Charge {
        let start = BillingPeriod::new(2026, 8)
            .start()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = BillingPeriod::new(2026, 8)
            .end_exclusive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        Charge {
            service_name: Some("ECS".to_string()),
            ..Charge::new(start, end, "CNY".to_string())
        }
    }

    fn total(charges: &[Charge]) -> f64 {
        charges.iter().filter_map(|charge| charge.billed_cost).sum()
    }

    #[test]
    fn the_usage_row_is_gross_and_each_deduction_is_a_credit() {
        let mut charges = Vec::new();
        decompose(
            &mut charges,
            template,
            320.50,
            288.45,
            &[("InvoiceDiscount", 22.05), ("DeductedByCoupons", 10.0)],
            "ECS",
        );

        assert_eq!(charges[0].billed_cost, Some(320.50));
        assert_eq!(charges[0].list_cost, Some(320.50));
        assert_eq!(charges[0].charge_category, ChargeCategory::Usage);

        assert_eq!(charges[1].billed_cost, Some(-22.05));
        assert_eq!(charges[1].charge_category, ChargeCategory::Credit);
        assert_eq!(charges[2].billed_cost, Some(-10.0));

        // The whole point: the rows sum to what was actually charged.
        assert!((total(&charges) - 288.45).abs() < 1e-9);
    }

    #[test]
    fn a_deduction_we_cannot_name_is_still_accounted_for() {
        let mut charges = Vec::new();
        decompose(
            &mut charges,
            template,
            100.0,
            62.0,
            &[("DeductedByPrepaidCard", 30.0)],
            "RDS",
        );

        let gap = charges
            .iter()
            .find(|charge| charge.charge_description.as_deref() == Some(UNRECONCILED))
            .expect("the gap is recorded");
        assert_eq!(gap.billed_cost, Some(-8.0));
        assert_eq!(gap.charge_category, ChargeCategory::Adjustment);
        assert!((total(&charges) - 62.0).abs() < 1e-9);
    }

    #[test]
    fn rounding_does_not_produce_a_reconciliation_row() {
        let mut charges = Vec::new();
        decompose(&mut charges, template, 10.0, 9.999, &[("D", 0.001)], "ECS");

        assert!(charges
            .iter()
            .all(|charge| charge.charge_description.as_deref() != Some(UNRECONCILED)));
    }

    #[test]
    fn a_zero_deduction_is_not_a_row() {
        let mut charges = Vec::new();
        decompose(&mut charges, template, 42.0, 42.0, &[("D", 0.0)], "OSS");

        assert_eq!(charges.len(), 1);
    }
}
