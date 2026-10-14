//! Backing off from accounts whose refresh keeps failing.
//!
//! The schedule wakes every [`TICK`](super::schedule::TICK). Without this,
//! an account whose fetch fails — a revoked key, a throttled API, a
//! provider outage — would be tried again every tick, around the clock, for
//! as long as the app runs. Each failure doubles the wait before the next
//! try, up to the refresh interval, and a success clears it. A manual
//! Refresh does not wait: someone pressed it.
//!
//! A failure the provider classed as [`Denied`](FailureKind::Denied) — a
//! revoked key, a missing permission — does not double from a tick: asking
//! again changes nothing, so it waits the whole interval. Any wait ends
//! early when the account changes under it: refreshed from the window, or
//! given new credentials, since either may mean the fault is gone.
//!
//! A fetch the provider answered and the ingest then lost is bounded by the
//! ingest itself (see `ingest::PAID_FETCHES`); this bounds everything else.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use super::schedule::TICK;
use crate::cloud::http::FailureKind;
use crate::cloud::CloudAccount;

/// The accounts that are failing, by account id.
#[derive(Default)]
pub struct Backoff {
    failing: HashMap<String, Streak>,
}

struct Streak {
    name: String,
    failures: u32,
    hold: Hold,
    last_error: String,
}

/// One failed refresh of an account.
pub struct Failure {
    pub account_id: String,
    pub name: String,
    /// The account's credential hint when it failed, to notice new ones.
    pub credentials: Option<String>,
    pub kind: Option<FailureKind>,
    pub message: String,
}

/// What a failing account waits out before it is tried again.
#[derive(Clone)]
pub struct Hold {
    failed_at: DateTime<Utc>,
    next_try: DateTime<Utc>,
    credentials: Option<String>,
}

impl Hold {
    /// Whether `account` is still waiting at `now`. Not once it has synced
    /// since it failed, or holds different credentials: what failed may
    /// not fail any more.
    pub fn applies_to(&self, account: &CloudAccount, now: DateTime<Utc>) -> bool {
        now < self.next_try
            && !account
                .last_synced_at
                .is_some_and(|synced| synced > self.failed_at)
            && account.access_key_hint == self.credentials
    }
}

impl Backoff {
    /// Record a failed refresh. Returns whether it starts a streak — the
    /// one failure worth telling someone about; the rest are the same news.
    pub fn failed(&mut self, failure: Failure, now: DateTime<Utc>, cap: Duration) -> bool {
        let streak = self
            .failing
            .entry(failure.account_id)
            .or_insert_with(|| Streak {
                name: failure.name,
                failures: 0,
                hold: Hold {
                    failed_at: now,
                    next_try: now,
                    credentials: None,
                },
                last_error: String::new(),
            });
        streak.failures += 1;
        let wait = match failure.kind {
            Some(FailureKind::Denied) => cap,
            _ => delay(streak.failures, cap),
        };
        streak.hold = Hold {
            failed_at: now,
            next_try: now + wait,
            credentials: failure.credentials,
        };
        streak.last_error = failure.message;
        streak.failures == 1
    }

    /// Each failing account's hold, for the schedule to check against the
    /// accounts as they are when it runs.
    pub fn holds(&self) -> HashMap<String, Hold> {
        self.failing
            .iter()
            .map(|(id, streak)| (id.clone(), streak.hold.clone()))
            .collect()
    }

    pub fn succeeded(&mut self, account_id: &str) {
        self.failing.remove(account_id);
    }

    /// Every failing account as `name: reason`, for the panel — including
    /// the ones waiting, which are still failing even though this pass did
    /// not try them.
    pub fn failures(&self) -> Vec<String> {
        let mut failures: Vec<String> = self
            .failing
            .values()
            .map(|streak| format!("{}: {}", streak.name, streak.last_error))
            .collect();
        failures.sort();
        failures
    }
}

