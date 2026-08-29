//! DeepSeek API integration - Balance query

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::raw::RawPart;
use super::{BillingPeriod, BillingSource, Normalized, RawBatch};
use crate::ledger::BalanceSnapshot;

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

const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

/// Name the balance payload is stored under in a raw batch.
const PART_BALANCE: &str = "balance";

/// DeepSeek service
pub struct DeepSeekService {
    api_key: String,
}

impl DeepSeekService {
    pub fn new(
        api_key: String,
        _secret: String, // Not used for DeepSeek, but kept for interface consistency
        _region: Option<String>, // Not used for DeepSeek
    ) -> Self {
        Self { api_key }
    }

    /// Ask the balance endpoint and return the response body unchanged.
    fn balance_raw(&self) -> Result<String> {
        let response = ureq::get(BALANCE_URL)
            .header("Accept", "application/json")
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .call()
            .map_err(|e| anyhow!("Failed to call DeepSeek API: {}", e))?;

        response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read response: {}", e))
    }

    /// Get user balance from DeepSeek API
    pub fn get_balance(&self) -> Result<BalanceResponse> {
        let body = self.balance_raw()?;

        serde_json::from_str(&body).map_err(|e| anyhow!("Failed to parse DeepSeek response: {}", e))
    }
}

/// Turn a fetched balance payload into ledger rows.
///
/// Pure — the observation time comes from the batch, not the clock.
///
/// A balance is state, not a charge: it says what is left, not what was
/// spent, so it lands in `fct_balance_snapshot` and contributes nothing to
/// a cost total. Top-ups are the part that is a charge, and DeepSeek does
/// not report them here; PR5 derives them from the movement between two
/// snapshots.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    let part = batch
        .part(PART_BALANCE)
        .ok_or_else(|| anyhow!("Raw batch has no '{}' payload", PART_BALANCE))?;
    let response: BalanceResponse = serde_json::from_str(&part.body)
        .map_err(|e| anyhow!("Failed to parse DeepSeek response: {}", e))?;

    let balances = response
        .balance_infos
        .into_iter()
        .map(|info| BalanceSnapshot {
            provider: batch.provider.clone(),
            account_id: batch.account_id.clone(),
            observed_at: batch.fetched_at,
            balance: info.total_balance.parse().unwrap_or(0.0),
            granted_balance: info.granted_balance.parse().ok(),
            topped_up_balance: info.topped_up_balance.parse().ok(),
            currency: info.currency,
        })
        .collect();

    Ok(Normalized {
        charges: Vec::new(),
        balances,
    })
}

impl BillingSource for DeepSeekService {
    fn validate_credentials(&self) -> Result<bool> {
        match self.get_balance() {
            Ok(_) => Ok(true),
            Err(e) => {
                tracing::warn!("DeepSeek credential validation failed: {}", e);
                Ok(false)
            }
        }
    }

    fn fetch(&self, _period: &BillingPeriod) -> Result<Vec<RawPart>> {
        // A balance is the same value whichever period is being ingested:
        // the endpoint reports the account as it stands right now.
        Ok(vec![RawPart::new(
            PART_BALANCE,
            format!("GET {}", BALANCE_URL),
            self.balance_raw()?,
        )])
    }

    fn normalize(&self, batch: &RawBatch) -> Result<Normalized> {
        normalize(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One recorded /user/balance response.
    const BALANCE: &str = include_str!("testdata/deepseek_balance.json");

    fn recorded_batch(body: &str) -> RawBatch {
        RawBatch {
            provider: "DeepSeek".to_string(),
            account_id: "acct-3".to_string(),
            period: BillingPeriod::new(2026, 8),
            batch_id: "b-1".to_string(),
            fetched_at: "2026-08-29T09:30:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART_BALANCE, "", body)],
        }
    }

    #[test]
    fn a_balance_becomes_a_snapshot_and_never_a_charge() {
        let normalized = normalize(&recorded_batch(BALANCE)).unwrap();

        assert!(normalized.charges.is_empty());
        assert_eq!(normalized.balances.len(), 1);

        let snapshot = &normalized.balances[0];
        assert_eq!(snapshot.balance, 42.75);
        assert_eq!(snapshot.granted_balance, Some(10.0));
        assert_eq!(snapshot.topped_up_balance, Some(32.75));
        assert_eq!(snapshot.currency, "CNY");
        assert_eq!(
            snapshot.observed_at.to_rfc3339(),
            "2026-08-29T09:30:00+00:00"
        );
        assert_eq!(snapshot.account_id, "acct-3");
    }

    #[test]
    fn an_account_holding_two_currencies_gets_a_snapshot_each() {
        let normalized = normalize(&recorded_batch(
            r#"{"is_available":true,"balance_infos":[
                 {"currency":"CNY","total_balance":"42.75","granted_balance":"10.00","topped_up_balance":"32.75"},
                 {"currency":"USD","total_balance":"6.00","granted_balance":"0.00","topped_up_balance":"6.00"}]}"#,
        ))
        .unwrap();

        let currencies: Vec<&str> = normalized
            .balances
            .iter()
            .map(|b| b.currency.as_str())
            .collect();
        assert_eq!(currencies, vec!["CNY", "USD"]);
    }
}
