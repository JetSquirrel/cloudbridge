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

pub mod inventory;

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{
    BalanceSnapshot, BillingPeriod, Charge, ChargeCategory, CloudAccount, CostBasis,
};

/// Account-id prefix marking every demo row; also the marker `ingest` uses to
/// keep demo accounts away from the real billing APIs.
pub const DEMO_PREFIX: &str = "demo-";

/// (provider id, demo account id, display name, billing currency).
pub const DEMO_SOURCES: &[(&str, &str, &str, &str)] = &[
    ("AWS", "demo-aws", "Demo AWS", "USD"),
    ("Aliyun", "demo-aliyun", "Demo Aliyun", "CNY"),
    ("DeepSeek", "demo-deepseek", "Demo DeepSeek", "CNY"),
    ("OpenAI", "demo-openai", "Demo OpenAI", "USD"),
    ("Anthropic", "demo-anthropic", "Demo Anthropic", "USD"),
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

/// For the model providers the "service" is the model itself; every row it
/// bills also names the model as its `service_category`, the way the OpenAI
/// and Anthropic usage exports report it.
const OPENAI_MODELS: &[(&str, f64, Option<&str>)] = &[
    ("gpt-5", 1800.0, Some("Inference")),
    ("gpt-5-mini", 450.0, Some("Inference")),
];

const ANTHROPIC_MODELS: &[(&str, f64, Option<&str>)] = &[
    ("claude-opus-4-5", 1200.0, Some("Inference")),
    ("claude-sonnet-4-5", 700.0, Some("Inference")),
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
        "OpenAI" => OPENAI_MODELS,
        "Anthropic" => ANTHROPIC_MODELS,
        _ => DEEPSEEK_SERVICES,
    }
}

/// The product code a cloud's own bill files a demo service under, so the
/// demo's rows are placed in FOCUS categories the way a real bill's are.
pub fn service_code(provider: &str, service: &str) -> Option<&'static str> {
    let code = match (provider, service) {
        ("AWS", "EC2") => "AmazonEC2",
        ("AWS", "RDS") => "AmazonRDS",
        ("AWS", "Bedrock") => "AmazonBedrock",
        ("AWS", "S3") => "AmazonS3",
        ("AWS", "Lambda") => "AWSLambda",
        ("AWS", "Data Transfer") => "AWSDataTransfer",
        ("AWS", "CloudWatch") => "AmazonCloudWatch",
        ("Aliyun", "ECS") => "ecs",
        ("Aliyun", "OSS") => "oss",
        ("Aliyun", "RDS") => "rds",
        ("Aliyun", "CDN") => "cdn",
        ("Aliyun", "SLB") => "slb",
        _ => return None,
    };
    Some(code)
}

/// Per-1M-token list prices for one demo model, so the token rows beside its
/// cost imply a blended unit cost in the right neighborhood.
struct ModelPrices {
    input: f64,
    output: f64,
    /// OpenAI's discounted re-read of its own prompt cache.
    cached_input: Option<f64>,
    /// Anthropic's prompt-cache read and write prices.
    cache_read: Option<f64>,
    cache_creation: Option<f64>,
}

/// The prices one demo model bills at, or `None` for a service that is not a
/// token-billed model.
fn model_prices(provider: &str, service: &str) -> Option<ModelPrices> {
    let prices = match (provider, service) {
        ("OpenAI", "gpt-5") => ModelPrices {
            input: 1.25,
            output: 10.0,
            cached_input: Some(0.125),
            cache_read: None,
            cache_creation: None,
        },
        ("OpenAI", "gpt-5-mini") => ModelPrices {
            input: 0.25,
            output: 2.0,
            cached_input: Some(0.025),
            cache_read: None,
            cache_creation: None,
        },
        ("Anthropic", "claude-opus-4-5") => ModelPrices {
            input: 5.0,
            output: 25.0,
            cached_input: None,
            cache_read: Some(0.50),
            cache_creation: Some(6.25),
        },
        ("Anthropic", "claude-sonnet-4-5") => ModelPrices {
            input: 3.0,
            output: 15.0,
            cached_input: None,
            cache_read: Some(0.30),
            cache_creation: Some(3.75),
        },
        _ => return None,
    };
    Some(prices)
}

