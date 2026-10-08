//! Fetching on a timer, so alerts fire without anyone pressing Refresh.
//!
//! Every [`TICK`] the schedule does what the Overview's Refresh does — fetch
//! each API account, which skips any period fetched within the refresh
//! interval — then reloads the menu bar's summary and announces the alerts
//! that fired. The tick only decides how soon a due account is noticed;
//! what a provider is asked for, and so what it costs, is still the
//! refresh interval's to decide. An account that keeps failing is backed
//! off ([`super::backoff`]), and its first failure is announced.

use std::collections::HashSet;
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
            cx.update(|cx| run_once(status.clone(), fetch, false, cx));
            cx.background_executor().timer(TICK).await;
        }
    })
    .detach();
}

/// One pass: fetch what is due (when `fetch`), reload the summary, announce
/// new alerts and newly failing accounts, and have the window reload if it
/// is open. A `manual` pass — someone pressed Refresh — also tries the
/// accounts that are backing off. A pass already running makes this a
/// no-op.
pub(super) fn run_once(status: Entity<Status>, fetch: bool, manual: bool, cx: &mut App) {
    if status.read(cx).refreshing {
        return;
    }
    tracing::debug!("Background refresh (fetch: {fetch}, manual: {manual})");
    let now = Utc::now();
    let waiting: HashSet<String> = if manual {
        HashSet::new()
    } else {
        status.read(cx).backoff.waiting(now)
    };
    status.update(cx, |status, cx| {
        status.refreshing = true;
        cx.notify();
    });

    cx.spawn(async move |cx| {
        let (fetched, summary) = smol::unblock(move || {
            let fetched = if fetch {
                fetch_due_accounts(&waiting)
            } else {
                Fetched::default()
            };
            (fetched, Summary::load())
        })
        .await;

        cx.update(|cx| {
            let cap = chrono::Duration::hours(i64::from(
                crate::config::load_config()
                    .map(|config| config.refresh_interval_hours)
                    .unwrap_or(crate::config::DEFAULT_REFRESH_INTERVAL_HOURS),
            ));
            let (announce, newly_failing) = status.update(cx, |status, cx| {
                let now = Utc::now();
                let mut newly_failing = Vec::new();
                for tried in fetched.accounts {
                    match tried.error {
                        None => status.backoff.succeeded(&tried.id),
                        Some(error) => {
                            if status.backoff.failed(
                                &tried.id,
                                &tried.name,
                                error.clone(),
                                now,
                                cap,
                            ) {
                                newly_failing.push((tried.name, error));
                            }
                        }
                    }
                }
                status.refreshing = false;
                status.last_run = Some(now);
                status.failures = status.backoff.failures();
                status.failures.extend(fetched.failures);
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
                (announce, newly_failing)
            });

            let notifications = crate::config::load_config()
                .map(|config| config.alert_notifications)
                .unwrap_or(true);
            if notifications {
                for alert in &announce {
                    notify::post(alert);
                }
                // An account that stopped refreshing has stopped being
                // watched; that is worth a notification of its own.
                for (name, error) in &newly_failing {
                    notify::post_refresh_failure(name, error);
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
    /// Each account tried, and how it went.
    accounts: Vec<Tried>,
    /// Failures that belong to no account, as `what: reason`.
    failures: Vec<String>,
}

struct Tried {
    id: String,
    name: String,
    error: Option<String>,
}

/// Fetch every account with an API channel, as Refresh does, except the
/// ones `waiting` out a backoff. Blocking.
fn fetch_due_accounts(waiting: &HashSet<String>) -> Fetched {
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
            && !waiting.contains(&account.id)
            && account
                .descriptor()
                .is_some_and(|source| source.fetches_from_api())
    });
    for account in due {
        let error = match crate::ingest::refresh_account(account, false) {
            Ok(outcome) => {
                fetched.ingested |= !outcome.ingested.is_empty();
                None
            }
            Err(e) => Some(e.to_string()),
        };
        fetched.accounts.push(Tried {
            id: account.id.clone(),
            name: account.name.clone(),
            error,
        });
    }
    fetched
}
