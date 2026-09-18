//! Reading the ledger, from memory.
//!
//! [`crate::ledger::query`] answers each of these with SQL against the
//! normalized view. Here the view is [`memory::normalized`] and the answers
//! are folds over it: same scope, same grouping, same ordering, so a caller
//! cannot tell which target answered it — except where the browser genuinely
//! has less to answer with, and each of those two says so where it stands.
//!
//! Two properties of the SQL are easy to lose in a rewrite and are kept on
//! purpose. A charge no FX rate covers has a `None` amount, and `sum`
//! ignores NULL — it adds nothing to a converted total rather than adding
//! zero. And every grouping that ranks is written with `HAVING amount > 0`,
//! so a bucket that nets to nothing is absent from the list, not present as
//! a zero row.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};

use super::{Channel, ChargeCategory, PeriodKey};
use crate::analytics::{self, QualityCounts, TwoPeriodBuckets};
use crate::memory::{self, NormalizedRow};
use crate::model::BillingPeriod;
use crate::store::Connection;

pub use crate::model::{
    AdhocResult, Balance, BreakdownDim, CategoryDelta, CostChangeDecomposition, DailyTotal,
    DataQualityIssue, DataQualityKind, ForecastBands, IssueSeverity, MovementKind, PeriodForecast,
    PeriodOverPeriod, ServiceDailyTotal, ServiceMovement, ServiceTagUsage, TopResource,
    UntaggedCharge, UntaggedServiceUsage,
};

/// The bucket a charge with no value for the tag key lands in, as the view's
/// `coalesce(nullif(json_extract_string(tags, ?), ''), 'Unallocated')` spells
/// it.
const UNALLOCATED: &str = "Unallocated";

// ==================== Scope and sums ====================

/// The axes a read filters on. `None` means "unfiltered", so a cross-account
/// read is the same read as its per-account counterpart with a wider scope.
#[derive(Debug, Default, Clone, Copy)]
struct Scope<'a> {
    provider: Option<&'a str>,
    account_id: Option<&'a str>,
    billing_period: Option<&'a str>,
    /// Matched against the coalesced service name, as the tag breakdowns
    /// scope: a charge with no service reads as `'Other'`.
    service: Option<&'a str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    /// Usage rows only, so a credit does not shrink what was consumed.
    usage_only: bool,
}

impl Scope<'_> {
    fn matches(&self, row: &NormalizedRow) -> bool {
        if self.usage_only && row.charge_category != ChargeCategory::Usage {
            return false;
        }

        // A `None` axis is unfiltered, which is not the same as matching an
        // empty string.
        let chosen = |expected: Option<&str>, actual: &str| match expected {
            Some(expected) => expected == actual,
            None => true,
        };
        if !chosen(self.provider, &row.provider)
            || !chosen(self.account_id, &row.account_id)
            || !chosen(self.billing_period, &row.billing_period)
            || !chosen(self.service, service_of(row))
        {
            return false;
        }

        // The view's bounds are `charge_period_start >= since` and
        // `charge_period_start < until`.
        !self
            .since
            .is_some_and(|since| row.charge_period_start < since)
            && !self
                .until
                .is_some_and(|until| row.charge_period_start >= until)
    }
}

/// `coalesce(service_name, 'Other')`, the name every service grouping uses.
fn service_of(row: &NormalizedRow) -> &str {
    row.service_name.as_deref().unwrap_or("Other")
}

/// Run a read against the converted ledger.
fn read<T>(f: impl FnOnce(&[NormalizedRow]) -> T) -> Result<T> {
    memory::with_store(|store| Ok(f(&memory::normalized(store))))
}

/// The converted rows `scope` selects.
fn selected<'a>(all: &'a [NormalizedRow], scope: &Scope<'_>) -> Vec<&'a NormalizedRow> {
    all.iter().filter(|row| scope.matches(row)).collect()
}

/// A net total across every charge category — 0.0 for an empty scope.
///
/// Rows no rate covers contribute nothing, as DuckDB's `sum` ignores NULL:
/// adding them at par would make a total over currencies that were never
/// comparable.
fn net_total(rows: &[&NormalizedRow]) -> f64 {
    rows.iter().filter_map(|row| row.billed_cost_base).sum()
}

/// Gross usage and credits, in the reporting currency.
///
/// Net totals hide credit-covered spend: an account whose usage is fully
/// offset reads as $0 while it really consumed dollars. Tax and Purchase
/// rows are in neither bucket — they still count toward the net total.
fn usage_and_credits_of_rows(rows: &[&NormalizedRow]) -> (f64, f64) {
    let mut usage = 0.0;
    let mut credits = 0.0;
    for row in rows {
        let Some(amount) = row.billed_cost_base else {
            continue;
        };
        match row.charge_category {
            ChargeCategory::Usage => usage += amount,
            ChargeCategory::Credit | ChargeCategory::Adjustment => credits += amount,
            ChargeCategory::Purchase | ChargeCategory::Tax => {}
        }
    }

    (usage, credits)
}

