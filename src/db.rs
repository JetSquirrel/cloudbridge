//! Application state: cloud accounts, budgets, and the response caches the
//! dashboard reads.
//!
//! Billing facts are not here — they live in [`crate::ledger`], in their own
//! DuckDB file. The split is deliberate: everything in this file is either
//! user-entered or re-fetchable, so it can be rebuilt at any time, while the
//! ledger is the record that has to survive.

use anyhow::Result;
use chrono::{DateTime, Utc};
use duckdb::{params, Connection};
use std::sync::{Arc, Mutex};

use crate::cloud::{BillingPeriod, BudgetInfo, BudgetStatus, CloudAccount, SourceId};
use crate::config::get_database_path;
use crate::crypto::get_crypto_manager;
use crate::ledger::{query, PeriodKey};
use crate::secret_store;

lazy_static::lazy_static! {
    static ref DB_CONNECTION: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(None));
}

/// Schema version of the application-state database.
///
/// v1 is the first version to be recorded at all: it splits the billing
/// ledger out into its own file (see [`crate::ledger`]), removes the dead
/// `cost_data` table, renames `provider` to `source_id` now that a source
/// is a registry row rather than an enum variant, and drops the credential
/// columns for good — secrets live in the OS keyring.
///
/// v2 drops the two response caches. The dashboard reads the ledger now,
/// which records when each period was ingested, so a separate copy of
/// display-shaped API responses has nothing left to do.
///
/// v3 restores the primary keys that the v1 rebuild silently dropped.
const APP_SCHEMA_VERSION: i32 = 3;

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

    let version = current_schema_version(conn)?;
    if version < 1 {
        migrate_to_v1(conn)?;
    }
    if version < 2 {
        conn.execute_batch(
            "DROP TABLE IF EXISTS cost_summary_cache;
             DROP TABLE IF EXISTS cost_trend_cache;",
        )?;
    }

    create_tables(conn)?;

    if version < 3 {
        migrate_to_v3(conn)?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO schema_version (version, applied_at) VALUES (?, ?)",
        params![APP_SCHEMA_VERSION, Utc::now().to_rfc3339()],
    )?;

    Ok(())
}

/// The current shape. Anything a migration already rebuilt is left alone.
///
/// The tables of this database, each as `(name, column definition, the
/// columns to carry over when it is rebuilt)`.
///
/// One definition per table, because a rebuild has to produce exactly what
/// a fresh install would: `CREATE TABLE ... AS SELECT` copies rows and
/// column types but *not* constraints, and a `cloud_accounts` without its
/// primary key cannot be written to at all — DuckDB implements
/// `INSERT OR REPLACE` as an upsert and refuses one with no key to conflict
/// on.
///
/// No foreign keys: DuckDB will not drop or alter a table another table
/// points at, which is what makes a rebuild necessary in the first place.
/// `delete_account` cleans up dependants instead.
const TABLES: &[(&str, &str, &str)] = &[
    (
        "cloud_accounts",
        r#"(
            id             VARCHAR PRIMARY KEY,
            name           VARCHAR NOT NULL,
            -- A registry SourceId; see cloud::registry. Stored verbatim, so
            -- these strings are part of the on-disk format.
            source_id      VARCHAR NOT NULL,
            region         VARCHAR,
            created_at     VARCHAR NOT NULL,
            last_synced_at VARCHAR,
            enabled        BOOLEAN NOT NULL DEFAULT true
        )"#,
        "id, name, source_id, region, created_at, last_synced_at, enabled",
    ),
    (
        "budgets",
        r#"(
            account_id      VARCHAR PRIMARY KEY,
            monthly_budget  DOUBLE NOT NULL,
            currency        VARCHAR NOT NULL,
            alert_threshold DOUBLE NOT NULL DEFAULT 80.0,
            created_at      VARCHAR NOT NULL,
            updated_at      VARCHAR NOT NULL
        )"#,
        "account_id, monthly_budget, currency, alert_threshold, created_at, updated_at",
    ),
];

