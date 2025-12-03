//! Database module - Using DuckDB for data storage

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use duckdb::{params, Connection};
use std::sync::{Arc, Mutex};

use crate::cloud::{
    CloudAccount, CloudProvider, CostData, CostSummary, CostTrend, DailyCost, ServiceCost,
};
use crate::config::get_database_path;
use crate::crypto::get_crypto_manager;

lazy_static::lazy_static! {
    static ref DB_CONNECTION: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(None));
}

/// Cache time-to-live (hours)
const CACHE_TTL_HOURS: i64 = 6;

/// Initialize database
pub fn init_database() -> Result<()> {
    let db_path = get_database_path()?;
    let conn = Connection::open(&db_path)?;

    // Create cloud accounts table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS cloud_accounts (
            id VARCHAR PRIMARY KEY,
            name VARCHAR NOT NULL,
            provider VARCHAR NOT NULL,
            access_key_id VARCHAR NOT NULL,
            secret_access_key VARCHAR NOT NULL,
            region VARCHAR,
            created_at VARCHAR NOT NULL,
            last_synced_at VARCHAR,
            enabled BOOLEAN NOT NULL DEFAULT true
        )
        "#,
        [],
    )?;

    // Create cost data table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS cost_data (
            id INTEGER PRIMARY KEY,
            account_id VARCHAR NOT NULL,
            date VARCHAR NOT NULL,
            service VARCHAR NOT NULL,
            amount DOUBLE NOT NULL,
            currency VARCHAR NOT NULL,
            created_at VARCHAR,
            FOREIGN KEY (account_id) REFERENCES cloud_accounts(id)
        )
        "#,
        [],
    )?;

    // Create index
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_cost_data_account_date ON cost_data(account_id, date)",
        [],
    )?;

    // Create cost summary cache table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS cost_summary_cache (
            account_id VARCHAR PRIMARY KEY,
            current_month_cost DOUBLE NOT NULL,
            last_month_cost DOUBLE NOT NULL,
            currency VARCHAR NOT NULL,
            month_over_month_change DOUBLE NOT NULL,
            current_month_details TEXT,
            last_month_details TEXT,
            cached_at VARCHAR NOT NULL
        )
        "#,
        [],
    )?;

    // Create daily cost trend cache table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS cost_trend_cache (
            account_id VARCHAR NOT NULL,
            date VARCHAR NOT NULL,
            amount DOUBLE NOT NULL,
            currency VARCHAR NOT NULL,
            cached_at VARCHAR NOT NULL,
            PRIMARY KEY (account_id, date)
        )
        "#,
        [],
    )?;

    let mut db = DB_CONNECTION.lock().unwrap();
    *db = Some(conn);

    tracing::info!("Database initialized: {:?}", db_path);
    Ok(())
}

/// Get database connection
fn get_connection() -> Result<std::sync::MutexGuard<'static, Option<Connection>>> {
    let db = DB_CONNECTION
        .lock()
        .map_err(|e| anyhow::anyhow!("Failed to get database connection: {}", e))?;
    if db.is_none() {
        return Err(anyhow::anyhow!("Database not initialized"));
    }
    Ok(db)
}

/// Save cloud account
pub fn save_account(account: &CloudAccount) -> Result<()> {
    let crypto = get_crypto_manager()?;
    let encrypted_ak = crypto.encrypt(&account.access_key_id)?;
    let encrypted_sk = crypto.encrypt(&account.secret_access_key)?;

    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    conn.execute(
        r#"
        INSERT OR REPLACE INTO cloud_accounts 
        (id, name, provider, access_key_id, secret_access_key, region, created_at, last_synced_at, enabled)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            account.id,
            account.name,
            format!("{:?}", account.provider),
            encrypted_ak,
            encrypted_sk,
            account.region,
            account.created_at.to_rfc3339(),
            account.last_synced_at.map(|dt| dt.to_rfc3339()),
            account.enabled,
        ],
    )?;

    Ok(())
}

