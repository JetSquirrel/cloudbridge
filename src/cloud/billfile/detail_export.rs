//! The bill detail export both Chinese clouds produce.
//!
//! Alibaba Cloud and Volcengine publish the same table under different
//! column names: one row per instance per billing item, carrying the list
//! price, the deductions taken off it, and what was actually charged. The
//! shape is identical, so it is read once here and each provider supplies
//! only a [`Layout`] — the names its console happens to use.
//!
//! Both consoles emit Chinese *or* English headers depending on which one
//! the export was taken from, and neither is documented as a file format,
//! so every column is looked up through a list of aliases. A column that
//! matches nothing is an error naming what the file does have, because the
//! fix is to add an alias and the message is what says which.

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};

use super::{date, period, periods_in_column, tags_json, text_of, Record, Sheet};
use crate::cloud::{deduction, BillingPeriod, Normalized, RawBatch};
use crate::ledger::Charge;

/// The column names one provider's bill detail export uses.
///
/// Every field but `net` is optional in the file: consoles let the user
/// choose the columns, and a narrower export is still a bill. `net` is the
/// one figure without which there is nothing to record.
pub struct Layout {
    /// Columns the header row is found by — every alias that can carry a
    /// row's date, since that is the column these exports always have.
    pub anchors: &'static [&'static str],
    /// The month a row belongs to.
    pub billing_cycle: &'static [&'static str],
    /// The day within it, in a daily export.
    pub billing_date: &'static [&'static str],
    pub product_name: &'static [&'static str],
    pub product_code: &'static [&'static str],
    pub product_detail: &'static [&'static str],
    pub billing_item: &'static [&'static str],
    pub instance_id: &'static [&'static str],
    pub instance_name: &'static [&'static str],
    pub region: &'static [&'static str],
    pub currency: &'static [&'static str],
    /// Currency for an export that names none.
    pub default_currency: &'static str,
    pub usage: &'static [&'static str],
    pub usage_unit: &'static [&'static str],
    pub tag: &'static [&'static str],
    /// What the line would have cost before any deduction.
    pub gross: &'static [&'static str],
    /// What was actually charged, after all of them.
    pub net: &'static [&'static str],
    /// The deductions this export names, each paired with the name the
    /// ledger records it under — which is the provider's own API field
    /// name, so a credit is the same thing whichever channel it arrived
    /// through.
    pub deductions: &'static [(&'static str, &'static [&'static str])],
}

