//! Alibaba Cloud service implementation - using ureq + Alibaba Cloud signature

use anyhow::{anyhow, Result};
use chrono::{Datelike, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha1::Sha1;
use std::collections::BTreeMap;

use super::raw::RawPart;
use super::{BillingPeriod, BillingSource, Normalized, RawBatch};
use crate::ledger::{Charge, ChargeCategory};

type HmacSha1 = Hmac<Sha1>;

/// Alibaba Cloud service
pub struct AliyunCloudService {
    access_key_id: String,
    access_key_secret: String,
}

impl AliyunCloudService {
    pub fn new(access_key_id: String, access_key_secret: String, _region: Option<String>) -> Self {
        Self {
            access_key_id,
            access_key_secret,
        }
    }

    /// Calculate HMAC-SHA1 and return Base64 encoded result
    fn hmac_sha1_base64(key: &str, data: &str) -> String {
        let mut mac =
            HmacSha1::new_from_slice(key.as_bytes()).expect("HMAC can take key of any size");
        mac.update(data.as_bytes());
        let result = mac.finalize().into_bytes();
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, result)
    }

    /// URL encoding (Alibaba Cloud's special encoding requirements)
    fn percent_encode(s: &str) -> String {
        let mut result = String::new();
        for c in s.chars() {
            match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                    result.push(c);
                }
                _ => {
                    for byte in c.to_string().as_bytes() {
                        result.push_str(&format!("%{:02X}", byte));
                    }
                }
            }
        }
        result
    }

    /// Generate Alibaba Cloud signature V1
    fn sign_request(&self, params: &BTreeMap<String, String>) -> String {
        // 1. Build canonical query string
        let canonical_query: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", Self::percent_encode(k), Self::percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        // 2. Build string to sign
        let string_to_sign = format!(
            "GET&{}&{}",
            Self::percent_encode("/"),
            Self::percent_encode(&canonical_query)
        );

        // 3. Calculate signature (key needs trailing &)
        let sign_key = format!("{}&", self.access_key_secret);
        Self::hmac_sha1_base64(&sign_key, &string_to_sign)
    }

    /// Generate common request parameters
    fn common_params(&self, action: &str) -> BTreeMap<String, String> {
        let mut params = BTreeMap::new();
        params.insert("Format".to_string(), "JSON".to_string());
        params.insert("Version".to_string(), "2017-12-14".to_string());
        params.insert("AccessKeyId".to_string(), self.access_key_id.clone());
        params.insert("SignatureMethod".to_string(), "HMAC-SHA1".to_string());
        params.insert(
            "Timestamp".to_string(),
            Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        );
        params.insert("SignatureVersion".to_string(), "1.0".to_string());
        params.insert(
            "SignatureNonce".to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        params.insert("Action".to_string(), action.to_string());
        params
    }

    /// Call Alibaba Cloud BSS API
    fn call_bss_api(&self, action: &str, extra_params: &[(&str, &str)]) -> Result<String> {
        let mut params = self.common_params(action);

        for (k, v) in extra_params {
            params.insert(k.to_string(), v.to_string());
        }

        // Calculate signature
        let signature = self.sign_request(&params);
        params.insert("Signature".to_string(), signature);

        // Build URL
        let query: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", Self::percent_encode(k), Self::percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("https://business.aliyuncs.com/?{}", query);

        // Send request
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .new_agent();

        let response = agent
            .get(&url)
            .call()
            .map_err(|e| anyhow!("Alibaba Cloud API request failed: {}", e))?;

        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;

        // Always print response body for debugging
        tracing::debug!("Alibaba Cloud API response: HTTP {}", status);

        if status >= 400 {
            tracing::error!("Alibaba Cloud API error (HTTP {}): {}", status, body);
            return Err(anyhow!(
                "Alibaba Cloud API request failed: HTTP {} - {}",
                status,
                body
            ));
        }

        // Check for business errors - Note: Alibaba Cloud returns "Success" as code on success
        if let Ok(error) = serde_json::from_str::<AliyunErrorResponse>(&body) {
            if let Some(ref code) = error.code {
                // Only treat as error when code is not "Success"
                if code != "Success" {
                    let msg = error.message.clone().unwrap_or_default();
                    tracing::error!("Alibaba Cloud business error: {} - {}", code, msg);
                    return Err(anyhow!("Alibaba Cloud API error: {} - {}", code, msg));
                }
            }
        }

        Ok(body)
    }

    /// Query bill overview
    fn query_bill_overview(&self, billing_cycle: &str) -> Result<BillOverviewResponse> {
        let body = self.call_bss_api("QueryBillOverview", &[("BillingCycle", billing_cycle)])?;

        serde_json::from_str(&body)
            .map_err(|e| anyhow!("Failed to parse bill overview: {} - {}", e, body))
    }
}

