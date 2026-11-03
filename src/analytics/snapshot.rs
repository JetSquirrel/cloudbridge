//! Month-end snapshots: everything computed from a frozen copy of a bill.
//!
//! Each backend stores the rows its own way; the identity of a charge, what
//! a checksum covers, how totals are reached and how two versions of a month
//! differ are decided here, once, for both.
//!
//! Totals are always kept per billing currency. An amount in one currency is
//! never added to another, so a frozen total can be read back without any FX
//! table. The rate a row converted at is recorded beside it, for the reporting
//! figure the month showed on the day it was frozen.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use super::{midnight, period_of};
use crate::model::{
    AmountDelta, BillingSnapshot, Channel, Charge, CurrencyDelta, PeriodKey, SnapshotComparison,
    SnapshotLine, SnapshotRate, SnapshotTotal,
};

/// The format a charge's period takes in its identity.
const IDENTITY_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Differences below this are floating-point residue from summing, not money.
const RESIDUE: f64 = 1e-9;

/// Where a period's current rows came from.
#[derive(Debug, Clone)]
pub struct BatchSource {
    pub batch_id: Option<String>,
    pub channel: Channel,
    pub source_ref: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// A stable identity for each charge of a period.
///
/// The natural key leaves the amounts out, so the same line of a corrected
/// bill keeps its identity and can be recognized as changed. Rows sharing a
/// natural key are told apart by the order they arrive in.
pub fn charge_ids(key: &PeriodKey, charges: &[Charge]) -> Vec<String> {
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut ids = Vec::with_capacity(charges.len());

    for charge in charges {
        let mut natural_key = [
            key.provider.as_str(),
            key.account_id.as_str(),
            key.billing_period.as_str(),
            &charge
                .charge_period_start
                .format(IDENTITY_TIMESTAMP_FORMAT)
                .to_string(),
            &charge
                .charge_period_end
                .format(IDENTITY_TIMESTAMP_FORMAT)
                .to_string(),
            charge.charge_category.as_str(),
            charge.billing_account_id.as_deref().unwrap_or(""),
            charge.service_name.as_deref().unwrap_or(""),
            charge.resource_id.as_deref().unwrap_or(""),
            charge.region_id.as_deref().unwrap_or(""),
            charge.charge_description.as_deref().unwrap_or(""),
            charge.pricing_unit.as_deref().unwrap_or(""),
            charge.billing_currency.as_str(),
        ]
        .join("\u{1f}");
        // Appended only when present, so a row without one keeps the id it
        // had before the column existed.
        if let Some(sub_account) = &charge.sub_account_id {
            natural_key.push_str("\u{1f}sub_account=");
            natural_key.push_str(sub_account);
        }

        let occurrence = seen.entry(natural_key.clone()).or_insert(0);
        let mut hasher = Sha256::new();
        hasher.update(natural_key.as_bytes());
        hasher.update(format!("\u{1f}{}", occurrence).as_bytes());
        *occurrence += 1;

        ids.push(hex::encode(hasher.finalize())[..32].to_string());
    }

    ids
}

/// Every billing field of a line, in a form two reads compare equal on.
///
/// Amounts are written as their bits, so a checksum cannot drift with how a
/// float prints. The FX rate is left out on purpose: it describes the day of
/// the freeze, not the bill, and changing the reporting currency does not
/// make a month's bill different.
fn canonical(line: &SnapshotLine) -> String {
    let charge = &line.charge;
    let start = charge.charge_period_start.to_rfc3339();
    let end = charge.charge_period_end.to_rfc3339();
    let mut out = String::with_capacity(256);
    for value in [
        Some(line.charge_id.as_str()),
        Some(start.as_str()),
        Some(end.as_str()),
        Some(charge.charge_category.as_str()),
        Some(charge.cost_basis.as_str()),
        Some(charge.billing_currency.as_str()),
        charge.billing_account_id.as_deref(),
        charge.sub_account_id.as_deref(),
        charge.sub_account_name.as_deref(),
        charge.charge_description.as_deref(),
        charge.service_name.as_deref(),
        charge.service_category.as_deref(),
        charge.service_subcategory.as_deref(),
        charge.x_service_code.as_deref(),
        charge.x_model.as_deref(),
        charge.resource_id.as_deref(),
        charge.resource_name.as_deref(),
        charge.region_id.as_deref(),
        charge.pricing_unit.as_deref(),
        charge.tags.as_deref(),
        charge.consumed_unit.as_deref(),
        charge.sku_meter.as_deref(),
        charge.sku_price_details.as_deref(),
        charge.principal_id.as_deref(),
        charge.credential_id.as_deref(),
        charge.requester_details.as_deref(),
    ] {
        match value {
            Some(value) => {
                out.push('s');
                out.push_str(value);
            }
            None => out.push('n'),
        }
        out.push('\u{1f}');
    }
    for value in [
        charge.billed_cost,
        charge.effective_cost,
        charge.list_cost,
        charge.pricing_quantity,
        charge.consumed_quantity,
    ] {
        match value {
            Some(value) => out.push_str(&format!("f{:016x}", value.to_bits())),
            None => out.push('n'),
        }
        out.push('\u{1f}');
    }
    out
}

fn sorted(lines: &[SnapshotLine]) -> Vec<&SnapshotLine> {
    let mut sorted: Vec<&SnapshotLine> = lines.iter().collect();
    sorted.sort_by(|a, b| a.charge_id.cmp(&b.charge_id));
    sorted
}

/// SHA-256 over every billing field of every line, in `charge_id` order.
pub fn checksum(lines: &[SnapshotLine]) -> String {
    let mut hasher = Sha256::new();
    for line in sorted(lines) {
        hasher.update(canonical(line).as_bytes());
        hasher.update(b"\x1e");
    }
    hex::encode(hasher.finalize())
}

/// Totals per billing currency and charge category.
///
/// A row with no amount is counted, not summed as zero: it is what lets a
/// usage-only month read as having no amounts rather than as costing nothing.
pub fn summarize(lines: &[SnapshotLine]) -> Vec<SnapshotTotal> {
    let mut totals: BTreeMap<(String, String), SnapshotTotal> = BTreeMap::new();
    for line in sorted(lines) {
        let charge = &line.charge;
        let category = charge.charge_category.as_str().to_string();
        let total = totals
            .entry((charge.billing_currency.clone(), category.clone()))
            .or_insert_with(|| SnapshotTotal {
                currency: charge.billing_currency.clone(),
                category,
                billed_cost: 0.0,
                effective_cost: 0.0,
                billed_cost_base: 0.0,
                rows: 0,
                rows_without_amount: 0,
                rows_unconverted: 0,
            });
        total.rows += 1;
        total.effective_cost += charge.effective_cost.unwrap_or(0.0);
        match charge.billed_cost {
            Some(amount) => {
                total.billed_cost += amount;
                match line.fx_rate {
                    Some(rate) => total.billed_cost_base += amount * rate,
                    None => total.rows_unconverted += 1,
                }
            }
            None => total.rows_without_amount += 1,
        }
    }
    totals.into_values().collect()
}

/// The rates a period's lines converted at, per billing currency. A month
/// spanning a rate change has more than one for a currency; `rate: None` is
/// the rows no rate covered.
pub fn rates(lines: &[SnapshotLine]) -> Vec<SnapshotRate> {
    let mut rates: BTreeMap<(String, Option<u64>), u64> = BTreeMap::new();
    for line in lines {
        *rates
            .entry((
                line.charge.billing_currency.clone(),
                line.fx_rate.map(f64::to_bits),
            ))
            .or_insert(0) += 1;
    }
    rates
        .into_iter()
        .map(|((currency, rate), rows)| SnapshotRate {
            currency,
            rate: rate.map(f64::from_bits),
            rows,
        })
        .collect()
}

/// A freeze of `lines`, as revision `revision` of the period.
pub fn build(
    key: &PeriodKey,
    revision: u32,
    lines: &[SnapshotLine],
    source: BatchSource,
    reporting_currency: &str,
    now: DateTime<Utc>,
) -> Result<BillingSnapshot> {
    Ok(BillingSnapshot {
        snapshot_id: uuid::Uuid::new_v4().to_string(),
        key: key.clone(),
        revision,
        frozen_at: now,
        batch_id: source.batch_id,
        channel: source.channel,
        source_ref: source.source_ref,
        batch_completed_at: source.completed_at,
        row_count: lines.len() as u64,
        checksum: checksum(lines),
        reporting_currency: reporting_currency.to_string(),
        period_open: period_is_open(&key.billing_period, now)?,
        totals: summarize(lines),
        rates: rates(lines),
    })
}

/// Whether a `YYYY-MM` month has yet to end at `now`.
pub fn period_is_open(billing_period: &str, now: DateTime<Utc>) -> Result<bool> {
    let period = period_of(billing_period)?;
    Ok(now < midnight(period.end_exclusive()))
}

/// Whether stored lines still are what was frozen: the same content under the
/// checksum, and the recorded totals reachable from them alone.
pub fn verify(snapshot: &BillingSnapshot, lines: &[SnapshotLine]) -> bool {
    if lines.len() as u64 != snapshot.row_count || checksum(lines) != snapshot.checksum {
        return false;
    }
    let recomputed = summarize(lines);
    recomputed.len() == snapshot.totals.len()
        && recomputed.iter().zip(&snapshot.totals).all(|(a, b)| {
            a.currency == b.currency
                && a.category == b.category
                && a.rows == b.rows
                && a.rows_without_amount == b.rows_without_amount
                && (a.billed_cost - b.billed_cost).abs() < 1e-6
                && (a.effective_cost - b.effective_cost).abs() < 1e-6
        })
}

/// The billed total per currency. `None` for a currency none of whose rows
/// carries an amount: that is no total, not a total of zero.
pub fn currency_totals(totals: &[SnapshotTotal]) -> Vec<(String, Option<f64>)> {
    let mut by_currency: BTreeMap<&str, Option<f64>> = BTreeMap::new();
    for total in totals {
        let slot = by_currency.entry(&total.currency).or_insert(None);
        if total.rows > total.rows_without_amount {
            *slot = Some(slot.unwrap_or(0.0) + total.billed_cost);
        }
    }
    by_currency
        .into_iter()
        .map(|(currency, total)| (currency.to_string(), total))
        .collect()
}

/// How a frozen month differs from the ledger's current one.
pub fn compare(frozen: &[SnapshotLine], current: &[SnapshotLine]) -> SnapshotComparison {
    let frozen_by_id: HashMap<&str, &SnapshotLine> = frozen
        .iter()
        .map(|line| (line.charge_id.as_str(), line))
        .collect();
    let current_by_id: HashMap<&str, &SnapshotLine> = current
        .iter()
        .map(|line| (line.charge_id.as_str(), line))
        .collect();

    let mut comparison = SnapshotComparison::default();
    for line in current {
        match frozen_by_id.get(line.charge_id.as_str()) {
            None => comparison.new_lines += 1,
            Some(was) if canonical(was) != canonical(line) => comparison.changed_lines += 1,
            Some(_) => comparison.unchanged_lines += 1,
        }
    }
    comparison.removed_lines = frozen
        .iter()
        .filter(|line| !current_by_id.contains_key(line.charge_id.as_str()))
        .count();

    comparison.by_service = deltas(frozen, current, |charge| {
        charge
            .service_name
            .clone()
            .unwrap_or_else(|| "Other".to_string())
    });
    comparison.by_category = deltas(frozen, current, |charge| {
        charge.charge_category.as_str().to_string()
    });

    let frozen_totals = currency_totals(&summarize(frozen));
    let current_totals = currency_totals(&summarize(current));
    let mut currencies: Vec<&str> = frozen_totals
        .iter()
        .chain(&current_totals)
        .map(|(currency, _)| currency.as_str())
        .collect();
    currencies.sort_unstable();
    currencies.dedup();
    let total_of = |totals: &[(String, Option<f64>)], currency: &str| {
        totals
            .iter()
            .find(|(c, _)| c == currency)
            .and_then(|(_, total)| *total)
    };
    comparison.by_currency = currencies
        .into_iter()
        .map(|currency| {
            let frozen = total_of(&frozen_totals, currency);
            let current = total_of(&current_totals, currency);
            CurrencyDelta {
                currency: currency.to_string(),
                frozen,
                current,
                delta: current.unwrap_or(0.0) - frozen.unwrap_or(0.0),
            }
        })
        .collect();
    comparison
}

/// Billed amount per `(group, currency)` on each side, keeping only what
/// moved, largest move first.
fn deltas(
    frozen: &[SnapshotLine],
    current: &[SnapshotLine],
    group: impl Fn(&Charge) -> String,
) -> Vec<AmountDelta> {
    let mut sums: BTreeMap<(String, String), (f64, f64)> = BTreeMap::new();
    for line in frozen {
        let sum = sums
            .entry((group(&line.charge), line.charge.billing_currency.clone()))
            .or_default();
        sum.0 += line.charge.billed_cost.unwrap_or(0.0);
    }
    for line in current {
        let sum = sums
            .entry((group(&line.charge), line.charge.billing_currency.clone()))
            .or_default();
        sum.1 += line.charge.billed_cost.unwrap_or(0.0);
    }
    let mut moved: Vec<AmountDelta> = sums
        .into_iter()
        .filter(|(_, (frozen, current))| (current - frozen).abs() > RESIDUE)
        .map(|((label, currency), (frozen, current))| AmountDelta {
            label,
            currency,
            frozen,
            current,
            delta: current - frozen,
        })
        .collect();
    moved.sort_by(|a, b| b.delta.abs().total_cmp(&a.delta.abs()));
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChargeCategory, CostBasis};
    use chrono::TimeZone;

