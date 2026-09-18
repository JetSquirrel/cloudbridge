//! The arithmetic both data backends share.
//!
//! A read splits into two halves: selecting and grouping the charges, which
//! only the backend holding them can do — SQL against DuckDB on the desktop,
//! a fold over vectors in the browser — and the statistics computed from
//! those groups, which are the same arithmetic either way.
//!
//! The second half lives here. A forecast, its confidence bands, a
//! period-over-period comparison, a cost-change decomposition, the trailing
//! daily average and the data-quality findings are all pure functions over
//! aggregates, so neither backend owns them and neither can quietly diverge
//! from the other: change the formula here and both targets change with it.
//!
//! Nothing in this module reads a store, opens a connection or looks at a
//! clock — `now` is always an argument, which is also what makes the tests
//! below possible without either backend present.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};

use crate::model::{
    BillingPeriod, CategoryDelta, CostChangeDecomposition, DailyTotal, DataQualityIssue,
    DataQualityKind, ForecastBands, IssueSeverity, MovementKind, PeriodForecast, PeriodOverPeriod,
    ServiceMovement,
};

// ==================== Billing-period arithmetic ====================

/// The period a `YYYY-MM` label names, if it names a real month.
pub fn period_of(billing_period: &str) -> Result<BillingPeriod> {
    let (year, month) = billing_period
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("Not a YYYY-MM billing period: {:?}", billing_period))?;
    let year: i32 = year.parse()?;
    let month: u32 = month.parse()?;
    if NaiveDate::from_ymd_opt(year, month, 1).is_none() {
        anyhow::bail!("Not a real billing period: {:?}", billing_period);
    }

    Ok(BillingPeriod::new(year, month))
}

/// The label of the period immediately before this one.
pub fn previous_period(billing_period: &str) -> Result<String> {
    Ok(period_of(billing_period)?.previous().label())
}

/// First of the month `date` falls in.
pub fn first_of_month(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).expect("the first of a real month exists")
}

/// First of the month `months` before the one starting at `month_start`; a
/// negative shift moves forward.
pub fn months_before(month_start: NaiveDate, months: i64) -> NaiveDate {
    let index = month_start.year() as i64 * 12 + month_start.month() as i64 - 1 - months;
    let year = i32::try_from(index.div_euclid(12)).expect("a real year");
    let month = u32::try_from(index.rem_euclid(12)).expect("a month remainder") + 1;
    NaiveDate::from_ymd_opt(year, month, 1).expect("a real month")
}

