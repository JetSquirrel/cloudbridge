//! Application state: cloud accounts, budgets, and the response caches the
//! dashboard reads.
//!
//! Billing facts are not here — they live in [`crate::ledger`], in their own
//! DuckDB file. The split is deliberate: everything in this file is either
//! user-entered or re-fetchable, so it can be rebuilt at any time, while the
//! ledger is the record that has to survive.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use duckdb::{params, Connection};
use std::sync::{Arc, Mutex};

use crate::cloud::{
    BudgetInfo, BudgetStatus, CloudAccount, CostSummary, CostTrend, DailyCost, ServiceCost,
    SourceId,
};
use crate::config::get_database_path;
use crate::crypto::get_crypto_manager;
use crate::secret_store;

lazy_static::lazy_static! {
    static ref DB_CONNECTION: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(None));
}

/// Cache time-to-live (hours)
const CACHE_TTL_HOURS: i64 = 6;

/// Schema version of the application-state database.
///
/// v1 is the first version to be recorded at all: it splits the billing
/// ledger out into its own file (see [`crate::ledger`]), removes the dead
/// `cost_data` table, renames `provider` to `source_id` now that a source
/// is a registry row rather than an enum variant, and drops the credential
/// columns for good — secrets live in the OS keyring.
const APP_SCHEMA_VERSION: i32 = 1;

/// Initialize database
pub fn init_database() -> Result<()> {
    let db_path = get_database_path()?;
    let conn = Connection::open(&db_path)?;
    prepare_schema(&conn)?;

    let mut db = DB_CONNECTION.lock().unwrap();
    *db = Some(conn);

    tracing::info!("Database initialized: {:?}", db_path);
    Ok(())
}

/// Bring a database file up to [`APP_SCHEMA_VERSION`], creating it from
/// scratch if it is empty.
fn prepare_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            applied_at VARCHAR NOT NULL
        )
        "#,
    )?;

    if current_schema_version(conn)? < 1 {
        migrate_to_v1(conn)?;
    }

    create_v1_tables(conn)?;

    conn.execute(
        "INSERT OR REPLACE INTO schema_version (version, applied_at) VALUES (?, ?)",
        params![APP_SCHEMA_VERSION, Utc::now().to_rfc3339()],
    )?;

    Ok(())
}

/// The v1 shape. Anything the migration already rebuilt is left alone.
///
/// Tables are declared without foreign keys: DuckDB will not drop or alter a
/// table another table points at, which is what makes a rebuild like
/// [`rebuild_accounts_v1`] necessary in the first place. `delete_account`
/// cleans up dependants instead.
fn create_v1_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS cloud_accounts (
            id             VARCHAR PRIMARY KEY,
            name           VARCHAR NOT NULL,
            -- A registry SourceId; see cloud::registry. Stored verbatim, so
            -- these strings are part of the on-disk format.
            source_id      VARCHAR NOT NULL,
            region         VARCHAR,
            created_at     VARCHAR NOT NULL,
            last_synced_at VARCHAR,
            enabled        BOOLEAN NOT NULL DEFAULT true
        );

        CREATE TABLE IF NOT EXISTS budgets (
            account_id      VARCHAR PRIMARY KEY,
            monthly_budget  DOUBLE NOT NULL,
            currency        VARCHAR NOT NULL,
            alert_threshold DOUBLE NOT NULL DEFAULT 80.0,
            created_at      VARCHAR NOT NULL,
            updated_at      VARCHAR NOT NULL
        );

        -- The two cache tables below hold display-shaped API responses, not
        -- billing facts. They go away in PR5, once every source normalizes
        -- into fct_charge and the dashboard reads through the ledger.
        CREATE TABLE IF NOT EXISTS cost_summary_cache (
            account_id              VARCHAR PRIMARY KEY,
            current_month_cost      DOUBLE NOT NULL,
            last_month_cost         DOUBLE NOT NULL,
            currency                VARCHAR NOT NULL,
            month_over_month_change DOUBLE NOT NULL,
            current_month_details   TEXT,
            last_month_details      TEXT,
            cached_at               VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS cost_trend_cache (
            account_id VARCHAR NOT NULL,
            date       VARCHAR NOT NULL,
            amount     DOUBLE NOT NULL,
            currency   VARCHAR NOT NULL,
            cached_at  VARCHAR NOT NULL,
            PRIMARY KEY (account_id, date)
        );
        "#,
    )?;

    Ok(())
}

