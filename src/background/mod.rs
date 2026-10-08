//! CloudBridge while nobody is looking at it.
//!
//! A cost alert is only worth something if it arrives while the money is
//! still being spent. Before this module the app fetched only when someone
//! pressed Refresh with the window open, so a runaway Durable Object could
//! run for weeks with every alert unread. This keeps the app working once
//! its window is closed:
//!
//! - **a schedule** ([`schedule`]) that fetches whatever accounts are due
//!   and runs the alert rules, every [`schedule::TICK`], window or not;
//! - **notifications** ([`notify`]) for each alert that fires during the run;
//! - **a menu bar icon** (`tray`, macOS and Windows, unless Settings turns
//!   it off) whose title is the month's spend and whose panel says where it
//!   is going and what is open;
//! - **a login item** ([`login_item`]) so all of that is running after a
//!   restart without anyone opening the app.
//!
//! On Linux there is no icon (see `Cargo.toml`), so there is nothing to
//! hold a windowless app: it quits with its window as it always did, and the
//! schedule and notifications run while that window is open.
//!
//! The state the menu bar shows is one entity, [`Status`]; the schedule
//! writes it and the icon and panel observe it.

pub mod backoff;
pub mod login_item;
pub mod notify;
pub mod schedule;
pub mod summary;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod panel;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod tray;

use chrono::{DateTime, Utc};
use gpui_kit::*;

use notify::Announcer;
use summary::Summary;

/// The argument a login launch passes: start in the menu bar, with no
/// window.
pub const BACKGROUND_ARG: &str = "--background";

/// The identifier the macOS bundle is signed under (scripts/package-macos.sh),
/// reused as the login item's name on every platform.
pub const BUNDLE_ID: &str = "io.github.jetsquirrel.cloudbridge";

/// Whether this build keeps running with no window open: only where there
/// is a menu bar icon to come back through and quit from.
pub const RUNS_WINDOWLESS: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Whether the app keeps running in the menu bar once its window closes:
/// this build has a menu bar icon, and Settings has not turned it off.
pub fn menu_bar_enabled() -> bool {
    RUNS_WINDOWLESS
        && crate::config::load_config()
            .map(|config| config.keep_in_menu_bar)
            .unwrap_or(true)
}

/// Whether this process was started by the login item and should stay
/// windowless. With the menu bar turned off a login launch opens the
/// window instead, since there would be nothing else to show it was
/// running.
pub fn launched_in_background() -> bool {
    menu_bar_enabled() && std::env::args().any(|arg| arg == BACKGROUND_ARG)
}

/// What the menu bar shows, and the schedule's own bookkeeping.
pub struct Status {
    /// The last summary loaded; `None` until the first lands.
    pub summary: Option<Summary>,
    /// A refresh is running.
    pub refreshing: bool,
    /// When the last refresh finished.
    pub last_run: Option<DateTime<Utc>>,
    /// The accounts that are failing to refresh, as `name: reason` —
    /// including those the schedule is backing off from.
    pub failures: Vec<String>,
    announcer: Announcer,
    backoff: backoff::Backoff,
}

/// The [`Status`] entity, for anything that needs to read or refresh it.
pub struct GlobalStatus(pub Entity<Status>);

impl Global for GlobalStatus {}

/// Start the schedule, the notifications and, where there is one, the menu
/// bar icon. Call once, after the stores have opened. `started_at` is when
/// the process started: alerts from before it are not announced.
pub fn start(started_at: DateTime<Utc>, cx: &mut App) {
    notify::init();
    let status = cx.new(|_| Status {
        summary: None,
        refreshing: false,
        last_run: None,
        failures: Vec::new(),
        announcer: Announcer::new(started_at),
        backoff: backoff::Backoff::default(),
    });
    cx.set_global(GlobalStatus(status.clone()));

    set_menu_bar(menu_bar_enabled(), cx);

    schedule::start(status, cx);
}

/// Put the menu bar icon up or take it down, and with it whether closing
/// the window quits: without an icon there is no way back to a closed
/// window, nor a Quit to reach, so the app quits with its window.
pub fn set_menu_bar(enabled: bool, cx: &mut App) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let installed = match cx.try_global::<GlobalStatus>().map(|g| g.0.clone()) {
        Some(status) if enabled => tray::install(status, cx),
        _ => {
            tray::uninstall(cx);
            false
        }
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let installed = {
        let _ = enabled;
        false
    };
    cx.set_quit_mode(if installed {
        QuitMode::Explicit
    } else {
        QuitMode::LastWindowClosed
    });
}

/// Whether the menu bar icon is up right now.
pub fn in_menu_bar(cx: &App) -> bool {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return cx.has_global::<tray::MenuBar>();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = cx;
        false
    }
}

/// Fetch what is due now rather than at the next tick — the panel's
/// Refresh. A no-op before [`start`] or while a refresh is running.
pub fn refresh_now(cx: &mut App) {
    if let Some(status) = cx.try_global::<GlobalStatus>().map(|g| g.0.clone()) {
        schedule::run_once(status, true, true, cx);
    }
}
