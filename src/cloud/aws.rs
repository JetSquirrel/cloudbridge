//! AWS Cloud Service Implementation - Using ureq + AWS Signature V4

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::raw::RawPart;
use super::{BillingPeriod, BillingSource, CostData, CostSummary, Normalized, RawBatch, SourceId};
use crate::ledger::Charge;

type HmacSha256 = Hmac<Sha256>;

/// AWS Cloud Service
pub struct AwsCloudService {
    account_id: String,
    account_name: String,
    access_key_id: String,
    secret_access_key: String,
    region: String,
}

impl AwsCloudService {
    pub fn new(
        account_id: String,
        account_name: String,
        access_key_id: String,
        secret_access_key: String,
        region: Option<String>,
    ) -> Self {
        Self {
            account_id,
            account_name,
            access_key_id,
            secret_access_key,
            region: region.unwrap_or_else(|| "us-east-1".to_string()),
        }
    }

    /// Calculate SHA256 hash
    fn sha256_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Calculate HMAC-SHA256
    fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// Create AWS Signature V4 signature
    #[allow(clippy::too_many_arguments)]
    fn sign_request(
        &self,
        method: &str,
        service: &str,
        host: &str,
        uri: &str,
        query_string: &str,
        headers: &[(String, String)],
        payload: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<String> {
        let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = timestamp.format("%Y%m%d").to_string();

        // 1. Create canonical request
        let payload_hash = Self::sha256_hash(payload.as_bytes());

        // Collect all headers (including host and x-amz-date)
        let mut all_headers: Vec<(String, String)> = headers.to_vec();
        all_headers.push(("host".to_string(), host.to_string()));
        all_headers.push(("x-amz-date".to_string(), amz_date.clone()));
        all_headers.push(("x-amz-content-sha256".to_string(), payload_hash.clone()));

        // Sort by lowercase key
        all_headers.sort_by_key(|(name, _)| name.to_lowercase());

        let canonical_headers: String = all_headers
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k.to_lowercase(), v.trim()))
            .collect();

        let signed_headers: String = all_headers
            .iter()
            .map(|(k, _)| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method, uri, query_string, canonical_headers, signed_headers, payload_hash
        );

