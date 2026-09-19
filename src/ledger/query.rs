//! Reading the ledger.
//!
//! Everything the UI shows comes through [`schema::NORMALIZED_VIEW`], so
//! amounts arrive already expressed in the reporting currency. Nothing in
//! here adds up two currencies.

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use duckdb::types::{TimeUnit, Value, ValueRef};
use duckdb::{params, AccessMode, Config, Connection};

use super::schema::{NORMALIZED_VIEW, TIMESTAMP_FORMAT};
use super::{with_connection_ref, Channel, PeriodKey};
use crate::analytics::{self, QualityCounts, TwoPeriodBuckets};
use crate::model::BillingPeriod;
pub use crate::model::{
    AdhocResult, Balance, BreakdownDim, CategoryDelta, CostChangeDecomposition, DailyTotal,
    DataQualityIssue, DataQualityKind, ForecastBands, IssueSeverity, MovementKind, PeriodForecast,
    PeriodOverPeriod, ServiceDailyTotal, ServiceMovement, ServiceTagUsage, TopResource,
    UntaggedCharge, UntaggedServiceUsage,
};

/// Total charged in one billing period, in the reporting currency.
pub fn period_total(key: &PeriodKey) -> Result<f64> {
    with_connection_ref(|conn| period_total_of(conn, key))
}

/// Total charged across every account in a billing period.
///
/// This is the cross-cloud, cross-currency number: one query, one currency
/// out, no summing of amounts that were never comparable.
pub fn total_for_period(billing_period: &str) -> Result<f64> {
    with_connection_ref(|conn| total_for_period_of(conn, billing_period))
}

/// Charges of one period grouped by service, largest first.
pub fn service_breakdown(key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| service_breakdown_of(conn, key))
}

/// Daily charge totals for an account since an instant, oldest first.
pub fn daily_totals(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    with_connection_ref(|conn| daily_totals_of(conn, provider, account_id, since))
}

/// The newest balance snapshot for an account, if it reports one.
pub fn latest_balance(provider: &str, account_id: &str) -> Result<Option<Balance>> {
    with_connection_ref(|conn| latest_balance_of(conn, provider, account_id))
}

/// When a period was last ingested, if it ever was.
///
/// Only the batch whose rows are in `fct_charge` counts: a superseded one
/// says the period was fetched once, not that what is stored is that fresh.
pub fn last_ingest(key: &PeriodKey) -> Result<Option<DateTime<Utc>>> {
    with_connection_ref(|conn| last_ingest_of(conn, key))
}

/// Which channel the rows of a period arrived through.
///
/// Read before replaying normalization, so a month imported from the
/// provider's own bill export is not re-tagged as an API fetch.
pub fn channel_of(key: &PeriodKey) -> Result<Channel> {
    with_connection_ref(|conn| {
        // Only one batch per period is 'complete' — `write_period`
        // supersedes the rest — so this is the batch whose rows are in
        // `fct_charge`.
        let mut stmt = conn.prepare(
            "SELECT channel FROM ingest_batch
             WHERE provider = ? AND account_id = ? AND billing_period = ? AND status = 'complete'
             LIMIT 1",
        )?;

        let mut rows = stmt.query(params![key.provider, key.account_id, key.billing_period])?;

        match rows.next()? {
            Some(row) => Ok(Channel::from_stored(
                row.get::<_, Option<String>>(0)?.as_deref(),
            )),
            // A period that was never ingested was never imported either.
            None => Ok(Channel::Api),
        }
    })
}

/// Daily charge totals across every provider and account since an instant,
/// oldest first.
pub fn daily_totals_all(since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    with_connection_ref(|conn| daily_totals_all_of(conn, since))
}

fn daily_totals_all_of(conn: &Connection, since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    sum_by_day(
        conn,
        &Scope {
            since: Some(since),
            ..Default::default()
        },
    )
}

/// Daily charge totals per `(provider, service)` since an instant, oldest
/// first — the input to cost-anomaly detection.
pub fn daily_totals_by_service(since: DateTime<Utc>) -> Result<Vec<ServiceDailyTotal>> {
    with_connection_ref(|conn| daily_totals_by_service_of(conn, since))
}

pub(crate) fn daily_totals_by_service_of(
    conn: &Connection,
    since: DateTime<Utc>,
) -> Result<Vec<ServiceDailyTotal>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, coalesce(service_name, 'Other') AS service,
                strftime(charge_period_start, '%Y-%m-%d') AS day,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE charge_period_start >= CAST(? AS TIMESTAMP)
         GROUP BY provider, service, day
         ORDER BY day"
    ))?;

    let rows = stmt
        .query_map(params![since.format(TIMESTAMP_FORMAT).to_string()], |row| {
            Ok(ServiceDailyTotal {
                provider: row.get(0)?,
                service: row.get(1)?,
                day: row.get(2)?,
                amount: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// [`daily_totals_by_service_of`] for a single account.
///
/// The shared query groups across accounts and exposes no account column, so
/// an account-scoped anomaly rule needs its own read of the same view.
pub(crate) fn daily_totals_for_account_of(
    conn: &Connection,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<ServiceDailyTotal>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, coalesce(service_name, 'Other') AS service,
                strftime(charge_period_start, '%Y-%m-%d') AS day,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE account_id = ? AND charge_period_start >= CAST(? AS TIMESTAMP)
         GROUP BY provider, service, day
         ORDER BY day"
    ))?;

    let rows = stmt
        .query_map(
            params![account_id, since.format(TIMESTAMP_FORMAT).to_string()],
            |row| {
                Ok(ServiceDailyTotal {
                    provider: row.get(0)?,
                    service: row.get(1)?,
                    day: row.get(2)?,
                    amount: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// The date of a ledger-rendered timestamp (`YYYY-MM-DD HH:MM:SS`, UTC).
///
/// [`super::parse_timestamp`] keeps the whole instant; the forecast compares
/// dates, so the date is all this keeps.
fn ledger_date_of(stamp: &str) -> Result<NaiveDate> {
    let stamp = stamp.split('.').next().unwrap_or(stamp);
    Ok(chrono::NaiveDateTime::parse_from_str(stamp, TIMESTAMP_FORMAT)?.date())
}

/// The run-rate forecast of one account for a billing period —
/// [`forecast_for_period_of`] at account granularity, against the caller's
/// connection and clock. (The public one reads the global connection at the
/// wall clock; neither takes an account.)
pub(crate) fn account_forecast_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
    period: BillingPeriod,
    now: DateTime<Utc>,
) -> Result<PeriodForecast> {
    let (spent, first_charge): (Option<f64>, Option<String>) = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base), CAST(min(charge_period_start) AS VARCHAR)
             FROM {NORMALIZED_VIEW}
             WHERE provider = ? AND account_id = ? AND billing_period = ?
               AND charge_period_start < CAST(? AS TIMESTAMP)"
        ),
        params![
            provider,
            account_id,
            period.label(),
            now.format(TIMESTAMP_FORMAT).to_string()
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let first_charge = first_charge
        .map(|stamp| ledger_date_of(&stamp))
        .transpose()?;

    Ok(analytics::run_rate(
        &period,
        now,
        spent.unwrap_or(0.0),
        first_charge,
    ))
}

/// Charges of one period grouped by `(provider, service)`, largest first.
pub fn provider_service_totals(billing_period: &str) -> Result<Vec<(String, String, f64)>> {
    with_connection_ref(|conn| provider_service_totals_of(conn, billing_period))
}

fn provider_service_totals_of(
    conn: &Connection,
    billing_period: &str,
) -> Result<Vec<(String, String, f64)>> {
    sum_by_provider_service(
        conn,
        &Scope {
            billing_period: Some(billing_period),
            ..Default::default()
        },
    )
}

/// Charges of one period grouped by one tag's value, largest first.
///
/// `tags` is JSON object text; a charge with no tags column, an empty
/// value, or no `tag_key` in it counts toward `'Unallocated'` — that is
/// the row the attribution page hangs its explainer card on.
pub fn tag_breakdown(billing_period: &str, tag_key: &str) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| tag_breakdown_of(conn, billing_period, tag_key, None))
}

/// [`tag_breakdown`] narrowed to one provider and service: which tag
/// values that spend drives. Same `'Unallocated'` bucket as the period-wide
/// breakdown.
pub fn service_tag_breakdown(
    billing_period: &str,
    provider: &str,
    service: &str,
    tag_key: &str,
) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| {
        tag_breakdown_of(conn, billing_period, tag_key, Some((provider, service)))
    })
}

pub(crate) fn tag_breakdown_of(
    conn: &Connection,
    billing_period: &str,
    tag_key: &str,
    scope: Option<(&str, &str)>,
) -> Result<Vec<(String, f64)>> {
    let (provider, service) = scope.unzip();
    tag_breakdown_query(
        conn,
        tag_key,
        &Scope {
            billing_period: Some(billing_period),
            provider,
            service,
            ..Default::default()
        },
    )
}

/// How many charges could not be converted, because no rate covers their
/// currency. They are missing from every converted total.
pub fn unconverted_charges(billing_period: &str) -> Result<i64> {
    with_connection_ref(|conn| unconverted_charges_of(conn, billing_period))
}

/// The largest charges of a period that carry no value for `tag_key`,
/// biggest first — the rows behind the Unallocated explainer card.
pub fn untagged_detail(
    billing_period: &str,
    tag_key: &str,
    limit: usize,
) -> Result<Vec<UntaggedCharge>> {
    with_connection_ref(|conn| untagged_detail_of(conn, billing_period, tag_key, limit))
}

/// A caller that wants "every row" passes `usize::MAX`, which wraps to -1
/// as an i64 — and DuckDB refuses a negative LIMIT outright.
fn bounded_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

// ==================== Shared query scaffolding ====================
//
// Every aggregate read in this file filters on the same axes — account,
// charge-time window, charge category — and groups by a handful of keys.
// `Scope` is those axes and the `sum_by_*` functions below are the
// groupings; the metric variants (period-keyed, window-bounded,
// per-account, usage-only) are then one line each, and a new variant
// cannot drift from the SQL of its siblings.

/// The axes an aggregate read filters on. `None` means "unfiltered", so a
/// cross-account read is the same query as its per-account counterpart
/// with a wider scope.
#[derive(Debug, Default, Clone)]
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
    /// The WHERE fragment, with `?` placeholders in [`Scope::params`] order.
    fn where_sql(&self) -> String {
        let mut clauses = vec!["TRUE".to_string()];
        if self.provider.is_some() {
            clauses.push("provider = ?".to_string());
        }
        if self.account_id.is_some() {
            clauses.push("account_id = ?".to_string());
        }
        if self.billing_period.is_some() {
            clauses.push("billing_period = ?".to_string());
        }
        if self.service.is_some() {
            clauses.push("coalesce(service_name, 'Other') = ?".to_string());
        }
        if self.since.is_some() {
            clauses.push("charge_period_start >= CAST(? AS TIMESTAMP)".to_string());
        }
        if self.until.is_some() {
            clauses.push("charge_period_start < CAST(? AS TIMESTAMP)".to_string());
        }
        if self.usage_only {
            clauses.push("charge_category = 'Usage'".to_string());
        }
        clauses.join(" AND ")
    }

    /// Bind values for [`Scope::where_sql`], in clause order.
    fn params(&self) -> Vec<String> {
        let mut bound = Vec::new();
        if let Some(provider) = self.provider {
            bound.push(provider.to_string());
        }
        if let Some(account_id) = self.account_id {
            bound.push(account_id.to_string());
        }
        if let Some(billing_period) = self.billing_period {
            bound.push(billing_period.to_string());
        }
        if let Some(service) = self.service {
            bound.push(service.to_string());
        }
        for instant in [self.since, self.until].into_iter().flatten() {
            bound.push(instant.format(TIMESTAMP_FORMAT).to_string());
        }
        bound
    }
}

/// Net total over a scope, across every charge category — 0.0 for an
/// empty one.
fn sum_total(conn: &Connection, scope: &Scope) -> Result<f64> {
    let total: Option<f64> = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base) FROM {NORMALIZED_VIEW} WHERE {}",
            scope.where_sql()
        ),
        duckdb::params_from_iter(scope.params()),
        |row| row.get(0),
    )?;

    Ok(total.unwrap_or(0.0))
}

/// Daily totals over a scope, oldest first.
///
/// Served from the day-grain rollup when that answers the scope exactly —
/// net semantics over a day-aligned window — and the rollup is current;
/// anything else reads the view, as before. The rollup has no
/// `charge_category` column, so a usage-only scope can never come from it.
fn sum_by_day(conn: &Connection, scope: &Scope) -> Result<Vec<DailyTotal>> {
    if let Some(rows) = sum_by_day_rolled_up(conn, scope)? {
        return Ok(rows);
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT strftime(charge_period_start, '%Y-%m-%d') AS day, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE {}
         GROUP BY day
         ORDER BY day",
        scope.where_sql()
    ))?;

    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(day, amount)| (day, amount.unwrap_or(0.0)))
        .collect())
}

