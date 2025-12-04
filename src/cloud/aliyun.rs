//! Alibaba Cloud service implementation - using ureq + Alibaba Cloud signature

use anyhow::{anyhow, Result};
use chrono::{Datelike, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha1::Sha1;
use std::collections::BTreeMap;

use super::{CloudProvider, CloudService, CostData, CostSummary, ServiceCost};

type HmacSha1 = Hmac<Sha1>;

/// Alibaba Cloud service
pub struct AliyunCloudService {
    account_id: String,
    account_name: String,
    access_key_id: String,
    access_key_secret: String,
}

impl AliyunCloudService {
    pub fn new(
        account_id: String,
        account_name: String,
        access_key_id: String,
        access_key_secret: String,
        _region: Option<String>,
    ) -> Self {
        Self {
            account_id,
            account_name,
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

    /// Query instance bill (daily details)
    fn describe_instance_bill(
        &self,
        billing_cycle: &str,
        granularity: &str,
    ) -> Result<InstanceBillResponse> {
        let body = self.call_bss_api(
            "DescribeInstanceBill",
            &[
                ("BillingCycle", billing_cycle),
                ("Granularity", granularity), // DAILY or MONTHLY
                ("MaxResults", "300"),
            ],
        )?;

        serde_json::from_str(&body)
            .map_err(|e| anyhow!("Failed to parse instance bill: {} - {}", e, body))
    }

    /// Query instance bill for a specific date (daily granularity requires BillingDate)
    fn describe_instance_bill_by_date(
        &self,
        billing_cycle: &str,
        billing_date: &str,
    ) -> Result<InstanceBillResponse> {
        let body = self.call_bss_api(
            "DescribeInstanceBill",
            &[
                ("BillingCycle", billing_cycle),
                ("BillingDate", billing_date),
                ("Granularity", "DAILY"),
                ("MaxResults", "300"),
            ],
        )?;

        serde_json::from_str(&body)
            .map_err(|e| anyhow!("Failed to parse instance bill: {} - {}", e, body))
    }
}

impl CloudService for AliyunCloudService {
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

    fn get_cost_data(&self, start_date: &str, end_date: &str) -> Result<Vec<CostData>> {
        // Alibaba Cloud queries by month, extract year-month
        let billing_cycle = &start_date[..7]; // YYYY-MM

        let response = self.describe_instance_bill(billing_cycle, "DAILY")?;

        let mut costs = Vec::new();
        if let Some(items) = response.data.and_then(|d| d.items) {
            for item in items {
                let date = item.billing_date.unwrap_or_default();
                // Filter by date range
                if date.as_str() >= start_date && date.as_str() <= end_date {
                    costs.push(CostData {
                        account_id: self.account_id.clone(),
                        date,
                        service: item.product_name.unwrap_or_else(|| "Unknown".to_string()),
                        amount: item.pretax_amount.unwrap_or(0.0),
                        currency: "CNY".to_string(),
                    });
                }
            }
        }

        Ok(costs)
    }

    fn get_cost_summary(&self) -> Result<CostSummary> {
        let now = Utc::now();

        // Current month
        let current_month = format!("{}-{:02}", now.year(), now.month());
        // Last month
        let last_month_date = now - chrono::Duration::days(now.day() as i64 + 1);
        let last_month = format!("{}-{:02}", last_month_date.year(), last_month_date.month());

        // Query current month bill overview
        let current_overview = self.query_bill_overview(&current_month)?;
        let last_overview = self.query_bill_overview(&last_month)?;

        // Parse current month costs
        let (current_month_cost, current_month_details) = parse_bill_overview(&current_overview);
        let (last_month_cost, last_month_details) = parse_bill_overview(&last_overview);

        // Calculate month-over-month change
        let month_over_month_change = if last_month_cost > 0.0 {
            ((current_month_cost - last_month_cost) / last_month_cost) * 100.0
        } else if current_month_cost > 0.0 {
            100.0
        } else {
            0.0
        };

        Ok(CostSummary {
            account_id: self.account_id.clone(),
            account_name: self.account_name.clone(),
            provider: CloudProvider::Aliyun,
            current_month_cost,
            last_month_cost,
            currency: "CNY".to_string(),
            month_over_month_change,
            current_month_details,
            last_month_details,
        })
    }

    fn get_cost_trend(&self, start_date: &str, end_date: &str) -> Result<super::CostTrend> {
        // Aggregate costs by date
        let mut daily_map: std::collections::HashMap<String, f64> =
            std::collections::HashMap::new();

        // Use chrono to iterate through each day in the date range
        use chrono::NaiveDate;

        let start = NaiveDate::parse_from_str(start_date, "%Y-%m-%d")
            .map_err(|e| anyhow!("Invalid start date: {}", e))?;
        let end = NaiveDate::parse_from_str(end_date, "%Y-%m-%d")
            .map_err(|e| anyhow!("Invalid end date: {}", e))?;

        let mut current = start;
        while current < end {
            let date_str = current.format("%Y-%m-%d").to_string();
            let billing_cycle = current.format("%Y-%m").to_string();

            match self.describe_instance_bill_by_date(&billing_cycle, &date_str) {
                Ok(response) => {
                    if let Some(items) = response.data.and_then(|d| d.items) {
                        let mut day_total = 0.0;
                        for item in items {
                            let amount = item.pretax_amount.unwrap_or(0.0);
                            day_total += amount;
                        }
                        if day_total > 0.0 {
                            daily_map.insert(date_str.clone(), day_total);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to query bill for {}: {}", date_str, e);
                }
            }

            current += chrono::Duration::days(1);
        }

        // Convert to sorted list
        let mut daily_costs: Vec<super::DailyCost> = daily_map
            .into_iter()
            .map(|(date, amount)| super::DailyCost { date, amount })
            .collect();

        daily_costs.sort_by(|a, b| a.date.cmp(&b.date));

        Ok(super::CostTrend {
            account_id: self.account_id.clone(),
            currency: "CNY".to_string(),
            daily_costs,
        })
    }
}

/// Parse bill overview
fn parse_bill_overview(response: &BillOverviewResponse) -> (f64, Vec<ServiceCost>) {
    let mut total_cost = 0.0;
    let mut details = Vec::new();

    if let Some(data) = &response.data {
        if let Some(items_wrapper) = &data.items {
            if let Some(items) = &items_wrapper.item {
                for item in items {
                    let amount = item.pretax_amount.unwrap_or(0.0);
                    total_cost += amount;

                    if amount > 0.0 {
                        details.push(ServiceCost {
                            service: item
                                .product_name
                                .clone()
                                .unwrap_or_else(|| "Unknown".to_string()),
                            amount,
                            currency: "CNY".to_string(),
                        });
                    }
                }
            }
        }
    }

    // Sort by amount in descending order
    details.sort_by(|a, b| {
        b.amount
            .partial_cmp(&a.amount)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    (total_cost, details)
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
    pretax_amount: Option<f64>,
    #[serde(rename = "PretaxGrossAmount")]
    pretax_gross_amount: Option<f64>,
    currency: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct InstanceBillResponse {
    request_id: Option<String>,
    success: Option<bool>,
    code: Option<String>,
    message: Option<String>,
    data: Option<InstanceBillData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct InstanceBillData {
    billing_cycle: Option<String>,
    account_id: Option<String>,
    total_count: Option<i32>,
    next_token: Option<String>,
    max_results: Option<i32>,
    /// DescribeInstanceBill returns Items as a direct array, not wrapped in an object
    items: Option<Vec<InstanceBillItem>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct InstanceBillItem {
    billing_date: Option<String>,
    product_code: Option<String>,
    product_name: Option<String>,
    instance_id: Option<String>,
    pretax_amount: Option<f64>,
    #[serde(rename = "PretaxGrossAmount")]
    pretax_gross_amount: Option<f64>,
    currency: Option<String>,
}
