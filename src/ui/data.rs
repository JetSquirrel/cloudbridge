//! The real-data view-model layer behind the redesigned pages.
//!
//! Every loader is a blocking `pub fn load_*() -> Result<...>`; pages wrap
//! them in `smol::unblock` themselves (see `accounts.rs` for the pattern).
//! The structs mirror `mock.rs` field for field where the mock had the
//! right shape, with owned `String`s in place of `&'static str`.
//!
//! Empty-state semantics: on a fresh or empty ledger every loader returns
//! zeros and empty vectors, never an error. The pages render their empty
//! states from these.
//!
//! Usage vs net: headline amounts stay net (`billed_cost_base` summed
//! across charge categories), but every ranking, chart, and percentage is
//! computed on gross usage (`charge_category = 'Usage'`) through the
//! `*_usage` ledger queries — credits can net an account to ≈ $0, and UI
//! math on that base is noise.

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::fmt;
use crate::alerts::{self, AlertKind, AlertStatus, AlertView, RuleView};
use crate::analytics;
use crate::cloud::registry;
use crate::cloud::{BillingPeriod, BudgetInfo, BudgetStatus};
use crate::ledger::query;
use crate::{db, ingest};

/// The tag that maps a charge to a business line.
///
/// Charges carry tags as a JSON object; the value under this key is what
/// the Overview "Where it went" rows, the Attribution Sankey's last hop,
/// and the unallocated-share rule all group by. A charge without it counts
/// as "Unallocated" everywhere.
pub const BUSINESS_LINE_TAG: &str = "business_line";

/// The label every breakdown gives usage that carries no
/// [`BUSINESS_LINE_TAG`] value.
pub const UNALLOCATED: &str = "Unallocated";

/// The reporting currency every amount below is expressed in, so a page
/// can format without asking config again.
pub fn reporting_currency() -> String {
    crate::config::load_config()
        .map(|settings| settings.reporting_currency)
        .unwrap_or_else(|_| crate::config::DEFAULT_REPORTING_CURRENCY.to_string())
}

// ==================== Overview ====================

/// A half-open charge-time window `[since, until)`.
pub type Window = (DateTime<Utc>, DateTime<Utc>);

/// Range selection of the Overview header segmented control: the month to
/// date, the rolling last 30 days, or the last 12 calendar months. Every
/// number on the page is computed for the selected range's window.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Mtd,
    Days30,
    Months12,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::Mtd, Range::Days30, Range::Months12];

    pub fn label(self) -> &'static str {
        match self {
            Range::Mtd => "MTD",
            Range::Days30 => "30d",
            Range::Months12 => "12m",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Range::Mtd => "range-mtd",
            Range::Days30 => "range-30d",
            Range::Months12 => "range-12m",
        }
    }

    /// The window `[since, until)` the range covers, and the window right
    /// before it that change percents compare against. The rolling ranges
    /// align to calendar boundaries so the chart's day and month buckets
    /// fill the window exactly; MTD's comparison window is the whole
    /// previous month (its headline percent still uses the same-day rule).
    pub fn windows(self, now: DateTime<Utc>) -> (Window, Window) {
        let midnight = |date: NaiveDate| {
            date.and_hms_opt(0, 0, 0)
                .expect("midnight exists")
                .and_utc()
        };
        let current = BillingPeriod::containing(now);
        match self {
            Range::Mtd => {
                let since = midnight(current.start());
                ((since, now), (midnight(current.previous().start()), since))
            }
            Range::Days30 => {
                // Today plus the 29 before it: 30 day buckets, ending now.
                let since = midnight(now.date_naive() - chrono::Duration::days(29));
                ((since, now), (since - chrono::Duration::days(30), since))
            }
            Range::Months12 => {
                let mut first = current;
                for _ in 0..11 {
                    first = first.previous();
                }
                let mut prior_first = first;
                for _ in 0..12 {
                    prior_first = prior_first.previous();
                }
                let since = midnight(first.start());
                ((since, now), (midnight(prior_first.start()), since))
            }
        }
    }

    /// The header caption's range part, e.g. "September 2026, month to
    /// date"; the view appends the reporting currency.
    pub fn header_caption(self, now: DateTime<Utc>) -> String {
        match self {
            Range::Mtd => format!("{}, month to date", now.format("%B %Y")),
            Range::Days30 => "Last 30 days".to_string(),
            Range::Months12 => "Last 12 months".to_string(),
        }
    }
}

/// Headline numbers for the Overview page, all for the selected range's
/// window.
///
/// The hybrid split: `spend` is net (usage and credits summed) and stays
/// the big number; everything trended or ranked here — the change percent,
/// the chart, "Where it went", the movers — runs on gross usage, because a
/// credit-covered account nets to ≈ $0.
pub struct OverviewStats {
    /// Window spend, net of credits.
    pub spend: f64,
    /// Window gross usage (charges with category "Usage").
    pub usage: f64,
    /// Window credits (negative) — what takes usage down to net.
    pub credits: f64,
    /// Percent change of usage vs the comparison window (signed). `None`
    /// when the comparison base is under a cent: a ratio on dust is noise
    /// (a real account produced −318.9%).
    pub change_pct: Option<f64>,
    /// Share of usage that reaches no business line (0–100).
    pub unallocated_pct: f64,
    /// Usage that reaches no business line.
    pub unallocated_amount: f64,
    /// Open alerts.
    pub open_alerts: usize,
    /// Of the open alerts, how many are critical.
    pub critical_alerts: usize,
    /// Of the open alerts, how many are warnings.
    pub warning_alerts: usize,
}

/// One point of the spend chart.
#[derive(Clone)]
pub struct ChartPoint {
    /// What the point covers: `YYYY-MM-DD` for the daily ranges,
    /// `YYYY-MM` for the 12-month one.
    pub label: String,
    /// Gross usage in the reporting currency.
    pub amount: f64,
}

/// The spend chart: actual usage against a 7-day trailing mean baseline.
/// The 12-month range carries no baseline — a trailing mean over twelve
/// totals would just lag them — so `baseline` is empty there and the view
/// hides the dashed series and its legend entry.
pub struct SpendChart {
    /// One point per day (MTD, 30d) or per month (12m) of the window.
    pub actual: Vec<ChartPoint>,
    /// Trailing 7-day mean ending the day before each actual point.
    pub baseline: Vec<ChartPoint>,
}

/// One row of the "Where it went" breakdown.
pub struct BusinessLineRow {
    pub name: String,
    /// Window gross usage.
    pub amount: f64,
}

/// One row of the "Biggest movers" table, ranked by the size of the swing
/// against the comparison window.
pub struct MoverRow {
    pub provider: String,
    pub service: String,
    /// Window gross usage.
    pub amount: f64,
    /// Percent change of usage vs the comparison window (signed). `None`
    /// when the service's comparison-window usage is under a cent — same
    /// dust-division rule as the headline percent.
    pub change_pct: Option<f64>,
    /// The business line this usage mostly drives, or "Unallocated".
    pub drives: String,
}

/// One service of the month-over-month card: this month against last.
pub struct ServiceMonthComparison {
    pub service: String,
    /// Net charged this month to date.
    pub current: f64,
    /// Net charged over the whole previous month.
    pub previous: f64,
    /// Percent change (signed); `None` when last month's base is under a
    /// cent — the same dust-division rule as the headline percent.
    pub change_pct: Option<f64>,
}

/// The MTD page's month-over-month card, from one
/// [`query::period_over_period`] pass. Net, not gross usage: the
/// comparison mirrors the headline spend number, so a credit-heavy month
/// reads as what it cost rather than what was consumed. (The movers table
/// stays the gross-usage per-service view.)
pub struct MonthOverMonth {
    /// Net charged this month to date.
    pub current_total: f64,
    /// Net charged over the whole previous month.
    pub previous_total: f64,
    /// Percent change of the totals (signed); `None` on a sub-cent
    /// previous-month base.
    pub change_pct: Option<f64>,
    /// The largest services of the two months combined, biggest first.
    pub services: Vec<ServiceMonthComparison>,
    /// The "why it changed" section; `None` when the decomposition query
    /// failed — the card still renders its totals without it.
    pub decomposition: Option<ChangeDecomposition>,
}

/// The direction badge of one service movement in the "why it changed"
/// section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovementBadge {
    Appeared,
    Vanished,
    Grown,
    Shrunk,
}

impl MovementBadge {
    pub fn label(self) -> &'static str {
        match self {
            Self::Appeared => "Appeared",
            Self::Vanished => "Vanished",
            Self::Grown => "Grown",
            Self::Shrunk => "Shrunk",
        }
    }
}

/// One charge-category delta of the "why it changed" section (usage, tax,
/// credits, refunds…).
pub struct CategoryDeltaRow {
    pub category: String,
    /// Net change of the category against last month (signed).
    pub delta: f64,
}

/// One service movement of the "why it changed" section.
pub struct ServiceMovementRow {
    pub service: String,
    pub badge: MovementBadge,
    /// Net change of the service against last month (signed).
    pub delta: f64,
}

/// The "why it changed" section of the month-over-month card, from one
/// [`query::cost_change_decomposition`] pass: what the delta is made of,
/// and whether those components add back up to it (Wealthfolio's
/// data-quality-as-UI reconciliation check).
pub struct ChangeDecomposition {
    /// Top category deltas, largest absolute first.
    pub categories: Vec<CategoryDeltaRow>,
    /// Top service movements, largest absolute first.
    pub movements: Vec<ServiceMovementRow>,
    /// What the category split fails to explain of the total delta.
    pub residual: f64,
    /// Whether `residual` is small enough to ignore. When false the card
    /// warns that the breakdown does not fully reconcile.
    pub reconciled: bool,
}

/// How bad one data-quality finding is; declaration order is severity
/// order, so `Critical` sorts last ascending and the strip reverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DataQualitySeverity {
    Info,
    Warning,
    Critical,
}

impl DataQualitySeverity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Warning => "Warning",
            Self::Critical => "Critical",
        }
    }
}

