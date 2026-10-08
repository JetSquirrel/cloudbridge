//! Cloudflare — the account's billable usage, as FOCUS rows.
//!
//! `GET /accounts/{id}/billable-usage` reports what a pay-as-you-go account
//! has been metered for, one row per service per day, in FOCUS column names.
//! Daily rows are the point: a runaway Worker or Durable Object shows up the
//! day after it starts, not on the invoice at the end of the month.
//!
//! Two things shape the fetch. Cloudflare bills on a cycle anchored at the
//! subscription's start, not on the calendar month, and the endpoint returns
//! nothing for a range that does not contain the anchor day — so a calendar
//! month is asked for as the one or two cycles that overlap it, and the
//! normalizer keeps the days that fall inside it. And the endpoint covers
//! PayGo (self-serve) accounts only; `/billable-usage/info` says which kind
//! this is, and an account it does not cover is an error, not an empty
//! month.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::Deserialize;
use std::collections::{BTreeSet, HashSet};

use super::raw::RawPart;
use super::{BillingPeriod, BillingSource, Fetched, Normalized, RawBatch};
use crate::ledger::Charge;

const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Name the info payload is stored under in a raw batch.
const PART_INFO: &str = "billable_usage_info";

/// Prefix of the usage payloads, one per billing cycle asked for; the
/// cycle's first day follows it.
const PART_USAGE_PREFIX: &str = "billable_usage:";

/// Cloudflare service
pub struct CloudflareService {
    account_id: String,
    api_token: String,
}

impl CloudflareService {
    pub fn new(account_id: String, api_token: String, _region: Option<String>) -> Self {
        Self {
            account_id: account_id.trim().to_string(),
            api_token: api_token.trim().to_string(),
        }
    }

    /// GET a path under the account and return the body unchanged, once
    /// the response envelope says it succeeded.
    fn get(&self, path: &str) -> Result<String> {
        // The account id lands in the URL path; a tag is 32 hex characters,
        // so anything else is a mistyped field rather than an id.
        if self.account_id.is_empty() || !self.account_id.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Err(anyhow!(
                "'{}' is not a Cloudflare account ID — copy it from the dashboard's \
                 account home, it is 32 hexadecimal characters",
                self.account_id
            ));
        }

        let url = format!("{}/accounts/{}{}", API_BASE, self.account_id, path);
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .new_agent();

        let response = agent
            .get(&url)
            .header("Accept", "application/json")
            .header("Authorization", &format!("Bearer {}", self.api_token))
            .call()
            .map_err(|e| anyhow!("Cloudflare API request failed: {}", e))?;

        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read Cloudflare response: {}", e))?;

        // Every v4 response carries the same envelope; its messages say
        // more than the status code does.
        let envelope: Envelope = serde_json::from_str(&body).map_err(|_| {
            anyhow!(
                "Cloudflare API returned HTTP {} with an unreadable body",
                status
            )
        })?;
        if status >= 400 || !envelope.success {
            return Err(anyhow!(
                "Cloudflare API request failed: HTTP {} - {}",
                status,
                envelope.error_text()
            ));
        }

        Ok(body)
    }

    fn info_raw(&self) -> Result<String> {
        self.get("/billable-usage/info")
    }
}

impl BillingSource for CloudflareService {
    fn validate_credentials(&self) -> Result<bool> {
        match self.info_raw().and_then(|body| parse_info(&body)) {
            Ok(info) if info.covered => Ok(true),
            Ok(_) => {
                tracing::warn!("{}", NOT_COVERED);
                Ok(false)
            }
            Err(e) => {
                tracing::warn!("Cloudflare credential validation failed: {}", e);
                Ok(false)
            }
        }
    }

