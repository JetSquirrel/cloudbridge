use super::*;

// ==================== Sidebar ====================

/// The sync summary in the status bar.
pub struct SyncStatus {
    /// The freshest successful ingest across all accounts, if any.
    pub last_synced_at: Option<DateTime<Utc>>,
    /// How many accounts are configured.
    pub source_count: usize,
    /// When Refresh would next fetch rather than skip: the last sync plus
    /// the configured freshness window, which the background schedule
    /// waits out like the button does. `None`
    /// before the first sync.
    pub next_fetch_at: Option<DateTime<Utc>>,
}

/// Load the status bar's sync status. Blocking; wrap in `smol::unblock`.
pub fn load_sync_status() -> Result<SyncStatus> {
    let accounts = db::get_all_accounts().unwrap_or_else(|e| {
        tracing::warn!("Could not list accounts for the sync status: {}", e);
        Vec::new()
    });
    let last = query::last_ingests()
        .unwrap_or_else(|e| {
            tracing::warn!(
                "Could not read the last-ingest times for the sync status: {}",
                e
            );
            Vec::new()
        })
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
