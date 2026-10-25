//! Microsoft Azure — a subscription's actual cost from Cost Management's
//! query API, one row per day per service per charge type.
//!
//! The credential is a service principal (an app registration's client id
//! and secret) with the Cost Management Reader role on the subscription.
//! Three requests make a fetch:
//!
//! 1. The subscription's tenant, read off the challenge an unauthenticated
//!    request gets back — the same discovery the Azure CLI does — so the
//!    form asks for the subscription and not for a tenant id as well. A
//!    `tenant/subscription` value in the field skips it.
//! 2. A token for Azure Resource Manager, by the client credentials grant.
//! 3. `POST …/providers/Microsoft.CostManagement/query`, daily, grouped by
//!    service and charge type, following `nextLink` until it runs out.
//!
//! Only the query responses are kept as raw parts; the token never is. The
//! query allows two groupings, and service and charge type are the two the
//! ledger needs: a reservation purchase or a refund must not read as usage.
//! Resource-level detail is the Cost Management export's — import its FOCUS
//! CSV for that.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::http::{self, FailureKind};
use super::raw::RawPart;
use super::{BillingPeriod, BillingSource, Fetched, Normalized, RawBatch};
use crate::ledger::{Charge, ChargeCategory};

const MANAGEMENT: &str = "https://management.azure.com";
const LOGIN: &str = "https://login.microsoftonline.com";
const QUERY_API_VERSION: &str = "2025-03-01";
const SUBSCRIPTION_API_VERSION: &str = "2022-12-01";

/// Prefix of the query payloads, one per page; the page number follows.
const PART_QUERY_PREFIX: &str = "cost_query:";

/// How many pages one period may take. A month of daily rows by service and
/// charge type is a few thousand rows at most; past this, `nextLink` is
/// looping rather than paging.
const MAX_PAGES: usize = 50;

pub struct AzureService {
    client_id: String,
    client_secret: String,
    /// What the account's scope field holds: a subscription id, or
    /// `tenant/subscription`.
    scope: String,
}

impl AzureService {
    pub fn new(client_id: String, client_secret: String, scope: Option<String>) -> Self {
        Self {
            client_id: client_id.trim().to_string(),
            client_secret: client_secret.trim().to_string(),
            scope: scope.unwrap_or_default().trim().to_string(),
        }
    }

    /// The tenant to sign in to and the subscription to query.
    fn tenant_and_subscription(&self) -> Result<(String, String)> {
        let (tenant, subscription) = parse_scope(&self.scope)?;
        let tenant = match tenant {
            Some(tenant) => tenant,
            None => discover_tenant(&subscription)?,
        };
        Ok((tenant, subscription))
    }

    /// An Azure Resource Manager access token for the service principal.
    fn token(&self, tenant: &str) -> Result<String> {
        if self.client_id.is_empty() || self.client_secret.is_empty() {
            return Err(http::failure(
                Some(FailureKind::Denied),
                "Enter the app registration's client ID and client secret".to_string(),
            ));
        }
        let response = http::agent(http::API_TIMEOUT)
            .post(&format!("{LOGIN}/{tenant}/oauth2/v2.0/token"))
            .send_form([
                ("grant_type", "client_credentials"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("scope", "https://management.azure.com/.default"),
            ])
            .map_err(|e| http::request_failed("Azure sign-in failed", e))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read the Azure sign-in response: {}", e))?;

        let token: TokenResponse = serde_json::from_str(&body).map_err(|_| {
            anyhow!(
                "Azure sign-in returned HTTP {} with an unreadable body",
                status
            )
        })?;
        match token.access_token {
            Some(token) if status < 400 => Ok(token),
            _ => Err(http::failure(
                // A wrong secret, an expired one, or an unknown client id is
                // a 400 or 401 here; nothing clears it but a new credential.
                match status {
                    400 | 401 | 403 => Some(FailureKind::Denied),
                    status => http::status_kind(status),
                },
                format!(
                    "Azure sign-in failed: HTTP {} - {}",
                    status,
                    token
                        .error_description
                        .or(token.error)
                        .unwrap_or_else(|| "no reason given".to_string())
                ),
            )),
        }
    }

    /// POST the query to `url`, returning the body unchanged once the
    /// status says it succeeded.
    fn query_page(&self, url: &str, token: &str, body: &str) -> Result<String> {
        let response = http::agent(http::API_TIMEOUT)
            .post(url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|e| http::request_failed("Azure Cost Management request failed", e))?;
        let status = response.status().as_u16();
        let text = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read the Azure Cost Management response: {}", e))?;
        if status >= 400 {
            return Err(http::status_failed(
                status,
                format!(
                    "Azure Cost Management request failed: HTTP {} - {}",
                    status,
                    error_message(&text)
                ),
            ));
        }
        Ok(text)
    }

    /// Every page of the period's query, as raw parts.
    fn query(&self, period: &BillingPeriod) -> Result<Vec<RawPart>> {
        let (tenant, subscription) = self.tenant_and_subscription()?;
        let token = self.token(&tenant)?;
        let body = query_body(period);
        let mut url = format!(
            "{MANAGEMENT}/subscriptions/{subscription}/providers/Microsoft.CostManagement/query?api-version={QUERY_API_VERSION}"
        );

        let mut parts = Vec::new();
        for page in 0..MAX_PAGES {
            let response = self.query_page(&url, &token, &body)?;
            let next = next_link(&response);
            parts.push(RawPart::new(
                format!("{PART_QUERY_PREFIX}{page}"),
                format!("POST {url}\n{body}"),
                response,
            ));
            match next {
                Some(next) => url = next,
                None => return Ok(parts),
            }
        }
        Err(anyhow!(
            "Azure Cost Management kept paging past {MAX_PAGES} pages for {}",
            period.label()
        ))
    }
}

impl BillingSource for AzureService {
    fn validate_credentials(&self) -> Result<bool> {
        let period = BillingPeriod::containing(Utc::now());
        match self.query(&period) {
            Ok(_) => Ok(true),
            Err(e) => {
                tracing::warn!("Azure credential validation failed: {}", e);
                Ok(false)
            }
        }
    }