    fn key() -> PeriodKey {
        PeriodKey::new("AWS", "acct-1", "2026-09")
    }

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, day, 0, 0, 0).unwrap()
    }

    fn charge(service: &str, currency: &str, amount: Option<f64>) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: amount,
            effective_cost: amount,
            cost_basis: if amount.is_some() {
                CostBasis::Authoritative
            } else {
                CostBasis::Absent
            },
            ..Charge::new(at(1), at(2), currency)
        }
    }

    fn lines(charges: Vec<Charge>, fx_rate: Option<f64>) -> Vec<SnapshotLine> {
        charge_ids(&key(), &charges)
            .into_iter()
            .zip(charges)
            .map(|(charge_id, charge)| SnapshotLine {
                charge_id,
                charge,
                fx_rate,
            })
            .collect()
    }

    fn source() -> BatchSource {
        BatchSource {
            batch_id: Some("b-1".to_string()),
            channel: Channel::File,
            source_ref: None,
            completed_at: Some(at(3)),
        }
    }

    #[test]
    fn a_checksum_ignores_row_order_and_the_rate_but_not_an_amount() {
        let a = lines(
            vec![
                charge("EC2", "USD", Some(10.0)),
                charge("S3", "USD", Some(2.0)),
            ],
            Some(1.0),
        );
        let mut reversed = a.clone();
        reversed.reverse();
        assert_eq!(checksum(&a), checksum(&reversed));

        let mut other_rate = a.clone();
        other_rate[0].fx_rate = Some(7.1);
        assert_eq!(checksum(&a), checksum(&other_rate));

        let mut corrected = a.clone();
        corrected[0].charge.billed_cost = Some(10.01);
        assert_ne!(checksum(&a), checksum(&corrected));
    }

    #[test]
    fn an_absent_amount_differs_from_an_amount_of_zero() {
        let absent = lines(vec![charge("EC2", "USD", None)], None);
        let zero = lines(vec![charge("EC2", "USD", Some(0.0))], None);
        assert_ne!(checksum(&absent), checksum(&zero));
    }

    #[test]
    fn totals_stay_in_their_billing_currency_and_keep_credits_and_adjustments() {
        let credit = Charge {
            charge_category: ChargeCategory::Credit,
            ..charge("EC2", "USD", Some(-4.0))
        };
        let adjustment = Charge {
            charge_category: ChargeCategory::Adjustment,
            ..charge("EC2", "USD", Some(1.5))
        };
        let frozen = lines(
            vec![
                charge("EC2", "USD", Some(10.0)),
                credit,
                adjustment,
                charge("ECS", "CNY", Some(70.0)),
            ],
            Some(1.0),
        );
        let totals = summarize(&frozen);
        assert_eq!(
            currency_totals(&totals),
            vec![
                ("CNY".to_string(), Some(70.0)),
                ("USD".to_string(), Some(7.5)),
            ]
        );
        let category = |name: &str| {
            totals
                .iter()
                .find(|t| t.currency == "USD" && t.category == name)
                .unwrap()
                .billed_cost
        };
        assert_eq!(category("Credit"), -4.0);
        assert_eq!(category("Adjustment"), 1.5);
    }

    #[test]
    fn a_usage_only_month_has_no_total_rather_than_a_zero_one() {
        let totals = summarize(&lines(vec![charge("Tokens", "USD", None)], None));
        assert_eq!(totals[0].rows_without_amount, 1);
        assert_eq!(currency_totals(&totals), vec![("USD".to_string(), None)]);
    }

    #[test]
    fn a_row_with_no_rate_is_counted_and_left_out_of_the_reporting_figure() {
        let mut mixed = lines(
            vec![
                charge("EC2", "EUR", Some(10.0)),
                charge("S3", "EUR", Some(5.0)),
            ],
            Some(2.0),
        );
        mixed[1].fx_rate = None;
        let totals = summarize(&mixed);
        assert_eq!(totals[0].billed_cost, 15.0);
        assert_eq!(totals[0].billed_cost_base, 20.0);
        assert_eq!(totals[0].rows_unconverted, 1);
        let rates = rates(&mixed);
        assert_eq!(rates.len(), 2);
        assert!(rates
            .iter()
            .any(|rate| rate.rate.is_none() && rate.rows == 1));
    }

    #[test]
    fn a_freeze_inside_the_month_is_marked_open() {
        let frozen = lines(vec![charge("EC2", "USD", Some(1.0))], Some(1.0));
        let inside = build(&key(), 1, &frozen, source(), "USD", at(30)).unwrap();
        assert!(inside.period_open);
        let after = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        let closed = build(&key(), 1, &frozen, source(), "USD", after).unwrap();
        assert!(!closed.period_open);
    }

    #[test]
    fn verifying_catches_a_changed_line_and_a_missing_line() {
        let frozen = lines(
            vec![
                charge("EC2", "USD", Some(10.0)),
                charge("S3", "USD", Some(2.0)),
            ],
            Some(1.0),
        );
        let snapshot = build(&key(), 1, &frozen, source(), "USD", at(30)).unwrap();
        assert!(verify(&snapshot, &frozen));

        let mut changed = frozen.clone();
        changed[0].charge.billed_cost = Some(11.0);
        assert!(!verify(&snapshot, &changed));
        assert!(!verify(&snapshot, &frozen[..1]));
    }

    #[test]
    fn comparing_names_new_removed_and_changed_lines_and_the_money_moved() {
        let frozen = lines(
            vec![
                charge("EC2", "USD", Some(10.0)),
                charge("S3", "USD", Some(2.0)),
                charge("RDS", "USD", Some(5.0)),
                charge("Lambda", "USD", Some(1.0)),
            ],
            Some(1.0),
        );
        let current = lines(
            vec![
                charge("EC2", "USD", Some(12.5)),
                charge("S3", "USD", Some(2.0)),
                charge("RDS", "USD", Some(5.0)),
                charge("Glue", "USD", Some(3.0)),
            ],
            Some(1.0),
        );
        let diff = compare(&frozen, &current);
        assert_eq!(diff.new_lines, 1);
        assert_eq!(diff.removed_lines, 1);
        assert_eq!(diff.changed_lines, 1);
        assert_eq!(diff.unchanged_lines, 2);
        assert!(!diff.is_identical());
        assert_eq!(diff.by_currency[0].frozen, Some(18.0));
        assert_eq!(diff.by_currency[0].current, Some(22.5));
        assert_eq!(diff.by_currency[0].delta, 4.5);
        assert_eq!(
            diff.by_service
                .iter()
                .map(|d| (d.label.as_str(), d.delta))
                .collect::<Vec<_>>(),
            vec![("Glue", 3.0), ("EC2", 2.5), ("Lambda", -1.0)]
        );
        assert_eq!(diff.by_category[0].label, "Usage");
        assert_eq!(diff.by_category[0].delta, 4.5);
    }

    #[test]
    fn an_amount_arriving_on_a_usage_row_is_a_change_not_a_new_line() {
        let frozen = lines(vec![charge("Tokens", "USD", None)], None);
        let current = lines(vec![charge("Tokens", "USD", Some(3.0))], None);
        let diff = compare(&frozen, &current);
        assert_eq!(diff.changed_lines, 1);
        assert_eq!(diff.new_lines, 0);
        assert_eq!(diff.by_currency[0].frozen, None);
        assert_eq!(diff.by_currency[0].current, Some(3.0));
    }

    #[test]
    fn comparing_a_month_with_itself_finds_nothing() {
        let frozen = lines(vec![charge("EC2", "USD", Some(10.0))], Some(1.0));
        let diff = compare(&frozen, &frozen);
        assert!(diff.is_identical());
        assert!(diff.by_service.is_empty());
        assert_eq!(diff.by_currency[0].delta, 0.0);
    }

    #[test]
    fn a_currency_only_on_one_side_still_shows() {
        let frozen = lines(vec![charge("EC2", "USD", Some(10.0))], Some(1.0));
        let current = lines(vec![charge("EC2", "CNY", Some(71.0))], Some(1.0));
        let diff = compare(&frozen, &current);
        assert_eq!(diff.new_lines, 1);
        assert_eq!(diff.removed_lines, 1);
        assert_eq!(diff.by_currency.len(), 2);
        assert_eq!(diff.by_currency[0].currency, "CNY");
        assert_eq!(diff.by_currency[0].frozen, None);
        assert_eq!(diff.by_currency[1].current, None);
    }
}
