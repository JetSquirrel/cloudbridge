//! Alerting: rules, and the events they fire.
//!
//! Three rules ship with the app and are seeded into the app-state
//! database on first run (see [`seed_default_rules`]); from then on they
//! are the user's to disable. Storage lives in [`crate::db`]; this module
//! is the evaluation logic and the read API the Alerts and Rules pages
//! consume.
//!
//! Evaluation is deliberately re-derivable: every run re-reads the ledger,
//! decides whether each condition holds right now, and fires only when no
//! open or still-snoozed event carries the same `dedupe_key`. A condition
//! that holds for a week produces one event a day, not one per refresh. A low
//! balance is the exception: it stays one event, kept up to date, until it
//! recovers or is dismissed.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

use crate::analytics;
use crate::cloud::BudgetInfo;
use crate::db;
use crate::ledger::query;
use crate::store::Connection;
use crate::ui::data::BUSINESS_LINE_TAG;

mod detect;
mod manage;
mod resolve;
#[cfg(test)]
mod tests;
mod view;

pub use detect::*;
pub use manage::*;
pub use resolve::*;
use view::*;

/// Rule id of the cost-growth anomaly rule.
pub const RULE_COST_ANOMALY: &str = "cost-growth-anomaly";
/// Rule id of the balance-floor rule.
pub const RULE_BALANCE_FLOOR: &str = "balance-floor";
/// Rule id of the untagged-ratio rule.
pub const RULE_UNTAGGED_RATIO: &str = "untagged-ratio";
/// Rule kind of the per-account budget rules. There is no seeded budget
/// rule: each one watches one account's monthly budget, so every budget
/// rule is a custom one.
pub const RULE_BUDGET: &str = "budget";

/// The untagged share the seeded `untagged-ratio` rule fires over, as a
/// fraction (0.15 = 15%). The account-row "Untagged spend" badge reads the
/// same line, from here — the two must not drift apart.
pub const DEFAULT_UNTAGGED_THRESHOLD: f64 = 0.15;

/// How far back the anomaly baseline reads. A 7-day trailing mean for each
/// of the last two checked days needs the seven days before them, and a
/// little slack for a late-arriving day.
const ANOMALY_WINDOW_DAYS: i64 = 14;

/// How many days of balance history the burn projection reads.
const BURN_WINDOW_DAYS: i64 = 7;

/// How serious an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
        }
    }

    /// Anything unrecognized reads as a warning rather than failing the row.
    pub fn from_stored(value: &str) -> Self {
        match value {
            "critical" => Self::Critical,
            _ => Self::Warning,
        }
    }
}

/// Lifecycle of an alert event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertStatus {
    Open,
    Snoozed,
    Resolved,
    Dismissed,
}

impl AlertStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Snoozed => "snoozed",
            Self::Resolved => "resolved",
            Self::Dismissed => "dismissed",
        }
    }

    /// Anything unrecognized reads as open: hiding it would lose an alert.
    pub fn from_stored(value: &str) -> Self {
        match value {
            "snoozed" => Self::Snoozed,
            "resolved" => Self::Resolved,
            "dismissed" => Self::Dismissed,
            _ => Self::Open,
        }
    }
}

/// An alerting rule, as stored in `alert_rule`. `config` is a JSON object
/// whose keys depend on `kind`; see [`default_rules`].
#[derive(Debug, Clone)]
pub struct AlertRule {
    pub id: String,
    /// Which evaluator runs it: `cost-growth-anomaly`, `balance-floor` or
    /// `untagged-ratio`.
    pub kind: String,
    pub name: String,
    /// Scope chip the Rules page shows, e.g. "Prepaid accounts".
    pub scope: String,
    pub enabled: bool,
    pub config: serde_json::Value,
    pub last_fired_at: Option<DateTime<Utc>>,
}

/// One firing of a rule, as stored in `alert_event`.
///
/// `fields_json` is `{"fields": [{"label", "value"}], "context": {...}}`:
/// the fields are what the alert card renders, the context is what
/// [`resolve_stale_alerts`] needs to re-check the condition (provider,
/// account id, floor, ...). `stat_json` is `{"label", "value"}` for the
/// right-side highlight stat, when the event has one.
#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub id: String,
    pub rule_id: String,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub fields_json: String,
    pub stat_json: Option<String>,
    pub created_at: DateTime<Utc>,
    pub status: AlertStatus,
    pub snoozed_until: Option<DateTime<Utc>>,
    pub dedupe_key: String,
    /// When the event reached a final state (resolved or dismissed), if it
    /// has. "Resolved this month" keys on this, not on `created_at`.
    pub resolved_at: Option<DateTime<Utc>>,
}

/// The filter chips the Alerts page groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    CostAnomaly,
    Balance,
    UntaggedRatio,
    Budget,
}

impl AlertKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::CostAnomaly => "Cost anomaly",
            Self::Balance => "Balance",
            Self::UntaggedRatio => "Untagged spend",
            Self::Budget => "Budget",
        }
    }

    /// The kind an event belongs to, from the rule that fired it.
    fn of_rule(rule_kind: &str) -> Self {
        match rule_kind {
            RULE_COST_ANOMALY => Self::CostAnomaly,
            RULE_BALANCE_FLOOR => Self::Balance,
            RULE_BUDGET => Self::Budget,
            _ => Self::UntaggedRatio,
        }
    }
}

/// A label/value row on an alert card.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertField {
    pub label: String,
    pub value: String,
}

/// The right-side highlight stat on an alert card, when there is one.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct AlertStat {
    pub label: String,
    pub value: String,
}