        // 2. Create string to sign
        let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, self.region, service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            Self::sha256_hash(canonical_request.as_bytes())
        );

        // 3. Calculate signature
        let k_date = Self::hmac_sha256(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = Self::hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = Self::hmac_sha256(&k_region, service.as_bytes());
        let k_signing = Self::hmac_sha256(&k_service, b"aws4_request");
        let signature = hex::encode(Self::hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        // 4. Create authorization header
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key_id, credential_scope, signed_headers, signature
        );

        Ok(authorization)
    }

    /// Call STS GetCallerIdentity API
    fn call_sts_get_caller_identity(&self) -> Result<StsCallerIdentity> {
        let timestamp = Utc::now();
        let service = "sts";
        let host = format!("sts.{}.amazonaws.com", self.region);
        let uri = "/";
        let query_string = "Action=GetCallerIdentity&Version=2011-06-15";

        let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let payload_hash = Self::sha256_hash(b"");

        let authorization =
            self.sign_request("GET", service, &host, uri, query_string, &[], "", timestamp)?;

        let url = format!("https://{}{}?{}", host, uri, query_string);

        let response = ureq::get(&url)
            .header("Authorization", &authorization)
            .header("X-Amz-Date", &amz_date)
            .header("X-Amz-Content-Sha256", &payload_hash)
            .header("Host", &host)
            .call()
            .map_err(|e| anyhow!("STS request failed: {}", e))?;

        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;

        // Parse XML response
        parse_sts_response(&body)
    }

    /// Ask Cost Explorer for one time range and return the response body
    /// unchanged.
    ///
    /// The only place in this file that talks to Cost Explorer. Each call
    /// is billed, so callers ask for everything they need in one request.
    ///
    /// Note: the Cost Explorer endpoint only exists in us-east-1.
    fn cost_and_usage_raw(&self, request: &serde_json::Value) -> Result<String> {
        let timestamp = Utc::now();
        let service = "ce";
        let ce_region = "us-east-1";
        let host = format!("ce.{}.amazonaws.com", ce_region);
        let uri = "/";

        let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let payload = serde_json::to_string(request)?;
        let payload_hash = Self::sha256_hash(payload.as_bytes());

        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-amz-json-1.1".to_string(),
            ),
            (
                "x-amz-target".to_string(),
                "AWSInsightsIndexService.GetCostAndUsage".to_string(),
            ),
        ];

        let authorization = self.sign_request_with_region(
            "POST", service, ce_region, &host, uri, "", &headers, &payload, timestamp,
        )?;

        let url = format!("https://{}{}", host, uri);

        // Do not treat a 4xx/5xx as a transport error, so the response body
        // makes it into the log — Cost Explorer explains itself there.
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .new_agent();

        tracing::debug!("Sending Cost Explorer request: {}", url);

        let response = agent
            .post(&url)
            .header("Authorization", &authorization)
            .header("X-Amz-Date", &amz_date)
            .header("X-Amz-Content-Sha256", &payload_hash)
            .header("Host", &host)
            .header("Content-Type", "application/x-amz-json-1.1")
            .header("X-Amz-Target", "AWSInsightsIndexService.GetCostAndUsage")
            .send(&payload)
            .map_err(|e| {
                tracing::error!("Cost Explorer request error details: {:?}", e);
                anyhow!("Cost Explorer request failed: {}", e)
            })?;

        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;

        if status >= 400 {
            tracing::error!("Cost Explorer error response (HTTP {}): {}", status, body);
            return Err(anyhow!(
                "Cost Explorer request failed: HTTP {} - {}",
                status,
                body
            ));
        }

        Ok(body)
    }

    /// Call Cost Explorer API
    fn call_cost_explorer(&self, start_date: &str, end_date: &str) -> Result<Vec<CostData>> {
        let body = self.cost_and_usage_raw(&cost_and_usage_request(start_date, end_date, true))?;
        parse_cost_explorer_response(&body, &self.account_id, &self.account_name)
    }

    /// Call Cost Explorer API to get daily costs (not grouped by service, for trend charts)
    fn call_cost_explorer_daily(&self, start_date: &str, end_date: &str) -> Result<Vec<CostData>> {
        let body = self.cost_and_usage_raw(&cost_and_usage_request(start_date, end_date, false))?;
        parse_daily_cost_response(&body, &self.account_id)
    }

    /// Sign with specified region (for services like Cost Explorer that are only available in specific regions)
    #[allow(clippy::too_many_arguments)]
    fn sign_request_with_region(
        &self,
        method: &str,
        service: &str,
        region: &str,
        host: &str,
        uri: &str,
        query_string: &str,
        headers: &[(String, String)],
        payload: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<String> {
        let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = timestamp.format("%Y%m%d").to_string();

        // 1. Create canonical request
        let payload_hash = Self::sha256_hash(payload.as_bytes());

        // Collect all headers (including host and x-amz-date)
        let mut all_headers: Vec<(String, String)> = headers.to_vec();
        all_headers.push(("host".to_string(), host.to_string()));
        all_headers.push(("x-amz-date".to_string(), amz_date.clone()));
        all_headers.push(("x-amz-content-sha256".to_string(), payload_hash.clone()));

        // Sort by lowercase key
        all_headers.sort_by_key(|(name, _)| name.to_lowercase());

        let canonical_headers: String = all_headers
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k.to_lowercase(), v.trim()))
            .collect();

        let signed_headers: String = all_headers
            .iter()
            .map(|(k, _)| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method, uri, query_string, canonical_headers, signed_headers, payload_hash
        );

        // 2. Create string to sign - use the passed region instead of self.region
        let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            Self::sha256_hash(canonical_request.as_bytes())
        );

        // 3. Calculate signature - use the passed region
        let k_date = Self::hmac_sha256(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = Self::hmac_sha256(&k_date, region.as_bytes());
        let k_service = Self::hmac_sha256(&k_region, service.as_bytes());
        let k_signing = Self::hmac_sha256(&k_service, b"aws4_request");
        let signature = hex::encode(Self::hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        // 4. Create authorization header
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key_id, credential_scope, signed_headers, signature
        );

        Ok(authorization)
    }
}

/// Name the Cost Explorer payload is stored under in a raw batch.
const PART_COST_AND_USAGE: &str = "cost_and_usage";

