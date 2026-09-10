//! The cost or usage export a model provider produces.
//!
//! OpenAI and Anthropic both publish one row per day per model, and both
//! offer two exports from the same console: a **cost** export, which carries
//! an amount, and a **usage** export, which carries token counts and no
//! money at all. This module reads either, and the difference between them
//! is exactly what `cost_basis` exists for:
//!
//! - a row with an amount becomes one `Usage` charge, `Authoritative`
//! - a row with only token counts becomes one charge per token type, with
//!   `billed_cost` NULL and `cost_basis` `Absent`
//!
//! The second is not a lesser version of the first. Multiplying tokens by a
//! list price would put a number in `billed_cost` that nobody was charged,
//! and the ledger is explicit that a shadow cost must never read as money
//! spent. A usage export tells you what was consumed; it does not tell you
//! what it cost, and it says so.
//!
//! A cost row keeps a quantity only when exactly one token column is filled
//! in. Input and output tokens are priced differently, so their sum is not
//! a quantity the amount divides by — the same reason the AWS normalizer
//! drops a quantity Cost Explorer reports against the unit `N/A`.

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};

use super::{period, periods_in_column, text_of, Record, Sheet};
use crate::cloud::{BillingPeriod, Normalized, RawBatch};
use crate::ledger::{Charge, CostBasis};

/// The column names one provider's export uses.
pub struct Layout {
    /// Recorded as `service_name`. These exports cover one provider, and
    /// name it nowhere in the file.
    pub service_name: &'static str,
    /// The day, or the start of the bucket.
    ///
    /// Doubles as what the header row is found by: these exports are a
    /// bare table whose one certain column is a date.
    pub date: &'static [&'static str],
    /// What was charged. Absent from a usage export.
    pub cost: &'static [&'static str],
    pub currency: &'static [&'static str],
    /// Currency for an export that names none — both providers bill in one.
    pub default_currency: &'static str,
    pub model: &'static [&'static str],
    /// What the line was for, when the export says more than the model.
    pub description: &'static [&'static str],
    /// The project or workspace the spend belongs to.
    pub scope_name: &'static [&'static str],
    pub scope_id: &'static [&'static str],
    /// Token columns, each paired with the `pricing_unit` it is counted in.
    pub tokens: &'static [(&'static str, &'static [&'static str])],
}

/// Where each of a [`Layout`]'s columns actually is.
struct Columns {
    date: usize,
    cost: Option<usize>,
    currency: Option<usize>,
    model: Option<usize>,
    description: Option<usize>,
    scope_name: Option<usize>,
    scope_id: Option<usize>,
    /// Only the token columns this export carries.
    tokens: Vec<(&'static str, usize)>,
}

impl Columns {
    fn resolve(layout: &Layout, sheet: &Sheet) -> Result<Self> {
        let columns = Self {
            date: sheet.require("date", layout.date)?,
            cost: sheet.column(layout.cost),
            currency: sheet.column(layout.currency),
            model: sheet.column(layout.model),
            description: sheet.column(layout.description),
            scope_name: sheet.column(layout.scope_name),
            scope_id: sheet.column(layout.scope_id),
            tokens: layout
                .tokens
                .iter()
                .filter_map(|(unit, aliases)| sheet.column(aliases).map(|column| (*unit, column)))
                .collect(),
        };

        // Neither an amount nor a token count is a file with nothing in it.
        // Failing here, naming the headers, beats importing a month of rows
        // that are all NULL.
        if columns.cost.is_none() && columns.tokens.is_empty() {
            return Err(anyhow::anyhow!(
                "This export has neither a cost column nor a token count \
                 (looked for a cost among {:?}). The columns it has are: {}",
                layout.cost,
                sheet.headers.join(", ")
            ));
        }

        Ok(columns)
    }
}

/// The months an export covers, oldest first.
pub fn periods(layout: &Layout, text: &str) -> Result<Vec<BillingPeriod>> {
    let sheet = Sheet::parse(text, layout.date)?;
    let columns = Columns::resolve(layout, &sheet)?;
    Ok(periods_in_column(&sheet, columns.date))
}

/// Turn the rows belonging to `batch.period` into ledger rows.
pub fn normalize(layout: &Layout, batch: &RawBatch, part: &str) -> Result<Normalized> {
    let sheet = Sheet::parse(text_of(batch, part)?, layout.date)?;
    let columns = Columns::resolve(layout, &sheet)?;

    let mut charges = Vec::new();
    for record in sheet.records() {
        let Some(row_period) = period(record.get(columns.date)) else {
            tracing::debug!(
                "Skipping a row with no date: {:?}",
                record.get(columns.date)
            );
            continue;
        };
        if row_period != batch.period {
            continue;
        }

        let (start, end) = charge_period(&record, &columns, row_period);
        let currency = record
            .text(columns.currency)
            .map(|currency| currency.to_uppercase())
            .unwrap_or_else(|| layout.default_currency.to_string());

        // Every token column this row actually filled in.
        let counted: Vec<(&'static str, f64)> = columns
            .tokens
            .iter()
            .map(|(unit, column)| Ok((*unit, record.quantity(Some(*column))?)))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(unit, count)| count.map(|count| (unit, count)))
            .filter(|(_, count)| *count != 0.0)
            .collect();

        let template = || Charge {
            service_name: Some(layout.service_name.to_string()),
            // The model is what a model provider's spend is grouped by.
            service_category: record.text(columns.model),
            charge_description: record
                .text(columns.description)
                .or_else(|| record.text(columns.model)),
            // A project or workspace is the provider-side account the
            // charge was billed against, which is what this column is for.
            billing_account_id: record.text(columns.scope_id),
            resource_name: record.text(columns.scope_name),
            ..Charge::new(start, end, currency.clone())
        };

        match record.amount(columns.cost)? {
            Some(cost) => charges.push(Charge {
                billed_cost: Some(cost),
                // A quantity only when it is the quantity this amount was
                // charged for; two token types at two prices do not add up
                // to one.
                pricing_quantity: match counted.as_slice() {
                    [(_, count)] => Some(*count),
                    _ => None,
                },
                pricing_unit: match counted.as_slice() {
                    [(unit, _)] => Some((*unit).to_string()),
                    _ => None,
                },
                ..template()
            }),
            // A usage export: what was consumed, with no claim about what
            // it cost.
            None => charges.extend(counted.iter().map(|(unit, count)| Charge {
                cost_basis: CostBasis::Absent,
                billed_cost: None,
                pricing_quantity: Some(*count),
                pricing_unit: Some((*unit).to_string()),
                ..template()
            })),
        }
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

/// The span a row covers: its own day, or the whole month when the export
/// is a monthly one.
fn charge_period(
    record: &Record<'_>,
    columns: &Columns,
    row_period: BillingPeriod,
) -> (DateTime<Utc>, DateTime<Utc>) {
    match super::date(record.get(columns.date)) {
        Some(day) => (
            midnight(day),
            midnight(day.succ_opt().expect("a bill date has a following day")),
        ),
        None => (
            midnight(row_period.start()),
            midnight(row_period.end_exclusive()),
        ),
    }
}

fn midnight(day: NaiveDate) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc()
}
