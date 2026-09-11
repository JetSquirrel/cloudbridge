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
//! that holds for a week produces one event a day, not one per refresh.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use duckdb::Connection;
use serde_json::json;
use std::collections::BTreeMap;

use crate::db;
use crate::ledger::query;
use crate::ui::data::BUSINESS_LINE_TAG;

/// Rule id of the cost-growth anomaly rule.
pub const RULE_COST_ANOMALY: &str = "cost-growth-anomaly";
/// Rule id of the balance-floor rule.
pub const RULE_BALANCE_FLOOR: &str = "balance-floor";
/// Rule id of the untagged-ratio rule.
pub const RULE_UNTAGGED_RATIO: &str = "untagged-ratio";

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
}

impl AlertKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::CostAnomaly => "Cost anomaly",
            Self::Balance => "Balance",
            Self::UntaggedRatio => "Untagged spend",
        }
    }

    /// The kind an event belongs to, from the rule that fired it.
    fn of_rule(rule_kind: &str) -> Self {
        match rule_kind {
            RULE_COST_ANOMALY => Self::CostAnomaly,
            RULE_BALANCE_FLOOR => Self::Balance,
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
        let exists: i64 = conn.query_row(
            "SELECT count(*) FROM alert_rule WHERE id = ?",
            duckdb::params![rule.id],
            |row| row.get(0),
        )?;
        if exists == 0 {
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

/// The mean daily cost of the seven calendar days before `day`; a day with
/// no charges counts as zero.
fn trailing_mean(daily: &BTreeMap<NaiveDate, f64>, day: NaiveDate) -> f64 {
    let sum: f64 = (1..=7)
        .filter_map(|back| day.checked_sub_days(chrono::Days::new(back)))
        .map(|before| daily.get(&before).copied().unwrap_or(0.0))
        .sum();
    sum / 7.0
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
    let baseline = trailing_mean(daily, last);
    if baseline <= 0.0 || daily[&last] <= baseline * multiplier {
        return None;
    }

    // Walk the streak back from the latest day while each earlier day
    // breached its own baseline.
    let mut first = last;
    while let Some(previous) = first.pred_opt() {
        let mean = trailing_mean(daily, previous);
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

fn evaluate_cost_anomalies(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    now: DateTime<Utc>,
) -> Result<usize> {
    let multiplier = config_f64(rule, "multiplier", 2.5);
    let consecutive = config_u64(rule, "consecutive_days", 2);
    let currency = reporting_currency();

    let rows =
        query::daily_totals_by_service_of(ledger, now - Duration::days(ANOMALY_WINDOW_DAYS))?;

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
        let Some(breach) = detect_breach(&daily, multiplier, consecutive) else {
            continue;
        };

        let dedupe_key = format!("cost|{}|{}|{}", provider, service, breach.last_day);
        if !should_fire(app, &dedupe_key, now)? {
            continue;
        }

        let month_end = breach.last_amount * f64::from(days_in_month(breach.last_day));
        let ratio = breach.last_amount / breach.baseline;
        let event = AlertEvent {
            id: uuid::Uuid::new_v4().to_string(),
            rule_id: rule.id.clone(),
            severity: Severity::Critical,
            title: format!("Cost growing far above baseline: {provider} {service}"),
            body: format!(
                "{provider} {service} ran {} on {} against a 7-day baseline of {} ({:.1}×). \
                 The streak has held for {} consecutive days, first breach {}. \
                 At this rate the month ends near {}.",
                fmt_amount(breach.last_amount, &currency),
                breach.last_day,
                fmt_amount(breach.baseline, &currency),
                ratio,
                breach.streak_days(),
                breach.first_day,
                fmt_amount(month_end, &currency),
            ),
            fields_json: json!({
                "fields": [
                    { "label": "Provider", "value": provider.clone() },
                    { "label": "Service", "value": service.clone() },
                    { "label": "24h cost", "value": fmt_amount(breach.last_amount, &currency) },
                    { "label": "7-day baseline", "value": fmt_amount(breach.baseline, &currency) },
                    { "label": "First breach", "value": breach.first_day.to_string() },
                    { "label": "Month-end if held", "value": fmt_amount(month_end, &currency) },
                ],
                "context": {
                    "provider": provider.clone(),
                    "service": service.clone(),
                    "multiplier": multiplier,
                    "consecutive_days": consecutive,
                },
            })
            .to_string(),
            stat_json: Some(
                json!({
                    "label": format!("Billed on {}", breach.last_day),
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

fn evaluate_balance_floors(
    app: &Connection,
    ledger: &Connection,
    rule: &AlertRule,
    now: DateTime<Utc>,
) -> Result<usize> {
    let default_floor = config_f64(rule, "floor", 200.0);
    let today = now.date_naive();

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

        let dedupe_key = format!("balance|{}|{}|{}", provider, account.id, today);
        if !should_fire(app, &dedupe_key, now)? {
            continue;
        }

        let burn = query::balance_burn_of(ledger, provider, &account.id, BURN_WINDOW_DAYS)?;
        let days_left = burn
            .filter(|burn| *burn > 0.0)
            .map(|burn| balance.balance / burn);
        let projection = match (burn, days_left) {
            (Some(burn), Some(days_left)) if burn > 0.0 => format!(
                " At the last {} days of burn ({}/day) the account runs dry in roughly {}.",
                BURN_WINDOW_DAYS,
                fmt_amount(burn, &balance.currency),
                fmt_days(days_left),
            ),
            _ => String::new(),
        };

        let event = AlertEvent {
            id: uuid::Uuid::new_v4().to_string(),
            rule_id: rule.id.clone(),
            severity: Severity::Warning,
            title: format!("{} balance below floor", descriptor.display_name),
            body: format!(
                "Balance is {} against a {} floor.{}",
                fmt_amount(balance.balance, &balance.currency),
                fmt_amount(floor, &balance.currency),
                projection,
            ),
            fields_json: json!({
                "fields": [
                    { "label": "Account", "value": account.name },
                    { "label": "Balance", "value": fmt_amount(balance.balance, &balance.currency) },
                    { "label": "Floor", "value": fmt_amount(floor, &balance.currency) },
                ],
                "context": {
                    "provider": provider,
                    "account_id": account.id,
                    "floor": floor,
                },
            })
            .to_string(),
            stat_json: days_left
                .map(|days| json!({ "label": "Runs dry in", "value": fmt_days(days) }).to_string()),
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

fn evaluate_untagged_ratio(
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
        title: "Untagged spend share is growing".to_string(),
        body: format!(
            "{:.0}% of {} spend reaches no business line ({}) — up from {:.0}% last month. \
             New workloads are landing outside every allocation rule.",
            current_share * 100.0,
            current.label(),
            fmt_amount(current_share * total, &currency),
            previous_share * 100.0,
        ),
        fields_json: json!({
            "fields": [
                { "label": "Unallocated", "value": fmt_amount(current_share * total, &currency) },
                { "label": "Share this month", "value": format!("{:.0}%", current_share * 100.0) },
                { "label": "Share last month", "value": format!("{:.0}%", previous_share * 100.0) },
                { "label": "Tag", "value": BUSINESS_LINE_TAG },
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

// ==================== Resolution ====================

/// Auto-resolve open events whose condition no longer holds — the
/// "Resolved this month" list. Returns how many were resolved.
pub fn resolve_stale_alerts() -> Result<usize> {
    crate::ledger::with_connection_ref(|ledger| {
        db::with_connection(|app| resolve_stale_with(app, ledger, Utc::now()))
    })
}

pub(crate) fn resolve_stale_with(
    app: &Connection,
    ledger: &Connection,
    now: DateTime<Utc>,
) -> Result<usize> {
    let rules = db::get_alert_rules_of(app)?;
    let open = db::get_alert_events_of(app, &[AlertStatus::Open])?;

    let mut resolved = 0;
    for event in open {
        let Some(rule) = rules.iter().find(|rule| rule.id == event.rule_id) else {
            continue;
        };
        let context = event.context().unwrap_or_else(|| json!({}));

        let stale = match rule.kind.as_str() {
            RULE_BALANCE_FLOOR => {
                let provider = context.get("provider").and_then(|v| v.as_str());
                let account_id = context.get("account_id").and_then(|v| v.as_str());
                let floor = context.get("floor").and_then(|v| v.as_f64());
                match (provider, account_id, floor) {
                    (Some(provider), Some(account_id), Some(floor)) => {
                        query::latest_balance_of(ledger, provider, account_id)?
                            .is_some_and(|balance| balance.balance >= floor)
                    }
                    _ => false,
                }
            }
            RULE_UNTAGGED_RATIO => {
                let threshold = config_f64(rule, "threshold", DEFAULT_UNTAGGED_THRESHOLD);
                let current = crate::cloud::BillingPeriod::containing(now);
                let share = untagged_share(&query::tag_breakdown_of(
                    ledger,
                    &current.label(),
                    BUSINESS_LINE_TAG,
                    None,
                )?);
                share <= threshold
            }
            RULE_COST_ANOMALY => {
                let provider = context.get("provider").and_then(|v| v.as_str());
                let service = context.get("service").and_then(|v| v.as_str());
                match (provider, service) {
                    (Some(provider), Some(service)) => {
                        let multiplier = config_f64(rule, "multiplier", 2.5);
                        let consecutive = config_u64(rule, "consecutive_days", 2);
                        let rows = query::daily_totals_by_service_of(
                            ledger,
                            now - Duration::days(ANOMALY_WINDOW_DAYS),
                        )?;
                        let daily: BTreeMap<NaiveDate, f64> = rows
                            .into_iter()
                            .filter(|row| row.provider == provider && row.service == service)
                            .filter_map(|row| {
                                NaiveDate::parse_from_str(&row.day, "%Y-%m-%d")
                                    .ok()
                                    .map(|day| (day, row.amount))
                            })
                            .collect();
                        detect_breach(&daily, multiplier, consecutive).is_none()
                    }
                    _ => false,
                }
            }
            _ => false,
        };

        if stale {
            db::set_alert_event_status_to(app, &event.id, AlertStatus::Resolved, None)?;
            resolved += 1;
        }
    }

    Ok(resolved)
}

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

    let rule = AlertRule {
        id: format!("{}{}", CUSTOM_RULE_PREFIX, uuid::Uuid::new_v4()),
        kind: kind.to_string(),
        name: name.to_string(),
        scope: default_scope(kind).to_string(),
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
fn ensure_custom_rule(id: &str) -> Result<()> {
    anyhow::ensure!(
        id.starts_with(CUSTOM_RULE_PREFIX),
        "Only custom rules can be deleted; a built-in rule is switched off instead"
    );
    Ok(())
}

/// The scope chip a rule of this kind shows, matching the seeded rule.
fn default_scope(kind: &str) -> &'static str {
    match kind {
        RULE_BALANCE_FLOOR => "Prepaid accounts",
        _ => "All sources",
    }
}

/// The config constraints of each kind, checked before a custom rule is
/// stored — the evaluators fall back to defaults for missing keys, so what
/// is written down must be both present and sane.
fn validate_config(kind: &str, config: &serde_json::Value) -> Result<()> {
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

impl AlertEvent {
    /// The machine-readable half of `fields_json`.
    fn context(&self) -> Option<serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(&self.fields_json)
            .ok()
            .and_then(|value| value.get("context").cloned())
    }
}

/// An event as the Alerts page renders it.
fn view_of(event: &AlertEvent, rules: &[AlertRule]) -> AlertView {
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
struct AlertFieldSerde {
    label: String,
    value: String,
}

/// A rule as the Rules page renders it: the description and condition
/// chips follow from the kind and its config.
fn rule_view_of(rule: &AlertRule) -> RuleView {
    let (description, condition_chips, delivery_chips) = match rule.kind.as_str() {
        RULE_COST_ANOMALY => (
            "Fires when a service's daily cost exceeds its trailing 7-day baseline by more \
             than the threshold, for consecutive days."
                .to_string(),
            vec![
                format!(
                    "daily_cost > baseline_7d × {}",
                    trim_float(config_f64(rule, "multiplier", 2.5))
                ),
                format!(
                    "{} consecutive days",
                    config_u64(rule, "consecutive_days", 2)
                ),
            ],
            vec!["Desktop + alert centre".to_string()],
        ),
        RULE_BALANCE_FLOOR => (
            "Watches balance-reporting sources against a per-account floor (the account's \
             budget, or the default) and projects days remaining from the last 7 days of burn."
                .to_string(),
            vec![
                format!(
                    "balance < floor ({})",
                    trim_float(config_f64(rule, "floor", 200.0))
                ),
                "Every refresh".to_string(),
            ],
            vec!["Desktop".to_string()],
        ),
        _ => (
            "Raises a warning when the share of spend that reaches no business line grows \
             month over month, so attribution rules keep pace with new workloads."
                .to_string(),
            vec![
                format!(
                    "unallocated_share > {:.0}%",
                    config_f64(rule, "threshold", DEFAULT_UNTAGGED_THRESHOLD) * 100.0
                ),
                "Daily".to_string(),
            ],
            vec!["Alert centre only".to_string()],
        ),
    };

    RuleView {
        id: rule.id.clone(),
        name: rule.name.clone(),
        scope: rule.scope.clone(),
        description,
        condition_chips,
        delivery_chips,
        enabled: rule.enabled,
        last_fired_at: rule.last_fired_at,
    }
}

// ==================== Formatting ====================

/// The currency amounts are read in, for the messages that quote one.
fn reporting_currency() -> String {
    crate::config::load_config()
        .map(|settings| settings.reporting_currency)
        .unwrap_or_else(|_| crate::config::DEFAULT_REPORTING_CURRENCY.to_string())
}

/// `1,234.56` with the currency's usual symbol when it has one.
fn fmt_amount(amount: f64, currency: &str) -> String {
    let formatted = format!("{:.2}", amount);
    match currency {
        "USD" => format!("${formatted}"),
        "CNY" => format!("¥{formatted}"),
        _ => format!("{formatted} {currency}"),
    }
}

/// "2 days" / "1 day" / "less than a day".
fn fmt_days(days: f64) -> String {
    if days < 1.0 {
        "less than a day".to_string()
    } else if days < 1.5 {
        "1 day".to_string()
    } else {
        format!("{} days", days.floor() as i64)
    }
}

/// A float without a trailing ".0", for condition chips.
fn trim_float(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    }
}

fn days_in_month(date: NaiveDate) -> u32 {
    let (year, month) = if date.month() == 12 {
        (date.year() + 1, 1)
    } else {
        (date.year(), date.month() + 1)
    };
    let first_of_next =
        NaiveDate::from_ymd_opt(year, month, 1).expect("the month after a real one");
    (first_of_next - date.with_day(1).expect("day 1 exists")).num_days() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::BillingPeriod;
    use crate::ledger::{self, BalanceSnapshot, Channel, Charge, PeriodKey};
    use chrono::TimeZone;

    /// Both stores, in memory: app state and the ledger.
    fn stores() -> (Connection, Connection) {
        let app = Connection::open_in_memory().expect("in-memory duckdb");
        db::prepare_schema(&app).expect("app schema applies");

        let ledger = Connection::open_in_memory().expect("in-memory duckdb");
        ledger::schema::apply(&ledger).expect("ledger schema applies");
        ledger::schema::apply_reporting_currency(&ledger, "USD").expect("view applies");

        (app, ledger)
    }

    fn day(days_ago: i64) -> DateTime<Utc> {
        (Utc::now() - Duration::days(days_ago))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

    fn usage(service: &str, amount: f64, at: DateTime<Utc>, tags: Option<&str>) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            billed_cost: Some(amount),
            tags: tags.map(str::to_string),
            ..Charge::new(at, at + Duration::hours(1), "USD")
        }
    }

    /// Write a whole period's charges (the ledger replaces periods whole).
    fn write_period(ledger: &mut Connection, period: BillingPeriod, charges: &[Charge]) {
        let key = PeriodKey::new("AWS", "acct-1", period.label());
        ledger::write_period(
            ledger,
            &key,
            &ledger::new_batch_id(),
            charges,
            None,
            Channel::Api,
        )
        .unwrap();
    }

    fn add_deepseek_account(app: &Connection) {
        app.execute(
            "INSERT INTO cloud_accounts
             (id, name, source_id, region, created_at, last_synced_at, enabled)
             VALUES ('acct-ds', 'DeepSeek dev', 'DeepSeek', NULL,
                     '2026-08-01T00:00:00+00:00', NULL, true)",
            [],
        )
        .unwrap();
    }

    fn snapshot(ledger: &mut Connection, days_ago: i64, balance: f64) {
        ledger::write_balance(
            ledger,
            &BalanceSnapshot {
                provider: "DeepSeek".to_string(),
                account_id: "acct-ds".to_string(),
                observed_at: day(days_ago),
                balance,
                granted_balance: None,
                topped_up_balance: Some(balance),
                currency: "CNY".to_string(),
            },
        )
        .unwrap();
    }

    #[test]
    fn the_default_rules_seed_once() {
        let (app, _) = stores();

        seed_default_rules_on(&app).unwrap();
        seed_default_rules_on(&app).unwrap();

        let rules = db::get_alert_rules_of(&app).unwrap();
        assert_eq!(rules.len(), 3);
        assert!(rules.iter().all(|rule| rule.enabled));
    }

    #[test]
    fn a_disabled_default_rule_stays_disabled() {
        let (app, _) = stores();

        seed_default_rules_on(&app).unwrap();
        db::set_alert_rule_enabled_to(&app, RULE_BALANCE_FLOOR, false).unwrap();
        seed_default_rules_on(&app).unwrap();

        let rules = db::get_alert_rules_of(&app).unwrap();
        let floor = rules
            .iter()
            .find(|rule| rule.id == RULE_BALANCE_FLOOR)
            .unwrap();
        assert!(!floor.enabled);
    }

    #[test]
    fn a_breach_needs_consecutive_days_over_the_baseline() {
        let daily: BTreeMap<NaiveDate, f64> = (1..=14)
            .map(|d| {
                let day = Utc
                    .with_ymd_and_hms(2026, 8, d, 0, 0, 0)
                    .unwrap()
                    .date_naive();
                // Ten a day, then 40 on the last two: 4× a baseline of 10.
                (day, if d >= 13 { 40.0 } else { 10.0 })
            })
            .collect();

        let breach = detect_breach(&daily, 2.5, 2).expect("two days at 4× is a breach");
        assert_eq!(breach.streak_days(), 2);
        assert!((breach.last_amount - 40.0).abs() < 1e-9);
        // The last day's baseline spans the six flat days and the first
        // breach day: (6×10 + 40) / 7.
        assert!((breach.baseline - 100.0 / 7.0).abs() < 1e-9);

        // One breaching day is not yet a streak.
        let last = Utc
            .with_ymd_and_hms(2026, 8, 14, 0, 0, 0)
            .unwrap()
            .date_naive();
        let one_day: BTreeMap<NaiveDate, f64> = daily
            .keys()
            .map(|day| (*day, if *day == last { 40.0 } else { 10.0 }))
            .collect();
        assert!(detect_breach(&one_day, 2.5, 2).is_none());
    }

    #[test]
    fn a_zero_baseline_never_fires() {
        // A brand-new service: any spend is infinitely above "nothing".
        let daily: BTreeMap<NaiveDate, f64> = [(2, 50.0), (3, 60.0)]
            .iter()
            .map(|(d, amount)| {
                (
                    Utc.with_ymd_and_hms(2026, 8, *d, 0, 0, 0)
                        .unwrap()
                        .date_naive(),
                    *amount,
                )
            })
            .collect();

        assert!(detect_breach(&daily, 2.5, 2).is_none());
    }

    #[test]
    fn a_cost_anomaly_fires_once_a_day_while_the_streak_holds() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();

        // Flat 10/day for twelve days, then 40/day for two. Everything is
        // tagged, so the untagged-ratio rule has nothing to say here.
        let tag = Some(r#"{"business_line":"etl"}"#);
        let mut charges = Vec::new();
        for back in (2..=13).rev() {
            charges.push(usage("Claude", 10.0, day(back), tag));
        }
        charges.push(usage("Claude", 40.0, day(1), tag));
        charges.push(usage("Claude", 40.0, day(0), tag));
        // A flat service alongside, which must not fire.
        for back in (0..=13).rev() {
            charges.push(usage("S3", 10.0, day(back), tag));
        }

        // Split by billing period, as the ledger replaces whole periods.
        let current = BillingPeriod::containing(Utc::now());
        let previous = current.previous();
        let this_month: Vec<Charge> = charges
            .iter()
            .filter(|c| BillingPeriod::containing(c.charge_period_start) == current)
            .cloned()
            .collect();
        let last_month: Vec<Charge> = charges
            .iter()
            .filter(|c| BillingPeriod::containing(c.charge_period_start) == previous)
            .cloned()
            .collect();
        write_period(&mut ledger, current, &this_month);
        if !last_month.is_empty() {
            write_period(&mut ledger, previous, &last_month);
        }

        let created = evaluate_with(&app, &ledger, Utc::now()).unwrap();
        assert_eq!(created, 1);

        let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.rule_id, RULE_COST_ANOMALY);
        assert_eq!(event.severity, Severity::Critical);
        assert!(event.title.contains("Claude"), "{}", event.title);
        assert!(event.body.contains("2 consecutive days"), "{}", event.body);
        assert!(event.stat_json.is_some());

        // Same day, same condition: no second event.
        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);

        // A disabled rule fires nothing, even on a later day.
        db::set_alert_rule_enabled_to(&app, RULE_COST_ANOMALY, false).unwrap();
        assert_eq!(
            evaluate_with(&app, &ledger, Utc::now() + Duration::days(1)).unwrap(),
            0
        );
    }

    #[test]
    fn a_balance_below_its_floor_projects_days_remaining() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();
        add_deepseek_account(&app);

        // ¥10/day of burn, ¥30 left against the default ¥200 floor.
        for (days_ago, balance) in [(6, 90.0), (4, 70.0), (2, 50.0), (0, 30.0)] {
            snapshot(&mut ledger, days_ago, balance);
        }

        let created = evaluate_with(&app, &ledger, Utc::now()).unwrap();
        assert_eq!(created, 1);

        let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
        let event = &events[0];
        assert_eq!(event.rule_id, RULE_BALANCE_FLOOR);
        assert_eq!(event.severity, Severity::Warning);
        assert!(event.body.contains("¥30.00"), "{}", event.body);
        assert!(event.body.contains("¥200.00 floor"), "{}", event.body);
        assert!(event.body.contains("roughly 3 days"), "{}", event.body);
    }

    #[test]
    fn an_accounts_budget_is_its_floor() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();
        add_deepseek_account(&app);
        app.execute(
            "INSERT INTO budgets
             (account_id, monthly_budget, currency, alert_threshold, created_at, updated_at)
             VALUES ('acct-ds', 25.0, 'CNY', 80.0,
                     '2026-08-01T00:00:00+00:00', '2026-08-01T00:00:00+00:00')",
            [],
        )
        .unwrap();

        // ¥30: below the default floor of 200, but above this account's 25.
        snapshot(&mut ledger, 0, 30.0);

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
    }

    #[test]
    fn a_balance_back_above_the_floor_resolves_the_event() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();
        add_deepseek_account(&app);
        snapshot(&mut ledger, 0, 30.0);

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);

        // A top-up arrives.
        snapshot(&mut ledger, -1, 500.0);
        assert_eq!(resolve_stale_with(&app, &ledger, Utc::now()).unwrap(), 1);
        assert!(db::get_alert_events_of(&app, &[AlertStatus::Open])
            .unwrap()
            .is_empty());
        assert_eq!(
            db::get_alert_events_of(&app, &[AlertStatus::Resolved])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_growing_untagged_share_warns_once_a_day() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();

        let current = BillingPeriod::containing(Utc::now());
        let previous = current.previous();
        let at_in = |period: BillingPeriod| period.start().and_hms_opt(12, 0, 0).unwrap().and_utc();

        // This month: half untagged. Last month: a tenth untagged.
        let this_month = vec![
            usage("EC2", 50.0, at_in(current), None),
            usage(
                "S3",
                50.0,
                at_in(current),
                Some(r#"{"business_line":"etl"}"#),
            ),
        ];
        let last_month = vec![
            usage("EC2", 10.0, at_in(previous), None),
            usage(
                "S3",
                90.0,
                at_in(previous),
                Some(r#"{"business_line":"etl"}"#),
            ),
        ];

        let key = PeriodKey::new("AWS", "acct-1", current.label());
        ledger::write_period(
            &mut ledger,
            &key,
            &ledger::new_batch_id(),
            &this_month,
            None,
            Channel::Api,
        )
        .unwrap();
        let key = PeriodKey::new("AWS", "acct-1", previous.label());
        ledger::write_period(
            &mut ledger,
            &key,
            &ledger::new_batch_id(),
            &last_month,
            None,
            Channel::Api,
        )
        .unwrap();

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
        let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
        assert_eq!(events[0].rule_id, RULE_UNTAGGED_RATIO);
        assert!(events[0].body.contains("50%"), "{}", events[0].body);

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
    }

    #[test]
    fn a_shrinking_or_small_untagged_share_does_not_warn() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();

        let current = BillingPeriod::containing(Utc::now());
        let at_in = |period: BillingPeriod| period.start().and_hms_opt(12, 0, 0).unwrap().and_utc();

        // 10% untagged: under the threshold.
        let charges = vec![
            usage("EC2", 10.0, at_in(current), None),
            usage(
                "S3",
                90.0,
                at_in(current),
                Some(r#"{"business_line":"etl"}"#),
            ),
        ];
        let key = PeriodKey::new("AWS", "acct-1", current.label());
        ledger::write_period(
            &mut ledger,
            &key,
            &ledger::new_batch_id(),
            &charges,
            None,
            Channel::Api,
        )
        .unwrap();

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
    }

    #[test]
    fn a_snooze_holds_the_condition_quiet_until_it_runs_out() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();
        add_deepseek_account(&app);
        snapshot(&mut ledger, 0, 30.0);

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
        let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();

        // Snoozed into the future: no refire.
        db::set_alert_event_status_to(
            &app,
            &events[0].id,
            AlertStatus::Snoozed,
            Some(Utc::now() + Duration::hours(24)),
        )
        .unwrap();
        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);

        // Snooze expired: the condition fires again.
        db::set_alert_event_status_to(
            &app,
            &events[0].id,
            AlertStatus::Snoozed,
            Some(Utc::now() - Duration::hours(1)),
        )
        .unwrap();
        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
    }

    #[test]
    fn an_empty_ledger_and_no_accounts_fire_nothing() {
        let (app, ledger) = stores();
        seed_default_rules_on(&app).unwrap();

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
        assert_eq!(resolve_stale_with(&app, &ledger, Utc::now()).unwrap(), 0);
    }

    #[test]
    fn a_custom_rule_is_created_enabled() {
        let (app, _) = stores();

        let view = create_rule_on(
            &app,
            RULE_UNTAGGED_RATIO,
            "Watch unallocated",
            json!({ "threshold": 0.25 }),
        )
        .unwrap();

        assert!(view.id.starts_with(CUSTOM_RULE_PREFIX), "{}", view.id);
        assert!(view.enabled);
        assert_eq!(view.scope, "All sources");
        assert!(
            view.condition_chips[0].contains("25%"),
            "{:?}",
            view.condition_chips
        );

        let rules = db::get_alert_rules_of(&app).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "Watch unallocated");
    }

    #[test]
    fn invalid_configs_are_refused() {
        let (app, _) = stores();

        assert!(create_rule_on(
            &app,
            RULE_COST_ANOMALY,
            "x",
            json!({ "multiplier": 1.0, "consecutive_days": 2 })
        )
        .is_err());
        assert!(create_rule_on(
            &app,
            RULE_COST_ANOMALY,
            "x",
            json!({ "multiplier": 2.5, "consecutive_days": 0 })
        )
        .is_err());
        assert!(create_rule_on(&app, RULE_BALANCE_FLOOR, "x", json!({ "floor": 0.0 })).is_err());
        assert!(
            create_rule_on(&app, RULE_UNTAGGED_RATIO, "x", json!({ "threshold": 1.5 })).is_err()
        );
        assert!(create_rule_on(&app, "nonsense", "x", json!({})).is_err());
        assert!(create_rule_on(&app, RULE_BALANCE_FLOOR, "   ", json!({ "floor": 50.0 })).is_err());

        // Nothing was stored along the way.
        assert!(db::get_alert_rules_of(&app).unwrap().is_empty());
    }

    #[test]
    fn only_custom_rules_can_be_deleted() {
        let (app, _) = stores();
        seed_default_rules_on(&app).unwrap();

        assert!(delete_rule_on(&app, RULE_BALANCE_FLOOR).is_err());

        let view = create_rule_on(
            &app,
            RULE_BALANCE_FLOOR,
            "Low floor",
            json!({ "floor": 50.0 }),
        )
        .unwrap();
        assert_eq!(db::get_alert_rules_of(&app).unwrap().len(), 4);

        delete_rule_on(&app, &view.id).unwrap();
        let rules = db::get_alert_rules_of(&app).unwrap();
        assert_eq!(rules.len(), 3);
        assert!(rules.iter().all(|rule| rule.id != view.id));
    }

    #[test]
    fn a_custom_rule_evaluates_like_a_seeded_one() {
        let (app, mut ledger) = stores();
        seed_default_rules_on(&app).unwrap();
        // The seeded floor of 200 would not fire on a balance of 300; the
        // custom floor of 500 must.
        db::set_alert_rule_enabled_to(&app, RULE_BALANCE_FLOOR, false).unwrap();
        add_deepseek_account(&app);

        let view = create_rule_on(
            &app,
            RULE_BALANCE_FLOOR,
            "High floor",
            json!({ "floor": 500.0 }),
        )
        .unwrap();
        snapshot(&mut ledger, 0, 300.0);

        assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
        let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
        assert_eq!(events[0].rule_id, view.id);
    }
}