/// One row of the data-quality warnings strip.
pub struct DataQualityRow {
    /// The dismissal key, `{kind}:{billing_period}`; the Dismiss button
    /// stores it so the finding stays hidden for its period.
    pub key: String,
    pub severity: DataQualitySeverity,
    /// User-readable, with the numbers in it (count included).
    pub message: String,
    /// Reporting-currency amount behind the issue, if it has one.
    pub affected_amount: Option<f64>,
}

/// The band around card 2's month-end forecast.
pub struct ForecastBand {
    pub pessimistic: f64,
    pub optimistic: f64,
}

/// Everything the Overview page renders, including the labels: the view
/// renders, this decides what a range is called.
pub struct OverviewData {
    pub currency: String,
    pub range: Range,
    pub stats: OverviewStats,
    /// Card 1 label above the net number ("MONTH TO DATE", …).
    pub spend_label: &'static str,
    /// Text after card 1's change percent ("vs same day last month", …).
    pub change_caption: &'static str,
    /// Card 2: the month-end forecast for MTD, the window's mean usage for
    /// the rolling ranges.
    pub card2_label: &'static str,
    pub card2_value: f64,
    pub card2_caption: &'static str,
    /// The optimistic/pessimistic band around card 2's forecast; only a
    /// forecast range (MTD) carries one — a mean has no confidence band.
    pub forecast_band: Option<ForecastBand>,
    /// The flat "typical day" benchmark over the MTD chart, from the
    /// 6-month trailing daily average; `None` without the history to
    /// average (the chart then draws no benchmark line).
    pub chart_benchmark: Option<f64>,
    /// Data-quality findings for the current period, worst severity first;
    /// empty when the period is clean (or the check failed — a missing
    /// health strip must not take the page down).
    pub data_quality: Vec<DataQualityRow>,
    /// The header caption's range part; the view appends the currency.
    pub window_caption: String,
    /// Movers table delta column header ("VS LAST MONTH", …).
    pub movers_delta_header: &'static str,
    /// Movers card title.
    pub movers_title: &'static str,
    /// Movers amount column header ("MTD", "30D", "12M").
    pub movers_amount_header: String,
    pub chart_title: &'static str,
    pub chart_caption: &'static str,
    pub chart: SpendChart,
    pub business_lines: Vec<BusinessLineRow>,
    pub movers: Vec<MoverRow>,
    /// Month-over-month comparison; only the MTD range has a calendar
    /// month to compare, so the rolling ranges carry `None`.
    pub month_over_month: Option<MonthOverMonth>,
}

/// Chart title and caption of the daily ranges — MTD and 30d both plot
/// daily points against the 7-day trailing mean.
const DAILY_CHART_TITLE: &str = "Daily spend, all sources";
const DAILY_CHART_CAPTION: &str = "Actual against the 7-day trailing mean.";

/// Load the Overview page's data for a range. Blocking; wrap in
/// `smol::unblock`.
///
/// Net stays only in `stats.spend`; every trend and ranking below is gross
/// usage (see the module doc), so a credit-covered account still shows its
/// real burn instead of a ≈ $0 with wild percentages.
pub fn load_overview(range: Range) -> Result<OverviewData> {
    let now = Utc::now();
    match range {
        Range::Mtd => load_overview_mtd(now),
        _ => load_overview_window(range, now),
    }
}

/// The month-to-date page: calendar month to date, the change percent
/// against the same day last month, and the month-end forecast card.
fn load_overview_mtd(now: DateTime<Utc>) -> Result<OverviewData> {
    let current = BillingPeriod::containing(now);
    let previous = current.previous();
    let today = now.day();

    let mtd = query::total_for_period(&current.label())?;
    let (mtd_usage, mtd_credits) = query::usage_and_credits(&current.label())?;

    // Same-day usage MTD of the previous period: the sum of its days
    // 1..=today. A previous month shorter than today (e.g. February vs a
    // 31st) just runs out of days.
    let two_months_back = now - chrono::Duration::days(i64::from(today) + 31);
    let daily_all = query::daily_usage_all(two_months_back)?;
    let prev_label = previous.label();
    let prev_mtd: f64 = daily_all
        .iter()
        .filter(|(day, _)| {
            day.starts_with(&prev_label)
                && day
                    .get(8..10)
                    .and_then(|d| d.parse::<u32>().ok())
                    .is_some_and(|d| d <= today)
        })
        .map(|(_, amount)| amount)
        .sum();

    // A percentage against a sub-cent base is noise, not signal.
    let change_pct =
        (prev_mtd >= fmt::DUST_THRESHOLD).then(|| (mtd_usage - prev_mtd) / prev_mtd * 100.0);

    // The canonical run-rate forecast: MTD plus the mean daily rate —
    // measured from the period's first charge, so a mid-month cold start
    // does not drag it down — for the days left.
    let forecast = query::forecast_for_period(&current.label())?;
    // The confidence band around it; additive decoration, so a band
    // failure must not take the card down.
    let forecast_band = match query::forecast_bands_for_period(&current.label()) {
        Ok(bands) => Some(ForecastBand {
            pessimistic: bands.pessimistic,
            optimistic: bands.optimistic,
        }),
        Err(e) => {
            tracing::warn!("Could not compute the forecast bands: {}", e);
            None
        }
    };

    // Unallocated share of the current period's usage.
    let breakdown = query::tag_usage_breakdown(&current.label(), BUSINESS_LINE_TAG)?;
    let (unallocated_pct, unallocated_amount) = unallocated(&breakdown, mtd_usage);

    let (open, critical, warning) = alert_counts();

    // One point per day with data, against the 7-day trailing mean ending
    // the day before it. The daily series reaches back before the month
    // starts, so the first week's mean sees the previous month's days too.
    let by_day = daily_map(daily_all);
    let mut actual = Vec::new();
    let mut baseline = Vec::new();
    let mut day = current.start();
    let today_date = now.date_naive();
    while day <= today_date {
        if let Some(amount) = by_day.get(&day) {
            let label = day.format("%Y-%m-%d").to_string();
            actual.push(ChartPoint {
                label: label.clone(),
                amount: *amount,
            });
            baseline.push(ChartPoint {
                label,
                amount: analytics::trailing_mean(&by_day, day),
            });
        }
        day += chrono::Duration::days(1);
    }

    let business_lines = business_lines(breakdown);

    // Movers: current vs previous period usage per (provider, service).
    // One period-wide tag query resolves every mover's business line.
    let current_totals = query::provider_service_usage(&current.label())?;
    let previous_totals = query::provider_service_usage(&previous.label())?;
    let drives = drives_by_service(query::tag_usage_breakdown_by_service(
        &current.label(),
        BUSINESS_LINE_TAG,
    )?);
    let movers = movers(current_totals, &previous_totals, |provider, service| {
        Ok(drives
            .get(&(provider.to_string(), service.to_string()))
            .cloned()
            .unwrap_or_else(|| UNALLOCATED.to_string()))
    })?;

    let decomposition = match query::cost_change_decomposition(&current.label()) {
        Ok(decomposition) => Some(decomposition),
        Err(e) => {
            tracing::warn!("Could not decompose the month-over-month change: {}", e);
            None
        }
    };
    let month_over_month = month_over_month(
        query::period_over_period(&current.label())?,
        change_decomposition(decomposition),
    );

    // The "typical day" benchmark line over the chart: the flat value of
    // the 6-month trailing daily average. Additive, like the band.
    let chart_benchmark = benchmark_value(
        query::trailing_daily_average(i64::from(today), 6).unwrap_or_else(|e| {
            tracing::warn!("Could not compute the typical-day benchmark: {}", e);
            Vec::new()
        }),
    );

    let data_quality = load_data_quality(&current.label());

    Ok(OverviewData {
        currency: reporting_currency(),
        range: Range::Mtd,
        stats: OverviewStats {
            spend: mtd,
            usage: mtd_usage,
            credits: mtd_credits,
            change_pct,
            unallocated_pct,
            unallocated_amount,
            open_alerts: open,
            critical_alerts: critical,
            warning_alerts: warning,
        },
        spend_label: "MONTH TO DATE",
        change_caption: "vs same day last month",
        card2_label: "MONTH-END FORECAST",
        card2_value: forecast.forecast,
        card2_caption: "MTD plus the daily run rate for the rest of the month",
        forecast_band,
        chart_benchmark,
        data_quality,
        window_caption: Range::Mtd.header_caption(now),
        movers_delta_header: "VS LAST MONTH",
        movers_title: "Biggest movers",
        movers_amount_header: Range::Mtd.label().to_uppercase(),
        chart_title: DAILY_CHART_TITLE,
        chart_caption: DAILY_CHART_CAPTION,
        chart: SpendChart { actual, baseline },
        business_lines,
        movers,
        month_over_month: Some(month_over_month),
    })
}

