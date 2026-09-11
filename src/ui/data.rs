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

use crate::alerts::{self, AlertKind, AlertStatus, AlertView, RuleView};
use crate::cloud::registry;
use crate::cloud::BillingPeriod;
use crate::ledger::query;
use crate::{db, ingest};

/// The tag that maps a charge to a business line.
///
/// Charges carry tags as a JSON object; the value under this key is what
/// the Overview "Where it went" rows, the Attribution Sankey's last hop,
/// and the unallocated-share rule all group by. A charge without it counts
/// as "Unallocated" everywhere.
pub const BUSINESS_LINE_TAG: &str = "business_line";

/// The reporting currency every amount below is expressed in, so a page
/// can format without asking config again.
fn reporting_currency() -> String {
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

/// One row of the "Biggest movers" table, ranked by window usage.
pub struct MoverRow {
    pub provider: String,
    pub service: String,
    /// Window gross usage.
    pub amount: f64,
    /// Percent change of usage vs the comparison window (signed). `None`
    /// when the service's comparison-window usage is under a cent — same
    /// dust-division rule as the headline percent.
    pub change_pct: Option<f64>,
    /// The business line this usage mostly drives, or "Untagged".
    pub drives: String,
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
    /// The header caption's range part; the view appends the currency.
    pub window_caption: String,
    /// Movers table delta column header ("VS LAST MONTH", …).
    pub movers_delta_header: &'static str,
    /// Movers card title ("Biggest movers this month", …).
    pub movers_title: &'static str,
    /// Movers amount column header ("MTD", "30D", "12M").
    pub movers_amount_header: &'static str,
    pub chart_title: &'static str,
    pub chart_caption: &'static str,
    pub chart: SpendChart,
    pub business_lines: Vec<BusinessLineRow>,
    pub movers: Vec<MoverRow>,
}

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
    let change_pct = (prev_mtd >= 0.01).then(|| (mtd_usage - prev_mtd) / prev_mtd * 100.0);

    // Forecast: usage MTD plus the recent run rate for the days left.
    let current_days: Vec<(u32, f64)> = daily_all
        .iter()
        .filter(|(day, _)| day.starts_with(&current.label()))
        .filter_map(|(day, amount)| {
            day.get(8..10)
                .and_then(|d| d.parse::<u32>().ok())
                .map(|d| (d, *amount))
        })
        .collect();
    let recent: Vec<f64> = current_days
        .iter()
        .map(|(_, amount)| *amount)
        .filter(|amount| *amount > 0.0)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(7)
        .collect();
    let run_rate = if recent.is_empty() {
        0.0
    } else {
        recent.iter().sum::<f64>() / recent.len() as f64
    };
    let days_in_month = days_in(current.year, current.month);
    let forecast = mtd_usage + run_rate * f64::from(days_in_month.saturating_sub(today));

    // Unallocated share of the current period's usage.
    let breakdown = query::tag_usage_breakdown(&current.label(), BUSINESS_LINE_TAG)?;
    let (unallocated_pct, unallocated_amount) = unallocated(&breakdown, mtd_usage);

    let (open, critical, warning) = alert_counts();

    // Baseline: the 7-day trailing mean ending the day before each point.
    let by_day: BTreeMap<u32, f64> = current_days.iter().copied().collect();
    let actual_days: Vec<(u32, f64)> = (1..=today)
        .filter_map(|d| by_day.get(&d).map(|amount| (d, *amount)))
        .collect();
    let baseline = actual_days
        .iter()
        .map(|(day, _)| {
            let window: Vec<f64> = (1..=7u32)
                .filter_map(|back| day.checked_sub(back))
                .map(|d| by_day.get(&d).copied().unwrap_or(0.0))
                .collect();
            ChartPoint {
                label: format!("{}-{day:02}", current.label()),
                amount: window.iter().sum::<f64>() / 7.0,
            }
        })
        .collect();
    let actual = actual_days
        .into_iter()
        .map(|(day, amount)| ChartPoint {
            label: format!("{}-{day:02}", current.label()),
            amount,
        })
        .collect();

    let business_lines = business_lines(breakdown);

    // Movers: current vs previous period usage per (provider, service).
    let current_totals = query::provider_service_usage(&current.label())?;
    let previous_totals = query::provider_service_usage(&previous.label())?;
    let period = current.label();
    let movers = movers(current_totals, &previous_totals, |provider, service| {
        drives_of_period(&period, provider, service)
    })?;

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
        card2_value: forecast,
        card2_caption: "MTD plus the mean of the last 7 days of burn",
        window_caption: Range::Mtd.header_caption(now),
        movers_delta_header: "VS LAST MONTH",
        movers_title: "Biggest movers this month",
        movers_amount_header: "MTD",
        chart_title: "Daily spend, all sources",
        chart_caption: "Actual against the 7-day trailing mean.",
        chart: SpendChart { actual, baseline },
        business_lines,
        movers,
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
    let change_pct = (prior_usage >= 0.01).then(|| (usage - prior_usage) / prior_usage * 100.0);

    let breakdown = query::tag_usage_breakdown_between(since, until, BUSINESS_LINE_TAG)?;
    let (unallocated_pct, unallocated_amount) = unallocated(&breakdown, usage);
    let (open, critical, warning) = alert_counts();

    let (
        spend_label,
        change_caption,
        movers_delta_header,
        movers_title,
        movers_amount_header,
        chart_title,
        chart_caption,
        chart,
        card2,
    ) = match range {
        Range::Days30 => (
            "LAST 30 DAYS",
            "vs prior 30 days",
            "VS PRIOR 30D",
            "Biggest movers, last 30 days",
            "30D",
            "Daily spend, all sources",
            "Actual against the 7-day trailing mean.",
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
            "Biggest movers, last 12 months",
            "12M",
            "Monthly spend, all sources",
            "One point per month of gross usage; no baseline.",
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
        window_caption: range.header_caption(now),
        movers_delta_header,
        movers_title,
        movers_amount_header,
        chart_title,
        chart_caption,
        chart,
        business_lines: business_lines(breakdown),
        movers,
    })
}