/// Midnight UTC on `date`, the instant a day-aligned window bound names.
pub fn midnight(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// The day a `YYYY-MM-DD` key names, as both backends spell a daily bucket.
fn day_of(key: &str) -> Result<NaiveDate> {
    Ok(NaiveDate::parse_from_str(key, "%Y-%m-%d")?)
}

// ==================== Forecasting ====================

/// The OptScale run-rate model over one period's charges.
///
/// `month_to_date` is the period's charges before `now`, and
/// `first_charge_day` the day the earliest of them landed on. The daily rate
/// is measured from a baseline of `max(period start, first charge day)`: an
/// account that started reporting — or landed its first charge — mid-month
/// is not averaged over days it was not running, so the cold-start ramp-up
/// does not drag the rate down.
pub fn run_rate(
    period: &BillingPeriod,
    now: DateTime<Utc>,
    month_to_date: f64,
    first_charge_day: Option<NaiveDate>,
) -> PeriodForecast {
    let start = period.start();
    let end = period.end_exclusive();
    let days_in_month = (end - start).num_days();
    let last_day = end.pred_opt().expect("the day before a real one exists");

    let today = now.date_naive();
    let days_elapsed = if today < start {
        0
    } else {
        (today.min(last_day) - start).num_days() + 1
    };

    // The baseline is a date at 00:00, so every charge counted into
    // `month_to_date` falls on or after it: the cost since the baseline is
    // the month-to-date total itself.
    let baseline = first_charge_day
        .map(|first| first.max(start))
        .unwrap_or(start);
    let days_since_baseline = ((today.min(last_day) - baseline).num_days() + 1).max(1);

    let daily_rate = month_to_date / days_since_baseline as f64;
    let forecast = month_to_date + daily_rate * (days_in_month - days_elapsed) as f64;

    PeriodForecast {
        month_to_date,
        daily_rate,
        forecast,
        days_elapsed,
        days_in_month,
    }
}

/// Confidence bands around a period forecast.
///
/// `daily` is the same charge set the forecast totals — this period, before
/// `now` — bucketed by day. The sample is measured from the same baseline as
/// [`run_rate`], `max(period start, first charge day)`, and a day inside it
/// with no charge counts as zero, so `daily_mean` agrees with the forecast's
/// daily rate. With fewer than two sampled days a standard deviation does not
/// exist, and both bands collapse onto `expected`.
pub fn forecast_bands(
    period: &BillingPeriod,
    now: DateTime<Utc>,
    forecast: PeriodForecast,
    daily: &[DailyTotal],
) -> Result<ForecastBands> {
    let start = period.start();
    let last_day = period
        .end_exclusive()
        .pred_opt()
        .expect("the day before a real one exists");
    let today = now.date_naive().min(last_day);
    let remaining = (forecast.days_in_month - forecast.days_elapsed).max(0) as f64;

    let by_day: BTreeMap<NaiveDate, f64> = daily
        .iter()
        .map(|(day, amount)| Ok((day_of(day)?, *amount)))
        .collect::<Result<_>>()?;

    let baseline = by_day
        .keys()
        .next()
        .map(|first| (*first).max(start))
        .unwrap_or(start);

    // One value per day since the baseline, zero-filled, so the mean agrees
    // with the forecast's daily rate.
    let mut values = Vec::new();
    let mut day = baseline;
    while day <= today {
        values.push(by_day.get(&day).copied().unwrap_or(0.0));
        day = day.succ_opt().expect("the day after a real one exists");
    }

    let n = values.len();
    let daily_mean = if n == 0 {
        0.0
    } else {
        values.iter().sum::<f64>() / n as f64
    };
    let daily_stddev = if n < 2 {
        0.0
    } else {
        let variance = values
            .iter()
            .map(|value| (value - daily_mean).powi(2))
            .sum::<f64>()
            / (n - 1) as f64;
        variance.sqrt()
    };

    let (optimistic, pessimistic) = if n < 2 {
        (forecast.forecast, forecast.forecast)
    } else {
        (
            forecast.month_to_date + (daily_mean + daily_stddev) * remaining,
            forecast.month_to_date + (daily_mean - daily_stddev).max(0.0) * remaining,
        )
    };

    Ok(ForecastBands {
        month_to_date: forecast.month_to_date,
        expected: forecast.forecast,
        optimistic,
        pessimistic,
        daily_mean,
        daily_stddev,
    })
}

// ==================== Period comparison ====================

/// Two adjacent periods' per-service totals, as the comparison presents them.
///
/// Each side's net total is the sum of its buckets — every charge of either
/// period is in one of them — while the ranked lists drop a service that nets
/// to zero or below, as every other ranking here drops one.
pub fn compare_periods(
    current: BTreeMap<String, f64>,
    previous: BTreeMap<String, f64>,
) -> PeriodOverPeriod {
    PeriodOverPeriod {
        current_total: current.values().sum(),
        previous_total: previous.values().sum(),
        current_by_service: ranked(current),
        previous_by_service: ranked(previous),
    }
}

/// One period's `(bucket, amount)` pairs, largest first, with a net-negative
/// bucket dropped.
///
/// The sort is over a map, so buckets of equal amount keep their key order
/// and two reads of the same data order them the same way.
fn ranked(totals: BTreeMap<String, f64>) -> Vec<(String, f64)> {
    let mut rows: Vec<(String, f64)> = totals
        .into_iter()
        .filter(|(_, amount)| *amount > 0.0)
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));

    rows
}

// ==================== Cost-change decomposition ====================

/// The Wealthfolio attribution tolerance: 0.2% of the amount being explained,
/// never less than a dollar — below that the gap is arithmetic, not a missing
/// component.
const RECONCILE_REL_TOLERANCE: f64 = 0.002;
const RECONCILE_ABS_FLOOR: f64 = 1.0;

/// The gap between a total and what its components explain, and whether the
/// gap is small enough to ignore.
pub fn reconcile(total_delta: f64, explained: f64) -> (f64, bool) {
    let residual = total_delta - explained;
    let tolerance = (total_delta.abs() * RECONCILE_REL_TOLERANCE).max(RECONCILE_ABS_FLOOR);
    (residual, residual.abs() <= tolerance)
}

/// One bucket's `(current, previous)` amounts across two adjacent periods.
pub type TwoPeriodBuckets = BTreeMap<String, (f64, f64)>;