/// Where each of a [`Layout`]'s columns actually is, resolved once.
struct Columns {
    period: usize,
    day: Option<usize>,
    product_name: Option<usize>,
    product_code: Option<usize>,
    product_detail: Option<usize>,
    billing_item: Option<usize>,
    instance_id: Option<usize>,
    instance_name: Option<usize>,
    region: Option<usize>,
    currency: Option<usize>,
    usage: Option<usize>,
    usage_unit: Option<usize>,
    tag: Option<usize>,
    gross: Option<usize>,
    net: usize,
    /// Only the deductions this particular export carries.
    deductions: Vec<(&'static str, usize)>,
}

impl Columns {
    fn resolve(layout: &Layout, sheet: &Sheet) -> Result<Self> {
        // The billing cycle names the month outright. A daily export that
        // omits it still has a date the month can be read off.
        let period = match sheet.column(layout.billing_cycle) {
            Some(column) => column,
            None => sheet.require("billing cycle", layout.anchors)?,
        };

        Ok(Self {
            period,
            day: sheet.column(layout.billing_date),
            product_name: sheet.column(layout.product_name),
            product_code: sheet.column(layout.product_code),
            product_detail: sheet.column(layout.product_detail),
            billing_item: sheet.column(layout.billing_item),
            instance_id: sheet.column(layout.instance_id),
            instance_name: sheet.column(layout.instance_name),
            region: sheet.column(layout.region),
            currency: sheet.column(layout.currency),
            usage: sheet.column(layout.usage),
            usage_unit: sheet.column(layout.usage_unit),
            tag: sheet.column(layout.tag),
            gross: sheet.column(layout.gross),
            net: sheet.require("net amount", layout.net)?,
            deductions: layout
                .deductions
                .iter()
                .filter_map(|(name, aliases)| sheet.column(aliases).map(|column| (*name, column)))
                .collect(),
        })
    }
}

/// The months an export covers, oldest first.
pub fn periods(layout: &Layout, text: &str) -> Result<Vec<BillingPeriod>> {
    let sheet = Sheet::parse(text, layout.anchors)?;
    let columns = Columns::resolve(layout, &sheet)?;
    Ok(periods_in_column(&sheet, columns.period))
}

/// Turn the rows belonging to `batch.period` into ledger rows.
///
/// Pure: the file's text and the period are both on the batch. Rows for
/// other months are skipped rather than rewritten, because each month is
/// imported as its own whole-period replacement.
pub fn normalize(layout: &Layout, batch: &RawBatch, part: &str) -> Result<Normalized> {
    let sheet = Sheet::parse(text_of(batch, part)?, layout.anchors)?;
    let columns = Columns::resolve(layout, &sheet)?;

    let mut charges = Vec::new();
    for record in sheet.records() {
        // A row with no readable month is the total line the console
        // appends under the table, or a note. It carries amounts, so it has
        // to be skipped deliberately rather than added to the month.
        let Some(row_period) = period(record.get(columns.period)) else {
            tracing::debug!(
                "Skipping a row with no billing cycle: {:?}",
                record.get(columns.period)
            );
            continue;
        };
        if row_period != batch.period {
            continue;
        }

        let net = record.amount(Some(columns.net))?.unwrap_or(0.0);
        // An export without the list price is read at what was charged: the
        // line still lands, it simply has no discount to decompose.
        let gross = record.amount(columns.gross)?.unwrap_or(net);
        if net == 0.0 && gross == 0.0 {
            continue;
        }

        let (start, end) = charge_period(&record, &columns, row_period);
        let currency = currency_of(&record, &columns, layout.default_currency);
        let quantity = record.quantity(columns.usage)?;

        let template = || Charge {
            service_name: record.text(columns.product_name),
            // A product code is stable across locales; a product name is
            // not. The billing API files it here too, so the two channels
            // group the same way.
            service_category: record.text(columns.product_code),
            // For a model service this is the model and what was metered.
            charge_description: record
                .text(columns.billing_item)
                .or_else(|| record.text(columns.product_detail)),
            resource_id: record.text(columns.instance_id),
            resource_name: record
                .text(columns.instance_name)
                .or_else(|| record.text(columns.product_detail)),
            region_id: record.text(columns.region),
            pricing_quantity: quantity,
            pricing_unit: record.text(columns.usage_unit),
            tags: record.optional(columns.tag).and_then(tags_json),
            ..Charge::new(start, end, currency.clone())
        };

        let deductions = columns
            .deductions
            .iter()
            .map(|(name, column)| Ok((*name, record.amount(Some(*column))?.unwrap_or(0.0))))
            .collect::<Result<Vec<_>>>()?;

        deduction::decompose(
            &mut charges,
            template,
            gross,
            net,
            &deductions,
            &describe(&record, &columns),
        );
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

/// The span a row covers: the day it is dated, or the whole month when the
/// export is a monthly one.
fn charge_period(
    record: &Record<'_>,
    columns: &Columns,
    row_period: BillingPeriod,
) -> (DateTime<Utc>, DateTime<Utc>) {
    match columns.day.and_then(|column| date(record.get(column))) {
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

/// The currency a row is billed in, as a code.
///
/// A Chinese console writes the name rather than the code, and the ledger
/// converts through `dim_fx_rate`, which is keyed on the code.
fn currency_of(record: &Record<'_>, columns: &Columns, default: &str) -> String {
    match record.optional(columns.currency) {
        None => default.to_string(),
        Some("人民币") | Some("CNY") | Some("元") => "CNY".to_string(),
        Some("美元") | Some("USD") => "USD".to_string(),
        Some(other) => other.to_string(),
    }
}

/// How a row is named in the warning an unreconciled line produces.
fn describe(record: &Record<'_>, columns: &Columns) -> String {
    [
        columns.product_name,
        columns.billing_item,
        columns.instance_id,
    ]
    .into_iter()
    .filter_map(|column| record.optional(column))
    .collect::<Vec<_>>()
    .join(" / ")
}