/// The rollup-backed [`sum_by_day`]: `None` when the view path must answer
/// instead — a usage-only scope, a period or service filter, a window edge
/// inside a day (the day grain cannot exclude part of it), or a stale
/// rollup.
fn sum_by_day_rolled_up(conn: &Connection, scope: &Scope) -> Result<Option<Vec<DailyTotal>>> {
    let day_aligned = |instant: DateTime<Utc>| instant.time() == chrono::NaiveTime::MIN;
    if scope.usage_only
        || scope.billing_period.is_some()
        || scope.service.is_some()
        || scope.until.is_some()
        || scope.since.is_some_and(|since| !day_aligned(since))
    {
        return Ok(None);
    }
    if !rollup_is_current(conn)? {
        return Ok(None);
    }

    let mut clauses = vec!["TRUE".to_string()];
    let mut bound: Vec<String> = Vec::new();
    if let Some(provider) = scope.provider {
        clauses.push("provider = ?".to_string());
        bound.push(provider.to_string());
    }
    if let Some(account_id) = scope.account_id {
        clauses.push("account_id = ?".to_string());
        bound.push(account_id.to_string());
    }
    if let Some(since) = scope.since {
        clauses.push("day >= CAST(? AS DATE)".to_string());
        bound.push(since.format(TIMESTAMP_FORMAT).to_string());
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT CAST(day AS VARCHAR) AS day, sum(billed_cost_base) AS amount
         FROM daily_cost_rollup
         WHERE {}
         GROUP BY day
         ORDER BY day",
        clauses.join(" AND ")
    ))?;

    let rows = stmt
        .query_map(duckdb::params_from_iter(bound), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Some(
        rows.into_iter()
            .map(|(day, amount)| (day, amount.unwrap_or(0.0)))
            .collect(),
    ))
}

/// [`super::rollup::is_current_of`] against the currency the reading view
/// converts to, probed from the view itself. An empty ledger has no row to
/// probe — and nothing for either path to return.
fn rollup_is_current(conn: &Connection) -> Result<bool> {
    let mut stmt = conn.prepare(&format!(
        "SELECT reporting_currency FROM {NORMALIZED_VIEW} LIMIT 1"
    ))?;
    let currency: Option<String> = stmt.query_map([], |row| row.get(0))?.next().transpose()?;
    match currency {
        Some(currency) => super::rollup::is_current_of(conn, &currency),
        None => Ok(true),
    }
}

/// Totals per billing period over a scope, ordered by period label — the
/// 12-month Overview chart's series shape.
fn sum_by_period(conn: &Connection, scope: &Scope) -> Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT billing_period, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE {}
         GROUP BY billing_period
         ORDER BY billing_period",
        scope.where_sql()
    ))?;

    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(period, amount)| (period, amount.unwrap_or(0.0)))
        .collect())
}

/// Charges over a scope grouped by one bucket expression, largest first;
/// a net-negative bucket is dropped.
fn sum_by_bucket(conn: &Connection, scope: &Scope, bucket: &str) -> Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {bucket} AS bucket, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE {}
         GROUP BY bucket
         HAVING amount > 0
         ORDER BY amount DESC",
        scope.where_sql()
    ))?;

    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// Charges over a scope grouped by `(provider, service)`, largest first; a
/// net-negative group is dropped.
fn sum_by_provider_service(conn: &Connection, scope: &Scope) -> Result<Vec<(String, String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, coalesce(service_name, 'Other') AS service,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE {}
         GROUP BY provider, service
         HAVING amount > 0
         ORDER BY amount DESC",
        scope.where_sql()
    ))?;

    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// Gross usage and credits over a scope, in the reporting currency.
///
/// Net totals hide credit-covered spend: an account whose usage is fully
/// offset reads as $0 while it really consumed dollars. Tax and Purchase
/// rows are in neither bucket — they still count toward the net total.
fn sum_usage_and_credits(conn: &Connection, scope: &Scope) -> Result<(f64, f64)> {
    let (usage, credits): (Option<f64>, Option<f64>) = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base) FILTER (WHERE charge_category = 'Usage'),
                    sum(billed_cost_base) FILTER (WHERE charge_category IN ('Credit', 'Adjustment'))
             FROM {NORMALIZED_VIEW}
             WHERE {}",
            scope.where_sql()
        ),
        duckdb::params_from_iter(scope.params()),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    Ok((usage.unwrap_or(0.0), credits.unwrap_or(0.0)))
}

