//! System notifications for alerts that fire while the app runs.
//!
//! A notification is how an alert reaches someone who is not looking at
//! CloudBridge — which, with the window closed, is everyone. Each alert is
//! announced once per run: the ones already open when the app started were
//! either announced last time or are on the Alerts page, and repeating them
//! at every login would teach people to ignore the notifications.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use super::summary::AlertLine;
use crate::alerts::Severity;

/// Which alerts have been announced, and since when announcing started.
pub struct Announcer {
    started_at: DateTime<Utc>,
    announced: HashSet<String>,
}

impl Announcer {
    /// Announce only what fires from `started_at` on.
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            announced: HashSet::new(),
        }
    }

    /// The open alerts not announced yet, each marked as announced.
    pub fn take_new<'a>(&mut self, open: &'a [AlertLine]) -> Vec<&'a AlertLine> {
        open.iter()
            .filter(|alert| alert.created_at >= self.started_at)
            .filter(|alert| self.announced.insert(alert.id.clone()))
            .collect()
    }
}

/// Post one alert as a system notification. A failure is logged, never
/// raised: a notification centre that refuses is no reason to stop
/// refreshing.
pub fn post(alert: &AlertLine) {
    let summary = match alert.severity {
        Severity::Critical => "CloudBridge — critical alert",
        Severity::Warning => "CloudBridge alert",
    };
    let result = notify_rust::Notification::new()
        .appname("CloudBridge")
        .summary(summary)
        .body(&alert.title)
        .show();
    if let Err(e) = result {
        tracing::warn!("Could not post a notification for {}: {}", alert.id, e);
    }
}

/// Tell someone an account has stopped refreshing — which means its spend
/// has stopped being watched. Posted once per run of failures; see
/// `backoff`.
pub fn post_refresh_failure(account: &str, error: &str) {
    let result = notify_rust::Notification::new()
        .appname("CloudBridge")
        .summary(&format!("CloudBridge could not refresh {account}"))
        .body(error)
        .show();
    if let Err(e) = result {
        tracing::warn!("Could not post a notification for {}: {}", account, e);
    }
}

/// Tell the notification centre which app is speaking, so the banner
/// carries CloudBridge's name and icon. Only a bundled build has an
/// identity to give; an unbundled one keeps the default.
pub fn init() {
    #[cfg(target_os = "macos")]
    {
        let bundled = std::env::current_exe()
            .map(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
            .unwrap_or(false);
        if bundled {
            if let Err(e) = notify_rust::set_application(super::BUNDLE_ID) {
                tracing::warn!("Could not name the app to the notification centre: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn alert(id: &str, created_at: DateTime<Utc>) -> AlertLine {
        AlertLine {
            id: id.to_string(),
            severity: Severity::Critical,
            title: "Durable Objects cost 12× its baseline".to_string(),
            created_at,
        }
    }

    #[test]
    fn an_alert_is_announced_once() {
        let start = Utc::now();
        let mut announcer = Announcer::new(start);
        let open = vec![alert("a", start + Duration::minutes(5))];

        assert_eq!(announcer.take_new(&open).len(), 1);
        assert!(announcer.take_new(&open).is_empty());
    }

    #[test]
    fn what_was_open_before_the_run_is_not_announced_again() {
        let start = Utc::now();
        let mut announcer = Announcer::new(start);
        let open = vec![
            alert("yesterday", start - Duration::days(1)),
            alert("now", start + Duration::seconds(1)),
        ];

        let ids: Vec<&str> = announcer
            .take_new(&open)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ids, vec!["now"]);
    }

    /// The first evaluation of a run happens as the app starts; what it
    /// fires is news, and must not be lost to a clock read a moment late.
    #[test]
    fn an_alert_fired_at_the_start_of_the_run_is_announced() {
        let start = Utc::now();
        let mut announcer = Announcer::new(start);
        assert_eq!(announcer.take_new(&[alert("boot", start)]).len(), 1);
    }
}
