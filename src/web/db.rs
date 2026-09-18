//! Application state, held in memory.
//!
//! The desktop keeps accounts, budgets, alert rules and their events in
//! their own DuckDB file. A page in a browser has no file to open, so the
//! rows live in [`crate::memory`] instead — but nothing above this module
//! can tell. The pages, the alerting engine and the account forms call
//! `db::…` exactly as they do on the desktop, and every function here
//! answers with the shape its DuckDB twin answered.
//!
//! That shape is the contract, not a database. Rows are upserted by primary
//! key the way `INSERT OR REPLACE` upserted them; `get_alert_rules` keeps
//! the order rules were first stored in while `get_alert_events` is newest
//! first; and a live alert event is still one in `open` or `snoozed`. Each
//! of those is load-bearing for callers compiled for both targets — the
//! alerting engine's de-duplication reads the last of them on every
//! evaluation — so they are reproduced here rather than simplified.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};

use crate::alerts::{AlertEvent, AlertRule, AlertStatus, RULE_BUDGET};
use crate::cloud::{
    BillingPeriod, BudgetInfo, BudgetStatus, CloudAccount, SourceContext, SourceDescriptor,
};
use crate::ledger::{query, PeriodKey};
use crate::memory;
use crate::model::access_key_hint;
use crate::secret_store;
use crate::store::Connection;

/// Nothing to open.
///
/// The desktop opens the app-state file and brings it up to the current
/// schema version. The store exists from the first call, so all this has to
/// do is hand [`prepare_schema`] the call `init_database` owes it on both
/// targets.
pub fn init_database() -> Result<()> {
    prepare_schema(&Connection)
}

/// Nothing to create.
///
/// The desktop creates the tables and migrates the file. Here the store's
/// fields are the schema, so this exists for the shared code's tests, which
/// call it against whichever backend the target compiled.
pub(crate) fn prepare_schema(_conn: &Connection) -> Result<()> {
    Ok(())
}

/// Run `f` against the app-state handle.
///
/// The handle carries nothing — both databases are in [`crate::memory`] — so
/// this is the call shape rather than a transaction. Callers that hold the
/// ledger handle at the same time (the alerting engine) nest the same way
/// they do on the desktop, where the two connections are separate objects.
pub(crate) fn with_connection<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    f(&Connection)
}

// ==================== Accounts ====================

/// Save a cloud account: the credentials to the secret store, everything
/// else to the store.
///
/// The hint is derived from the key given here rather than taken from
/// `account`, so the stored row cannot end up describing a key it was not
/// saved with. An account with no access key keeps no hint — the demo seeds
/// all of its accounts that way, since none of them signs a request.
pub fn save_account(
    account: &CloudAccount,
    access_key_id: &str,
    secret_access_key: &str,
) -> Result<()> {
    let hint = if access_key_id.is_empty() {
        None
    } else {
        secret_store::store_account_secrets(&account.id, access_key_id, secret_access_key)?;
        Some(access_key_hint(access_key_id))
    };

    let stored = CloudAccount {
        access_key_hint: hint,
        ..account.clone()
    };

    memory::with_store(|store| {
        let mut accounts = store.accounts.borrow_mut();
        match accounts
            .iter_mut()
            .find(|existing| existing.id == stored.id)
        {
            Some(existing) => *existing = stored,
            None => accounts.push(stored),
        }
    });

    Ok(())
}

/// The credentials for an account, read at the moment they are needed.
///
/// The desktop reads them from the keyring and fails when it finds none.
/// There is no keyring in a browser, so this fails for the same reason
/// rather than for a different one: nothing was ever stored, and a request
/// cannot be signed with a credential that does not exist.
pub fn account_context(
    account: &CloudAccount,
    descriptor: &SourceDescriptor,
) -> Result<SourceContext> {
    let (access_key_id, secret_access_key) = secret_store::get_account_secrets(&account.id)?
        .ok_or_else(|| {
            anyhow!(
                "No credentials stored for account {}; they have to be re-entered",
                account.name
            )
        })?;

    // An account stored before the hint was recorded has none. Write it now,
    // from a key that has just been read anyway, rather than reading one for
    // the sake of the display.
    if account.access_key_hint.is_none() {
        set_access_key_hint(&account.id, &access_key_id);
    }

    Ok(SourceContext {
        access_key_id,
        secret_access_key,
        region: descriptor.region_or_default(account.region.clone()),
        export_uri: account.export_uri.clone(),
    })
}