/// A rolling-range page (30 days or 12 months): everything re-queries for
/// the range's window, with the window right before it as the comparison
/// base.
fn load_overview_window(range: Range, now: DateTime<Utc>) -> Result<OverviewData> {
    let ((since, until), (prior_since, prior_until)) = range.windows(now);

    let spend = query::total_between(since, until)?;
    let (usage, credits) = query::usage_and_credits_between(since, until)?;
    let (prior_usage, _) = query::usage_and_credits_between(prior_since, prior_until)?;
    let change_pct =
        (prior_usage >= fmt::DUST_THRESHOLD).then(|| (usage - prior_usage) / prior_usage * 100.0);

    let breakdown = query::tag_usage_breakdown_between(since, until, BUSINESS_LINE_TAG)?;
    let (unallocated_pct, unallocated_amount) = unallocated(&breakdown, usage);
    let (open, critical, warning) = alert_counts();

    let (
        spend_label,
        change_caption,
        movers_delta_header,
        chart_title,
        chart_caption,
        chart,
        card2,
    ) = match range {
        Range::Days30 => (
            "LAST 30 DAYS",
            "vs prior 30 days",
            "VS PRIOR 30D",
            DAILY_CHART_TITLE,
            DAILY_CHART_CAPTION,
            daily_chart(since, until)?,
            (
                "DAILY AVERAGE",
                usage / 30.0,
                "Mean daily usage over the last 30 days",
            ),
        ),
        Range::Months12 => (
            "LAST 12 MONTHS",
            "vs prior 12 months",
            "VS PRIOR 12M",
            "Monthly spend, all sources",
            "One point per month of gross usage.",
            monthly_chart(now)?,
            (
                "MONTHLY AVERAGE",
                usage / 12.0,
                "Mean monthly usage over the last 12 months",
            ),
        ),
        Range::Mtd => unreachable!("MTD loads through load_overview_mtd"),
    };
    let (card2_label, card2_value, card2_caption) = card2;

    let current_totals = query::provider_service_usage_between(since, until)?;
    let previous_totals = query::provider_service_usage_between(prior_since, prior_until)?;
    let movers = movers(current_totals, &previous_totals, |provider, service| {
        drives_between(since, until, provider, service)
    })?;

    // The strip reports the current period's health regardless of which
    // window the page shows.
    let data_quality = load_data_quality(&BillingPeriod::containing(now).label());

    Ok(OverviewData {
        currency: reporting_currency(),
        range,
        stats: OverviewStats {
            spend,
            usage,
            credits,
            change_pct,
            unallocated_pct,
            unallocated_amount,
            open_alerts: open,
            critical_alerts: critical,
            warning_alerts: warning,
        },
        spend_label,
        change_caption,
        card2_label,
        card2_value,
        card2_caption,
        forecast_band: None,
        chart_benchmark: None,
        data_quality,
        window_caption: range.header_caption(now),
        movers_delta_header,
        movers_title: "Biggest movers",
        movers_amount_header: range.label().to_uppercase(),
        chart_title,
        chart_caption,
        chart,
        business_lines: business_lines(breakdown),
        movers,
        month_over_month: None,
    })
}

/// `(YYYY-MM-DD, amount)` rows keyed by date; a row that does not parse
/// is dropped rather than failing the chart.
fn daily_map(rows: Vec<(String, f64)>) -> BTreeMap<NaiveDate, f64> {
    rows.into_iter()
        .filter_map(|(day, amount)| {
            NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                .ok()
                .map(|day| (day, amount))
        })
        .collect()
}

/// A daily series over `[since, until]`: one zero-filled point per day.
/// `with_baseline` adds the 7-day trailing mean — the cross-account
/// overview's baseline; a single account's mean is noisier than it is
/// informative, so the detail page goes without.
fn daily_series(
    by_day: &BTreeMap<NaiveDate, f64>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    with_baseline: bool,
) -> SpendChart {
    let last = until.date_naive();
    let mut actual = Vec::new();
    let mut baseline = Vec::new();
    let mut day = since.date_naive();
    while day <= last {
        let label = day.format("%Y-%m-%d").to_string();
        actual.push(ChartPoint {
            label: label.clone(),
            amount: by_day.get(&day).copied().unwrap_or(0.0),
        });
        if with_baseline {
            baseline.push(ChartPoint {
                label,
                amount: analytics::trailing_mean(by_day, day),
            });
        }
        day += chrono::Duration::days(1);
    }
    SpendChart { actual, baseline }
}

/// The rolling-range daily chart: the trailing-mean window reaches a week
/// before the range starts, so the first week's baseline is real data.
fn daily_chart(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<SpendChart> {
    let by_day = daily_map(query::daily_usage_all(since - chrono::Duration::days(7))?);
    Ok(daily_series(&by_day, since, until, true))
}

/// The last 12 calendar billing periods ending with the one containing
/// `now`, oldest first.
fn trailing_year_periods(now: DateTime<Utc>) -> Vec<BillingPeriod> {
    let mut periods = vec![BillingPeriod::containing(now)];
    for _ in 0..11 {
        periods.push(periods.last().expect("one period seeded").previous());
    }
    periods.reverse();
    periods
}

/// Midnight at the start of the oldest period — the `since` of the usage
/// query behind a 12-month chart.
fn year_since(periods: &[BillingPeriod]) -> DateTime<Utc> {
    periods[0]
        .start()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// A monthly series: one zero-filled point per period, and no baseline —
/// a trailing mean over twelve totals would just lag them.
fn monthly_series(by_period: &BTreeMap<String, f64>, periods: Vec<BillingPeriod>) -> SpendChart {
    let actual = periods
        .into_iter()
        .map(|period| {
            let label = period.label();
            let amount = by_period.get(&label).copied().unwrap_or(0.0);
            ChartPoint { label, amount }
        })
        .collect();
    SpendChart {
        actual,
        baseline: Vec::new(),
    }
}

/// The 12-month chart.
fn monthly_chart(now: DateTime<Utc>) -> Result<SpendChart> {
    let periods = trailing_year_periods(now);
    let by_period: BTreeMap<String, f64> = query::monthly_usage(year_since(&periods))?
        .into_iter()
        .collect();
    Ok(monthly_series(&by_period, periods))
}

/// The "Where it went" rows of a business-line breakdown.
fn business_lines(breakdown: Vec<(String, f64)>) -> Vec<BusinessLineRow> {
    breakdown
        .into_iter()
        .map(|(name, amount)| BusinessLineRow { name, amount })
        .collect()
}

/// The "Biggest movers" rows: the five services whose usage swung hardest
/// against the comparison window, by absolute percent change, with the
/// business line each mostly drives. Services with no meaningful prior
/// base have no percentage and rank below every one that does. `drives`
/// resolves that line, since the lookup differs between the period-keyed
/// and window-bounded queries.
fn movers(
    current_totals: Vec<(String, String, f64)>,
    previous_totals: &[(String, String, f64)],
    drives: impl Fn(&str, &str) -> Result<String>,
) -> Result<Vec<MoverRow>> {
    let mut ranked: Vec<(String, String, f64, Option<f64>)> = current_totals
        .into_iter()
        .map(|(provider, service, amount)| {
            let before = previous_totals
                .iter()
                .find(|(p, s, _)| p == &provider && s == &service)
                .map(|(_, _, amount)| *amount)
                .unwrap_or(0.0);
            let change_pct =
                (before >= fmt::DUST_THRESHOLD).then(|| (amount - before) / before * 100.0);
            (provider, service, amount, change_pct)
        })
        .collect();
    ranked.sort_by(|a, b| match (a.3, b.3) {
        (Some(a), Some(b)) => b.abs().total_cmp(&a.abs()),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.2.total_cmp(&a.2),
    });
    ranked.truncate(5);
    ranked
        .into_iter()
        .map(|(provider, service, amount, change_pct)| {
            Ok(MoverRow {
                drives: drives(&provider, &service)?,
                provider,
                service,
                amount,
                change_pct,
            })
        })
        .collect()
}

/// The business line each `(provider, service)` of a period mostly
/// drives, from one period-wide [`query::tag_usage_breakdown_by_service`]
/// pass: the first non-Unallocated value wins — the rows are largest
/// first, so that is the service's biggest tagged line — and a service
/// with no tagged usage maps to "Unallocated".
fn drives_by_service(rows: Vec<query::ServiceTagUsage>) -> BTreeMap<(String, String), String> {
    let mut drives = BTreeMap::new();
    for row in rows {
        if row.tag_value == UNALLOCATED {
            continue;
        }
        drives
            .entry((row.provider, row.service))
            .or_insert(row.tag_value);
    }
    drives
}

/// How many services the month-over-month card lists.
const TOP_MONTH_OVER_MONTH_SERVICES: usize = 5;

/// How many category deltas and service movements the "why it changed"
/// section lists.
const TOP_DECOMPOSITION_ROWS: usize = 5;

/// The month-over-month card from one [`query::period_over_period`] pass:
/// the totals' percent change, and the largest services of the two months
/// with each side's amount and percent change. `decomposition` is the
/// "why it changed" section, `None` when its query failed.
fn month_over_month(
    pop: query::PeriodOverPeriod,
    decomposition: Option<ChangeDecomposition>,
) -> MonthOverMonth {
    let change_pct = (pop.previous_total >= fmt::DUST_THRESHOLD)
        .then(|| (pop.current_total - pop.previous_total) / pop.previous_total * 100.0);

    let previous: BTreeMap<String, f64> = pop.previous_by_service.into_iter().collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut services: Vec<ServiceMonthComparison> = Vec::new();
    for (service, current) in pop.current_by_service {
        let before = previous.get(&service).copied().unwrap_or(0.0);
        seen.insert(service.clone());
        services.push(ServiceMonthComparison {
            change_pct: (before >= fmt::DUST_THRESHOLD)
                .then(|| (current - before) / before * 100.0),
            service,
            current,
            previous: before,
        });
    }
    // A service that vanished this month still earned its row: it is a
    // −100% mover.
    for (service, before) in &previous {
        if seen.contains(service) {
            continue;
        }
        services.push(ServiceMonthComparison {
            service: service.clone(),
            current: 0.0,
            previous: *before,
            change_pct: (*before >= fmt::DUST_THRESHOLD).then_some(-100.0),
        });
    }
    services.sort_by(|a, b| {
        b.current
            .max(b.previous)
            .total_cmp(&a.current.max(a.previous))
    });
    services.truncate(TOP_MONTH_OVER_MONTH_SERVICES);

    MonthOverMonth {
        current_total: pop.current_total,
        previous_total: pop.previous_total,
        change_pct,
        services,
        decomposition,
    }
}

/// The "why it changed" section from a
/// [`query::CostChangeDecomposition`]: the top category deltas and service
/// movements (both arrive largest-absolute first) plus the reconciliation
/// flags the card warns from. `None` in means `None` out.
fn change_decomposition(
    decomposition: Option<query::CostChangeDecomposition>,
) -> Option<ChangeDecomposition> {
    let decomposition = decomposition?;
    let badge = |kind: query::MovementKind| match kind {
        query::MovementKind::Appeared => MovementBadge::Appeared,
        query::MovementKind::Vanished => MovementBadge::Vanished,
        query::MovementKind::Grown => MovementBadge::Grown,
        query::MovementKind::Shrunk => MovementBadge::Shrunk,
    };
    Some(ChangeDecomposition {
        categories: decomposition
            .by_category
            .into_iter()
            .take(TOP_DECOMPOSITION_ROWS)
            .map(|row| CategoryDeltaRow {
                category: row.category,
                delta: row.delta,
            })
            .collect(),
        movements: decomposition
            .by_service
            .into_iter()
            .take(TOP_DECOMPOSITION_ROWS)
            .map(|row| ServiceMovementRow {
                badge: badge(row.kind),
                service: row.service,
                delta: row.delta,
            })
            .collect(),
        residual: decomposition.residual,
        reconciled: decomposition.reconciled,
    })
}

/// The flat "typical day" benchmark of a trailing-average series: the
/// series is flat within a month, so its last value is the line. `None`
/// when the series is empty or nothing but dust — the Wealthfolio rule is
/// to skip the overlay when there is no real history to average, and a
/// flat zero line is exactly that.
fn benchmark_value(series: Vec<(String, f64)>) -> Option<f64> {
    let (_, value) = series.last()?;
    (*value >= fmt::DUST_THRESHOLD).then_some(*value)
}

/// The dismissal keys the data-quality surfaces filter by. A failed read
/// logs and yields an empty set rather than taking the page down — the
/// findings simply all show.
pub(crate) fn dismissed_quality_keys() -> std::collections::HashSet<String> {
    db::dismissed_quality_issue_keys().unwrap_or_else(|e| {
        tracing::warn!("Could not read the dismissed data-quality issues: {}", e);
        std::collections::HashSet::new()
    })
}

/// The data-quality strip rows for a period, worst severity first. The
/// check is additive page decoration: a failed check logs and yields an
/// empty strip rather than taking the page down.
///
/// Findings the user already dismissed for this period are filtered out
/// here, at the data layer, so a refresh cannot resurrect them — the strip
/// reappears only when a load surfaces a finding that is new (a different
/// kind, or a new period).
fn load_data_quality(billing_period: &str) -> Vec<DataQualityRow> {
    let issues =
        query::data_quality_issues(billing_period, BUSINESS_LINE_TAG).unwrap_or_else(|e| {
            tracing::warn!("Could not check the period's data quality: {}", e);
            Vec::new()
        });
    let dismissed = dismissed_quality_keys();
    data_quality_rows(billing_period, issues, &dismissed)
}

/// The strip rows of a set of findings: dismissal keys assigned, dismissed
/// findings dropped, severity mapped, Critical first.
fn data_quality_rows(
    billing_period: &str,
    issues: Vec<query::DataQualityIssue>,
    dismissed: &std::collections::HashSet<String>,
) -> Vec<DataQualityRow> {
    let mut rows: Vec<DataQualityRow> = issues
        .into_iter()
        .map(|issue| DataQualityRow {
            key: issue.dismissal_key(billing_period),
            severity: match issue.severity {
                query::IssueSeverity::Info => DataQualitySeverity::Info,
                query::IssueSeverity::Warning => DataQualitySeverity::Warning,
                query::IssueSeverity::Critical => DataQualitySeverity::Critical,
            },
            message: issue.message,
            affected_amount: issue.affected_amount,
        })
        .filter(|row| !dismissed.contains(&row.key))
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.severity));
    rows
}