/// The GetCostAndUsage request body.
///
/// `group_by_service` off is the trend query: one total per day, which is
/// a cheaper response than summing the grouped one client-side.
fn cost_and_usage_request(
    start_date: &str,
    end_date: &str,
    group_by_service: bool,
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "TimePeriod": {
            "Start": start_date,
            "End": end_date
        },
        "Granularity": "DAILY",
        "Metrics": ["UnblendedCost"]
    });

    if group_by_service {
        request["GroupBy"] = serde_json::json!([{
            "Type": "DIMENSION",
            "Key": "SERVICE"
        }]);
    }

    request
}

/// Turn a fetched Cost Explorer payload into ledger rows.
///
/// Pure — every input is in `batch`. Cost Explorer reports `UnblendedCost`,
/// which is what was actually charged, so `cost_basis` is `authoritative`
/// and `effective_cost` stays empty until PR4 also requests
/// `AmortizedCost`. Every row is `Usage` for the same reason: without
/// `RECORD_TYPE` in the grouping the payload cannot tell a credit from a
/// charge, and guessing would put refunds on the wrong side of the total.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    #[derive(Deserialize)]
    struct CeResponse {
        #[serde(rename = "ResultsByTime")]
        results_by_time: Option<Vec<TimeResult>>,
    }

    #[derive(Deserialize)]
    struct TimeResult {
        #[serde(rename = "TimePeriod")]
        time_period: TimePeriod,
        #[serde(rename = "Groups")]
        groups: Option<Vec<CostGroup>>,
    }

    #[derive(Deserialize)]
    struct TimePeriod {
        #[serde(rename = "Start")]
        start: String,
        #[serde(rename = "End")]
        end: String,
    }

    #[derive(Deserialize)]
    struct CostGroup {
        #[serde(rename = "Keys")]
        keys: Vec<String>,
        #[serde(rename = "Metrics")]
        metrics: CostMetrics,
    }

    #[derive(Deserialize)]
    struct CostMetrics {
        #[serde(rename = "UnblendedCost")]
        unblended_cost: CostAmount,
    }

    #[derive(Deserialize)]
    struct CostAmount {
        #[serde(rename = "Amount")]
        amount: String,
        #[serde(rename = "Unit")]
        unit: String,
    }

    let part = batch
        .part(PART_COST_AND_USAGE)
        .ok_or_else(|| anyhow!("Raw batch has no '{}' payload", PART_COST_AND_USAGE))?;
    let response: CeResponse = serde_json::from_str(&part.body)
        .map_err(|e| anyhow!("Failed to parse Cost Explorer payload: {}", e))?;

    let mut charges = Vec::new();
    for result in response.results_by_time.unwrap_or_default() {
        let start = parse_day(&result.time_period.start)?;
        let end = parse_day(&result.time_period.end)?;

        for group in result.groups.unwrap_or_default() {
            let amount: f64 = group.metrics.unblended_cost.amount.parse().unwrap_or(0.0);
            // Cost Explorer returns a row for every service in the account,
            // most of them zero. They carry no information and would bloat
            // the fact table by an order of magnitude.
            if amount == 0.0 {
                continue;
            }

            charges.push(Charge {
                service_name: group.keys.first().cloned(),
                billed_cost: Some(amount),
                ..Charge::new(start, end, group.metrics.unblended_cost.unit)
            });
        }
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

/// Parse a Cost Explorer `YYYY-MM-DD` into an instant at UTC midnight.
fn parse_day(date: &str) -> Result<DateTime<Utc>> {
    let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|e| anyhow!("Unexpected Cost Explorer date {:?}: {}", date, e))?;
    Ok(day.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc())
}

/// STS Caller Identity
#[derive(Debug)]
struct StsCallerIdentity {
    account: String,
    arn: String,
    #[allow(dead_code)]
    user_id: String,
}

/// Parse STS GetCallerIdentity XML response
fn parse_sts_response(xml: &str) -> Result<StsCallerIdentity> {
    // Simple XML parsing (avoid additional dependencies)
    let extract = |tag: &str| -> Option<String> {
        let start_tag = format!("<{}>", tag);
        let end_tag = format!("</{}>", tag);
        let start = xml.find(&start_tag)? + start_tag.len();
        let end = xml.find(&end_tag)?;
        Some(xml[start..end].to_string())
    };

    // Check for errors
    if xml.contains("<Error>") {
        let code = extract("Code").unwrap_or_else(|| "Unknown".to_string());
        let message = extract("Message").unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow!("AWS STS error: {} - {}", code, message));
    }

    Ok(StsCallerIdentity {
        account: extract("Account").unwrap_or_default(),
        arn: extract("Arn").unwrap_or_default(),
        user_id: extract("UserId").unwrap_or_default(),
    })
}