/// The token rows one slice of model spend implies, as (unit, quantity)
/// pairs. Output runs at ~30% of input and each provider's cache class rides
/// at its usual share, so the quantities priced at [`ModelPrices`] add back
/// up to the amount they sit beside.
fn token_quantities(
    provider: &str,
    prices: &ModelPrices,
    amount: f64,
    price_scale: f64,
) -> Vec<(&'static str, f64)> {
    const OUTPUT_RATIO: f64 = 0.30;

    if provider == "OpenAI" {
        // Cached tokens are a share of the input, billed at the lower rate.
        const CACHED_SHARE: f64 = 0.30;
        let per_input = (1.0 - CACHED_SHARE) * prices.input
            + CACHED_SHARE * prices.cached_input.unwrap_or(prices.input)
            + OUTPUT_RATIO * prices.output;
        let input = amount * 1e6 / (per_input * price_scale);
        vec![
            ("Input Tokens", input),
            ("Cached Input Tokens", input * CACHED_SHARE),
            ("Output Tokens", input * OUTPUT_RATIO),
        ]
    } else {
        // Anthropic reports its cache traffic beside the uncached input.
        const CACHE_READ_RATIO: f64 = 0.50;
        const CACHE_CREATION_RATIO: f64 = 0.10;
        let per_input = prices.input
            + CACHE_READ_RATIO * prices.cache_read.unwrap_or(0.0)
            + CACHE_CREATION_RATIO * prices.cache_creation.unwrap_or(0.0)
            + OUTPUT_RATIO * prices.output;
        let input = amount * 1e6 / (per_input * price_scale);
        vec![
            ("Input Tokens", input),
            ("Output Tokens", input * OUTPUT_RATIO),
            ("Cache Read Tokens", input * CACHE_READ_RATIO),
            ("Cache Creation Tokens", input * CACHE_CREATION_RATIO),
        ]
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
        if index == SPIKE_MONTH
            && matches!(
                *service,
                "Bedrock" | "deepseek-reasoner" | "claude-opus-4-5"
            )
        {
            amount *= 2.6;
        }

        let prices = model_prices(provider, service);
        // Opus's list price drifts up over the year, so the price/volume
        // split has a model whose cost rises faster than its tokens.
        let price_scale = if *service == "claude-opus-4-5" {
            0.85 + 0.05 * index as f64
        } else {
            1.0
        };

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
                if matches!(*service, "deepseek-reasoner" | "gpt-5") && (now - start).num_days() < 3
                {
                    daily *= 3.4;
                }
                let Some(next) = day.succ_opt() else { break };
                let end = day_start(next);
                charges.push(usage_charge(
                    provider, service, *line, currency, daily, start, end,
                ));
                if let Some(prices) = &prices {
                    for (unit, quantity) in token_quantities(provider, prices, daily, price_scale) {
                        let quantity = quantity * (1.0 + jitter((service, unit), day));
                        charges.push(token_charge(
                            provider, service, *line, currency, quantity, unit, start, end,
                        ));
                    }
                }
                day = next;
            }
        } else {
            let start = day_start(period.start());
            let end = day_start(period.end_exclusive());
            charges.push(usage_charge(
                provider, service, *line, currency, amount, start, end,
            ));
            if let Some(prices) = &prices {
                for (unit, quantity) in token_quantities(provider, prices, amount, price_scale) {
                    let quantity = quantity * (1.0 + jitter((service, unit), period.start()));
                    charges.push(token_charge(
                        provider, service, *line, currency, quantity, unit, start, end,
                    ));
                }
            }
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
    if model_prices(provider, service).is_some() {
        // For the model providers the service is the model; the row names it
        // as its model the way the usage exports do.
        charge.x_model = Some(service.to_string());
    }
    charge.x_service_code = service_code(provider, service).map(str::to_string);
    charge.charge_description = Some(format!("{provider} {service} usage"));
    charge.billed_cost = Some(amount);
    charge.effective_cost = Some(amount);
    charge.tags = line.map(|line| format!("{{\"business_line\":\"{line}\"}}"));
    charge
}