    fn fetch(&self, period: &BillingPeriod) -> Result<Fetched> {
        Ok(Fetched::parts_only(self.query(period)?))
    }

    fn normalize(&self, batch: &RawBatch) -> Result<Normalized> {
        normalize(batch)
    }
}

/// The scope field as `(tenant, subscription)`. A subscription id alone
/// leaves the tenant to discovery.
fn parse_scope(scope: &str) -> Result<(Option<String>, String)> {
    let (tenant, subscription) = match scope.split_once('/') {
        Some((tenant, subscription)) => (Some(tenant.trim()), subscription.trim()),
        None => (None, scope.trim()),
    };
    let is_guid =
        |value: &str| value.len() == 36 && value.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !is_guid(subscription) {
        return Err(anyhow!(
            "'{}' is not an Azure subscription ID. Copy it from the portal's \
             Subscriptions page; it looks like 00000000-0000-0000-0000-000000000000",
            subscription
        ));
    }
    if let Some(tenant) = tenant.filter(|tenant| !is_guid(tenant)) {
        return Err(anyhow!("'{}' is not an Azure tenant ID", tenant));
    }
    Ok((tenant.map(str::to_string), subscription.to_string()))
}

/// The tenant a subscription belongs to, from the challenge Azure Resource
/// Manager answers an unauthenticated request with.
fn discover_tenant(subscription: &str) -> Result<String> {
    let response = http::agent(http::API_TIMEOUT)
        .get(&format!(
            "{MANAGEMENT}/subscriptions/{subscription}?api-version={SUBSCRIPTION_API_VERSION}"
        ))
        .call()
        .map_err(|e| http::request_failed("Azure tenant lookup failed", e))?;
    response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .and_then(tenant_from_challenge)
        .ok_or_else(|| {
            anyhow!(
                "Couldn't find the tenant of subscription {}. Enter it as \
                 tenant-id/subscription-id instead.",
                subscription
            )
        })
}

/// The tenant id in a `WWW-Authenticate` challenge's `authorization_uri`.
fn tenant_from_challenge(challenge: &str) -> Option<String> {
    let start = challenge.find("authorization_uri=\"")? + "authorization_uri=\"".len();
    let uri = &challenge[start..];
    let uri = &uri[..uri.find('"')?];
    let tenant = uri.trim_end_matches('/').rsplit('/').next()?;
    (!tenant.is_empty()).then(|| tenant.to_string())
}

/// The query for one calendar month: actual cost, daily, by service and
/// charge type.
fn query_body(period: &BillingPeriod) -> String {
    let last_day = period
        .end_exclusive()
        .pred_opt()
        .expect("a month has a last day");
    serde_json::json!({
        "type": "ActualCost",
        "timeframe": "Custom",
        "timePeriod": {
            "from": format!("{}T00:00:00Z", period.start()),
            "to": format!("{last_day}T23:59:59Z"),
        },
        "dataset": {
            "granularity": "Daily",
            "aggregation": { "totalCost": { "name": "Cost", "function": "Sum" } },
            "grouping": [
                { "type": "Dimension", "name": "ServiceName" },
                { "type": "Dimension", "name": "ChargeType" },
            ],
        },
    })
    .to_string()
}

fn next_link(body: &str) -> Option<String> {
    let result: QueryResult = serde_json::from_str(body).ok()?;
    result
        .properties
        .next_link
        .filter(|link| !link.trim().is_empty())
}

/// The message an Azure error body carries, or the body itself.
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

/// Turn fetched query pages into ledger rows. Pure — every input is in
/// `batch`. A row outside the batch's month, or one that costs nothing, is
/// left out.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    let pages: Vec<&RawPart> = batch
        .parts
        .iter()
        .filter(|part| part.name.starts_with(PART_QUERY_PREFIX))
        .collect();
    if pages.is_empty() {
        return Err(anyhow!("Raw batch has no Azure cost query payload"));
    }