    fn fetch(&self, period: &BillingPeriod) -> Result<Fetched> {
        let info_body = self.info_raw()?;
        let info = parse_info(&info_body)?;
        if !info.covered {
            return Err(anyhow!(NOT_COVERED));
        }

        let anchor_days: BTreeSet<u32> = info
            .subscriptions
            .iter()
            .filter_map(|subscription| subscription.anchor_day())
            .collect();

        let mut parts = vec![RawPart::new(
            PART_INFO,
            "GET /billable-usage/info",
            info_body,
        )];

        // A cycle that has not started yet has nothing to report.
        let today = Utc::now().date_naive();
        let cycles: BTreeSet<(NaiveDate, NaiveDate)> = anchor_days
            .iter()
            .flat_map(|&day| cycles_overlapping(period, day))
            .filter(|(start, _)| *start <= today)
            .collect();

        if cycles.is_empty() {
            // No anchor to aim at: the endpoint's own default is the
            // current billing cycle, which is the best that can be asked.
            let path = "/billable-usage";
            parts.push(RawPart::new(
                format!("{}current", PART_USAGE_PREFIX),
                format!("GET {}", path),
                self.get(path)?,
            ));
        }
        for (start, end) in cycles {
            let path = format!("/billable-usage?from={}&to={}", start, end);
            parts.push(RawPart::new(
                format!("{}{}", PART_USAGE_PREFIX, start),
                format!("GET {}", path),
                self.get(&path)?,
            ));
        }

        Ok(Fetched::parts_only(parts))
    }

    fn normalize(&self, batch: &RawBatch) -> Result<Normalized> {
        normalize(batch)
    }
}

const NOT_COVERED: &str = "This Cloudflare account is not covered by the billable usage API, \
     which reports pay-as-you-go (self-serve) accounts only";

/// The billing cycles, as `[first day, first day of the next)`, that
/// overlap a calendar month, for a subscription anchored on `anchor_day`.
///
/// A cycle starts on the anchor day of each month, or on the month's last
/// day when the month is too short to have one — a subscription anchored on
/// the 31st renews on 30 April.
fn cycles_overlapping(period: &BillingPeriod, anchor_day: u32) -> Vec<(NaiveDate, NaiveDate)> {
    let previous = period.previous();
    let next = BillingPeriod::containing(
        period
            .end_exclusive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists")
            .and_utc(),
    );
    let starts = [
        cycle_start(&previous, anchor_day),
        cycle_start(period, anchor_day),
        cycle_start(&next, anchor_day),
    ];

    starts
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .filter(|(start, end)| *start < period.end_exclusive() && *end > period.start())
        .collect()
}

/// The day a cycle anchored on `anchor_day` starts in `period`.
fn cycle_start(period: &BillingPeriod, anchor_day: u32) -> NaiveDate {
    let last_day = period
        .end_exclusive()
        .pred_opt()
        .expect("a month has a last day")
        .day();
    NaiveDate::from_ymd_opt(period.year, period.month, anchor_day.clamp(1, last_day))
        .expect("a day within the month")
}