/// Parse Cost Explorer JSON response
fn parse_cost_explorer_response(
    json: &str,
    account_id: &str,
    _account_name: &str,
) -> Result<Vec<CostData>> {
    #[derive(Deserialize)]
    struct CeResponse {
        #[serde(rename = "ResultsByTime")]
        results_by_time: Option<Vec<TimeResult>>,
    }

    #[derive(Deserialize)]
    struct TimeResult {
        #[serde(rename = "TimePeriod")]
        time_period: TimePeriod,
        #[serde(rename = "Groups")]
        groups: Option<Vec<CostGroup>>,
    }

    #[derive(Deserialize)]
    struct TimePeriod {
        #[serde(rename = "Start")]
        start: String,
    }

    #[derive(Deserialize)]
    struct CostGroup {
        #[serde(rename = "Keys")]
        keys: Vec<String>,
        #[serde(rename = "Metrics")]
        metrics: CostMetrics,
    }

    #[derive(Deserialize)]
    struct CostMetrics {
        #[serde(rename = "UnblendedCost")]
        unblended_cost: CostAmount,
    }

    #[derive(Deserialize)]
    struct CostAmount {
        #[serde(rename = "Amount")]
        amount: String,
        #[serde(rename = "Unit")]
        unit: String,
    }

    let response: CeResponse = serde_json::from_str(json)?;

    let mut cost_data = Vec::new();
    if let Some(results) = response.results_by_time {
        tracing::info!(
            "Cost Explorer returned data for {} time periods",
            results.len()
        );
        for result in results {
            if let Some(groups) = result.groups {
                for group in groups {
                    let service_name = group.keys.first().cloned().unwrap_or_default();
                    let amount: f64 = group.metrics.unblended_cost.amount.parse().unwrap_or(0.0);
                    let currency = group.metrics.unblended_cost.unit;

                    if amount > 0.0 {
                        tracing::debug!("Service {}: {} {}", service_name, amount, currency);
                        cost_data.push(CostData {
                            account_id: account_id.to_string(),
                            date: result.time_period.start.clone(),
                            service: service_name,
                            amount,
                            currency,
                        });
                    }
                }
            }
        }
    }

    tracing::info!("Parsed {} cost data records", cost_data.len());
    Ok(cost_data)
}

/// Parse Cost Explorer daily cost response (not grouped by service)
fn parse_daily_cost_response(json: &str, account_id: &str) -> Result<Vec<CostData>> {
    #[derive(Deserialize)]
    struct CeResponse {
        #[serde(rename = "ResultsByTime")]
        results_by_time: Option<Vec<TimeResult>>,
    }

    #[derive(Deserialize)]
    struct TimeResult {
        #[serde(rename = "TimePeriod")]
        time_period: TimePeriod,
        #[serde(rename = "Total")]
        total: Option<CostMetrics>,
    }

    #[derive(Deserialize)]
    struct TimePeriod {
        #[serde(rename = "Start")]
        start: String,
    }

    #[derive(Deserialize)]
    struct CostMetrics {
        #[serde(rename = "UnblendedCost")]
        unblended_cost: CostAmount,
    }

    #[derive(Deserialize)]
    struct CostAmount {
        #[serde(rename = "Amount")]
        amount: String,
        #[serde(rename = "Unit")]
        unit: String,
    }

    let response: CeResponse = serde_json::from_str(json)?;

    let mut cost_data = Vec::new();
    if let Some(results) = response.results_by_time {
        tracing::debug!(
            "Daily cost response returned {} time periods",
            results.len()
        );
        for result in results {
            if let Some(total) = result.total {
                let amount: f64 = total.unblended_cost.amount.parse().unwrap_or(0.0);
                let currency = total.unblended_cost.unit;

                cost_data.push(CostData {
                    account_id: account_id.to_string(),
                    date: result.time_period.start.clone(),
                    service: "Total".to_string(),
                    amount,
                    currency,
                });
            }
        }
    }

    tracing::debug!("Parsed {} daily cost data records", cost_data.len());
    Ok(cost_data)
}

