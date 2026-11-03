//! Invoice-to-ledger reconciliation: does the provider's final bill match
//! what was imported?
//!
//! Only authoritative billed cost in the invoice's own currency is compared.
//! Effective cost, derived and estimated amounts, usage with no amount and FX
//! conversions are never part of the figure, and nothing here writes a
//! charge. The same lines come from the ledger or a frozen snapshot, so the
//! basis of a comparison is the caller's choice.
//!
//! The rule that shapes the rest: `Matched` is earned. Anything that keeps the
//! two figures from being comparable is `CannotVerify`, even when they agree
//! by accident.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};

use super::{midnight, period_of};
use crate::model::{
    ChargeCategory, CostBasis, Invoice, InvoiceSource, PeriodKey, ReconcileComponent,
    ReconcileCoverage, ReconcileReason, ReconcileStatus, Reconciliation, SnapshotLine,
};

/// How far a ledger's rows may stop short of the month's edges before the
/// export counts as partial. A day either side allows for a provider's rows
/// settling late and a quiet account's last day.
const COVERAGE_SLACK_DAYS: i64 = 2;

/// Floating-point residue from summing, on top of a currency's tolerance.
const RESIDUE: f64 = 1e-9;

/// The columns an invoice CSV may carry, in any order. Those from `tax` on are
/// optional.
pub const CSV_COLUMNS: [&str; 10] = [
    "provider",
    "account_id",
    "billing_period",
    "invoice_id",
    "currency",
    "amount",
    "tax",
    "credits",
    "adjustments",
    "billing_account_id",
];

/// Everything a comparison reads besides the invoices.
pub struct Evidence<'a> {
    /// The period's charges, from the ledger or a snapshot.
    pub lines: &'a [SnapshotLine],
    /// The month had not ended when these lines were read or frozen.
    pub period_open: bool,
    /// Authoritative totals of the months either side, in the invoice's
    /// currency, for spotting an invoice filed under the wrong month.
    pub neighbours: &'a [(String, f64)],
}

/// Why an invoice's fields were refused.
#[derive(Debug, Clone, PartialEq)]
pub enum InvoiceError {
    MissingField(&'static str),
    BadPeriod(String),
    BadCurrency(String),
    BadAmount { field: &'static str, value: String },
    PositiveCredit(String),
}

/// An invoice as typed or read from a CSV, not yet validated.
#[derive(Debug, Clone, Default)]
pub struct InvoiceDraft {
    pub provider: String,
    pub account_id: String,
    pub billing_period: String,
    pub invoice_ref: String,
    pub currency: String,
    pub amount: String,
    pub tax: String,
    pub credits: String,
    pub adjustments: String,
    pub billing_account_id: String,
}

/// A rejected CSV: the 1-based line and what was wrong with it.
#[derive(Debug, Clone, PartialEq)]
pub enum CsvError {
    Empty,
    MissingColumn(&'static str),
    Row { line: usize, error: InvoiceError },
    WrongFieldCount { line: usize },
}

/// How far apart two totals may be and still match: one minor unit.
pub fn tolerance(currency: &str) -> f64 {
    match currency {
        "JPY" | "KRW" => 1.0,
        _ => 0.01,
    }
}

/// A plain decimal. A thousands separator is refused rather than guessed at:
/// `1,234` is a thousand in one locale and one in another.
fn parse_amount(field: &'static str, value: &str) -> Result<f64, InvoiceError> {
    let bad = || InvoiceError::BadAmount {
        field,
        value: value.to_string(),
    };
    let trimmed = value.trim();
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        return Err(bad());
    }
    let amount: f64 = trimmed.parse().map_err(|_| bad())?;
    amount.is_finite().then_some(amount).ok_or_else(bad)
}

fn optional_amount(field: &'static str, value: &str) -> Result<Option<f64>, InvoiceError> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    parse_amount(field, value).map(Some)
}

fn required<'a>(field: &'static str, value: &'a str) -> Result<&'a str, InvoiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(InvoiceError::MissingField(field))
    } else {
        Ok(trimmed)
    }
}