/// A token-count row beside a model's cost row: the usage, with no amount of
/// its own, the way the providers' usage exports report it.
#[allow(clippy::too_many_arguments)]
fn token_charge(
    provider: &str,
    service: &str,
    line: Option<&str>,
    currency: &str,
    quantity: f64,
    unit: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Charge {
    let mut charge = Charge::new(start, end, currency);
    charge.service_name = Some(service.to_string());
    charge.x_model = Some(service.to_string());
    charge.charge_description = Some(format!("{provider} {service} {unit}"));
    charge.cost_basis = CostBasis::Absent;
    charge.pricing_quantity = Some(quantity);
    charge.pricing_unit = Some(unit.to_string());
    charge.tags = line.map(|line| format!("{{\"business_line\":\"{line}\"}}"));
    charge
}

/// Midnight UTC on `date`.
pub fn day_start(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// Deterministic per-(key, day) wobble in ±12%, so charts look real but
/// re-seeding changes nothing. The key is the service for costs and the
/// (service, unit) pair for token quantities.
fn jitter(key: impl std::hash::Hash, day: NaiveDate) -> f64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (key, day).hash(&mut hasher);
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
            "OpenAI",
            services_of("OpenAI"),
            "USD",
            periods(now())[11],
            11,
            now(),
        );
        let second = period_charges(
            "OpenAI",
            services_of("OpenAI"),
            "USD",
            periods(now())[11],
            11,
            now(),
        );

        // Costs and token quantities alike: the wobble is a hash, not a dice
        // roll, so re-seeding changes nothing.
        let amounts = |charges: &[Charge]| {
            charges
                .iter()
                .map(|charge| (charge.billed_cost, charge.pricing_quantity))
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

        // The flagship model spikes with them.
        let plain = period_charges(
            "Anthropic",
            services_of("Anthropic"),
            "USD",
            periods[7],
            7,
            now(),
        );
        let spiked = period_charges(
            "Anthropic",
            services_of("Anthropic"),
            "USD",
            periods[8],
            8,
            now(),
        );
        assert!(of(&spiked, "claude-opus-4-5") > of(&plain, "claude-opus-4-5") * 2.0);
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
        assert_eq!(DEMO_SOURCES.len(), 5);
        assert!(DEMO_SOURCES
            .iter()
            .all(|(_, account_id, _, _)| account_id.starts_with(DEMO_PREFIX)));
        assert!(batch_id("AWS", periods(now())[0]).starts_with(DEMO_PREFIX));
    }

    #[test]
    fn model_rows_carry_their_category_and_their_tokens_are_usage_not_money() {
        let periods = periods(now());

        let anthropic = period_charges(
            "Anthropic",
            services_of("Anthropic"),
            "USD",
            periods[11],
            11,
            now(),
        );
        // Every Anthropic row names its model, and leaves the category to
        // the writer's mapping.
        assert!(anthropic.iter().all(
            |charge| charge.x_model == charge.service_name && charge.service_category.is_none()
        ));

        let tokens: Vec<&Charge> = anthropic
            .iter()
            .filter(|charge| charge.pricing_quantity.is_some())
            .collect();
        assert!(!tokens.is_empty());
        // The token rows count usage; the cost lives on its own rows.
        assert!(tokens
            .iter()
            .all(|charge| charge.cost_basis == CostBasis::Absent
                && charge.billed_cost.is_none()
                && charge.pricing_quantity.unwrap() > 0.0));
        assert!(anthropic
            .iter()
            .filter(|charge| charge.billed_cost.is_some())
            .all(|charge| charge.pricing_quantity.is_none()));

        let units: std::collections::HashSet<&str> = tokens
            .iter()
            .filter_map(|charge| charge.pricing_unit.as_deref())
            .collect();
        for unit in [
            "Input Tokens",
            "Output Tokens",
            "Cache Read Tokens",
            "Cache Creation Tokens",
        ] {
            assert!(units.contains(unit), "missing {unit}");
        }

        // Settled periods carry the same token rows, monthly aggregated.
        let settled = period_charges(
            "Anthropic",
            services_of("Anthropic"),
            "USD",
            periods[0],
            0,
            now(),
        );
        assert!(settled
            .iter()
            .any(|charge| charge.pricing_quantity.is_some()));

        // OpenAI reports its cache class instead of Anthropic's.
        let openai = period_charges(
            "OpenAI",
            services_of("OpenAI"),
            "USD",
            periods[11],
            11,
            now(),
        );
        let units: std::collections::HashSet<&str> = openai
            .iter()
            .filter_map(|charge| charge.pricing_unit.as_deref())
            .collect();
        assert!(units.contains("Cached Input Tokens"));
    }

    #[test]
    fn opus_gets_pricier_over_the_year() {
        let periods = periods(now());

        // The blended $/token of the flagship model, for one settled month.
        let blended = |index: usize| {
            let charges = period_charges(
                "Anthropic",
                services_of("Anthropic"),
                "USD",
                periods[index],
                index,
                now(),
            );
            let opus = |charge: &&Charge| charge.service_name.as_deref() == Some("claude-opus-4-5");
            let cost: f64 = charges
                .iter()
                .filter(opus)
                .filter_map(|charge| charge.billed_cost)
                .sum();
            let tokens: f64 = charges
                .iter()
                .filter(opus)
                .filter_map(|charge| charge.pricing_quantity)
                .sum();
            cost / tokens
        };

        // Cost rises faster than tokens, so the price/volume split has
        // something to say.
        assert!(blended(10) > blended(1) * 1.2);
    }
}
