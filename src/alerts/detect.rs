use super::*;

// ==================== Cost growth anomaly ====================

/// What [`detect_breach`] found: a streak of days each costing more than
/// `multiplier` × the mean of the seven days before them.
#[derive(Debug, Clone, PartialEq)]
pub struct Breach {
    /// First day of the streak.
    pub first_day: NaiveDate,
    /// Latest day of the streak — always the latest day the service has
    /// data for.
    pub last_day: NaiveDate,
    /// What `last_day` cost.
    pub last_amount: f64,
    /// The trailing 7-day mean `last_day` is compared against.
    pub baseline: f64,
}

impl Breach {
    pub fn streak_days(&self) -> i64 {
        (self.last_day - self.first_day).num_days() + 1
    }
}

/// Whether the latest days of a service's daily costs are a run of
/// breaches: each of the last `consecutive` days above `multiplier` × its
/// own trailing 7-day mean.
///
/// A service with a zero baseline never fires: appearing in the ledger for
/// the first time is not "growth above baseline", it is a new workload —
/// the untagged-ratio rule is the one that watches those.
pub fn detect_breach(
    daily: &BTreeMap<NaiveDate, f64>,
    multiplier: f64,
    consecutive: u64,
) -> Option<Breach> {
    let (&last, _) = daily.iter().next_back()?;
    let baseline = analytics::trailing_mean(daily, last);
    if baseline <= 0.0 || daily[&last] <= baseline * multiplier {
        return None;
    }

    // Walk the streak back from the latest day while each earlier day
    // breached its own baseline.
    let mut first = last;
    while let Some(previous) = first.pred_opt() {
        let mean = analytics::trailing_mean(daily, previous);
        match daily.get(&previous) {
            Some(&amount) if mean > 0.0 && amount > mean * multiplier => first = previous,
            _ => break,
        }
    }

    let breach = Breach {
        first_day: first,
        last_day: last,
        last_amount: daily[&last],
        baseline,
    };
    (breach.streak_days() >= consecutive as i64).then_some(breach)
}

/// [`detect_breach`] with OptScale's import-lag tolerance: bill imports run
/// about a day behind, so when the check against the latest day with data
/// finds no breach, re-check as of yesterday before concluding. This is what
/// keeps a part-imported today — a small amount so far — from breaking a
/// streak that completed yesterday.
pub(super) fn detect_breach_with_lag(
    daily: &BTreeMap<NaiveDate, f64>,
    multiplier: f64,
    consecutive: u64,
    now: DateTime<Utc>,
) -> Option<Breach> {
    detect_breach(daily, multiplier, consecutive).or_else(|| {
        let yesterday = now.date_naive().pred_opt()?;
        let lagged: BTreeMap<NaiveDate, f64> = daily
            .range(..=yesterday)
            .map(|(day, amount)| (*day, *amount))
            .collect();
        detect_breach(&lagged, multiplier, consecutive)
    })
}