/// Highest schema version recorded in this file, or 0 for a database that
/// predates versioning (or has just been created).
fn current_schema_version(conn: &Connection) -> Result<i32> {
    let version: Option<i32> =
        conn.query_row("SELECT max(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    Ok(version.unwrap_or(0))
}

fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT column_name FROM duckdb_columns() WHERE table_name = ?")?;
    let names = stmt
        .query_map(params![table], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

/// Bring a pre-versioning database up to v1.
///
/// Driven by which columns are actually present, so it is a no-op on a
/// fresh install and safe to re-enter if it is interrupted before the
/// version row is written.
fn migrate_to_v1(conn: &Connection) -> Result<()> {
    let account_columns = column_names(conn, "cloud_accounts")?;
    if account_columns.is_empty() {
        // Fresh install: nothing to carry over.
        return Ok(());
    }

    tracing::info!("Migrating application database to schema v1");

    // Credentials first, because the rebuild is what actually removes them
    // from disk. Any account we cannot recover a secret for is named in the
    // log — it has to be re-entered.
    if account_columns.iter().any(|c| c == "access_key_id") {
        recover_legacy_secrets(conn)?;
    }

    rebuild_accounts_v1(conn, &account_columns)
}

/// Rebuild `cloud_accounts` in its v1 shape, carrying the rows across.
///
/// A rebuild rather than a sequence of `ALTER`s because DuckDB will not
/// alter or drop a table that a foreign key points at, and both `cost_data`
/// and `budgets` pointed at this one.
fn rebuild_accounts_v1(conn: &Connection, account_columns: &[String]) -> Result<()> {
    let source_column = if account_columns.iter().any(|c| c == "source_id") {
        "source_id"
    } else {
        "provider"
    };

    // cost_data is dead code and its contents are re-fetchable; budgets is
    // copied across.
    conn.execute_batch("DROP TABLE IF EXISTS cost_data")?;

    let has_budgets = !column_names(conn, "budgets")?.is_empty();
    if has_budgets {
        conn.execute_batch(
            "CREATE OR REPLACE TABLE budgets_v1_backup AS SELECT * FROM budgets;
             DROP TABLE budgets;",
        )?;
    }

    conn.execute(
        &format!(
            "CREATE OR REPLACE TABLE cloud_accounts_v1 AS
             SELECT id, name, {source_column} AS source_id, region,
                    created_at, last_synced_at, enabled
             FROM cloud_accounts"
        ),
        [],
    )?;
    conn.execute_batch(
        "DROP TABLE cloud_accounts;
         ALTER TABLE cloud_accounts_v1 RENAME TO cloud_accounts;",
    )?;

    if has_budgets {
        conn.execute_batch(
            "CREATE TABLE budgets AS SELECT * FROM budgets_v1_backup;
             DROP TABLE budgets_v1_backup;",
        )?;
    }

    Ok(())
}

/// Move any credentials still stored in the database into the OS keyring,
/// before the columns holding them are dropped.
fn recover_legacy_secrets(conn: &Connection) -> Result<()> {
    let crypto = match get_crypto_manager() {
        Ok(crypto) => crypto,
        Err(e) => {
            tracing::warn!(
                "Cannot decrypt stored credentials ({}); accounts whose secrets \
                 are not already in the keyring will have to be re-entered",
                e
            );
            return Ok(());
        }
    };

    let mut stmt = conn.prepare(
        "SELECT id, name, access_key_id, secret_access_key FROM cloud_accounts
         WHERE access_key_id <> '' OR secret_access_key <> ''",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    for (id, name, encrypted_ak, encrypted_sk) in rows {
        if secret_store::get_account_secrets(&id)?.is_some() {
            continue;
        }

        let access_key_id = crypto.decrypt(&encrypted_ak).unwrap_or_default();
        let secret_access_key = crypto.decrypt(&encrypted_sk).unwrap_or_default();
        if access_key_id.is_empty() && secret_access_key.is_empty() {
            tracing::warn!(
                "Could not decrypt the stored credentials for account {} ({}); \
                 they will have to be re-entered",
                name,
                id
            );
            continue;
        }

        if let Err(e) = secret_store::store_account_secrets(&id, &access_key_id, &secret_access_key)
        {
            tracing::warn!("Failed to move secrets into the keyring for {}: {}", id, e);
        }
    }

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
    // Secrets go to the OS keyring; the database holds only the account's
    // identity and settings.
    secret_store::store_account_secrets(
        &account.id,
        &account.access_key_id,
        &account.secret_access_key,
    )?;

    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    conn.execute(
        r#"
        INSERT OR REPLACE INTO cloud_accounts
        (id, name, source_id, region, created_at, last_synced_at, enabled)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            account.id,
            account.name,
            account.source_id.as_str(),
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
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let mut stmt = conn.prepare(
        "SELECT id, name, source_id, region, created_at, last_synced_at, enabled FROM cloud_accounts",
    )?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                SourceId::from(row.get::<_, String>(2)?),
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut result = Vec::new();
    for (id, name, source_id, region, created_at_str, last_synced_str, enabled) in rows {
        // An id with no descriptor comes from a build that knew a source this
        // one does not. Skip the row rather than guessing: silently reading it
        // as some other provider would sign requests with the wrong scheme and
        // file the resulting costs under the wrong source.
        if source_id.descriptor().is_none() {
            tracing::warn!(
                "Skipping account {} ({}): no billing source registered under '{}'",
                name,
                id,
                source_id.as_str()
            );
            continue;
        }

        // Credentials live only in the OS keyring (schema v1 moved the last
        // of them out of the database). An account whose secrets are gone is
        // still listed, so the user can see it and re-enter them.
        let (access_key_id, secret_access_key) = match secret_store::get_account_secrets(&id)? {
            Some(secrets) => secrets,
            None => {
                tracing::warn!(
                    "No credentials in the keyring for account {} ({}); it needs to be re-entered",
                    name,
                    id
                );
                (String::new(), String::new())
            }
        };

        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let last_synced_at = last_synced_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        result.push(CloudAccount {
            id,
            name,
            source_id,
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

    // Dependants first: nothing references cloud_accounts through a foreign
    // key any more, so the order is ours to keep.
    conn.execute(
        "DELETE FROM budgets WHERE account_id = ?",
        params![account_id],
    )?;
    conn.execute(
        "DELETE FROM cost_summary_cache WHERE account_id = ?",
        params![account_id],
    )?;
    conn.execute(
        "DELETE FROM cost_trend_cache WHERE account_id = ?",
        params![account_id],
    )?;
    conn.execute(
        "DELETE FROM cloud_accounts WHERE id = ?",
        params![account_id],
    )?;

    // Remove secrets from OS keyring as well
    if let Err(e) = secret_store::delete_account_secrets(account_id) {
        tracing::warn!("Failed to delete account secrets from keyring: {}", e);
    }

    Ok(())
}

// ==================== Cache Functions ====================

/// Check if cost summary cache is valid
/// account_name and source_id are passed by the caller to avoid deadlock when acquiring lock while holding database lock
pub fn get_cached_cost_summary_with_account(
    account_id: &str,
    account_name: &str,
    source_id: &SourceId,
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
                source_id: source_id.clone(),
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

// ==================== Budget Functions ====================

/// Save or update budget for an account
#[allow(dead_code)] // TODO(v0.2.0): remove once the budget UI calls this
pub fn save_budget(budget: &BudgetInfo) -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    conn.execute(
        r#"
        INSERT OR REPLACE INTO budgets
        (account_id, monthly_budget, currency, alert_threshold, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
        params![
            budget.account_id,
            budget.monthly_budget,
            budget.currency,
            budget.alert_threshold,
            budget.created_at.to_rfc3339(),
            budget.updated_at.to_rfc3339(),
        ],
    )?;

    tracing::info!("Saved budget for account {}", budget.account_id);
    Ok(())
}

/// Get budget for an account
#[allow(dead_code)] // TODO(v0.2.0): remove once the budget UI calls this
pub fn get_budget(account_id: &str) -> Result<Option<BudgetInfo>> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let mut stmt = conn.prepare(
        "SELECT account_id, monthly_budget, currency, alert_threshold, created_at, updated_at
         FROM budgets WHERE account_id = ?",
    )?;

    let result = stmt.query_row(params![account_id], |row| {
        let created_at_str: String = row.get(4)?;
        let updated_at_str: String = row.get(5)?;

        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        Ok(BudgetInfo {
            account_id: row.get(0)?,
            monthly_budget: row.get(1)?,
            currency: row.get(2)?,
            alert_threshold: row.get(3)?,
            created_at,
            updated_at,
        })
    });

    match result {
        Ok(budget) => Ok(Some(budget)),
        Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Failed to get budget: {}", e)),
    }
}

/// Get all budgets
#[allow(dead_code)] // TODO(v0.2.0): remove once the budget UI calls this
pub fn get_all_budgets() -> Result<Vec<BudgetInfo>> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    let mut stmt = conn.prepare(
        "SELECT account_id, monthly_budget, currency, alert_threshold, created_at, updated_at
         FROM budgets",
    )?;

    let budgets = stmt
        .query_map([], |row| {
            let created_at_str: String = row.get(4)?;
            let updated_at_str: String = row.get(5)?;

            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            Ok(BudgetInfo {
                account_id: row.get(0)?,
                monthly_budget: row.get(1)?,
                currency: row.get(2)?,
                alert_threshold: row.get(3)?,
                created_at,
                updated_at,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(budgets)
}

/// Delete budget for an account
#[allow(dead_code)] // TODO(v0.2.0): remove once the budget UI calls this
pub fn delete_budget(account_id: &str) -> Result<()> {
    let db = get_connection()?;
    let conn = db.as_ref().unwrap();

    conn.execute(
        "DELETE FROM budgets WHERE account_id = ?",
        params![account_id],
    )?;

    tracing::info!("Deleted budget for account {}", account_id);
    Ok(())
}

/// Get budget status (compares budget with current costs)
#[allow(dead_code)] // TODO(v0.2.0): remove once the budget UI calls this
pub fn get_budget_status(account_id: &str) -> Result<Option<BudgetStatus>> {
    // Get budget
    let budget = match get_budget(account_id)? {
        Some(b) => b,
        None => return Ok(None),
    };

    // Get account info
    let accounts = get_all_accounts()?;
    let account = accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| anyhow::anyhow!("Account not found"))?;

    // Get cached cost summary
    let cost_summary =
        get_cached_cost_summary_with_account(account_id, &account.name, &account.source_id)?;

    let current_cost = cost_summary.map(|cs| cs.current_month_cost).unwrap_or(0.0);

    // Calculate metrics
    let percentage_used = if budget.monthly_budget > 0.0 {
        (current_cost / budget.monthly_budget) * 100.0
    } else {
        0.0
    };

    let remaining = budget.monthly_budget - current_cost;
    let alert_triggered = percentage_used >= budget.alert_threshold;

    Ok(Some(BudgetStatus {
        account_id: account_id.to_string(),
        account_name: account.name.clone(),
        monthly_budget: budget.monthly_budget,
        current_cost,
        currency: budget.currency,
        percentage_used,
        remaining,
        alert_triggered,
    }))
}

/// Get all budget statuses
#[allow(dead_code)] // TODO(v0.2.0): remove once the budget UI calls this
pub fn get_all_budget_statuses() -> Result<Vec<BudgetStatus>> {
    let budgets = get_all_budgets()?;
    let mut statuses = Vec::new();

    for budget in budgets {
        if let Some(status) = get_budget_status(&budget.account_id)? {
            statuses.push(status);
        }
    }

    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 0.1 schema, as it was written before versioning existed.
    const LEGACY_SCHEMA: &str = r#"
        CREATE TABLE cloud_accounts (
            id VARCHAR PRIMARY KEY,
            name VARCHAR NOT NULL,
            provider VARCHAR NOT NULL,
            access_key_id VARCHAR NOT NULL,
            secret_access_key VARCHAR NOT NULL,
            region VARCHAR,
            created_at VARCHAR NOT NULL,
            last_synced_at VARCHAR,
            enabled BOOLEAN NOT NULL DEFAULT true
        );
        CREATE TABLE cost_data (
            id INTEGER PRIMARY KEY,
            account_id VARCHAR NOT NULL,
            date VARCHAR NOT NULL,
            service VARCHAR NOT NULL,
            amount DOUBLE NOT NULL,
            currency VARCHAR NOT NULL,
            created_at VARCHAR,
            FOREIGN KEY (account_id) REFERENCES cloud_accounts(id)
        );
        CREATE TABLE budgets (
            account_id VARCHAR PRIMARY KEY,
            monthly_budget DOUBLE NOT NULL,
            currency VARCHAR NOT NULL,
            alert_threshold DOUBLE NOT NULL DEFAULT 80.0,
            created_at VARCHAR NOT NULL,
            updated_at VARCHAR NOT NULL,
            FOREIGN KEY (account_id) REFERENCES cloud_accounts(id)
        );
        INSERT INTO cloud_accounts VALUES
            ('acct-1', 'Prod', 'AWS', '', '', 'us-east-1', '2026-08-01T00:00:00+00:00', NULL, true);
        INSERT INTO cost_data VALUES
            (1, 'acct-1', '2026-08-01', 'EC2', 12.5, 'USD', '2026-08-02T00:00:00+00:00');
        INSERT INTO budgets VALUES
            ('acct-1', 100.0, 'USD', 80.0, '2026-08-01T00:00:00+00:00', '2026-08-01T00:00:00+00:00');
    "#;

    fn legacy_database() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(LEGACY_SCHEMA).expect("legacy schema");
        conn
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        !column_names(conn, table).unwrap().is_empty()
    }

    #[test]
    fn a_fresh_database_starts_at_the_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_schema(&conn).unwrap();

        assert_eq!(current_schema_version(&conn).unwrap(), APP_SCHEMA_VERSION);
        assert_eq!(
            column_names(&conn, "cloud_accounts").unwrap(),
            vec![
                "id",
                "name",
                "source_id",
                "region",
                "created_at",
                "last_synced_at",
                "enabled"
            ]
        );
        assert!(!table_exists(&conn, "cost_data"));

        // Re-opening an up-to-date database changes nothing.
        prepare_schema(&conn).unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), APP_SCHEMA_VERSION);
    }

    #[test]
    fn the_v1_rebuild_carries_accounts_and_budgets_across() {
        let conn = legacy_database();
        let columns = column_names(&conn, "cloud_accounts").unwrap();

        rebuild_accounts_v1(&conn, &columns).unwrap();
        create_v1_tables(&conn).unwrap();

        let (id, source_id, region): (String, String, String) = conn
            .query_row(
                "SELECT id, source_id, region FROM cloud_accounts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (id.as_str(), source_id.as_str(), region.as_str()),
            ("acct-1", "AWS", "us-east-1")
        );

        // The credential columns are gone, not merely emptied.
        let columns = column_names(&conn, "cloud_accounts").unwrap();
        assert!(!columns.iter().any(|c| c == "access_key_id"));
        assert!(!columns.iter().any(|c| c == "secret_access_key"));
        assert!(!columns.iter().any(|c| c == "provider"));

        // Dead table dropped, user-entered data kept.
        assert!(!table_exists(&conn, "cost_data"));
        assert!(!table_exists(&conn, "budgets_v1_backup"));
        let budget: f64 = conn
            .query_row("SELECT monthly_budget FROM budgets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(budget, 100.0);
    }

    #[test]
    fn the_v1_rebuild_leaves_an_already_renamed_column_alone() {
        // A database that got as far as source_id before being interrupted.
        let conn = legacy_database();
        conn.execute_batch(
            "DROP TABLE cost_data;
             DROP TABLE budgets;
             ALTER TABLE cloud_accounts RENAME COLUMN provider TO source_id;",
        )
        .unwrap();

        let columns = column_names(&conn, "cloud_accounts").unwrap();
        rebuild_accounts_v1(&conn, &columns).unwrap();
        create_v1_tables(&conn).unwrap();

        let source_id: String = conn
            .query_row("SELECT source_id FROM cloud_accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(source_id, "AWS");
    }
}
