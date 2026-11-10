use super::*;

impl AlertEvent {
    /// The machine-readable half of `fields_json`.
    pub(super) fn context(&self) -> Option<serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(&self.fields_json)
            .ok()
            .and_then(|value| value.get("context").cloned())
    }
}

/// An event as the Alerts page renders it.
pub(super) fn view_of(event: &AlertEvent, rules: &[AlertRule]) -> AlertView {
    let kind = rules
        .iter()
        .find(|rule| rule.id == event.rule_id)
        .map(|rule| AlertKind::of_rule(&rule.kind))
        .unwrap_or_else(|| AlertKind::of_rule(&event.rule_id));

    let fields = serde_json::from_str::<serde_json::Value>(&event.fields_json)
        .ok()
        .and_then(|value| value.get("fields").cloned())
        .and_then(|value| serde_json::from_value::<Vec<AlertFieldSerde>>(value).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|field| AlertField {
            label: field.label,
            value: field.value,
        })
        .collect();

    let stat = event
        .stat_json
        .as_deref()
        .and_then(|stat| serde_json::from_str::<AlertStat>(stat).ok());

    let actions = match kind {
        AlertKind::CostAnomaly => vec!["Trace in attribution", "Snooze 24h", "Dismiss"],
        AlertKind::Balance => vec!["Snooze 24h", "Dismiss"],
        AlertKind::UntaggedRatio => vec!["Write an allocation rule", "Snooze 24h", "Dismiss"],
        AlertKind::Budget => vec!["Snooze 24h", "Dismiss"],
    }
    .into_iter()
    .map(str::to_string)
    .collect();

    AlertView {
        id: event.id.clone(),
        kind,
        severity: event.severity,
        title: event.title.clone(),
        body: event.body.clone(),
        fields,
        stat,
        actions,
        created_at: event.created_at,
        resolved_at: event.resolved_at,
        context: event.context().unwrap_or_else(|| json!({})),
    }
}

/// The JSON shape `fields` is stored in.
#[derive(serde::Deserialize)]
pub(super) struct AlertFieldSerde {
    label: String,
    value: String,
}

/// A rule as the Rules page renders it: the description and condition
/// chips follow from the kind and its config.
pub(super) fn rule_view_of(rule: &AlertRule) -> RuleView {
    let (description, condition_chips, delivery_chips) = match rule.kind.as_str() {
        RULE_COST_ANOMALY => (
            rust_i18n::t!("rule_view.anomaly_desc").into_owned(),
            vec![
                format!(
                    "daily_cost > baseline_7d × {}",
                    trim_float(config_f64(rule, "multiplier", 2.5))
                ),
                rust_i18n::t!(
                    "rule_view.consecutive_days",
                    n = config_u64(rule, "consecutive_days", 2)
                )
                .into_owned(),
            ],
            vec![rust_i18n::t!("rule_view.desktop_and_centre").into_owned()],
        ),
        RULE_BALANCE_FLOOR => (
            rust_i18n::t!("rule_view.balance_desc").into_owned(),
            vec![
                format!(
                    "balance < floor ({})",
                    trim_float(config_f64(rule, "floor", 200.0))
                ),
                rust_i18n::t!("rule_view.every_refresh").into_owned(),
            ],
            vec![rust_i18n::t!("rule_view.desktop").into_owned()],
        ),
        RULE_BUDGET => {
            let based = config_str(rule, "based").unwrap_or_else(|| BASED_COST.to_string());
            let based_label = if based == BASED_FORECAST {
                "month_end_forecast"
            } else {
                "month_to_date_cost"
            };
            let threshold_chip = match config_str(rule, "threshold_type").as_deref() {
                Some("absolute") => format!(
                    "{based_label} ≥ {}",
                    fmt_amount(config_f64(rule, "threshold", 0.0), &reporting_currency())
                ),
                _ => rust_i18n::t!(
                    "rule_view.pct_of_budget",
                    based = based_label,
                    pct = trim_float(config_f64(rule, "threshold", 80.0))
                )
                .into_owned(),
            };
            (
                rust_i18n::t!("rule_view.budget_desc").into_owned(),
                vec![
                    threshold_chip,
                    rust_i18n::t!("rule_view.daily").into_owned(),
                ],
                vec![rust_i18n::t!("rule_view.centre_only").into_owned()],
            )
        }
        _ => (
            rust_i18n::t!("rule_view.untagged_desc").into_owned(),
            vec![
                format!(
                    "unallocated_share > {:.0}%",
                    config_f64(rule, "threshold", DEFAULT_UNTAGGED_THRESHOLD) * 100.0
                ),
                rust_i18n::t!("rule_view.daily").into_owned(),
            ],
            vec![rust_i18n::t!("rule_view.centre_only").into_owned()],
        ),
    };

    RuleView {
        id: rule.id.clone(),
        name: shown_name(rule),
        scope: shown_scope(&rule.scope),
        description,
        condition_chips,
        delivery_chips,
        enabled: rule.enabled,
        last_fired_at: rule.last_fired_at,
    }
}

