//! Reading the ledger.
//!
//! Everything the UI shows comes through [`schema::NORMALIZED_VIEW`], so
//! amounts arrive already expressed in the reporting currency. Nothing in
//! here adds up two currencies.

use anyhow::Result;
use chrono::{DateTime, Utc};
use duckdb::{params, Connection};

use super::schema::{NORMALIZED_VIEW, TIMESTAMP_FORMAT};
use super::{with_connection_ref, Channel, PeriodKey};

/// The most recent balance a source reported for an account.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub balance: f64,
    pub granted_balance: Option<f64>,
    pub topped_up_balance: Option<f64>,
    /// The currency the source reports in, which is not converted: a
    /// balance is what is left in an account, not an amount spent.
    pub currency: String,
    pub observed_at: DateTime<Utc>,
}

/// One day's charges, as `(YYYY-MM-DD, amount)` in the reporting currency.
pub type DailyTotal = (String, f64);

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
    let mut stmt = conn.prepare(&format!(
        "SELECT strftime(charge_period_start, '%Y-%m-%d') AS day, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE charge_period_start >= CAST(? AS TIMESTAMP)
         GROUP BY day
         ORDER BY day"
    ))?;

    let rows = stmt
        .query_map(params![since.format(TIMESTAMP_FORMAT).to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(day, amount)| (day, amount.unwrap_or(0.0)))
        .collect())
}

/// One service's charges on one day, in the reporting currency.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceDailyTotal {
    pub provider: String,
    /// `coalesce(service_name, 'Other')`, as everywhere a service is grouped.
    pub service: String,
    /// `YYYY-MM-DD`.
    pub day: String,
    pub amount: f64,
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

/// Charges of one period grouped by `(provider, service)`, largest first.
pub fn provider_service_totals(billing_period: &str) -> Result<Vec<(String, String, f64)>> {
    with_connection_ref(|conn| provider_service_totals_of(conn, billing_period))
}

fn provider_service_totals_of(
    conn: &Connection,
    billing_period: &str,
) -> Result<Vec<(String, String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, coalesce(service_name, 'Other') AS service,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE billing_period = ?
         GROUP BY provider, service
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let rows = stmt
        .query_map(params![billing_period], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
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
    let (scope_sql, scope_params): (&str, Vec<String>) = match scope {
        Some((provider, service)) => (
            "AND provider = ? AND coalesce(service_name, 'Other') = ?",
            vec![provider.to_string(), service.to_string()],
        ),
        None => ("", Vec::new()),
    };

    let mut stmt = conn.prepare(&format!(
        "SELECT coalesce(nullif(json_extract_string(tags, ?), ''), 'Unallocated') AS tag_value,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE billing_period = ? {scope_sql}
         GROUP BY tag_value
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let mut bound: Vec<String> = vec![tag_key.to_string(), billing_period.to_string()];
    bound.extend(scope_params);

    let rows = stmt
        .query_map(duckdb::params_from_iter(bound.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// How many charges could not be converted, because no rate covers their
/// currency. They are missing from every converted total.
pub fn unconverted_charges(billing_period: &str) -> Result<i64> {
    with_connection_ref(|conn| unconverted_charges_of(conn, billing_period))
}

/// One of the largest charges of a period that carries no value for a tag.
#[derive(Debug, Clone, PartialEq)]
pub struct UntaggedCharge {
    pub provider: String,
    pub service: Option<String>,
    pub description: Option<String>,
    /// In the reporting currency.
    pub amount: f64,
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
    let (usage, credits): (Option<f64>, Option<f64>) = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base) FILTER (WHERE charge_category = 'Usage'),
                    sum(billed_cost_base) FILTER (WHERE charge_category IN ('Credit', 'Adjustment'))
             FROM {NORMALIZED_VIEW}
             WHERE billing_period = ?"
        ),
        params![billing_period],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    Ok((usage.unwrap_or(0.0), credits.unwrap_or(0.0)))
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
    let (usage, credits): (Option<f64>, Option<f64>) = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base) FILTER (WHERE charge_category = 'Usage'),
                    sum(billed_cost_base) FILTER (WHERE charge_category IN ('Credit', 'Adjustment'))
             FROM {NORMALIZED_VIEW}
             WHERE charge_period_start >= CAST(? AS TIMESTAMP)
               AND charge_period_start < CAST(? AS TIMESTAMP)"
        ),
        params![
            since.format(TIMESTAMP_FORMAT).to_string(),
            until.format(TIMESTAMP_FORMAT).to_string()
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    Ok((usage.unwrap_or(0.0), credits.unwrap_or(0.0)))
}

/// Net total charged in a charge-time window `[since, until)`, across
/// every account and charge category — the rolling-range counterpart of
/// [`total_for_period`].
pub fn total_between(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<f64> {
    with_connection_ref(|conn| total_between_of(conn, since, until))
}

fn total_between_of(conn: &Connection, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<f64> {
    let total: Option<f64> = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base) FROM {NORMALIZED_VIEW}
             WHERE charge_period_start >= CAST(? AS TIMESTAMP)
               AND charge_period_start < CAST(? AS TIMESTAMP)"
        ),
        params![
            since.format(TIMESTAMP_FORMAT).to_string(),
            until.format(TIMESTAMP_FORMAT).to_string()
        ],
        |row| row.get(0),
    )?;

    Ok(total.unwrap_or(0.0))
}

/// Usage totals per billing period since an instant, as `(YYYY-MM,
/// amount)` ordered by period label — the 12-month Overview chart's
/// series. Grouped by `billing_period` rather than by charge-time month
/// so the buckets are the same months the rest of the app reasons about.
pub fn monthly_usage(since: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
    with_connection_ref(|conn| monthly_usage_of(conn, since))
}

fn monthly_usage_of(conn: &Connection, since: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT billing_period, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE charge_period_start >= CAST(? AS TIMESTAMP)
           AND charge_category = 'Usage'
         GROUP BY billing_period
         ORDER BY billing_period"
    ))?;

    let rows = stmt
        .query_map(params![since.format(TIMESTAMP_FORMAT).to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(period, amount)| (period, amount.unwrap_or(0.0)))
        .collect())
}