/// Charges over `rows` grouped by `bucket`, in bucket order — the reads that
/// are `GROUP BY … ORDER BY` on the key rather than on the amount.
///
/// A row with no converted amount adds zero, so a bucket whose rows are all
/// unconverted sums to 0.0 — what `sum(…) IS NULL` read through
/// `unwrap_or(0.0)` produced.
fn totals_by_key<K: Ord>(
    rows: &[&NormalizedRow],
    bucket: impl Fn(&NormalizedRow) -> K,
) -> Vec<(K, f64)> {
    let mut sums: BTreeMap<K, f64> = BTreeMap::new();
    for row in rows {
        *sums.entry(bucket(row)).or_insert(0.0) += row.billed_cost_base.unwrap_or(0.0);
    }

    sums.into_iter().collect()
}

/// Charges over `rows` grouped by `bucket`, largest first; a bucket that
/// nets to zero or below is dropped, as the `HAVING amount > 0` dropped it.
///
/// The sort is over the grouped map, so buckets of equal amount keep their
/// bucket order and a re-read of the same store orders them the same way.
fn totals_by_bucket<K: Ord>(
    rows: &[&NormalizedRow],
    bucket: impl Fn(&NormalizedRow) -> K,
) -> Vec<(K, f64)> {
    let mut totals: Vec<(K, f64)> = totals_by_key(rows, bucket)
        .into_iter()
        .filter(|(_, amount)| *amount > 0.0)
        .collect();
    totals.sort_by(|a, b| b.1.total_cmp(&a.1));

    totals
}

fn totals_by_day(rows: &[&NormalizedRow]) -> Vec<DailyTotal> {
    totals_by_key(rows, |row| memory::day_of(row.charge_period_start))
}

fn totals_by_billing_period(rows: &[&NormalizedRow]) -> Vec<(String, f64)> {
    totals_by_key(rows, |row| row.billing_period.clone())
}

fn totals_by_provider_service(rows: &[&NormalizedRow]) -> Vec<(String, String, f64)> {
    totals_by_bucket(rows, |row| {
        (row.provider.clone(), service_of(row).to_string())
    })
    .into_iter()
    .map(|((provider, service), amount)| (provider, service, amount))
    .collect()
}

fn totals_by_tag(rows: &[&NormalizedRow], tag_key: &str) -> Vec<(String, f64)> {
    totals_by_bucket(rows, |row| memory::tag_value(row.tags.as_deref(), tag_key))
}

/// Whether a charge carries no value for `tag_key`.
///
/// The view spells this `coalesce(nullif(json_extract_string(tags, ?), ''),
/// '') = ''`; [`memory::tag_value`] answers the same question the other way
/// round, by naming the `'Unallocated'` bucket such a charge lands in. The
/// two differ only for a charge whose tag value is literally `'Unallocated'`,
/// which no other read in the app can tell from an untagged one.
fn untagged(row: &NormalizedRow, tag_key: &str) -> bool {
    memory::tag_value(row.tags.as_deref(), tag_key) == UNALLOCATED
}

/// A bucket expression, as the native `sum_by_bucket` took it. A charge
/// without the dimension reads as `'Other'`, as a charge without a service
/// does.
fn bucket_of(row: &NormalizedRow, dim: BreakdownDim) -> String {
    let named = |value: &Option<String>| value.clone().unwrap_or_else(|| "Other".to_string());
    match dim {
        BreakdownDim::Service => service_of(row).to_string(),
        BreakdownDim::Region => named(&row.region_id),
        BreakdownDim::ServiceCategory => named(&row.service_category),
    }
}

// ==================== Period and per-account totals ====================

/// Total charged in one billing period, in the reporting currency.
pub fn period_total(key: &PeriodKey) -> Result<f64> {
    read(|all| {
        net_total(&selected(
            all,
            &Scope {
                provider: Some(&key.provider),
                account_id: Some(&key.account_id),
                billing_period: Some(&key.billing_period),
                ..Default::default()
            },
        ))
    })
}

/// Total charged across every account in a billing period.
///
/// This is the cross-cloud, cross-currency number: one read, one currency
/// out, no adding up amounts that were never comparable.
pub fn total_for_period(billing_period: &str) -> Result<f64> {
    read(|all| {
        net_total(&selected(
            all,
            &Scope {
                billing_period: Some(billing_period),
                ..Default::default()
            },
        ))
    })
}

/// [`total_for_period`] against a caller's own handle, as
/// [`crate::alerts`] reads it.
pub(crate) fn total_for_period_of(_conn: &Connection, billing_period: &str) -> Result<f64> {
    total_for_period(billing_period)
}

/// Charges of one period grouped by service, largest first.
pub fn service_breakdown(key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    breakdown_by(key, BreakdownDim::Service)
}

/// Charges of one period grouped by `dim`, largest first.
pub fn breakdown_by(key: &PeriodKey, dim: BreakdownDim) -> Result<Vec<(String, f64)>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(&key.provider),
                account_id: Some(&key.account_id),
                billing_period: Some(&key.billing_period),
                ..Default::default()
            },
        );

        totals_by_bucket(&rows, |row| bucket_of(row, dim))
    })
}