/// Persistently dismiss data-quality findings by their dismissal keys, so
/// they stay hidden for their billing period across sessions and
/// refreshes. Blocking, but a single-row write per key — the views call it
/// straight from the Dismiss click handler.
pub fn dismiss_quality_issues(keys: &[String]) -> Result<()> {
    for key in keys {
        db::dismiss_quality_issue(key)?;
    }
    Ok(())
}

/// [`drives_of_period`] over a charge-time window.
fn drives_between(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    provider: &str,
    service: &str,
) -> Result<String> {
    Ok(query::service_tag_usage_breakdown_between(
        since,
        until,
        provider,
        service,
        BUSINESS_LINE_TAG,
    )?
    .into_iter()
    .find(|(value, _)| value != UNALLOCATED)
    .map(|(value, _)| value)
    .unwrap_or_else(|| UNALLOCATED.to_string()))
}

/// The unallocated amount of a business-line breakdown and its share of
/// `usage` (0–100).
fn unallocated(breakdown: &[(String, f64)], usage: f64) -> (f64, f64) {
    let amount = breakdown
        .iter()
        .find(|(value, _)| value == UNALLOCATED)
        .map(|(_, amount)| *amount)
        .unwrap_or(0.0);
    let pct = if usage > 0.0 {
        amount / usage * 100.0
    } else {
        0.0
    };
    (pct, amount)
}

/// Open-alert counts for the alerts card: total, critical, warning.
fn alert_counts() -> (usize, usize, usize) {
    let open = alerts::open_alerts().unwrap_or_else(|e| {
        tracing::warn!("Could not read open alerts for the alerts card: {}", e);
        Vec::new()
    });
    let critical = open
        .iter()
        .filter(|a| a.severity == alerts::Severity::Critical)
        .count();
    let warning = open
        .iter()
        .filter(|a| a.severity == alerts::Severity::Warning)
        .count();
    (open.len(), critical, warning)
}

// ==================== Accounts ====================

/// The state badge of an account row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    Healthy,
    Anomaly,
    UntaggedSpend,
    LowBalance,
}

impl AccountState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::Anomaly => "Anomaly",
            Self::UntaggedSpend => "Unallocated spend",
            Self::LowBalance => "Low balance",
        }
    }
}

/// One row of the accounts table.
pub struct AccountRowData {
    pub id: String,
    pub name: String,
    /// Provider display name from the registry.
    pub provider: String,
    /// What the source reports, e.g. "Usage + cost" or "Balance only".
    pub source_kind: String,
    /// Month-to-date charges, in the reporting currency.
    pub mtd: f64,
    /// Latest balance and its own currency, for balance-reporting sources.
    pub balance: Option<(f64, String)>,
    /// When the account's rows were last ingested, if ever.
    pub last_sync: Option<DateTime<Utc>>,
    pub state: AccountState,
}

/// The "Paid API budget" card (AWS Cost Explorer spend on fetches).
pub struct BudgetCardData {
    /// Calls used this month.
    pub used: i64,
    /// Monthly call ceiling.
    pub ceiling: u32,
    /// What those calls cost at $0.01 each.
    pub spent: f64,
}

/// The "Raw payloads" card.
pub struct RawPayloadsData {
    /// Total size of the raw store, in bytes.
    pub bytes: u64,
    /// Where it lives, for the card body.
    pub path: PathBuf,
}

/// Everything the Accounts page renders.
pub struct AccountsData {
    pub currency: String,
    pub accounts: Vec<AccountRowData>,
    pub budget: BudgetCardData,
    pub raw: RawPayloadsData,
}

/// The ceiling of paid fetches the budget card measures against.
pub const API_CALL_CEILING: u32 = 100;

/// What one paid billing-API call costs, in USD.
pub const API_CALL_COST_USD: f64 = 0.01;

/// Load the Accounts page's data. Blocking; wrap in `smol::unblock`.
pub fn load_accounts() -> Result<AccountsData> {
    let now = Utc::now();
    let current = BillingPeriod::containing(now);
    let accounts = db::get_all_accounts()?;
    let ingests = query::last_ingests().unwrap_or_else(|e| {
        tracing::warn!("Could not read the last-ingest times: {}", e);
        Vec::new()
    });
    let open = alerts::open_alerts().unwrap_or_else(|e| {
        tracing::warn!("Could not read open alerts for the account badges: {}", e);
        Vec::new()
    });

    // Untagged usage per provider of the current period, for the badge
    // check: one period-wide query, built on first use and shared by every
    // account instead of being re-queried per account.
    let mut untagged_by_provider: Option<BTreeMap<String, f64>> = None;

    let mut rows = Vec::new();
    for account in accounts {
        let Some(descriptor) = account.descriptor() else {
            continue;
        };
        let provider = descriptor.id.to_string();

        let mtd = query::period_total(&ingest::period_key(&account, &current))?;
        let balance = query::latest_balance(&provider, &account.id)?;
        let last_sync = ingests
            .iter()
            .find(|(p, a, _)| p == &provider && a == &account.id)
            .map(|(_, _, at)| *at)
            .or(account.last_synced_at);

        // The worst applicable badge wins; the order is the mock's order of
        // severity.
        let state = account_state(
            &account,
            descriptor.is_snapshot(),
            &provider,
            &open,
            &current,
            &mut untagged_by_provider,
        )?;

        rows.push(AccountRowData {
            id: account.id.clone(),
            name: account.name.clone(),
            provider: descriptor.display_name.to_string(),
            source_kind: source_kind(descriptor),
            mtd,
            balance: balance.map(|b| (b.balance, b.currency)),
            last_sync,
            state,
        });
    }

    let used = query::api_fetches_this_month().unwrap_or_else(|e| {
        tracing::warn!("Could not count this month's paid fetches: {}", e);
        0
    });
    let (bytes, path) = match crate::cloud::raw::raw_dir_size() {
        Ok(bytes) => (bytes, crate::cloud::raw::raw_dir_path().unwrap_or_default()),
        Err(e) => {
            tracing::warn!("Could not measure the raw-payload store: {}", e);
            (0, PathBuf::new())
        }
    };

    Ok(AccountsData {
        currency: reporting_currency(),
        accounts: rows,
        budget: BudgetCardData {
            used,
            ceiling: API_CALL_CEILING,
            spent: used as f64 * API_CALL_COST_USD,
        },
        raw: RawPayloadsData { bytes, path },
    })
}

/// What a source reports, as the accounts table's kind column.
fn source_kind(descriptor: &registry::SourceDescriptor) -> String {
    match (
        descriptor.is_snapshot(),
        descriptor.fetches_from_api(),
        descriptor.imports_bill_file(),
    ) {
        (true, _, _) => "Balance only".to_string(),
        (false, true, true) => "Cost API + bill import".to_string(),
        (false, true, false) => "Cost API".to_string(),
        (false, false, true) => "Bill import".to_string(),
        (false, false, false) => "None".to_string(),
    }
}

