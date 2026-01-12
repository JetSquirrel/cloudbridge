//! DeepSeek API integration - Balance query

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::{CloudProvider, CloudService, CostData, CostSummary, CostTrend, ServiceCost};

/// DeepSeek balance info
#[derive(Debug, Deserialize)]
pub struct BalanceInfo {
    /// Currency (CNY or USD)
    pub currency: String,
    /// Total available balance
    pub total_balance: String,
    /// Granted balance (not expired)
    pub granted_balance: String,
    /// Topped-up balance
    pub topped_up_balance: String,
}

/// DeepSeek balance response
#[derive(Debug, Deserialize)]
pub struct BalanceResponse {
    /// Whether balance is sufficient for API calls
    #[allow(dead_code)]
    pub is_available: bool,
    /// Balance info array
    pub balance_infos: Vec<BalanceInfo>,
}

/// DeepSeek service
pub struct DeepSeekService {
    account_id: String,
    account_name: String,
    api_key: String,
}

impl DeepSeekService {
    pub fn new(
        account_id: String,
        account_name: String,
        api_key: String,
        _secret: String, // Not used for DeepSeek, but kept for interface consistency
        _region: Option<String>, // Not used for DeepSeek
    ) -> Self {
        Self {
            account_id,
            account_name,
            api_key,
        }
    }

    /// Get user balance from DeepSeek API
    pub fn get_balance(&self) -> Result<BalanceResponse> {
        let response = ureq::get("https://api.deepseek.com/user/balance")
            .header("Accept", "application/json")
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .call()
            .map_err(|e| anyhow!("Failed to call DeepSeek API: {}", e))?;

        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;

        let balance: BalanceResponse = serde_json::from_str(&body)
            .map_err(|e| anyhow!("Failed to parse DeepSeek response: {}", e))?;

        Ok(balance)
    }
}

impl CloudService for DeepSeekService {
    fn validate_credentials(&self) -> Result<bool> {
        match self.get_balance() {
            Ok(_) => Ok(true),
            Err(e) => {
                tracing::warn!("DeepSeek credential validation failed: {}", e);
                Ok(false)
            }
        }
    }

    fn get_cost_data(&self, _start_date: &str, _end_date: &str) -> Result<Vec<CostData>> {
        // DeepSeek doesn't provide detailed cost history, return empty
        Ok(vec![])
    }

    fn get_cost_summary(&self) -> Result<CostSummary> {
        let balance = self.get_balance()?;

        // Prefer CNY balance first, then USD, then fallback to first
        let balance_info = balance
            .balance_infos
            .iter()
            .find(|b| b.currency == "CNY")
            .or_else(|| balance.balance_infos.iter().find(|b| b.currency == "USD"))
            .or_else(|| balance.balance_infos.first())
            .ok_or_else(|| anyhow!("No balance info found"))?;

        let total: f64 = balance_info.total_balance.parse().unwrap_or(0.0);
        let granted: f64 = balance_info.granted_balance.parse().unwrap_or(0.0);
        let topped_up: f64 = balance_info.topped_up_balance.parse().unwrap_or(0.0);

        // Build service details showing balance breakdown
        let mut details = Vec::new();
        if granted > 0.0 {
            details.push(ServiceCost {
                service: "Granted Balance".to_string(),
                amount: granted,
                currency: balance_info.currency.clone(),
            });
        }
        if topped_up > 0.0 {
            details.push(ServiceCost {
                service: "Topped-up Balance".to_string(),
                amount: topped_up,
                currency: balance_info.currency.clone(),
            });
        }

        // For DeepSeek, we show balance instead of cost
        // current_month_cost = remaining balance (positive)
        // last_month_cost = 0 (no historical data)
        Ok(CostSummary {
            account_id: self.account_id.clone(),
            account_name: self.account_name.clone(),
            provider: CloudProvider::DeepSeek,
            current_month_cost: total,
            last_month_cost: 0.0,
            currency: balance_info.currency.clone(),
            month_over_month_change: 0.0, // No comparison for balance
            current_month_details: details,
            last_month_details: vec![],
        })
    }

    fn get_cost_trend(&self, _start_date: &str, _end_date: &str) -> Result<CostTrend> {
        // DeepSeek doesn't provide daily usage history
        // Return empty trend with current balance as single point
        Ok(CostTrend {
            account_id: self.account_id.clone(),
            currency: "USD".to_string(),
            daily_costs: vec![],
        })
    }
}
