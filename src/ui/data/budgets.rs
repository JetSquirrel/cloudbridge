use super::*;

// ==================== Budgets ====================

/// Per-account budget consumption, for the Rules page's budget list.
/// Blocking.
pub fn load_budget_statuses() -> Result<Vec<BudgetStatus>> {
    db::get_all_budget_statuses()
}

/// `(id, name)` of every account, for the budget editor and the budget
/// rule's account picker. Blocking.
pub fn account_names() -> Result<Vec<(String, String)>> {
    Ok(db::get_all_accounts()?
        .into_iter()
        .map(|account| (account.id, account.name))
        .collect())
}

/// Save an account's budget. The original creation stamp is kept — a save
/// is an update. Blocking.
pub fn save_budget(account_id: &str, monthly_budget: f64, alert_threshold: f64) -> Result<()> {
    let existing = db::get_budget(account_id)?;
    let now = Utc::now();
    db::save_budget(&BudgetInfo {
        account_id: account_id.to_string(),
        monthly_budget,
        // Consumption is measured in the reporting currency, so that is the
        // currency a budget is recorded in.
        currency: reporting_currency(),
        alert_threshold,
        created_at: existing.map(|budget| budget.created_at).unwrap_or(now),
        updated_at: now,
    })
}

/// Remove an account's budget. Blocking.
pub fn delete_budget(account_id: &str) -> Result<()> {
    db::delete_budget(account_id)
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