/// Get all cloud accounts
pub fn get_all_accounts() -> Result<Vec<CloudAccount>> {
    let crypto = get_crypto_manager()?;
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let mut stmt = conn.prepare(
        "SELECT id, name, provider, access_key_id, secret_access_key, region, created_at, last_synced_at, enabled FROM cloud_accounts"
    )?;

    let accounts = stmt
        .query_map([], |row| {
            let provider_str: String = row.get(2)?;
            let provider = match provider_str.as_str() {
                "AWS" => CloudProvider::AWS,
                "Aliyun" => CloudProvider::Aliyun,
                "Azure" => CloudProvider::Azure,
                "GCP" => CloudProvider::GCP,
                _ => CloudProvider::AWS,
            };

            let encrypted_ak: String = row.get(3)?;
            let encrypted_sk: String = row.get(4)?;

            let created_at_str: String = row.get(6)?;
            let last_synced_str: Option<String> = row.get(7)?;

            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                provider,
                encrypted_ak,
                encrypted_sk,
                row.get::<_, Option<String>>(5)?,
                created_at_str,
                last_synced_str,
                row.get::<_, bool>(8)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut result = Vec::new();
    for (
        id,
        name,
        provider,
        encrypted_ak,
        encrypted_sk,
        region,
        created_at_str,
        last_synced_str,
        enabled,
    ) in accounts
    {
        let access_key_id = crypto.decrypt(&encrypted_ak).unwrap_or_default();
        let secret_access_key = crypto.decrypt(&encrypted_sk).unwrap_or_default();
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let last_synced_at = last_synced_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        result.push(CloudAccount {
            id,
            name,
            provider,
            access_key_id,
            secret_access_key,
            region,
            created_at,
            last_synced_at,
            enabled,
        });
    }

    Ok(result)
}

/// Delete cloud account
pub fn delete_account(account_id: &str) -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    // First delete associated cost data
    conn.execute(
        "DELETE FROM cost_data WHERE account_id = ?",
        params![account_id],
    )?;
    // Then delete the account
    conn.execute(
        "DELETE FROM cloud_accounts WHERE id = ?",
        params![account_id],
    )?;

    Ok(())
}

/// Save cost data (reserved interface)
#[allow(dead_code)]
pub fn save_cost_data(costs: &[CostData]) -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    for cost in costs {
        conn.execute(
            r#"
            INSERT INTO cost_data (account_id, date, service, amount, currency)
            VALUES (?, ?, ?, ?, ?)
            "#,
            params![
                cost.account_id,
                cost.date,
                cost.service,
                cost.amount,
                cost.currency,
            ],
        )?;
    }

    Ok(())
}