/// Decompose the current-vs-previous-period delta into its components, by
/// charge category and by service, and check that they add back up.
///
/// Both totals come from `categories`, which partitions every charge of
/// either period; `services` partitions the same charges a second way, so it
/// is not summed again. Ordering is by the size of the swing, ties broken by
/// bucket name — a map, not a hash table, so a re-read ranks them the same.
pub fn decompose(
    billing_period: &str,
    previous_period: String,
    categories: TwoPeriodBuckets,
    services: TwoPeriodBuckets,
) -> CostChangeDecomposition {
    let current_total = categories.values().map(|(current, _)| *current).sum();
    let previous_total = categories.values().map(|(_, previous)| *previous).sum();

    let mut by_category: Vec<CategoryDelta> = categories
        .into_iter()
        .map(|(category, (current, previous))| CategoryDelta {
            category,
            current,
            previous,
            delta: current - previous,
        })
        .collect();
    by_category.sort_by(|a, b| b.delta.abs().total_cmp(&a.delta.abs()));

    let mut by_service: Vec<ServiceMovement> = services
        .into_iter()
        .filter_map(|(service, (current, previous))| {
            let delta = current - previous;
            if delta == 0.0 {
                return None;
            }
            let kind = if previous == 0.0 {
                MovementKind::Appeared
            } else if current == 0.0 {
                MovementKind::Vanished
            } else if delta > 0.0 {
                MovementKind::Grown
            } else {
                MovementKind::Shrunk
            };
            Some(ServiceMovement {
                service,
                kind,
                current,
                previous,
                delta,
            })
        })
        .collect();
    by_service.sort_by(|a, b| b.delta.abs().total_cmp(&a.delta.abs()));

    let total_delta = current_total - previous_total;
    let explained: f64 = by_category.iter().map(|component| component.delta).sum();
    let (residual, reconciled) = reconcile(total_delta, explained);

    CostChangeDecomposition {
        billing_period: billing_period.to_string(),
        previous_period,
        current_total,
        previous_total,
        total_delta,
        by_category,
        by_service,
        residual,
        reconciled,
    }
}

// ==================== Trailing-average overlay ====================

/// The charge-time window [`trailing_average`] reads usage from, as
/// `(start, end_exclusive)` — `None` for degenerate inputs, which produce an
/// empty series and need no read at all.
///
/// It is the widest window any output day can need. No window reaches into
/// today's month, so the current month never compares to itself.
pub fn trailing_window(
    day_count: i64,
    months: i64,
    now: DateTime<Utc>,
) -> Option<(NaiveDate, NaiveDate)> {
    if day_count < 1 || months < 1 {
        return None;
    }
    let today = now.date_naive();
    let first_day = today - Duration::days(day_count - 1);

    Some((
        months_before(first_of_month(first_day), months),
        first_of_month(today),
    ))
}

/// A "typical day" series to overlay on the current month's daily line — the
/// Wealthfolio benchmark-comparison pattern.
///
/// For each of the last `day_count` days, oldest first, as `(YYYY-MM-DD,
/// amount)`: the average daily **usage** of the `months` complete calendar
/// months preceding the day's own month, spread over their calendar days — a
/// day with no charges counts as zero. The window ends where the day's month
/// begins, so a month is never compared against itself; every day of one
/// month therefore reads the same value and the series is the flat benchmark
/// line the actual daily line is drawn against.
///
/// `monthly` is usage totalled by `YYYY-MM` of **charge time**, over the
/// window [`trailing_window`] names — keyed the same way the lookups below
/// are, which is why it is not keyed by billing period. Degenerate inputs
/// (`day_count` or `months` below 1) yield an empty series.
pub fn trailing_average(
    day_count: i64,
    months: i64,
    now: DateTime<Utc>,
    monthly: &BTreeMap<String, f64>,
) -> Vec<DailyTotal> {
    if day_count < 1 || months < 1 {
        return Vec::new();
    }
    let today = now.date_naive();
    let first_day = today - Duration::days(day_count - 1);

    let mut series = Vec::with_capacity(day_count.min(1024) as usize);
    let mut day = first_day;
    while day <= today {
        let end = first_of_month(day);
        let start = months_before(end, months);
        let mut month = start;
        let mut total = 0.0;
        while month < end {
            total += monthly
                .get(&month.format("%Y-%m").to_string())
                .copied()
                .unwrap_or(0.0);
            month = months_before(month, -1);
        }
        let days = (end - start).num_days() as f64;
        series.push((day.format("%Y-%m-%d").to_string(), total / days));
        day = day.succ_opt().expect("the day after a real one exists");
    }

    series
}

// ==================== Balance burn ====================

/// One balance observation: when it was taken, what it read, and in which
/// currency.
pub type BalanceObservation = (DateTime<Utc>, f64, String);

