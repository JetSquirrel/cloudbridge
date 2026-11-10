use super::*;

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

    pub fn label(self) -> SharedString {
        match self {
            Range::Mtd => tr!("range.mtd"),
            Range::Days30 => tr!("range.30d"),
            Range::Months12 => tr!("range.12m"),
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
            Range::Mtd => tr!(
                "range.mtd_caption",
                month = now.format(&rust_i18n::t!("range.month_format"))
            )
            .into(),
            Range::Days30 => tr!("overview.range_30d").into(),
            Range::Months12 => tr!("overview.range_12m").into(),
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
    pub fn label(self) -> SharedString {
        match self {
            Self::Appeared => tr!("movement.appeared"),
            Self::Vanished => tr!("movement.vanished"),
            Self::Grown => tr!("movement.grown"),
            Self::Shrunk => tr!("movement.shrunk"),
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
/// order, so `Critical` sorts last ascending and the card reverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DataQualitySeverity {
    Info,
    Warning,
    Critical,
}

impl DataQualitySeverity {
    /// The ledger's severity, for drawing a card row with the same
    /// finding row the Accounts page uses.
    pub fn issue_severity(self) -> query::IssueSeverity {
        match self {
            Self::Info => query::IssueSeverity::Info,
            Self::Warning => query::IssueSeverity::Warning,
            Self::Critical => query::IssueSeverity::Critical,
        }
    }
}

/// One row of the Overview Data health card.
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
    pub spend_label: SharedString,
    /// Text after card 1's change percent ("vs same day last month", …).
    pub change_caption: SharedString,
    /// Card 2: the month-end forecast for MTD, the window's mean usage for
    /// the rolling ranges.
    pub card2_label: SharedString,
    pub card2_value: f64,
    pub card2_caption: SharedString,
    /// The optimistic/pessimistic band around card 2's forecast; only a
    /// forecast range (MTD) carries one — a mean has no confidence band.
    pub forecast_band: Option<ForecastBand>,
    /// The flat "typical day" benchmark over the MTD chart, from the
    /// 6-month trailing daily average; `None` without the history to
    /// average (the chart then draws no benchmark line).
    pub chart_benchmark: Option<f64>,
    /// Charges in the range with no exchange rate, so missing from its totals.
    pub unconverted_charges: i64,
    /// Data-quality findings for the current period, worst severity first;
    /// empty when the period is clean (or the check failed — a missing
    /// Data health card must not take the page down).
    pub data_quality: Vec<DataQualityRow>,
    /// The header caption's range part; the view appends the currency.
    pub window_caption: String,
    /// Movers table delta column header ("VS LAST MONTH", …).
    pub movers_delta_header: SharedString,
    /// Movers card title.
    pub movers_title: SharedString,
    /// Movers amount column header ("MTD", "30D", "12M").
    pub movers_amount_header: String,
    pub chart_title: SharedString,
    pub chart_caption: SharedString,
    pub chart: SpendChart,
    pub business_lines: Vec<BusinessLineRow>,
    pub movers: Vec<MoverRow>,
    /// Month-over-month comparison; only the MTD range has a calendar
    /// month to compare, so the rolling ranges carry `None`.
    pub month_over_month: Option<MonthOverMonth>,
    /// Configured accounts, split by whether they are demo ones: the empty
    /// state tells "no accounts" from "nothing fetched yet", and demo rows
    /// get a notice that they are not a real bill.
    pub accounts: AccountCounts,
}

/// How many accounts are configured, real and demo.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountCounts {
    pub real: usize,
    pub demo: usize,
}

impl AccountCounts {
    pub fn load() -> Result<Self> {
        let mut counts = Self::default();
        for account in db::get_all_accounts()? {
            if account.id.starts_with(crate::demo_data::DEMO_PREFIX) {
                counts.demo += 1;
            } else {
                counts.real += 1;
            }
        }
        Ok(counts)
    }
}

/// Load the Overview page's data for a range. Blocking; wrap in
/// `smol::unblock`.
///
/// Net stays only in `stats.spend`; every trend and ranking below is gross
/// usage (see the module doc), so a credit-covered account still shows its
/// real burn instead of a ≈ $0 with wild percentages.
pub fn load_overview(range: Range) -> Result<OverviewData> {
    let now = Utc::now();
    let mut data = match range {
        Range::Mtd => load_overview_mtd(now),
        _ => load_overview_window(range, now),
    }?;
    data.accounts = AccountCounts::load()?;
    Ok(data)
}

/// Load or clear the demo ledger, then re-evaluate the alert rules so the
/// demo's spike and low balance show up (or their alerts go). Returns the
/// summary line for the UI. Blocking; wrap in `smol::unblock`.
pub fn set_demo_data(load: bool) -> Result<String> {
    let summary = if load {
        crate::ledger::demo::seed_demo()
    } else {
        crate::ledger::demo::clear_demo()
    }?;
    alerts::evaluate()?;
    Ok(summary)
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

    let unconverted_charges = query::unconverted_between(midnight_of(current.start()), now)?;
    Ok(OverviewData {
        unconverted_charges,
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
        spend_label: tr!("overview.card_mtd"),
        change_caption: tr!("overview.vs_same_day"),
        card2_label: tr!("overview.card_forecast"),
        card2_value: forecast.forecast,
        card2_caption: tr!("overview.card_forecast_caption"),
        forecast_band,
        chart_benchmark,
        data_quality,
        window_caption: Range::Mtd.header_caption(now),
        movers_delta_header: tr!("overview.vs_last_month"),
        movers_title: tr!("overview.biggest_movers"),
        movers_amount_header: Range::Mtd.label().to_uppercase(),
        chart_title: tr!("overview.daily_chart"),
        chart_caption: tr!("overview.daily_chart_caption"),
        chart: SpendChart { actual, baseline },
        business_lines,
        movers,
        month_over_month: Some(month_over_month),
        accounts: AccountCounts::default(),
    })
}

fn midnight_of(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
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
            tr!("overview.card_30d"),
            tr!("overview.vs_prior_30d"),
            tr!("overview.col_vs_prior_30d"),
            tr!("overview.daily_chart"),
            tr!("overview.daily_chart_caption"),
            daily_chart(since, until)?,
            (
                tr!("overview.card_daily_avg"),
                usage / 30.0,
                tr!("overview.card_daily_avg_caption"),
            ),
        ),
        Range::Months12 => (
            tr!("overview.card_12m"),
            tr!("overview.vs_prior_12m"),
            tr!("overview.col_vs_prior_12m"),
            tr!("overview.monthly_chart"),
            tr!("overview.monthly_chart_caption"),
            monthly_chart(now)?,
            (
                tr!("overview.card_monthly_avg"),
                usage / 12.0,
                tr!("overview.card_monthly_avg_caption"),
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

    // The Data health card reports the current period's health regardless of which
    // window the page shows.
    let data_quality = load_data_quality(&BillingPeriod::containing(now).label());

    let unconverted_charges = query::unconverted_between(since, until)?;
    Ok(OverviewData {
        unconverted_charges,
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
        movers_title: tr!("overview.biggest_movers"),
        movers_amount_header: range.label().to_uppercase(),
        chart_title,
        chart_caption,
        chart,
        business_lines: business_lines(breakdown),
        movers,
        month_over_month: None,
        accounts: AccountCounts::default(),
    })
}

/// `(YYYY-MM-DD, amount)` rows keyed by date; a row that does not parse
/// is dropped rather than failing the chart.
pub(super) fn daily_map(rows: Vec<(String, f64)>) -> BTreeMap<NaiveDate, f64> {
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
pub(super) fn daily_series(
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
pub(super) fn trailing_year_periods(now: DateTime<Utc>) -> Vec<BillingPeriod> {
    let mut periods = vec![BillingPeriod::containing(now)];
    for _ in 0..11 {
        periods.push(periods.last().expect("one period seeded").previous());
    }
    periods.reverse();
    periods
}

/// Midnight at the start of the oldest period — the `since` of the usage
/// query behind a 12-month chart.
pub(super) fn year_since(periods: &[BillingPeriod]) -> DateTime<Utc> {
    periods[0]
        .start()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// A monthly series: one zero-filled point per period, and no baseline —
/// a trailing mean over twelve totals would just lag them.
pub(super) fn monthly_series(
    by_period: &BTreeMap<String, f64>,
    periods: Vec<BillingPeriod>,
) -> SpendChart {
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
pub(super) fn drives_by_service(
    rows: Vec<query::ServiceTagUsage>,
) -> BTreeMap<(String, String), String> {
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
pub(super) const TOP_DECOMPOSITION_ROWS: usize = 5;

/// The month-over-month card from one [`query::period_over_period`] pass:
/// the totals' percent change, and the largest services of the two months
/// with each side's amount and percent change. `decomposition` is the
/// "why it changed" section, `None` when its query failed.
pub(super) fn month_over_month(
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
pub(super) fn change_decomposition(
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
pub(super) fn benchmark_value(series: Vec<(String, f64)>) -> Option<f64> {
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

/// The Data health card rows for a period, worst severity first. The
/// check is additive page decoration: a failed check logs and yields an
/// empty card rather than taking the page down.
///
/// Findings the user already dismissed for this period are filtered out
/// here, at the data layer, so a refresh cannot resurrect them — the card
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

/// The card rows of a set of findings: dismissal keys assigned, dismissed
/// findings dropped, severity mapped, Critical first.
pub(super) fn data_quality_rows(
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