/// The badge of one account row.
///
/// The untagged check is scoped to the account's provider, not the account
/// itself: tag breakdowns are not account-split today, so two accounts of
/// one provider share the badge. `untagged_by_provider` caches the
/// period-wide untagged sums across the accounts-table loop.
fn account_state(
    account: &crate::cloud::CloudAccount,
    is_snapshot: bool,
    provider: &str,
    open: &[AlertView],
    current: &BillingPeriod,
    untagged_by_provider: &mut Option<BTreeMap<String, f64>>,
) -> Result<AccountState> {
    // The worst applicable badge wins, in the mock's order of severity.
    let low_balance = is_snapshot
        && open.iter().any(|alert| {
            alert.kind == AlertKind::Balance
                && alert.context.get("account_id").and_then(|v| v.as_str()) == Some(&account.id)
        });
    if low_balance {
        return Ok(AccountState::LowBalance);
    }

    let anomaly = open.iter().any(|alert| {
        alert.kind == AlertKind::CostAnomaly
            && alert.context.get("provider").and_then(|v| v.as_str()) == Some(provider)
    });
    if anomaly {
        return Ok(AccountState::Anomaly);
    }

    let key = ingest::period_key(account, current);
    let total = query::period_total(&key)?;
    if total > 0.0 {
        if untagged_by_provider.is_none() {
            *untagged_by_provider = Some(untagged_usage_by_provider(&key.billing_period)?);
        }
        let untagged = untagged_by_provider
            .as_ref()
            .expect("just populated")
            .get(provider)
            .copied()
            .unwrap_or(0.0);
        if untagged / total > alerts::DEFAULT_UNTAGGED_THRESHOLD {
            return Ok(AccountState::UntaggedSpend);
        }
    }

    Ok(AccountState::Healthy)
}

/// Untagged usage of a period summed per provider — one period-wide
/// `untagged_detail` pass, grouped here so the per-account badge check
/// does not re-query for every account.
fn untagged_usage_by_provider(billing_period: &str) -> Result<BTreeMap<String, f64>> {
    let mut by_provider: BTreeMap<String, f64> = BTreeMap::new();
    for charge in query::untagged_detail(billing_period, BUSINESS_LINE_TAG, usize::MAX)? {
        *by_provider.entry(charge.provider).or_insert(0.0) += charge.amount;
    }
    Ok(by_provider)
}

// ==================== Attribution ====================

/// One step of the attribution path (Source → Service → Business line),
/// matching the Sankey's three columns.
pub struct PathStep {
    pub label: String,
}

/// A node in the Sankey. `column` is 0-based: 0 source, 1 service,
/// 2 business line.
pub struct SankeyNode {
    pub id: String,
    pub label: String,
    pub column: usize,
    /// Throughput in the reporting currency; every column sums to the
    /// period's gross usage.
    pub value: f64,
}

/// A link between two nodes, by node id.
pub struct SankeyLink {
    pub from: String,
    pub to: String,
    pub value: f64,
}

pub struct SankeyData {
    pub nodes: Vec<SankeyNode>,
    pub links: Vec<SankeyLink>,
}

/// One of the largest unattributed services, for the Unallocated card.
pub struct UnallocatedItem {
    pub provider: String,
    pub service: Option<String>,
    /// Free-text charge description; always `None` today, because the
    /// largest rows come from an aggregate by service, not raw charges.
    pub description: Option<String>,
    /// Untagged gross usage.
    pub amount: f64,
}

/// The "Unallocated" explainer card.
pub struct UnallocatedCardData {
    /// Untagged usage.
    pub amount: f64,
    /// Share of the period's usage that is untagged (0–100).
    pub pct: f64,
    /// The largest unattributed services, biggest first.
    pub largest: Vec<UnallocatedItem>,
    pub action: String,
}

/// Everything the Attribution page renders.
pub struct AttributionData {
    pub currency: String,
    pub path: Vec<PathStep>,
    pub sankey: SankeyData,
    pub unallocated: UnallocatedCardData,
}

/// Load the Attribution page's data. Blocking; wrap in `smol::unblock`.
///
/// The Sankey is three levels — source, service, business line —
/// because the ledger holds no API-key hop: charges arrive per account,
/// and the `business_line` tag is the only split below the service. Every
/// flow is gross usage, not net: net flows can be negative, which is
/// meaningless in a Sankey.
///
/// One Sankey row before it becomes nodes and links: the provider, the
/// service label the column shows, its gross usage, and how that usage
/// splits across business lines.
type ServiceFlow = (String, String, f64, Vec<(String, f64)>);

pub fn load_attribution() -> Result<AttributionData> {
    let period = BillingPeriod::containing(Utc::now()).label();
    let currency = reporting_currency();

    let path = ["Source", "Service", "Business line"]
        .into_iter()
        .map(|label| PathStep {
            label: label.to_string(),
        })
        .collect();

    // (provider, service) → tag rows, assembled link by link so every
    // column sums to the same usage total. One period-wide tag query
    // covers every service, merged "Other" tails included — filtering its
    // rows to one service gives exactly what a per-service breakdown
    // query would return.
    let services = query::provider_service_usage(&period)?;
    let mut tags_by_service: BTreeMap<(String, String), Vec<(String, f64)>> = BTreeMap::new();
    for row in query::tag_usage_breakdown_by_service(&period, BUSINESS_LINE_TAG)? {
        tags_by_service
            .entry((row.provider, row.service))
            .or_default()
            .push((row.tag_value, row.amount));
    }
    let tags_of = |provider: &str, service: &str| {
        tags_by_service
            .get(&(provider.to_string(), service.to_string()))
            .cloned()
            .unwrap_or_default()
    };

    // Per provider, keep the top services and merge the tail into one
    // "Other" node — tag breakdown included, so every column still sums to
    // the same usage total. Beyond a handful of nodes per column the
    // ribbons cross into mush and the thin ones are illegible anyway.
    const TOP_SERVICES_PER_PROVIDER: usize = 5;
    let mut by_provider: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    for (provider, service, amount) in services {
        by_provider
            .entry(provider)
            .or_default()
            .push((service, amount));
    }

    let mut services: Vec<ServiceFlow> = Vec::new();
    for (provider, mut rows) in by_provider {
        // Largest first: nodes stack bottom-up, so the biggest flows sit
        // low and parallel instead of crossing.
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        let tail = rows.split_off(TOP_SERVICES_PER_PROVIDER.min(rows.len()));
        let mut tail_sum = 0.0;
        let mut tail_tags: BTreeMap<String, f64> = BTreeMap::new();
        for (service, amount) in tail {
            tail_sum += amount;
            for (tag, tag_amount) in tags_of(&provider, &service) {
                *tail_tags.entry(tag).or_insert(0.0) += tag_amount;
            }
        }
        for (service, amount) in rows {
            services.push((
                provider.clone(),
                service.clone(),
                amount,
                tags_of(&provider, &service),
            ));
        }
        if tail_sum > 0.0 {
            services.push((
                provider,
                "Other".to_string(),
                tail_sum,
                tail_tags.into_iter().collect(),
            ));
        }
    }
    // Global largest-first so all three columns stack in matching orders.
    services.sort_by(|a, b| b.2.total_cmp(&a.2));

    let mut nodes: Vec<SankeyNode> = Vec::new();
    let mut links: Vec<SankeyLink> = Vec::new();
    let mut provider_totals: Vec<(String, f64)> = Vec::new();
    let mut line_totals: Vec<(String, f64)> = Vec::new();

    let add_total = |totals: &mut Vec<(String, f64)>, id: String, amount: f64| match totals
        .iter_mut()
        .find(|(existing, _)| *existing == id)
    {
        Some((_, total)) => *total += amount,
        None => totals.push((id, amount)),
    };

    for (provider, service, amount, tags) in &services {
        let src = format!("src-{provider}");
        let svc = format!("svc-{provider}-{service}");
        add_total(&mut provider_totals, src.clone(), *amount);

        // Thick ribbons attach first (lowest) on both ends.
        let mut tags = tags.clone();
        tags.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (tag, tag_amount) in tags {
            let line = format!("line-{tag}");
            add_total(&mut line_totals, line.clone(), tag_amount);
            links.push(SankeyLink {
                from: svc.clone(),
                to: line,
                value: tag_amount,
            });
        }
        // The service node's own throughput is its total, tagged or not.
        links.push(SankeyLink {
            from: src,
            to: svc.clone(),
            value: *amount,
        });
        nodes.push(SankeyNode {
            id: svc,
            label: service.clone(),
            column: 1,
            value: *amount,
        });
    }

    // Largest at the bottom in every column, matching the service column.
    provider_totals.sort_by(|a, b| b.1.total_cmp(&a.1));
    line_totals.sort_by(|a, b| b.1.total_cmp(&a.1));

    for (id, value) in provider_totals {
        let provider = id.trim_start_matches("src-");
        nodes.push(SankeyNode {
            label: registry::get(provider)
                .map(|descriptor| descriptor.display_name.to_string())
                .unwrap_or_else(|| provider.to_string()),
            id,
            column: 0,
            value,
        });
    }
    for (id, value) in line_totals {
        nodes.push(SankeyNode {
            label: id.trim_start_matches("line-").to_string(),
            id,
            column: 2,
            value,
        });
    }

    let (usage_total, _) = query::usage_and_credits(&period)?;
    let unallocated_amount = query::tag_usage_breakdown(&period, BUSINESS_LINE_TAG)?
        .into_iter()
        .find(|(value, _)| value == UNALLOCATED)
        .map(|(_, amount)| amount)
        .unwrap_or(0.0);
    const TOP_UNALLOCATED_SERVICES: usize = 3;
    let largest =
        query::untagged_usage_by_service(&period, BUSINESS_LINE_TAG, TOP_UNALLOCATED_SERVICES)?
            .into_iter()
            .map(|row| UnallocatedItem {
                provider: row.provider,
                service: row.service,
                description: None,
                amount: row.amount,
            })
            .collect();

    Ok(AttributionData {
        currency,
        path,
        sankey: SankeyData { nodes, links },
        unallocated: UnallocatedCardData {
            amount: unallocated_amount,
            pct: if usage_total > 0.0 {
                unallocated_amount / usage_total * 100.0
            } else {
                0.0
            },
            largest,
            action: "Write an allocation rule".to_string(),
        },
    })
}