/// The rolling-range daily chart: one point per day of `[since, until)`,
/// zero-filled so the series spans the whole window, plus the 7-day
/// trailing mean whose window reaches a week before the range starts.
fn daily_chart(since: DateTime<Utc>, until: DateTime<Utc>) -> Result<SpendChart> {
    let by_day: BTreeMap<NaiveDate, f64> =
        query::daily_usage_all(since - chrono::Duration::days(7))?
            .into_iter()
            .filter_map(|(day, amount)| {
                NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                    .ok()
                    .map(|day| (day, amount))
            })
            .collect();

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
        let mean = (1..=7)
            .map(|back| {
                by_day
                    .get(&(day - chrono::Duration::days(back)))
                    .copied()
                    .unwrap_or(0.0)
            })
            .sum::<f64>()
            / 7.0;
        baseline.push(ChartPoint {
            label,
            amount: mean,
        });
        day += chrono::Duration::days(1);
    }

    Ok(SpendChart { actual, baseline })
}

/// The 12-month chart: one point per calendar month of the window,
/// zero-filled, and no baseline series.
fn monthly_chart(now: DateTime<Utc>) -> Result<SpendChart> {
    let mut periods = vec![BillingPeriod::containing(now)];
    for _ in 0..11 {
        periods.push(periods.last().expect("one period seeded").previous());
    }
    periods.reverse();

    let since = periods[0]
        .start()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();
    let by_period: BTreeMap<String, f64> = query::monthly_usage(since)?.into_iter().collect();
    let actual = periods
        .into_iter()
        .map(|period| {
            let label = period.label();
            let amount = by_period.get(&label).copied().unwrap_or(0.0);
            ChartPoint { label, amount }
        })
        .collect();

    Ok(SpendChart {
        actual,
        baseline: Vec::new(),
    })
}

/// The "Where it went" rows of a business-line breakdown.
fn business_lines(breakdown: Vec<(String, f64)>) -> Vec<BusinessLineRow> {
    breakdown
        .into_iter()
        .map(|(name, amount)| BusinessLineRow { name, amount })
        .collect()
}

/// The "Biggest movers" rows: the five largest services of the window,
/// each against its comparison-window usage, with the business line it
/// mostly drives. `drives` resolves that line, since the lookup differs
/// between the period-keyed and window-bounded queries.
fn movers(
    current_totals: Vec<(String, String, f64)>,
    previous_totals: &[(String, String, f64)],
    drives: impl Fn(&str, &str) -> Result<String>,
) -> Result<Vec<MoverRow>> {
    let mut rows = Vec::new();
    for (provider, service, amount) in current_totals.into_iter().take(5) {
        let before = previous_totals
            .iter()
            .find(|(p, s, _)| p == &provider && s == &service)
            .map(|(_, _, amount)| *amount)
            .unwrap_or(0.0);
        let change_pct = (before >= 0.01).then(|| (amount - before) / before * 100.0);
        rows.push(MoverRow {
            drives: drives(&provider, &service)?,
            provider,
            service,
            amount,
            change_pct,
        });
    }
    Ok(rows)
}

/// The business line a service's period usage mostly drives, or
/// "Untagged".
fn drives_of_period(period: &str, provider: &str, service: &str) -> Result<String> {
    Ok(
        query::service_tag_usage_breakdown(period, provider, service, BUSINESS_LINE_TAG)?
            .into_iter()
            .find(|(value, _)| value != "Unallocated")
            .map(|(value, _)| value)
            .unwrap_or_else(|| "Untagged".to_string()),
    )
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
    .find(|(value, _)| value != "Unallocated")
    .map(|(value, _)| value)
    .unwrap_or_else(|| "Untagged".to_string()))
}

/// The unallocated amount of a business-line breakdown and its share of
/// `usage` (0–100).
fn unallocated(breakdown: &[(String, f64)], usage: f64) -> (f64, f64) {
    let amount = breakdown
        .iter()
        .find(|(value, _)| value == "Unallocated")
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
    (open.len(), critical, open.len() - critical)
}