/// Mean daily burn over the last `days`, in the currency of the most recent
/// observation.
///
/// `observations` are that account's snapshots ordered by currency, then by
/// time. Computed from the drops between consecutive observations — a rise is
/// a top-up, not consumption. `None` when the history holds fewer than two
/// observations in the reported currency, because then burn is unknowable.
///
/// Per currency: the last observation before the window is the baseline,
/// drops inside the window are consumption. An account that holds balances in
/// more than one currency burns each separately; the caller reads the balance
/// of the currency it reports.
pub fn burn(observations: &[BalanceObservation], days: i64, now: DateTime<Utc>) -> Option<f64> {
    let since = now - Duration::days(days);

    let mut previous: BTreeMap<String, f64> = BTreeMap::new();
    let mut burned: BTreeMap<String, f64> = BTreeMap::new();
    let mut latest: Option<(DateTime<Utc>, &str)> = None;

    for (observed_at, balance, currency) in observations {
        if let Some(before) = previous.insert(currency.clone(), *balance) {
            if *observed_at >= since && *balance < before {
                *burned.entry(currency.clone()).or_insert(0.0) += before - balance;
            }
        }
        latest = match latest {
            Some((at, _)) if at > *observed_at => latest,
            _ => Some((*observed_at, currency)),
        };
    }

    let (_, currency) = latest?;
    match burned.get(currency).copied() {
        Some(total) if total > 0.0 => Some(total / days as f64),
        _ => None,
    }
}

// ==================== Data-quality summary ====================

/// Untagged usage above this share of the period's usage is a warning; below
/// it, a note.
const UNTAGGED_WARNING_SHARE: f64 = 0.20;
/// A service whose region-less usage exceeds this share of the period's usage
/// is worth listing...
const REGION_NOTICE_SHARE: f64 = 0.05;
/// ...and above this one it is a warning.
const REGION_WARNING_SHARE: f64 = 0.25;

/// What a backend has to count for [`data_quality`] to judge a period.
///
/// Every field is an aggregate over one billing period, so a backend answers
/// it however it reads best and the thresholds, wording and severities stay
/// in one place.
pub struct QualityCounts<'a> {
    /// The tag key the attribution page groups by.
    pub tag_key: &'a str,
    /// Charges in the period, the denominator of the unconverted share.
    pub rows: i64,
    /// Charges carrying an amount that no FX rate covers.
    pub unconverted: i64,
    /// Gross usage in the period, the denominator of both share checks.
    pub usage: f64,
    /// Usage carrying no value for `tag_key`, and how many rows it is.
    pub untagged: f64,
    pub untagged_count: i64,
    /// Usage with no region, as `(service, amount, charges)`, largest first
    /// and already filtered to positive amounts.
    pub regionless: Vec<(String, f64, i64)>,
    /// `cloud::deduction`'s escape hatch, as `(count, amount)`: money a bill
    /// accounts for that no named deduction covers. `None` on a backend that
    /// cannot see it — the browser's reading view carries no charge
    /// description, and no row it can be given would raise the finding.
    pub unreconciled: Option<(i64, f64)>,
}