/// The wait after the `failures`-th failure in a row: one tick, then
/// doubling, never longer than `cap`.
fn delay(failures: u32, cap: Duration) -> Duration {
    let tick = Duration::from_std(TICK).expect("a tick fits in a chrono Duration");
    let doublings = failures.saturating_sub(1).min(16);
    (tick * 2_i32.pow(doublings)).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "AKIA…WXYZ";

    fn day() -> Duration {
        Duration::hours(24)
    }

    /// An account that has never synced, holding `credentials`.
    fn prod(credentials: &str) -> CloudAccount {
        let mut account = crate::demo_data::account("aws", "a", "Prod", Utc::now());
        account.last_synced_at = None;
        account.access_key_hint = Some(credentials.to_string());
        account
    }

    fn failure(kind: Option<FailureKind>, message: &str) -> Failure {
        Failure {
            account_id: "a".to_string(),
            name: "Prod".to_string(),
            credentials: Some(KEY.to_string()),
            kind,
            message: message.to_string(),
        }
    }

    fn is_waiting(backoff: &Backoff, account: &CloudAccount, now: DateTime<Utc>) -> bool {
        backoff
            .holds()
            .get(&account.id)
            .is_some_and(|hold| hold.applies_to(account, now))
    }

    #[test]
    fn the_wait_doubles_up_to_the_refresh_interval() {
        let waits: Vec<i64> = (1..=9).map(|n| delay(n, day()).num_minutes()).collect();
        assert_eq!(waits, [15, 30, 60, 120, 240, 480, 960, 1440, 1440]);
    }

    /// A fault that never clears costs a handful of tries a day, not the
    /// 96 a tick every 15 minutes would make.
    #[test]
    fn a_day_of_failures_is_a_handful_of_tries() {
        let account = prod(KEY);
        let start = Utc::now();
        let mut backoff = Backoff::default();
        let mut tries = 0;
        let mut now = start;
        while now < start + day() {
            if !is_waiting(&backoff, &account, now) {
                tries += 1;
                let throttled = failure(Some(FailureKind::Temporary), "throttled");
                backoff.failed(throttled, now, day());
            }
            now += Duration::from_std(TICK).unwrap();
        }
        assert!(tries <= 8, "{tries} tries in a day");
    }

    /// Asking again with credentials the provider refused changes nothing.
    #[test]
    fn a_refusal_waits_the_whole_interval() {
        let account = prod(KEY);
        let now = Utc::now();
        let mut backoff = Backoff::default();
        backoff.failed(failure(Some(FailureKind::Denied), "denied"), now, day());
        assert!(is_waiting(&backoff, &account, now + Duration::hours(23)));
        assert!(!is_waiting(&backoff, &account, now + day()));
    }

    #[test]
    fn new_credentials_end_the_wait() {
        let now = Utc::now();
        let mut backoff = Backoff::default();
        backoff.failed(failure(Some(FailureKind::Denied), "denied"), now, day());
        let rotated = prod("AKIA…NEW1");
        assert!(!is_waiting(&backoff, &rotated, now + Duration::minutes(5)));
    }

    /// Fixed and refreshed from the window, it is watched again at once.
    #[test]
    fn a_sync_since_the_failure_ends_the_wait() {
        let now = Utc::now();
        let mut backoff = Backoff::default();
        backoff.failed(failure(Some(FailureKind::Denied), "denied"), now, day());
        let mut account = prod(KEY);
        account.last_synced_at = Some(now - Duration::hours(1));
        assert!(is_waiting(&backoff, &account, now + Duration::minutes(5)));
        account.last_synced_at = Some(now + Duration::minutes(1));
        assert!(!is_waiting(&backoff, &account, now + Duration::minutes(5)));
    }

    #[test]
    fn only_the_first_failure_of_a_streak_is_news() {
        let account = prod(KEY);
        let now = Utc::now();
        let mut backoff = Backoff::default();
        assert!(backoff.failed(failure(None, "denied"), now, day()));
        assert!(!backoff.failed(failure(None, "denied"), now, day()));
        backoff.succeeded("a");
        assert!(!is_waiting(&backoff, &account, now));
        assert!(backoff.failed(failure(None, "denied"), now, day()));
    }

    #[test]
    fn a_waiting_account_still_reads_as_failing() {
        let account = prod(KEY);
        let now = Utc::now();
        let mut backoff = Backoff::default();
        backoff.failed(failure(None, "denied"), now, day());
        assert!(is_waiting(&backoff, &account, now + Duration::minutes(5)));
        assert_eq!(backoff.failures(), ["Prod: denied"]);
    }
}