impl BillingSource for AliyunCloudService {
    fn validate_credentials(&self) -> Result<bool> {
        // Try calling a simple API to validate credentials
        let now = Utc::now();
        let billing_cycle = format!("{}-{:02}", now.year(), now.month());

        match self.query_bill_overview(&billing_cycle) {
            Ok(_) => Ok(true),
            Err(e) => {
                tracing::error!("Alibaba Cloud credential validation failed: {}", e);
                Ok(false)
            }
        }
    }

    fn fetch(&self, period: &BillingPeriod) -> Result<Vec<RawPart>> {
        let billing_cycle = period.label();
        let body = self.call_bss_api("QueryBillOverview", &[("BillingCycle", &billing_cycle)])?;

        Ok(vec![RawPart::new(
            PART_BILL_OVERVIEW,
            format!("QueryBillOverview BillingCycle={}", billing_cycle),
            body,
        )])
    }

    fn normalize(&self, batch: &RawBatch) -> Result<Normalized> {
        normalize(batch)
    }
}

/// Name the bill overview payload is stored under in a raw batch.
const PART_BILL_OVERVIEW: &str = "bill_overview";

/// Description given to the row that closes the gap when the named
/// deductions do not add up to the difference between gross and net.
const UNRECONCILED: &str = "Unreconciled";

/// Half a fen. Below this the gap is rounding, not a missing deduction.
const RECONCILIATION_TOLERANCE: f64 = 0.005;