/// Validate a draft into evidence that can be stored.
pub fn invoice_from(
    draft: &InvoiceDraft,
    source: InvoiceSource,
    source_file: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Invoice, InvoiceError> {
    let provider = required("provider", &draft.provider)?;
    let account_id = required("account_id", &draft.account_id)?;
    let period = required("billing_period", &draft.billing_period)?;
    period_of(period).map_err(|_| InvoiceError::BadPeriod(period.to_string()))?;
    let invoice_ref = required("invoice_id", &draft.invoice_ref)?;
    let currency = required("currency", &draft.currency)?.to_ascii_uppercase();
    if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(InvoiceError::BadCurrency(draft.currency.trim().to_string()));
    }
    let amount = parse_amount("amount", required("amount", &draft.amount)?)?;
    let credits = optional_amount("credits", &draft.credits)?;
    // A credit reduces a bill. A positive one is a sign slip that would
    // otherwise read as a gap of twice its size.
    if credits.is_some_and(|credits| credits > 0.0) {
        return Err(InvoiceError::PositiveCredit(
            draft.credits.trim().to_string(),
        ));
    }
    let billing_account = draft.billing_account_id.trim();
    Ok(Invoice {
        invoice_id: uuid::Uuid::new_v4().to_string(),
        key: PeriodKey::new(provider, account_id, period),
        invoice_ref: invoice_ref.to_string(),
        currency,
        amount,
        tax: optional_amount("tax", &draft.tax)?,
        credits,
        adjustments: optional_amount("adjustments", &draft.adjustments)?,
        billing_account_id: (!billing_account.is_empty()).then(|| billing_account.to_string()),
        source,
        source_file: source_file.map(str::to_string),
        entered_at: now,
    })
}

/// One CSV line's fields, honoring quotes. `None` for an unterminated quote.
fn csv_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    (!quoted).then_some(fields)
}

/// Read an invoice CSV. All or nothing: a file with one bad line is refused
/// whole, so a half-read file cannot read as a complete set of invoices.
pub fn invoices_from_csv(
    text: &str,
    source_file: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Vec<Invoice>, CsvError> {
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());
    let (_, header) = lines.next().ok_or(CsvError::Empty)?;
    let header: Vec<String> = csv_fields(header)
        .ok_or(CsvError::WrongFieldCount { line: 1 })?
        .into_iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    let column = |name: &'static str| header.iter().position(|field| field == name);
    for name in [
        "provider",
        "account_id",
        "billing_period",
        "invoice_id",
        "currency",
        "amount",
    ] {
        column(name).ok_or(CsvError::MissingColumn(name))?;
    }

    let mut invoices = Vec::new();
    for (index, line) in lines {
        let number = index + 1;
        let fields = csv_fields(line)
            .filter(|fields| fields.len() == header.len())
            .ok_or(CsvError::WrongFieldCount { line: number })?;
        let value = |name: &'static str| {
            column(name)
                .map(|at| fields[at].clone())
                .unwrap_or_default()
        };
        let draft = InvoiceDraft {
            provider: value("provider"),
            account_id: value("account_id"),
            billing_period: value("billing_period"),
            invoice_ref: value("invoice_id"),
            currency: value("currency"),
            amount: value("amount"),
            tax: value("tax"),
            credits: value("credits"),
            adjustments: value("adjustments"),
            billing_account_id: value("billing_account_id"),
        };
        invoices.push(
            invoice_from(&draft, InvoiceSource::Csv, source_file, now).map_err(|error| {
                CsvError::Row {
                    line: number,
                    error,
                }
            })?,
        );
    }
    if invoices.is_empty() {
        return Err(CsvError::Empty);
    }
    Ok(invoices)
}

/// Invoices that answer for one figure together: a provider may issue several
/// for one month, and each set is compared as a sum, not one at a time.
pub fn group_invoices(invoices: Vec<Invoice>) -> Vec<Vec<Invoice>> {
    let mut groups: BTreeMap<(String, String, String, String, String), Vec<Invoice>> =
        BTreeMap::new();
    for invoice in invoices {
        groups
            .entry((
                invoice.key.provider.clone(),
                invoice.key.account_id.clone(),
                invoice.key.billing_period.clone(),
                invoice.currency.clone(),
                invoice.billing_account_id.clone().unwrap_or_default(),
            ))
            .or_default()
            .push(invoice);
    }
    groups.into_values().collect()
}