// ==================== Alerts & Rules ====================

/// One filter chip above the alert list.
pub struct AlertFilterData {
    pub label: String,
    pub count: usize,
}

/// Everything the Alerts page renders.
pub struct AlertsData {
    pub filters: Vec<AlertFilterData>,
    pub open: Vec<AlertView>,
    /// Events that reached their final state in the current period, keyed
    /// on when they were resolved — not on when they were created — newest
    /// first.
    pub resolved_this_month: Vec<AlertView>,
}

/// Heading of the resolved-alerts section under the open ones.
pub const RESOLVED_SECTION_TITLE: &str = "RESOLVED THIS MONTH";

/// Load the Alerts page's data. Blocking; wrap in `smol::unblock`.
///
/// Loading is also what sweeps: resolution is re-derivable, so nothing
/// watches conditions between evaluations — the page resolves what stopped
/// holding since the last look before it lists anything. A failed sweep is
/// logged and the page loads anyway; a stale event is less harmful than no
/// Alerts page.
pub fn load_alerts() -> Result<AlertsData> {
    if let Err(e) = alerts::resolve_stale_alerts() {
        tracing::warn!(
            "Could not resolve stale alerts before loading the page: {}",
            e
        );
    }

    let open = alerts::open_alerts()?;
    let resolved = alerts::alerts_by_status(&[AlertStatus::Resolved])?;
    let current = BillingPeriod::containing(Utc::now()).label();
    let resolved_this_month: Vec<AlertView> = resolved
        .into_iter()
        // resolved_at, not created_at: an alert resolved this month belongs
        // to this month whenever it was raised. Events resolved before the
        // stamp existed (schema v6) carry none and drop out of the list.
        .filter(|alert| {
            alert
                .resolved_at
                .is_some_and(|at| at.format("%Y-%m").to_string() == current)
        })
        .collect();

    let count = |kind: AlertKind| open.iter().filter(|a| a.kind == kind).count();
    let filters = vec![
        AlertFilterData {
            label: "All".to_string(),
            count: open.len(),
        },
        AlertFilterData {
            label: AlertKind::CostAnomaly.label().to_string(),
            count: count(AlertKind::CostAnomaly),
        },
        AlertFilterData {
            label: AlertKind::Balance.label().to_string(),
            count: count(AlertKind::Balance),
        },
        AlertFilterData {
            label: AlertKind::UntaggedRatio.label().to_string(),
            count: count(AlertKind::UntaggedRatio),
        },
        AlertFilterData {
            label: AlertKind::Budget.label().to_string(),
            count: count(AlertKind::Budget),
        },
    ];

    Ok(AlertsData {
        filters,
        open,
        resolved_this_month,
    })
}

/// Everything the Rules page renders.
pub struct RulesData {
    pub rules: Vec<RuleView>,
}

/// Load the Rules page's data. Blocking; wrap in `smol::unblock`.
pub fn load_rules() -> Result<RulesData> {
    Ok(RulesData {
        rules: alerts::rules()?,
    })
}

// ==================== Alert & rule actions ====================
//
// Thin wrappers so a page never imports crate::alerts directly: the data
// layer is the whole seam between the views and the backend.

/// Enable or disable a rule. Blocking.
pub fn set_rule_enabled(id: &str, enabled: bool) -> Result<()> {
    alerts::set_rule_enabled(id, enabled)
}

/// Create an enabled rule of one of the three kinds. Blocking.
pub fn create_rule(kind: &str, name: &str, config: serde_json::Value) -> Result<RuleView> {
    alerts::create_rule(kind, name, config)
}

/// Delete a custom rule. Blocking.
pub fn delete_rule(id: &str) -> Result<()> {
    alerts::delete_rule(id)
}

/// Run every enabled rule against the ledger now — after a create or a
/// toggle, so a condition that already holds fires immediately. Blocking;
/// wrap in `smol::unblock`.
pub fn evaluate_rules() -> Result<usize> {
    alerts::evaluate()
}

/// Snooze an alert for `hours`. Blocking.
pub fn snooze_alert(id: &str, hours: i64) -> Result<()> {
    alerts::snooze_alert(id, hours)
}

/// Dismiss an alert. Blocking.
pub fn dismiss_alert(id: &str) -> Result<()> {
    alerts::dismiss_alert(id)
}

// ==================== Budgets ====================

/// Per-account budget consumption, for the Rules page's budget list.
/// Blocking.
pub fn load_budget_statuses() -> Result<Vec<BudgetStatus>> {
    db::get_all_budget_statuses()
}

/// `(id, name)` of every account, for the budget editor and the budget
/// rule's account picker. Blocking.
pub fn account_names() -> Result<Vec<(String, String)>> {
    Ok(db::get_all_accounts()?
        .into_iter()
        .map(|account| (account.id, account.name))
        .collect())
}

/// Save an account's budget. The original creation stamp is kept — a save
/// is an update. Blocking.
pub fn save_budget(account_id: &str, monthly_budget: f64, alert_threshold: f64) -> Result<()> {
    let existing = db::get_budget(account_id)?;
    let now = Utc::now();
    db::save_budget(&BudgetInfo {
        account_id: account_id.to_string(),
        monthly_budget,
        // Consumption is measured in the reporting currency, so that is the
        // currency a budget is recorded in.
        currency: reporting_currency(),
        alert_threshold,
        created_at: existing.map(|budget| budget.created_at).unwrap_or(now),
        updated_at: now,
    })
}

/// Remove an account's budget. Blocking.
pub fn delete_budget(account_id: &str) -> Result<()> {
    db::delete_budget(account_id)
}

/// Fetch an account's stale periods now — the Refresh button (`force:
/// false`) and Force refresh (`force: true`). Blocking; wrap in
/// `smol::unblock`, like the loaders.
pub fn refresh_account(
    account: &crate::cloud::CloudAccount,
    force: bool,
) -> Result<ingest::RefreshOutcome> {
    ingest::refresh_account(account, force)
}

/// Re-normalize every stored raw payload without fetching — the "Replay
/// normalization" button. Blocking.
pub fn replay_normalization() -> Result<ingest::ReplayOutcome> {
    ingest::replay_all()
}

// ==================== Account detail ====================

/// One row of the Account detail page's service table.
pub struct ServiceRow {
    pub name: String,
    /// Window gross usage in the reporting currency.
    pub amount: f64,
    /// Share of the window's usage, 0.0–1.0.
    pub share: f64,
    /// Change against the comparison window; `None` when the base is too
    /// small for a percentage to mean anything.
    pub change_pct: Option<f64>,
}

/// The Account detail page: one account's trend and service breakdown for
/// the selected range. Everything is gross usage except `spend`, which is
/// net — the same split the Overview makes.
pub struct AccountDetailData {
    pub account_name: String,
    /// The registry display name, e.g. "Amazon Web Services".
    pub provider: String,
    /// A balance-only source has no API-reported usage; its usage rows
    /// arrive through bill file import.
    pub is_snapshot: bool,
    pub currency: String,
    pub range: Range,
    pub window_caption: String,
    pub usage: f64,
    pub credits: f64,
    /// Net charged: usage plus (negative) credits.
    pub spend: f64,
    pub change_pct: Option<f64>,
    pub change_caption: &'static str,
    /// No baseline series: a single account's 7-day trailing mean is
    /// noisier than it is informative.
    pub chart: SpendChart,
    pub services: Vec<ServiceRow>,
}

/// Load the Account detail page's data. Blocking; wrap in `smol::unblock`.
pub fn load_account_detail(account_id: &str, range: Range) -> Result<AccountDetailData> {
    let now = Utc::now();
    let account = db::get_all_accounts()?
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| anyhow::anyhow!("No account {account_id}"))?;
    let provider = account.source_id.as_str().to_string();
    let descriptor = account.descriptor();

    let ((since, until), (prior_since, prior_until)) = range.windows(now);
    let (usage, credits) =
        query::usage_and_credits_of_between(&provider, account_id, since, until)?;

    // MTD compares against the same days of last month — a partial month
    // against a full one would always read as a drop; the rolling ranges
    // compare against the full prior window.
    let (change_pct, change_caption) = match range {
        Range::Mtd => {
            let today = now.day();
            let prev_label = BillingPeriod::containing(now).previous().label();
            let lookback = now - chrono::Duration::days(i64::from(today) + 31);
            let prior: f64 = query::daily_usage_of(&provider, account_id, lookback)?
                .into_iter()
                .filter(|(day, _)| {
                    day.starts_with(&prev_label)
                        && day
                            .get(8..10)
                            .and_then(|d| d.parse::<u32>().ok())
                            .is_some_and(|d| d <= today)
                })
                .map(|(_, amount)| amount)
                .sum();
            (
                (prior >= fmt::DUST_THRESHOLD).then(|| (usage - prior) / prior * 100.0),
                "vs same day last month",
            )
        }
        _ => {
            let (prior, _) = query::usage_and_credits_of_between(
                &provider,
                account_id,
                prior_since,
                prior_until,
            )?;
            let caption = match range {
                Range::Days30 => "vs prior 30 days",
                _ => "vs prior 12 months",
            };
            (
                (prior >= fmt::DUST_THRESHOLD).then(|| (usage - prior) / prior * 100.0),
                caption,
            )
        }
    };

    let chart = match range {
        Range::Months12 => account_monthly_chart(&provider, account_id, now)?,
        _ => account_daily_chart(&provider, account_id, since, until)?,
    };

    let current = query::service_usage_of_between(&provider, account_id, since, until)?;
    let previous =
        query::service_usage_of_between(&provider, account_id, prior_since, prior_until)?;
    let services = current
        .into_iter()
        .map(|(name, amount)| {
            let prior = previous
                .iter()
                .find(|(prior_name, _)| *prior_name == name)
                .map(|(_, amount)| *amount)
                .unwrap_or(0.0);
            ServiceRow {
                name,
                amount,
                share: if usage > 0.0 { amount / usage } else { 0.0 },
                change_pct: (prior >= fmt::DUST_THRESHOLD)
                    .then(|| (amount - prior) / prior * 100.0),
            }
        })
        .collect();

    Ok(AccountDetailData {
        account_name: account.name.clone(),
        provider: descriptor
            .map(|d| d.display_name)
            .unwrap_or(&provider)
            .to_string(),
        is_snapshot: descriptor.is_some_and(|d| d.is_snapshot()),
        currency: reporting_currency(),
        range,
        window_caption: range.header_caption(now),
        usage,
        credits,
        spend: usage + credits,
        change_pct,
        change_caption,
        chart,
        services,
    })
}

