//! Keeping CloudBridge up to date.
//!
//! Once a day while the app runs — window open or not — it asks GitHub for
//! the newest release ([`release`]). When that is newer and this build can
//! replace itself, the update is downloaded and checked in the background
//! ([`install::stage`]), and installed the next time the app quits or
//! someone restarts it from Settings ([`install::apply`]). Nothing is
//! installed under a running app, and nothing restarts on its own.
//!
//! "Update automatically" in Settings turns off all of it: with it off, the
//! app asks GitHub nothing until someone presses Check now. A debug build
//! never checks on its own, and only a release build run from where it was
//! installed replaces itself; any other build is told about a new version
//! and sent to the release page.
//!
//! The state Settings shows is one entity, [`Updater`].

pub mod install;
pub mod release;

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui_kit::*;

pub use install::Staged;
pub use release::{release_page, Release, CURRENT};

/// How often a check is due.
const CHECK_INTERVAL: chrono::TimeDelta = chrono::TimeDelta::hours(24);
/// The first check waits this long after launch, out of the way of the
/// stores opening and the first page load.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
/// How often the schedule looks at whether a check is due. Wall-clock time
/// decides, so a Mac that slept through the day checks soon after waking.
const TICK: Duration = Duration::from_secs(60 * 60);

/// Where updating stands.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    /// Not checked since the app started.
    Idle,
    Checking,
    UpToDate,
    /// A newer release, not downloaded: updating automatically is off, or
    /// this build cannot replace itself.
    Available(Release),
    Downloading(Release),
    /// Downloaded and checked; installs at the next quit or restart.
    Ready(Staged),
    /// Being moved into place for a restart.
    Installing(Staged),
    /// The last check or download failed. `release` is what was being
    /// downloaded, if anything, for its page.
    Failed {
        error: String,
        release: Option<Release>,
    },
}

/// What Settings shows about updating, and the schedule's bookkeeping.
pub struct Updater {
    pub state: UpdateState,
    /// When a check last got an answer.
    pub last_checked: Option<DateTime<Utc>>,
    /// What an update replaces; `None` for a build that cannot replace
    /// itself ([`install::install_target`]).
    target: Option<PathBuf>,
}

impl Updater {
    /// Whether this build can download and install an update itself.
    pub fn can_install(&self) -> bool {
        self.target.is_some()
    }

    fn busy(&self) -> bool {
        matches!(
            self.state,
            UpdateState::Checking | UpdateState::Downloading(_) | UpdateState::Installing(_)
        )
    }
}

/// The [`Updater`] entity.
pub struct GlobalUpdater(pub Entity<Updater>);

impl Global for GlobalUpdater {}

fn updater(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<GlobalUpdater>().map(|g| g.0.clone())
}

/// Whether to check and download without being asked.
fn automatic() -> bool {
    !cfg!(debug_assertions)
        && crate::config::load_config()
            .map(|config| config.auto_update)
            .unwrap_or(true)
}

/// Start the schedule, pick up an update an earlier run downloaded, and
/// install whatever is staged when the app quits. Call once, at launch.
pub fn start(cx: &mut App) {
    let target = install::install_target();
    let updater = cx.new(|_| Updater {
        state: UpdateState::Idle,
        last_checked: None,
        target: target.clone(),
    });
    cx.set_global(GlobalUpdater(updater.clone()));

    // Install at quit. Called before the process exits, with no way to
    // cancel; the renames are quick enough not to hold it up.
    cx.on_app_quit({
        let updater = updater.clone();
        move |cx| {
            let updater = updater.read(cx);
            if let (UpdateState::Ready(staged), Some(target)) = (&updater.state, &updater.target) {
                if let Err(e) = install::apply(staged, target) {
                    tracing::error!("Could not install CloudBridge {}: {e:#}", staged.version);
                }
            }
            async {}
        }
    })
    .detach();

    cx.spawn(async move |cx| {
        let found = smol::unblock(move || install::tidy(target.as_deref())).await;
        if let Some(staged) = found {
            cx.update(|cx| {
                updater.update(cx, |updater, cx| {
                    if updater.state == UpdateState::Idle {
                        updater.state = UpdateState::Ready(staged);
                        cx.notify();
                    }
                })
            });
        }
        cx.background_executor().timer(FIRST_CHECK_DELAY).await;
        loop {
            cx.update(|cx| {
                let due = {
                    let updater = updater.read(cx);
                    !updater.busy()
                        && !matches!(updater.state, UpdateState::Ready(_))
                        && updater
                            .last_checked
                            .is_none_or(|at| Utc::now() - at >= CHECK_INTERVAL)
                };
                if due && automatic() {
                    check(false, cx);
                }
            });
            cx.background_executor().timer(TICK).await;
        }
    })
    .detach();
}

/// Ask GitHub now — Settings' Check now. A no-op while a check, download
/// or install is running.
pub fn check_now(cx: &mut App) {
    check(true, cx);
}