/// Turn a fetched `QueryBillOverview` payload into ledger rows.
///
/// Pure — every input is in `batch`. The overview is per product for the
/// whole month, so one row covers the entire billing period;
/// instance-level detail arrives with the bill export channel in P1.
///
/// Each product becomes a `Usage` charge at its **gross** amount plus one
/// `Credit` row per deduction that reduced it. Alibaba Cloud reports both
/// figures on one line, and putting the net amount on the usage row *and*
/// the deductions beside it would count them twice. Decomposed this way
/// the rows sum to `PretaxAmount` — what was actually charged — while
/// still saying what the discount was worth and where it came from.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    let part = batch
        .part(PART_BILL_OVERVIEW)
        .ok_or_else(|| anyhow!("Raw batch has no '{}' payload", PART_BILL_OVERVIEW))?;
    let response: BillOverviewResponse = serde_json::from_str(&part.body)
        .map_err(|e| anyhow!("Failed to parse bill overview: {}", e))?;

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

    let items = response
        .data
        .and_then(|data| data.items)
        .and_then(|items| items.item)
        .unwrap_or_default();

    let mut charges = Vec::new();
    for item in items {
        let net = item.pretax_amount.unwrap_or(0.0);
        let gross = item.pretax_gross_amount.unwrap_or(0.0);
        // A product with nothing on either side of the deductions was not
        // used this month.
        if net == 0.0 && gross == 0.0 {
            continue;
        }

        let currency = item.currency.clone().unwrap_or_else(|| "CNY".to_string());
        let template = || Charge {
            service_name: item.product_name.clone(),
            // ProductCode is stable across locales; ProductName is not.
            service_category: item.product_code.clone(),
            ..Charge::new(start, end, currency.clone())
        };

        charges.push(Charge {
            billed_cost: Some(gross),
            list_cost: Some(gross),
            ..template()
        });

        let mut deducted = 0.0;
        for (name, amount) in item.deductions() {
            if amount == 0.0 {
                continue;
            }
            deducted += amount;
            charges.push(Charge {
                charge_category: ChargeCategory::Credit,
                charge_description: Some(name.to_string()),
                billed_cost: Some(-amount),
                ..template()
            });
        }

        // Anything left between gross, the deductions we know the names of,
        // and the net figure is money the bill accounts for and this parser
        // does not. Recording it keeps the total honest and makes the gap
        // visible instead of losing it.
        let residual = gross - deducted - net;
        if residual.abs() > RECONCILIATION_TOLERANCE {
            tracing::warn!(
                "Alibaba Cloud bill for {} does not reconcile: {:.2} {} unaccounted for",
                item.product_name.as_deref().unwrap_or("?"),
                residual,
                currency
            );
            charges.push(Charge {
                charge_category: ChargeCategory::Adjustment,
                charge_description: Some(UNRECONCILED.to_string()),
                billed_cost: Some(-residual),
                ..template()
            });
        }
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

// ==================== Response Structs ====================
// Note: These fields are used for serde deserialization of Alibaba Cloud API responses.
// Some fields may not be directly read in the code, but are needed for correct JSON parsing.
// Using #[allow(dead_code)] to suppress warnings.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AliyunErrorResponse {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct BillOverviewResponse {
    request_id: Option<String>,
    success: Option<bool>,
    code: Option<String>,
    message: Option<String>,
    data: Option<BillOverviewData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct BillOverviewData {
    billing_cycle: Option<String>,
    account_id: Option<String>,
    account_name: Option<String>,
    items: Option<BillOverviewItems>,
}

/// Alibaba Cloud's Items is an object containing an Item array
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BillOverviewItems {
    item: Option<Vec<BillOverviewItem>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct BillOverviewItem {
    product_code: Option<String>,
    product_name: Option<String>,
    /// `Subscription` (prepaid) or `PayAsYouGo`.
    subscription_type: Option<String>,
    /// What was charged, after every deduction below.
    pretax_amount: Option<f64>,
    /// What it would have cost before any of them.
    #[serde(rename = "PretaxGrossAmount")]
    pretax_gross_amount: Option<f64>,
    /// Negotiated or activity discount.
    invoice_discount: Option<f64>,
    /// Coupons (代金券), cash coupons and stored-value cards.
    deducted_by_coupons: Option<f64>,
    deducted_by_cash_coupons: Option<f64>,
    deducted_by_prepaid_card: Option<f64>,
    currency: Option<String>,
}

impl BillOverviewItem {
    /// The deductions between the gross and the net amount, each named as
    /// Alibaba Cloud names it.
    fn deductions(&self) -> [(&'static str, f64); 4] {
        [
            ("InvoiceDiscount", self.invoice_discount.unwrap_or(0.0)),
            ("DeductedByCoupons", self.deducted_by_coupons.unwrap_or(0.0)),
            (
                "DeductedByCashCoupons",
                self.deducted_by_cash_coupons.unwrap_or(0.0),
            ),
            (
                "DeductedByPrepaidCard",
                self.deducted_by_prepaid_card.unwrap_or(0.0),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::CostBasis;

    /// One recorded QueryBillOverview response.
    const BILL_OVERVIEW: &str = include_str!("testdata/aliyun_bill_overview.json");

    fn recorded_batch(body: &str) -> RawBatch {
        RawBatch {
            provider: "Aliyun".to_string(),
            account_id: "acct-2".to_string(),
            period: BillingPeriod::new(2026, 8),
            batch_id: "b-1".to_string(),
            fetched_at: "2026-09-01T02:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART_BILL_OVERVIEW, "", body)],
        }
    }

    /// Rows for one product, in the order the normalizer emitted them.
    fn rows<'a>(normalized: &'a Normalized, product: &str) -> Vec<&'a Charge> {
        normalized
            .charges
            .iter()
            .filter(|charge| charge.service_category.as_deref() == Some(product))
            .collect()
    }

    #[test]
    fn a_product_becomes_a_gross_usage_charge() {
        let normalized = normalize(&recorded_batch(BILL_OVERVIEW)).unwrap();

        let oss = rows(&normalized, "oss");
        assert_eq!(oss.len(), 1, "nothing was deducted from OSS");

        let charge = oss[0];
        assert_eq!(charge.service_name.as_deref(), Some("对象存储 OSS"));
        assert_eq!(charge.billed_cost, Some(42.0));
        assert_eq!(charge.list_cost, Some(42.0));
        assert_eq!(charge.billing_currency, "CNY");
        assert_eq!(charge.charge_category, ChargeCategory::Usage);
        assert_eq!(charge.cost_basis, CostBasis::Authoritative);

        // The unused CDN product is dropped entirely.
        assert!(rows(&normalized, "cdn").is_empty());
    }

    #[test]
    fn each_deduction_becomes_its_own_credit_row() {
        let normalized = normalize(&recorded_batch(BILL_OVERVIEW)).unwrap();
        let ecs = rows(&normalized, "ecs");

        assert_eq!(ecs[0].billed_cost, Some(320.5));
        assert_eq!(ecs[0].charge_category, ChargeCategory::Usage);

        let credits: Vec<(&str, Option<f64>)> = ecs[1..]
            .iter()
            .map(|charge| {
                (
                    charge.charge_description.as_deref().unwrap(),
                    charge.billed_cost,
                )
            })
            .collect();
        assert_eq!(
            credits,
            vec![
                ("InvoiceDiscount", Some(-22.05)),
                ("DeductedByCoupons", Some(-10.0)),
            ]
        );
        assert!(ecs[1..]
            .iter()
            .all(|charge| charge.charge_category == ChargeCategory::Credit));
    }

    #[test]
    fn a_products_rows_sum_to_what_was_actually_charged() {
        let normalized = normalize(&recorded_batch(BILL_OVERVIEW)).unwrap();

        // PretaxAmount for ECS is 288.45, for OSS 42.00, for RDS 62.00.
        for (product, charged) in [("ecs", 288.45), ("oss", 42.0), ("rds", 62.0)] {
            let total: f64 = rows(&normalized, product)
                .iter()
                .filter_map(|charge| charge.billed_cost)
                .sum();
            assert!(
                (total - charged).abs() < 1e-9,
                "{product}: {total} != {charged}"
            );
        }
    }

    #[test]
    fn a_deduction_this_parser_cannot_name_is_still_accounted_for() {
        let normalized = normalize(&recorded_batch(BILL_OVERVIEW)).unwrap();
        let rds = rows(&normalized, "rds");

        // RDS: 100.00 gross, 30.00 off a stored-value card, 62.00 charged.
        // The remaining 8.00 is a deduction under a name this parser does
        // not know; it is recorded rather than dropped.
        let unreconciled = rds
            .iter()
            .find(|charge| charge.charge_description.as_deref() == Some(UNRECONCILED))
            .expect("the gap is recorded");
        assert_eq!(unreconciled.billed_cost, Some(-8.0));
        assert_eq!(unreconciled.charge_category, ChargeCategory::Adjustment);
    }

    #[test]
    fn rounding_does_not_produce_a_reconciliation_row() {
        let normalized = normalize(&recorded_batch(
            r#"{"Code":"Success","Data":{"Items":{"Item":[
                 {"ProductCode":"ecs","ProductName":"ECS","PretaxGrossAmount":10.0,
                  "InvoiceDiscount":0.001,"PretaxAmount":9.999,"Currency":"CNY"}]}}}"#,
        ))
        .unwrap();

        assert!(normalized
            .charges
            .iter()
            .all(|charge| charge.charge_description.as_deref() != Some(UNRECONCILED)));
    }

    #[test]
    fn a_monthly_overview_row_covers_the_whole_period() {
        let normalized = normalize(&recorded_batch(BILL_OVERVIEW)).unwrap();
        let charge = &normalized.charges[0];

        assert_eq!(
            charge.charge_period_start.to_rfc3339(),
            "2026-08-01T00:00:00+00:00"
        );
        assert_eq!(
            charge.charge_period_end.to_rfc3339(),
            "2026-09-01T00:00:00+00:00"
        );
    }

    #[test]
    fn an_empty_bill_normalizes_to_nothing() {
        let normalized = normalize(&recorded_batch(
            r#"{"Code":"Success","Data":{"BillingCycle":"2026-08"}}"#,
        ))
        .unwrap();
        assert!(normalized.charges.is_empty());
    }
}
