//! Fetching on a timer, so alerts fire without anyone pressing Refresh.
//!
//! Every [`TICK`] the schedule does what the Overview's Refresh does — fetch
//! each API account, which skips any period fetched within the refresh
//! interval — then reloads the menu bar's summary and announces the alerts
//! that fired. The tick only decides how soon a due account is noticed;
//! what a provider is asked for, and so what it costs, is still the
//! refresh interval's to decide.

use std::time::Duration;

use chrono::Utc;
use gpui_kit::*;

use super::summary::{AlertLine, Summary};
use super::{notify, Status};

/// How often the schedule wakes. Short beside the shortest refresh
/// interval (6 h), so a due account waits at most this long past it.
pub const TICK: Duration = Duration::from_secs(15 * 60);

/// The first run waits this long after the stores open, so it does not
/// compete with the window's first page load for the ledger.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(20);

pub(super) fn start(status: Entity<Status>, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_RUN_DELAY).await;
        loop {
            let fetch = crate::config::load_config()
                .map(|config| config.background_refresh)
                .unwrap_or(true);
            cx.update(|cx| run_once(status.clone(), fetch, cx));
            cx.background_executor().timer(TICK).await;
        }
    })
    .detach();
}

/// One pass: fetch what is due (when `fetch`), reload the summary, announce
/// new alerts, and have the window reload if it is open. A pass already
/// running makes this a no-op.
pub(super) fn run_once(status: Entity<Status>, fetch: bool, cx: &mut App) {
    if status.read(cx).refreshing {
        return;
    }
    tracing::debug!("Background refresh (fetch: {fetch})");
    status.update(cx, |status, cx| {
        status.refreshing = true;
        cx.notify();
    });

    cx.spawn(async move |cx| {
        let (fetched, summary) = smol::unblock(move || {
            let fetched = if fetch {
                fetch_due_accounts()
            } else {
                Fetched::default()
            };
            (fetched, Summary::load())
        })
        .await;

        cx.update(|cx| {
            let announce = status.update(cx, |status, cx| {
                status.refreshing = false;
                status.last_run = Some(Utc::now());
                status.failures = fetched.failures;
                let announce = match summary {
                    Ok(summary) => {
                        let fresh: Vec<AlertLine> = status
                            .announcer
                            .take_new(&summary.alerts)
                            .into_iter()
                            .cloned()
                            .collect();
                        status.summary = Some(summary);
                        fresh
                    }
                    Err(e) => {
                        tracing::warn!("Could not load the menu bar summary: {}", e);
                        Vec::new()
                    }
                };
                cx.notify();
                announce
            });

            let notifications = crate::config::load_config()
                .map(|config| config.alert_notifications)
                .unwrap_or(true);
            if notifications {
                for alert in &announce {
                    notify::post(alert);
                }
            }

            // What the window shows changed under it; a pass that fetched
            // nothing leaves the page alone rather than reloading it from
            // under the reader every tick.
            if fetched.ingested && cx.has_global::<crate::app::GlobalAppState>() {
                crate::app::request_reload(cx);
            }
        });
    })
    .detach();
}

/// What one pass of fetching did.
#[derive(Default)]
struct Fetched {
    /// Some period landed in the ledger.
    ingested: bool,
    /// The accounts that could not be fetched, as `name: reason`.
    failures: Vec<String>,
}

/// Fetch every account with an API channel, as Refresh does. Blocking.
fn fetch_due_accounts() -> Fetched {
    let mut fetched = Fetched::default();
    let accounts = match crate::db::get_all_accounts() {
        Ok(accounts) => accounts,
        Err(e) => {
            fetched.failures.push(format!("accounts: {e}"));
            return fetched;
        }
    };
    let due = accounts.iter().filter(|account| {
        account.enabled
            && account
                .descriptor()
                .is_some_and(|source| source.fetches_from_api())
    });
    for account in due {
        match crate::ingest::refresh_account(account, false) {
            Ok(outcome) => fetched.ingested |= !outcome.ingested.is_empty(),
            Err(e) => fetched.failures.push(format!("{}: {}", account.name, e)),
        }
    }
    fetched
}