/// Record the leading characters of an account's access key on its row, for
/// a list that must not read the key itself.
fn set_access_key_hint(account_id: &str, access_key_id: &str) {
    memory::with_store(|store| {
        if let Some(account) = store
            .accounts
            .borrow_mut()
            .iter_mut()
            .find(|account| account.id == account_id)
        {
            account.access_key_hint = Some(access_key_hint(access_key_id));
        }
    });
}

/// Get all cloud accounts.
pub fn get_all_accounts() -> Result<Vec<CloudAccount>> {
    with_connection(get_all_accounts_of)
}

pub(crate) fn get_all_accounts_of(_conn: &Connection) -> Result<Vec<CloudAccount>> {
    Ok(memory::with_store(|store| {
        store
            .accounts
            .borrow()
            .iter()
            .filter(|account| {
                // An id with no descriptor comes from a build that knew a
                // source this one does not. Skip the row rather than
                // guessing: filing its costs under another source would be
                // worse than leaving it out of the list.
                let known = account.source_id.descriptor().is_some();
                if !known {
                    tracing::warn!(
                        "Skipping account {} ({}): no billing source registered under '{}'",
                        account.name,
                        account.id,
                        account.source_id.as_str()
                    );
                }
                known
            })
            .cloned()
            .collect()
    }))
}

/// Delete a cloud account, and the budget that only meant anything with it.
///
/// The alerts fired for it stay, as they do on the desktop, which deletes
/// exactly these rows and the account's credentials.
pub fn delete_account(account_id: &str) -> Result<()> {
    memory::with_store(|store| {
        store
            .budgets
            .borrow_mut()
            .retain(|budget| budget.account_id != account_id);
        store
            .accounts
            .borrow_mut()
            .retain(|account| account.id != account_id);
    });

    if let Err(e) = secret_store::delete_account_secrets(account_id) {
        tracing::warn!("Failed to delete account secrets: {}", e);
    }

    Ok(())
}

// ==================== Budgets ====================

/// Save or update an account's budget.
///
/// An update keeps the row's place, as replacing a row under a primary key
/// did — the Rules page lists budgets in the order they were added.
pub fn save_budget(budget: &BudgetInfo) -> Result<()> {
    memory::with_store(|store| {
        let mut budgets = store.budgets.borrow_mut();
        match budgets
            .iter_mut()
            .find(|existing| existing.account_id == budget.account_id)
        {
            Some(existing) => *existing = budget.clone(),
            None => budgets.push(budget.clone()),
        }
    });

    tracing::info!("Saved budget for account {}", budget.account_id);
    Ok(())
}

/// Get budget for an account.
pub fn get_budget(account_id: &str) -> Result<Option<BudgetInfo>> {
    with_connection(|conn| get_budget_of(conn, account_id))
}

pub(crate) fn get_budget_of(_conn: &Connection, account_id: &str) -> Result<Option<BudgetInfo>> {
    Ok(memory::with_store(|store| {
        store
            .budgets
            .borrow()
            .iter()
            .find(|budget| budget.account_id == account_id)
            .cloned()
    }))
}

/// Get all budgets.
pub fn get_all_budgets() -> Result<Vec<BudgetInfo>> {
    Ok(memory::with_store(|store| store.budgets.borrow().clone()))
}

/// Delete budget for an account.
pub fn delete_budget(account_id: &str) -> Result<()> {
    memory::with_store(|store| {
        store
            .budgets
            .borrow_mut()
            .retain(|budget| budget.account_id != account_id);
    });

    tracing::info!("Deleted budget for account {}", account_id);
    Ok(())
}

/// Get budget status (compares budget with current costs).
pub fn get_budget_status(account_id: &str) -> Result<Option<BudgetStatus>> {
    let Some(budget) = get_budget(account_id)? else {
        return Ok(None);
    };

    let accounts = get_all_accounts()?;
    let account = accounts
        .iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| anyhow!("Account not found"))?;

    // What the ledger says has been charged this month, in the reporting
    // currency. Budgets are recorded in that same currency (the Rules page
    // writes them so), which is what makes the comparison meaningful.
    let period = BillingPeriod::containing(Utc::now());
    let current_cost = query::period_total(&PeriodKey::new(
        account.source_id.as_str().to_string(),
        account.id.clone(),
        period.label(),
    ))?;

    let percentage_used = if budget.monthly_budget > 0.0 {
        (current_cost / budget.monthly_budget) * 100.0
    } else {
        0.0
    };
    let remaining = budget.monthly_budget - current_cost;

    // Mirrors the budget alert rules: a live event means an evaluated rule
    // fired for this account. The threshold check keeps the badge honest
    // before the first evaluation runs.
    let alert_triggered =
        has_live_budget_alert(account_id) || percentage_used >= budget.alert_threshold;

    Ok(Some(BudgetStatus {
        account_id: account_id.to_string(),
        account_name: account.name.clone(),
        monthly_budget: budget.monthly_budget,
        current_cost,
        currency: budget.currency,
        percentage_used,
        remaining,
        alert_triggered,
    }))
}