/// Turn fetched billable-usage payloads into ledger rows.
///
/// Pure — every input is in `batch`. Each usage row becomes one `Usage`
/// charge over its own charge period, kept only when that period starts in
/// the batch's calendar month: the cycles asked for spill into the months
/// on either side, and those days belong to the neighbouring batches.
///
/// Two cycles requested for two subscriptions can return the same row, so
/// a row is taken once however many payloads carry it.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    let start = batch
        .period
        .start()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();
    let end = batch
        .period
        .end_exclusive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();

    let usage_parts: Vec<&RawPart> = batch
        .parts
        .iter()
        .filter(|part| part.name.starts_with(PART_USAGE_PREFIX))
        .collect();
    if usage_parts.is_empty() {
        return Err(anyhow!(
            "Raw batch has no Cloudflare billable usage payload"
        ));
    }

    let mut seen = HashSet::new();
    let mut charges = Vec::new();
    for part in usage_parts {
        let response: UsageResponse = serde_json::from_str(&part.body)
            .map_err(|e| anyhow!("Failed to parse Cloudflare billable usage: {}", e))?;

        for row in response.result.unwrap_or_default() {
            let charge_start = parse_time(&row.charge_period_start, "ChargePeriodStart")?;
            let charge_end = parse_time(&row.charge_period_end, "ChargePeriodEnd")?;
            if charge_start < start || charge_start >= end {
                continue;
            }

            let cost = row.billed_cost.unwrap_or(0.0);
            // What was used, free allowance included. `PricingQuantity` is
            // only the part past the allowance — zero for the whole of a
            // month inside it, which is exactly when a runaway starts:
            // usage climbs for days before the first cent is billed.
            let consumed = row
                .consumed_quantity
                .or(row.pricing_quantity)
                .unwrap_or(0.0);
            // A service listed with nothing used and nothing charged.
            if cost == 0.0 && consumed == 0.0 {
                continue;
            }

            let identity = (
                charge_start,
                row.service_name.clone(),
                row.charge_description.clone(),
                row.subscription_id.clone(),
                row.zone_id.clone(),
            );
            if !seen.insert(identity) {
                continue;
            }

            charges.push(Charge {
                billing_account_id: row.billing_account_id,
                charge_description: row.charge_description,
                service_name: row.service_name,
                // The family (`Workers`) is the stable grouping a category
                // keys on; the service name splits it by plan and meter.
                x_service_code: row.service_family_name,
                // A zone is what Cloudflare attributes zone-level usage to;
                // account-level services such as Workers carry none.
                resource_id: row.zone_id,
                resource_name: row.zone_name,
                billed_cost: Some(cost),
                effective_cost: row.effective_cost.or(Some(cost)),
                list_cost: row.list_cost,
                // The ledger's one quantity column holds what was consumed,
                // as a usage export's rows do; `billed_cost` says what of
                // it was charged for.
                pricing_quantity: Some(consumed),
                pricing_unit: row
                    .consumed_unit
                    .filter(|unit| !unit.trim().is_empty())
                    .or(row.pricing_unit),
                ..Charge::new(
                    charge_start,
                    charge_end,
                    row.billing_currency.unwrap_or_else(|| "USD".to_string()),
                )
            });
        }
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

fn parse_time(value: &Option<String>, field: &str) -> Result<DateTime<Utc>> {
    let value = value
        .as_deref()
        .ok_or_else(|| anyhow!("Cloudflare usage row has no {}", field))?;
    value.parse::<DateTime<Utc>>().map_err(|e| {
        anyhow!(
            "Cloudflare usage row has a bad {} '{}': {}",
            field,
            value,
            e
        )
    })
}

fn parse_info(body: &str) -> Result<UsageInfo> {
    let response: InfoResponse = serde_json::from_str(body)
        .map_err(|e| anyhow!("Failed to parse Cloudflare billable usage info: {}", e))?;
    response
        .result
        .ok_or_else(|| anyhow!("Cloudflare billable usage info has no result"))
}

// ==================== Response Structs ====================

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiMessage>,
}