    let mut charges = Vec::new();
    for page in pages {
        let result: QueryResult = serde_json::from_str(&page.body)
            .map_err(|e| anyhow!("Failed to parse the Azure cost query: {}", e))?;
        let columns = Columns::resolve(&result.properties.columns)?;
        for row in &result.properties.rows {
            let Some(cost) = row.get(columns.cost).and_then(Value::as_f64) else {
                continue;
            };
            let day = row
                .get(columns.date)
                .and_then(usage_date)
                .ok_or_else(|| anyhow!("Azure cost row has an unreadable UsageDate: {:?}", row))?;
            if day < batch.period.start() || day >= batch.period.end_exclusive() || cost == 0.0 {
                continue;
            }
            let text = |index: Option<usize>| {
                index
                    .and_then(|index| row.get(index))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let start: DateTime<Utc> = day.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
            let charge_type = text(columns.charge_type);
            charges.push(Charge {
                charge_category: charge_category(charge_type.as_deref()),
                charge_description: charge_type,
                service_name: text(columns.service),
                billed_cost: Some(cost),
                effective_cost: Some(cost),
                ..Charge::new(
                    start,
                    start + Duration::days(1),
                    text(columns.currency).unwrap_or_else(|| "USD".to_string()),
                )
            });
        }
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

/// Where the columns the normalizer reads are in one page. The query names
/// its output columns after the fields asked for, so they are found by
/// name rather than by position.
struct Columns {
    cost: usize,
    date: usize,
    currency: Option<usize>,
    service: Option<usize>,
    charge_type: Option<usize>,
}

impl Columns {
    fn resolve(columns: &[Column]) -> Result<Self> {
        let find = |names: &[&str]| {
            columns.iter().position(|column| {
                names
                    .iter()
                    .any(|name| column.name.eq_ignore_ascii_case(name))
            })
        };
        let names = || {
            columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        Ok(Self {
            cost: find(&["Cost", "PreTaxCost", "totalCost"])
                .ok_or_else(|| anyhow!("Azure cost query has no cost column ({})", names()))?,
            date: find(&["UsageDate"])
                .ok_or_else(|| anyhow!("Azure cost query has no UsageDate column ({})", names()))?,
            currency: find(&["Currency", "BillingCurrency"]),
            service: find(&["ServiceName"]),
            charge_type: find(&["ChargeType"]),
        })
    }
}

/// A `UsageDate` cell: `20261003` as a number, or an ISO date string, as
/// the API has returned it in different versions.
fn usage_date(value: &Value) -> Option<NaiveDate> {
    if let Some(number) = value.as_f64() {
        let number = number as i64;
        return NaiveDate::from_ymd_opt(
            (number / 10_000) as i32,
            (number / 100 % 100) as u32,
            (number % 100) as u32,
        );
    }
    let text = value.as_str()?;
    NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok()
}

/// The ledger category of an Azure charge type. Unused reservation and
/// savings plan hours are what the commitment cost while idle: still usage.
fn charge_category(charge_type: Option<&str>) -> ChargeCategory {
    match charge_type.map(str::to_ascii_lowercase).as_deref() {
        Some("purchase") => ChargeCategory::Purchase,
        Some("refund") => ChargeCategory::Credit,
        Some("tax") => ChargeCategory::Tax,
        Some("roundingadjustment") => ChargeCategory::Adjustment,
        _ => ChargeCategory::Usage,
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct QueryResult {
    properties: QueryProperties,
}

#[derive(Deserialize)]
struct QueryProperties {
    #[serde(default)]
    columns: Vec<Column>,
    #[serde(default)]
    rows: Vec<Vec<Value>>,
    #[serde(rename = "nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct Column {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBSCRIPTION: &str = "11111111-2222-3333-4444-555555555555";
    const TENANT: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    fn batch(body: &str) -> RawBatch {
        RawBatch {
            provider: "Azure".to_string(),
            account_id: "a".to_string(),
            period: BillingPeriod::new(2026, 9),
            batch_id: "b".to_string(),
            fetched_at: Utc::now(),
            parts: vec![RawPart::new(format!("{PART_QUERY_PREFIX}0"), "POST", body)],
            payload_files: Vec::new(),
        }
    }

    #[test]
    fn a_page_becomes_daily_charges_by_service_and_type() {
        let normalized = normalize(&batch(include_str!("testdata/azure_cost_query.json"))).unwrap();
        let charges = &normalized.charges;

        // The zero-cost row and the row from October are left out.
        assert_eq!(charges.len(), 4, "{charges:#?}");
        let vm = &charges[0];
        assert_eq!(vm.service_name.as_deref(), Some("Virtual Machines"));
        assert_eq!(vm.billed_cost, Some(12.5));
        assert_eq!(vm.billing_currency, "USD");
        assert_eq!(vm.charge_category, ChargeCategory::Usage);
        assert_eq!(
            vm.charge_period_start.date_naive(),
            NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()
        );
        assert_eq!(
            vm.charge_period_end - vm.charge_period_start,
            Duration::days(1)
        );
        // A reservation bought and a refund are not usage.
        assert_eq!(charges[2].charge_category, ChargeCategory::Purchase);
        assert_eq!(charges[3].charge_category, ChargeCategory::Credit);
        assert_eq!(charges[3].billed_cost, Some(-3.0));
    }

    #[test]
    fn a_batch_without_query_pages_is_an_error() {
        let mut empty = batch("{}");
        empty.parts.clear();
        assert!(normalize(&empty).is_err());
    }

    #[test]
    fn the_usage_date_reads_as_a_number_or_a_string() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 3);
        assert_eq!(usage_date(&serde_json::json!(20260903)), day);
        assert_eq!(usage_date(&serde_json::json!(20260903.0)), day);
        assert_eq!(usage_date(&serde_json::json!("2026-09-03T00:00:00")), day);
        assert_eq!(usage_date(&serde_json::json!("soon")), None);
    }

    #[test]
    fn the_scope_is_a_subscription_or_tenant_slash_subscription() {
        assert_eq!(
            parse_scope(SUBSCRIPTION).unwrap(),
            (None, SUBSCRIPTION.to_string())
        );
        assert_eq!(
            parse_scope(&format!(" {TENANT} / {SUBSCRIPTION} ")).unwrap(),
            (Some(TENANT.to_string()), SUBSCRIPTION.to_string())
        );
        assert!(parse_scope("my-subscription").is_err());
        assert!(parse_scope(&format!("contoso/{SUBSCRIPTION}")).is_err());
    }

    #[test]
    fn the_tenant_is_read_off_the_challenge() {
        let challenge = format!(
            "Bearer authorization_uri=\"https://login.windows.net/{TENANT}\", \
             error=\"invalid_token\", error_description=\"The authentication failed\""
        );
        assert_eq!(tenant_from_challenge(&challenge).as_deref(), Some(TENANT));
        assert_eq!(tenant_from_challenge("Bearer realm=\"x\""), None);
    }

    #[test]
    fn the_query_asks_for_the_whole_month_daily() {
        let body: Value = serde_json::from_str(&query_body(&BillingPeriod::new(2026, 2))).unwrap();
        assert_eq!(body["timePeriod"]["from"], "2026-02-01T00:00:00Z");
        assert_eq!(body["timePeriod"]["to"], "2026-02-28T23:59:59Z");
        assert_eq!(body["dataset"]["granularity"], "Daily");
        assert_eq!(body["dataset"]["grouping"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn paging_follows_next_link_until_it_is_empty() {
        let page = |link: &str| {
            format!(r#"{{"properties":{{"columns":[],"rows":[],"nextLink":{link}}}}}"#)
        };
        assert_eq!(
            next_link(&page("\"https://next\"")).as_deref(),
            Some("https://next")
        );
        assert_eq!(next_link(&page("null")), None);
        assert_eq!(next_link(&page("\"\"")), None);
    }
}