/// Whether any open or snoozed budget-rule event names this account.
///
/// The desktop asks this in SQL, joining each event to its rule to check the
/// kind. The join matters here too: an event whose rule was deleted counts
/// for nothing, as it does there.
fn has_live_budget_alert(account_id: &str) -> bool {
    let prefix = format!("budget|{}|", account_id);

    memory::with_store(|store| {
        let events = store.alert_events.borrow();
        let rules = store.alert_rules.borrow();

        events.iter().any(|event| {
            event.dedupe_key.starts_with(&prefix)
                && matches!(event.status, AlertStatus::Open | AlertStatus::Snoozed)
                && rules
                    .iter()
                    .any(|rule| rule.id == event.rule_id && rule.kind == RULE_BUDGET)
        })
    })
}

/// Get all budget statuses.
pub fn get_all_budget_statuses() -> Result<Vec<BudgetStatus>> {
    let budgets = get_all_budgets()?;
    let mut statuses = Vec::new();

    for budget in budgets {
        if let Some(status) = get_budget_status(&budget.account_id)? {
            statuses.push(status);
        }
    }

    Ok(statuses)
}

/// Record that an account was successfully refreshed.
pub fn mark_account_synced(account_id: &str, at: DateTime<Utc>) -> Result<()> {
    memory::with_store(|store| {
        if let Some(account) = store
            .accounts
            .borrow_mut()
            .iter_mut()
            .find(|account| account.id == account_id)
        {
            account.last_synced_at = Some(at);
        }
    });

    Ok(())
}

// ==================== Dismissed quality issues ====================
//
// The persistence half of the data-quality dismissal scheme: the UI keys a
// finding as `{kind}:{billing_period}` and the loaders filter stored keys
// out, so a dismissed finding stays hidden for its period while a new
// period or a different kind still shows.

/// Record a data-quality issue dismissal. Re-dismissing a key is a no-op:
/// the key is what is stored, and the desktop's upsert by key says the same.
pub fn dismiss_quality_issue(issue_key: &str) -> Result<()> {
    memory::with_store(|store| {
        let mut keys = store.dismissed_quality_keys.borrow_mut();
        if !keys.iter().any(|key| key == issue_key) {
            keys.push(issue_key.to_string());
        }
    });

    Ok(())
}

/// Every dismissed issue key.
pub fn dismissed_quality_issue_keys() -> Result<HashSet<String>> {
    Ok(memory::with_store(|store| {
        store
            .dismissed_quality_keys
            .borrow()
            .iter()
            .cloned()
            .collect()
    }))
}

/// Forget every dismissal — all findings resurface on the next load.
pub fn clear_dismissed_quality_issues() -> Result<()> {
    memory::with_store(|store| store.dismissed_quality_keys.borrow_mut().clear());

    Ok(())
}

// ==================== Alert rules ====================

/// Save or update an alerting rule.
///
/// Like a budget, an update keeps the rule where it was, and for a stronger
/// reason: `get_alert_rules` is documented as the order rules were first
/// stored, and the Rules page shows them in that order.
pub(crate) fn save_alert_rule_to(_conn: &Connection, rule: &AlertRule) -> Result<()> {
    memory::with_store(|store| {
        let mut rules = store.alert_rules.borrow_mut();
        match rules.iter_mut().find(|existing| existing.id == rule.id) {
            Some(existing) => *existing = rule.clone(),
            None => rules.push(rule.clone()),
        }
    });

    Ok(())
}

/// Whether a rule with this id is already stored.
pub(crate) fn alert_rule_exists(_conn: &Connection, id: &str) -> Result<bool> {
    Ok(memory::with_store(|store| {
        store.alert_rules.borrow().iter().any(|rule| rule.id == id)
    }))
}

/// Every alerting rule, in the order they were first stored.
pub fn get_alert_rules() -> Result<Vec<AlertRule>> {
    with_connection(get_alert_rules_of)
}

pub(crate) fn get_alert_rules_of(_conn: &Connection) -> Result<Vec<AlertRule>> {
    Ok(memory::with_store(|store| {
        store.alert_rules.borrow().clone()
    }))
}