/// Check, and when updating automatically, download what is found. A
/// `manual` check reports a failure; an automatic one keeps what was shown
/// and tries again at the next tick.
fn check(manual: bool, cx: &mut App) {
    let Some(updater) = updater(cx) else {
        return;
    };
    if updater.read(cx).busy() {
        return;
    }
    let previous = updater.update(cx, |updater, cx| {
        cx.notify();
        std::mem::replace(&mut updater.state, UpdateState::Checking)
    });

    cx.spawn(async move |cx| {
        let result = smol::unblock(release::fetch_latest).await;
        cx.update(|cx| {
            let download_now = updater.update(cx, |updater, cx| {
                cx.notify();
                let release = match result {
                    Ok(release) => release,
                    Err(e) => {
                        tracing::warn!("Update check failed: {e:#}");
                        updater.state = match previous {
                            UpdateState::Ready(_) => previous,
                            _ if manual => UpdateState::Failed {
                                error: format!("{e:#}"),
                                release: None,
                            },
                            UpdateState::Checking => UpdateState::Idle,
                            _ => previous,
                        };
                        return false;
                    }
                };
                updater.last_checked = Some(Utc::now());
                match previous {
                    // What is already downloaded stands unless something
                    // newer came out since.
                    UpdateState::Ready(staged)
                        if !release::is_newer(&release.version, &staged.version) =>
                    {
                        updater.state = UpdateState::Ready(staged);
                        false
                    }
                    _ if release::is_newer(&release.version, CURRENT) => {
                        let installable = updater.can_install() && release.asset.is_some();
                        updater.state = UpdateState::Available(release);
                        installable && automatic()
                    }
                    _ => {
                        updater.state = UpdateState::UpToDate;
                        false
                    }
                }
            });
            if download_now {
                download(true, cx);
            }
        });
    })
    .detach();
}

/// Download and stage the release that is available — Settings' Download
/// and install, or an automatic check that found one.
pub fn download_update(cx: &mut App) {
    download(false, cx);
}

fn download(automatic: bool, cx: &mut App) {
    let Some(updater) = updater(cx) else {
        return;
    };
    let (release, target) = {
        let updater = updater.read(cx);
        let release = match &updater.state {
            UpdateState::Available(release)
            | UpdateState::Failed {
                release: Some(release),
                ..
            } => release.clone(),
            _ => return,
        };
        match &updater.target {
            Some(target) if release.asset.is_some() => (release, target.clone()),
            _ => return,
        }
    };
    updater.update(cx, |updater, cx| {
        updater.state = UpdateState::Downloading(release.clone());
        cx.notify();
    });

    cx.spawn(async move |cx| {
        let staging = release.clone();
        let result = smol::unblock(move || install::stage(&staging, &target)).await;
        cx.update(|cx| {
            updater.update(cx, |updater, cx| {
                updater.state = match result {
                    Ok(staged) => UpdateState::Ready(staged),
                    Err(e) => {
                        tracing::error!(
                            "Could not download CloudBridge {}: {e:#}",
                            release.version
                        );
                        UpdateState::Failed {
                            error: format!("{e:#}"),
                            release: Some(release.clone()),
                        }
                    }
                };
                cx.notify();
            });
            if automatic && matches!(updater.read(cx).state, UpdateState::Ready(_)) {
                announce(&release.version);
            }
        });
    })
    .detach();
}

/// Tell someone an update is waiting, once per version: the app may live in
/// the menu bar for weeks without anyone opening Settings.
fn announce(version: &str) {
    let Ok(mut config) = crate::config::load_config() else {
        return;
    };
    if config.announced_update.as_deref() == Some(version) {
        return;
    }
    crate::background::notify::post_update_ready(version);
    config.announced_update = Some(version.to_string());
    if let Err(e) = crate::config::save_config(&config) {
        tracing::warn!("Could not record the update announcement: {e}");
    }
}

/// Install the staged update and start the new version — Settings' Restart
/// to update.
pub fn restart_to_update(cx: &mut App) {
    let Some(updater) = updater(cx) else {
        return;
    };
    let (staged, target) = {
        let updater = updater.read(cx);
        match (&updater.state, &updater.target) {
            (UpdateState::Ready(staged), Some(target)) => (staged.clone(), target.clone()),
            _ => return,
        }
    };
    updater.update(cx, |updater, cx| {
        updater.state = UpdateState::Installing(staged.clone());
        cx.notify();
    });

    cx.spawn(async move |cx| {
        let installing = staged.clone();
        let restart_at = target.clone();
        let result = smol::unblock(move || install::apply(&installing, &target)).await;
        cx.update(|cx| match result {
            Ok(()) => {
                cx.set_restart_path(restart_at);
                cx.restart();
            }
            Err(e) => {
                tracing::error!("Could not install CloudBridge {}: {e:#}", staged.version);
                updater.update(cx, |updater, cx| {
                    updater.state = UpdateState::Failed {
                        error: format!("{e:#}"),
                        release: Some(Release {
                            version: staged.version.clone(),
                            asset: None,
                        }),
                    };
                    cx.notify();
                });
            }
        });
    })
    .detach();
}