/// Charges of one period grouped by `(provider, service)`, largest first.
pub fn provider_service_totals(billing_period: &str) -> Result<Vec<(String, String, f64)>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                billing_period: Some(billing_period),
                ..Default::default()
            },
        );

        totals_by_provider_service(&rows)
    })
}

/// The `limit` costliest resources of a period, largest first — a charge with
/// no `resource_id` cannot be attributed to one and is left out.
pub fn top_resources(key: &PeriodKey, limit: usize) -> Result<Vec<TopResource>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(&key.provider),
                account_id: Some(&key.account_id),
                billing_period: Some(&key.billing_period),
                ..Default::default()
            },
        );

        // `any_value` of the group: a resource's name and service are stable
        // per id, so the first row carrying them names the resource.
        let mut grouped: BTreeMap<String, (Option<String>, String, f64)> = BTreeMap::new();
        for row in rows {
            let Some(resource_id) = row.resource_id.as_ref() else {
                continue;
            };
            let entry = grouped
                .entry(resource_id.clone())
                .or_insert_with(|| (row.resource_name.clone(), service_of(row).to_string(), 0.0));
            entry.2 += row.billed_cost_base.unwrap_or(0.0);
        }

        let mut resources: Vec<TopResource> = grouped
            .into_iter()
            .filter(|(_, (_, _, amount))| *amount > 0.0)
            .map(
                |(resource_id, (resource_name, service, amount))| TopResource {
                    resource_id,
                    resource_name,
                    service,
                    amount,
                },
            )
            .collect();
        resources.sort_by(|a, b| b.amount.total_cmp(&a.amount));
        resources.truncate(limit);

        resources
    })
}

// ==================== Daily and monthly series ====================

/// Daily charge totals for an account since an instant, oldest first.
pub fn daily_totals(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                ..Default::default()
            },
        );

        totals_by_day(&rows)
    })
}

/// Daily charge totals across every provider and account since an instant,
/// oldest first.
pub fn daily_totals_all(since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                since: Some(since),
                ..Default::default()
            },
        );

        totals_by_day(&rows)
    })
}

/// [`daily_totals_all`] for a single account.
pub fn daily_usage_of(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_day(&rows)
    })
}

/// Daily usage totals across every provider and account since an instant,
/// oldest first — like [`daily_totals_all`], but Usage rows only, so a credit
/// landing on one day does not dip the series below what was consumed.
pub fn daily_usage_all(since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                since: Some(since),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_day(&rows)
    })
}

/// Daily charge totals per `(provider, service)` since an instant, oldest
/// first — the input to cost-anomaly detection.
pub fn daily_totals_by_service(since: DateTime<Utc>) -> Result<Vec<ServiceDailyTotal>> {
    service_daily_totals(None, since)
}

/// [`daily_totals_by_service`] against a caller's own handle, as
/// [`crate::alerts`] reads it.
pub(crate) fn daily_totals_by_service_of(
    _conn: &Connection,
    since: DateTime<Utc>,
) -> Result<Vec<ServiceDailyTotal>> {
    service_daily_totals(None, since)
}

/// [`daily_totals_by_service`] for a single account.
///
/// The cross-account read groups across accounts and exposes no account
/// column, so an account-scoped anomaly rule needs its own read of the same
/// view.
pub(crate) fn daily_totals_for_account_of(
    _conn: &Connection,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<ServiceDailyTotal>> {
    service_daily_totals(Some(account_id), since)
}

fn service_daily_totals(
    account_id: Option<&str>,
    since: DateTime<Utc>,
) -> Result<Vec<ServiceDailyTotal>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                account_id,
                since: Some(since),
                ..Default::default()
            },
        );

        // Grouped by `(provider, service, day)` but ordered by day alone, so
        // the grouping map's own order is sorted away below.
        let mut grouped: BTreeMap<(String, String, String), f64> = BTreeMap::new();
        for row in rows {
            let key = (
                row.provider.clone(),
                service_of(row).to_string(),
                memory::day_of(row.charge_period_start),
            );
            *grouped.entry(key).or_insert(0.0) += row.billed_cost_base.unwrap_or(0.0);
        }

        let mut totals: Vec<ServiceDailyTotal> = grouped
            .into_iter()
            .map(|((provider, service, day), amount)| ServiceDailyTotal {
                provider,
                service,
                day,
                amount,
            })
            .collect();
        totals.sort_by(|a, b| a.day.cmp(&b.day));

        totals
    })
}

/// Usage totals per billing period since an instant, as `(YYYY-MM, amount)`
/// ordered by period label — the 12-month Overview chart's series.
///
/// Grouped by `billing_period` rather than by charge-time month so the
/// buckets are the months the rest of the app reasons about.
pub fn monthly_usage(since: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                since: Some(since),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_billing_period(&rows)
    })
}

/// [`monthly_usage`] for a single account.
pub fn monthly_usage_of(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<(String, f64)>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_billing_period(&rows)
    })
}

// ==================== Windows ====================

