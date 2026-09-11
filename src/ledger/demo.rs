//! Demo data: a realistic-shaped fake ledger, loaded from the Settings
//! page so design and layout review runs against a full app instead of an
//! empty shell.
//!
//! Everything demo is keyed under the `demo-` prefix — batch ids and
//! account ids — so clearing is a prefix delete and re-seeding first
//! replaces every period wholesale, making the result deterministic.
//! Demo accounts hold no credentials and are skipped by refresh, so no
//! demo row ever reaches a real API.

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};

use super::{record_balance, replace_period, with_connection, BalanceSnapshot, Channel, Charge};
use super::{ChargeCategory, PeriodKey};
use crate::cloud::{BillingPeriod, CloudAccount};
use crate::db;

/// Account-id prefix marking every demo row; also the marker `ingest`
/// uses to keep demo accounts away from the real billing APIs.
pub const DEMO_PREFIX: &str = "demo-";

/// (provider id, demo account id, display name, billing currency).
const DEMO_SOURCES: &[(&str, &str, &str, &str)] = &[
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
const DEMO_MONTHS: usize = 12;
/// The month index (0 = oldest) whose model spend spikes, so the trend
/// chart and the movers table have something to say.
const SPIKE_MONTH: usize = 8;

/// Fill the ledger with the demo accounts, charges, and balances,
/// replacing any demo data already present. Blocking; wrap in
/// `smol::unblock`. Returns a one-line summary for the UI.
pub fn seed_demo() -> Result<String> {
    let now = Utc::now();
    let mut periods = Vec::with_capacity(DEMO_MONTHS);
    let mut period = BillingPeriod::containing(now);
    for _ in 0..DEMO_MONTHS {
        periods.push(period);
        period = period.previous();
    }
    periods.reverse();

    for (provider, account_id, name, _currency) in DEMO_SOURCES {
        db::save_account(
            &CloudAccount {
                id: account_id.to_string(),
                name: name.to_string(),
                source_id: (*provider).into(),
                region: None,
                created_at: now,
                last_synced_at: Some(now),
                enabled: true,
                // `save_account` derives the hint from the key it is
                // given; with no key there is none to show.
                access_key_hint: None,
            },
            // No credentials: demo accounts never sign a request, and an
            // empty key means nothing reaches the OS keyring either.
            "",
            "",
        )?;
    }

    let mut charge_count = 0usize;
    for (provider, account_id, _, currency) in DEMO_SOURCES {
        let services = match *provider {
            "AWS" => AWS_SERVICES,
            "Aliyun" => ALIYUN_SERVICES,
            _ => DEEPSEEK_SERVICES,
        };
        for (index, period) in periods.iter().enumerate() {
            let charges = period_charges(provider, services, currency, *period, index, now);
            charge_count += charges.len();
            replace_period(
                &PeriodKey::new(*provider, *account_id, period.label()),
                &format!("demo-{provider}-{}", period.label()),
                &charges,
                None,
                Channel::Api,
            )?;
        }
    }

    // DeepSeek reports a balance, not charges; give it a falling balance
    // with the occasional top-up so the balance-floor rule has input.
    let mut balance = 800.0_f64;
    for period in &periods {
        balance = (balance - 620.0).max(0.0) + if balance < 300.0 { 500.0 } else { 0.0 };
        record_balance(&BalanceSnapshot {
            provider: "DeepSeek".to_string(),
            account_id: "demo-deepseek".to_string(),
            observed_at: day_start(period.start()),
            balance,
            granted_balance: None,
            topped_up_balance: Some(balance),
            currency: "CNY".to_string(),
        })?;
    }

    for (_, account_id, _, _) in DEMO_SOURCES {
        db::mark_account_synced(account_id, now)?;
    }

    Ok(format!(
        "{} accounts, {} periods, {} charges",
        DEMO_SOURCES.len(),
        DEMO_SOURCES.len() * periods.len(),
        charge_count
    ))
}

/// Remove every demo row and demo account. Blocking. Returns a one-line
/// summary for the UI.
pub fn clear_demo() -> Result<String> {
    let accounts = db::get_all_accounts()?
        .into_iter()
        .filter(|account| account.id.starts_with(DEMO_PREFIX))
        .collect::<Vec<_>>();
    let removed = accounts.len();
    for account in accounts {
        db::delete_account(&account.id)?;
    }

    with_connection(|conn| {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM fct_charge WHERE batch_id LIKE 'demo-%'", [])?;
        tx.execute("DELETE FROM ingest_batch WHERE batch_id LIKE 'demo-%'", [])?;
        tx.execute(
            "DELETE FROM fct_balance_snapshot WHERE account_id LIKE 'demo-%'",
            [],
        )?;
        tx.commit()?;
        Ok(())
    })?;

    Ok(format!("Removed {removed} demo account(s) and their data"))
}

/// One period's charges for one source: a monthly row per service for
/// settled periods, daily rows for the current and previous period so the
/// 30-day and MTD charts have points to draw.
fn period_charges(
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

/// A usage charge for one service and period slice; both cost columns set,
/// so gross and effective views agree.
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
fn day_start(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// Deterministic per-(service, day) wobble in ±12%, so charts look real
/// but re-seeding changes nothing.
fn jitter(service: &str, day: NaiveDate) -> f64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (service, day).hash(&mut hasher);
    (hasher.finish() % 1000) as f64 / 1000.0 * 0.24 - 0.12
}