/// The one tag-breakdown query behind every tag variant: charges over
/// `scope` grouped by the value of `tag_key`, largest first. A charge with
/// no tags column, an empty value, or no `tag_key` in it counts toward
/// `'Unallocated'` — the row the attribution page hangs its explainer card
/// on. `tag_key` binds first: it appears in the SELECT list, before the
/// WHERE clause.
fn tag_breakdown_query(
    conn: &Connection,
    tag_key: &str,
    scope: &Scope,
) -> Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT coalesce(nullif(json_extract_string(tags, ?), ''), 'Unallocated') AS tag_value,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE {}
         GROUP BY tag_value
         HAVING amount > 0
         ORDER BY amount DESC",
        scope.where_sql()
    ))?;

    let mut bound: Vec<String> = vec![tag_key.to_string()];
    bound.extend(scope.params());

    let rows = stmt
        .query_map(duckdb::params_from_iter(bound.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

fn untagged_detail_of(
    conn: &Connection,
    billing_period: &str,
    tag_key: &str,
    limit: usize,
) -> Result<Vec<UntaggedCharge>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, service_name, charge_description, billed_cost_base
         FROM {NORMALIZED_VIEW}
         WHERE billing_period = ?
           AND coalesce(nullif(json_extract_string(tags, ?), ''), '') = ''
           AND billed_cost_base > 0
         ORDER BY billed_cost_base DESC
         LIMIT ?"
    ))?;

    let rows = stmt
        .query_map(
            params![billing_period, tag_key, bounded_limit(limit)],
            |row| {
                Ok(UntaggedCharge {
                    provider: row.get(0)?,
                    service: row.get(1)?,
                    description: row.get(2)?,
                    amount: row.get(3)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// Gross usage and credits of one period, in the reporting currency.
///
/// Net totals hide credit-covered spend: an account whose usage is fully
/// offset reads as $0 while it really consumed dollars. The UI keeps the
/// net total as its headline and shows these two buckets next to it. Tax
/// and Purchase rows are in neither bucket — they still count toward the
/// net total.
pub fn usage_and_credits(billing_period: &str) -> Result<(f64, f64)> {
    with_connection_ref(|conn| usage_and_credits_of(conn, billing_period))
}

fn usage_and_credits_of(conn: &Connection, billing_period: &str) -> Result<(f64, f64)> {
    sum_usage_and_credits(
        conn,
        &Scope {
            billing_period: Some(billing_period),
            ..Default::default()
        },
    )
}

/// [`usage_and_credits`] over a charge-time window `[since, until)`
/// instead of one billing period — the rolling-range variant, where a
/// calendar-month key cannot express the bounds.
pub fn usage_and_credits_between(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<(f64, f64)> {
    with_connection_ref(|conn| usage_and_credits_between_of(conn, since, until))
}

fn usage_and_credits_between_of(
    conn: &Connection,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<(f64, f64)> {
    sum_usage_and_credits(
        conn,
        &Scope {
            since: Some(since),
            until: Some(until),
            ..Default::default()
        },
    )
}

/// Net total charged in a charge-time window `[since, until)`, across
/// every account and charge category — the rolling-range counterpart of
/// [`total_for_period`].
pub fn total_between(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<f64> {
    with_connection_ref(|conn| total_between_of(conn, since, until))
}

fn total_between_of(conn: &Connection, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<f64> {
    sum_total(
        conn,
        &Scope {
            since: Some(since),
            until: Some(until),
            ..Default::default()
        },
    )
}

/// Usage totals per billing period since an instant, as `(YYYY-MM,
/// amount)` ordered by period label — the 12-month Overview chart's
/// series. Grouped by `billing_period` rather than by charge-time month
/// so the buckets are the same months the rest of the app reasons about.
pub fn monthly_usage(since: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| monthly_usage_all_of(conn, since))
}

fn monthly_usage_all_of(conn: &Connection, since: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
    sum_by_period(
        conn,
        &Scope {
            since: Some(since),
            usage_only: true,
            ..Default::default()
        },
    )
}

/// Daily usage totals across every provider and account since an instant,
/// oldest first — like [`daily_totals_all`], but Usage rows only, so a
/// credit landing on one day does not dip the series below what was
/// actually consumed.
pub fn daily_usage_all(since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    with_connection_ref(|conn| daily_usage_all_of(conn, since))
}

fn daily_usage_all_of(conn: &Connection, since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    sum_by_day(
        conn,
        &Scope {
            since: Some(since),
            usage_only: true,
            ..Default::default()
        },
    )
}

/// Usage of one period grouped by `(provider, service)`, largest first —
/// like [`provider_service_totals`], but Usage rows only, so rankings
/// reflect what was consumed rather than what credits happened to offset.
pub fn provider_service_usage(billing_period: &str) -> Result<Vec<(String, String, f64)>> {
    with_connection_ref(|conn| provider_service_usage_of(conn, billing_period))
}

fn provider_service_usage_of(
    conn: &Connection,
    billing_period: &str,
) -> Result<Vec<(String, String, f64)>> {
    sum_by_provider_service(
        conn,
        &Scope {
            billing_period: Some(billing_period),
            usage_only: true,
            ..Default::default()
        },
    )
}

/// [`provider_service_usage`] over a charge-time window `[since, until)`
/// instead of one billing period — the rolling-range variant.
pub fn provider_service_usage_between(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<(String, String, f64)>> {
    with_connection_ref(|conn| provider_service_usage_between_of(conn, since, until))
}

fn provider_service_usage_between_of(
    conn: &Connection,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<(String, String, f64)>> {
    sum_by_provider_service(
        conn,
        &Scope {
            since: Some(since),
            until: Some(until),
            usage_only: true,
            ..Default::default()
        },
    )
}

/// Usage of one period grouped by one tag's value, largest first — like
/// [`tag_breakdown`], same `'Unallocated'` bucketing, but Usage rows
/// only, so a credit does not shrink the bucket it would have offset.
pub fn tag_usage_breakdown(billing_period: &str, tag_key: &str) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| tag_usage_breakdown_of(conn, billing_period, tag_key, None))
}

/// [`tag_usage_breakdown`] narrowed to one provider and service: which
/// tag values that usage drives. Same `'Unallocated'` bucket as the
/// period-wide breakdown.
pub fn service_tag_usage_breakdown(
    billing_period: &str,
    provider: &str,
    service: &str,
    tag_key: &str,
) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| {
        tag_usage_breakdown_of(conn, billing_period, tag_key, Some((provider, service)))
    })
}

fn tag_usage_breakdown_of(
    conn: &Connection,
    billing_period: &str,
    tag_key: &str,
    scope: Option<(&str, &str)>,
) -> Result<Vec<(String, f64)>> {
    let (provider, service) = scope.unzip();
    tag_breakdown_query(
        conn,
        tag_key,
        &Scope {
            billing_period: Some(billing_period),
            provider,
            service,
            usage_only: true,
            ..Default::default()
        },
    )
}

/// [`tag_usage_breakdown`] over a charge-time window `[since, until)`
/// instead of one billing period — the rolling-range variant. Same
/// `'Unallocated'` bucketing.
pub fn tag_usage_breakdown_between(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    tag_key: &str,
) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| tag_usage_breakdown_between_of(conn, since, until, tag_key, None))
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
    with_connection_ref(|conn| {
        tag_usage_breakdown_between_of(conn, since, until, tag_key, Some((provider, service)))
    })
}

fn tag_usage_breakdown_between_of(
    conn: &Connection,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    tag_key: &str,
    scope: Option<(&str, &str)>,
) -> Result<Vec<(String, f64)>> {
    let (provider, service) = scope.unzip();
    tag_breakdown_query(
        conn,
        tag_key,
        &Scope {
            provider,
            service,
            since: Some(since),
            until: Some(until),
            usage_only: true,
            ..Default::default()
        },
    )
}

/// Untagged usage of a period grouped by `(provider, service)`, largest
/// first — the roll-up behind [`untagged_detail`]'s per-charge list, so
/// three small charges of one service read as the one row the UI acts on.
pub fn untagged_usage_by_service(
    billing_period: &str,
    tag_key: &str,
    limit: usize,
) -> Result<Vec<UntaggedServiceUsage>> {
    with_connection_ref(|conn| untagged_usage_by_service_of(conn, billing_period, tag_key, limit))
}

fn untagged_usage_by_service_of(
    conn: &Connection,
    billing_period: &str,
    tag_key: &str,
    limit: usize,
) -> Result<Vec<UntaggedServiceUsage>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, service_name, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE billing_period = ?
           AND charge_category = 'Usage'
           AND coalesce(nullif(json_extract_string(tags, ?), ''), '') = ''
         GROUP BY provider, service_name
         ORDER BY amount DESC
         LIMIT ?"
    ))?;

    let rows = stmt
        .query_map(
            params![billing_period, tag_key, bounded_limit(limit)],
            |row| {
                Ok(UntaggedServiceUsage {
                    provider: row.get(0)?,
                    service: row.get(1)?,
                    amount: row.get(2)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// When each account's rows were last ingested, as
/// `(provider, account_id, completed_at)`. Only the batch whose rows are
/// in `fct_charge` counts, as in [`last_ingest`].
pub fn last_ingests() -> Result<Vec<(String, String, DateTime<Utc>)>> {
    with_connection_ref(last_ingests_of)
}

fn last_ingests_of(conn: &Connection) -> Result<Vec<(String, String, DateTime<Utc>)>> {
    let mut stmt = conn.prepare(
        "SELECT provider, account_id, CAST(max(completed_at) AS VARCHAR)
         FROM ingest_batch
         WHERE status = 'complete'
         GROUP BY provider, account_id",
    )?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    rows.into_iter()
        .map(|(provider, account_id, stamp)| {
            let completed_at = stamp
                .map(|stamp| super::parse_timestamp(&stamp))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("A 'complete' ingest batch has no completed_at"))?;
            Ok((provider, account_id, completed_at))
        })
        .collect()
}

/// How many ingest batches arrived through a billing API this billing
/// period, as a proxy for what paid calls (Cost Explorer bills per
/// request) have cost so far. A superseded batch was still a paid call,
/// so it counts.
pub fn api_fetches_this_month() -> Result<i64> {
    with_connection_ref(api_fetches_this_month_of)
}

fn api_fetches_this_month_of(conn: &Connection) -> Result<i64> {
    let period = crate::cloud::BillingPeriod::containing(Utc::now()).label();
    conn.query_row(
        "SELECT count(*) FROM ingest_batch
         WHERE billing_period = ? AND coalesce(channel, 'api') = 'api'",
        params![period],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Mean daily burn of a balance-reporting account over the last `days`,
/// in the balance's own currency.
///
/// Computed from the drops between consecutive balance observations — a
/// rise is a top-up, not consumption. `None` when the history holds fewer
/// than two observations, because then burn is unknowable.
pub fn balance_burn(provider: &str, account_id: &str, days: i64) -> Result<Option<f64>> {
    with_connection_ref(|conn| balance_burn_of(conn, provider, account_id, days))
}

pub(crate) fn balance_burn_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
    days: i64,
) -> Result<Option<f64>> {
    let mut stmt = conn.prepare(
        "SELECT CAST(observed_at AS VARCHAR), balance, currency
         FROM fct_balance_snapshot
         WHERE provider = ? AND account_id = ?
         ORDER BY currency, observed_at",
    )?;

    let observations = stmt
        .query_map(params![provider, account_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(stamp, balance, currency)| Ok((super::parse_timestamp(&stamp)?, balance, currency)))
        .collect::<Result<Vec<_>>>()?;

    Ok(analytics::burn(&observations, days, Utc::now()))
}

fn period_total_of(conn: &Connection, key: &PeriodKey) -> Result<f64> {
    sum_total(
        conn,
        &Scope {
            provider: Some(&key.provider),
            account_id: Some(&key.account_id),
            billing_period: Some(&key.billing_period),
            ..Default::default()
        },
    )
}

pub(crate) fn total_for_period_of(conn: &Connection, billing_period: &str) -> Result<f64> {
    sum_total(
        conn,
        &Scope {
            billing_period: Some(billing_period),
            ..Default::default()
        },
    )
}

fn service_breakdown_of(conn: &Connection, key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    sum_by_bucket(
        conn,
        &Scope {
            provider: Some(&key.provider),
            account_id: Some(&key.account_id),
            billing_period: Some(&key.billing_period),
            ..Default::default()
        },
        "coalesce(service_name, 'Other')",
    )
}

fn daily_totals_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    sum_by_day(
        conn,
        &Scope {
            provider: Some(provider),
            account_id: Some(account_id),
            since: Some(since),
            ..Default::default()
        },
    )
}

pub(crate) fn latest_balance_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
) -> Result<Option<Balance>> {
    let mut stmt = conn.prepare(
        "SELECT balance, granted_balance, topped_up_balance, currency,
                CAST(observed_at AS VARCHAR)
         FROM fct_balance_snapshot
         WHERE provider = ? AND account_id = ?
         ORDER BY observed_at DESC
         LIMIT 1",
    )?;

    let mut rows = stmt.query_map(params![provider, account_id], |row| {
        Ok((
            row.get::<_, f64>(0)?,
            row.get::<_, Option<f64>>(1)?,
            row.get::<_, Option<f64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;

    let Some(row) = rows.next().transpose()? else {
        return Ok(None);
    };
    let (balance, granted_balance, topped_up_balance, currency, observed_at) = row;

    Ok(Some(Balance {
        balance,
        granted_balance,
        topped_up_balance,
        currency,
        observed_at: super::parse_timestamp(&observed_at)?,
    }))
}

fn last_ingest_of(conn: &Connection, key: &PeriodKey) -> Result<Option<DateTime<Utc>>> {
    let mut stmt = conn.prepare(
        "SELECT CAST(max(completed_at) AS VARCHAR) FROM ingest_batch
         WHERE provider = ? AND account_id = ? AND billing_period = ? AND status = 'complete'",
    )?;

    let completed_at: Option<String> = stmt.query_row(
        params![key.provider, key.account_id, key.billing_period],
        |row| row.get(0),
    )?;

    completed_at
        .map(|stamp| super::parse_timestamp(&stamp))
        .transpose()
}

fn unconverted_charges_of(conn: &Connection, billing_period: &str) -> Result<i64> {
    conn.query_row(
        &format!(
            "SELECT count(*) FROM {NORMALIZED_VIEW}
             WHERE billing_period = ? AND billed_cost IS NOT NULL AND billed_cost_base IS NULL"
        ),
        params![billing_period],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

// ==================== Forecasting and period comparison ====================

/// The canonical MTD forecast for a billing period, replacing the "mean of
/// the last 7 non-zero days" heuristic the UI grew first.
///
/// The daily rate is measured from a baseline of `max(period start, first
/// expense date in the period)`: an account that started reporting — or
/// landed its first charge — mid-month is not averaged over days it was
/// not running, so the cold-start ramp-up does not drag the rate down.
pub fn forecast_for_period(billing_period: &str) -> Result<PeriodForecast> {
    with_connection_ref(|conn| forecast_for_period_of(conn, billing_period, Utc::now()))
}

fn forecast_for_period_of(
    conn: &Connection,
    billing_period: &str,
    now: DateTime<Utc>,
) -> Result<PeriodForecast> {
    let (spent, first_charge): (Option<f64>, Option<String>) = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base), CAST(min(charge_period_start) AS VARCHAR)
             FROM {NORMALIZED_VIEW}
             WHERE billing_period = ? AND charge_period_start < CAST(? AS TIMESTAMP)"
        ),
        params![billing_period, now.format(TIMESTAMP_FORMAT).to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let first_charge = first_charge
        .map(|stamp| super::parse_timestamp(&stamp).map(|at| at.date_naive()))
        .transpose()?;

    Ok(analytics::run_rate(
        &analytics::period_of(billing_period)?,
        now,
        spent.unwrap_or(0.0),
        first_charge,
    ))
}

/// Current period vs. the immediately preceding one — billing periods here
/// being monthly, the previous month.
pub fn period_over_period(billing_period: &str) -> Result<PeriodOverPeriod> {
    with_connection_ref(|conn| period_over_period_of(conn, billing_period))
}

fn period_over_period_of(conn: &Connection, billing_period: &str) -> Result<PeriodOverPeriod> {
    let previous = analytics::previous_period(billing_period)?;

    let mut stmt = conn.prepare(&format!(
        "SELECT billing_period = ? AS current_period,
                coalesce(service_name, 'Other') AS service,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE billing_period IN (?, ?)
         GROUP BY current_period, service"
    ))?;

    let rows = stmt
        .query_map(params![billing_period, billing_period, previous], |row| {
            Ok((
                row.get::<_, bool>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut current = std::collections::BTreeMap::new();
    let mut prior = std::collections::BTreeMap::new();
    for (is_current, service, amount) in rows {
        let side = if is_current { &mut current } else { &mut prior };
        *side.entry(service).or_insert(0.0) += amount;
    }

    Ok(analytics::compare_periods(current, prior))
}

// ==================== Cost-change decomposition ====================

/// Decompose the current-vs-previous-period delta into its components, by
/// charge category and by service, and check that they add back up.
pub fn cost_change_decomposition(billing_period: &str) -> Result<CostChangeDecomposition> {
    with_connection_ref(|conn| cost_change_decomposition_of(conn, billing_period))
}

fn cost_change_decomposition_of(
    conn: &Connection,
    billing_period: &str,
) -> Result<CostChangeDecomposition> {
    let previous = analytics::previous_period(billing_period)?;

    let categories = two_period_buckets(conn, billing_period, &previous, "charge_category")?;
    let services = two_period_buckets(
        conn,
        billing_period,
        &previous,
        "coalesce(service_name, 'Other')",
    )?;

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
    conn: &Connection,
    billing_period: &str,
    previous: &str,
    bucket: &str,
) -> Result<TwoPeriodBuckets> {
    let mut stmt = conn.prepare(&format!(
        "SELECT billing_period = ? AS current_period, {bucket} AS bucket,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE billing_period IN (?, ?)
         GROUP BY current_period, bucket"
    ))?;

    let rows = stmt
        .query_map(params![billing_period, billing_period, previous], |row| {
            Ok((
                row.get::<_, bool>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut buckets = TwoPeriodBuckets::new();
    for (is_current, bucket, amount) in rows {
        let entry = buckets.entry(bucket).or_insert((0.0, 0.0));
        let side = if is_current {
            &mut entry.0
        } else {
            &mut entry.1
        };
        *side += amount;
    }

    Ok(buckets)
}

// ==================== Forecast confidence bands ====================

/// Confidence bands around the period forecast.
///
/// The daily sample is measured from the same baseline as
/// [`forecast_for_period`] — `max(period start, first charge day)` — and a
/// day inside it with no charge counts as zero, so `daily_mean` agrees with
/// the forecast's daily rate. With fewer than two sampled days a standard
/// deviation does not exist, and both bands collapse onto `expected`.
pub fn forecast_bands_for_period(billing_period: &str) -> Result<ForecastBands> {
    with_connection_ref(|conn| forecast_bands_for_period_of(conn, billing_period, Utc::now()))
}

fn forecast_bands_for_period_of(
    conn: &Connection,
    billing_period: &str,
    now: DateTime<Utc>,
) -> Result<ForecastBands> {
    let forecast = forecast_for_period_of(conn, billing_period, now)?;

    // The same charge set the forecast totals: this period, before `now`.
    let daily = sum_by_day(
        conn,
        &Scope {
            billing_period: Some(billing_period),
            until: Some(now),
            ..Default::default()
        },
    )?;

    analytics::forecast_bands(
        &analytics::period_of(billing_period)?,
        now,
        forecast,
        &daily,
    )
}

// ==================== Trailing-average overlay ====================

/// A "typical day" series to overlay on the current month's daily line —
/// the Wealthfolio benchmark-comparison pattern.
///
/// For each of the last `day_count` days, oldest first, as `(YYYY-MM-DD,
/// amount)`: the average daily **usage** of the `months` complete calendar
/// months preceding the day's own month, spread over their calendar days —
/// a day with no charges counts as zero. The window ends where the day's
/// month begins, so a month is never compared against itself; every day of
/// one month therefore reads the same value and the series is the flat
/// benchmark line the actual daily line is drawn against. Usage rows only,
/// so a landed credit does not dip what a typical day costs.
///
/// Degenerate inputs (`day_count` or `months` below 1) yield an empty
/// series.
pub fn trailing_daily_average(day_count: i64, months: i64) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| trailing_daily_average_of(conn, day_count, months, Utc::now()))
}

fn trailing_daily_average_of(
    conn: &Connection,
    day_count: i64,
    months: i64,
    now: DateTime<Utc>,
) -> Result<Vec<(String, f64)>> {
    let Some((window_start, window_end)) = analytics::trailing_window(day_count, months, now)
    else {
        return Ok(Vec::new());
    };
    let at_midnight = |date: NaiveDate| {
        analytics::midnight(date)
            .format(TIMESTAMP_FORMAT)
            .to_string()
    };

    // Keyed by charge time, not by `billing_period`: the window is bounded by
    // charge-time instants, so the keys it is read back with have to be
    // measured the same way.
    let mut stmt = conn.prepare(&format!(
        "SELECT strftime(charge_period_start, '%Y-%m') AS month,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE charge_category = 'Usage'
           AND charge_period_start >= CAST(? AS TIMESTAMP)
           AND charge_period_start < CAST(? AS TIMESTAMP)
         GROUP BY month"
    ))?;

    let monthly: std::collections::BTreeMap<String, f64> = stmt
        .query_map(
            params![at_midnight(window_start), at_midnight(window_end)],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                ))
            },
        )?
        .collect::<Result<_, _>>()?;

    Ok(analytics::trailing_average(
        day_count, months, now, &monthly,
    ))
}

// ==================== Data-quality summary ====================

/// The data-quality findings for a period: unconverted charges, untagged
/// usage for `tag_key`, services whose usage carries no region, and
/// unreconciled adjustments. A clean period yields an empty list.
pub fn data_quality_issues(billing_period: &str, tag_key: &str) -> Result<Vec<DataQualityIssue>> {
    with_connection_ref(|conn| data_quality_issues_of(conn, billing_period, tag_key))
}

fn data_quality_issues_of(
    conn: &Connection,
    billing_period: &str,
    tag_key: &str,
) -> Result<Vec<DataQualityIssue>> {
    // Charges no rate covers, against the period's row count.
    let (rows, unconverted): (i64, i64) = conn.query_row(
        &format!(
            "SELECT count(*),
                    count(*) FILTER (WHERE billed_cost IS NOT NULL AND billed_cost_base IS NULL)
             FROM {NORMALIZED_VIEW}
             WHERE billing_period = ?"
        ),
        params![billing_period],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // Period usage is the denominator of both share checks.
    let (usage, _) = sum_usage_and_credits(
        conn,
        &Scope {
            billing_period: Some(billing_period),
            ..Default::default()
        },
    )?;

    // Usage with no value for the tag the attribution page groups by.
    let (untagged, untagged_count): (Option<f64>, i64) = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base), count(*)
             FROM {NORMALIZED_VIEW}
             WHERE billing_period = ?
               AND charge_category = 'Usage'
               AND coalesce(nullif(json_extract_string(tags, ?), ''), '') = ''"
        ),
        params![billing_period, tag_key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // Region-less usage, per service.
    let mut stmt = conn.prepare(&format!(
        "SELECT coalesce(service_name, 'Other') AS service,
                sum(billed_cost_base) AS amount, count(*) AS charges
         FROM {NORMALIZED_VIEW}
         WHERE billing_period = ?
           AND charge_category = 'Usage'
           AND region_id IS NULL
         GROUP BY service
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;
    let regionless = stmt
        .query_map(params![billing_period], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // `cloud::deduction`'s escape hatch: money a bill accounts for that no
    // named deduction covers.
    let unreconciled: (i64, Option<f64>) = conn.query_row(
        &format!(
            "SELECT count(*), sum(billed_cost_base)
             FROM {NORMALIZED_VIEW}
             WHERE billing_period = ?
               AND charge_category = 'Adjustment'
               AND charge_description = ?"
        ),
        params![billing_period, crate::cloud::deduction::UNRECONCILED],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    Ok(analytics::data_quality(QualityCounts {
        tag_key,
        rows,
        unconverted,
        usage,
        untagged: untagged.unwrap_or(0.0),
        untagged_count,
        regionless,
        unreconciled: Some((unreconciled.0, unreconciled.1.unwrap_or(0.0))),
    }))
}

// ==================== Breakdown dimensions ====================

impl BreakdownDim {
    /// The bucket expression; a charge without the dimension reads as
    /// `'Other'`, as a charge without a service does.
    fn bucket_sql(self) -> &'static str {
        match self {
            Self::Service => "coalesce(service_name, 'Other')",
            Self::Region => "coalesce(region_id, 'Other')",
            Self::ServiceCategory => "coalesce(service_category, 'Other')",
        }
    }
}

/// Charges of one period grouped by `dim`, largest first. With
/// [`BreakdownDim::Service`] this is [`service_breakdown`].
pub fn breakdown_by(key: &PeriodKey, dim: BreakdownDim) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| breakdown_by_of(conn, key, dim))
}

fn breakdown_by_of(
    conn: &Connection,
    key: &PeriodKey,
    dim: BreakdownDim,
) -> Result<Vec<(String, f64)>> {
    sum_by_bucket(
        conn,
        &Scope {
            provider: Some(&key.provider),
            account_id: Some(&key.account_id),
            billing_period: Some(&key.billing_period),
            ..Default::default()
        },
        dim.bucket_sql(),
    )
}

/// The `limit` costliest resources of a period, largest first — a charge
/// with no `resource_id` cannot be attributed to one and is left out.
pub fn top_resources(key: &PeriodKey, limit: usize) -> Result<Vec<TopResource>> {
    with_connection_ref(|conn| top_resources_of(conn, key, limit))
}

fn top_resources_of(conn: &Connection, key: &PeriodKey, limit: usize) -> Result<Vec<TopResource>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT resource_id, any_value(resource_name),
                any_value(coalesce(service_name, 'Other')) AS service,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE provider = ? AND account_id = ? AND billing_period = ?
           AND resource_id IS NOT NULL
         GROUP BY resource_id
         HAVING amount > 0
         ORDER BY amount DESC
         LIMIT ?"
    ))?;

    let rows = stmt
        .query_map(
            params![
                key.provider,
                key.account_id,
                key.billing_period,
                bounded_limit(limit)
            ],
            |row| {
                Ok(TopResource {
                    resource_id: row.get(0)?,
                    resource_name: row.get(1)?,
                    service: row.get(2)?,
                    amount: row.get(3)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// Usage of one period grouped by `(provider, service, tag_value)` in a
/// single round-trip, largest first — the attribution page's N+1 killer:
/// filtering the rows of one `(provider, service)` gives exactly what
/// [`service_tag_usage_breakdown`] returns for it, so a page that shows
/// every service no longer queries once per service.
pub fn tag_usage_breakdown_by_service(
    billing_period: &str,
    tag_key: &str,
) -> Result<Vec<ServiceTagUsage>> {
    with_connection_ref(|conn| tag_usage_breakdown_by_service_of(conn, billing_period, tag_key))
}

fn tag_usage_breakdown_by_service_of(
    conn: &Connection,
    billing_period: &str,
    tag_key: &str,
) -> Result<Vec<ServiceTagUsage>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, coalesce(service_name, 'Other') AS service,
                coalesce(nullif(json_extract_string(tags, ?), ''), 'Unallocated') AS tag_value,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE billing_period = ?
           AND charge_category = 'Usage'
         GROUP BY provider, service, tag_value
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let rows = stmt
        .query_map(params![tag_key, billing_period], |row| {
            Ok(ServiceTagUsage {
                provider: row.get(0)?,
                service: row.get(1)?,
                tag_value: row.get(2)?,
                amount: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

// ==================== Per-account reads ====================
//
// The Account detail page's series: each mirrors its cross-account
// original above, filtered to one `(provider, account_id)`.

/// [`daily_usage_all`] for a single account.
pub fn daily_usage_of(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    with_connection_ref(|conn| {
        sum_by_day(
            conn,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                usage_only: true,
                ..Default::default()
            },
        )
    })
}

/// [`monthly_usage`] for a single account.
pub fn monthly_usage_of(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| {
        sum_by_period(
            conn,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                usage_only: true,
                ..Default::default()
            },
        )
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
    with_connection_ref(|conn| {
        sum_by_bucket(
            conn,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                until: Some(until),
                usage_only: true,
                ..Default::default()
            },
            "coalesce(service_name, 'Other')",
        )
    })
}

/// [`usage_and_credits_between`] for a single account.
pub fn usage_and_credits_of_between(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<(f64, f64)> {
    with_connection_ref(|conn| {
        sum_usage_and_credits(
            conn,
            &Scope {
                provider: Some(provider),
                account_id: Some(account_id),
                since: Some(since),
                until: Some(until),
                ..Default::default()
            },
        )
    })
}

// ==================== Ad-hoc queries ====================

/// How much one ad-hoc result may hold, in rows and in cells — a wide
/// result hits the cell budget long before the row one.
const MAX_ROWS: usize = 100_000;
const MAX_CELLS: usize = 2_000_000;

/// Run a read-only query against the ledger and render the result.
///
/// Unlike every other read in this file, this does not take the shared
/// connection: an ad-hoc query can run for seconds, and holding the global
/// lock would stall every page behind it. The connection is opened
/// read-only, so a statement that slips past the guard still cannot write.
pub fn run_adhoc(sql: &str) -> Result<AdhocResult> {
    let path = crate::config::get_ledger_database_path()?;
    let conn =
        Connection::open_with_flags(path, Config::default().access_mode(AccessMode::ReadOnly)?)?;
    run_adhoc_of(&conn, sql)
}

fn run_adhoc_of(conn: &Connection, sql: &str) -> Result<AdhocResult> {
    run_adhoc_within(conn, sql, MAX_ROWS, MAX_CELLS)
}

fn run_adhoc_within(
    conn: &Connection,
    sql: &str,
    max_rows: usize,
    max_cells: usize,
) -> Result<AdhocResult> {
    if !is_read_only(sql) {
        anyhow::bail!("only read-only queries are allowed");
    }

    let started = std::time::Instant::now();
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    // Names are only known once the statement has been executed — asking
    // the Statement for them before `query` panics.
    let columns = rows
        .as_ref()
        .map(|statement| statement.column_names())
        .unwrap_or_default();
    let row_budget = max_rows.min(max_cells / columns.len().max(1));

    let mut numeric: Vec<Option<bool>> = vec![None; columns.len()];
    let mut out: Vec<Vec<Option<String>>> = Vec::new();
    let mut truncated = false;

    while let Some(row) = rows.next()? {
        if out.len() >= row_budget {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(columns.len());
        for (index, slot) in numeric.iter_mut().enumerate() {
            let value = row.get_ref(index)?;
            if slot.is_none() {
                *slot = match value {
                    ValueRef::Null => None,
                    _ => Some(is_numeric(&value)),
                };
            }
            cells.push(format_cell(&value));
        }
        out.push(cells);
    }

    Ok(AdhocResult {
        columns,
        numeric: numeric
            .into_iter()
            .map(|slot| slot.unwrap_or(false))
            .collect(),
        rows: out,
        truncated,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Statements that must never reach the ledger from the query page.
const MUTATING_KEYWORDS: &[&str] = &[
    "insert",
    "update",
    "delete",
    "drop",
    "create",
    "alter",
    "attach",
    "detach",
    "copy",
    "export",
    "import",
    "install",
    "load",
    "pragma",
    "set",
    "use",
    "call",
    "vacuum",
    "checkpoint",
    "replace",
    "merge",
    "truncate",
    "grant",
    "revoke",
];

/// What an ad-hoc query may start with.
const READ_ONLY_STARTERS: &[&str] = &[
    "select",
    "with",
    "explain",
    "describe",
    "show",
    "summarize",
    "values",
];

/// Whether `sql` is safe to run from the query page.
///
/// Word-wise rather than trusting the leading keyword, so `SELECT 1; DROP
/// TABLE t` and an INSERT inside a CTE are both caught — and erring toward
/// `false`: a false hit costs the user a rephrased query, a miss costs the
/// ledger. Comments and string literals are blanked first, so a `-- drop`
/// note or a `'DROP TABLE'` value does not count as a statement.
fn is_read_only(sql: &str) -> bool {
    let code = code_without_literals(sql);
    let mut words = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty());

    let Some(first) = words.next() else {
        return false;
    };
    if !READ_ONLY_STARTERS
        .iter()
        .any(|starter| first.eq_ignore_ascii_case(starter))
    {
        return false;
    }

    !words.any(|word| {
        MUTATING_KEYWORDS
            .iter()
            .any(|keyword| word.eq_ignore_ascii_case(keyword))
    })
}

/// `sql` with `--` and `/* */` comments and single-quoted string literals
/// blanked to spaces, so keyword scanning sees only code. A `''` inside a
/// literal is an escaped quote, not the end of it.
fn code_without_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push(' ');
                loop {
                    match chars.next() {
                        Some('\'') if chars.peek() == Some(&'\'') => {
                            chars.next();
                        }
                        Some('\'') | None => break,
                        _ => {}
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }

    out
}

/// Whether a value is one the UI should right-align.
fn is_numeric(value: &ValueRef) -> bool {
    matches!(
        value,
        ValueRef::TinyInt(_)
            | ValueRef::SmallInt(_)
            | ValueRef::Int(_)
            | ValueRef::BigInt(_)
            | ValueRef::HugeInt(_)
            | ValueRef::UTinyInt(_)
            | ValueRef::USmallInt(_)
            | ValueRef::UInt(_)
            | ValueRef::UBigInt(_)
            | ValueRef::Float(_)
            | ValueRef::Double(_)
            | ValueRef::Decimal(_)
    )
}

/// A cell as the UI shows it: NULL stays `None`, everything else is text.
fn format_cell(value: &ValueRef) -> Option<String> {
    match value {
        ValueRef::Null => None,
        // Text and Blob are read straight from the borrow: `to_owned` on
        // Text expects valid UTF-8 and would panic on the rare value that
        // is not.
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Some(format!("<{} bytes>", bytes.len())),
        other => Some(format_value(&other.to_owned())),
    }
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(v) => v.to_string(),
        Value::TinyInt(v) => v.to_string(),
        Value::SmallInt(v) => v.to_string(),
        Value::Int(v) => v.to_string(),
        Value::BigInt(v) => v.to_string(),
        Value::HugeInt(v) => v.to_string(),
        Value::UTinyInt(v) => v.to_string(),
        Value::USmallInt(v) => v.to_string(),
        Value::UInt(v) => v.to_string(),
        Value::UBigInt(v) => v.to_string(),
        // Rust's Display for floats never switches to scientific notation.
        Value::Float(v) => v.to_string(),
        Value::Double(v) => v.to_string(),
        Value::Decimal(v) => v.to_string(),
        Value::Timestamp(unit, v) => format_timestamp(*unit, *v),
        Value::Text(v) | Value::Enum(v) => v.clone(),
        Value::Blob(v) => format!("<{} bytes>", v.len()),
        Value::Date32(days) => format_date(*days),
        Value::Time64(unit, v) => format_time(*unit, *v),
        Value::Interval {
            months,
            days,
            nanos,
        } => format_interval(*months, *days, *nanos),
        Value::List(items) | Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Struct(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(key, value)| format!("{key}: {}", format_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(key, value)| format!("{}: {}", format_value(key), format_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Union(inner) => format_value(inner),
    }
}

/// Microseconds since the epoch as `YYYY-MM-DD HH:MM:SS`, with fractional
/// seconds only when they are not zero — the way DuckDB itself renders one.
fn format_timestamp(unit: TimeUnit, value: i64) -> String {
    let micros = unit.to_micros(value);
    match chrono::DateTime::from_timestamp_micros(micros) {
        Some(stamp) => {
            let base = stamp.format(TIMESTAMP_FORMAT).to_string();
            let fraction = stamp.timestamp_subsec_micros();
            if fraction == 0 {
                base
            } else {
                format!("{base}.{}", format!("{fraction:06}").trim_end_matches('0'))
            }
        }
        None => micros.to_string(),
    }
}

/// Days since the epoch as `YYYY-MM-DD`.
fn format_date(days: i32) -> String {
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("the epoch exists");
    match epoch.checked_add_signed(chrono::Duration::days(days as i64)) {
        Some(date) => date.format("%Y-%m-%d").to_string(),
        None => days.to_string(),
    }
}

/// Time of day as `HH:MM:SS`, with fractional seconds only when they are
/// not zero.
fn format_time(unit: TimeUnit, value: i64) -> String {
    let micros = unit.to_micros(value);
    let seconds = micros.div_euclid(1_000_000);
    let nanos = micros.rem_euclid(1_000_000) as u32 * 1000;
    match chrono::NaiveTime::from_num_seconds_from_midnight_opt(seconds as u32, nanos) {
        Some(time) => {
            let base = time.format("%H:%M:%S").to_string();
            let fraction = micros.rem_euclid(1_000_000);
            if fraction == 0 {
                base
            } else {
                format!("{base}.{}", format!("{fraction:06}").trim_end_matches('0'))
            }
        }
        None => value.to_string(),
    }
}

fn format_interval(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = Vec::new();
    if months != 0 {
        parts.push(format!("{months} months"));
    }
    if days != 0 {
        parts.push(format!("{days} days"));
    }
    if nanos != 0 {
        parts.push(format!("{} seconds", nanos as f64 / 1e9));
    }
    if parts.is_empty() {
        "0 seconds".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::BillingPeriod;
    use crate::ledger::schema;
    use crate::ledger::{BalanceSnapshot, Channel, Charge, ChargeCategory};
    use chrono::TimeZone;

    fn conn(reporting_currency: &str) -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        schema::apply(&conn).expect("schema applies");
        schema::apply_reporting_currency(&conn, reporting_currency).expect("view applies");
        conn
    }

    /// The tag rollups call `json_extract_string` and the raw store writes
    /// and re-reads its batches as Parquet, so both extensions have to be
    /// there without duckdb downloading anything: under the hardened runtime
    /// library validation refuses to map an extension signed by another
    /// team, and a runner with no route to the extension repository cannot
    /// autoload one at all. The `json` and `parquet` features on the duckdb
    /// dependency compile them in, which this asserts by turning both
    /// autoload and auto-install off first.
    #[test]
    fn extensions_are_compiled_in() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(
            "SET extension_directory='/tmp/cloudbridge-no-extensions';
             SET autoinstall_known_extensions=false;
             SET autoload_known_extensions=false;",
        )
        .expect("settings apply");

        let value: String = conn
            .query_row(
                r#"SELECT json_extract_string('{"env":"prod"}', '$.env')"#,
                [],
                |row| row.get(0),
            )
            .expect("json works without a downloaded extension");

        assert_eq!(value, "prod");

        let file = std::env::temp_dir().join("cloudbridge_compiled_in.parquet");
        let path = file.to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!(
            "COPY (SELECT 'prod' AS env) TO '{path}' (FORMAT PARQUET);
             CREATE TABLE probe AS SELECT env FROM read_parquet('{path}');"
        ))
        .expect("parquet works without a downloaded extension");
        let env: String = conn
            .query_row("SELECT env FROM probe", [], |row| row.get(0))
            .expect("the written row reads back");

        assert_eq!(env, "prod");
        std::fs::remove_file(&file).ok();
    }

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, 0, 0, 0).unwrap()
    }

    fn charge(service: &str, amount: f64, currency: &str, day: u32) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: Some(amount),
            ..Charge::new(at(day), at(day + 1), currency)
        }
    }

    /// A charge starting at an arbitrary instant, for windows that span
    /// months — `charge` only builds August days.
    fn charge_on(service: &str, amount: f64, currency: &str, start: DateTime<Utc>) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: Some(amount),
            ..Charge::new(start, start + chrono::Duration::days(1), currency)
        }
    }

    fn aws() -> PeriodKey {
        PeriodKey::new("AWS", "acct-1", "2026-08")
    }

    fn aliyun() -> PeriodKey {
        PeriodKey::new("Aliyun", "acct-2", "2026-08")
    }

    fn last_channel_of(conn: &Connection, key: &PeriodKey) -> Result<Option<Channel>> {
        // Only one batch per period is 'complete' — `write_period` supersedes
        // the rest — so this is the batch whose rows are in `fct_charge`.
        let mut stmt = conn.prepare(
            "SELECT channel FROM ingest_batch
             WHERE provider = ? AND account_id = ? AND billing_period = ? AND status = 'complete'
             LIMIT 1",
        )?;

        let mut rows = stmt.query(params![key.provider, key.account_id, key.billing_period])?;

        match rows.next()? {
            Some(row) => Ok(Some(Channel::from_stored(
                row.get::<_, Option<String>>(0)?.as_deref(),
            ))),
            None => Ok(None),
        }
    }

    fn write(conn: &mut Connection, key: &PeriodKey, charges: &[Charge]) {
        write_through(conn, key, charges, Channel::Api);
    }

    fn write_through(conn: &mut Connection, key: &PeriodKey, charges: &[Charge], channel: Channel) {
        let batch_id = crate::ledger::new_batch_id();
        crate::ledger::write_period(conn, key, &batch_id, charges, None, channel).unwrap();
    }

    /// A refresh has to be able to tell that a month was imported by hand,
    /// so that an automatic fetch does not replace the provider's own bill
    /// with a coarser reading of the same month.
    #[test]
    fn a_period_remembers_which_channel_it_arrived_through() {
        let mut conn = conn("USD");

        write_through(
            &mut conn,
            &aliyun(),
            &[charge("Model Studio", 12.34, "CNY", 9)],
            Channel::File,
        );
        write(&mut conn, &aws(), &[charge("EC2", 12.5, "USD", 1)]);

        assert_eq!(
            last_channel_of(&conn, &aliyun()).unwrap(),
            Some(Channel::File)
        );
        assert_eq!(last_channel_of(&conn, &aws()).unwrap(), Some(Channel::Api));
    }

    /// Re-importing, or fetching over an imported month, replaces the
    /// channel along with the rows: only the batch holding the rows counts.
    #[test]
    fn replacing_a_period_replaces_the_channel_it_is_credited_to() {
        let mut conn = conn("USD");

        write_through(
            &mut conn,
            &aliyun(),
            &[charge("Model Studio", 12.34, "CNY", 9)],
            Channel::File,
        );
        write_through(
            &mut conn,
            &aliyun(),
            &[charge("Model Studio", 12.34, "CNY", 9)],
            Channel::Api,
        );

        assert_eq!(
            last_channel_of(&conn, &aliyun()).unwrap(),
            Some(Channel::Api)
        );
    }

    #[test]
    fn a_period_that_was_never_ingested_has_no_channel() {
        let conn = conn("USD");
        assert_eq!(last_channel_of(&conn, &aws()).unwrap(), None);
    }

    #[test]
    fn a_cross_cloud_total_is_one_query_in_one_currency() {
        let mut conn = conn("USD");
        write(&mut conn, &aws(), &[charge("EC2", 12.5, "USD", 1)]);
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 1)]);

        // 710 CNY at the built-in 0.1408 is 99.968 USD.
        let total = total_for_period_of(&conn, "2026-08").unwrap();
        assert!((total - 112.468).abs() < 1e-6, "got {total}");

        // Each account still reports in the same currency as the total.
        assert!((period_total_of(&conn, &aws()).unwrap() - 12.5).abs() < 1e-9);
        assert!((period_total_of(&conn, &aliyun()).unwrap() - 99.968).abs() < 1e-6);
    }

    #[test]
    fn changing_the_reporting_currency_rereads_the_same_rows() {
        let mut conn = conn("USD");
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 1)]);

        assert!((period_total_of(&conn, &aliyun()).unwrap() - 99.968).abs() < 1e-6);

        // No rewrite of the fact table: only the view changes.
        schema::apply_reporting_currency(&conn, "CNY").unwrap();
        assert!((period_total_of(&conn, &aliyun()).unwrap() - 710.0).abs() < 1e-9);
    }

    #[test]
    fn a_charge_in_a_currency_no_rate_covers_is_reported_rather_than_counted() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 12.5, "USD", 1),
                charge("Something", 100.0, "JPY", 1),
            ],
        );

        // The unconvertible row is left out of the total...
        let total = total_for_period_of(&conn, "2026-08").unwrap();
        assert!((total - 12.5).abs() < 1e-9, "got {total}");
        // ...and is countable, so the UI can say so.
        assert_eq!(unconverted_charges_of(&conn, "2026-08").unwrap(), 1);
    }

    #[test]
    fn a_rate_is_taken_from_the_charges_own_time() {
        let mut conn = conn("USD");
        conn.execute_batch(
            "INSERT OR REPLACE INTO dim_fx_rate VALUES ('CNY', 'USD', DATE '2026-08-15', 0.2, 'test')",
        )
        .unwrap();

        write(
            &mut conn,
            &aliyun(),
            &[
                charge("ECS", 100.0, "CNY", 1),
                charge("ECS", 100.0, "CNY", 20),
            ],
        );

        // The 1 August charge predates the new rate and keeps the old one;
        // the 20 August charge takes the newer.
        let daily = daily_totals_of(&conn, "Aliyun", "acct-2", at(1)).unwrap();
        assert_eq!(daily.len(), 2);
        assert!((daily[0].1 - 14.08).abs() < 1e-9, "got {:?}", daily[0]);
        assert!((daily[1].1 - 20.0).abs() < 1e-9, "got {:?}", daily[1]);
    }

    #[test]
    fn a_breakdown_is_by_service_largest_first() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("S3", 0.75, "USD", 1),
                charge("EC2", 12.5, "USD", 1),
                charge("EC2", 4.0, "USD", 2),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-2.0),
                    ..charge("EC2", -2.0, "USD", 2)
                },
            ],
        );

        let breakdown = service_breakdown_of(&conn, &aws()).unwrap();
        assert_eq!(
            breakdown,
            vec![("EC2".to_string(), 14.5), ("S3".to_string(), 0.75)]
        );
    }

    #[test]
    fn a_period_that_was_never_ingested_has_no_ingest_time() {
        let mut conn = conn("USD");
        assert!(last_ingest_of(&conn, &aws()).unwrap().is_none());

        write(&mut conn, &aws(), &[charge("EC2", 1.0, "USD", 1)]);
        assert!(last_ingest_of(&conn, &aws()).unwrap().is_some());
    }

    #[test]
    fn the_newest_balance_is_the_one_reported() {
        let mut conn = conn("USD");
        for (day, amount) in [(1, 50.0), (3, 30.0), (2, 40.0)] {
            crate::ledger::write_balance(
                &mut conn,
                &BalanceSnapshot {
                    provider: "DeepSeek".to_string(),
                    account_id: "acct-3".to_string(),
                    observed_at: at(day),
                    balance: amount,
                    granted_balance: Some(5.0),
                    topped_up_balance: Some(amount - 5.0),
                    currency: "CNY".to_string(),
                },
            )
            .unwrap();
        }

        let balance = latest_balance_of(&conn, "DeepSeek", "acct-3")
            .unwrap()
            .expect("a balance was recorded");
        assert_eq!(balance.balance, 30.0);
        assert_eq!(balance.observed_at, at(3));
        // Not converted: a balance is what is left, not what was spent.
        assert_eq!(balance.currency, "CNY");

        assert!(latest_balance_of(&conn, "DeepSeek", "unknown")
            .unwrap()
            .is_none());
    }

    fn tagged_charge(service: &str, amount: f64, day: u32, tags: Option<&str>) -> Charge {
        Charge {
            tags: tags.map(str::to_string),
            ..charge(service, amount, "USD", day)
        }
    }

    #[test]
    fn daily_totals_all_spans_providers_oldest_first() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[charge("EC2", 12.5, "USD", 1), charge("EC2", 4.0, "USD", 2)],
        );
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 2)]);

        let daily = daily_totals_all_of(&conn, at(1)).unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].0, "2026-08-01");
        assert!((daily[0].1 - 12.5).abs() < 1e-9);
        // 4.0 USD + 710 CNY at 0.1408.
        assert!((daily[1].1 - 103.968).abs() < 1e-6, "got {:?}", daily[1]);

        assert!(daily_totals_all_of(&conn, at(10)).unwrap().is_empty());
    }

    #[test]
    fn daily_totals_come_from_the_rollup_when_it_is_current() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[charge("EC2", 12.5, "USD", 1), charge("EC2", 4.0, "USD", 2)],
        );
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 2)]);
        crate::ledger::rollup::rebuild_all_of(&conn).unwrap();

        // The same numbers the view path computes, off the day-grain table.
        let daily = daily_totals_all_of(&conn, at(1)).unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].0, "2026-08-01");
        assert!((daily[0].1 - 12.5).abs() < 1e-9);
        assert!((daily[1].1 - 103.968).abs() < 1e-6, "got {:?}", daily[1]);

        let per_account = daily_totals_of(&conn, "Aliyun", "acct-2", at(1)).unwrap();
        assert_eq!(per_account.len(), 1);
        assert!((per_account[0].1 - 99.968).abs() < 1e-6);

        // Proof of the source: with the fact table emptied the rollup still
        // answers, where the view path would see nothing.
        conn.execute("DELETE FROM fct_charge", []).unwrap();
        assert_eq!(daily_totals_all_of(&conn, at(1)).unwrap().len(), 2);
    }

    #[test]
    fn a_sub_day_window_edge_reads_the_view_not_the_rollup() {
        let mut conn = conn("USD");
        write(&mut conn, &aws(), &[charge("EC2", 12.5, "USD", 1)]);
        crate::ledger::rollup::rebuild_all_of(&conn).unwrap();

        // Noon: the day grain cannot exclude the morning of a day, so the
        // view answers — and the midnight charge falls outside the window.
        let since = at(1) + chrono::Duration::hours(12);
        assert!(daily_totals_of(&conn, "AWS", "acct-1", since)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn provider_service_totals_group_and_order() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 12.5, "USD", 1),
                charge("S3", 0.75, "USD", 1),
                Charge {
                    service_name: None,
                    billed_cost: Some(2.0),
                    ..charge("ignored", 2.0, "USD", 1)
                },
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-5.0),
                    ..charge("Refunded", -5.0, "USD", 1)
                },
            ],
        );
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 1)]);

        let totals = provider_service_totals_of(&conn, "2026-08").unwrap();
        assert_eq!(totals[0].0, "Aliyun");
        assert_eq!(totals[0].1, "ECS");
        assert!((totals[0].2 - 99.968).abs() < 1e-6);
        // A NULL service reads as 'Other'; a net-negative group is dropped.
        let names: Vec<&str> = totals.iter().map(|(_, s, _)| s.as_str()).collect();
        assert!(names.contains(&"Other"));
        assert!(!names.contains(&"Refunded"));
        assert!(!names.contains(&"ignored"));
    }

    #[test]
    fn a_tag_breakdown_buckets_everything_without_the_key_as_unallocated() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                tagged_charge("EC2", 10.0, 1, Some(r#"{"business_line":"etl"}"#)),
                tagged_charge("S3", 3.0, 1, Some(r#"{"other":"x"}"#)),
                tagged_charge("NAT", 2.0, 1, None),
                tagged_charge("RDS", 1.0, 1, Some(r#"{"business_line":""}"#)),
                tagged_charge("EC2", 5.0, 2, Some(r#"{"business_line":"etl"}"#)),
            ],
        );

        let breakdown = tag_breakdown_of(&conn, "2026-08", "business_line", None).unwrap();
        assert_eq!(
            breakdown,
            vec![("etl".to_string(), 15.0), ("Unallocated".to_string(), 6.0),]
        );
    }

    #[test]
    fn a_service_tag_breakdown_sees_only_that_service() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                tagged_charge("EC2", 10.0, 1, Some(r#"{"business_line":"etl"}"#)),
                tagged_charge("EC2", 4.0, 1, None),
                tagged_charge("S3", 3.0, 1, Some(r#"{"business_line":"search"}"#)),
            ],
        );

        let breakdown =
            tag_breakdown_of(&conn, "2026-08", "business_line", Some(("AWS", "EC2"))).unwrap();
        assert_eq!(
            breakdown,
            vec![("etl".to_string(), 10.0), ("Unallocated".to_string(), 4.0),]
        );
    }

    #[test]
    fn untagged_detail_is_the_largest_unattributed_charges() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                Charge {
                    charge_description: Some("NAT gateway".to_string()),
                    ..tagged_charge("VPC", 24.0, 1, None)
                },
                tagged_charge("S3", 17.0, 1, Some(r#"{"business_line":"etl"}"#)),
                tagged_charge("EC2", 12.5, 1, None),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    ..tagged_charge("EC2", -9.0, 1, None)
                },
            ],
        );
        write(&mut conn, &aliyun(), &[tagged_charge("ECS", 30.0, 1, None)]);

        let detail = untagged_detail_of(&conn, "2026-08", "business_line", 1).unwrap();
        assert_eq!(detail.len(), 1);
        // Largest first, across providers; credits are not "untagged spend".
        assert_eq!(detail[0].provider, "Aliyun");
        assert_eq!(detail[0].service.as_deref(), Some("ECS"));
        assert!((detail[0].amount - 30.0).abs() < 1e-9);

        let all = untagged_detail_of(&conn, "2026-08", "business_line", 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[1].description.as_deref(), Some("NAT gateway"));

        // "Every row" as usize::MAX must not wrap to a negative LIMIT.
        let every = untagged_detail_of(&conn, "2026-08", "business_line", usize::MAX).unwrap();
        assert_eq!(every.len(), 3);
    }

    #[test]
    fn usage_and_credits_splits_gross_usage_from_credits() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 12.5, "USD", 1),
                charge("S3", 2.5, "USD", 1),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-10.0),
                    ..charge("EC2", -10.0, "USD", 2)
                },
                Charge {
                    charge_category: ChargeCategory::Tax,
                    billed_cost: Some(1.25),
                    ..charge("Tax", 1.25, "USD", 2)
                },
            ],
        );

        let (usage, credits) = usage_and_credits_of(&conn, "2026-08").unwrap();
        assert!((usage - 15.0).abs() < 1e-9, "got {usage}");
        assert!((credits - -10.0).abs() < 1e-9, "got {credits}");

        // The Tax row is in neither bucket but stays in the net total.
        let net = total_for_period_of(&conn, "2026-08").unwrap();
        assert!((net - 6.25).abs() < 1e-9, "got {net}");
    }

    #[test]
    fn untagged_usage_by_service_rolls_charges_up_to_one_row_per_service() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                tagged_charge("EC2", 12.5, 1, None),
                tagged_charge("EC2", 4.0, 2, None),
                tagged_charge("EC2", 1.0, 3, None),
                tagged_charge("S3", 20.0, 1, Some(r#"{"business_line":"etl"}"#)),
                tagged_charge("S3", 2.0, 1, None),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    ..tagged_charge("EC2", -9.0, 1, None)
                },
            ],
        );

        let rows = untagged_usage_by_service_of(&conn, "2026-08", "business_line", 10).unwrap();
        assert_eq!(rows.len(), 2);
        // Three EC2 charges read as one row; a credit is not usage.
        assert_eq!(rows[0].provider, "AWS");
        assert_eq!(rows[0].service.as_deref(), Some("EC2"));
        assert!((rows[0].amount - 17.5).abs() < 1e-9);
        assert_eq!(rows[1].service.as_deref(), Some("S3"));
        assert!((rows[1].amount - 2.0).abs() < 1e-9);

        // The limit applies to rolled-up rows, not to charges.
        let top = untagged_usage_by_service_of(&conn, "2026-08", "business_line", 1).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].service.as_deref(), Some("EC2"));
    }

    #[test]
    fn a_tag_usage_breakdown_counts_usage_only() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                tagged_charge("EC2", 10.0, 1, Some(r#"{"business_line":"etl"}"#)),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    ..tagged_charge("EC2", -4.0, 1, Some(r#"{"business_line":"etl"}"#))
                },
                tagged_charge("NAT", 2.0, 1, None),
            ],
        );

        let breakdown = tag_usage_breakdown_of(&conn, "2026-08", "business_line", None).unwrap();
        assert_eq!(
            breakdown,
            vec![("etl".to_string(), 10.0), ("Unallocated".to_string(), 2.0),]
        );
    }

    #[test]
    fn a_windowed_usage_and_credits_is_half_open_on_charge_time() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 1.0, "USD", 9),  // before the window
                charge("EC2", 2.0, "USD", 10), // in
                charge("S3", 4.0, "USD", 15),  // in
                charge("EC2", 8.0, "USD", 20), // at `until`: out
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-1.5),
                    ..charge("EC2", -1.5, "USD", 12)
                },
                Charge {
                    charge_category: ChargeCategory::Tax,
                    billed_cost: Some(0.5),
                    ..charge("Tax", 0.5, "USD", 12)
                },
            ],
        );

        let (usage, credits) = usage_and_credits_between_of(&conn, at(10), at(20)).unwrap();
        assert!((usage - 6.0).abs() < 1e-9, "got {usage}");
        assert!((credits - -1.5).abs() < 1e-9, "got {credits}");

        // An empty window reads as zeros, not an error.
        assert_eq!(
            usage_and_credits_between_of(&conn, at(25), at(26)).unwrap(),
            (0.0, 0.0)
        );
    }

    #[test]
    fn a_windowed_total_is_net_of_every_category_in_the_window() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 2.0, "USD", 10),
                charge("EC2", 8.0, "USD", 20), // at `until`: out
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-1.5),
                    ..charge("EC2", -1.5, "USD", 12)
                },
                Charge {
                    charge_category: ChargeCategory::Tax,
                    billed_cost: Some(0.5),
                    ..charge("Tax", 0.5, "USD", 12)
                },
            ],
        );

        // 2.0 usage − 1.5 credit + 0.5 tax; the day-20 charge is out.
        let total = total_between_of(&conn, at(10), at(20)).unwrap();
        assert!((total - 1.0).abs() < 1e-9, "got {total}");
    }

    #[test]
    fn a_windowed_tag_usage_breakdown_buckets_like_the_period_one() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                tagged_charge("EC2", 10.0, 10, Some(r#"{"business_line":"etl"}"#)),
                tagged_charge("EC2", 4.0, 11, None),
                tagged_charge("S3", 3.0, 12, Some(r#"{"business_line":"search"}"#)),
                tagged_charge("EC2", 99.0, 25, Some(r#"{"business_line":"etl"}"#)), // out
                Charge {
                    charge_category: ChargeCategory::Credit,
                    ..tagged_charge("EC2", -7.0, 13, Some(r#"{"business_line":"etl"}"#))
                },
            ],
        );

        let breakdown =
            tag_usage_breakdown_between_of(&conn, at(10), at(20), "business_line", None).unwrap();
        assert_eq!(
            breakdown,
            vec![
                ("etl".to_string(), 10.0),
                ("Unallocated".to_string(), 4.0),
                ("search".to_string(), 3.0),
            ]
        );

        // The scoped variant sees only that service's window usage.
        let scoped = tag_usage_breakdown_between_of(
            &conn,
            at(10),
            at(20),
            "business_line",
            Some(("AWS", "EC2")),
        )
        .unwrap();
        assert_eq!(
            scoped,
            vec![("etl".to_string(), 10.0), ("Unallocated".to_string(), 4.0),]
        );
    }

    #[test]
    fn a_windowed_provider_service_usage_groups_orders_and_bounds() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 12.5, "USD", 10),
                charge("S3", 0.75, "USD", 11),
                charge("EC2", 50.0, "USD", 25), // out
            ],
        );
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 12)]);

        let rows = provider_service_usage_between_of(&conn, at(10), at(20)).unwrap();
        assert_eq!(rows.len(), 3);
        // Largest first, across providers, in the reporting currency.
        assert_eq!(rows[0].0, "Aliyun");
        assert_eq!(rows[0].1, "ECS");
        assert!((rows[0].2 - 99.968).abs() < 1e-6, "got {:?}", rows[0]);
        assert_eq!(rows[1], ("AWS".to_string(), "EC2".to_string(), 12.5));
        assert_eq!(rows[2], ("AWS".to_string(), "S3".to_string(), 0.75));
    }

    #[test]
    fn monthly_usage_groups_by_billing_period_oldest_first() {
        let mut conn = conn("USD");
        let jul = |d: u32| Utc.with_ymd_and_hms(2026, 7, d, 0, 0, 0).unwrap();
        let sep = |d: u32| Utc.with_ymd_and_hms(2026, 9, d, 0, 0, 0).unwrap();

        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-07"),
            &[charge_on("EC2", 10.0, "USD", jul(15))],
        );
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 5.0, "USD", 1),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-2.0),
                    ..charge("EC2", -2.0, "USD", 2)
                },
            ],
        );
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-09"),
            &[charge_on("EC2", 7.0, "USD", sep(2))],
        );

        // Ordered by period label; the credit is not usage.
        let monthly = monthly_usage_all_of(&conn, jul(1)).unwrap();
        assert_eq!(
            monthly,
            vec![
                ("2026-07".to_string(), 10.0),
                ("2026-08".to_string(), 5.0),
                ("2026-09".to_string(), 7.0),
            ]
        );

        // `since` bounds by charge time: mid-August drops the earlier months.
        assert_eq!(
            monthly_usage_all_of(&conn, at(15)).unwrap(),
            vec![("2026-09".to_string(), 7.0)]
        );
    }

    #[test]
    fn last_ingests_reports_each_account_once() {
        let mut conn = conn("USD");
        write(&mut conn, &aws(), &[charge("EC2", 12.5, "USD", 1)]);
        write(&mut conn, &aws(), &[charge("EC2", 13.0, "USD", 1)]);
        write(&mut conn, &aliyun(), &[charge("ECS", 710.0, "CNY", 1)]);

        let ingests = last_ingests_of(&conn).unwrap();
        assert_eq!(ingests.len(), 2);
        let aws = ingests
            .iter()
            .find(|(p, a, _)| p == "AWS" && a == "acct-1")
            .expect("the AWS account ingested");
        // The second write is fresher than the first, and is the one reported.
        assert!(aws.2 <= Utc::now());
    }

    #[test]
    fn api_fetches_this_month_counts_api_batches_only() {
        let mut conn = conn("USD");
        let this_month = BillingPeriod::containing(Utc::now()).label();
        let api = PeriodKey::new("AWS", "acct-1", &this_month);

        write(&mut conn, &api, &[charge("EC2", 1.0, "USD", 1)]);
        // A second fetch of the same period was still a paid call.
        write(&mut conn, &api, &[charge("EC2", 1.0, "USD", 1)]);
        write_through(
            &mut conn,
            &api,
            &[charge("EC2", 1.0, "USD", 1)],
            Channel::File,
        );
        // Another month does not count.
        write(&mut conn, &aws(), &[charge("EC2", 1.0, "USD", 1)]);

        assert_eq!(api_fetches_this_month_of(&conn).unwrap(), 2);
    }

    #[test]
    fn balance_burn_is_the_mean_of_the_drops() {
        let mut conn = conn("USD");
        let now = Utc::now();
        let snapshot = |days_ago: i64, balance: f64| BalanceSnapshot {
            provider: "DeepSeek".to_string(),
            account_id: "acct-3".to_string(),
            observed_at: now - chrono::Duration::days(days_ago),
            balance,
            granted_balance: None,
            topped_up_balance: Some(balance),
            currency: "CNY".to_string(),
        };

        crate::ledger::write_balance(&mut conn, &snapshot(6, 100.0)).unwrap();
        crate::ledger::write_balance(&mut conn, &snapshot(4, 90.0)).unwrap();
        // A rise is a top-up, not consumption.
        crate::ledger::write_balance(&mut conn, &snapshot(3, 140.0)).unwrap();
        crate::ledger::write_balance(&mut conn, &snapshot(1, 132.0)).unwrap();

        // (100-90) + (140-132) = 18 over 7 days.
        let burn = balance_burn_of(&conn, "DeepSeek", "acct-3", 7).unwrap();
        assert!((burn.unwrap() - 18.0 / 7.0).abs() < 1e-9);

        // One observation says nothing about burn.
        assert!(balance_burn_of(&conn, "DeepSeek", "unknown", 7)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_plain_select_and_a_cte_are_read_only() {
        assert!(is_read_only("SELECT 1"));
        assert!(is_read_only("  with x as (select 1) select * from x"));
        assert!(is_read_only("EXPLAIN SELECT 1"));
        assert!(is_read_only("SUMMARIZE fct_charge"));
        assert!(is_read_only("VALUES (1, 2)"));
    }

    #[test]
    fn an_insert_inside_a_cte_is_not_read_only() {
        assert!(!is_read_only(
            "WITH x AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM x"
        ));
    }

    #[test]
    fn a_second_statement_after_the_semicolon_is_scanned_too() {
        assert!(!is_read_only("SELECT 1; DROP TABLE t"));
    }

    #[test]
    fn keywords_in_comments_do_not_count() {
        assert!(is_read_only("SELECT 1 -- DROP TABLE t"));
        assert!(is_read_only("/* DROP TABLE t */ SELECT 1"));
        assert!(is_read_only("-- INSERT INTO t\nSELECT 1"));
    }

    #[test]
    fn keywords_in_string_literals_do_not_count() {
        assert!(is_read_only("SELECT 'DROP TABLE t'"));
        assert!(is_read_only("SELECT '-- delete'"));
        // An escaped quote does not end the literal early.
        assert!(is_read_only("SELECT 'it''s a DROP'"));
        // ...but the word after a genuinely closed literal is scanned.
        assert!(!is_read_only("SELECT 'it''s fine'; DROP TABLE t"));
    }

    #[test]
    fn anything_not_starting_with_a_read_keyword_is_rejected() {
        assert!(!is_read_only("PRAGMA database_list"));
        assert!(!is_read_only(""));
        assert!(!is_read_only("   -- just a comment"));
    }

    #[test]
    fn the_guard_error_is_explicit() {
        let conn = conn("USD");
        let error = run_adhoc_of(&conn, "DROP TABLE fct_charge").unwrap_err();
        assert!(error.to_string().contains("read-only"), "got {error}");
    }

    #[test]
    fn the_row_budget_is_rows_or_cells_whichever_bites_first() {
        let conn = conn("USD");

        // 10 rows available; a row budget of 3 stops at 3 and says so.
        let result = run_adhoc_within(&conn, "SELECT * FROM range(10)", 3, usize::MAX / 2).unwrap();
        assert_eq!(result.rows.len(), 3);
        assert!(result.truncated);

        // A wide query hits the cell budget: 4 cells/row out of a 9-cell
        // budget allows 2 rows.
        let wide = run_adhoc_within(
            &conn,
            "SELECT i, i, i, i FROM (SELECT * FROM range(10)) AS t(i)",
            usize::MAX / 2,
            9,
        )
        .unwrap();
        assert_eq!(wide.rows.len(), 2);
        assert!(wide.truncated);

        // Exactly budget-many rows is not truncated.
        let exact = run_adhoc_within(&conn, "SELECT * FROM range(3)", 3, usize::MAX / 2).unwrap();
        assert_eq!(exact.rows.len(), 3);
        assert!(!exact.truncated);
    }

    #[test]
    fn an_adhoc_result_is_typed_and_rendered() {
        let conn = conn("USD");

        let result = run_adhoc_of(
            &conn,
            "SELECT 42 AS n, 'x' AS s, NULL AS missing, 1.5 AS f, DATE '2026-08-01' AS d",
        )
        .unwrap();

        assert_eq!(result.columns, vec!["n", "s", "missing", "f", "d"]);
        // `missing` has no non-NULL value, so nothing to right-align by.
        assert_eq!(result.numeric, vec![true, false, false, true, false]);
        assert_eq!(
            result.rows,
            vec![vec![
                Some("42".to_string()),
                Some("x".to_string()),
                None,
                Some("1.5".to_string()),
                Some("2026-08-01".to_string()),
            ]]
        );
        assert!(!result.truncated);

        // The column keeps the type of its first non-NULL value.
        let mixed = run_adhoc_of(&conn, "SELECT * FROM (VALUES (NULL), (7)) AS t(v)").unwrap();
        assert_eq!(mixed.numeric, vec![true]);
        assert_eq!(mixed.rows[0], vec![None]);
        assert_eq!(mixed.rows[1], vec![Some("7".to_string())]);
    }

    fn midday(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, 12, 0, 0).unwrap()
    }

    #[test]
    fn the_forecast_is_the_run_rate_plus_what_already_landed() {
        let mut conn = conn("USD");
        // $10 on each of the first twelve days of August.
        let charges: Vec<Charge> = (1..=12)
            .map(|day| charge("EC2", 10.0, "USD", day))
            .collect();
        write(&mut conn, &aws(), &charges);

        let forecast = forecast_for_period_of(&conn, "2026-08", midday(12)).unwrap();
        assert!((forecast.month_to_date - 120.0).abs() < 1e-9);
        assert!((forecast.daily_rate - 10.0).abs() < 1e-9);
        assert_eq!(forecast.days_elapsed, 12);
        assert_eq!(forecast.days_in_month, 31);
        // 120 + 10 * 19 remaining days.
        assert!((forecast.forecast - 310.0).abs() < 1e-9);
    }

    /// The rate is measured from the first expense day, not the period
    /// start: an account that landed its first charge on the 10th is not
    /// averaged over the nine days it was not running.
    #[test]
    fn the_forecast_skips_the_cold_start_before_the_first_expense() {
        let mut conn = conn("USD");
        let charges: Vec<Charge> = (10..=12)
            .map(|day| charge("EC2", 10.0, "USD", day))
            .collect();
        write(&mut conn, &aws(), &charges);

        let forecast = forecast_for_period_of(&conn, "2026-08", midday(12)).unwrap();
        assert!((forecast.month_to_date - 30.0).abs() < 1e-9);
        // 30 over 3 days since the first expense, not 30 over 12 elapsed.
        assert!(
            (forecast.daily_rate - 10.0).abs() < 1e-9,
            "got {forecast:?}"
        );
        assert!((forecast.forecast - 220.0).abs() < 1e-9);
    }

    #[test]
    fn a_past_period_forecasts_its_own_total() {
        let mut conn = conn("USD");
        let jul = |d: u32| Utc.with_ymd_and_hms(2026, 7, d, 0, 0, 0).unwrap();
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-07"),
            &[charge_on("EC2", 50.0, "USD", jul(3))],
        );

        let forecast = forecast_for_period_of(&conn, "2026-07", midday(12)).unwrap();
        assert_eq!(forecast.days_elapsed, 31);
        assert_eq!(forecast.days_in_month, 31);
        assert!((forecast.month_to_date - 50.0).abs() < 1e-9);
        assert!((forecast.forecast - 50.0).abs() < 1e-9);
    }

    #[test]
    fn a_period_with_no_charges_forecasts_zero() {
        let conn = conn("USD");

        let forecast = forecast_for_period_of(&conn, "2026-08", midday(12)).unwrap();
        assert_eq!(forecast.month_to_date, 0.0);
        assert_eq!(forecast.daily_rate, 0.0);
        assert_eq!(forecast.forecast, 0.0);
        assert_eq!(forecast.days_elapsed, 12);
        assert_eq!(forecast.days_in_month, 31);
    }

    #[test]
    fn period_over_period_reads_both_periods_in_one_pass() {
        let mut conn = conn("USD");
        let jul = |d: u32| Utc.with_ymd_and_hms(2026, 7, d, 0, 0, 0).unwrap();
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-07"),
            &[
                charge_on("EC2", 100.0, "USD", jul(3)),
                charge_on("S3", 20.0, "USD", jul(4)),
            ],
        );
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 120.0, "USD", 1),
                // Net-negative: in the total, out of the breakdown.
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-5.0),
                    ..charge("Refunded", -5.0, "USD", 2)
                },
            ],
        );

        let pop = period_over_period_of(&conn, "2026-08").unwrap();
        assert!((pop.current_total - 115.0).abs() < 1e-9, "got {pop:?}");
        assert!((pop.previous_total - 120.0).abs() < 1e-9);
        assert_eq!(pop.current_by_service, vec![("EC2".to_string(), 120.0)]);
        assert_eq!(
            pop.previous_by_service,
            vec![("EC2".to_string(), 100.0), ("S3".to_string(), 20.0)]
        );
    }

    #[test]
    fn breakdown_by_groups_on_each_stored_dimension() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                Charge {
                    region_id: Some("us-east-1".to_string()),
                    service_category: Some("Compute".to_string()),
                    ..charge("EC2", 12.5, "USD", 1)
                },
                Charge {
                    service_category: Some("Storage".to_string()),
                    ..charge("S3", 0.75, "USD", 1)
                },
                Charge {
                    region_id: Some("us-west-2".to_string()),
                    service_category: Some("Compute".to_string()),
                    ..charge("EC2", 4.0, "USD", 2)
                },
            ],
        );

        // Service is the breakdown the app already had.
        assert_eq!(
            breakdown_by_of(&conn, &aws(), BreakdownDim::Service).unwrap(),
            service_breakdown_of(&conn, &aws()).unwrap()
        );
        // A charge with no region reads as 'Other'.
        assert_eq!(
            breakdown_by_of(&conn, &aws(), BreakdownDim::Region).unwrap(),
            vec![
                ("us-east-1".to_string(), 12.5),
                ("us-west-2".to_string(), 4.0),
                ("Other".to_string(), 0.75),
            ]
        );
        assert_eq!(
            breakdown_by_of(&conn, &aws(), BreakdownDim::ServiceCategory).unwrap(),
            vec![("Compute".to_string(), 16.5), ("Storage".to_string(), 0.75),]
        );
    }

    #[test]
    fn top_resources_ranks_resources_and_skips_unresourced_charges() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                Charge {
                    resource_id: Some("i-1".to_string()),
                    resource_name: Some("web".to_string()),
                    ..charge("EC2", 10.0, "USD", 1)
                },
                // Same resource, second charge: one row of 15.
                Charge {
                    resource_id: Some("i-1".to_string()),
                    resource_name: Some("web".to_string()),
                    ..charge("EC2", 5.0, "USD", 2)
                },
                Charge {
                    resource_id: Some("i-2".to_string()),
                    ..charge("EC2", 8.0, "USD", 1)
                },
                // No resource id: cannot be a top resource.
                charge("S3", 99.0, "USD", 1),
            ],
        );

        let top = top_resources_of(&conn, &aws(), 10).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].resource_id, "i-1");
        assert_eq!(top[0].resource_name.as_deref(), Some("web"));
        assert_eq!(top[0].service, "EC2");
        assert!((top[0].amount - 15.0).abs() < 1e-9);
        assert_eq!(top[1].resource_id, "i-2");
        assert_eq!(top[1].resource_name, None);

        let one = top_resources_of(&conn, &aws(), 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].resource_id, "i-1");
    }

    /// One grouped round-trip carries what the attribution page used to
    /// fetch per service: filtering its rows for one `(provider, service)`
    /// gives exactly that service's breakdown.
    #[test]
    fn the_grouped_tag_breakdown_matches_the_per_service_queries() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                tagged_charge("EC2", 10.0, 1, Some(r#"{"business_line":"etl"}"#)),
                tagged_charge("EC2", 4.0, 1, None),
                tagged_charge("S3", 3.0, 1, Some(r#"{"business_line":"search"}"#)),
                Charge {
                    charge_category: ChargeCategory::Credit,
                    ..tagged_charge("EC2", -2.0, 1, Some(r#"{"business_line":"etl"}"#))
                },
            ],
        );
        write(
            &mut conn,
            &aliyun(),
            &[tagged_charge(
                "ECS",
                710.0,
                1,
                Some(r#"{"business_line":"etl"}"#),
            )],
        );

        let rows = tag_usage_breakdown_by_service_of(&conn, "2026-08", "business_line").unwrap();
        // Largest first, across providers.
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].provider, "Aliyun");
        assert_eq!(rows[0].service, "ECS");
        assert!((rows[0].amount - 710.0).abs() < 1e-9, "got {rows:?}");

        let of = |provider: &str, service: &str| {
            rows.iter()
                .filter(|row| row.provider == provider && row.service == service)
                .map(|row| (row.tag_value.clone(), row.amount))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            of("AWS", "EC2"),
            tag_usage_breakdown_of(&conn, "2026-08", "business_line", Some(("AWS", "EC2")))
                .unwrap()
        );
        assert_eq!(
            of("AWS", "S3"),
            tag_usage_breakdown_of(&conn, "2026-08", "business_line", Some(("AWS", "S3"))).unwrap()
        );
        assert_eq!(of("AWS", "S3"), vec![("search".to_string(), 3.0)]);
    }

    /// The per-account and usage-only variants are the cross-account query
    /// under a narrower scope; pin that directly on the builder.
    #[test]
    fn a_scope_combines_account_window_and_category_filters() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 2.0, "USD", 10),
                charge("EC2", 8.0, "USD", 20), // at `until`: out
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-1.0),
                    ..charge("EC2", -1.0, "USD", 12)
                },
            ],
        );
        write(&mut conn, &aliyun(), &[charge("ECS", 100.0, "USD", 12)]);

        let account = Scope {
            provider: Some("AWS"),
            account_id: Some("acct-1"),
            since: Some(at(10)),
            until: Some(at(20)),
            ..Default::default()
        };
        assert!((sum_total(&conn, &account).unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(sum_usage_and_credits(&conn, &account).unwrap(), (2.0, -1.0));

        let usage_only = Scope {
            usage_only: true,
            ..account.clone()
        };
        assert!((sum_total(&conn, &usage_only).unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(
            sum_by_day(&conn, &usage_only).unwrap(),
            vec![("2026-08-10".to_string(), 2.0)]
        );
        assert_eq!(
            sum_by_bucket(&conn, &account, "coalesce(service_name, 'Other')").unwrap(),
            vec![("EC2".to_string(), 1.0)]
        );
    }

    /// Both partitions — category and service — must add back up to the
    /// delta they explain, on data written by the usual code path.
    #[test]
    fn the_decomposition_explains_the_delta_and_reconciles() {
        let mut conn = conn("USD");
        let jul = |d: u32| Utc.with_ymd_and_hms(2026, 7, d, 0, 0, 0).unwrap();
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-07"),
            &[
                charge_on("EC2", 100.0, "USD", jul(3)),
                charge_on("S3", 20.0, "USD", jul(4)),
                charge_on("RDS", 10.0, "USD", jul(5)),
                Charge {
                    charge_category: ChargeCategory::Tax,
                    billed_cost: Some(5.0),
                    ..charge_on("Tax", 5.0, "USD", jul(6))
                },
            ],
        );
        write(
            &mut conn,
            &aws(),
            &[
                charge("EC2", 130.0, "USD", 1),
                charge("Lambda", 40.0, "USD", 2),
                Charge {
                    charge_category: ChargeCategory::Tax,
                    billed_cost: Some(8.0),
                    ..charge("Tax", 8.0, "USD", 3)
                },
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-15.0),
                    ..charge("EC2", -15.0, "USD", 4)
                },
            ],
        );

        let d = cost_change_decomposition_of(&conn, "2026-08").unwrap();
        assert_eq!(d.billing_period, "2026-08");
        assert_eq!(d.previous_period, "2026-07");
        assert!((d.current_total - 163.0).abs() < 1e-9);
        assert!((d.previous_total - 135.0).abs() < 1e-9);
        assert!((d.total_delta - 28.0).abs() < 1e-9);
        assert!(d.reconciled, "got {d:?}");
        assert!(d.residual.abs() < 1e-9, "got {d:?}");

        // Each partition on its own sums to the total delta.
        let category_sum: f64 = d.by_category.iter().map(|c| c.delta).sum();
        assert!((category_sum - d.total_delta).abs() < 1e-9);
        let service_sum: f64 = d.by_service.iter().map(|s| s.delta).sum();
        assert!((service_sum - d.total_delta).abs() < 1e-9);

        let usage = d
            .by_category
            .iter()
            .find(|c| c.category == "Usage")
            .expect("a Usage component");
        assert!((usage.current - 170.0).abs() < 1e-9);
        assert!((usage.previous - 130.0).abs() < 1e-9);
        assert!((usage.delta - 40.0).abs() < 1e-9);

        // Largest absolute first: Lambda's 40 appeared out of nothing.
        assert_eq!(d.by_service[0].service, "Lambda");
        assert_eq!(d.by_service[0].kind, MovementKind::Appeared);
        let movement = |service: &str| {
            d.by_service
                .iter()
                .find(|s| s.service == service)
                .unwrap_or_else(|| panic!("a movement for {service}"))
        };
        // The EC2 credit lands in EC2's movement: 130 − 15 vs 100.
        assert_eq!(movement("EC2").kind, MovementKind::Grown);
        assert!((movement("EC2").delta - 15.0).abs() < 1e-9);
        assert_eq!(movement("S3").kind, MovementKind::Vanished);
        assert!((movement("S3").delta - -20.0).abs() < 1e-9);
        assert_eq!(movement("RDS").kind, MovementKind::Vanished);
    }

    #[test]
    fn bands_open_around_the_expected_forecast() {
        let mut conn = conn("USD");
        // Alternating $8/$12 days: mean 10, sample variance 48/11.
        let charges: Vec<Charge> = (1..=12)
            .map(|day| charge("EC2", if day % 2 == 0 { 12.0 } else { 8.0 }, "USD", day))
            .collect();
        write(&mut conn, &aws(), &charges);

        let bands = forecast_bands_for_period_of(&conn, "2026-08", midday(12)).unwrap();
        let stddev = (48.0_f64 / 11.0).sqrt();
        assert!((bands.month_to_date - 120.0).abs() < 1e-9);
        // The mean agrees with the forecast's daily rate.
        assert!((bands.daily_mean - 10.0).abs() < 1e-9);
        assert!((bands.daily_stddev - stddev).abs() < 1e-9);
        assert!((bands.expected - 310.0).abs() < 1e-9);
        // 19 remaining days, one standard deviation each way.
        assert!((bands.optimistic - (120.0 + (10.0 + stddev) * 19.0)).abs() < 1e-9);
        assert!((bands.pessimistic - (120.0 + (10.0 - stddev) * 19.0)).abs() < 1e-9);
        assert!(bands.optimistic > bands.expected);
        assert!(bands.expected > bands.pessimistic);
    }

    #[test]
    fn bands_with_fewer_than_two_days_of_data_collapse_onto_the_forecast() {
        let mut conn = conn("USD");
        write(&mut conn, &aws(), &[charge("EC2", 50.0, "USD", 12)]);

        // A single day since the baseline: no standard deviation, no band.
        let bands = forecast_bands_for_period_of(&conn, "2026-08", midday(12)).unwrap();
        assert!((bands.month_to_date - 50.0).abs() < 1e-9);
        assert!((bands.daily_mean - 50.0).abs() < 1e-9);
        assert_eq!(bands.daily_stddev, 0.0);
        // 50 + 50 * 19 remaining days.
        assert!((bands.expected - 1000.0).abs() < 1e-9);
        assert_eq!(bands.optimistic, bands.expected);
        assert_eq!(bands.pessimistic, bands.expected);

        // No data at all: everything zero.
        let empty = forecast_bands_for_period_of(&conn, "2026-09", midday(12)).unwrap();
        assert_eq!(empty.month_to_date, 0.0);
        assert_eq!(empty.expected, 0.0);
        assert_eq!(empty.optimistic, 0.0);
        assert_eq!(empty.pessimistic, 0.0);
    }

    #[test]
    fn the_trailing_average_is_a_flat_typical_day_that_excludes_the_current_month() {
        let mut conn = conn("USD");
        let on = |month: u32, day: u32| Utc.with_ymd_and_hms(2026, month, day, 0, 0, 0).unwrap();
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-06"),
            &[charge_on("EC2", 60.0, "USD", on(6, 10))],
        );
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-07"),
            &[
                charge_on("EC2", 93.0, "USD", on(7, 10)),
                // A credit is not usage: the typical day is unchanged.
                Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(-50.0),
                    ..charge_on("EC2", -50.0, "USD", on(7, 11))
                },
            ],
        );
        write(&mut conn, &aws(), &[charge("EC2", 999.0, "USD", 1)]);

        let series = trailing_daily_average_of(&conn, 5, 2, midday(12)).unwrap();
        assert_eq!(series.len(), 5);
        assert_eq!(series[0].0, "2026-08-08");
        assert_eq!(series[4].0, "2026-08-12");
        // June + July usage over 61 calendar days; August's 999 is out.
        let typical = 153.0 / 61.0;
        for (_, value) in &series {
            assert!((value - typical).abs() < 1e-9, "got {series:?}");
        }

        assert!(trailing_daily_average_of(&conn, 0, 2, midday(12))
            .unwrap()
            .is_empty());
        assert!(trailing_daily_average_of(&conn, 5, 0, midday(12))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_trailing_average_window_slides_at_a_month_boundary() {
        let mut conn = conn("USD");
        let on = |month: u32, day: u32| Utc.with_ymd_and_hms(2026, month, day, 0, 0, 0).unwrap();
        // May (31 days) totals 31, June (30) 60, July (31) 93.
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-05"),
            &[charge_on("EC2", 31.0, "USD", on(5, 10))],
        );
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-06"),
            &[charge_on("EC2", 60.0, "USD", on(6, 10))],
        );
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-07"),
            &[charge_on("EC2", 93.0, "USD", on(7, 10))],
        );

        let now = Utc.with_ymd_and_hms(2026, 8, 2, 12, 0, 0).unwrap();
        let series = trailing_daily_average_of(&conn, 4, 2, now).unwrap();
        // The July days compare against May + June (91 over 61 days)...
        assert_eq!(series[0].0, "2026-07-30");
        assert!((series[0].1 - 91.0 / 61.0).abs() < 1e-9, "got {series:?}");
        assert_eq!(series[1].0, "2026-07-31");
        assert!((series[1].1 - 91.0 / 61.0).abs() < 1e-9);
        // ...the August days against June + July (153 over 61 days).
        assert_eq!(series[2].0, "2026-08-01");
        assert!((series[2].1 - 153.0 / 61.0).abs() < 1e-9);
        assert!((series[3].1 - 153.0 / 61.0).abs() < 1e-9);
    }

    #[test]
    fn a_clean_period_raises_no_data_quality_issues() {
        let mut conn = conn("USD");
        let clean = |service: &str, amount: f64, day: u32| Charge {
            region_id: Some("us-east-1".to_string()),
            ..tagged_charge(service, amount, day, Some(r#"{"business_line":"etl"}"#))
        };
        write(
            &mut conn,
            &aws(),
            &[clean("EC2", 100.0, 1), clean("S3", 10.0, 2)],
        );

        assert!(data_quality_issues_of(&conn, "2026-08", "business_line")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unconverted_charges_are_flagged_with_their_row_share() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                Charge {
                    region_id: Some("us-east-1".to_string()),
                    ..tagged_charge("EC2", 100.0, 1, Some(r#"{"business_line":"etl"}"#))
                },
                Charge {
                    region_id: Some("us-east-1".to_string()),
                    tags: Some(r#"{"business_line":"etl"}"#.to_string()),
                    ..charge("Something", 100.0, "JPY", 1)
                },
            ],
        );

        let issues = data_quality_issues_of(&conn, "2026-08", "business_line").unwrap();
        assert_eq!(issues.len(), 1);
        let issue = &issues[0];
        assert_eq!(issue.kind, DataQualityKind::UnconvertedCharges);
        assert_eq!(issue.severity, IssueSeverity::Warning);
        // The amount stays in a currency that cannot be summed.
        assert_eq!(issue.affected_amount, None);
        assert_eq!(issue.affected_count, 1);
        assert!(issue.message.contains("50.0%"), "got {}", issue.message);
    }

    #[test]
    fn untagged_usage_warns_above_a_fifth_of_the_period() {
        let mut conn = conn("USD");
        let regioned = |c: Charge| Charge {
            region_id: Some("us-east-1".to_string()),
            ..c
        };
        write(
            &mut conn,
            &aws(),
            &[
                regioned(tagged_charge(
                    "EC2",
                    100.0,
                    1,
                    Some(r#"{"business_line":"etl"}"#),
                )),
                regioned(tagged_charge("NAT", 30.0, 2, None)),
            ],
        );

        // 30 of 130 = 23.1%: over the warning line.
        let issues = data_quality_issues_of(&conn, "2026-08", "business_line").unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, DataQualityKind::UntaggedUsage);
        assert_eq!(issues[0].severity, IssueSeverity::Warning);
        assert_eq!(issues[0].affected_amount, Some(30.0));
        assert_eq!(issues[0].affected_count, 1);
        assert!(issues[0].message.contains("business_line"));
        assert!(
            issues[0].message.contains("23.1%"),
            "got {}",
            issues[0].message
        );

        // Below the line the same finding is only a note: 10 of 110 = 9.1%.
        let jul = |d: u32| Utc.with_ymd_and_hms(2026, 7, d, 0, 0, 0).unwrap();
        write(
            &mut conn,
            &PeriodKey::new("AWS", "acct-1", "2026-07"),
            &[
                regioned(Charge {
                    tags: Some(r#"{"business_line":"etl"}"#.to_string()),
                    ..charge_on("EC2", 100.0, "USD", jul(3))
                }),
                regioned(charge_on("NAT", 10.0, "USD", jul(4))),
            ],
        );
        let issues = data_quality_issues_of(&conn, "2026-07", "business_line").unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, DataQualityKind::UntaggedUsage);
        assert_eq!(issues[0].severity, IssueSeverity::Info);
    }

    #[test]
    fn a_service_whose_usage_lacks_a_region_is_flagged_by_share() {
        let mut conn = conn("USD");
        let tagged = |c: Charge| Charge {
            tags: Some(r#"{"business_line":"etl"}"#.to_string()),
            ..c
        };
        write(
            &mut conn,
            &aws(),
            &[
                tagged(Charge {
                    region_id: Some("us-east-1".to_string()),
                    ..charge("EC2", 100.0, "USD", 1)
                }),
                tagged(charge("Lambda", 60.0, "USD", 2)), // 30% of usage
                tagged(charge("S3", 40.0, "USD", 3)),     // 20% of usage
            ],
        );

        let issues = data_quality_issues_of(&conn, "2026-08", "business_line").unwrap();
        assert_eq!(issues.len(), 2);
        // Largest first; 30% is over the warning line, 20% is a note.
        assert_eq!(issues[0].kind, DataQualityKind::MissingRegion);
        assert_eq!(issues[0].severity, IssueSeverity::Warning);
        assert!(
            issues[0].message.contains("Lambda"),
            "got {}",
            issues[0].message
        );
        assert_eq!(issues[0].affected_amount, Some(60.0));
        assert_eq!(issues[1].kind, DataQualityKind::MissingRegion);
        assert_eq!(issues[1].severity, IssueSeverity::Info);
        assert!(
            issues[1].message.contains("S3"),
            "got {}",
            issues[1].message
        );
    }

    #[test]
    fn unreconciled_adjustments_are_critical() {
        let mut conn = conn("USD");
        write(
            &mut conn,
            &aws(),
            &[
                Charge {
                    region_id: Some("us-east-1".to_string()),
                    ..tagged_charge("ECS", 100.0, 1, Some(r#"{"business_line":"etl"}"#))
                },
                Charge {
                    charge_category: ChargeCategory::Adjustment,
                    charge_description: Some(crate::cloud::deduction::UNRECONCILED.to_string()),
                    billed_cost: Some(-8.0),
                    ..charge("ECS", -8.0, "USD", 2)
                },
            ],
        );

        let issues = data_quality_issues_of(&conn, "2026-08", "business_line").unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, DataQualityKind::UnreconciledAdjustment);
        assert_eq!(issues[0].severity, IssueSeverity::Critical);
        assert_eq!(issues[0].affected_amount, Some(-8.0));
        assert_eq!(issues[0].affected_count, 1);
        assert!(issues[0].message.contains("Unreconciled"));
    }
}