/// The data-quality findings for a period: unconverted charges, untagged
/// usage, services whose usage carries no region, and unreconciled
/// adjustments. A clean period yields an empty list.
pub fn data_quality(counts: QualityCounts<'_>) -> Vec<DataQualityIssue> {
    let QualityCounts {
        tag_key,
        rows,
        unconverted,
        usage,
        untagged,
        untagged_count,
        regionless,
        unreconciled,
    } = counts;
    let mut issues = Vec::new();

    // (a) Charges no rate covers. Their amounts stay in currencies that
    // cannot be summed, so the issue carries the row count and its share,
    // not an amount.
    if unconverted > 0 {
        let share = unconverted as f64 / rows.max(1) as f64 * 100.0;
        issues.push(DataQualityIssue {
            kind: DataQualityKind::UnconvertedCharges,
            severity: IssueSeverity::Warning,
            message: format!(
                "{unconverted} charges ({share:.1}% of the period's rows) have no FX rate \
                 and are missing from every converted total"
            ),
            affected_amount: None,
            affected_count: unconverted,
        });
    }

    // (b) Usage with no value for the tag the attribution page groups by.
    if untagged > 0.0 {
        let share = if usage > 0.0 { untagged / usage } else { 0.0 };
        issues.push(DataQualityIssue {
            kind: DataQualityKind::UntaggedUsage,
            severity: if share > UNTAGGED_WARNING_SHARE {
                IssueSeverity::Warning
            } else {
                IssueSeverity::Info
            },
            message: format!(
                "{untagged:.2} of usage ({:.1}% of the period's usage) carries no '{tag_key}' tag",
                share * 100.0
            ),
            affected_amount: Some(untagged),
            affected_count: untagged_count,
        });
    }

    // (c) Region-less usage, per service: a charge without a region cannot be
    // placed on the region breakdown.
    for (service, amount, charges) in regionless {
        let share = if usage > 0.0 { amount / usage } else { 0.0 };
        if share <= REGION_NOTICE_SHARE {
            continue;
        }
        issues.push(DataQualityIssue {
            kind: DataQualityKind::MissingRegion,
            severity: if share > REGION_WARNING_SHARE {
                IssueSeverity::Warning
            } else {
                IssueSeverity::Info
            },
            message: format!(
                "{amount:.2} of {service} usage ({:.1}% of the period's usage) has no region",
                share * 100.0
            ),
            affected_amount: Some(amount),
            affected_count: charges,
        });
    }

    // (d) Money a bill accounts for that no named deduction covers.
    if let Some((count, amount)) = unreconciled.filter(|(count, _)| *count > 0) {
        issues.push(DataQualityIssue {
            kind: DataQualityKind::UnreconciledAdjustment,
            severity: IssueSeverity::Critical,
            message: format!(
                "{count} 'Unreconciled' adjustment rows totalling {amount:.2}: \
                 bill lines whose named deductions did not add up"
            ),
            affected_amount: Some(amount),
            affected_count: count,
        });
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("a real date")
    }

    fn noon(year: i32, month: u32, date: u32) -> DateTime<Utc> {
        day(year, month, date)
            .and_hms_opt(12, 0, 0)
            .expect("midday exists")
            .and_utc()
    }

    fn totals(days: &[(&str, f64)]) -> Vec<DailyTotal> {
        days.iter()
            .map(|(day, amount)| ((*day).to_string(), *amount))
            .collect()
    }

    #[test]
    fn a_period_label_round_trips_and_a_bad_one_is_rejected() {
        assert_eq!(period_of("2026-02").unwrap().label(), "2026-02");
        assert_eq!(previous_period("2026-01").unwrap(), "2025-12");
        assert_eq!(previous_period("2026-12").unwrap(), "2026-11");

        assert!(period_of("2026").is_err());
        assert!(period_of("2026-13").is_err());
        assert!(period_of("not-a-period").is_err());
    }

    #[test]
    fn month_shifts_cross_year_boundaries_in_both_directions() {
        assert_eq!(first_of_month(day(2026, 3, 17)), day(2026, 3, 1));
        assert_eq!(months_before(day(2026, 1, 1), 1), day(2025, 12, 1));
        assert_eq!(months_before(day(2026, 1, 1), 13), day(2024, 12, 1));
        // A negative shift moves forward, as the trailing average walks.
        assert_eq!(months_before(day(2025, 12, 1), -1), day(2026, 1, 1));
    }

    #[test]
    fn the_forecast_is_the_run_rate_plus_what_already_landed() {
        // 300 over the first 10 days of a 31-day month: 30/day, 21 to go.
        let forecast = run_rate(
            &BillingPeriod::new(2026, 1),
            noon(2026, 1, 10),
            300.0,
            Some(day(2026, 1, 1)),
        );

        assert_eq!(forecast.days_in_month, 31);
        assert_eq!(forecast.days_elapsed, 10);
        assert!((forecast.daily_rate - 30.0).abs() < 1e-9);
        assert!((forecast.forecast - 930.0).abs() < 1e-9);
    }

    #[test]
    fn the_forecast_skips_the_cold_start_before_the_first_expense() {
        // The same 300, but nothing landed until the 6th: the rate is
        // measured over 5 days, not 10.
        let forecast = run_rate(
            &BillingPeriod::new(2026, 1),
            noon(2026, 1, 10),
            300.0,
            Some(day(2026, 1, 6)),
        );

        assert_eq!(forecast.days_elapsed, 10);
        assert!((forecast.daily_rate - 60.0).abs() < 1e-9);
        assert!((forecast.forecast - (300.0 + 60.0 * 21.0)).abs() < 1e-9);
    }

    #[test]
    fn a_past_period_forecasts_its_own_total() {
        let forecast = run_rate(
            &BillingPeriod::new(2025, 11),
            noon(2026, 1, 10),
            500.0,
            Some(day(2025, 11, 1)),
        );

        assert_eq!(forecast.days_elapsed, 30);
        assert!((forecast.forecast - 500.0).abs() < 1e-9);
    }

    #[test]
    fn a_period_with_no_charges_forecasts_zero() {
        let forecast = run_rate(&BillingPeriod::new(2026, 1), noon(2026, 1, 10), 0.0, None);

        assert_eq!(forecast.month_to_date, 0.0);
        assert_eq!(forecast.daily_rate, 0.0);
        assert_eq!(forecast.forecast, 0.0);
    }

    #[test]
    fn a_period_that_has_not_started_has_no_days_elapsed() {
        let forecast = run_rate(&BillingPeriod::new(2026, 6), noon(2026, 1, 10), 0.0, None);

        assert_eq!(forecast.days_elapsed, 0);
        assert_eq!(forecast.days_in_month, 30);
    }

    #[test]
    fn bands_open_around_the_expected_forecast() {
        let period = BillingPeriod::new(2026, 1);
        let now = noon(2026, 1, 4);
        let daily = totals(&[
            ("2026-01-01", 10.0),
            ("2026-01-02", 30.0),
            ("2026-01-03", 20.0),
            ("2026-01-04", 40.0),
        ]);
        let forecast = run_rate(&period, now, 100.0, Some(day(2026, 1, 1)));
        let bands = forecast_bands(&period, now, forecast, &daily).unwrap();

        assert!((bands.daily_mean - 25.0).abs() < 1e-9);
        assert!(bands.daily_stddev > 0.0);
        assert!(bands.pessimistic < bands.expected);
        assert!(bands.optimistic > bands.expected);
        // The mean of the sample is the forecast's own daily rate.
        assert!((bands.daily_mean - 100.0 / 4.0).abs() < 1e-9);
    }

    #[test]
    fn bands_with_fewer_than_two_days_of_data_collapse_onto_the_forecast() {
        let period = BillingPeriod::new(2026, 1);
        let now = noon(2026, 1, 1);
        let daily = totals(&[("2026-01-01", 100.0)]);
        let forecast = run_rate(&period, now, 100.0, Some(day(2026, 1, 1)));
        let bands = forecast_bands(&period, now, forecast, &daily).unwrap();

        assert_eq!(bands.daily_stddev, 0.0);
        assert_eq!(bands.optimistic, bands.expected);
        assert_eq!(bands.pessimistic, bands.expected);
    }

    #[test]
    fn a_day_inside_the_sample_with_no_charge_counts_as_zero() {
        let period = BillingPeriod::new(2026, 1);
        let now = noon(2026, 1, 4);
        // Nothing on the 2nd and 3rd: the sample is four days, not two.
        let daily = totals(&[("2026-01-01", 50.0), ("2026-01-04", 50.0)]);
        let forecast = run_rate(&period, now, 100.0, Some(day(2026, 1, 1)));
        let bands = forecast_bands(&period, now, forecast, &daily).unwrap();

        assert!((bands.daily_mean - 25.0).abs() < 1e-9);
    }

    #[test]
    fn a_comparison_totals_every_bucket_but_ranks_only_the_positive_ones() {
        let current = BTreeMap::from([
            ("EC2".to_string(), 100.0),
            ("S3".to_string(), 40.0),
            // A service that netted below zero after a credit landed.
            ("Refunds".to_string(), -30.0),
        ]);
        let previous = BTreeMap::from([("EC2".to_string(), 80.0)]);

        let comparison = compare_periods(current, previous);

        assert!((comparison.current_total - 110.0).abs() < 1e-9);
        assert!((comparison.previous_total - 80.0).abs() < 1e-9);
        assert_eq!(
            comparison.current_by_service,
            vec![("EC2".to_string(), 100.0), ("S3".to_string(), 40.0)]
        );
    }

    #[test]
    fn the_decomposition_explains_the_delta_and_reconciles() {
        let categories = TwoPeriodBuckets::from([
            ("Usage".to_string(), (1200.0, 1000.0)),
            ("Credit".to_string(), (-100.0, -50.0)),
        ]);
        let services = TwoPeriodBuckets::from([
            ("EC2".to_string(), (700.0, 500.0)),
            ("S3".to_string(), (400.0, 450.0)),
            ("Bedrock".to_string(), (0.0, 0.0)),
        ]);

        let decomposition = decompose("2026-08", "2026-07".to_string(), categories, services);

        assert_eq!(decomposition.previous_period, "2026-07");
        assert!((decomposition.current_total - 1100.0).abs() < 1e-9);
        assert!((decomposition.previous_total - 950.0).abs() < 1e-9);
        assert!((decomposition.total_delta - 150.0).abs() < 1e-9);
        assert!(decomposition.reconciled);
        // Largest swing first, and a service that did not move is absent.
        assert_eq!(decomposition.by_service.len(), 2);
        assert_eq!(decomposition.by_service[0].service, "EC2");
        assert_eq!(decomposition.by_service[0].kind, MovementKind::Grown);
        assert_eq!(decomposition.by_service[1].kind, MovementKind::Shrunk);
    }

    #[test]
    fn a_service_that_arrived_or_left_is_named_as_such() {
        let services = TwoPeriodBuckets::from([
            ("Bedrock".to_string(), (240.0, 0.0)),
            ("Athena".to_string(), (0.0, 60.0)),
        ]);
        let decomposition = decompose(
            "2026-08",
            "2026-07".to_string(),
            TwoPeriodBuckets::from([("Usage".to_string(), (240.0, 60.0))]),
            services,
        );

        let kinds: Vec<_> = decomposition
            .by_service
            .iter()
            .map(|movement| (movement.service.as_str(), movement.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("Bedrock", MovementKind::Appeared),
                ("Athena", MovementKind::Vanished)
            ]
        );
    }

    #[test]
    fn the_reconciler_flags_a_planted_residual() {
        // 0.5% of the delta is outside the 0.2% tolerance...
        let (residual, reconciled) = reconcile(1000.0, 995.0);
        assert!((residual - 5.0).abs() < 1e-9);
        assert!(!reconciled);

        // ...while 0.15% is inside it.
        let (_, reconciled) = reconcile(1000.0, 998.5);
        assert!(reconciled);

        // On a delta of nothing the absolute floor is what decides.
        let (_, reconciled) = reconcile(0.0, 0.5);
        assert!(reconciled);
        let (_, reconciled) = reconcile(0.0, 5.0);
        assert!(!reconciled);
    }

    #[test]
    fn the_trailing_average_is_a_flat_typical_day_of_the_preceding_months() {
        // 620 of usage in January (31 days) reads as 20/day through all of
        // February, whose own charges are not in the window.
        let monthly = BTreeMap::from([("2026-01".to_string(), 620.0)]);
        let series = trailing_average(3, 1, noon(2026, 2, 3), &monthly);

        assert_eq!(
            series,
            vec![
                ("2026-02-01".to_string(), 20.0),
                ("2026-02-02".to_string(), 20.0),
                ("2026-02-03".to_string(), 20.0),
            ]
        );
    }

    #[test]
    fn the_trailing_average_window_slides_at_a_month_boundary() {
        // Two days either side of the boundary: the January days average
        // December alone, the February days average January alone.
        let monthly = BTreeMap::from([
            ("2025-12".to_string(), 310.0),
            ("2026-01".to_string(), 620.0),
        ]);
        let series = trailing_average(2, 1, noon(2026, 2, 1), &monthly);

        assert_eq!(
            series,
            vec![
                ("2026-01-31".to_string(), 10.0),
                ("2026-02-01".to_string(), 20.0),
            ]
        );
    }

    #[test]
    fn a_degenerate_trailing_average_reads_nothing_at_all() {
        assert!(trailing_window(0, 1, noon(2026, 2, 1)).is_none());
        assert!(trailing_window(30, 0, noon(2026, 2, 1)).is_none());
        assert!(trailing_average(0, 1, noon(2026, 2, 1), &BTreeMap::new()).is_empty());
    }

    #[test]
    fn the_trailing_window_covers_every_month_any_output_day_averages() {
        // Three months of benchmark for a 30-day series ending 2026-02-03:
        // the oldest day needed is October 2025, the newest month excluded
        // is February itself.
        let (start, end) = trailing_window(30, 3, noon(2026, 2, 3)).unwrap();

        assert_eq!(start, day(2025, 10, 1));
        assert_eq!(end, day(2026, 2, 1));
    }

    #[test]
    fn burn_is_the_mean_of_the_drops() {
        // 100 -> 70 -> 90 (a top-up) -> 60: 30 + 30 burned over 10 days.
        let observations = vec![
            (noon(2026, 1, 1), 100.0, "USD".to_string()),
            (noon(2026, 1, 3), 70.0, "USD".to_string()),
            (noon(2026, 1, 5), 90.0, "USD".to_string()),
            (noon(2026, 1, 7), 60.0, "USD".to_string()),
        ];

        let burned = burn(&observations, 10, noon(2026, 1, 8)).unwrap();
        assert!((burned - 6.0).abs() < 1e-9);
    }

    #[test]
    fn burn_is_unknowable_from_a_single_observation() {
        let observations = vec![(noon(2026, 1, 1), 100.0, "USD".to_string())];
        assert!(burn(&observations, 10, noon(2026, 1, 8)).is_none());
        assert!(burn(&[], 10, noon(2026, 1, 8)).is_none());
    }

    #[test]
    fn burn_is_reported_in_the_currency_of_the_newest_observation() {
        // Two currencies, each burning separately; the CNY balance is the
        // most recent, so it is the one reported.
        let observations = vec![
            (noon(2026, 1, 1), 500.0, "CNY".to_string()),
            (noon(2026, 1, 6), 300.0, "CNY".to_string()),
            (noon(2026, 1, 1), 100.0, "USD".to_string()),
            (noon(2026, 1, 3), 90.0, "USD".to_string()),
        ];

        let burned = burn(&observations, 10, noon(2026, 1, 8)).unwrap();
        assert!((burned - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_rise_alone_is_not_consumption() {
        let observations = vec![
            (noon(2026, 1, 1), 100.0, "USD".to_string()),
            (noon(2026, 1, 3), 140.0, "USD".to_string()),
        ];
        assert!(burn(&observations, 10, noon(2026, 1, 8)).is_none());
    }

    fn clean() -> QualityCounts<'static> {
        QualityCounts {
            tag_key: "business_line",
            rows: 100,
            unconverted: 0,
            usage: 1000.0,
            untagged: 0.0,
            untagged_count: 0,
            regionless: Vec::new(),
            unreconciled: None,
        }
    }

    #[test]
    fn a_clean_period_raises_no_data_quality_issues() {
        assert!(data_quality(clean()).is_empty());
    }

    #[test]
    fn unconverted_charges_are_flagged_with_their_row_share() {
        let issues = data_quality(QualityCounts {
            unconverted: 25,
            ..clean()
        });

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, DataQualityKind::UnconvertedCharges);
        assert_eq!(issues[0].severity, IssueSeverity::Warning);
        assert_eq!(issues[0].affected_count, 25);
        assert!(issues[0].affected_amount.is_none());
        assert!(issues[0].message.contains("25.0%"));
    }

    #[test]
    fn untagged_usage_warns_above_a_fifth_of_the_period() {
        let note = data_quality(QualityCounts {
            untagged: 100.0,
            untagged_count: 4,
            ..clean()
        });
        assert_eq!(note[0].severity, IssueSeverity::Info);

        let warning = data_quality(QualityCounts {
            untagged: 300.0,
            untagged_count: 9,
            ..clean()
        });
        assert_eq!(warning[0].kind, DataQualityKind::UntaggedUsage);
        assert_eq!(warning[0].severity, IssueSeverity::Warning);
        assert_eq!(warning[0].affected_amount, Some(300.0));
        assert!(warning[0].message.contains("business_line"));
    }

    #[test]
    fn regionless_usage_is_flagged_by_share_and_ignored_below_the_notice() {
        let issues = data_quality(QualityCounts {
            regionless: vec![
                ("EC2".to_string(), 300.0, 6),
                ("S3".to_string(), 80.0, 3),
                // 4% of the period — below the notice share, so absent.
                ("Lambda".to_string(), 40.0, 2),
            ],
            ..clean()
        });

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].severity, IssueSeverity::Warning);
        assert!(issues[0].message.contains("EC2"));
        assert_eq!(issues[1].severity, IssueSeverity::Info);
        assert!(issues[1].message.contains("S3"));
    }

    #[test]
    fn unreconciled_adjustments_are_critical_and_absent_where_unreadable() {
        let issues = data_quality(QualityCounts {
            unreconciled: Some((3, -120.0)),
            ..clean()
        });
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, DataQualityKind::UnreconciledAdjustment);
        assert_eq!(issues[0].severity, IssueSeverity::Critical);

        // A backend that cannot see them, and one that saw none, both say
        // nothing rather than reporting a clean sweep.
        assert!(data_quality(QualityCounts {
            unreconciled: None,
            ..clean()
        })
        .is_empty());
        assert!(data_quality(QualityCounts {
            unreconciled: Some((0, 0.0)),
            ..clean()
        })
        .is_empty());
    }

    #[test]
    fn a_period_with_no_usage_still_reports_its_untagged_rows() {
        // The share is the denominator's problem, not the finding's: with no
        // usage to divide by it reads 0%, and the amount still shows.
        let issues = data_quality(QualityCounts {
            usage: 0.0,
            untagged: 50.0,
            untagged_count: 2,
            ..clean()
        });

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, IssueSeverity::Info);
        assert!(issues[0].message.contains("0.0%"));
    }
}
