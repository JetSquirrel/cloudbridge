use super::*;

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
    pub change_caption: SharedString,
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
                tr!("overview.vs_same_day"),
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
                Range::Days30 => tr!("overview.vs_prior_30d"),
                _ => tr!("overview.vs_prior_12m"),
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
