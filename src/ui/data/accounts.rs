use super::*;

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
    pub fn label(self) -> SharedString {
        match self {
            Self::Healthy => tr!("account_state.healthy"),
            Self::Anomaly => tr!("account_state.anomaly"),
            Self::UntaggedSpend => tr!("account_state.untagged"),
            Self::LowBalance => tr!("account_state.low_balance"),
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
    /// A balance-reporting source: its number is the balance, not MTD.
    pub is_snapshot: bool,
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
    /// What the ledger holds for each account, configured or not.
    pub histories: Vec<query::LedgerAccount>,
    /// The histories no account row names any more: left behind by a
    /// delete that kept them, and still counted in every total.
    pub orphans: Vec<query::LedgerAccount>,
}

/// The ceiling of paid fetches the budget card measures against.
pub const API_CALL_CEILING: u32 = 100;

/// What one paid billing-API call costs, in USD.
pub const API_CALL_COST_USD: f64 = 0.01;

/// Load the Accounts page's data. Blocking; wrap in `smol::unblock`.
pub fn load_accounts() -> Result<AccountsData> {
    let now = Utc::now();
    let current = BillingPeriod::containing(now);
    let accounts = db::get_all_accounts()?;
    let histories = query::ledger_accounts().unwrap_or_else(|e| {
        tracing::warn!("Could not read the ledger's accounts: {}", e);
        Vec::new()
    });
    let orphans = orphaned_histories(&histories, &accounts);
    let ingests = query::last_ingests().unwrap_or_else(|e| {
        tracing::warn!("Could not read the last-ingest times: {}", e);
        Vec::new()
    });
    let open = alerts::open_alerts().unwrap_or_else(|e| {
        tracing::warn!("Could not read open alerts for the account badges: {}", e);
        Vec::new()
    });

    // Untagged usage per provider of the current period, for the badge
    // check: one period-wide query, built on first use and shared by every
    // account instead of being re-queried per account.
    let mut untagged_by_account: Option<BTreeMap<String, f64>> = None;

    let mut rows = Vec::new();
    for account in accounts {
        let Some(descriptor) = account.descriptor() else {
            continue;
        };
        let provider = descriptor.id.to_string();

        let mtd = query::period_total(&ingest::period_key(&account, &current))?;
        let balance = query::latest_balance(&provider, &account.id)?;
        let last_sync = ingests
            .iter()
            .find(|(p, a, _)| p == &provider && a == &account.id)
            .map(|(_, _, at)| *at)
            .or(account.last_synced_at);

        // The worst applicable badge wins; the order is the mock's order of
        // severity.
        let state = account_state(
            &account,
            descriptor.is_snapshot(),
            &provider,
            mtd,
            &open,
            &current,
            &mut untagged_by_account,
        )?;

        rows.push(AccountRowData {
            id: account.id.clone(),
            name: account.name.clone(),
            provider: descriptor.display_name.to_string(),
            source_kind: source_kind(descriptor),
            is_snapshot: descriptor.is_snapshot(),
            mtd,
            balance: balance.map(|b| (b.balance, b.currency)),
            last_sync,
            state,
        });
    }

    let used = query::api_fetches_this_month().unwrap_or_else(|e| {
        tracing::warn!("Could not count this month's paid fetches: {}", e);
        0
    });
    let (bytes, path) = match crate::cloud::raw::raw_dir_size() {
        Ok(bytes) => (bytes, crate::cloud::raw::raw_dir_path().unwrap_or_default()),
        Err(e) => {
            tracing::warn!("Could not measure the raw-payload store: {}", e);
            (0, PathBuf::new())
        }
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
        histories,
        orphans,
    })
}

/// The ledger histories whose account no longer exists. Measured against
/// every account row, not the table's: an account whose source this build
/// does not know is left out of the table but still owns its history.
pub(super) fn orphaned_histories(
    histories: &[query::LedgerAccount],
    accounts: &[CloudAccount],
) -> Vec<query::LedgerAccount> {
    histories
        .iter()
        .filter(|history| {
            !accounts
                .iter()
                .any(|account| account.id == history.account_id)
        })
        .cloned()
        .collect()
}

/// Delete an account's billing history, ledger rows and raw payloads.
/// Blocking; wrap in `smol::unblock`.
pub fn delete_account_history(account_id: &str) -> Result<usize> {
    ingest::delete_account_history(account_id)
}

/// What a source reports, as the accounts table's kind column.
fn source_kind(descriptor: &registry::SourceDescriptor) -> String {
    match (
        descriptor.is_snapshot(),
        descriptor.fetches_from_api(),
        descriptor.imports_bill_file(),
    ) {
        (true, _, _) => tr!("source_kind.balance_only").into(),
        (false, true, true) => tr!("source_kind.api_and_import").into(),
        (false, true, false) => tr!("source_kind.api").into(),
        (false, false, true) => tr!("source_kind.import").into(),
        (false, false, false) => tr!("source_kind.none").into(),
    }
}

/// The badge of one account row.
///
/// The untagged check is scoped to the account's provider, not the account
/// itself: tag breakdowns are not account-split today, so two accounts of
/// one provider share the badge. `untagged_by_provider` caches the
/// period-wide untagged sums across the accounts-table loop.
fn account_state(
    account: &crate::cloud::CloudAccount,
    is_snapshot: bool,
    provider: &str,
    mtd: f64,
    open: &[AlertView],
    current: &BillingPeriod,
    untagged_by_account: &mut Option<BTreeMap<String, f64>>,
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

    // A rule scoped to an account names it. An unscoped rule watches the whole
    // provider, so it can only be matched that far.
    let anomaly = open.iter().any(|alert| {
        alert.kind == AlertKind::CostAnomaly
            && match alert.context.get("account_id").and_then(|v| v.as_str()) {
                Some(account_id) => account_id == account.id,
                None => alert.context.get("provider").and_then(|v| v.as_str()) == Some(provider),
            }
    });
    if anomaly {
        return Ok(AccountState::Anomaly);
    }

    // The month-to-date total comes from the caller, which already read it
    // for the row's mtd column.
    if mtd > 0.0 {
        if untagged_by_account.is_none() {
            *untagged_by_account = Some(
                query::untagged_totals_by_account(&current.label(), BUSINESS_LINE_TAG)?
                    .into_iter()
                    .collect(),
            );
        }
        let untagged = untagged_by_account
            .as_ref()
            .expect("just populated")
            .get(&account.id)
            .copied()
            .unwrap_or(0.0);
        if untagged / mtd > alerts::DEFAULT_UNTAGGED_THRESHOLD {
            return Ok(AccountState::UntaggedSpend);
        }
    }

    Ok(AccountState::Healthy)
}
