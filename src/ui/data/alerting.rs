use super::*;

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