/// Enable or disable a rule. A disabled rule keeps its events but fires no
/// new ones.
pub fn set_alert_rule_enabled(id: &str, enabled: bool) -> Result<()> {
    with_connection(|conn| set_alert_rule_enabled_to(conn, id, enabled))
}

pub(crate) fn set_alert_rule_enabled_to(_conn: &Connection, id: &str, enabled: bool) -> Result<()> {
    memory::with_store(|store| {
        if let Some(rule) = store
            .alert_rules
            .borrow_mut()
            .iter_mut()
            .find(|rule| rule.id == id)
        {
            rule.enabled = enabled;
        }
    });

    Ok(())
}

/// Record when a rule last produced an event, for the "last fired" the
/// Rules page shows.
pub(crate) fn mark_rule_fired_to(_conn: &Connection, id: &str, at: DateTime<Utc>) -> Result<()> {
    memory::with_store(|store| {
        if let Some(rule) = store
            .alert_rules
            .borrow_mut()
            .iter_mut()
            .find(|rule| rule.id == id)
        {
            rule.last_fired_at = Some(at);
        }
    });

    Ok(())
}

/// Delete an alerting rule. Its past events stay: they are history, not part
/// of the rule.
pub fn delete_alert_rule(id: &str) -> Result<()> {
    with_connection(|conn| delete_alert_rule_to(conn, id))
}

pub(crate) fn delete_alert_rule_to(_conn: &Connection, id: &str) -> Result<()> {
    memory::with_store(|store| {
        store.alert_rules.borrow_mut().retain(|rule| rule.id != id);
    });

    Ok(())
}

// ==================== Alert events ====================

/// Record a new alert event.
pub(crate) fn insert_alert_event_to(_conn: &Connection, event: &AlertEvent) -> Result<()> {
    memory::with_store(|store| {
        let mut events = store.alert_events.borrow_mut();
        match events.iter_mut().find(|existing| existing.id == event.id) {
            Some(existing) => *existing = event.clone(),
            None => events.push(event.clone()),
        }
    });

    Ok(())
}

/// Events in one of the given states, newest first.
pub fn get_alert_events(statuses: &[AlertStatus]) -> Result<Vec<AlertEvent>> {
    with_connection(|conn| get_alert_events_of(conn, statuses))
}

pub(crate) fn get_alert_events_of(
    _conn: &Connection,
    statuses: &[AlertStatus],
) -> Result<Vec<AlertEvent>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    Ok(memory::with_store(|store| {
        let events = store.alert_events.borrow();
        let mut matching: Vec<AlertEvent> = events
            .iter()
            .filter(|event| statuses.contains(&event.status))
            .cloned()
            .collect();

        // The sort is stable, so events sharing a timestamp stay in the
        // order they were inserted — the order `ORDER BY created_at DESC`
        // was free to pick from.
        matching.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        matching
    }))
}

/// The open or snoozed event under a dedupe key, if there is one — the
/// check that keeps a condition from alerting twice while it still holds.
pub(crate) fn find_live_alert_event_of(
    _conn: &Connection,
    dedupe_key: &str,
) -> Result<Option<AlertEvent>> {
    Ok(memory::with_store(|store| {
        store
            .alert_events
            .borrow()
            .iter()
            .filter(|event| event.dedupe_key == dedupe_key)
            .filter(|event| matches!(event.status, AlertStatus::Open | AlertStatus::Snoozed))
            .max_by_key(|event| event.created_at)
            .cloned()
    }))
}

/// Move an event to a new state. `snoozed_until` matters only for
/// [`AlertStatus::Snoozed`]. Reaching a final state stamps `resolved_at`;
/// leaving one (reopened, or snoozed again) clears it.
pub fn set_alert_event_status(
    id: &str,
    status: AlertStatus,
    snoozed_until: Option<DateTime<Utc>>,
) -> Result<()> {
    with_connection(|conn| set_alert_event_status_to(conn, id, status, snoozed_until))
}

pub(crate) fn set_alert_event_status_to(
    _conn: &Connection,
    id: &str,
    status: AlertStatus,
    snoozed_until: Option<DateTime<Utc>>,
) -> Result<()> {
    let resolved_at = match status {
        AlertStatus::Resolved | AlertStatus::Dismissed => Some(Utc::now()),
        _ => None,
    };

    memory::with_store(|store| {
        if let Some(event) = store
            .alert_events
            .borrow_mut()
            .iter_mut()
            .find(|event| event.id == id)
        {
            event.status = status;
            event.snoozed_until = snoozed_until;
            event.resolved_at = resolved_at;
        }
    });

    Ok(())
}