/// Days in a calendar month.
fn days_in(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).expect("a valid month");
    let next = chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("a valid month");
    (next - first).num_days() as u32
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
            Self::UntaggedSpend => "Untagged spend",
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
/// one provider share the badge.
fn account_state(
    account: &crate::cloud::CloudAccount,
    is_snapshot: bool,
    provider: &str,
    open: &[AlertView],
    current: &BillingPeriod,
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
        let untagged: f64 =
            query::untagged_detail(&key.billing_period, BUSINESS_LINE_TAG, usize::MAX)?
                .into_iter()
                .filter(|charge| charge.provider == provider)
                .map(|charge| charge.amount)
                .sum();
        if untagged / total > alerts::DEFAULT_UNTAGGED_THRESHOLD {
            return Ok(AccountState::UntaggedSpend);
        }
    }

    Ok(AccountState::Healthy)
}

// ==================== Attribution ====================

/// One step of the attribution path (Source → … → Business line).
pub struct PathStep {
    pub label: String,
    pub dimmed: bool,
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
/// The Sankey is three levels — source, model/service, business line —
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

    let path = ["Source", "Model / service", "Tag", "Business line"]
        .into_iter()
        .map(|label| PathStep {
            label: label.to_string(),
            dimmed: false,
        })
        .collect();

    // (provider, service) → tag rows, assembled link by link so every
    // column sums to the same usage total.
    let services = query::provider_service_usage(&period)?;

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
            for (tag, tag_amount) in
                query::service_tag_usage_breakdown(&period, &provider, &service, BUSINESS_LINE_TAG)?
            {
                *tail_tags.entry(tag).or_insert(0.0) += tag_amount;
            }
        }
        for (service, amount) in rows {
            let tags = query::service_tag_usage_breakdown(
                &period,
                &provider,
                &service,
                BUSINESS_LINE_TAG,
            )?;
            services.push((provider.clone(), service, amount, tags));
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
        nodes.push(SankeyNode {
            label: id.trim_start_matches("src-").to_string(),
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
        .find(|(value, _)| value == "Unallocated")
        .map(|(_, amount)| amount)
        .unwrap_or(0.0);
    let largest = query::untagged_usage_by_service(&period, BUSINESS_LINE_TAG, 3)?
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
                (prior >= 0.01).then(|| (usage - prior) / prior * 100.0),
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
                (prior >= 0.01).then(|| (usage - prior) / prior * 100.0),
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
                change_pct: (prior >= 0.01).then(|| (amount - prior) / prior * 100.0),
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

/// The detail page's daily chart: one zero-filled point per day of the
/// window, no baseline.
fn account_daily_chart(
    provider: &str,
    account_id: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<SpendChart> {
    let by_day: BTreeMap<NaiveDate, f64> = query::daily_usage_of(provider, account_id, since)?
        .into_iter()
        .filter_map(|(day, amount)| {
            NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                .ok()
                .map(|day| (day, amount))
        })
        .collect();

    let last = until.date_naive();
    let mut day = since.date_naive();
    let mut actual = Vec::new();
    while day <= last {
        actual.push(ChartPoint {
            label: day.format("%Y-%m-%d").to_string(),
            amount: by_day.get(&day).copied().unwrap_or(0.0),
        });
        day += chrono::Duration::days(1);
    }

    Ok(SpendChart {
        actual,
        baseline: Vec::new(),
    })
}

/// The detail page's 12-month chart: one zero-filled point per calendar
/// month, no baseline.
fn account_monthly_chart(
    provider: &str,
    account_id: &str,
    now: DateTime<Utc>,
) -> Result<SpendChart> {
    let mut periods = vec![BillingPeriod::containing(now)];
    for _ in 0..11 {
        periods.push(periods.last().expect("one period seeded").previous());
    }
    periods.reverse();

    let since = periods[0]
        .start()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();
    let by_period: BTreeMap<String, f64> = query::monthly_usage_of(provider, account_id, since)?
        .into_iter()
        .collect();
    let actual = periods
        .into_iter()
        .map(|period| {
            let label = period.label();
            let amount = by_period.get(&label).copied().unwrap_or(0.0);
            ChartPoint { label, amount }
        })
        .collect();

    Ok(SpendChart {
        actual,
        baseline: Vec::new(),
    })
}

// ==================== Sidebar ====================

/// The sync summary in the sidebar footer.
pub struct SyncStatus {
    /// The freshest successful ingest across all accounts, if any.
    pub last_synced_at: Option<DateTime<Utc>>,
    /// How many accounts are configured.
    pub source_count: usize,
    /// When the next automatic fetch is due: the last sync plus the
    /// configured freshness window. `None` before the first sync.
    pub next_fetch_at: Option<DateTime<Utc>>,
}

/// Load the sidebar's sync status. Blocking; wrap in `smol::unblock`.
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