impl Envelope {
    fn error_text(&self) -> String {
        if self.errors.is_empty() {
            return "no error message".to_string();
        }
        self.errors
            .iter()
            .map(|error| match error.code {
                Some(code) => format!("{} ({})", error.message, code),
                None => error.message.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    #[serde(default)]
    message: String,
    code: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct InfoResponse {
    result: Option<UsageInfo>,
}

#[derive(Debug, Deserialize)]
struct UsageInfo {
    #[serde(default)]
    covered: bool,
    #[serde(default)]
    subscriptions: Vec<Subscription>,
}

#[derive(Debug, Deserialize)]
struct Subscription {
    billing_cycle_anchor_timestamp: Option<String>,
    /// Present only once the subscription has been cancelled.
    end_timestamp: Option<String>,
}

impl Subscription {
    /// The day of the month this subscription's cycles start on, for one
    /// that is still active.
    fn anchor_day(&self) -> Option<u32> {
        if self.end_timestamp.is_some() {
            return None;
        }
        self.billing_cycle_anchor_timestamp
            .as_deref()?
            .parse::<DateTime<Utc>>()
            .ok()
            .map(|anchor| anchor.day())
    }
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    result: Option<Vec<UsageRow>>,
}

/// One billable-usage record. FOCUS column names, as Cloudflare spells
/// them; every field optional, since an alpha endpoint's nulls are not a
/// reason to drop a month.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UsageRow {
    billed_cost: Option<f64>,
    effective_cost: Option<f64>,
    list_cost: Option<f64>,
    billing_account_id: Option<String>,
    billing_currency: Option<String>,
    charge_description: Option<String>,
    charge_period_start: Option<String>,
    charge_period_end: Option<String>,
    pricing_quantity: Option<f64>,
    pricing_unit: Option<String>,
    consumed_quantity: Option<f64>,
    consumed_unit: Option<String>,
    service_name: Option<String>,
    service_family_name: Option<String>,
    subscription_id: Option<String>,
    zone_id: Option<String>,
    zone_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped after the documented response of `GET /billable-usage`; not
    /// a recorded one.
    const USAGE: &str = include_str!("testdata/cloudflare_billable_usage.json");

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn batch(year: i32, month: u32, bodies: &[&str]) -> RawBatch {
        let mut parts = vec![RawPart::new(
            PART_INFO,
            "",
            r#"{"success":true,"result":{"covered":true,"subscriptions":[]}}"#,
        )];
        for (index, body) in bodies.iter().enumerate() {
            parts.push(RawPart::new(
                format!("{}{}", PART_USAGE_PREFIX, index),
                "",
                *body,
            ));
        }
        RawBatch {
            provider: "Cloudflare".to_string(),
            account_id: "acct-cf".to_string(),
            period: BillingPeriod::new(year, month),
            batch_id: "b-1".to_string(),
            fetched_at: "2026-10-08T09:30:00Z".parse().unwrap(),
            parts,
            payload_files: Vec::new(),
        }
    }

    #[test]
    fn each_day_of_usage_becomes_a_charge_of_its_own() {
        let normalized = normalize(&batch(2026, 10, &[USAGE])).unwrap();

        assert!(normalized.balances.is_empty());
        let durable: Vec<&Charge> = normalized
            .charges
            .iter()
            .filter(|c| c.x_service_code.as_deref() == Some("Durable Objects"))
            .collect();
        assert_eq!(durable.len(), 2);

        let first = durable[0];
        assert_eq!(
            first.service_name.as_deref(),
            Some("Durable Objects Rows Written")
        );
        assert_eq!(first.billed_cost, Some(1250.0));
        assert_eq!(first.pricing_quantity, Some(1_250_000_000.0));
        assert_eq!(first.pricing_unit.as_deref(), Some("Rows"));
        assert_eq!(first.billing_currency, "USD");
        assert_eq!(
            first.charge_period_start.to_rfc3339(),
            "2026-10-01T00:00:00+00:00"
        );
        assert_eq!(
            first.charge_period_end.to_rfc3339(),
            "2026-10-02T00:00:00+00:00"
        );
        assert_eq!(
            first.billing_account_id.as_deref(),
            Some("023e105f4ecef8ad9ca31a8372d0c353")
        );
    }

    #[test]
    fn days_outside_the_calendar_month_are_left_to_their_own_batch() {
        // The fixture's cycle starts on 28 September.
        let october = normalize(&batch(2026, 10, &[USAGE])).unwrap();
        assert!(october
            .charges
            .iter()
            .all(|c| c.charge_period_start.month() == 10));

        let september = normalize(&batch(2026, 9, &[USAGE])).unwrap();
        assert_eq!(september.charges.len(), 1);
        assert_eq!(
            september.charges[0].charge_period_start.to_rfc3339(),
            "2026-09-28T00:00:00+00:00"
        );
    }

    #[test]
    fn a_row_two_payloads_both_carry_is_counted_once() {
        let once = normalize(&batch(2026, 10, &[USAGE])).unwrap();
        let twice = normalize(&batch(2026, 10, &[USAGE, USAGE])).unwrap();
        assert_eq!(once.charges.len(), twice.charges.len());
    }

    #[test]
    fn a_zone_s_usage_is_filed_against_the_zone() {
        let normalized = normalize(&batch(2026, 10, &[USAGE])).unwrap();
        let zoned = normalized
            .charges
            .iter()
            .find(|c| c.resource_id.is_some())
            .expect("the fixture has a zone-level row");
        assert_eq!(zoned.resource_name.as_deref(), Some("example.com"));
    }

    /// A month inside the free allowance bills nothing and prices nothing
    /// — every `PricingQuantity` is zero — but it was used, and the usage
    /// is what a runaway shows first. Shaped after a real response.
    #[test]
    fn usage_inside_the_free_allowance_is_kept() {
        let normalized = normalize(&batch(
            2026,
            10,
            &[r#"{"success":true,"result":[{
                "BilledCost":0,"ListCost":0,"EffectiveCost":0,"BillingCurrency":"USD",
                "ChargePeriodStart":"2026-10-03T00:00:00Z",
                "ChargePeriodEnd":"2026-10-04T00:00:00Z",
                "ServiceName":"R2 Storage Class B Operations (First 10M included)",
                "ServiceFamilyName":"R2",
                "ConsumedQuantity":734,"ConsumedUnit":"",
                "PricingQuantity":0,"PricingUnit":"Count"}]}"#],
        ))
        .unwrap();
        assert_eq!(normalized.charges.len(), 1);
        let row = &normalized.charges[0];
        assert_eq!(row.billed_cost, Some(0.0));
        assert_eq!(row.pricing_quantity, Some(734.0));
        // An empty consumed unit falls back to the pricing unit.
        assert_eq!(row.pricing_unit.as_deref(), Some("Count"));
    }

    #[test]
    fn a_row_with_nothing_used_or_charged_is_dropped() {
        let normalized = normalize(&batch(
            2026,
            10,
            &[r#"{"success":true,"result":[{
                "BilledCost":0,"PricingQuantity":0,"BillingCurrency":"USD",
                "ChargePeriodStart":"2026-10-03T00:00:00Z",
                "ChargePeriodEnd":"2026-10-04T00:00:00Z",
                "ServiceName":"Queues Operations"}]}"#],
        ))
        .unwrap();
        assert!(normalized.charges.is_empty());
    }

    #[test]
    fn a_batch_without_usage_is_an_error_not_an_empty_month() {
        let mut empty = batch(2026, 10, &[]);
        empty.parts.retain(|part| part.name == PART_INFO);
        assert!(normalize(&empty).is_err());
    }

    #[test]
    fn an_anchor_on_the_first_is_the_calendar_month() {
        assert_eq!(
            cycles_overlapping(&BillingPeriod::new(2026, 10), 1),
            vec![(date("2026-10-01"), date("2026-11-01"))]
        );
    }

    #[test]
    fn a_mid_month_anchor_takes_the_two_cycles_either_side() {
        assert_eq!(
            cycles_overlapping(&BillingPeriod::new(2026, 10), 15),
            vec![
                (date("2026-09-15"), date("2026-10-15")),
                (date("2026-10-15"), date("2026-11-15")),
            ]
        );
    }

    #[test]
    fn an_anchor_past_a_short_month_s_end_falls_on_its_last_day() {
        assert_eq!(
            cycles_overlapping(&BillingPeriod::new(2026, 2), 31),
            vec![
                (date("2026-01-31"), date("2026-02-28")),
                (date("2026-02-28"), date("2026-03-31")),
            ]
        );
        // December's next month is the following January.
        assert_eq!(
            cycles_overlapping(&BillingPeriod::new(2026, 12), 20),
            vec![
                (date("2026-11-20"), date("2026-12-20")),
                (date("2026-12-20"), date("2027-01-20")),
            ]
        );
    }

    #[test]
    fn a_cancelled_subscription_has_no_cycles_left() {
        let info = parse_info(
            r#"{"success":true,"result":{"covered":true,"subscriptions":[
                {"id":"a","billing_cycle_anchor_timestamp":"2025-03-09T00:00:00Z",
                 "start_timestamp":"2025-03-09T00:00:00Z"},
                {"id":"b","billing_cycle_anchor_timestamp":"2024-01-20T00:00:00Z",
                 "start_timestamp":"2024-01-20T00:00:00Z",
                 "end_timestamp":"2025-01-20T00:00:00Z"}]}}"#,
        )
        .unwrap();
        let days: Vec<Option<u32>> = info
            .subscriptions
            .iter()
            .map(Subscription::anchor_day)
            .collect();
        assert_eq!(days, vec![Some(9), None]);
    }

    #[test]
    fn an_api_error_reads_as_cloudflare_s_own_message() {
        let envelope: Envelope = serde_json::from_str(
            r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}],
                "messages":[],"result":null}"#,
        )
        .unwrap();
        assert_eq!(envelope.error_text(), "Authentication error (10000)");
    }

    #[test]
    fn an_account_id_that_is_not_a_tag_is_refused_before_any_request() {
        let service = CloudflareService::new("../zones".to_string(), "token".to_string(), None);
        let error = service.get("/billable-usage/info").unwrap_err().to_string();
        assert!(error.contains("not a Cloudflare account ID"), "{}", error);
    }
}