/// The months either side of a `YYYY-MM` month.
pub fn neighbouring_periods(billing_period: &str) -> Vec<String> {
    let Ok(period) = period_of(billing_period) else {
        return Vec::new();
    };
    let label = |date: NaiveDate| date.format("%Y-%m").to_string();
    vec![
        label(period.start() - Duration::days(1)),
        label(period.end_exclusive()),
    ]
}

/// The sum of a line set's authoritative billed amounts, per currency. This is
/// the only figure an invoice is ever held against.
pub fn authoritative_totals(lines: &[SnapshotLine]) -> Vec<(String, f64)> {
    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    for line in lines {
        if let (CostBasis::Authoritative, Some(amount)) =
            (line.charge.cost_basis, line.charge.billed_cost)
        {
            *totals
                .entry(line.charge.billing_currency.clone())
                .or_default() += amount;
        }
    }
    totals.into_iter().collect()
}

/// Compare one group of invoices, all of one scope and currency, with a line
/// set.
pub fn reconcile(invoices: &[Invoice], evidence: &Evidence<'_>) -> Reconciliation {
    let first = invoices
        .first()
        .expect("a reconciliation compares at least one invoice");
    let currency = first.currency.as_str();
    let tolerance = tolerance(currency);
    let close = |a: f64, b: f64| (a - b).abs() <= tolerance + RESIDUE;
    let invoice_total: f64 = invoices.iter().map(|invoice| invoice.amount).sum();

    let in_scope: Vec<&SnapshotLine> = evidence
        .lines
        .iter()
        .filter(|line| match &first.billing_account_id {
            Some(scope) => line.charge.billing_account_id.as_deref() == Some(scope.as_str()),
            None => true,
        })
        .collect();
    let lines: Vec<&SnapshotLine> = in_scope
        .iter()
        .filter(|line| line.charge.billing_currency == currency)
        .copied()
        .collect();

    let authoritative: Vec<&SnapshotLine> = lines
        .iter()
        .filter(|line| {
            line.charge.cost_basis == CostBasis::Authoritative && line.charge.billed_cost.is_some()
        })
        .copied()
        .collect();
    let without_amount = lines
        .iter()
        .filter(|line| line.charge.billed_cost.is_none())
        .count();
    let other_basis = lines.len() - authoritative.len() - without_amount;
    let ledger_total: f64 = authoritative
        .iter()
        .filter_map(|line| line.charge.billed_cost)
        .sum();

    // Reasons that stop the figures being comparable at all.
    let mut blocking = Vec::new();
    let billing_accounts: BTreeSet<&str> = in_scope
        .iter()
        .filter_map(|line| line.charge.billing_account_id.as_deref())
        .collect();
    if in_scope.is_empty() {
        blocking.push(ReconcileReason::NoLedgerRows);
    } else if lines.is_empty() {
        blocking.push(ReconcileReason::CurrencyMismatch);
    } else if authoritative.is_empty() && other_basis == 0 {
        blocking.push(ReconcileReason::NoAmounts);
    }
    if evidence.period_open {
        blocking.push(ReconcileReason::PeriodOpen);
    }
    if other_basis > 0 {
        blocking.push(ReconcileReason::NonAuthoritative { rows: other_basis });
    }
    if first.billing_account_id.is_none() && billing_accounts.len() > 1 {
        blocking.push(ReconcileReason::MixedBillingAccounts {
            accounts: billing_accounts.len(),
        });
    }

    let coverage = coverage_of(&lines, &first.key.billing_period);
    let figures_agree = !lines.is_empty() && close(ledger_total, invoice_total);

    let status = if !blocking.is_empty() {
        ReconcileStatus::CannotVerify
    } else if figures_agree {
        ReconcileStatus::Matched
    } else if coverage.partial {
        ReconcileStatus::CannotVerify
    } else {
        ReconcileStatus::Difference
    };

    let components = components_of(invoices, &authoritative);
    let mut reasons = blocking;
    if !figures_agree {
        if coverage.partial {
            reasons.push(ReconcileReason::PartialCoverage {
                from: coverage.from,
                through: coverage.through,
            });
        }
        let gap = invoice_total - ledger_total;
        for component in &components {
            let ledger = component.ledger;
            let reason = |invoice: Option<f64>| match invoice {
                // The invoice states this category and the ledger disagrees.
                Some(invoice) => !close(invoice, ledger),
                // It does not, but leaving it out explains the whole gap.
                None => ledger.abs() > tolerance && close(gap, -ledger),
            };
            match component.category {
                ChargeCategory::Tax if reason(component.invoice) => {
                    reasons.push(ReconcileReason::TaxDiffers)
                }
                ChargeCategory::Credit if reason(component.invoice) => {
                    reasons.push(ReconcileReason::CreditsDiffer)
                }
                ChargeCategory::Adjustment if reason(component.invoice) => {
                    reasons.push(ReconcileReason::AdjustmentsDiffer)
                }
                _ => {}
            }
        }
        for (period, total) in evidence.neighbours {
            if close(*total, invoice_total) {
                reasons.push(ReconcileReason::MonthMismatch {
                    period: period.clone(),
                });
            }
        }
    }

    // Worth saying whatever the result, because neither changes the figure.
    if without_amount > 0 {
        reasons.push(ReconcileReason::UsageWithoutAmount {
            rows: without_amount,
        });
    }
    let mut others: BTreeMap<&str, usize> = BTreeMap::new();
    for line in &in_scope {
        if line.charge.billing_currency != currency {
            *others.entry(&line.charge.billing_currency).or_default() += 1;
        }
    }
    for (currency, rows) in others {
        reasons.push(ReconcileReason::OtherCurrency {
            currency: currency.to_string(),
            rows,
        });
    }

    Reconciliation {
        status,
        currency: currency.to_string(),
        invoice_total,
        ledger_total: (!authoritative.is_empty()).then_some(ledger_total),
        difference: (!authoritative.is_empty()).then_some(invoice_total - ledger_total),
        tolerance,
        components,
        reasons,
        coverage: ReconcileCoverage {
            rows: lines.len(),
            authoritative_rows: authoritative.len(),
            from: coverage.from,
            through: coverage.through,
        },
    }
}