/// Net total charged in a charge-time window `[since, until)`, across every
/// account and charge category — the rolling-range counterpart of
/// [`total_for_period`].
pub fn total_between(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<f64> {
    read(|all| {
        net_total(&selected(
            all,
            &Scope {
                since: Some(since),
                until: Some(until),
                ..Default::default()
            },
        ))
    })
}

/// Gross usage and credits of one period, in the reporting currency.
///
/// Net totals hide credit-covered spend: an account whose usage is fully
/// offset reads as $0 while it really consumed dollars. The UI keeps the net
/// total as its headline and shows these two buckets next to it. Tax and
/// Purchase rows are in neither bucket — they still count toward the net
/// total.
pub fn usage_and_credits(billing_period: &str) -> Result<(f64, f64)> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                billing_period: Some(billing_period),
                ..Default::default()
            },
        );

        usage_and_credits_of_rows(&rows)
    })
}

/// [`usage_and_credits`] over a charge-time window `[since, until)` instead
/// of one billing period — the rolling-range variant, where a calendar-month
/// key cannot express the bounds.
pub fn usage_and_credits_between(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<(f64, f64)> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                since: Some(since),
                until: Some(until),
                ..Default::default()
            },
        );

        usage_and_credits_of_rows(&rows)
    })
}

/// [`usage_and_credits_between`] for a single account.
pub fn usage_and_credits_of_between(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<(f64, f64)> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                until: Some(until),
                ..Default::default()
            },
        );

        usage_and_credits_of_rows(&rows)
    })
}

/// Usage of one period grouped by `(provider, service)`, largest first — like
/// [`provider_service_totals`], but Usage rows only, so rankings reflect what
/// was consumed rather than what credits happened to offset.
pub fn provider_service_usage(billing_period: &str) -> Result<Vec<(String, String, f64)>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                billing_period: Some(billing_period),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_provider_service(&rows)
    })
}

/// [`provider_service_usage`] over a charge-time window `[since, until)`
/// instead of one billing period — the rolling-range variant.
pub fn provider_service_usage_between(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<(String, String, f64)>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                since: Some(since),
                until: Some(until),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_provider_service(&rows)
    })
}

/// [`provider_service_usage_between`] narrowed to a single account, so the
/// service column drops out of the grouping.
pub fn service_usage_of_between(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<(String, f64)>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                until: Some(until),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_bucket(&rows, |row| service_of(row).to_string())
    })
}

// ==================== Tag breakdowns ====================

/// Charges of one period grouped by one tag's value, largest first.
///
/// `tags` is JSON object text; a charge with no tags column, an empty value,
/// or no `tag_key` in it counts toward `'Unallocated'` — the row the
/// attribution page hangs its explainer card on.
pub fn tag_breakdown(billing_period: &str, tag_key: &str) -> Result<Vec<(String, f64)>> {
    tag_totals(billing_period, tag_key, None, false)
}

/// [`tag_breakdown`] narrowed to one provider and service: which tag values
/// that spend drives. Same `'Unallocated'` bucket as the period-wide
/// breakdown.
pub fn service_tag_breakdown(
    billing_period: &str,
    provider: &str,
    service: &str,
    tag_key: &str,
) -> Result<Vec<(String, f64)>> {
    tag_totals(billing_period, tag_key, Some((provider, service)), false)
}

/// [`tag_breakdown`] against a caller's own handle, and optionally scoped to
/// one service, as [`crate::alerts`] reads it.
pub(crate) fn tag_breakdown_of(
    _conn: &Connection,
    billing_period: &str,
    tag_key: &str,
    scope: Option<(&str, &str)>,
) -> Result<Vec<(String, f64)>> {
    tag_totals(billing_period, tag_key, scope, false)
}

/// Usage of one period grouped by one tag's value, largest first — like
/// [`tag_breakdown`], same `'Unallocated'` bucketing, but Usage rows only, so
/// a credit does not shrink the bucket it would have offset.
pub fn tag_usage_breakdown(billing_period: &str, tag_key: &str) -> Result<Vec<(String, f64)>> {
    tag_totals(billing_period, tag_key, None, true)
}

/// [`tag_usage_breakdown`] narrowed to one provider and service: which tag
/// values that usage drives. Same `'Unallocated'` bucket as the period-wide
/// breakdown.
pub fn service_tag_usage_breakdown(
    billing_period: &str,
    provider: &str,
    service: &str,
    tag_key: &str,
) -> Result<Vec<(String, f64)>> {
    tag_totals(billing_period, tag_key, Some((provider, service)), true)
}

/// [`tag_usage_breakdown`] over a charge-time window `[since, until)` instead
/// of one billing period — the rolling-range variant. Same `'Unallocated'`
/// bucketing.
pub fn tag_usage_breakdown_between(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    tag_key: &str,
) -> Result<Vec<(String, f64)>> {
    tag_usage_breakdown_between_of(since, until, tag_key, None)
}

/// [`tag_usage_breakdown_between`] narrowed to one provider and service:
/// which tag values that usage drives. Same `'Unallocated'` bucket as the
/// window-wide breakdown.
pub fn service_tag_usage_breakdown_between(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    provider: &str,
    service: &str,
    tag_key: &str,
) -> Result<Vec<(String, f64)>> {
    tag_usage_breakdown_between_of(since, until, tag_key, Some((provider, service)))
}

