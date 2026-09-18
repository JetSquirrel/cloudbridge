//! The demo bill, as rows.
//!
//! The Settings page can fill the ledger with a realistic-shaped fake bill so
//! design and layout review runs against a full app instead of an empty
//! shell; in the browser that bill *is* the ledger. Either way the rows are
//! the same rows — same accounts, same services, same spike, same jitter — so
//! a page reviewed in one place shows the numbers it will show in the other.
//!
//! What differs is only where they are written: twelve transactions per
//! account against DuckDB on the desktop, a push onto a vector in the
//! browser. That belongs to each backend's `ledger::demo`, which is what
//! calls in here.
//!
//! Everything demo is keyed under the [`DEMO_PREFIX`] — batch ids and account
//! ids — so clearing is a prefix delete and re-seeding first replaces every
//! period wholesale, making the result deterministic. Demo accounts hold no
//! credentials and are skipped by refresh, so no demo row ever reaches a real
//! API.

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{BalanceSnapshot, BillingPeriod, Charge, ChargeCategory, CloudAccount};

/// Account-id prefix marking every demo row; also the marker `ingest` uses to
/// keep demo accounts away from the real billing APIs.
pub const DEMO_PREFIX: &str = "demo-";

/// (provider id, demo account id, display name, billing currency).
pub const DEMO_SOURCES: &[(&str, &str, &str, &str)] = &[
    ("AWS", "demo-aws", "Demo AWS", "USD"),
    ("Aliyun", "demo-aliyun", "Demo Aliyun", "CNY"),
    ("DeepSeek", "demo-deepseek", "Demo DeepSeek", "CNY"),
];

/// (service, base monthly amount in the source's currency, business line).
/// `None` leaves the charge untagged, which is what feeds the Unallocated
/// card and the untagged-ratio rule.
const AWS_SERVICES: &[(&str, f64, Option<&str>)] = &[
    ("EC2", 380.0, Some("Platform")),
    ("RDS", 165.0, Some("Platform")),
    ("Bedrock", 240.0, Some("Inference")),
    ("S3", 88.0, Some("Inference")),
    ("Lambda", 42.0, Some("Search")),
    ("Data Transfer", 54.0, None),
    ("CloudWatch", 26.0, None),
];

const ALIYUN_SERVICES: &[(&str, f64, Option<&str>)] = &[
    ("ECS", 1150.0, Some("Search")),
    ("OSS", 320.0, Some("Search")),
    ("RDS", 480.0, Some("Platform")),
    ("CDN", 210.0, Some("Growth")),
    ("SLB", 140.0, None),
];

const DEEPSEEK_SERVICES: &[(&str, f64, Option<&str>)] = &[
    ("deepseek-chat", 260.0, Some("Inference")),
    ("deepseek-reasoner", 480.0, Some("Inference")),
];

/// How many months of history the demo ledger carries.
pub const DEMO_MONTHS: usize = 12;
/// The month index (0 = oldest) whose model spend spikes, so the trend chart
/// and the movers table have something to say.
const SPIKE_MONTH: usize = 8;

/// The periods the demo covers, oldest first, ending with the one `now` falls
/// in.
pub fn periods(now: DateTime<Utc>) -> Vec<BillingPeriod> {
    let mut periods = Vec::with_capacity(DEMO_MONTHS);
    let mut period = BillingPeriod::containing(now);
    for _ in 0..DEMO_MONTHS {
        periods.push(period);
        period = period.previous();
    }
    periods.reverse();

    periods
}

/// The service table one demo provider bills from.
pub fn services_of(provider: &str) -> &'static [(&'static str, f64, Option<&'static str>)] {
    match provider {
        "AWS" => AWS_SERVICES,
        "Aliyun" => ALIYUN_SERVICES,
        _ => DEEPSEEK_SERVICES,
    }
}

/// One demo account, as [`DEMO_SOURCES`] describes it.
///
/// It carries no credentials: a demo account never signs a request, so
/// nothing reaches the OS keyring on the desktop and the browser needs no
/// keyring to begin with.
pub fn account(provider: &str, account_id: &str, name: &str, now: DateTime<Utc>) -> CloudAccount {
    CloudAccount {
        id: account_id.to_string(),
        name: name.to_string(),
        source_id: provider.into(),
        region: None,
        created_at: now,
        last_synced_at: Some(now),
        enabled: true,
        // `save_account` derives the hint from the key it is given; with no
        // key there is none to show.
        access_key_hint: None,
        export_uri: None,
    }
}