impl BillingSource for AwsCloudService {
    fn validate_credentials(&self) -> Result<bool> {
        match self.call_sts_get_caller_identity() {
            Ok(identity) => {
                tracing::info!(
                    "AWS credential validation successful: Account={}, Arn={}",
                    identity.account,
                    identity.arn
                );
                Ok(true)
            }
            Err(e) => {
                tracing::error!("AWS credential validation failed: {}", e);
                Err(e)
            }
        }
    }

    fn fetch(&self, period: &BillingPeriod) -> Result<Vec<RawPart>> {
        let request = cost_and_usage_request(
            &period.start().to_string(),
            &period.end_exclusive().to_string(),
            true,
        );
        let body = self.cost_and_usage_raw(&request)?;

        Ok(vec![RawPart::new(
            PART_COST_AND_USAGE,
            serde_json::to_string(&request)?,
            body,
        )])
    }

    fn normalize(&self, batch: &RawBatch) -> Result<Normalized> {
        normalize(batch)
    }

    fn get_cost_data(&self, start_date: &str, end_date: &str) -> Result<Vec<CostData>> {
        self.call_cost_explorer(start_date, end_date)
    }

    fn get_cost_summary(&self) -> Result<CostSummary> {
        let now = Utc::now();
        let current_month_start = format!("{}-{:02}-01", now.year(), now.month());
        let current_month_end = format!("{}-{:02}-{:02}", now.year(), now.month(), now.day());

        // Last month
        let last_month = if now.month() == 1 {
            chrono::NaiveDate::from_ymd_opt(now.year() - 1, 12, 1).unwrap()
        } else {
            chrono::NaiveDate::from_ymd_opt(now.year(), now.month() - 1, 1).unwrap()
        };
        let last_month_start = format!("{}-{:02}-01", last_month.year(), last_month.month());
        let last_month_end = current_month_start.clone();

        // Get current month costs
        let current_costs = self.get_cost_data(&current_month_start, &current_month_end)?;
        let current_month_cost: f64 = current_costs.iter().map(|c| c.amount).sum();
        tracing::info!(
            "Current month cost: {} USD ({} records)",
            current_month_cost,
            current_costs.len()
        );

        // Get last month costs
        let last_costs = self.get_cost_data(&last_month_start, &last_month_end)?;
        let last_month_cost: f64 = last_costs.iter().map(|c| c.amount).sum();
        tracing::info!(
            "Last month cost: {} USD ({} records)",
            last_month_cost,
            last_costs.len()
        );

        // Calculate month-over-month change
        let month_over_month_change = if last_month_cost > 0.0 {
            ((current_month_cost - last_month_cost) / last_month_cost) * 100.0
        } else {
            0.0
        };

        let currency = current_costs
            .first()
            .map(|c| c.currency.clone())
            .unwrap_or_else(|| "USD".to_string());

        // Aggregate current month costs by service
        let current_month_details = aggregate_costs_by_service(&current_costs);
        // Aggregate last month costs by service
        let last_month_details = aggregate_costs_by_service(&last_costs);

        Ok(CostSummary {
            account_id: self.account_id.clone(),
            account_name: self.account_name.clone(),
            source_id: SourceId::from("AWS"),
            current_month_cost,
            last_month_cost,
            currency,
            month_over_month_change,
            current_month_details,
            last_month_details,
        })
    }

    fn get_cost_trend(&self, start_date: &str, end_date: &str) -> Result<super::CostTrend> {
        tracing::info!("Getting cost trend: {} to {}", start_date, end_date);

        // Call Cost Explorer API to get daily costs
        let cost_data = self.call_cost_explorer_daily(start_date, end_date)?;

        // Aggregate daily costs
        let (daily_costs, currency) = aggregate_daily_costs(&cost_data);

        Ok(super::CostTrend {
            account_id: self.account_id.clone(),
            currency,
            daily_costs,
        })
    }
}