fn tag_usage_breakdown_between_of(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    tag_key: &str,
    scope: Option<(&str, &str)>,
) -> Result<Vec<(String, f64)>> {
    let (provider, service) = scope.unzip();
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider,
                service,
                since: Some(since),
                until: Some(until),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_tag(&rows, tag_key)
    })
}

fn tag_totals(
    billing_period: &str,
    tag_key: &str,
    scope: Option<(&str, &str)>,
    usage_only: bool,
) -> Result<Vec<(String, f64)>> {
    let (provider, service) = scope.unzip();
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider,
                service,
                billing_period: Some(billing_period),
                usage_only,
                ..Default::default()
            },
        );

        totals_by_tag(&rows, tag_key)
    })
}

/// Usage of one period grouped by `(provider, service, tag_value)` in a
/// single pass, largest first — the attribution page's N+1 killer: filtering
/// the rows of one `(provider, service)` gives exactly what
/// [`service_tag_usage_breakdown`] returns for it, so a page that shows every
/// service no longer reads once per service.
pub fn tag_usage_breakdown_by_service(
    billing_period: &str,
    tag_key: &str,
) -> Result<Vec<ServiceTagUsage>> {
    read(|all| {
        let rows = selected(
            all,
            &Scope {
                billing_period: Some(billing_period),
                usage_only: true,
                ..Default::default()
            },
        );

        totals_by_bucket(&rows, |row| {
            (
                row.provider.clone(),
                service_of(row).to_string(),
                memory::tag_value(row.tags.as_deref(), tag_key),
            )
        })
        .into_iter()
        .map(|((provider, service, tag_value), amount)| ServiceTagUsage {
            provider,
            service,
            tag_value,
            amount,
        })
        .collect()
    })
}

// ==================== Untagged usage ====================

/// The largest charges of a period that carry no value for `tag_key`, biggest
/// first — the rows behind the Unallocated explainer card.
pub fn untagged_detail(
    billing_period: &str,
    tag_key: &str,
    limit: usize,
) -> Result<Vec<UntaggedCharge>> {
    read(|all| {
        let mut rows: Vec<UntaggedCharge> = all
            .iter()
            .filter(|row| row.billing_period == billing_period && untagged(row, tag_key))
            .filter_map(|row| {
                let amount = row.billed_cost_base?;
                (amount > 0.0).then(|| UntaggedCharge {
                    provider: row.provider.clone(),
                    service: row.service_name.clone(),
                    description: row.charge_description.clone(),
                    amount,
                })
            })
            .collect();
        rows.sort_by(|a, b| b.amount.total_cmp(&a.amount));
        rows.truncate(limit);

        rows
    })
}

/// Untagged usage of a period grouped by `(provider, service)`, largest first
/// — the roll-up behind [`untagged_detail`]'s per-charge list, so three small
/// charges of one service read as the one row the UI acts on.
pub fn untagged_usage_by_service(
    billing_period: &str,
    tag_key: &str,
    limit: usize,
) -> Result<Vec<UntaggedServiceUsage>> {
    read(|all| {
        // Grouped on the raw `service_name`, not the coalesced one, so a
        // charge with no service stays a distinct `(provider, None)` row.
        let mut grouped: BTreeMap<(String, Option<String>), f64> = BTreeMap::new();
        for row in all {
            if row.billing_period != billing_period
                || row.charge_category != ChargeCategory::Usage
                || !untagged(row, tag_key)
            {
                continue;
            }
            *grouped
                .entry((row.provider.clone(), row.service_name.clone()))
                .or_insert(0.0) += row.billed_cost_base.unwrap_or(0.0);
        }

        let mut rows: Vec<UntaggedServiceUsage> = grouped
            .into_iter()
            .map(|((provider, service), amount)| UntaggedServiceUsage {
                provider,
                service,
                amount,
            })
            .collect();
        rows.sort_by(|a, b| b.amount.total_cmp(&a.amount));
        rows.truncate(limit);

        rows
    })
}

/// How many charges could not be converted, because no rate covers their
/// currency. They are missing from every converted total.
pub fn unconverted_charges(billing_period: &str) -> Result<i64> {
    read(|all| {
        all.iter()
            .filter(|row| row.billing_period == billing_period)
            .filter(|row| row.billed_cost.is_some() && row.billed_cost_base.is_none())
            .count() as i64
    })
}

// ==================== Balances, ingests and the fetch count ====================

/// The newest balance snapshot for an account, if it reports one.
pub fn latest_balance(provider: &str, account_id: &str) -> Result<Option<Balance>> {
    balance_of(provider, account_id)
}

/// [`latest_balance`] against a caller's own handle, as [`crate::alerts`]
/// reads it.
pub(crate) fn latest_balance_of(
    _conn: &Connection,
    provider: &str,
    account_id: &str,
) -> Result<Option<Balance>> {
    balance_of(provider, account_id)
}