/// Get account cost data (reserved interface)
#[allow(dead_code)]
pub fn get_cost_data(account_id: &str, start_date: &str, end_date: &str) -> Result<Vec<CostData>> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let mut stmt = conn.prepare(
        "SELECT account_id, date, service, amount, currency FROM cost_data WHERE account_id = ? AND date >= ? AND date <= ? ORDER BY date"
    )?;

    let costs = stmt
        .query_map(params![account_id, start_date, end_date], |row| {
            Ok(CostData {
                account_id: row.get(0)?,
                date: row.get(1)?,
                service: row.get(2)?,
                amount: row.get(3)?,
                currency: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(costs)
}

/// Get cost summaries for all accounts (reserved interface)
#[allow(dead_code)]
pub fn get_all_cost_summaries() -> Result<Vec<CostSummary>> {
    let accounts = get_all_accounts()?;
    let mut summaries = Vec::new();

    for account in accounts {
        if !account.enabled {
            continue;
        }

        // Return basic info only, actual costs need to be fetched from cloud
        summaries.push(CostSummary {
            account_id: account.id,
            account_name: account.name,
            provider: account.provider,
            current_month_cost: 0.0,
            last_month_cost: 0.0,
            currency: "USD".to_string(),
            month_over_month_change: 0.0,
            current_month_details: Vec::new(),
            last_month_details: Vec::new(),
        });
    }

    Ok(summaries)
}

// ==================== Cache Functions ====================

/// Check if cost summary cache is valid
/// account_name and provider are passed by the caller to avoid deadlock when acquiring lock while holding database lock
pub fn get_cached_cost_summary_with_account(
    account_id: &str,
    account_name: &str,
    provider: &CloudProvider,
) -> Result<Option<CostSummary>> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let mut stmt = conn.prepare(
        "SELECT current_month_cost, last_month_cost, currency, month_over_month_change, 
                current_month_details, last_month_details, cached_at 
         FROM cost_summary_cache WHERE account_id = ?",
    )?;

    let result = stmt.query_row(params![account_id], |row| {
        let cached_at_str: String = row.get(6)?;
        let current_details_json: Option<String> = row.get(4)?;
        let last_details_json: Option<String> = row.get(5)?;

        Ok((
            row.get::<_, f64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
            current_details_json,
            last_details_json,
            cached_at_str,
        ))
    });

    match result {
        Ok((
            current,
            last,
            currency,
            change,
            current_details_json,
            last_details_json,
            cached_at_str,
        )) => {
            // Check if cache is expired
            let cached_at = DateTime::parse_from_rfc3339(&cached_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now() - Duration::hours(CACHE_TTL_HOURS + 1));

            let now = Utc::now();
            if now - cached_at > Duration::hours(CACHE_TTL_HOURS) {
                tracing::info!("Cost summary cache expired (cached at: {})", cached_at_str);
                return Ok(None);
            }

            // Parse service details
            let current_month_details: Vec<ServiceCost> = current_details_json
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();
            let last_month_details: Vec<ServiceCost> = last_details_json
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();

            tracing::info!(
                "Using cost summary cache (cached at: {}, {} hours remaining)",
                cached_at_str,
                CACHE_TTL_HOURS - (now - cached_at).num_hours()
            );

            Ok(Some(CostSummary {
                account_id: account_id.to_string(),
                account_name: account_name.to_string(),
                provider: *provider,
                current_month_cost: current,
                last_month_cost: last,
                currency,
                month_over_month_change: change,
                current_month_details,
                last_month_details,
            }))
        }
        Err(_) => Ok(None),
    }
}

/// Save cost summary to cache
pub fn save_cost_summary_cache(summary: &CostSummary) -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let current_details_json = serde_json::to_string(&summary.current_month_details)?;
    let last_details_json = serde_json::to_string(&summary.last_month_details)?;

    conn.execute(
        r#"
        INSERT OR REPLACE INTO cost_summary_cache 
        (account_id, current_month_cost, last_month_cost, currency, month_over_month_change, 
         current_month_details, last_month_details, cached_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            summary.account_id,
            summary.current_month_cost,
            summary.last_month_cost,
            summary.currency,
            summary.month_over_month_change,
            current_details_json,
            last_details_json,
            Utc::now().to_rfc3339(),
        ],
    )?;

    tracing::info!("Cached cost summary for account {}", summary.account_id);
    Ok(())
}

/// Get cached cost trend
pub fn get_cached_cost_trend(
    account_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<Option<CostTrend>> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    // First check if there's cache for this date range and if it's expired
    let mut stmt = conn.prepare(
        "SELECT date, amount, currency, cached_at FROM cost_trend_cache 
         WHERE account_id = ? AND date >= ? AND date < ?
         ORDER BY date",
    )?;

    let rows = stmt.query_map(params![account_id, start_date, end_date], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    let mut daily_costs = Vec::new();
    let mut oldest_cache: Option<DateTime<Utc>> = None;
    let mut currency = "USD".to_string();

    for row in rows {
        let (date, amount, curr, cached_at_str) = row?;

        let cached_at = DateTime::parse_from_rfc3339(&cached_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now() - Duration::hours(CACHE_TTL_HOURS + 1));

        // Track the oldest cache time
        if oldest_cache.is_none() || cached_at < oldest_cache.unwrap() {
            oldest_cache = Some(cached_at);
        }

        currency = curr;
        daily_costs.push(DailyCost { date, amount });
    }

    // Return None if no data or cache expired
    if daily_costs.is_empty() {
        return Ok(None);
    }

    let now = Utc::now();
    if let Some(cached_at) = oldest_cache {
        if now - cached_at > Duration::hours(CACHE_TTL_HOURS) {
            tracing::info!("Cost trend cache expired");
            return Ok(None);
        }

        tracing::info!(
            "Using cost trend cache ({} data points, {} hours remaining)",
            daily_costs.len(),
            CACHE_TTL_HOURS - (now - cached_at).num_hours()
        );
    }

    Ok(Some(CostTrend {
        account_id: account_id.to_string(),
        currency,
        daily_costs,
    }))
}

/// Save cost trend to cache
pub fn save_cost_trend_cache(trend: &CostTrend) -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let now = Utc::now().to_rfc3339();

    for daily in &trend.daily_costs {
        conn.execute(
            r#"
            INSERT OR REPLACE INTO cost_trend_cache 
            (account_id, date, amount, currency, cached_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
            params![
                trend.account_id,
                daily.date,
                daily.amount,
                trend.currency,
                now,
            ],
        )?;
    }

    tracing::info!(
        "Cached cost trend for account {} ({} days)",
        trend.account_id,
        trend.daily_costs.len()
    );
    Ok(())
}

/// Clear all cache for specified account (for force refresh, reserved interface)
#[allow(dead_code)]
pub fn clear_account_cache(account_id: &str) -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    conn.execute(
        "DELETE FROM cost_summary_cache WHERE account_id = ?",
        params![account_id],
    )?;
    conn.execute(
        "DELETE FROM cost_trend_cache WHERE account_id = ?",
        params![account_id],
    )?;

    tracing::info!("Cleared all cache for account {}", account_id);
    Ok(())
}

/// Clear all cache (for global force refresh)
pub fn clear_all_cache() -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    conn.execute("DELETE FROM cost_summary_cache", [])?;
    conn.execute("DELETE FROM cost_trend_cache", [])?;

    tracing::info!("Cleared all cost cache");
    Ok(())
}
