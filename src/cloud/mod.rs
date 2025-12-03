//! Cloud provider module

pub mod aliyun;
pub mod aws;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Cloud provider type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[allow(clippy::upper_case_acronyms)]
pub enum CloudProvider {
    #[default]
    AWS,
    Aliyun,
    Azure,
    GCP,
}

impl CloudProvider {
    /// Get full name of cloud provider
    #[allow(dead_code)]
    pub fn display_name(&self) -> &'static str {
        match self {
            CloudProvider::AWS => "Amazon Web Services",
            CloudProvider::Aliyun => "Alibaba Cloud",
            CloudProvider::Azure => "Microsoft Azure",
            CloudProvider::GCP => "Google Cloud Platform",
        }
    }

    /// Get short name of cloud provider
    pub fn short_name(&self) -> &'static str {
        match self {
            CloudProvider::AWS => "AWS",
            CloudProvider::Aliyun => "Aliyun",
            CloudProvider::Azure => "Azure",
            CloudProvider::GCP => "GCP",
        }
    }
}

/// Cloud account information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudAccount {
    /// Account ID
    pub id: String,
    /// Account name (user-defined)
    pub name: String,
    /// Cloud provider
    pub provider: CloudProvider,
    /// Access Key ID (encrypted storage)
    pub access_key_id: String,
    /// Secret Access Key (encrypted storage)
    pub secret_access_key: String,
    /// Region (optional)
    pub region: Option<String>,
    /// Created time
    pub created_at: DateTime<Utc>,
    /// Last synced time
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Is enabled
    pub enabled: bool,
}

/// Cost data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostData {
    /// Account ID
    pub account_id: String,
    /// Date
    pub date: String,
    /// Service name
    pub service: String,
    /// Cost amount
    pub amount: f64,
    /// Currency
    pub currency: String,
}

/// Cost summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSummary {
    /// Account ID
    pub account_id: String,
    /// Account name
    pub account_name: String,
    /// Cloud provider
    pub provider: CloudProvider,
    /// Current month cost
    pub current_month_cost: f64,
    /// Last month cost
    pub last_month_cost: f64,
    /// Currency
    pub currency: String,
    /// Month-over-month change (percentage)
    pub month_over_month_change: f64,
    /// Current month service cost details
    pub current_month_details: Vec<ServiceCost>,
    /// Last month service cost details
    pub last_month_details: Vec<ServiceCost>,
}

/// Service cost detail
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCost {
    /// Service name
    pub service: String,
    /// Cost amount
    pub amount: f64,
    /// Currency
    pub currency: String,
}

/// Daily cost data (for chart display)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyCost {
    /// Date (YYYY-MM-DD format)
    pub date: String,
    /// Daily cost
    pub amount: f64,
}

/// Cost trend data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostTrend {
    /// Account ID
    pub account_id: String,
    /// Currency
    pub currency: String,
    /// Daily costs list
    pub daily_costs: Vec<DailyCost>,
}

/// Cloud service provider trait (sync version, using ureq)
pub trait CloudService: Send + Sync {
    /// Validate credentials
    fn validate_credentials(&self) -> Result<bool>;

    /// Get cost data
    fn get_cost_data(&self, start_date: &str, end_date: &str) -> Result<Vec<CostData>>;

    /// Get cost summary
    fn get_cost_summary(&self) -> Result<CostSummary>;

    /// Get cost trend (daily costs)
    fn get_cost_trend(&self, start_date: &str, end_date: &str) -> Result<CostTrend>;
}
