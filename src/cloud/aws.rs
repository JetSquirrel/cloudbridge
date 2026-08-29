//! AWS Cloud Service Implementation - Using ureq + AWS Signature V4

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::raw::RawPart;
use super::{BillingPeriod, BillingSource, Normalized, RawBatch};
use crate::ledger::{Charge, ChargeCategory};

type HmacSha256 = Hmac<Sha256>;

/// AWS Cloud Service
pub struct AwsCloudService {
    access_key_id: String,
    secret_access_key: String,
    region: String,
}

impl AwsCloudService {
    pub fn new(access_key_id: String, secret_access_key: String, region: Option<String>) -> Self {
        Self {
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

/// What was actually charged.
const METRIC_UNBLENDED: &str = "UnblendedCost";
/// The same spend with commitment fees spread over the term they cover.
const METRIC_AMORTIZED: &str = "AmortizedCost";
/// How much was consumed, when the grouping leaves one meaningful unit.
const METRIC_USAGE_QUANTITY: &str = "UsageQuantity";

/// Cost Explorer returns this unit when a group mixes usage types, which
/// grouping by service usually does. A quantity in mixed units cannot be
/// added to anything, so it is not stored.
const UNIT_NOT_APPLICABLE: &str = "N/A";

const DIMENSION_SERVICE: &str = "SERVICE";
const DIMENSION_RECORD_TYPE: &str = "RECORD_TYPE";

/// The GetCostAndUsage request the ledger is built from.
///
/// All three metrics ride in one request: Cost Explorer bills per request,
/// not per metric, so splitting them would triple the cost of an ingest
/// for nothing. `RECORD_TYPE` is what makes a credit distinguishable from
/// a charge — without it every line arrives as an unlabelled amount.
fn ledger_request(start_date: &str, end_date: &str) -> serde_json::Value {
    serde_json::json!({
        "TimePeriod": {
            "Start": start_date,
            "End": end_date
        },
        "Granularity": "DAILY",
        "Metrics": [METRIC_UNBLENDED, METRIC_AMORTIZED, METRIC_USAGE_QUANTITY],
        "GroupBy": [
            {
                "Type": "DIMENSION",
                "Key": DIMENSION_SERVICE
            },
            {
                "Type": "DIMENSION",
                "Key": DIMENSION_RECORD_TYPE
            }
        ]
    })
}

/// FOCUS category for an AWS `RECORD_TYPE`.
///
/// Discounts and negations are `Adjustment` rather than `Credit`: they
/// reduce what a charge costs, whereas AWS's own `Credit` record type is a
/// balance applied against the bill. An unrecognized type is also
/// `Adjustment`, and says so in the log — money moved, and filing it as
/// `Usage` would quietly inflate what looks like consumption.
fn charge_category(record_type: &str) -> ChargeCategory {
    match record_type {
        "Usage" | "DiscountedUsage" | "SavingsPlanCoveredUsage" => ChargeCategory::Usage,
        "Credit" => ChargeCategory::Credit,
        "Tax" => ChargeCategory::Tax,
        "Fee" | "RIFee" | "SavingsPlanUpfrontFee" | "SavingsPlanRecurringFee" | "Support" => {
            ChargeCategory::Purchase
        }
        "Refund"
        | "SavingsPlanNegation"
        | "BundledDiscount"
        | "PrivateRateDiscount"
        | "Enterprise Discount Program Discount"
        | "Solution Provider Program Discount" => ChargeCategory::Adjustment,
        other => {
            tracing::warn!(
                "Unrecognized Cost Explorer record type {:?}; filed as an Adjustment",
                other
            );
            ChargeCategory::Adjustment
        }
    }
}

/// Turn a fetched Cost Explorer payload into ledger rows.
///
/// Pure — every input is in `batch`.
///
/// `UnblendedCost` is what was actually charged, so it is `billed_cost`
/// and `cost_basis` is `authoritative`; `AmortizedCost` spreads commitment
/// fees over the term they cover, which is `effective_cost`. Amounts keep
/// the sign Cost Explorer gave them, so a credit stays negative and a
/// total comes out right by summation alone.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    #[derive(Deserialize)]
    struct CeResponse {
        #[serde(rename = "GroupDefinitions")]
        group_definitions: Option<Vec<GroupDefinition>>,
        #[serde(rename = "ResultsByTime")]
        results_by_time: Option<Vec<TimeResult>>,
    }

    #[derive(Deserialize)]
    struct GroupDefinition {
        #[serde(rename = "Key")]
        key: String,
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
        metrics: std::collections::HashMap<String, CostAmount>,
    }

    #[derive(Deserialize)]
    struct CostAmount {
        #[serde(rename = "Amount")]
        amount: String,
        #[serde(rename = "Unit")]
        unit: String,
    }

    impl CostAmount {
        fn value(&self) -> f64 {
            self.amount.parse().unwrap_or(0.0)
        }
    }

    let part = batch
        .part(PART_COST_AND_USAGE)
        .ok_or_else(|| anyhow!("Raw batch has no '{}' payload", PART_COST_AND_USAGE))?;
    let response: CeResponse = serde_json::from_str(&part.body)
        .map_err(|e| anyhow!("Failed to parse Cost Explorer payload: {}", e))?;

    // Which key is which comes from the response itself rather than from
    // the request this build would have sent, so a payload recorded by an
    // older version still normalizes.
    let definitions = response.group_definitions.unwrap_or_default();
    let position = |dimension: &str| definitions.iter().position(|d| d.key == dimension);
    let service_at = position(DIMENSION_SERVICE).unwrap_or(0);
    let record_type_at = position(DIMENSION_RECORD_TYPE);

    let mut charges = Vec::new();
    for result in response.results_by_time.unwrap_or_default() {
        let start = parse_day(&result.time_period.start)?;
        let end = parse_day(&result.time_period.end)?;

        for group in result.groups.unwrap_or_default() {
            let unblended = group.metrics.get(METRIC_UNBLENDED);
            let amortized = group.metrics.get(METRIC_AMORTIZED);
            let billed_cost = unblended.map(CostAmount::value);
            let effective_cost = amortized.map(CostAmount::value);

            // Cost Explorer returns a row for every service in the account,
            // most of them zero on every metric. They carry no information
            // and would bloat the fact table by an order of magnitude. A
            // row that is zero unblended but non-zero amortized — usage a
            // commitment already paid for — is not one of them.
            if billed_cost.unwrap_or(0.0) == 0.0 && effective_cost.unwrap_or(0.0) == 0.0 {
                continue;
            }

            // A quantity is only kept when the group leaves it in one unit.
            let quantity = group
                .metrics
                .get(METRIC_USAGE_QUANTITY)
                .filter(|q| q.unit != UNIT_NOT_APPLICABLE && !q.unit.is_empty());

            // Without RECORD_TYPE in the grouping, credits and refunds are
            // already netted into each service's amount and there is
            // nothing left to label: such a payload is Usage throughout,
            // which is what it was read as before the dimension was added.
            let record_type = record_type_at.and_then(|at| group.keys.get(at));
            let category = record_type.map_or(ChargeCategory::Usage, |rt| charge_category(rt));

            charges.push(Charge {
                service_name: group.keys.get(service_at).cloned(),
                charge_description: record_type.cloned(),
                billed_cost,
                effective_cost,
                pricing_quantity: quantity.map(|q| q.value()),
                pricing_unit: quantity.map(|q| q.unit.clone()),
                charge_category: category,
                ..Charge::new(
                    start,
                    end,
                    unblended
                        .or(amortized)
                        .map_or_else(|| "USD".to_string(), |amount| amount.unit.clone()),
                )
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
        let request = ledger_request(
            &period.start().to_string(),
            &period.end_exclusive().to_string(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{ChargeCategory, CostBasis};

    /// A recorded GetCostAndUsage response as this build asks for it:
    /// three metrics, grouped by service and record type.
    const COST_AND_USAGE: &str = include_str!("testdata/aws_cost_and_usage_record_type.json");

    /// A response recorded before RECORD_TYPE was in the grouping, of the
    /// kind already sitting in the raw store.
    const LEGACY_COST_AND_USAGE: &str = include_str!("testdata/aws_cost_and_usage.json");

    fn charge<'a>(normalized: &'a Normalized, service: &str, description: &str) -> &'a Charge {
        normalized
            .charges
            .iter()
            .find(|charge| {
                charge.service_name.as_deref() == Some(service)
                    && charge.charge_description.as_deref() == Some(description)
            })
            .unwrap_or_else(|| panic!("no {} / {} charge", service, description))
    }

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

        // Eight non-zero groups across two days; the all-zero KMS row is
        // dropped.
        assert_eq!(normalized.charges.len(), 8);
        assert!(normalized.balances.is_empty());

        let ec2 = charge(
            &normalized,
            "Amazon Elastic Compute Cloud - Compute",
            "Usage",
        );
        assert_eq!(ec2.billed_cost, Some(12.45));
        assert_eq!(ec2.effective_cost, Some(10.20));
        assert_eq!(ec2.billing_currency, "USD");
        assert_eq!(ec2.cost_basis, CostBasis::Authoritative);
        assert_eq!(
            ec2.charge_period_start.to_rfc3339(),
            "2026-08-01T00:00:00+00:00"
        );
        assert_eq!(
            ec2.charge_period_end.to_rfc3339(),
            "2026-08-02T00:00:00+00:00"
        );
    }

    #[test]
    fn the_record_type_decides_the_charge_category() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();

        let category = |service: &str, record_type: &str| {
            charge(&normalized, service, record_type).charge_category
        };
        let ec2 = "Amazon Elastic Compute Cloud - Compute";

        assert_eq!(category(ec2, "Usage"), ChargeCategory::Usage);
        assert_eq!(
            category(ec2, "SavingsPlanCoveredUsage"),
            ChargeCategory::Usage
        );
        assert_eq!(category(ec2, "Credit"), ChargeCategory::Credit);
        assert_eq!(category(ec2, "Refund"), ChargeCategory::Adjustment);
        assert_eq!(category("Tax", "Tax"), ChargeCategory::Tax);
        assert_eq!(
            category("AWS Support (Developer)", "Fee"),
            ChargeCategory::Purchase
        );
        // A record type this build has never seen still moved money, so it
        // is kept and labelled as an adjustment rather than as usage.
        assert_eq!(
            category("Amazon Route 53", "SomeFutureRecordType"),
            ChargeCategory::Adjustment
        );
    }

    #[test]
    fn credits_and_refunds_keep_their_sign_so_the_total_nets_out() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();
        let ec2 = "Amazon Elastic Compute Cloud - Compute";

        assert_eq!(charge(&normalized, ec2, "Credit").billed_cost, Some(-3.0));
        assert_eq!(charge(&normalized, ec2, "Refund").billed_cost, Some(-1.5));

        let total: f64 = normalized
            .charges
            .iter()
            .filter_map(|charge| charge.billed_cost)
            .sum();
        // 12.45 + 0 + 0.75 + 29.00 - 3.00 - 1.50 + 2.10 + 1.23
        assert!((total - 41.03).abs() < 1e-9, "got {total}");
    }

    #[test]
    fn usage_a_commitment_already_paid_for_is_not_mistaken_for_an_empty_row() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();
        let covered = charge(
            &normalized,
            "Amazon Elastic Compute Cloud - Compute",
            "SavingsPlanCoveredUsage",
        );

        // Nothing was charged for it this day, but the amortized figure is
        // what the commitment cost — dropping the row would lose it.
        assert_eq!(covered.billed_cost, Some(0.0));
        assert_eq!(covered.effective_cost, Some(3.10));
    }

    #[test]
    fn a_quantity_is_only_kept_when_it_has_one_real_unit() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();

        let ec2 = charge(
            &normalized,
            "Amazon Elastic Compute Cloud - Compute",
            "Usage",
        );
        assert_eq!(ec2.pricing_quantity, Some(24.0));
        assert_eq!(ec2.pricing_unit.as_deref(), Some("Hrs"));

        // Grouping by service mixes usage types, and Cost Explorer says so
        // with "N/A". A number in mixed units cannot be added to anything.
        let s3 = charge(&normalized, "Amazon Simple Storage Service", "Usage");
        assert_eq!(s3.pricing_quantity, None);
        assert_eq!(s3.pricing_unit, None);
    }

    #[test]
    fn a_payload_recorded_before_record_type_still_normalizes() {
        let normalized = normalize(&recorded_batch(LEGACY_COST_AND_USAGE)).unwrap();

        assert_eq!(normalized.charges.len(), 3);
        // Credits were already netted into each service's amount, so there
        // is nothing to label and nothing to amortize.
        assert!(normalized
            .charges
            .iter()
            .all(|charge| charge.charge_category == ChargeCategory::Usage));
        assert!(normalized
            .charges
            .iter()
            .all(|charge| charge.effective_cost.is_none()));
        assert_eq!(normalized.charges[0].billed_cost, Some(12.45));
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
    fn the_ledger_request_carries_every_metric_in_one_call() {
        let request = ledger_request("2026-08-01", "2026-09-01");

        // Cost Explorer bills per request: three metrics, one call.
        let metrics = request["Metrics"].as_array().unwrap();
        assert_eq!(metrics.len(), 3);
        assert!(metrics.iter().any(|m| m == "UnblendedCost"));
        assert!(metrics.iter().any(|m| m == "AmortizedCost"));
        assert!(metrics.iter().any(|m| m == "UsageQuantity"));

        assert_eq!(request["GroupBy"][0]["Key"], "SERVICE");
        assert_eq!(request["GroupBy"][1]["Key"], "RECORD_TYPE");
        assert_eq!(request["Granularity"], "DAILY");
    }
}