/// An alert event shaped for the Alerts page, owned, plus the
/// machine-readable `context` the event was fired with (provider, account
/// id, floor, ...) so account rows can tell which alerts are about them.
#[derive(Debug, Clone)]
pub struct AlertView {
    pub id: String,
    pub kind: AlertKind,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub fields: Vec<AlertField>,
    pub stat: Option<AlertStat>,
    /// Action buttons, in order.
    pub actions: Vec<String>,
    pub created_at: DateTime<Utc>,
    /// When the event was resolved or dismissed, if it has been.
    pub resolved_at: Option<DateTime<Utc>>,
    /// The context half of `fields_json`; `{}` for an event without one.
    pub context: serde_json::Value,
}

/// A rule shaped for the Rules page, owned.
#[derive(Debug, Clone)]
pub struct RuleView {
    pub id: String,
    pub name: String,
    /// Scope chip, e.g. "Prepaid accounts".
    pub scope: String,
    pub description: String,
    /// Condition chips, e.g. `daily_cost > baseline_7d × 2.5`.
    pub condition_chips: Vec<String>,
    /// Delivery chips.
    pub delivery_chips: Vec<String>,
    pub enabled: bool,
    pub last_fired_at: Option<DateTime<Utc>>,
}

/// The rules every install starts with. Idempotent via
/// [`seed_default_rules`]: an id that already exists is left alone, so a
/// rule the user disabled stays disabled.
fn default_rules() -> Vec<AlertRule> {
    vec![
        AlertRule {
            id: RULE_COST_ANOMALY.to_string(),
            kind: RULE_COST_ANOMALY.to_string(),
            name: "Model cost growth anomaly".to_string(),
            scope: "All sources".to_string(),
            enabled: true,
            config: json!({ "multiplier": 2.5, "consecutive_days": 2 }),
            last_fired_at: None,
        },
        AlertRule {
            id: RULE_BALANCE_FLOOR.to_string(),
            kind: RULE_BALANCE_FLOOR.to_string(),
            name: "Balance floor".to_string(),
            scope: "Prepaid accounts".to_string(),
            enabled: true,
            // The per-account override is the account's budget
            // (BudgetInfo.monthly_budget); this is the floor for an
            // account without one.
            config: json!({ "floor": 200.0 }),
            last_fired_at: None,
        },
        AlertRule {
            id: RULE_UNTAGGED_RATIO.to_string(),
            kind: RULE_UNTAGGED_RATIO.to_string(),
            name: "Untagged spend ratio".to_string(),
            scope: "All sources".to_string(),
            enabled: true,
            config: json!({ "threshold": DEFAULT_UNTAGGED_THRESHOLD }),
            last_fired_at: None,
        },
    ]
}

/// Insert the default rules on first run. Safe to call on every start.
pub fn seed_default_rules() -> Result<()> {
    db::with_connection(seed_default_rules_on)
}

pub(crate) fn seed_default_rules_on(conn: &Connection) -> Result<()> {
    for rule in default_rules() {
        if !db::alert_rule_exists(conn, &rule.id)? {
            db::save_alert_rule_to(conn, &rule)?;
        }
    }

    Ok(())
}

/// Run every enabled rule against the ledger, firing what newly holds.
///
/// Returns the number of new open events. Blocking and cx-free: call it
/// off the UI thread (after a successful ingest, and once at startup).
pub fn evaluate() -> Result<usize> {
    seed_default_rules()?;
    crate::ledger::with_connection_ref(|ledger| {
        db::with_connection(|app| evaluate_with(app, ledger, Utc::now()))
    })
}

pub(crate) fn evaluate_with(
    app: &Connection,
    ledger: &Connection,
    now: DateTime<Utc>,
) -> Result<usize> {
    let rules = db::get_alert_rules_of(app)?;
    let mut created = 0;

    for rule in rules.iter().filter(|rule| rule.enabled) {
        created += match rule.kind.as_str() {
            RULE_COST_ANOMALY => evaluate_cost_anomalies(app, ledger, rule, now)?,
            RULE_BALANCE_FLOOR => evaluate_balance_floors(app, ledger, rule, now)?,
            RULE_UNTAGGED_RATIO => evaluate_untagged_ratio(app, ledger, rule, now)?,
            RULE_BUDGET => evaluate_budgets(app, ledger, rule, now)?,
            other => {
                tracing::warn!("No evaluator for alert rule kind {:?}", other);
                0
            }
        };
    }

    Ok(created)
}

/// Whether a condition may fire under its dedupe key right now: no live
/// event under it, or a snooze that has run out.
fn should_fire(app: &Connection, dedupe_key: &str, now: DateTime<Utc>) -> Result<bool> {
    Ok(match db::find_live_alert_event_of(app, dedupe_key)? {
        None => true,
        Some(event) => match event.status {
            AlertStatus::Snoozed => event.snoozed_until.is_some_and(|until| until <= now),
            _ => false,
        },
    })
}

/// Insert the event and stamp the rule's last-fired time.
fn fire(app: &Connection, rule: &AlertRule, event: AlertEvent, now: DateTime<Utc>) -> Result<()> {
    db::insert_alert_event_to(app, &event)?;
    db::mark_rule_fired_to(app, &rule.id, now)?;
    Ok(())
}

fn config_f64(rule: &AlertRule, key: &str, default: f64) -> f64 {
    rule.config
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(default)
}

fn config_u64(rule: &AlertRule, key: &str, default: u64) -> u64 {
    rule.config
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(default)
}

fn config_str(rule: &AlertRule, key: &str) -> Option<String> {
    rule.config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}