/// Daily usage totals across every provider and account since an instant,
/// oldest first — like [`daily_totals_all`], but Usage rows only, so a
/// credit landing on one day does not dip the series below what was
/// actually consumed.
pub fn daily_usage_all(since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    with_connection_ref(|conn| daily_usage_all_of(conn, since))
}

fn daily_usage_all_of(conn: &Connection, since: DateTime<Utc>) -> Result<Vec<DailyTotal>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT strftime(charge_period_start, '%Y-%m-%d') AS day, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE charge_period_start >= CAST(? AS TIMESTAMP)
           AND charge_category = 'Usage'
         GROUP BY day
         ORDER BY day"
    ))?;

    let rows = stmt
        .query_map(params![since.format(TIMESTAMP_FORMAT).to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(day, amount)| (day, amount.unwrap_or(0.0)))
        .collect())
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
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, coalesce(service_name, 'Other') AS service,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE billing_period = ?
           AND charge_category = 'Usage'
         GROUP BY provider, service
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let rows = stmt
        .query_map(params![billing_period], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
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
    let mut stmt = conn.prepare(&format!(
        "SELECT provider, coalesce(service_name, 'Other') AS service,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE charge_period_start >= CAST(? AS TIMESTAMP)
           AND charge_period_start < CAST(? AS TIMESTAMP)
           AND charge_category = 'Usage'
         GROUP BY provider, service
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let rows = stmt
        .query_map(
            params![
                since.format(TIMESTAMP_FORMAT).to_string(),
                until.format(TIMESTAMP_FORMAT).to_string()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
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
    tag_usage_breakdown_query(
        conn,
        tag_key,
        "billing_period = ?",
        &[billing_period.to_string()],
        scope,
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
    tag_usage_breakdown_query(
        conn,
        tag_key,
        "charge_period_start >= CAST(? AS TIMESTAMP) AND charge_period_start < CAST(? AS TIMESTAMP)",
        &[
            since.format(TIMESTAMP_FORMAT).to_string(),
            until.format(TIMESTAMP_FORMAT).to_string(),
        ],
        scope,
    )
}

/// The one tag-breakdown query behind the period-keyed and window-bounded
/// variants: `window_sql` is the WHERE fragment that bounds the charges,
/// bound after `tag_key` and before the optional `(provider, service)`
/// scope.
fn tag_usage_breakdown_query(
    conn: &Connection,
    tag_key: &str,
    window_sql: &str,
    window_params: &[String],
    scope: Option<(&str, &str)>,
) -> Result<Vec<(String, f64)>> {
    let (scope_sql, scope_params): (&str, Vec<String>) = match scope {
        Some((provider, service)) => (
            "AND provider = ? AND coalesce(service_name, 'Other') = ?",
            vec![provider.to_string(), service.to_string()],
        ),
        None => ("", Vec::new()),
    };

    let mut stmt = conn.prepare(&format!(
        "SELECT coalesce(nullif(json_extract_string(tags, ?), ''), 'Unallocated') AS tag_value,
                sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE {window_sql} {scope_sql}
           AND charge_category = 'Usage'
         GROUP BY tag_value
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let mut bound: Vec<String> = vec![tag_key.to_string()];
    bound.extend(window_params.iter().cloned());
    bound.extend(scope_params);

    let rows = stmt
        .query_map(duckdb::params_from_iter(bound.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// Untagged Usage charges of a period rolled up to one `(provider,
/// service)` row — what the Unallocated explainer card lists.
#[derive(Debug, Clone, PartialEq)]
pub struct UntaggedServiceUsage {
    pub provider: String,
    pub service: Option<String>,
    /// In the reporting currency.
    pub amount: f64,
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
    let since = Utc::now() - chrono::Duration::days(days);
    let mut stmt = conn.prepare(
        "SELECT CAST(observed_at AS VARCHAR), balance, currency
         FROM fct_balance_snapshot
         WHERE provider = ? AND account_id = ?
         ORDER BY currency, observed_at",
    )?;

    let rows = stmt
        .query_map(params![provider, account_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Per currency: the last observation before the window is the
    // baseline, drops inside the window are consumption. An account that
    // holds balances in more than one currency burns each separately;
    // the caller reads the balance of the currency it reports.
    let mut previous: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut burned: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut latest: Option<(DateTime<Utc>, String)> = None;

    for (stamp, balance, currency) in rows {
        let observed_at = super::parse_timestamp(&stamp)?;
        if let Some(before) = previous.insert(currency.clone(), balance) {
            if observed_at >= since && balance < before {
                *burned.entry(currency.clone()).or_insert(0.0) += before - balance;
            }
        }
        latest = match latest {
            Some((at, _)) if at > observed_at => latest,
            _ => Some((observed_at, currency)),
        };
    }

    let Some((_, currency)) = latest else {
        return Ok(None);
    };

    match burned.get(&currency).copied() {
        Some(total) if total > 0.0 => Ok(Some(total / days as f64)),
        _ => Ok(None),
    }
}

fn period_total_of(conn: &Connection, key: &PeriodKey) -> Result<f64> {
    let total: Option<f64> = conn.query_row(
        &format!(
            "SELECT sum(billed_cost_base) FROM {NORMALIZED_VIEW}
             WHERE provider = ? AND account_id = ? AND billing_period = ?"
        ),
        params![key.provider, key.account_id, key.billing_period],
        |row| row.get(0),
    )?;

    Ok(total.unwrap_or(0.0))
}

pub(crate) fn total_for_period_of(conn: &Connection, billing_period: &str) -> Result<f64> {
    let total: Option<f64> = conn.query_row(
        &format!("SELECT sum(billed_cost_base) FROM {NORMALIZED_VIEW} WHERE billing_period = ?"),
        params![billing_period],
        |row| row.get(0),
    )?;

    Ok(total.unwrap_or(0.0))
}

fn service_breakdown_of(conn: &Connection, key: &PeriodKey) -> Result<Vec<(String, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT coalesce(service_name, 'Other') AS service, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE provider = ? AND account_id = ? AND billing_period = ?
         GROUP BY service
         HAVING amount > 0
         ORDER BY amount DESC"
    ))?;

    let rows = stmt
        .query_map(
            params![key.provider, key.account_id, key.billing_period],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

fn daily_totals_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<DailyTotal>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT strftime(charge_period_start, '%Y-%m-%d') AS day, sum(billed_cost_base) AS amount
         FROM {NORMALIZED_VIEW}
         WHERE provider = ? AND account_id = ? AND charge_period_start >= CAST(? AS TIMESTAMP)
         GROUP BY day
         ORDER BY day"
    ))?;

    let rows = stmt
        .query_map(
            params![
                provider,
                account_id,
                since.format(TIMESTAMP_FORMAT).to_string()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(day, amount)| (day, amount.unwrap_or(0.0)))
        .collect())
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
        let monthly = monthly_usage_of(&conn, jul(1)).unwrap();
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
            monthly_usage_of(&conn, at(15)).unwrap(),
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
}
