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

use anyhow::Result;
use chrono::{DateTime, Datelike, Utc};
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

/// Headline numbers for the Overview page.
pub struct OverviewStats {
    /// Month-to-date spend.
    pub mtd_spend: f64,
    /// Percent change vs the same day last month (signed). 0.0 when last
    /// month had nothing to compare against.
    pub mtd_change_pct: f64,
    /// Month-end forecast: MTD plus the mean of the last 7 nonzero daily
    /// totals times the remaining days.
    pub forecast_month_end: f64,
    /// Share of spend that reaches no business line (0–100).
    pub unallocated_pct: f64,
    /// Unallocated spend.
    pub unallocated_amount: f64,
    /// Open alerts.
    pub open_alerts: usize,
    /// Of the open alerts, how many are critical.
    pub critical_alerts: usize,
    /// Of the open alerts, how many are warnings.
    pub warning_alerts: usize,
}

/// One day of spend, keyed by day-of-month.
#[derive(Clone, Copy)]
pub struct DailyPoint {
    /// Day of month, 1-based.
    pub day: u8,
    /// Spend in the reporting currency.
    pub amount: f64,
}

/// The month-to-date daily spend chart: actual against a 7-day trailing
/// mean baseline.
pub struct DailySpend {
    /// One point per day of the current period so far.
    pub actual: Vec<DailyPoint>,
    /// Trailing 7-day mean ending the day before each actual point.
    pub baseline: Vec<DailyPoint>,
}

/// One row of the "Where it went" breakdown.
pub struct BusinessLineRow {
    pub name: String,
    /// Month-to-date spend.
    pub amount: f64,
}

/// One row of the "Biggest movers" table.
pub struct MoverRow {
    pub provider: String,
    pub service: String,
    /// Month-to-date spend.
    pub amount: f64,
    /// Percent change vs the previous period (signed). 0.0 for a service
    /// that had no spend last period.
    pub change_pct: f64,
    /// The business line this spend mostly drives, or "Untagged".
    pub drives: String,
}

/// Everything the Overview page renders.
pub struct OverviewData {
    pub currency: String,
    pub stats: OverviewStats,
    pub daily: DailySpend,
    pub business_lines: Vec<BusinessLineRow>,
    pub movers: Vec<MoverRow>,
}