/// The detail page's daily chart, no baseline — the window query starts
/// at `since`; the trailing-mean lookback is the overview's only.
fn account_daily_chart(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<SpendChart> {
    let by_day = daily_map(query::daily_usage_of(provider, account_id, since)?);
    Ok(daily_series(&by_day, since, until, false))
}

/// The detail page's 12-month chart.
fn account_monthly_chart(
    provider: &str,
    account_id: &str,
    now: DateTime<Utc>,
) -> Result<SpendChart> {
    let periods = trailing_year_periods(now);
    let by_period: BTreeMap<String, f64> =
        query::monthly_usage_of(provider, account_id, year_since(&periods))?
            .into_iter()
            .collect();
    Ok(monthly_series(&by_period, periods))
}

// ==================== Query ====================

/// One starter query of the Query page's template picker. The SQL is what
/// runs; the category, title, and description only organize the picker.
pub struct QueryTemplate {
    pub category: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub sql: &'static str,
}

/// The built-in starter queries. Every amount query reads
/// `v_charge_normalized`, so amounts come out in the reporting currency —
/// noted in the description or as a `currency` column, since the query
/// page has no other place that says what the numbers mean.
const QUERY_TEMPLATES: [QueryTemplate; 14] = [
    QueryTemplate {
        category: "Spend overview",
        title: "Monthly spend",
        description: "Total billed spend per billing month, in reporting currency.",
        sql: "SELECT billing_period,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
GROUP BY billing_period, reporting_currency
ORDER BY billing_period",
    },
    QueryTemplate {
        category: "Spend overview",
        title: "Spend by provider × month",
        description: "How each month's spend splits across providers, in reporting currency.",
        sql: "SELECT billing_period,
       provider,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
GROUP BY billing_period, provider, reporting_currency
ORDER BY billing_period, spend DESC",
    },
    QueryTemplate {
        category: "Spend overview",
        title: "Daily spend, last 30 days",
        description: "Day-by-day spend over the last 30 days, in reporting currency.",
        sql: "SELECT CAST(charge_period_start AS DATE) AS day,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE charge_period_start >= CURRENT_DATE - INTERVAL 30 DAY
GROUP BY day, reporting_currency
ORDER BY day",
    },
    QueryTemplate {
        category: "Composition",
        title: "Top 20 services this month",
        description: "Which services cost the most this billing month, in reporting currency.",
        sql: "SELECT provider,
       service_name,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
GROUP BY provider, service_name, reporting_currency
ORDER BY spend DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Composition",
        title: "Spend by business line",
        description: "How spend splits across business lines, in reporting currency; charges without the business_line tag count as Unallocated.",
        sql: "SELECT coalesce(nullif(json_extract_string(tags, 'business_line'), ''), 'Unallocated') AS business_line,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
GROUP BY business_line, reporting_currency
ORDER BY spend DESC",
    },
    QueryTemplate {
        category: "Composition",
        title: "Unallocated spend by service",
        description: "Which services carry no business_line tag, and what they cost, in reporting currency.",
        sql: "SELECT provider,
       service_name,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE coalesce(nullif(json_extract_string(tags, 'business_line'), ''), '') = ''
GROUP BY provider, service_name, reporting_currency
ORDER BY spend DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Forecast & trends",
        title: "Month-end forecast",
        description: "This month's spend to date, the daily run rate since the first charge, and the projected month-end total, in reporting currency.",
        sql: "WITH mtd AS (
    SELECT SUM(billed_cost_base) AS cost,
           MIN(charge_period_start::DATE) AS first_day
    FROM v_charge_normalized
    WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
)
SELECT ROUND(cost, 2) AS month_to_date,
       ROUND(cost / (CURRENT_DATE - greatest(date_trunc('month', CURRENT_DATE)::DATE, first_day) + 1), 2) AS daily_rate,
       ROUND(cost + cost / (CURRENT_DATE - greatest(date_trunc('month', CURRENT_DATE)::DATE, first_day) + 1)
             * date_diff('day', CURRENT_DATE, last_day(CURRENT_DATE)), 2) AS month_end_forecast
FROM mtd",
    },
    QueryTemplate {
        category: "Forecast & trends",
        title: "Month over month by service",
        description: "This month vs last month per service, in reporting currency, biggest movers first.",
        sql: "SELECT service_name,
       reporting_currency AS currency,
       ROUND(SUM(CASE WHEN billing_period = strftime(CURRENT_DATE, '%Y-%m') THEN billed_cost_base END), 2) AS this_month,
       ROUND(SUM(CASE WHEN billing_period = strftime(CURRENT_DATE - INTERVAL 1 MONTH, '%Y-%m') THEN billed_cost_base END), 2) AS last_month,
       ROUND(coalesce(this_month, 0) - coalesce(last_month, 0), 2) AS change
FROM v_charge_normalized
WHERE billing_period IN (strftime(CURRENT_DATE, '%Y-%m'),
                         strftime(CURRENT_DATE - INTERVAL 1 MONTH, '%Y-%m'))
GROUP BY service_name, reporting_currency
ORDER BY greatest(coalesce(this_month, 0), coalesce(last_month, 0)) DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Composition",
        title: "Spend by region this month",
        description: "How this billing month's spend splits across regions, in reporting currency; charges with no region count as Other.",
        sql: "SELECT coalesce(region_id, 'Other') AS region,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
GROUP BY region, reporting_currency
ORDER BY spend DESC",
    },
    QueryTemplate {
        category: "Composition",
        title: "Spend by service category this month",
        description: "How this billing month's spend splits across service categories, in reporting currency; charges with no category count as Other.",
        sql: "SELECT coalesce(service_category, 'Other') AS category,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
GROUP BY category, reporting_currency
ORDER BY spend DESC",
    },
    QueryTemplate {
        category: "Composition",
        title: "Top 20 resources this month",
        description: "The individual resources costing the most this billing month, in reporting currency.",
        sql: "SELECT provider,
       service_name,
       coalesce(resource_name, resource_id) AS resource,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
  AND resource_id IS NOT NULL
GROUP BY provider, service_name, resource, reporting_currency
ORDER BY spend DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Composition",
        title: "Discount vs list price",
        description: "List-price spend vs actually billed spend per service this month, in reporting currency — how much discounts and credits shave off.",
        sql: "SELECT provider,
       service_name,
       reporting_currency AS currency,
       ROUND(SUM(list_cost * fx_rate), 2) AS list_spend,
       ROUND(SUM(billed_cost_base), 2) AS billed_spend,
       ROUND(SUM(list_cost * fx_rate) - SUM(billed_cost_base), 2) AS discount
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
  AND list_cost IS NOT NULL
  AND fx_rate IS NOT NULL
GROUP BY provider, service_name, reporting_currency
HAVING SUM(list_cost * fx_rate) > 0
ORDER BY discount DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Data health",
        title: "Recent ingest batches",
        description: "The 20 most recent ingest runs, with their channel, status, and row count.",
        sql: "SELECT provider,
       account_id,
       billing_period,
       channel,
       status,
       row_count,
       started_at
FROM ingest_batch
ORDER BY started_at DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Data health",
        title: "Latest balance snapshots",
        description: "Each account's most recently observed balance, one row per currency at that instant.",
        sql: "SELECT s.provider,
       s.account_id,
       s.observed_at,
       s.balance,
       s.currency
FROM fct_balance_snapshot s
JOIN (
    SELECT provider, account_id, MAX(observed_at) AS observed_at
    FROM fct_balance_snapshot
    GROUP BY provider, account_id
) latest
  ON s.provider = latest.provider
 AND s.account_id = latest.account_id
 AND s.observed_at = latest.observed_at
ORDER BY s.provider, s.account_id",
    },
];

/// The starter queries the Query page's template picker lists.
pub fn query_templates() -> &'static [QueryTemplate] {
    &QUERY_TEMPLATES
}