fn balance_of(provider: &str, account_id: &str) -> Result<Option<Balance>> {
    memory::with_store(|store| {
        Ok(store
            .balances
            .borrow()
            .iter()
            .filter(|snapshot| snapshot.provider == provider && snapshot.account_id == account_id)
            .max_by_key(|snapshot| snapshot.observed_at)
            .map(|snapshot| Balance {
                balance: snapshot.balance,
                granted_balance: snapshot.granted_balance,
                topped_up_balance: snapshot.topped_up_balance,
                // A balance is what is left in an account, not an amount
                // spent, so it is reported in the currency it was observed in.
                currency: snapshot.currency.clone(),
                observed_at: snapshot.observed_at,
            }))
    })
}

/// When a period was last ingested, if it ever was.
///
/// Only the batch whose rows are in the ledger counts: a superseded one says
/// the period was fetched once, not that what is stored is that fresh. Here
/// the store keeps only the batch whose rows are current, so a period's own
/// `completed_at` is that instant.
pub fn last_ingest(key: &PeriodKey) -> Result<Option<DateTime<Utc>>> {
    memory::with_store(|store| {
        Ok(store
            .periods
            .borrow()
            .iter()
            .filter(|period| period.key == *key)
            .map(|period| period.completed_at)
            .max())
    })
}

/// When each account's rows were last ingested, as `(provider, account_id,
/// completed_at)`.
pub fn last_ingests() -> Result<Vec<(String, String, DateTime<Utc>)>> {
    memory::with_store(|store| {
        let mut latest: BTreeMap<(String, String), DateTime<Utc>> = BTreeMap::new();
        for period in store.periods.borrow().iter() {
            let key = (period.key.provider.clone(), period.key.account_id.clone());
            let slot = latest.entry(key).or_insert(period.completed_at);
            *slot = (*slot).max(period.completed_at);
        }

        Ok(latest
            .into_iter()
            .map(|((provider, account_id), at)| (provider, account_id, at))
            .collect())
    })
}

/// Which channel the rows of a period arrived through.
///
/// Read before replaying normalization, so a month imported from the
/// provider's own bill export is not re-tagged as an API fetch.
pub fn channel_of(key: &PeriodKey) -> Result<Channel> {
    memory::with_store(|store| {
        Ok(store
            .periods
            .borrow()
            .iter()
            .find(|period| period.key == *key)
            .map(|period| period.channel)
            // A period that was never ingested was never imported either.
            .unwrap_or(Channel::Api))
    })
}

/// How many ingest batches arrived through a billing API this billing period,
/// as a proxy for what paid calls have cost so far.
///
/// Counted the way the desktop counts them out of `ingest_batch`: one batch
/// per `(provider, account, period)`, and only the ones that came in through
/// the API rather than a file. Nothing here is billed — a browser makes no
/// call — but the number the demo shows is then the number the desktop shows
/// for the same seeded ledger, which is the point of seeding it at all.
pub fn api_fetches_this_month() -> Result<i64> {
    let current = BillingPeriod::containing(Utc::now()).label();

    memory::with_store(|store| {
        let periods = store.periods.borrow();
        Ok(periods
            .iter()
            .filter(|period| period.key.billing_period == current && period.channel == Channel::Api)
            .count() as i64)
    })
}

/// Mean daily burn of a balance-reporting account over the last `days`, in
/// the balance's own currency.
///
/// Computed from the drops between consecutive balance observations — a rise
/// is a top-up, not consumption. `None` when the history holds fewer than two
/// observations, because then burn is unknowable.
pub fn balance_burn(provider: &str, account_id: &str, days: i64) -> Result<Option<f64>> {
    burn_of(provider, account_id, days)
}

/// [`balance_burn`] against a caller's own handle, as [`crate::alerts`] reads
/// it.
pub(crate) fn balance_burn_of(
    _conn: &Connection,
    provider: &str,
    account_id: &str,
    days: i64,
) -> Result<Option<f64>> {
    burn_of(provider, account_id, days)
}

fn burn_of(provider: &str, account_id: &str, days: i64) -> Result<Option<f64>> {
    memory::with_store(|store| {
        let snapshots = store.balances.borrow();
        let mut observations: Vec<_> = snapshots
            .iter()
            .filter(|snapshot| snapshot.provider == provider && snapshot.account_id == account_id)
            .map(|snapshot| {
                (
                    snapshot.observed_at,
                    snapshot.balance,
                    snapshot.currency.clone(),
                )
            })
            .collect();
        observations.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)));

        Ok(analytics::burn(&observations, days, Utc::now()))
    })
}

// ==================== Forecasting and period comparison ====================

/// The canonical MTD forecast for a billing period.
///
/// The daily rate is measured from a baseline of `max(period start, first
/// expense date in the period)`: an account that started reporting — or
/// landed its first charge — mid-month is not averaged over days it was not
/// running, so the cold-start ramp-up does not drag the rate down.
pub fn forecast_for_period(billing_period: &str) -> Result<PeriodForecast> {
    forecast_at(billing_period, Utc::now())
}

