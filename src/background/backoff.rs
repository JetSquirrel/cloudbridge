//! Backing off from accounts whose refresh keeps failing.
//!
//! The schedule wakes every [`TICK`](super::schedule::TICK). Without this,
//! an account whose fetch fails — a revoked key, a throttled API, a
//! provider outage — would be tried again every tick, around the clock, for
//! as long as the app runs. Each failure doubles the wait before the next
//! try, up to the refresh interval, and a success clears it. A manual
//! Refresh does not wait: someone pressed it.
//!
//! A fetch the provider answered and the ingest then lost is bounded by the
//! ingest itself (see `ingest::PAID_FETCHES`); this bounds everything else.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use super::schedule::TICK;

/// The accounts that are failing, by account id.
#[derive(Default)]
pub struct Backoff {
    failing: HashMap<String, Streak>,
}

struct Streak {
    name: String,
    failures: u32,
    next_try: DateTime<Utc>,
    last_error: String,
}

impl Backoff {
    /// Whether an account is waiting out its backoff at `now`.
    pub fn is_waiting(&self, account_id: &str, now: DateTime<Utc>) -> bool {
        self.failing
            .get(account_id)
            .is_some_and(|streak| now < streak.next_try)
    }

    /// Record a failed refresh. Returns whether it starts a streak — the
    /// one failure worth telling someone about; the rest are the same news.
    pub fn failed(
        &mut self,
        account_id: &str,
        name: &str,
        error: String,
        now: DateTime<Utc>,
        cap: Duration,
    ) -> bool {
        let streak = self
            .failing
            .entry(account_id.to_string())
            .or_insert_with(|| Streak {
                name: name.to_string(),
                failures: 0,
                next_try: now,
                last_error: String::new(),
            });
        streak.failures += 1;
        streak.next_try = now + delay(streak.failures, cap);
        streak.last_error = error;
        streak.failures == 1
    }

    /// The accounts waiting out their backoff at `now`.
    pub fn waiting(&self, now: DateTime<Utc>) -> std::collections::HashSet<String> {
        self.failing
            .iter()
            .filter(|(_, streak)| now < streak.next_try)
            .map(|(id, _)| id.clone())
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

    fn day() -> Duration {
        Duration::hours(24)
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
        let start = Utc::now();
        let mut backoff = Backoff::default();
        let mut tries = 0;
        let mut now = start;
        while now < start + day() {
            if !backoff.is_waiting("a", now) {
                tries += 1;
                backoff.failed("a", "Prod", "throttled".to_string(), now, day());
            }
            now += Duration::from_std(TICK).unwrap();
        }
        assert!(tries <= 8, "{tries} tries in a day");
    }

    #[test]
    fn only_the_first_failure_of_a_streak_is_news() {
        let now = Utc::now();
        let mut backoff = Backoff::default();
        assert!(backoff.failed("a", "Prod", "denied".to_string(), now, day()));
        assert!(!backoff.failed("a", "Prod", "denied".to_string(), now, day()));
        backoff.succeeded("a");
        assert!(!backoff.is_waiting("a", now));
        assert!(backoff.failed("a", "Prod", "denied".to_string(), now, day()));
    }

    #[test]
    fn a_waiting_account_still_reads_as_failing() {
        let now = Utc::now();
        let mut backoff = Backoff::default();
        backoff.failed("a", "Prod", "denied".to_string(), now, day());
        assert!(backoff.is_waiting("a", now + Duration::minutes(5)));
        assert_eq!(backoff.failures(), ["Prod: denied"]);
    }
}