/// One period's charges for one source: a monthly row per service for settled
/// periods, daily rows for the current and previous period so the 30-day and
/// MTD charts have points to draw.
pub fn period_charges(
    provider: &str,
    services: &[(&str, f64, Option<&str>)],
    currency: &str,
    period: BillingPeriod,
    index: usize,
    now: DateTime<Utc>,
) -> Vec<Charge> {
    let current = BillingPeriod::containing(now);
    let recent = period.label() == current.label() || period.label() == current.previous().label();

    // Growth over the year, then a spike month for the model services.
    let growth = 0.62 + 0.08 * index as f64;
    let mut charges = Vec::new();

    for (service, base, line) in services {
        let mut amount = base * growth;
        if index == SPIKE_MONTH && matches!(*service, "Bedrock" | "deepseek-reasoner") {
            amount *= 2.6;
        }

        if recent {
            let mut day = period.start();
            while day < period.end_exclusive() {
                let start = day_start(day);
                if start > now {
                    break;
                }
                let mut daily = amount / 30.0 * (1.0 + jitter(service, day));
                // Three consecutive hot days right before now, so the
                // cost-anomaly rule (daily > 7-day baseline × 2.5) fires
                // against the demo data.
                if *service == "deepseek-reasoner" && (now - start).num_days() < 3 {
                    daily *= 3.4;
                }
                let Some(next) = day.succ_opt() else { break };
                charges.push(usage_charge(
                    provider,
                    service,
                    *line,
                    currency,
                    daily,
                    start,
                    day_start(next),
                ));
                day = next;
            }
        } else {
            charges.push(usage_charge(
                provider,
                service,
                *line,
                currency,
                amount,
                day_start(period.start()),
                day_start(period.end_exclusive()),
            ));
        }
    }

    // A monthly credit, so the usage/credits split has something to show.
    if provider == "AWS" {
        let total: f64 = AWS_SERVICES.iter().map(|(_, base, _)| base * growth).sum();
        let mut credit = Charge::new(
            day_start(period.start()),
            day_start(period.end_exclusive()),
            currency,
        );
        credit.charge_category = ChargeCategory::Credit;
        credit.charge_description = Some("Promotional credit".to_string());
        credit.billed_cost = Some(-(total * 0.08));
        credit.effective_cost = Some(-(total * 0.08));
        charges.push(credit);
    }

    charges
}

/// The demo balance history, one observation per period.
///
/// DeepSeek reports a balance rather than charges; the ladder falls with use
/// and tops up when it runs low, so the balance-floor rule has input.
pub fn balance_ladder(periods: &[BillingPeriod]) -> Vec<BalanceSnapshot> {
    let mut balance = 800.0_f64;

    periods
        .iter()
        .map(|period| {
            balance = (balance - 620.0).max(0.0) + if balance < 300.0 { 500.0 } else { 0.0 };
            BalanceSnapshot {
                provider: "DeepSeek".to_string(),
                account_id: "demo-deepseek".to_string(),
                observed_at: day_start(period.start()),
                balance,
                granted_balance: None,
                topped_up_balance: Some(balance),
                currency: "CNY".to_string(),
            }
        })
        .collect()
}

/// The batch id a demo period is written under.
pub fn batch_id(provider: &str, period: BillingPeriod) -> String {
    format!("demo-{provider}-{}", period.label())
}

/// The one-line summary a seed reports to the UI.
pub fn summary(periods: usize, charges: usize) -> String {
    format!(
        "{} accounts, {} periods, {} charges",
        DEMO_SOURCES.len(),
        DEMO_SOURCES.len() * periods,
        charges
    )
}