/// A rule's name as shown. The seeded rules are stored under their English
/// names; while a seeded rule still carries it, the name is translated.
/// A name the user typed is theirs and shown as typed.
pub(super) fn shown_name(rule: &AlertRule) -> String {
    let key = match (rule.id.as_str(), rule.name.as_str()) {
        (RULE_COST_ANOMALY, "Model cost growth anomaly") => "rules.kind_cost_anomaly",
        (RULE_BALANCE_FLOOR, "Balance floor") => "rules.kind_balance_floor",
        (RULE_UNTAGGED_RATIO, "Untagged spend ratio") => "rules.kind_untagged_ratio",
        _ => return rule.name.clone(),
    };
    rust_i18n::t!(key).into_owned()
}

/// A stored scope chip as shown; the scopes are the two fixed phrases of
/// [`default_scope`], kept English in the database.
pub(super) fn shown_scope(scope: &str) -> String {
    match scope {
        "All sources" => rust_i18n::t!("rule_view.all_sources").into_owned(),
        "Prepaid accounts" => rust_i18n::t!("rule_view.prepaid").into_owned(),
        other => other.to_string(),
    }
}

// ==================== Formatting ====================

/// The currency amounts are read in, for the messages that quote one.
/// Canonical home is the data layer; kept here as a thin delegate so the
/// engine and the pages cannot drift apart.
pub(super) fn reporting_currency() -> String {
    crate::ui::data::reporting_currency()
}

/// `1,234.56` with the currency's usual symbol when it has one.
pub(super) fn fmt_amount(amount: f64, currency: &str) -> String {
    let formatted = format!("{:.2}", amount);
    match currency {
        "USD" => format!("${formatted}"),
        "CNY" => format!("¥{formatted}"),
        _ => format!("{formatted} {currency}"),
    }
}

/// "2 days" / "1 day" / "less than a day".
pub(super) fn fmt_days(days: f64) -> String {
    if days < 1.0 {
        rust_i18n::t!("alert_msg.less_than_a_day").into_owned()
    } else if days < 1.5 {
        rust_i18n::t!("alert_msg.one_day").into_owned()
    } else {
        rust_i18n::t!("alert_msg.n_days", n = days.floor() as i64).into_owned()
    }
}

/// A float without a trailing ".0", for condition chips.
pub(super) fn trim_float(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    }
}

/// Days in the month `date` falls in. The month arithmetic lives on
/// [`crate::cloud::BillingPeriod`]; this is the date-keyed convenience.
pub(super) fn days_in_month(date: NaiveDate) -> u32 {
    let period = crate::cloud::BillingPeriod::new(date.year(), date.month());
    (period.end_exclusive() - period.start()).num_days() as u32
}