/// [`query::daily_totals_by_service_of`] for a single account, read through the
/// evaluation's own ledger connection rather than the global one.
pub(super) fn daily_totals_for_account_of(
    conn: &Connection,
    account_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<query::ServiceDailyTotal>> {
    query::daily_totals_for_account_of(conn, account_id, since)
}

pub(super) fn evaluate_cost_anomalies(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    now: DateTime<Utc>,
) -> Result<usize> {
    let multiplier = config_f64(rule, "multiplier", 2.5);
    let consecutive = config_u64(rule, "consecutive_days", 2);
    let currency = reporting_currency();
    // When the rule names an account, only that account's days are checked.
    let account_id = config_str(rule, "account_id");

    let rows = match account_id.as_deref() {
        Some(account) => {
            daily_totals_for_account_of(ledger, account, now - Duration::days(ANOMALY_WINDOW_DAYS))?
        }
        None => {
            query::daily_totals_by_service_of(ledger, now - Duration::days(ANOMALY_WINDOW_DAYS))?
        }
    };

    // Group days per (provider, service).
    let mut series: BTreeMap<(String, String), BTreeMap<NaiveDate, f64>> = BTreeMap::new();
    for row in rows {
        let Ok(day) = NaiveDate::parse_from_str(&row.day, "%Y-%m-%d") else {
            continue;
        };
        series
            .entry((row.provider, row.service))
            .or_default()
            .insert(day, row.amount);
    }

    let mut created = 0;
    for ((provider, service), daily) in series {
        let Some(breach) = detect_breach_with_lag(&daily, multiplier, consecutive, now) else {
            continue;
        };

        // A scoped rule's key names its account, so it never collides with
        // the unscoped rule watching the same (provider, service).
        let dedupe_key = match account_id.as_deref() {
            Some(account) => format!("cost|{account}|{provider}|{service}|{}", breach.last_day),
            None => format!("cost|{}|{}|{}", provider, service, breach.last_day),
        };
        if !should_fire(app, &dedupe_key, now)? {
            continue;
        }

        let month_end = breach.last_amount * f64::from(days_in_month(breach.last_day));
        let ratio = breach.last_amount / breach.baseline;
        let mut context = json!({
            "provider": provider.clone(),
            "service": service.clone(),
            "multiplier": multiplier,
            "consecutive_days": consecutive,
        });
        if let Some(account) = account_id.as_deref() {
            context["account_id"] = json!(account);
        }
        let event = AlertEvent {
            id: uuid::Uuid::new_v4().to_string(),
            rule_id: rule.id.clone(),
            severity: Severity::Critical,
            // Written in the interface language of the moment; a stored
            // event keeps the language it fired in.
            title: rust_i18n::t!(
                "alert_msg.anomaly_title",
                provider = provider,
                service = service
            )
            .into_owned(),
            body: rust_i18n::t!(
                "alert_msg.anomaly_body",
                provider = provider,
                service = service,
                amount = fmt_amount(breach.last_amount, &currency),
                day = breach.last_day,
                baseline = fmt_amount(breach.baseline, &currency),
                ratio = format!("{ratio:.1}"),
                streak = breach.streak_days(),
                first = breach.first_day,
                month_end = fmt_amount(month_end, &currency)
            )
            .into_owned(),
            fields_json: json!({
                "fields": [
                    { "label": rust_i18n::t!("alert_msg.f_provider"), "value": provider.clone() },
                    { "label": rust_i18n::t!("alert_msg.f_service"), "value": service.clone() },
                    { "label": rust_i18n::t!("alert_msg.f_24h_cost"), "value": fmt_amount(breach.last_amount, &currency) },
                    { "label": rust_i18n::t!("alert_msg.f_baseline"), "value": fmt_amount(breach.baseline, &currency) },
                    { "label": rust_i18n::t!("alert_msg.f_first_breach"), "value": breach.first_day.to_string() },
                    { "label": rust_i18n::t!("alert_msg.f_month_end_held"), "value": fmt_amount(month_end, &currency) },
                ],
                "context": context,
            })
            .to_string(),
            stat_json: Some(
                json!({
                    "label": rust_i18n::t!("alert_msg.billed_on", day = breach.last_day),
                    "value": fmt_amount(breach.last_amount, &currency),
                })
                .to_string(),
            ),
            created_at: now,
            status: AlertStatus::Open,
            snoozed_until: None,
            dedupe_key,
            resolved_at: None,
        };

        fire(app, rule, event, now)?;
        created += 1;
    }

    Ok(created)
}

// ==================== Balance floor ====================

pub(super) fn evaluate_balance_floors(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    now: DateTime<Utc>,
) -> Result<usize> {
    let default_floor = config_f64(rule, "floor", 200.0);

    let mut created = 0;
    for account in db::get_all_accounts_of(app)? {
        let Some(descriptor) = account.descriptor() else {
            continue;
        };
        if !descriptor.is_snapshot() {
            continue;
        }

        // The account's budget IS its floor when it has one; see the rule's
        // config.
        let floor = db::get_budget_of(app, &account.id)?
            .map(|budget| budget.monthly_budget)
            .unwrap_or(default_floor);

        let provider = descriptor.id;
        let Some(balance) = query::latest_balance_of(ledger, provider, &account.id)? else {
            continue;
        };
        if balance.balance >= floor {
            continue;
        }

        let dedupe_key = format!("balance|{}|{}", provider, account.id);
        // Events from before this key had no date in it carry it as a suffix.
        let same_balance = |event: &AlertEvent| {
            event.dedupe_key == dedupe_key
                || event.dedupe_key.starts_with(&format!("{dedupe_key}|"))
        };
        let mut live: Vec<AlertEvent> =
            db::get_alert_events_of(app, &[AlertStatus::Open, AlertStatus::Snoozed])?
                .into_iter()
                .filter(|event| same_balance(event))
                .collect();
        live.sort_by_key(|event| std::cmp::Reverse(event.created_at));
        let existing = (!live.is_empty()).then(|| live.remove(0));
        // Older cards for this balance say nothing the newest does not.
        for older in live {
            db::set_alert_event_status_to(app, &older.id, AlertStatus::Resolved, None)?;
        }
        // Dismissing quiets it for the day; it comes back if it still holds.
        if existing.is_none()
            && db::get_alert_events_of(app, &[AlertStatus::Dismissed])?
                .iter()
                .any(|event| {
                    same_balance(event)
                        && event
                            .resolved_at
                            .is_some_and(|at| at.date_naive() == now.date_naive())
                })
        {
            continue;
        }

        let burn = query::balance_burn_of(ledger, provider, &account.id, BURN_WINDOW_DAYS)?;
        let days_left = burn
            .filter(|burn| *burn > 0.0)
            .map(|burn| balance.balance / burn);
        let projection = match (burn, days_left) {
            (Some(burn), Some(days_left)) if burn > 0.0 => rust_i18n::t!(
                "alert_msg.balance_projection",
                days = BURN_WINDOW_DAYS,
                burn = fmt_amount(burn, &balance.currency),
                left = fmt_days(days_left)
            )
            .into_owned(),
            _ => String::new(),
        };

        let mut event = AlertEvent {
            id: uuid::Uuid::new_v4().to_string(),
            rule_id: rule.id.clone(),
            severity: Severity::Warning,
            title: rust_i18n::t!("alert_msg.balance_title", source = descriptor.display_name)
                .into_owned(),
            body: rust_i18n::t!(
                "alert_msg.balance_body",
                balance = fmt_amount(balance.balance, &balance.currency),
                floor = fmt_amount(floor, &balance.currency),
                projection = projection
            )
            .into_owned(),
            fields_json: json!({
                "fields": [
                    { "label": rust_i18n::t!("alert_msg.f_account"), "value": account.name },
                    { "label": rust_i18n::t!("alert_msg.f_balance"), "value": fmt_amount(balance.balance, &balance.currency) },
                    { "label": rust_i18n::t!("alert_msg.f_floor"), "value": fmt_amount(floor, &balance.currency) },
                ],
                "context": {
                    "provider": provider,
                    "account_id": account.id,
                    "floor": floor,
                },
            })
            .to_string(),
            stat_json: days_left
                .map(|days| json!({ "label": rust_i18n::t!("alert_msg.runs_dry_in"), "value": fmt_days(days) }).to_string()),
            created_at: now,
            status: AlertStatus::Open,
            snoozed_until: None,
            dedupe_key,
            resolved_at: None,
        };

        match existing {
            // The same card with today's figures. It counts as new, and so
            // notifies, only when a snooze has run out.
            Some(existing) => {
                let woke = existing.status == AlertStatus::Snoozed
                    && existing.snoozed_until.is_none_or(|until| until <= now);
                event.id = existing.id;
                event.created_at = existing.created_at;
                if !woke {
                    event.status = existing.status;
                    event.snoozed_until = existing.snoozed_until;
                }
                db::insert_alert_event_to(app, &event)?;
                if woke {
                    created += 1;
                }
            }
            None => {
                fire(app, rule, event, now)?;
                created += 1;
            }
        }
    }

    Ok(created)
}

// ==================== Untagged ratio ====================

/// The share of a tag breakdown that reached no value, 0.0–1.0. An empty
/// period has no unallocated share at all.
pub fn untagged_share(breakdown: &[(String, f64)]) -> f64 {
    let total: f64 = breakdown.iter().map(|(_, amount)| amount).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let unallocated: f64 = breakdown
        .iter()
        .filter(|(value, _)| value == "Unallocated")
        .map(|(_, amount)| amount)
        .sum();
    unallocated / total
}

pub(super) fn evaluate_untagged_ratio(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    now: DateTime<Utc>,
) -> Result<usize> {
    let threshold = config_f64(rule, "threshold", DEFAULT_UNTAGGED_THRESHOLD);

    let current = crate::cloud::BillingPeriod::containing(now);
    let current_share = untagged_share(&query::tag_breakdown_of(
        ledger,
        &current.label(),
        BUSINESS_LINE_TAG,
        None,
    )?);
    let previous_share = untagged_share(&query::tag_breakdown_of(
        ledger,
        &current.previous().label(),
        BUSINESS_LINE_TAG,
        None,
    )?);

    // Only a share that is both over the line and growing: a high but
    // shrinking share is being fixed, not neglected.
    if current_share <= threshold || current_share <= previous_share {
        return Ok(0);
    }

    let dedupe_key = format!("untagged|{}", now.date_naive());
    if !should_fire(app, &dedupe_key, now)? {
        return Ok(0);
    }

    let currency = reporting_currency();
    let total = query::total_for_period_of(ledger, &current.label())?;
    let event = AlertEvent {
        id: uuid::Uuid::new_v4().to_string(),
        rule_id: rule.id.clone(),
        severity: Severity::Warning,
        title: rust_i18n::t!("alert_msg.untagged_title").into_owned(),
        body: rust_i18n::t!(
            "alert_msg.untagged_body",
            share = format!("{:.0}", current_share * 100.0),
            period = current.label(),
            amount = fmt_amount(current_share * total, &currency),
            previous = format!("{:.0}", previous_share * 100.0)
        )
        .into_owned(),
        fields_json: json!({
            "fields": [
                { "label": rust_i18n::t!("alert_msg.f_unallocated"), "value": fmt_amount(current_share * total, &currency) },
                { "label": rust_i18n::t!("alert_msg.f_share_this"), "value": format!("{:.0}%", current_share * 100.0) },
                { "label": rust_i18n::t!("alert_msg.f_share_last"), "value": format!("{:.0}%", previous_share * 100.0) },
                { "label": rust_i18n::t!("alert_msg.f_tag"), "value": BUSINESS_LINE_TAG },
            ],
            "context": {
                "threshold": threshold,
            },
        })
        .to_string(),
        stat_json: None,
        created_at: now,
        status: AlertStatus::Open,
        snoozed_until: None,
        dedupe_key,
        resolved_at: None,
    };

    fire(app, rule, event, now)?;
    Ok(1)
}

// ==================== Budget ====================

/// Which number a budget rule compares against its threshold: the month's
/// actual cost so far, or the run-rate projection of where the month ends.
/// OptScale's pool alerts offer the same two.
pub(super) const BASED_COST: &str = "cost";
pub(super) const BASED_FORECAST: &str = "forecast";

/// What a budget rule's evaluation found, when the based value crossed the
/// threshold.
#[derive(Debug)]
pub(super) struct BudgetBreach {
    account_name: String,
    budget: BudgetInfo,
    based: String,
    /// Month-to-date actual when `based` is cost, month-end projection when
    /// it is forecast.
    value: f64,
    /// The threshold `value` crossed, in the budget's currency.
    threshold: f64,
    mtd: f64,
    forecast: f64,
}

/// The run-rate forecast of one account for a billing period, read through
/// the evaluation's own ledger connection and clock.
pub(super) fn account_forecast_of(
    conn: &Connection,
    provider: &str,
    account_id: &str,
    period: crate::cloud::BillingPeriod,
    now: DateTime<Utc>,
) -> Result<query::PeriodForecast> {
    query::account_forecast_of(conn, provider, account_id, period, now)
}

/// The threshold a budget rule fires at, in the budget's currency: the
/// config's absolute amount, or a percent of the monthly budget — the
/// config's percent, else the budget's own `alert_threshold`.
pub(super) fn budget_threshold(rule: &AlertRule, budget: &BudgetInfo) -> f64 {
    let threshold = config_f64(rule, "threshold", f64::NAN);
    match config_str(rule, "threshold_type").as_deref() {
        Some("absolute") => threshold,
        _ => {
            let percent = if threshold.is_nan() {
                budget.alert_threshold
            } else {
                threshold
            };
            budget.monthly_budget * percent / 100.0
        }
    }
}

/// Evaluate one budget rule: whether its account's month-to-date cost (or
/// run-rate month-end projection) has crossed the rule's threshold. Shared
/// by firing and by staleness resolution, so the two can never disagree.
pub(super) fn budget_breach_of(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    now: DateTime<Utc>,
) -> Result<Option<BudgetBreach>> {
    let accounts = db::get_all_accounts_of(app)?;
    budget_breach_with(app, ledger, rule, &accounts, now)
}

/// [`budget_breach_of`] against an accounts list the caller already read —
/// staleness resolution checks every open budget event in one pass and must
/// not re-read the accounts table for each.
pub(super) fn budget_breach_with(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    accounts: &[crate::cloud::CloudAccount],
    now: DateTime<Utc>,
) -> Result<Option<BudgetBreach>> {
    let Some(account_id) = config_str(rule, "account_id") else {
        return Ok(None);
    };
    // No budget recorded for the account: nothing to measure against.
    let Some(budget) = db::get_budget_of(app, &account_id)? else {
        return Ok(None);
    };
    let Some(account) = accounts.iter().find(|account| account.id == account_id) else {
        return Ok(None);
    };

    let period = crate::cloud::BillingPeriod::containing(now);
    let progress =
        account_forecast_of(ledger, account.source_id.as_str(), &account.id, period, now)?;

    let based = config_str(rule, "based").unwrap_or_else(|| BASED_COST.to_string());
    let value = if based == BASED_FORECAST {
        progress.forecast
    } else {
        progress.month_to_date
    };
    let threshold = budget_threshold(rule, &budget);

    Ok((value >= threshold).then_some(BudgetBreach {
        account_name: account.name.clone(),
        budget,
        based,
        value,
        threshold,
        mtd: progress.month_to_date,
        forecast: progress.forecast,
    }))
}

pub(super) fn evaluate_budgets(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    now: DateTime<Utc>,
) -> Result<usize> {
    let Some(breach) = budget_breach_of(app, ledger, rule, now)? else {
        return Ok(0);
    };

    // A day-grained dedupe key, like the other rules: a condition that holds
    // fires once a day, not once per refresh. (OptScale's pool alerts
    // re-check every 12h; everything here works to a day.)
    let account_id = config_str(rule, "account_id").unwrap_or_default();
    let dedupe_key = format!(
        "budget|{}|{}|{}",
        account_id,
        breach.based,
        now.date_naive()
    );
    if !should_fire(app, &dedupe_key, now)? {
        return Ok(0);
    }

    // Ledger amounts are in the reporting currency; budgets are recorded in
    // it too (the Rules page writes them so), one currency throughout.
    let currency = breach.budget.currency.clone();
    let budget = breach.budget.monthly_budget;
    let percent_of_budget = if budget > 0.0 {
        breach.value / budget * 100.0
    } else {
        0.0
    };
    let based_label = if breach.based == BASED_FORECAST {
        rust_i18n::t!("alert_msg.based_forecast")
    } else {
        rust_i18n::t!("alert_msg.based_cost")
    };
    let severity = if breach.value >= budget {
        Severity::Critical
    } else {
        Severity::Warning
    };

    let event = AlertEvent {
        id: uuid::Uuid::new_v4().to_string(),
        rule_id: rule.id.clone(),
        severity,
        title: rust_i18n::t!("alert_msg.budget_title", account = breach.account_name)
            .into_owned(),
        body: rust_i18n::t!(
            "alert_msg.budget_body",
            based = based_label,
            value = fmt_amount(breach.value, &currency),
            threshold = fmt_amount(breach.threshold, &currency),
            pct = format!("{percent_of_budget:.0}"),
            budget = fmt_amount(budget, &currency),
            mtd = fmt_amount(breach.mtd, &currency),
            forecast = fmt_amount(breach.forecast, &currency)
        )
        .into_owned(),
        fields_json: json!({
            "fields": [
                { "label": rust_i18n::t!("alert_msg.f_account"), "value": breach.account_name.clone() },
                { "label": rust_i18n::t!("alert_msg.f_mtd"), "value": fmt_amount(breach.mtd, &currency) },
                { "label": rust_i18n::t!("alert_msg.f_forecast"), "value": fmt_amount(breach.forecast, &currency) },
                { "label": rust_i18n::t!("alert_msg.f_budget"), "value": fmt_amount(budget, &currency) },
                { "label": rust_i18n::t!("alert_msg.f_threshold"), "value": fmt_amount(breach.threshold, &currency) },
            ],
            "context": {
                "account_id": account_id,
                "based": breach.based.clone(),
                "threshold": breach.threshold,
            },
        })
        .to_string(),
        stat_json: Some(
            json!({
                "label": rust_i18n::t!("alert_msg.of_budget"),
                "value": format!("{:.0}%", percent_of_budget),
            })
            .to_string(),
        ),
        created_at: now,
        status: AlertStatus::Open,
        snoozed_until: None,
        dedupe_key,
        resolved_at: None,
    };

    fire(app, rule, event, now)?;
    Ok(1)
}