fn create_tables(conn: &Connection) -> Result<()> {
    for (table, definition, _) in TABLES {
        conn.execute_batch(&format!("CREATE TABLE IF NOT EXISTS {table} {definition}"))?;
    }

    Ok(())
}

/// Whether a table has a primary key, which is what `INSERT OR REPLACE`
/// needs to exist at all.
fn has_primary_key(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM duckdb_constraints()
         WHERE table_name = ? AND constraint_type = 'PRIMARY KEY'",
        params![table],
        |row| row.get(0),
    )?;

    Ok(count > 0)
}

/// Rebuild a table in its declared shape, carrying the rows across.
fn rebuild_table(conn: &Connection, table: &str, definition: &str, columns: &str) -> Result<()> {
    let scratch = format!("{table}_rebuild");

    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {scratch};
         CREATE TABLE {scratch} {definition};
         INSERT INTO {scratch} ({columns}) SELECT {columns} FROM {table};
         DROP TABLE {table};
         ALTER TABLE {scratch} RENAME TO {table};"
    ))?;

    Ok(())
}

/// Give back the primary keys that the v1 rebuild dropped.
///
/// v1 moved rows with `CREATE TABLE ... AS SELECT`, which does not carry
/// constraints across. The tables looked right and read fine, so the loss
/// only surfaced on the next write: saving an account failed with "there
/// are no UNIQUE/PRIMARY KEY constraints that refer to this table". A
/// database that already has its keys — a fresh install — is left alone.
fn migrate_to_v3(conn: &Connection) -> Result<()> {
    for (table, definition, columns) in TABLES {
        if column_names(conn, table)?.is_empty() || has_primary_key(conn, table)? {
            continue;
        }

        tracing::info!("Restoring the primary key on {}", table);
        rebuild_table(conn, table, definition, columns)?;
    }

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

    // Written out in full rather than with CREATE TABLE AS SELECT, so the
    // primary key survives; see [`TABLES`].
    let (_, accounts_definition, _) = TABLES[0];
    conn.execute_batch(&format!(
        "CREATE OR REPLACE TABLE cloud_accounts_v1 {accounts_definition};
         INSERT INTO cloud_accounts_v1
             (id, name, source_id, region, created_at, last_synced_at, enabled)
         SELECT id, name, {source_column}, region, created_at, last_synced_at, enabled
         FROM cloud_accounts;
         DROP TABLE cloud_accounts;
         ALTER TABLE cloud_accounts_v1 RENAME TO cloud_accounts;"
    ))?;

    if has_budgets {
        let (_, budgets_definition, budgets_columns) = TABLES[1];
        conn.execute_batch(&format!(
            "CREATE TABLE budgets {budgets_definition};
             INSERT INTO budgets ({budgets_columns})
             SELECT {budgets_columns} FROM budgets_v1_backup;
             DROP TABLE budgets_v1_backup;"
        ))?;
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

    // What the ledger says has been charged this month. It is in the
    // reporting currency, while a budget carries a currency of its own;
    // reconciling the two belongs with the budget alerts in P2, which is
    // also where this function finally gets a caller.
    let period = BillingPeriod::containing(Utc::now());
    let current_cost = query::period_total(&PeriodKey::new(
        account.source_id.as_str().to_string(),
        account.id.clone(),
        period.label(),
    ))?;

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
        create_tables(&conn).unwrap();

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

    /// The write that `save_account` makes. DuckDB implements it as an
    /// upsert, so it needs a primary key to conflict on.
    fn upsert_account(conn: &Connection, id: &str, name: &str) -> Result<()> {
        conn.execute(
            r#"
            INSERT OR REPLACE INTO cloud_accounts
            (id, name, source_id, region, created_at, last_synced_at, enabled)
            VALUES (?, ?, 'AWS', 'us-east-1', '2026-08-01T00:00:00+00:00', NULL, true)
            "#,
            params![id, name],
        )?;

        Ok(())
    }

    #[test]
    fn an_upgraded_database_can_still_be_written_to() {
        let conn = legacy_database();
        prepare_schema(&conn).unwrap();

        upsert_account(&conn, "acct-1", "Prod renamed").unwrap();
        upsert_account(&conn, "acct-2", "Staging").unwrap();

        let (accounts, renamed): (i64, String) = conn
            .query_row(
                "SELECT count(*), max(name) FROM cloud_accounts WHERE id = 'acct-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        // Replacing an account updates it rather than duplicating it.
        assert_eq!(accounts, 1);
        assert_eq!(renamed, "Prod renamed");
    }

    #[test]
    fn a_database_that_lost_its_keys_gets_them_back() {
        // What 0.2.0 left behind: the v1 rebuild moved rows with
        // CREATE TABLE AS SELECT, which drops constraints.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at VARCHAR NOT NULL);
             INSERT INTO schema_version VALUES (2, '2026-09-01T00:00:00+00:00');
             CREATE TABLE cloud_accounts AS
                 SELECT 'acct-1' AS id, 'Prod' AS name, 'AWS' AS source_id,
                        'us-east-1' AS region, '2026-08-01T00:00:00+00:00' AS created_at,
                        NULL::VARCHAR AS last_synced_at, true AS enabled;
             CREATE TABLE budgets AS
                 SELECT 'acct-1' AS account_id, 100.0 AS monthly_budget, 'USD' AS currency,
                        80.0 AS alert_threshold, '2026-08-01T00:00:00+00:00' AS created_at,
                        '2026-08-01T00:00:00+00:00' AS updated_at;",
        )
        .unwrap();
        assert!(!has_primary_key(&conn, "cloud_accounts").unwrap());
        assert!(upsert_account(&conn, "acct-2", "Staging").is_err());

        prepare_schema(&conn).unwrap();

        assert!(has_primary_key(&conn, "cloud_accounts").unwrap());
        assert!(has_primary_key(&conn, "budgets").unwrap());
        assert_eq!(current_schema_version(&conn).unwrap(), APP_SCHEMA_VERSION);
        upsert_account(&conn, "acct-2", "Staging").unwrap();

        // The rebuild keeps what was there.
        let names: Vec<String> = conn
            .prepare("SELECT name FROM cloud_accounts ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(names, vec!["Prod".to_string(), "Staging".to_string()]);
        let budget: f64 = conn
            .query_row("SELECT monthly_budget FROM budgets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(budget, 100.0);
    }

    #[test]
    fn a_database_that_already_has_its_keys_is_left_alone() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_schema(&conn).unwrap();
        upsert_account(&conn, "acct-1", "Prod").unwrap();

        prepare_schema(&conn).unwrap();

        let accounts: i64 = conn
            .query_row("SELECT count(*) FROM cloud_accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(accounts, 1);
        assert!(has_primary_key(&conn, "cloud_accounts").unwrap());
    }

    #[test]
    fn the_response_caches_are_dropped_on_upgrade() {
        let conn = legacy_database();
        conn.execute_batch(
            "CREATE TABLE cost_summary_cache (account_id VARCHAR PRIMARY KEY);
             CREATE TABLE cost_trend_cache (account_id VARCHAR PRIMARY KEY);",
        )
        .unwrap();

        prepare_schema(&conn).unwrap();

        assert!(!table_exists(&conn, "cost_summary_cache"));
        assert!(!table_exists(&conn, "cost_trend_cache"));
        assert_eq!(current_schema_version(&conn).unwrap(), APP_SCHEMA_VERSION);
        // The account survives the upgrade that removed them.
        let accounts: i64 = conn
            .query_row("SELECT count(*) FROM cloud_accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(accounts, 1);
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
        create_tables(&conn).unwrap();

        let source_id: String = conn
            .query_row("SELECT source_id FROM cloud_accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(source_id, "AWS");
    }
}