/// One ad-hoc query's result, ready to render: column names, which columns
/// are numeric (right-aligned), the rows as display strings, and whether
/// the row cap cut the tail off.
pub struct QueryResultData {
    pub columns: Vec<String>,
    pub numeric: Vec<bool>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// Run an ad-hoc SQL query against the ledger. Blocking; wrap in
/// `smol::unblock`. The error arrives as text because the page renders it
/// verbatim in the result pane.
pub fn run_adhoc_query(sql: String) -> std::result::Result<QueryResultData, String> {
    query::run_adhoc(&sql)
        .map(|result| QueryResultData {
            columns: result.columns,
            numeric: result.numeric,
            rows: result.rows,
            truncated: result.truncated,
            elapsed_ms: result.elapsed_ms,
        })
        .map_err(|e| e.to_string())
}

// ==================== Sidebar ====================

/// The sync summary in the status bar.
pub struct SyncStatus {
    /// The freshest successful ingest across all accounts, if any.
    pub last_synced_at: Option<DateTime<Utc>>,
    /// How many accounts are configured.
    pub source_count: usize,
    /// When the next automatic fetch is due: the last sync plus the
    /// configured freshness window. `None` before the first sync.
    pub next_fetch_at: Option<DateTime<Utc>>,
}

/// Load the status bar's sync status. Blocking; wrap in `smol::unblock`.
pub fn load_sync_status() -> Result<SyncStatus> {
    let accounts = db::get_all_accounts().unwrap_or_else(|e| {
        tracing::warn!("Could not list accounts for the sync status: {}", e);
        Vec::new()
    });
    let last = query::last_ingests()
        .unwrap_or_else(|e| {
            tracing::warn!(
                "Could not read the last-ingest times for the sync status: {}",
                e
            );
            Vec::new()
        })
        .into_iter()
        .map(|(_, _, at)| at)
        .max();

    let hours = i64::from(
        crate::config::load_config()
            .map(|settings| settings.refresh_interval_hours)
            .unwrap_or(crate::config::DEFAULT_REFRESH_INTERVAL_HOURS),
    );

    Ok(SyncStatus {
        last_synced_at: last,
        source_count: accounts.len(),
        next_fetch_at: last.map(|at| at + chrono::Duration::hours(hours)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Midnight UTC of a `YYYY-MM-DD`, for chart tests.
    fn at(date: NaiveDate) -> DateTime<Utc> {
        date.and_hms_opt(0, 0, 0)
            .expect("midnight exists")
            .and_utc()
    }

    #[test]
    fn daily_series_zero_fills_and_baselines_on_demand() {
        let first = NaiveDate::from_ymd_opt(2026, 9, 1).expect("a real date");
        let last = NaiveDate::from_ymd_opt(2026, 9, 3).expect("a real date");
        let mut by_day = BTreeMap::new();
        by_day.insert(
            NaiveDate::from_ymd_opt(2026, 9, 2).expect("a real date"),
            5.0,
        );

        let chart = daily_series(&by_day, at(first), at(last), false);
        let amounts: Vec<f64> = chart.actual.iter().map(|p| p.amount).collect();
        assert_eq!(amounts, vec![0.0, 5.0, 0.0]);
        assert!(chart.baseline.is_empty());

        let chart = daily_series(&by_day, at(first), at(last), true);
        assert_eq!(chart.baseline.len(), chart.actual.len());
    }

    #[test]
    fn drives_prefers_the_largest_tagged_line() {
        let rows = vec![
            query::ServiceTagUsage {
                provider: "aws".to_string(),
                service: "EC2".to_string(),
                tag_value: UNALLOCATED.to_string(),
                amount: 100.0,
            },
            query::ServiceTagUsage {
                provider: "aws".to_string(),
                service: "EC2".to_string(),
                tag_value: "payments".to_string(),
                amount: 50.0,
            },
            query::ServiceTagUsage {
                provider: "aws".to_string(),
                service: "S3".to_string(),
                tag_value: "analytics".to_string(),
                amount: 10.0,
            },
        ];
        let drives = drives_by_service(rows);
        assert_eq!(
            drives.get(&("aws".to_string(), "EC2".to_string())),
            Some(&"payments".to_string())
        );
        assert_eq!(
            drives.get(&("aws".to_string(), "S3".to_string())),
            Some(&"analytics".to_string())
        );
        assert!(!drives.contains_key(&("gcp".to_string(), "EC2".to_string())));
    }

    #[test]
    fn month_over_month_unions_both_months() {
        let mom = month_over_month(
            query::PeriodOverPeriod {
                current_total: 150.0,
                previous_total: 100.0,
                current_by_service: vec![("EC2".to_string(), 150.0)],
                previous_by_service: vec![("EC2".to_string(), 80.0), ("Retired".to_string(), 20.0)],
            },
            None,
        );
        assert_eq!(mom.change_pct, Some(50.0));

        assert_eq!(mom.services.len(), 2);
        let ec2 = &mom.services[0];
        assert_eq!(ec2.service, "EC2");
        assert_eq!((ec2.current, ec2.previous), (150.0, 80.0));
        assert_eq!(ec2.change_pct, Some(87.5));
        // A service that vanished this month is a −100% mover.
        let retired = &mom.services[1];
        assert_eq!(retired.service, "Retired");
        assert_eq!((retired.current, retired.previous), (0.0, 20.0));
        assert_eq!(retired.change_pct, Some(-100.0));
        assert!(mom.decomposition.is_none());
    }

    #[test]
    fn month_over_month_skips_delta_on_a_dust_base() {
        let mom = month_over_month(
            query::PeriodOverPeriod {
                current_total: 10.0,
                previous_total: 0.0,
                current_by_service: vec![("EC2".to_string(), 10.0)],
                previous_by_service: Vec::new(),
            },
            None,
        );
        assert_eq!(mom.change_pct, None);
        assert_eq!(mom.services[0].change_pct, None);
    }

    /// A decomposition with every movement kind and more rows than the
    /// section lists.
    fn sample_decomposition() -> query::CostChangeDecomposition {
        query::CostChangeDecomposition {
            billing_period: "2026-09".to_string(),
            previous_period: "2026-08".to_string(),
            current_total: 150.0,
            previous_total: 100.0,
            total_delta: 50.0,
            by_category: (0..7)
                .map(|i| query::CategoryDelta {
                    category: format!("Category{i}"),
                    current: 10.0 + f64::from(i),
                    previous: 10.0,
                    delta: f64::from(i),
                })
                .collect(),
            by_service: vec![
                query::ServiceMovement {
                    service: "NewSvc".to_string(),
                    kind: query::MovementKind::Appeared,
                    current: 40.0,
                    previous: 0.0,
                    delta: 40.0,
                },
                query::ServiceMovement {
                    service: "GoneSvc".to_string(),
                    kind: query::MovementKind::Vanished,
                    current: 0.0,
                    previous: 20.0,
                    delta: -20.0,
                },
                query::ServiceMovement {
                    service: "BigSvc".to_string(),
                    kind: query::MovementKind::Grown,
                    current: 100.0,
                    previous: 90.0,
                    delta: 10.0,
                },
                query::ServiceMovement {
                    service: "SmallSvc".to_string(),
                    kind: query::MovementKind::Shrunk,
                    current: 10.0,
                    previous: 15.0,
                    delta: -5.0,
                },
            ],
            residual: 2.5,
            reconciled: false,
        }
    }

    #[test]
    fn change_decomposition_maps_badges_and_truncates() {
        let why = change_decomposition(Some(sample_decomposition())).expect("Some in, Some out");
        // Seven category rows in, five out.
        assert_eq!(why.categories.len(), TOP_DECOMPOSITION_ROWS);
        assert_eq!(why.movements.len(), 4);
        let badges: Vec<MovementBadge> = why.movements.iter().map(|row| row.badge).collect();
        assert_eq!(
            badges,
            vec![
                MovementBadge::Appeared,
                MovementBadge::Vanished,
                MovementBadge::Grown,
                MovementBadge::Shrunk,
            ]
        );
        // The reconciliation flags pass through untouched.
        assert!(!why.reconciled);
        assert_eq!(why.residual, 2.5);
    }

    #[test]
    fn change_decomposition_passes_none_through() {
        assert!(change_decomposition(None).is_none());
    }

    #[test]
    fn benchmark_value_skips_empty_and_dust_series() {
        assert_eq!(benchmark_value(Vec::new()), None);
        // A fresh ledger averages to a flat zero line: no real history,
        // no overlay.
        let zeros = (1..=5)
            .map(|day| (format!("2026-09-{day:02}"), 0.0))
            .collect();
        assert_eq!(benchmark_value(zeros), None);
    }

    #[test]
    fn benchmark_value_takes_the_flat_value() {
        let series = (1..=5)
            .map(|day| (format!("2026-09-{day:02}"), 42.0))
            .collect();
        assert_eq!(benchmark_value(series), Some(42.0));
    }

    #[test]
    fn data_quality_rows_sort_critical_first() {
        let issue = |severity: query::IssueSeverity| query::DataQualityIssue {
            kind: query::DataQualityKind::UntaggedUsage,
            severity,
            message: format!("{severity:?}"),
            affected_amount: None,
            affected_count: 1,
        };
        let rows = data_quality_rows(
            "2026-09",
            vec![
                issue(query::IssueSeverity::Info),
                issue(query::IssueSeverity::Critical),
                issue(query::IssueSeverity::Warning),
            ],
            &std::collections::HashSet::new(),
        );
        let severities: Vec<DataQualitySeverity> = rows.iter().map(|row| row.severity).collect();
        assert_eq!(
            severities,
            vec![
                DataQualitySeverity::Critical,
                DataQualitySeverity::Warning,
                DataQualitySeverity::Info,
            ]
        );
    }

    #[test]
    fn data_quality_rows_drop_dismissed_keys_and_keep_new_ones() {
        let issue = |kind: query::DataQualityKind| query::DataQualityIssue {
            kind,
            severity: query::IssueSeverity::Warning,
            message: format!("{kind:?}"),
            affected_amount: Some(10.0),
            affected_count: 1,
        };
        // Untagged usage was dismissed for the period; a different kind and
        // a different period are not covered by that dismissal.
        let dismissed: std::collections::HashSet<String> =
            ["untagged_usage:2026-09".to_string()].into_iter().collect();
        let rows = data_quality_rows(
            "2026-09",
            vec![
                issue(query::DataQualityKind::UntaggedUsage),
                issue(query::DataQualityKind::MissingRegion),
            ],
            &dismissed,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "missing_region:2026-09");

        // The same dismissal does not touch the next period.
        let rows = data_quality_rows(
            "2026-10",
            vec![issue(query::DataQualityKind::UntaggedUsage)],
            &dismissed,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "untagged_usage:2026-10");
    }

    const TEMPLATE_CATEGORIES: [&str; 4] = [
        "Spend overview",
        "Forecast & trends",
        "Composition",
        "Data health",
    ];

    #[test]
    fn templates_exist() {
        assert!(!query_templates().is_empty());
    }

    #[test]
    fn templates_are_selects() {
        for template in query_templates() {
            let sql = template.sql.trim_start();
            assert!(!sql.is_empty(), "{}: SQL must not be empty", template.title);
            let starts_select = sql
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("select "));
            let starts_with = sql.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("with"));
            assert!(
                starts_select || starts_with,
                "{}: SQL must start with SELECT or WITH",
                template.title
            );
        }
    }

    #[test]
    fn template_categories_are_known() {
        for template in query_templates() {
            assert!(
                TEMPLATE_CATEGORIES.contains(&template.category),
                "{}: unknown category {:?}",
                template.title,
                template.category
            );
        }
    }
}
