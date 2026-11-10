use super::*;

// ==================== Read API ====================

/// Open events, newest first, shaped for the Alerts page.
pub fn open_alerts() -> Result<Vec<AlertView>> {
    alerts_by_status(&[AlertStatus::Open])
}

/// Events in the given states, newest first, shaped for the Alerts page.
pub fn alerts_by_status(statuses: &[AlertStatus]) -> Result<Vec<AlertView>> {
    seed_default_rules()?;
    let rules = db::get_alert_rules()?;
    Ok(db::get_alert_events(statuses)?
        .into_iter()
        .map(|event| view_of(&event, &rules))
        .collect())
}

/// Every rule, shaped for the Rules page.
pub fn rules() -> Result<Vec<RuleView>> {
    seed_default_rules()?;
    Ok(db::get_alert_rules()?.iter().map(rule_view_of).collect())
}

/// Enable or disable a rule.
pub fn set_rule_enabled(id: &str, enabled: bool) -> Result<()> {
    db::set_alert_rule_enabled(id, enabled)
}

/// The prefix of a user-created rule's id. Only these may be deleted; the
/// seeded rules are the app's safety net and keep their disable switch
/// instead.
pub const CUSTOM_RULE_PREFIX: &str = "custom-";

/// Create a rule of one of the three kinds, enabled, and return it shaped
/// for the Rules page.
pub fn create_rule(kind: &str, name: &str, config: serde_json::Value) -> Result<RuleView> {
    db::with_connection(|conn| create_rule_on(conn, kind, name, config))
}

pub(crate) fn create_rule_on(
    conn: &Connection,
    kind: &str,
    name: &str,
    config: serde_json::Value,
) -> Result<RuleView> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "The rule needs a name");
    validate_config(kind, &config)?;

    let scope = if kind == RULE_BUDGET {
        // A budget rule's scope chip names the account it watches — which
        // must exist, or the rule could never fire.
        let account_id = config
            .get("account_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        db::get_all_accounts_of(conn)?
            .into_iter()
            .find(|account| account.id == account_id)
            .ok_or_else(|| anyhow::anyhow!("No account with id {account_id:?}"))?
            .name
    } else {
        default_scope(kind).to_string()
    };

    let rule = AlertRule {
        id: format!("{}{}", CUSTOM_RULE_PREFIX, uuid::Uuid::new_v4()),
        kind: kind.to_string(),
        name: name.to_string(),
        scope,
        enabled: true,
        config,
        last_fired_at: None,
    };
    db::save_alert_rule_to(conn, &rule)?;

    Ok(rule_view_of(&rule))
}

/// Delete a user-created rule.
pub fn delete_rule(id: &str) -> Result<()> {
    ensure_custom_rule(id)?;
    db::delete_alert_rule(id)
}

// Exercised by the tests; the public path above goes through the real db.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn delete_rule_on(conn: &Connection, id: &str) -> Result<()> {
    ensure_custom_rule(id)?;
    db::delete_alert_rule_to(conn, id)
}

/// The seeded rules are the app's safety net and stay; their disable
/// switch is the off switch.
pub(super) fn ensure_custom_rule(id: &str) -> Result<()> {
    anyhow::ensure!(
        id.starts_with(CUSTOM_RULE_PREFIX),
        "Only custom rules can be deleted; a built-in rule is switched off instead"
    );
    Ok(())
}

/// The scope chip a rule of this kind shows, matching the seeded rule.
pub(super) fn default_scope(kind: &str) -> &'static str {
    match kind {
        RULE_BALANCE_FLOOR => "Prepaid accounts",
        _ => "All sources",
    }
}

/// The config constraints of each kind, checked before a custom rule is
/// stored — the evaluators fall back to defaults for missing keys, so what
/// is written down must be both present and sane.
pub(super) fn validate_config(kind: &str, config: &serde_json::Value) -> Result<()> {
    match kind {
        RULE_COST_ANOMALY => {
            let multiplier = config
                .get("multiplier")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| anyhow::anyhow!("The multiplier must be a number, e.g. 2.5"))?;
            anyhow::ensure!(
                multiplier > 1.0,
                "The multiplier must be above 1.0 — 1.0 would fire on any day over baseline"
            );
            let consecutive = config
                .get("consecutive_days")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow::anyhow!("Consecutive days must be a whole number, e.g. 2")
                })?;
            anyhow::ensure!(consecutive >= 1, "Consecutive days must be at least 1");
            if let Some(account_id) = config.get("account_id").and_then(serde_json::Value::as_str) {
                anyhow::ensure!(
                    !account_id.trim().is_empty(),
                    "The account id must not be empty"
                );
            }
        }
        RULE_BALANCE_FLOOR => {
            let floor = config
                .get("floor")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| anyhow::anyhow!("The floor must be a number, e.g. 200"))?;
            anyhow::ensure!(floor > 0.0, "The floor must be above zero");
        }
        RULE_UNTAGGED_RATIO => {
            let threshold = config
                .get("threshold")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| anyhow::anyhow!("The threshold must be a number, e.g. 0.15"))?;
            anyhow::ensure!(
                (0.0..1.0).contains(&threshold),
                "The threshold must be between 0 and 1 (0.15 = 15%)"
            );
        }
        RULE_BUDGET => {
            let account_id = config
                .get("account_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            anyhow::ensure!(
                !account_id.trim().is_empty(),
                "A budget rule needs the account it watches"
            );
            if let Some(based) = config.get("based").and_then(serde_json::Value::as_str) {
                anyhow::ensure!(
                    based == BASED_COST || based == BASED_FORECAST,
                    "Based must be \"cost\" or \"forecast\""
                );
            }
            let threshold_type = config
                .get("threshold_type")
                .and_then(serde_json::Value::as_str);
            if let Some(threshold_type) = threshold_type {
                anyhow::ensure!(
                    threshold_type == "absolute" || threshold_type == "percent",
                    "The threshold type must be \"absolute\" or \"percent\""
                );
            }
            match config.get("threshold").and_then(serde_json::Value::as_f64) {
                Some(threshold) => {
                    anyhow::ensure!(threshold > 0.0, "The threshold must be above zero")
                }
                // A percent rule without a number falls back to the budget's
                // own alert threshold; an absolute one needs the amount.
                None => anyhow::ensure!(
                    threshold_type != Some("absolute"),
                    "An absolute threshold needs an amount, e.g. 500"
                ),
            }
        }
        other => anyhow::bail!("Unknown rule kind {other:?}"),
    }

    Ok(())
}

/// Hide an event for `hours`; the condition may fire again once the snooze
/// runs out.
pub fn snooze_alert(id: &str, hours: i64) -> Result<()> {
    db::set_alert_event_status(
        id,
        AlertStatus::Snoozed,
        Some(Utc::now() + Duration::hours(hours)),
    )
}

/// Dismiss an event: it stops showing and stops blocking nothing — the
/// same condition fires again under the next day's dedupe key.
pub fn dismiss_alert(id: &str) -> Result<()> {
    db::set_alert_event_status(id, AlertStatus::Dismissed, None)
}