/// Load the Overview page's data. Blocking; wrap in `smol::unblock`.
pub fn load_overview() -> Result<OverviewData> {
    let now = Utc::now();
    let current = BillingPeriod::containing(now);
    let previous = current.previous();
    let today = now.day();

    let mtd = query::total_for_period(&current.label())?;

    // Same-day MTD of the previous period: the sum of its days 1..=today.
    // A previous month shorter than today (e.g. February vs a 31st) just
    // runs out of days.
    let two_months_back = now - chrono::Duration::days(i64::from(today) + 31);
    let daily_all = query::daily_totals_all(two_months_back)?;
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

    let mtd_change_pct = if prev_mtd > 0.0 {
        (mtd - prev_mtd) / prev_mtd * 100.0
    } else {
        0.0
    };

    // Forecast: MTD plus the recent run rate for the days left.
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
    let forecast = mtd + run_rate * f64::from(days_in_month.saturating_sub(today));

    // Unallocated share of the current period.
    let breakdown = query::tag_breakdown(&current.label(), BUSINESS_LINE_TAG)?;
    let unallocated_amount = breakdown
        .iter()
        .find(|(value, _)| value == "Unallocated")
        .map(|(_, amount)| *amount)
        .unwrap_or(0.0);
    let unallocated_pct = if mtd > 0.0 {
        unallocated_amount / mtd * 100.0
    } else {
        0.0
    };

    let open = alerts::open_alerts().unwrap_or_default();
    let critical = open
        .iter()
        .filter(|a| a.severity == alerts::Severity::Critical)
        .count();

    // Baseline: the 7-day trailing mean ending the day before each point.
    let by_day: BTreeMap<u32, f64> = current_days.iter().copied().collect();
    let actual: Vec<DailyPoint> = (1..=today)
        .filter_map(|d| {
            by_day.get(&d).map(|amount| DailyPoint {
                day: d as u8,
                amount: *amount,
            })
        })
        .collect();
    let baseline = actual
        .iter()
        .map(|point| {
            let window: Vec<f64> = (1..=7u32)
                .filter_map(|back| point.day.checked_sub(back as u8))
                .map(|d| by_day.get(&u32::from(d)).copied().unwrap_or(0.0))
                .collect();
            DailyPoint {
                day: point.day,
                amount: window.iter().sum::<f64>() / 7.0,
            }
        })
        .collect();

    let business_lines = breakdown
        .into_iter()
        .map(|(name, amount)| BusinessLineRow { name, amount })
        .collect();

    // Movers: current vs previous period per (provider, service).
    let current_totals = query::provider_service_totals(&current.label())?;
    let previous_totals = query::provider_service_totals(&previous.label())?;
    let mut movers = Vec::new();
    for (provider, service, amount) in current_totals.into_iter().take(5) {
        let before = previous_totals
            .iter()
            .find(|(p, s, _)| p == &provider && s == &service)
            .map(|(_, _, amount)| *amount)
            .unwrap_or(0.0);
        let change_pct = if before > 0.0 {
            (amount - before) / before * 100.0
        } else {
            0.0
        };
        let drives =
            query::service_tag_breakdown(&current.label(), &provider, &service, BUSINESS_LINE_TAG)?
                .into_iter()
                .find(|(value, _)| value != "Unallocated")
                .map(|(value, _)| value)
                .unwrap_or_else(|| "Untagged".to_string());
        movers.push(MoverRow {
            provider,
            service,
            amount,
            change_pct,
            drives,
        });
    }

    Ok(OverviewData {
        currency: reporting_currency(),
        stats: OverviewStats {
            mtd_spend: mtd,
            mtd_change_pct,
            forecast_month_end: forecast,
            unallocated_pct,
            unallocated_amount,
            open_alerts: open.len(),
            critical_alerts: critical,
            warning_alerts: open.len() - critical,
        },
        daily: DailySpend { actual, baseline },
        business_lines,
        movers,
    })
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

/// The untagged share over which an account row reads "Untagged spend".
/// Kept in step with the default `untagged-ratio` rule threshold.
const UNTAGGED_STATE_THRESHOLD: f64 = 0.15;

/// Load the Accounts page's data. Blocking; wrap in `smol::unblock`.
pub fn load_accounts() -> Result<AccountsData> {
    let accounts = db::get_all_accounts()?;
    let ingests = query::last_ingests().unwrap_or_default();
    let open = alerts::open_alerts().unwrap_or_default();

    let mut rows = Vec::new();
    for account in accounts {
        let Some(descriptor) = account.descriptor() else {
            continue;
        };
        let provider = descriptor.id.to_string();

        let mtd = query::period_total(&ingest::period_key(
            &account,
            &BillingPeriod::containing(Utc::now()),
        ))?;
        let balance = query::latest_balance(&provider, &account.id)?;
        let last_sync = ingests
            .iter()
            .find(|(p, a, _)| p == &provider && a == &account.id)
            .map(|(_, _, at)| *at)
            .or(account.last_synced_at);

        // The worst applicable badge wins; the order is the mock's order of
        // severity.
        let state = account_state(&account, descriptor.is_snapshot(), &provider, &open)?;

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

    let used = query::api_fetches_this_month().unwrap_or(0);
    let (bytes, path) = match crate::cloud::raw::raw_dir_size() {
        Ok(bytes) => (bytes, crate::cloud::raw::raw_dir_path().unwrap_or_default()),
        Err(_) => (0, PathBuf::new()),
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

    let key = ingest::period_key(account, &BillingPeriod::containing(Utc::now()));
    let total = query::period_total(&key)?;
    if total > 0.0 {
        let untagged: f64 =
            query::untagged_detail(&key.billing_period, BUSINESS_LINE_TAG, usize::MAX)?
                .into_iter()
                .filter(|charge| charge.provider == provider)
                .map(|charge| charge.amount)
                .sum();
        if untagged / total > UNTAGGED_STATE_THRESHOLD {
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
    /// period total.
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

/// One of the largest unattributed charges, for the Unallocated card.
pub struct UnallocatedItem {
    pub provider: String,
    pub service: Option<String>,
    pub description: Option<String>,
    pub amount: f64,
}

/// The "Unallocated" explainer card.
pub struct UnallocatedCardData {
    pub amount: f64,
    pub pct: f64,
    /// The largest unattributed charges, biggest first.
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
/// and the `business_line` tag is the only split below the service.
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
    // column sums to the same total.
    let services = query::provider_service_totals(&period)?;
    let mut nodes: Vec<SankeyNode> = Vec::new();
    let mut links: Vec<SankeyLink> = Vec::new();
    let mut provider_totals: BTreeMap<String, f64> = BTreeMap::new();
    let mut line_totals: BTreeMap<String, f64> = BTreeMap::new();

    for (provider, service, amount) in &services {
        let src = format!("src-{provider}");
        let svc = format!("svc-{provider}-{service}");
        *provider_totals.entry(src.clone()).or_insert(0.0) += amount;

        let tags = query::service_tag_breakdown(&period, provider, service, BUSINESS_LINE_TAG)?;
        for (tag, tag_amount) in tags {
            let line = format!("line-{tag}");
            *line_totals.entry(line.clone()).or_insert(0.0) += tag_amount;
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

    let total = query::total_for_period(&period)?;
    let unallocated_amount = query::tag_breakdown(&period, BUSINESS_LINE_TAG)?
        .into_iter()
        .find(|(value, _)| value == "Unallocated")
        .map(|(_, amount)| amount)
        .unwrap_or(0.0);
    let largest = query::untagged_detail(&period, BUSINESS_LINE_TAG, 3)?
        .into_iter()
        .map(|charge| UnallocatedItem {
            provider: charge.provider,
            service: charge.service,
            description: charge.description,
            amount: charge.amount,
        })
        .collect();

    Ok(AttributionData {
        currency,
        path,
        sankey: SankeyData { nodes, links },
        unallocated: UnallocatedCardData {
            amount: unallocated_amount,
            pct: if total > 0.0 {
                unallocated_amount / total * 100.0
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
    /// Events resolved in the current period, newest first.
    pub resolved_this_month: Vec<AlertView>,
}

/// Heading of the resolved-alerts section under the open ones.
pub const RESOLVED_SECTION_TITLE: &str = "RESOLVED THIS MONTH";

/// Load the Alerts page's data. Blocking; wrap in `smol::unblock`.
pub fn load_alerts() -> Result<AlertsData> {
    // Conditions that stopped holding while nobody looked still resolve.
    let _ = alerts::resolve_stale_alerts();

    let open = alerts::open_alerts()?;
    let resolved = alerts::alerts_by_status(&[AlertStatus::Resolved])?;
    let current = BillingPeriod::containing(Utc::now()).label();
    let resolved_this_month: Vec<AlertView> = resolved
        .into_iter()
        .filter(|alert| alert.created_at.format("%Y-%m").to_string() == current)
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

/// Enable or disable a rule.
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

/// Snooze an alert for `hours`.
pub fn snooze_alert(id: &str, hours: i64) -> Result<()> {
    alerts::snooze_alert(id, hours)
}

/// Dismiss an alert.
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
    let accounts = db::get_all_accounts().unwrap_or_default();
    let last = query::last_ingests()
        .unwrap_or_default()
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