/// The run-rate forecast of one account for a billing period —
/// [`forecast_for_period`] at account granularity, against the caller's own
/// handle and clock.
pub(crate) fn account_forecast_of(
    _conn: &Connection,
    provider: &str,
    account_id: &str,
    period: BillingPeriod,
    now: DateTime<Utc>,
) -> Result<PeriodForecast> {
    let label = period.label();

    read(|all| {
        let rows = selected(
            all,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                billing_period: Some(&label),
                ..Default::default()
            },
        );

        run_rate(&rows, &period, now)
    })
}

fn forecast_at(billing_period: &str, now: DateTime<Utc>) -> Result<PeriodForecast> {
    let period = analytics::period_of(billing_period)?;

    read(|all| {
        let rows = selected(
            all,
            &Scope {
                billing_period: Some(billing_period),
                ..Default::default()
            },
        );

        run_rate(&rows, &period, now)
    })
}

/// The run-rate forecast over one period's charges.
///
/// Only the charges before `now` count, and their earliest day is what
/// [`analytics::run_rate`] measures the daily rate from.
fn run_rate(rows: &[&NormalizedRow], period: &BillingPeriod, now: DateTime<Utc>) -> PeriodForecast {
    let mut month_to_date = 0.0;
    let mut first_charge: Option<DateTime<Utc>> = None;
    for row in rows {
        if row.charge_period_start >= now {
            continue;
        }
        month_to_date += row.billed_cost_base.unwrap_or(0.0);
        first_charge = Some(match first_charge {
            Some(at) => at.min(row.charge_period_start),
            None => row.charge_period_start,
        });
    }

    analytics::run_rate(
        period,
        now,
        month_to_date,
        first_charge.map(|at| at.date_naive()),
    )
}

/// Current period vs. the immediately preceding one — billing periods here
/// being monthly, the previous month.
pub fn period_over_period(billing_period: &str) -> Result<PeriodOverPeriod> {
    let previous = analytics::previous_period(billing_period)?;

    read(|all| {
        // One pass over a window covering both periods, each bucket split by
        // which side it falls on.
        let mut current: BTreeMap<String, f64> = BTreeMap::new();
        let mut prior: BTreeMap<String, f64> = BTreeMap::new();
        for row in all {
            let side = if row.billing_period == billing_period {
                &mut current
            } else if row.billing_period == previous {
                &mut prior
            } else {
                continue;
            };
            *side.entry(service_of(row).to_string()).or_insert(0.0) +=
                row.billed_cost_base.unwrap_or(0.0);
        }

        analytics::compare_periods(current, prior)
    })
}

// ==================== Cost-change decomposition ====================

/// Decompose the current-vs-previous-period delta into its components, by
/// charge category and by service, and check that they add back up.
pub fn cost_change_decomposition(billing_period: &str) -> Result<CostChangeDecomposition> {
    let previous = analytics::previous_period(billing_period)?;

    let categories = two_period_buckets(billing_period, &previous, |row| {
        row.charge_category.as_str().to_string()
    })?;
    let services =
        two_period_buckets(billing_period, &previous, |row| service_of(row).to_string())?;

    Ok(analytics::decompose(
        billing_period,
        previous,
        categories,
        services,
    ))
}

/// Charges of two adjacent periods grouped by `bucket`, each bucket split
/// into `(current, previous)` amounts — one pass over the window covering
/// both periods, as in [`period_over_period`]. Both periods' net totals are
/// the sums of the two sides, which [`analytics::decompose`] takes from here.
fn two_period_buckets(
    billing_period: &str,
    previous: &str,
    bucket: impl Fn(&NormalizedRow) -> String,
) -> Result<TwoPeriodBuckets> {
    read(|all| {
        let mut buckets = TwoPeriodBuckets::new();
        for row in all {
            let current = if row.billing_period == billing_period {
                true
            } else if row.billing_period == previous {
                false
            } else {
                continue;
            };
            let entry = buckets.entry(bucket(row)).or_insert((0.0, 0.0));
            let side = if current { &mut entry.0 } else { &mut entry.1 };
            *side += row.billed_cost_base.unwrap_or(0.0);
        }

        buckets
    })
}

// ==================== Forecast confidence bands ====================

/// Confidence bands around the period forecast.
///
/// The daily sample is measured from the same baseline as
/// [`forecast_for_period`] — `max(period start, first charge day)` — and a day
/// inside it with no charge counts as zero, so `daily_mean` agrees with the
/// forecast's daily rate. With fewer than two sampled days a standard
/// deviation does not exist, and both bands collapse onto `expected`.
pub fn forecast_bands_for_period(billing_period: &str) -> Result<ForecastBands> {
    forecast_bands_at(billing_period, Utc::now())
}

fn forecast_bands_at(billing_period: &str, now: DateTime<Utc>) -> Result<ForecastBands> {
    let period = analytics::period_of(billing_period)?;
    let forecast = forecast_at(billing_period, now)?;

    // The same charge set the forecast totals: this period, before `now`.
    let daily = read(|all| {
        totals_by_day(&selected(
            all,
            &Scope {
                billing_period: Some(billing_period),
                until: Some(now),
                ..Default::default()
            },
        ))
    })?;

    analytics::forecast_bands(&period, now, forecast, &daily)
}

// ==================== Trailing-average overlay ====================