struct Span {
    from: Option<NaiveDate>,
    through: Option<NaiveDate>,
    partial: bool,
}

/// The days a line set covers, and whether they fall short of the month.
fn coverage_of(lines: &[&SnapshotLine], billing_period: &str) -> Span {
    let start = lines
        .iter()
        .map(|line| line.charge.charge_period_start)
        .min();
    let end = lines.iter().map(|line| line.charge.charge_period_end).max();
    let (Some(start), Some(end), Ok(period)) = (start, end, period_of(billing_period)) else {
        return Span {
            from: None,
            through: None,
            partial: false,
        };
    };
    let slack = Duration::days(COVERAGE_SLACK_DAYS);
    Span {
        from: Some(start.date_naive()),
        // An end is exclusive, so the last day covered is the one before it.
        through: Some((end - Duration::seconds(1)).date_naive()),
        partial: start - midnight(period.start()) > slack
            || midnight(period.end_exclusive()) - end > slack,
    }
}

/// The figure by charge category. The components add up to the ledger total:
/// every category is listed, so a credit or a tax cannot fall out of sight.
fn components_of(invoices: &[Invoice], authoritative: &[&SnapshotLine]) -> Vec<ReconcileComponent> {
    let stated = |pick: fn(&Invoice) -> Option<f64>| -> Option<f64> {
        invoices
            .iter()
            .filter_map(pick)
            .fold(None, |sum, value| Some(sum.unwrap_or(0.0) + value))
    };
    [
        (ChargeCategory::Usage, None),
        (ChargeCategory::Purchase, None),
        (ChargeCategory::Credit, stated(|invoice| invoice.credits)),
        (ChargeCategory::Tax, stated(|invoice| invoice.tax)),
        (
            ChargeCategory::Adjustment,
            stated(|invoice| invoice.adjustments),
        ),
    ]
    .into_iter()
    .filter_map(|(category, invoice)| {
        let mut rows = 0;
        let mut ledger = 0.0;
        for line in authoritative {
            if line.charge.charge_category == category {
                rows += 1;
                ledger += line.charge.billed_cost.unwrap_or(0.0);
            }
        }
        (rows > 0 || invoice.is_some()).then_some(ReconcileComponent {
            category,
            invoice,
            ledger,
            rows,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::snapshot;
    use crate::model::Charge;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 5, 0, 0, 0).unwrap()
    }

    fn key() -> PeriodKey {
        PeriodKey::new("AWS", "payer", "2026-09")
    }

    fn charge(day: u32, category: ChargeCategory, amount: f64, currency: &str) -> Charge {
        Charge {
            service_name: Some("EC2".to_string()),
            charge_category: category,
            billed_cost: Some(amount),
            effective_cost: Some(amount),
            billing_account_id: Some("payer".to_string()),
            ..Charge::new(
                Utc.with_ymd_and_hms(2026, 9, day, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 9, day, 23, 59, 59).unwrap(),
                currency,
            )
        }
    }

    /// A month's usage on its first and last days, so it covers the month.
    fn month(amount: f64) -> Vec<Charge> {
        vec![
            charge(1, ChargeCategory::Usage, amount / 2.0, "USD"),
            charge(30, ChargeCategory::Usage, amount / 2.0, "USD"),
        ]
    }

    fn lines(charges: Vec<Charge>) -> Vec<SnapshotLine> {
        snapshot::charge_ids(&key(), &charges)
            .into_iter()
            .zip(charges)
            .map(|(charge_id, charge)| SnapshotLine {
                charge_id,
                charge,
                fx_rate: None,
            })
            .collect()
    }

    fn invoice(amount: f64) -> Invoice {
        Invoice {
            invoice_id: "i-1".to_string(),
            key: key(),
            invoice_ref: "INV-1".to_string(),
            currency: "USD".to_string(),
            amount,
            tax: None,
            credits: None,
            adjustments: None,
            billing_account_id: None,
            source: InvoiceSource::Manual,
            source_file: None,
            entered_at: now(),
        }
    }

    fn check(invoices: &[Invoice], charges: Vec<Charge>) -> Reconciliation {
        reconcile(
            invoices,
            &Evidence {
                lines: &lines(charges),
                period_open: false,
                neighbours: &[],
            },
        )
    }

    #[test]
    fn an_equal_amount_matches_and_shows_its_basis() {
        let result = check(&[invoice(100.0)], month(100.0));
        assert_eq!(result.status, ReconcileStatus::Matched);
        assert_eq!(result.difference, Some(0.0));
        assert_eq!(result.ledger_total, Some(100.0));
        assert_eq!(result.coverage.authoritative_rows, 2);
        assert!(result.reasons.is_empty());
    }

    #[test]
    fn a_difference_inside_one_minor_unit_is_tolerated_and_still_stated() {
        let result = check(&[invoice(100.01)], month(100.0));
        assert_eq!(result.status, ReconcileStatus::Matched);
        assert!((result.difference.unwrap() - 0.01).abs() < 1e-9);
        let result = check(&[invoice(100.02)], month(100.0));
        assert_eq!(result.status, ReconcileStatus::Difference);
    }

    #[test]
    fn a_mismatch_gives_the_exact_original_currency_delta_and_a_breakdown() {
        let mut charges = month(100.0);
        charges.push(charge(15, ChargeCategory::Tax, 8.0, "USD"));
        let result = check(&[invoice(120.5)], charges);
        assert_eq!(result.status, ReconcileStatus::Difference);
        assert_eq!(result.currency, "USD");
        assert_eq!(result.difference, Some(12.5));
        let summed: f64 = result.components.iter().map(|c| c.ledger).sum();
        assert_eq!(summed, 108.0);
        assert_eq!(result.components[0].category, ChargeCategory::Usage);
        assert_eq!(result.components[1].category, ChargeCategory::Tax);
    }

    #[test]
    fn a_consolidated_payer_invoice_matches_its_linked_accounts_together() {
        let linked = |sub: &str, day: u32, amount: f64| Charge {
            sub_account_id: Some(sub.to_string()),
            ..charge(day, ChargeCategory::Usage, amount, "USD")
        };
        let charges = vec![
            linked("111111111111", 1, 60.0),
            linked("222222222222", 30, 40.0),
        ];
        let result = check(&[invoice(100.0)], charges);
        assert_eq!(result.status, ReconcileStatus::Matched);
    }

    #[test]
    fn two_billing_accounts_need_a_scope_before_one_invoice_can_answer() {
        let mut other = month(50.0);
        for charge in &mut other {
            charge.billing_account_id = Some("other".to_string());
        }
        let mut charges = month(100.0);
        charges.extend(other);

        let unscoped = check(&[invoice(150.0)], charges.clone());
        assert_eq!(unscoped.status, ReconcileStatus::CannotVerify);
        assert!(unscoped
            .reasons
            .contains(&ReconcileReason::MixedBillingAccounts { accounts: 2 }));

        let scoped = Invoice {
            billing_account_id: Some("payer".to_string()),
            ..invoice(100.0)
        };
        assert_eq!(check(&[scoped], charges).status, ReconcileStatus::Matched);
    }

    #[test]
    fn several_invoices_of_one_scope_answer_together() {
        let second = Invoice {
            invoice_id: "i-2".to_string(),
            invoice_ref: "INV-2".to_string(),
            ..invoice(40.0)
        };
        let result = check(&[invoice(60.0), second], month(100.0));
        assert_eq!(result.status, ReconcileStatus::Matched);
        assert_eq!(result.invoice_total, 100.0);
    }

    #[test]
    fn a_revised_bill_matches_the_frozen_copy_and_differs_from_the_ledger() {
        let frozen = month(100.0);
        let revised = month(90.0);
        assert_eq!(
            check(&[invoice(100.0)], frozen).status,
            ReconcileStatus::Matched
        );
        let against_current = check(&[invoice(100.0)], revised);
        assert_eq!(against_current.status, ReconcileStatus::Difference);
        assert_eq!(against_current.difference, Some(10.0));
    }

    #[test]
    fn no_rows_cannot_verify_rather_than_differing_from_zero() {
        let result = check(&[invoice(100.0)], Vec::new());
        assert_eq!(result.status, ReconcileStatus::CannotVerify);
        assert_eq!(result.reasons, vec![ReconcileReason::NoLedgerRows]);
        assert_eq!(result.ledger_total, None);
        assert_eq!(result.difference, None);
    }

    #[test]
    fn a_zero_invoice_against_no_rows_is_never_a_match() {
        let result = check(&[invoice(0.0)], Vec::new());
        assert_eq!(result.status, ReconcileStatus::CannotVerify);
    }

    #[test]
    fn an_incompatible_currency_cannot_verify_and_is_not_converted() {
        let charges = vec![
            charge(1, ChargeCategory::Usage, 100.0, "EUR"),
            charge(30, ChargeCategory::Usage, 100.0, "EUR"),
        ];
        let result = check(&[invoice(200.0)], charges);
        assert_eq!(result.status, ReconcileStatus::CannotVerify);
        assert!(result.reasons.contains(&ReconcileReason::CurrencyMismatch));
        assert!(result.reasons.contains(&ReconcileReason::OtherCurrency {
            currency: "EUR".to_string(),
            rows: 2
        }));
    }

    #[test]
    fn another_currency_stays_out_of_the_figure_and_is_named() {
        let mut charges = month(100.0);
        charges.push(charge(9, ChargeCategory::Usage, 70.0, "CNY"));
        let result = check(&[invoice(100.0)], charges);
        assert_eq!(result.status, ReconcileStatus::Matched);
        assert_eq!(
            result.reasons,
            vec![ReconcileReason::OtherCurrency {
                currency: "CNY".to_string(),
                rows: 1
            }]
        );
    }

    #[test]
    fn an_open_month_cannot_verify_even_when_the_figures_agree() {
        let result = reconcile(
            &[invoice(100.0)],
            &Evidence {
                lines: &lines(month(100.0)),
                period_open: true,
                neighbours: &[],
            },
        );
        assert_eq!(result.status, ReconcileStatus::CannotVerify);
        assert!(result.reasons.contains(&ReconcileReason::PeriodOpen));
    }

    #[test]
    fn a_partial_export_that_differs_cannot_verify_and_says_where_it_stops() {
        let charges = vec![
            charge(1, ChargeCategory::Usage, 40.0, "USD"),
            charge(20, ChargeCategory::Usage, 40.0, "USD"),
        ];
        let result = check(&[invoice(100.0)], charges);
        assert_eq!(result.status, ReconcileStatus::CannotVerify);
        assert!(result.reasons.iter().any(|reason| matches!(
            reason,
            ReconcileReason::PartialCoverage { through: Some(through), .. }
                if *through == NaiveDate::from_ymd_opt(2026, 9, 20).unwrap()
        )));
    }

    #[test]
    fn a_short_month_that_agrees_exactly_still_matches() {
        let charges = vec![charge(1, ChargeCategory::Usage, 100.0, "USD")];
        assert_eq!(
            check(&[invoice(100.0)], charges).status,
            ReconcileStatus::Matched
        );
    }

    #[test]
    fn derived_and_estimated_amounts_are_not_compared() {
        let mut charges = month(100.0);
        charges[0].cost_basis = CostBasis::Estimated;
        let result = check(&[invoice(100.0)], charges);
        assert_eq!(result.status, ReconcileStatus::CannotVerify);
        assert_eq!(result.ledger_total, Some(50.0));
        assert!(result
            .reasons
            .contains(&ReconcileReason::NonAuthoritative { rows: 1 }));
    }

    #[test]
    fn effective_cost_is_never_the_figure() {
        let mut charges = month(100.0);
        for charge in &mut charges {
            charge.effective_cost = Some(10.0);
        }
        assert_eq!(
            check(&[invoice(100.0)], charges).status,
            ReconcileStatus::Matched
        );
    }

    #[test]
    fn usage_rows_alone_cannot_verify_and_beside_a_bill_are_only_noted() {
        let tokens = |day: u32| Charge {
            cost_basis: CostBasis::Absent,
            billed_cost: None,
            effective_cost: None,
            ..charge(day, ChargeCategory::Usage, 0.0, "USD")
        };
        let alone = check(&[invoice(0.0)], vec![tokens(1), tokens(30)]);
        assert_eq!(alone.status, ReconcileStatus::CannotVerify);
        assert!(alone.reasons.contains(&ReconcileReason::NoAmounts));

        let mut beside = month(100.0);
        beside.push(tokens(5));
        let result = check(&[invoice(100.0)], beside);
        assert_eq!(result.status, ReconcileStatus::Matched);
        assert_eq!(
            result.reasons,
            vec![ReconcileReason::UsageWithoutAmount { rows: 1 }]
        );
    }

    #[test]
    fn credits_and_tax_count_once_on_the_ledger_side() {
        let mut charges = month(100.0);
        charges.push(charge(15, ChargeCategory::Credit, -20.0, "USD"));
        charges.push(charge(15, ChargeCategory::Tax, 8.0, "USD"));
        let stated = Invoice {
            tax: Some(8.0),
            credits: Some(-20.0),
            ..invoice(88.0)
        };
        let result = check(&[stated], charges);
        assert_eq!(result.status, ReconcileStatus::Matched);
        assert_eq!(result.ledger_total, Some(88.0));
        let credit = result
            .components
            .iter()
            .find(|c| c.category == ChargeCategory::Credit)
            .unwrap();
        assert_eq!(credit.ledger, -20.0);
        assert_eq!(credit.invoice, Some(-20.0));
    }

    #[test]
    fn tax_on_one_side_is_named_as_the_likely_gap() {
        let mut charges = month(100.0);
        charges.push(charge(15, ChargeCategory::Tax, 8.0, "USD"));
        let result = check(&[invoice(100.0)], charges);
        assert_eq!(result.status, ReconcileStatus::Difference);
        assert!(result.reasons.contains(&ReconcileReason::TaxDiffers));

        let stated = Invoice {
            tax: Some(8.0),
            ..invoice(108.0)
        };
        let result = check(&[stated], month(100.0));
        assert!(result.reasons.contains(&ReconcileReason::TaxDiffers));
        assert_eq!(result.components.last().unwrap().ledger, 0.0);
    }

    #[test]
    fn a_late_credit_or_adjustment_is_named() {
        let mut charges = month(100.0);
        charges.push(charge(30, ChargeCategory::Credit, -5.0, "USD"));
        let result = check(&[invoice(100.0)], charges);
        assert!(result.reasons.contains(&ReconcileReason::CreditsDiffer));

        let mut charges = month(100.0);
        charges.push(charge(30, ChargeCategory::Adjustment, 3.0, "USD"));
        let result = check(&[invoice(100.0)], charges);
        assert!(result.reasons.contains(&ReconcileReason::AdjustmentsDiffer));
    }

    #[test]
    fn an_invoice_that_fits_the_next_month_says_so() {
        let result = reconcile(
            &[invoice(250.0)],
            &Evidence {
                lines: &lines(month(100.0)),
                period_open: false,
                neighbours: &[("2026-10".to_string(), 250.0)],
            },
        );
        assert_eq!(result.status, ReconcileStatus::Difference);
        assert!(result.reasons.contains(&ReconcileReason::MonthMismatch {
            period: "2026-10".to_string()
        }));
    }

    fn draft() -> InvoiceDraft {
        InvoiceDraft {
            provider: "AWS".to_string(),
            account_id: "payer".to_string(),
            billing_period: "2026-09".to_string(),
            invoice_ref: "INV-1".to_string(),
            currency: "usd".to_string(),
            amount: "100.50".to_string(),
            ..InvoiceDraft::default()
        }
    }

    #[test]
    fn a_draft_is_validated_into_an_invoice() {
        let invoice = invoice_from(&draft(), InvoiceSource::Manual, None, now()).unwrap();
        assert_eq!(invoice.currency, "USD");
        assert_eq!(invoice.amount, 100.5);
        assert_eq!(invoice.tax, None);
        assert_eq!(invoice.key, key());

        let refused = |edit: fn(&mut InvoiceDraft)| {
            let mut draft = draft();
            edit(&mut draft);
            invoice_from(&draft, InvoiceSource::Manual, None, now()).unwrap_err()
        };
        assert_eq!(
            refused(|d| d.amount = String::new()),
            InvoiceError::MissingField("amount")
        );
        assert!(matches!(
            refused(|d| d.amount = "1,234.50".to_string()),
            InvoiceError::BadAmount { .. }
        ));
        assert!(matches!(
            refused(|d| d.amount = "NaN".to_string()),
            InvoiceError::BadAmount { .. }
        ));
        assert!(matches!(
            refused(|d| d.billing_period = "2026-13".to_string()),
            InvoiceError::BadPeriod(_)
        ));
        assert!(matches!(
            refused(|d| d.currency = "US".to_string()),
            InvoiceError::BadCurrency(_)
        ));
        assert!(matches!(
            refused(|d| d.credits = "5".to_string()),
            InvoiceError::PositiveCredit(_)
        ));
    }

    #[test]
    fn a_csv_reads_with_optional_columns_quotes_and_a_bom() {
        let text = "\u{feff}provider,account_id,billing_period,invoice_id,currency,amount,tax\n\
                    AWS,payer,2026-09,\"INV,1\",USD,100.50,8\n\
                    \n\
                    AWS,payer,2026-08,INV-2,usd,90,\n";
        let invoices = invoices_from_csv(text, Some("bills.csv"), now()).unwrap();
        assert_eq!(invoices.len(), 2);
        assert_eq!(invoices[0].invoice_ref, "INV,1");
        assert_eq!(invoices[0].tax, Some(8.0));
        assert_eq!(invoices[0].source, InvoiceSource::Csv);
        assert_eq!(invoices[0].source_file.as_deref(), Some("bills.csv"));
        assert_eq!(invoices[1].tax, None);
    }

    #[test]
    fn a_csv_with_one_bad_line_is_refused_whole_and_names_it() {
        let text = "provider,account_id,billing_period,invoice_id,currency,amount\n\
                    AWS,payer,2026-09,INV-1,USD,100\n\
                    AWS,payer,2026-08,INV-2,USD,oops\n";
        assert!(matches!(
            invoices_from_csv(text, None, now()),
            Err(CsvError::Row { line: 3, .. })
        ));
        assert_eq!(
            invoices_from_csv("provider,amount\nAWS,1\n", None, now()),
            Err(CsvError::MissingColumn("account_id"))
        );
        assert_eq!(invoices_from_csv("", None, now()), Err(CsvError::Empty));
        let short = "provider,account_id,billing_period,invoice_id,currency,amount\nAWS,payer\n";
        assert_eq!(
            invoices_from_csv(short, None, now()),
            Err(CsvError::WrongFieldCount { line: 2 })
        );
    }

    #[test]
    fn invoices_group_by_scope_and_currency() {
        let other_currency = Invoice {
            currency: "EUR".to_string(),
            ..invoice(1.0)
        };
        let groups = group_invoices(vec![invoice(1.0), invoice(2.0), other_currency]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), 3);
    }

    #[test]
    fn neighbours_cross_a_year() {
        assert_eq!(neighbouring_periods("2026-01"), vec!["2025-12", "2026-02"]);
        assert_eq!(neighbouring_periods("2026-12"), vec!["2026-11", "2027-01"]);
    }
}