/// Aggregate cost data by service
fn aggregate_costs_by_service(costs: &[CostData]) -> Vec<super::ServiceCost> {
    use std::collections::HashMap;

    let mut service_map: HashMap<String, f64> = HashMap::new();
    let mut currency = "USD".to_string();

    for cost in costs {
        *service_map.entry(cost.service.clone()).or_insert(0.0) += cost.amount;
        currency = cost.currency.clone();
    }

    let mut result: Vec<super::ServiceCost> = service_map
        .into_iter()
        .map(|(service, amount)| super::ServiceCost {
            service,
            amount,
            currency: currency.clone(),
        })
        .collect();

    // Sort by amount in descending order
    result.sort_by(|a, b| {
        b.amount
            .partial_cmp(&a.amount)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    result
}

/// Aggregate daily costs by date, returns (daily cost list, currency)
fn aggregate_daily_costs(costs: &[CostData]) -> (Vec<super::DailyCost>, String) {
    use std::collections::HashMap;

    let mut date_map: HashMap<String, f64> = HashMap::new();
    let mut currency = "USD".to_string();

    for cost in costs {
        *date_map.entry(cost.date.clone()).or_insert(0.0) += cost.amount;
        currency = cost.currency.clone();
    }

    let mut result: Vec<super::DailyCost> = date_map
        .into_iter()
        .map(|(date, amount)| super::DailyCost { date, amount })
        .collect();

    // Sort by date in ascending order
    result.sort_by(|a, b| a.date.cmp(&b.date));

    (result, currency)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{ChargeCategory, CostBasis};

    /// One recorded GetCostAndUsage response, DAILY and grouped by SERVICE.
    const COST_AND_USAGE: &str = include_str!("testdata/aws_cost_and_usage.json");

    fn recorded_batch(body: &str) -> RawBatch {
        RawBatch {
            provider: "AWS".to_string(),
            account_id: "acct-1".to_string(),
            period: BillingPeriod::new(2026, 8),
            batch_id: "b-1".to_string(),
            fetched_at: "2026-08-03T04:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART_COST_AND_USAGE, "{}", body)],
        }
    }

    #[test]
    fn test_sha256_hash() {
        let hash = AwsCloudService::sha256_hash(b"test");
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA256 produces 32 bytes = 64 hex characters
    }

    #[test]
    fn a_recorded_response_normalizes_to_one_charge_per_service_day() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();

        // Three non-zero groups across two days; the zero-cost KMS row is
        // dropped.
        assert_eq!(normalized.charges.len(), 3);
        assert!(normalized.balances.is_empty());

        let first = &normalized.charges[0];
        assert_eq!(
            first.service_name.as_deref(),
            Some("Amazon Elastic Compute Cloud - Compute")
        );
        assert_eq!(first.billed_cost, Some(12.45));
        assert_eq!(first.billing_currency, "USD");
        assert_eq!(first.charge_category, ChargeCategory::Usage);
        assert_eq!(first.cost_basis, CostBasis::Authoritative);
        assert_eq!(
            first.charge_period_start.to_rfc3339(),
            "2026-08-01T00:00:00+00:00"
        );
        assert_eq!(
            first.charge_period_end.to_rfc3339(),
            "2026-08-02T00:00:00+00:00"
        );

        // Amortization needs AmortizedCost, which this request does not ask
        // for: the column stays empty rather than being filled with the
        // unblended figure.
        assert_eq!(first.effective_cost, None);

        let total: f64 = normalized
            .charges
            .iter()
            .filter_map(|charge| charge.billed_cost)
            .sum();
        assert_eq!(total, 25.1);
    }

    #[test]
    fn normalizing_is_not_affected_by_when_it_runs() {
        let batch = recorded_batch(COST_AND_USAGE);
        let mut later = batch.clone();
        later.fetched_at = "2027-01-01T00:00:00Z".parse().unwrap();
        later.batch_id = "b-2".to_string();

        let first = normalize(&batch).unwrap();
        let second = normalize(&later).unwrap();

        assert_eq!(first.charges.len(), second.charges.len());
        for (a, b) in first.charges.iter().zip(&second.charges) {
            assert_eq!(a.billed_cost, b.billed_cost);
            assert_eq!(a.charge_period_start, b.charge_period_start);
            assert_eq!(a.service_name, b.service_name);
        }
    }

    #[test]
    fn a_batch_without_the_expected_payload_is_an_error() {
        let mut batch = recorded_batch(COST_AND_USAGE);
        batch.parts.clear();
        assert!(normalize(&batch).is_err());
    }

    #[test]
    fn the_trend_request_asks_for_totals_and_the_summary_request_for_services() {
        let grouped = cost_and_usage_request("2026-08-01", "2026-09-01", true);
        assert_eq!(grouped["GroupBy"][0]["Key"], "SERVICE");
        assert_eq!(grouped["Granularity"], "DAILY");

        let totals = cost_and_usage_request("2026-08-01", "2026-09-01", false);
        assert!(totals.get("GroupBy").is_none());
    }
}