/// A "typical day" series to overlay on the current month's daily line — the
/// Wealthfolio benchmark-comparison pattern.
///
/// For each of the last `day_count` days, oldest first, as `(YYYY-MM-DD,
/// amount)`: the average daily **usage** of the `months` complete calendar
/// months preceding the day's own month, spread over their calendar days — a
/// day with no charges counts as zero. The window ends where the day's month
/// begins, so a month is never compared against itself; every day of one
/// month therefore reads the same value and the series is the flat benchmark
/// line the actual daily line is drawn against. Usage rows only, so a landed
/// credit does not dip what a typical day costs.
///
/// Degenerate inputs (`day_count` or `months` below 1) yield an empty series.
pub fn trailing_daily_average(day_count: i64, months: i64) -> Result<Vec<(String, f64)>> {
    trailing_daily_average_at(day_count, months, Utc::now())
}

fn trailing_daily_average_at(
    day_count: i64,
    months: i64,
    now: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    let Some((window_start, window_end)) = analytics::trailing_window(day_count, months, now)
    else {
        return Ok(Vec::new());
    };

    let monthly: BTreeMap<String, f64> = read(|all| {
        let mut totals: BTreeMap<String, f64> = BTreeMap::new();
        for row in all {
            if row.charge_category != ChargeCategory::Usage
                || row.charge_period_start < analytics::midnight(window_start)
                || row.charge_period_start >= analytics::midnight(window_end)
            {
                continue;
            }
            // Keyed by charge time, not by `billing_period`: the window is
            // bounded by charge-time instants, so the keys the two are read
            // back with have to be measured the same way.
            *totals
                .entry(row.charge_period_start.format("%Y-%m").to_string())
                .or_insert(0.0) += row.billed_cost_base.unwrap_or(0.0);
        }

        totals
    })?;

    Ok(analytics::trailing_average(
        day_count, months, now, &monthly,
    ))
}

// ==================== Data-quality summary ====================

/// The data-quality findings for a period: unconverted charges, untagged
/// usage for `tag_key`, and services whose usage carries no region. A clean
/// period yields an empty list.
///
/// The desktop raises one more kind here — unreconciled bill adjustments —
/// which this target cannot see: the finding is keyed on the charge's
/// description, a column the web build's reading view does not carry, and the
/// only path that writes such a row is the desktop's bill-file importer. No
/// row the browser can be given would raise it.
pub fn data_quality_issues(billing_period: &str, tag_key: &str) -> Result<Vec<DataQualityIssue>> {
    read(|all| {
        let rows: Vec<&NormalizedRow> = all
            .iter()
            .filter(|row| row.billing_period == billing_period)
            .collect();

        // Charges no rate covers, against the period's row count.
        let unconverted = rows
            .iter()
            .filter(|row| row.billed_cost.is_some() && row.billed_cost_base.is_none())
            .count() as i64;

        // Period usage is the denominator of both share checks.
        let (usage, _) = usage_and_credits_of_rows(&rows);

        // Usage with no value for the tag the attribution page groups by.
        let untagged_rows: Vec<&NormalizedRow> = rows
            .iter()
            .copied()
            .filter(|row| row.charge_category == ChargeCategory::Usage && untagged(row, tag_key))
            .collect();
        let untagged_amount: f64 = untagged_rows
            .iter()
            .filter_map(|row| row.billed_cost_base)
            .sum();

        // Region-less usage, per service.
        let mut by_service: BTreeMap<String, (f64, i64)> = BTreeMap::new();
        for row in &rows {
            if row.charge_category != ChargeCategory::Usage || row.region_id.is_some() {
                continue;
            }
            let entry = by_service.entry(service_of(row).to_string()).or_default();
            entry.0 += row.billed_cost_base.unwrap_or(0.0);
            entry.1 += 1;
        }
        let mut regionless: Vec<(String, f64, i64)> = by_service
            .into_iter()
            .filter(|(_, (amount, _))| *amount > 0.0)
            .map(|(service, (amount, charges))| (service, amount, charges))
            .collect();
        regionless.sort_by(|a, b| b.1.total_cmp(&a.1));

        analytics::data_quality(QualityCounts {
            tag_key,
            rows: rows.len() as i64,
            unconverted,
            usage,
            untagged: untagged_amount,
            untagged_count: untagged_rows.len() as i64,
            regionless,
            // The desktop raises one more kind here — unreconciled bill
            // adjustments — which this target cannot see: the finding is keyed
            // on the charge's description, a column the web build's reading
            // view does not carry, and the only path that writes such a row is
            // the desktop's bill-file importer.
            unreconciled: None,
        })
    })
}

// ==================== Ad-hoc queries ====================

/// The Query page's SQL console.
///
/// There is no engine on this target: the ledger is vectors, and running a
/// statement against it would mean shipping a database to the browser. The
/// call shape is kept so the page needs no second code path, and the error
/// says what is missing rather than reporting an empty result as an answer.
pub fn run_adhoc(_sql: &str) -> Result<AdhocResult> {
    Err(anyhow!(
        "The web demo has no SQL engine: the ledger is held in memory and read by the app, not \
         queried"
    ))
}
