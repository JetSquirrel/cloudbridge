//! What the dashboard shows, read out of the ledger.
//!
//! Nothing here talks to a provider. The numbers come from
//! `v_charge_normalized`, so they are already in one currency by the time
//! anything adds them up — the dashboard used to sum AWS dollars and
//! Alibaba Cloud yuan into a single figure.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::cloud::registry::SourceDescriptor;
use crate::cloud::{BillingPeriod, CloudAccount, SourceId};
use crate::ledger::query::{self, Balance};
use crate::ledger::PeriodKey;

/// Shown in place of a source's name when its id is not in the registry.
const UNKNOWN_SOURCE: &str = "Unknown";

/// One service's share of a period.
#[derive(Debug, Clone)]
pub struct ServiceCost {
    pub service: String,
    pub amount: f64,
    pub currency: String,
}

/// One day of a trend chart.
#[derive(Debug, Clone)]
pub struct DailyCost {
    pub date: String,
    pub amount: f64,
}

/// One account's card on the dashboard.
#[derive(Debug, Clone)]
pub struct AccountReport {
    pub account_id: String,
    pub account_name: String,
    pub source_id: SourceId,
    /// The headline figure: what was charged this period, or — for a
    /// source that only reports state — what is left in the account.
    pub amount: f64,
    /// Currency `amount` is in: the reporting currency for charges, the
    /// source's own for a balance. A balance is not converted, because it
    /// is not spend and does not belong in a spend total.
    pub currency: String,
    /// Change against the period before, or `None` for a source that
    /// reports a balance and has no period to compare against.
    pub month_over_month_change: Option<f64>,
    /// What the headline figure is made of: services for a period cost,
    /// the granted and topped-up parts for a balance.
    pub services: Vec<ServiceCost>,
}

impl AccountReport {
    fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        self.source_id.descriptor()
    }

    /// Short label for the source, for badges.
    pub fn short_name(&self) -> &'static str {
        self.descriptor().map_or(UNKNOWN_SOURCE, |s| s.short_name)
    }

    /// Whether the headline figure is a balance rather than a period cost.
    /// The dashboard lists the two kinds separately and labels them
    /// differently.
    pub fn is_snapshot(&self) -> bool {
        self.descriptor().is_some_and(SourceDescriptor::is_snapshot)
    }
}

/// Everything one dashboard render needs.
#[derive(Debug, Clone)]
pub struct DashboardReport {
    pub accounts: Vec<AccountReport>,
    /// Cross-cloud totals, in the reporting currency.
    pub current_month: f64,
    pub last_month: f64,
    pub month_over_month_change: f64,
    pub reporting_currency: String,
    /// Charges left out of the totals because no rate covers their
    /// currency. Zero unless a source starts billing in something the
    /// built-in rate table does not know.
    pub unconverted_charges: i64,
}

/// Read the ledger for a set of accounts. No network access.
pub fn build(
    accounts: &[CloudAccount],
    now: DateTime<Utc>,
    reporting_currency: &str,
) -> Result<DashboardReport> {
    let current = BillingPeriod::containing(now);
    let previous = current.previous();

    let mut reports = Vec::new();
    for account in accounts {
        if !account.enabled {
            continue;
        }
        reports.push(account_report(
            account,
            &current,
            &previous,
            reporting_currency,
        )?);
    }

    let current_month = query::total_for_period(&current.label())?;
    let last_month = query::total_for_period(&previous.label())?;

    Ok(DashboardReport {
        accounts: reports,
        current_month,
        last_month,
        month_over_month_change: change(current_month, last_month).unwrap_or(0.0),
        reporting_currency: reporting_currency.to_string(),
        unconverted_charges: query::unconverted_charges(&current.label())?,
    })
}

/// Daily charges for an account over the last `days` days.
pub fn trend(account: &CloudAccount, days: i64, now: DateTime<Utc>) -> Result<Vec<DailyCost>> {
    let since = now - chrono::Duration::days(days);
    let daily = query::daily_totals(account.source_id.as_str(), &account.id, since)?;

    Ok(daily
        .into_iter()
        .map(|(date, amount)| DailyCost { date, amount })
        .collect())
}

/// The symbol an amount is shown with, or the code itself when there is
/// no familiar one.
pub fn symbol(currency: &str) -> &str {
    match currency {
        "USD" => "$",
        "CNY" => "¥",
        other => other,
    }
}

fn account_report(
    account: &CloudAccount,
    current: &BillingPeriod,
    previous: &BillingPeriod,
    reporting_currency: &str,
) -> Result<AccountReport> {
    let provider = account.source_id.as_str();
    let key = |period: &BillingPeriod| {
        PeriodKey::new(provider.to_string(), account.id.clone(), period.label())
    };

    let is_snapshot = account
        .descriptor()
        .is_some_and(SourceDescriptor::is_snapshot);
    let balance = query::latest_balance(provider, &account.id)?;

    let (amount, currency, last_month, services) = if is_snapshot {
        // A balance source has charges too — a top-up is a purchase — but
        // the number worth showing on the card is what is left.
        let currency = balance
            .as_ref()
            .map_or_else(|| reporting_currency.to_string(), |b| b.currency.clone());
        (
            balance.as_ref().map_or(0.0, |b| b.balance),
            currency.clone(),
            None,
            balance
                .as_ref()
                .map_or_else(Vec::new, |b| breakdown(b, &currency)),
        )
    } else {
        let services = query::service_breakdown(&key(current))?
            .into_iter()
            .map(|(service, amount)| ServiceCost {
                service,
                amount,
                currency: reporting_currency.to_string(),
            })
            .collect();

        (
            query::period_total(&key(current))?,
            reporting_currency.to_string(),
            Some(query::period_total(&key(previous))?),
            services,
        )
    };

    Ok(AccountReport {
        account_id: account.id.clone(),
        account_name: account.name.clone(),
        source_id: account.source_id.clone(),
        amount,
        currency,
        month_over_month_change: last_month.and_then(|last| change(amount, last)),
        services,
    })
}

/// What a balance is made of, for the expanded card. Only the parts the
/// source actually reports are listed.
fn breakdown(balance: &Balance, currency: &str) -> Vec<ServiceCost> {
    [
        ("Granted balance", balance.granted_balance),
        ("Topped-up balance", balance.topped_up_balance),
    ]
    .into_iter()
    .filter_map(|(service, amount)| {
        amount
            .filter(|amount| *amount != 0.0)
            .map(|amount| ServiceCost {
                service: service.to_string(),
                amount,
                currency: currency.to_string(),
            })
    })
    .collect()
}

/// Percentage change, or `None` when there is no base to compare against —
/// everything is up infinitely from nothing.
fn change(current: f64, last: f64) -> Option<f64> {
    (last != 0.0).then(|| ((current - last) / last) * 100.0)
}