/// A usage charge for one service and period slice; both cost columns set, so
/// gross and effective views agree.
fn usage_charge(
    provider: &str,
    service: &str,
    line: Option<&str>,
    currency: &str,
    amount: f64,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Charge {
    let mut charge = Charge::new(start, end, currency);
    charge.service_name = Some(service.to_string());
    charge.charge_description = Some(format!("{provider} {service} usage"));
    charge.billed_cost = Some(amount);
    charge.effective_cost = Some(amount);
    charge.tags = line.map(|line| format!("{{\"business_line\":\"{line}\"}}"));
    charge
}

/// Midnight UTC on `date`.
pub fn day_start(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// Deterministic per-(service, day) wobble in ±12%, so charts look real but
/// re-seeding changes nothing.
fn jitter(service: &str, day: NaiveDate) -> f64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (service, day).hash(&mut hasher);
    (hasher.finish() % 1000) as f64 / 1000.0 * 0.24 - 0.12
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(2026, 9, 18)
            .expect("a real date")
            .and_hms_opt(12, 0, 0)
            .expect("midday exists")
            .and_utc()
    }

    #[test]
    fn the_demo_covers_a_year_ending_in_the_current_period() {
        let periods = periods(now());

        assert_eq!(periods.len(), DEMO_MONTHS);
        assert_eq!(periods.last().unwrap().label(), "2026-09");
        assert_eq!(periods.first().unwrap().label(), "2025-10");
    }

    #[test]
    fn a_settled_period_bills_monthly_and_a_recent_one_daily() {
        let periods = periods(now());

        // A period well in the past: one row per service, plus the credit.
        let settled = period_charges("AWS", services_of("AWS"), "USD", periods[0], 0, now());
        assert_eq!(settled.len(), AWS_SERVICES.len() + 1);

        // The current period: a row per service per elapsed day, so the
        // 30-day and MTD charts have points.
        let current = period_charges("AWS", services_of("AWS"), "USD", periods[11], 11, now());
        assert!(current.len() > AWS_SERVICES.len() * 17);
        // Nothing is dated after `now`.
        assert!(current
            .iter()
            .all(|charge| charge.charge_period_start <= now()));
    }

    #[test]
    fn the_rows_are_the_same_rows_on_every_seed() {
        let first = period_charges(
            "Aliyun",
            services_of("Aliyun"),
            "CNY",
            periods(now())[11],
            11,
            now(),
        );
        let second = period_charges(
            "Aliyun",
            services_of("Aliyun"),
            "CNY",
            periods(now())[11],
            11,
            now(),
        );

        let amounts = |charges: &[Charge]| {
            charges
                .iter()
                .map(|charge| charge.billed_cost)
                .collect::<Vec<_>>()
        };
        assert_eq!(amounts(&first), amounts(&second));
    }

    #[test]
    fn the_spike_month_doubles_the_model_services_and_leaves_the_rest() {
        let periods = periods(now());
        let plain = period_charges("AWS", services_of("AWS"), "USD", periods[7], 7, now());
        let spiked = period_charges("AWS", services_of("AWS"), "USD", periods[8], 8, now());

        let of = |charges: &[Charge], service: &str| {
            charges
                .iter()
                .find(|charge| charge.service_name.as_deref() == Some(service))
                .and_then(|charge| charge.billed_cost)
                .expect("the service is billed")
        };

        // Bedrock jumps by more than the month's own growth; EC2 does not.
        assert!(of(&spiked, "Bedrock") > of(&plain, "Bedrock") * 2.0);
        assert!(of(&spiked, "EC2") < of(&plain, "EC2") * 1.2);
    }

    #[test]
    fn the_balance_ladder_falls_and_tops_up_before_it_empties() {
        let ladder = balance_ladder(&periods(now()));

        assert_eq!(ladder.len(), DEMO_MONTHS);
        assert!(ladder.iter().all(|snapshot| snapshot.balance >= 0.0));
        // It never flatlines at zero: a top-up lands whenever it runs low.
        assert!(ladder.iter().any(|snapshot| snapshot.balance > 300.0));
        assert!(ladder
            .windows(2)
            .any(|pair| pair[1].balance < pair[0].balance));
    }

    #[test]
    fn every_demo_id_carries_the_prefix_that_clears_it() {
        assert!(DEMO_SOURCES
            .iter()
            .all(|(_, account_id, _, _)| account_id.starts_with(DEMO_PREFIX)));
        assert!(batch_id("AWS", periods(now())[0]).starts_with(DEMO_PREFIX));
    }
}
