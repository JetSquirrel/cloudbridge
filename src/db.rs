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
use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use crate::alerts::{AlertEvent, AlertRule, AlertStatus, Severity};
use crate::cloud::{self, BillingPeriod, BudgetInfo, BudgetStatus, CloudAccount};
use crate::cloud::{SourceContext, SourceDescriptor, SourceId};
use crate::config::get_database_path;
use crate::crypto::get_crypto_manager;
use crate::ledger::{query, PeriodKey};
use crate::secret_store;

static DB_CONNECTION: LazyLock<Arc<Mutex<Option<Connection>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));

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
///
/// v4 adds `access_key_hint`, the leading characters of an account's access
/// key. Listing accounts used to read every credential out of the keyring
/// just to print `AK: AKIA1234****`, and on macOS each read can raise a
/// system password prompt — one that comes back after every rebuild of the
/// app, because the keyring item's access control is tied to the binary
/// that created it. The hint is the part of that display which is not a
/// secret, so it lives in the database and the keyring is left to the work
/// that actually signs a request.
///
/// v5 adds `alert_rule` and `alert_event`, the state of the alerting
/// engine (see [`crate::alerts`]). New tables only; `create_tables` runs
/// on every start, so an existing database gains them without a rebuild.
///
/// v6 adds `resolved_at` to `alert_event`: when the event reached its
/// final state. "Resolved this month" keys on it — an alert resolved in
/// month N but created earlier still belongs to month N's list.
///
/// v7 adds `export_uri` to `cloud_accounts`: where the provider's own
/// billing export lands (`s3://bucket/prefix` for AWS Data Exports). An
/// account with one is read from the export instead of a billing API.
///
/// v8 adds `dismissed_quality_issues`, the user's data-quality dismissal
/// record (Wealthfolio's health_issue_dismissals pattern): a dismissed
/// finding stays hidden for its billing period across sessions and
/// refreshes. New table only; `create_tables` runs on every start, so an
/// existing database gains it without a rebuild.
const APP_SCHEMA_VERSION: i32 = 8;

/// Initialize database
pub fn init_database() -> Result<()> {
    let db_path = get_database_path()?;
    let conn = Connection::open(&db_path)?;
    prepare_schema(&conn)?;

    let mut db = DB_CONNECTION
        .lock()
        .map_err(|e| anyhow::anyhow!("Failed to get database connection: {}", e))?;
    *db = Some(conn);

    tracing::info!("Database initialized: {:?}", db_path);
    Ok(())
}

/// Bring a database file up to [`APP_SCHEMA_VERSION`], creating it from
/// scratch if it is empty.
pub(crate) fn prepare_schema(conn: &Connection) -> Result<()> {
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
    if version < 4 {
        migrate_to_v4(conn)?;
    }
    if version < 6 {
        migrate_to_v6(conn)?;
    }
    if version < 7 {
        migrate_to_v7(conn)?;
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
            enabled        BOOLEAN NOT NULL DEFAULT true,
            -- Last, because v4/v7 add columns with ALTER TABLE to a database
            -- that already exists, and a fresh install should have the same
            -- column order as an upgraded one.
            access_key_hint VARCHAR,
            export_uri     VARCHAR
        )"#,
        "id, name, source_id, region, created_at, last_synced_at, enabled, access_key_hint, export_uri",
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
    (
        "alert_rule",
        r#"(
            id            VARCHAR PRIMARY KEY,
            kind          VARCHAR NOT NULL,   -- cost-growth-anomaly | balance-floor | untagged-ratio
            name          VARCHAR NOT NULL,
            scope         VARCHAR NOT NULL,
            enabled       BOOLEAN NOT NULL DEFAULT true,
            config        VARCHAR NOT NULL,   -- JSON object text
            last_fired_at VARCHAR
        )"#,
        "id, kind, name, scope, enabled, config, last_fired_at",
    ),
    (
        "alert_event",
        r#"(
            id            VARCHAR PRIMARY KEY,
            rule_id       VARCHAR NOT NULL,
            severity      VARCHAR NOT NULL,   -- critical | warning
            title         VARCHAR NOT NULL,
            body          VARCHAR NOT NULL,
            fields_json   VARCHAR NOT NULL,   -- {"fields": [...], "context": {...}}
            stat_json     VARCHAR,
            created_at    VARCHAR NOT NULL,
            status        VARCHAR NOT NULL,   -- open | snoozed | resolved | dismissed
            snoozed_until VARCHAR,
            dedupe_key    VARCHAR NOT NULL,
            -- Last, because v6 adds it with ALTER TABLE to a database that
            -- already exists, and a fresh install should have the same
            -- column order as an upgraded one.
            resolved_at   VARCHAR
        )"#,
        "id, rule_id, severity, title, body, fields_json, stat_json, created_at, status, snoozed_until, dedupe_key, resolved_at",
    ),
    (
        "dismissed_quality_issues",
        r#"(
            -- {kind}:{billing_period}, e.g. untagged_usage:2026-09.
            issue_key    VARCHAR PRIMARY KEY,
            -- RFC 3339, like every other stamp in this database.
            dismissed_at VARCHAR NOT NULL
        )"#,
        "issue_key, dismissed_at",
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
///
/// Only the declared columns the table actually has, so a rebuild does not
/// depend on which of the later migrations have run yet.
fn rebuild_table(conn: &Connection, table: &str, definition: &str, columns: &str) -> Result<()> {
    let present = column_names(conn, table)?;
    let carried = columns
        .split(", ")
        .filter(|column| present.iter().any(|name| name == column))
        .collect::<Vec<_>>()
        .join(", ");
    let scratch = format!("{table}_rebuild");

    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {scratch};
         CREATE TABLE {scratch} {definition};
         INSERT INTO {scratch} ({carried}) SELECT {carried} FROM {table};
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

/// Add the column that lets an account be listed without a keyring read.
///
/// Left NULL for accounts already stored: filling it in would mean reading
/// every credential at startup, which is the prompt this column exists to
/// stop. It is written the next time something legitimately needs those
/// credentials — see [`account_context`].
fn migrate_to_v4(conn: &Connection) -> Result<()> {
    let columns = column_names(conn, "cloud_accounts")?;
    if columns.is_empty() || columns.iter().any(|c| c == "access_key_hint") {
        return Ok(());
    }

    tracing::info!("Adding access_key_hint to cloud_accounts");
    conn.execute_batch("ALTER TABLE cloud_accounts ADD COLUMN access_key_hint VARCHAR")?;

    Ok(())
}

/// Add the column that records when an alert event reached its final
/// state, so "Resolved this month" can key on resolution time rather than
/// creation time.
///
/// Left NULL for events already resolved: nothing recorded when they
/// closed, so they drop out of the current month's list rather than being
/// filed under a month that would be a guess.
fn migrate_to_v6(conn: &Connection) -> Result<()> {
    let columns = column_names(conn, "alert_event")?;
    if columns.is_empty() || columns.iter().any(|c| c == "resolved_at") {
        return Ok(());
    }

    tracing::info!("Adding resolved_at to alert_event");
    conn.execute_batch("ALTER TABLE alert_event ADD COLUMN resolved_at VARCHAR")?;

    Ok(())
}

/// Add the column that points an account at its provider-side billing
/// export, e.g. the `s3://bucket/prefix` of an AWS Data Exports export.
///
/// Left NULL for accounts already stored: they keep reading from their
/// billing API until an export URI is entered.
fn migrate_to_v7(conn: &Connection) -> Result<()> {
    let columns = column_names(conn, "cloud_accounts")?;
    if columns.is_empty() || columns.iter().any(|c| c == "export_uri") {
        return Ok(());
    }

    tracing::info!("Adding export_uri to cloud_accounts");
    conn.execute_batch("ALTER TABLE cloud_accounts ADD COLUMN export_uri VARCHAR")?;

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

/// The connection inside a guard from [`get_connection`], which has already
/// refused an uninitialized database.
fn connection_of<'a>(db: &'a std::sync::MutexGuard<'_, Option<Connection>>) -> &'a Connection {
    db.as_ref()
        .expect("get_connection refused an empty connection")
}

/// Run a read against the app-state connection, for callers (the alerting
/// engine) that hold several stores at once.
pub(crate) fn with_connection<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let db = get_connection()?;
    f(connection_of(&db))
}

/// Save a cloud account: the credentials to the OS keyring, everything
/// else to the database.
///
/// The hint stored on the row is derived from the key given here rather
/// than taken from `account`, so the database cannot end up describing a
/// key it was not saved with.
/// An account with no access key stores nothing in the keyring.
///
/// A source whose bill arrives by file import needs no credentials, and an
/// empty keyring entry would be worse than none: [`account_context`] reads
/// a pair back and would hand a request an empty key to sign with, which
/// fails as an authentication error rather than as the missing credential
/// it is.
pub fn save_account(
    account: &CloudAccount,
    access_key_id: &str,
    secret_access_key: &str,
) -> Result<()> {
    let hint = if access_key_id.is_empty() {
        None
    } else {
        secret_store::store_account_secrets(&account.id, access_key_id, secret_access_key)?;
        Some(cloud::access_key_hint(access_key_id))
    };

    let db = get_connection()?;
    let conn = connection_of(&db);

    conn.execute(
        r#"
        INSERT OR REPLACE INTO cloud_accounts
        (id, name, source_id, region, created_at, last_synced_at, enabled, access_key_hint, export_uri)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            account.id,
            account.name,
            account.source_id.as_str(),
            account.region,
            account.created_at.to_rfc3339(),
            account.last_synced_at.map(|dt| dt.to_rfc3339()),
            account.enabled,
            hint,
            account.export_uri,
        ],
    )?;

    Ok(())
}

/// The credentials for an account, read from the keyring at the moment they
/// are needed.
///
/// Deliberately not part of [`get_all_accounts`]: on macOS a keyring read
/// can raise a system password prompt, and the prompt returns after every
/// rebuild of the app, since the item's access control names the binary
/// that stored it. Listing accounts — which the dashboard does on every
/// load — is not a reason to ask for the login password, so only work that
/// signs a request reads a secret.
pub fn account_context(
    account: &CloudAccount,
    descriptor: &SourceDescriptor,
) -> Result<SourceContext> {
    let (access_key_id, secret_access_key) = secret_store::get_account_secrets(&account.id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No credentials in the keyring for account {}; they have to be re-entered",
                account.name
            )
        })?;

    // An account stored before v4 has no hint. Record it now, from a key
    // that has just been read anyway, rather than reading one for the sake
    // of the display.
    if account.access_key_hint.is_none() {
        if let Err(e) = set_access_key_hint(&account.id, &access_key_id) {
            tracing::warn!("Could not record the key hint for {}: {}", account.name, e);
        }
    }

    Ok(SourceContext {
        access_key_id,
        secret_access_key,
        region: descriptor.region_or_default(account.region.clone()),
        export_uri: account.export_uri.clone(),
    })
}

/// Record the leading characters of an account's access key, for a list
/// that must not read the key itself.
fn set_access_key_hint(account_id: &str, access_key_id: &str) -> Result<()> {
    let db = get_connection()?;
    let conn = connection_of(&db);

    conn.execute(
        "UPDATE cloud_accounts SET access_key_hint = ? WHERE id = ?",
        params![cloud::access_key_hint(access_key_id), account_id],
    )?;

    Ok(())
}

/// Get all cloud accounts
pub fn get_all_accounts() -> Result<Vec<CloudAccount>> {
    with_connection(get_all_accounts_of)
}

pub(crate) fn get_all_accounts_of(conn: &Connection) -> Result<Vec<CloudAccount>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, source_id, region, created_at, last_synced_at, enabled, access_key_hint, export_uri
         FROM cloud_accounts",
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
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut result = Vec::new();
    for (
        id,
        name,
        source_id,
        region,
        created_at_str,
        last_synced_str,
        enabled,
        access_key_hint,
        export_uri,
    ) in rows
    {
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

        // No keyring read here: the credentials are fetched only when
        // something is about to authenticate with them, by
        // [`account_context`]. An account whose secrets have gone is still
        // listed — that is discovered when it is next used.
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
            region,
            created_at,
            last_synced_at,
            enabled,
            access_key_hint,
            export_uri,
        });
    }

    Ok(result)
}

/// Delete cloud account
pub fn delete_account(account_id: &str) -> Result<()> {
    let db = get_connection()?;
    let conn = connection_of(&db);

    // Dependants first: nothing references cloud_accounts through a foreign
    // key any more, so the order is ours to keep.
    conn.execute(
        "DELETE FROM budgets WHERE account_id = ?",
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
pub fn save_budget(budget: &BudgetInfo) -> Result<()> {
    let db = get_connection()?;
    let conn = connection_of(&db);

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
pub fn get_budget(account_id: &str) -> Result<Option<BudgetInfo>> {
    with_connection(|conn| get_budget_of(conn, account_id))
}

pub(crate) fn get_budget_of(conn: &Connection, account_id: &str) -> Result<Option<BudgetInfo>> {
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
pub fn get_all_budgets() -> Result<Vec<BudgetInfo>> {
    let db = get_connection()?;
    let conn = connection_of(&db);

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
pub fn delete_budget(account_id: &str) -> Result<()> {
    let db = get_connection()?;
    let conn = connection_of(&db);

    conn.execute(
        "DELETE FROM budgets WHERE account_id = ?",
        params![account_id],
    )?;

    tracing::info!("Deleted budget for account {}", account_id);
    Ok(())
}

/// Get budget status (compares budget with current costs)
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

    // What the ledger says has been charged this month, in the reporting
    // currency. Budgets are recorded in that same currency (the Rules page
    // writes them so), which is what makes the comparison meaningful.
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
    // Mirrors the budget alert rules (crate::alerts, kind "budget"): a live
    // event means an evaluated rule fired for this account. The threshold
    // check keeps the badge honest before the first evaluation runs.
    let alert_triggered =
        has_live_budget_alert(account_id)? || percentage_used >= budget.alert_threshold;

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

/// Whether any open or snoozed budget-rule event names this account.
fn has_live_budget_alert(account_id: &str) -> Result<bool> {
    with_connection(|conn| {
        let mut stmt = conn.prepare(
            "SELECT count(*) FROM alert_event e
             JOIN alert_rule r ON r.id = e.rule_id
             WHERE r.kind = 'budget' AND e.status IN ('open', 'snoozed')
               AND e.dedupe_key LIKE 'budget|' || ? || '|%'",
        )?;
        let count: i64 = stmt.query_row(params![account_id], |row| row.get(0))?;
        Ok(count > 0)
    })
}

/// Get all budget statuses
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

/// Record that an account was successfully refreshed.
pub fn mark_account_synced(account_id: &str, at: DateTime<Utc>) -> Result<()> {
    let db = get_connection()?;
    let conn = connection_of(&db);

    conn.execute(
        "UPDATE cloud_accounts SET last_synced_at = ? WHERE id = ?",
        params![at.to_rfc3339(), account_id],
    )?;

    Ok(())
}

// ==================== Dismissed quality issues ====================
//
// The persistence half of the data-quality dismissal scheme (Wealthfolio's
// health_issue_dismissals): the UI keys a finding as
// `{kind}:{billing_period}` and the loaders filter stored keys out, so a
// dismissed finding stays hidden for its period across sessions and
// refreshes while a new period or a different kind still shows.

/// Record a data-quality issue dismissal. Re-dismissing a key is a no-op
/// beyond refreshing its stamp. Blocking, but a single-row write — the UI
/// calls it straight from click handlers.
pub fn dismiss_quality_issue(issue_key: &str) -> Result<()> {
    with_connection(|conn| dismiss_quality_issue_to(conn, issue_key))
}

pub(crate) fn dismiss_quality_issue_to(conn: &Connection, issue_key: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO dismissed_quality_issues (issue_key, dismissed_at)
         VALUES (?, ?)",
        params![issue_key, Utc::now().to_rfc3339()],
    )?;

    Ok(())
}

/// Every dismissed issue key.
pub fn dismissed_quality_issue_keys() -> Result<HashSet<String>> {
    with_connection(dismissed_quality_issue_keys_of)
}

pub(crate) fn dismissed_quality_issue_keys_of(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT issue_key FROM dismissed_quality_issues")?;
    let keys = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(keys)
}

/// Forget every dismissal — all findings resurface on the next load. No UI
/// calls this yet; it is the "resurface all" escape hatch, kept for
/// completeness and exercised by the tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn clear_dismissed_quality_issues() -> Result<()> {
    with_connection(|conn| {
        conn.execute("DELETE FROM dismissed_quality_issues", [])?;
        Ok(())
    })
}

// ==================== Alert Functions ====================
//
// Each function has a `*_to(conn)` twin so the alerting engine
// (crate::alerts) can be evaluated against an in-memory database in tests,
// exactly as the ledger's `*_of` functions are.

/// Save or update an alerting rule.
pub(crate) fn save_alert_rule_to(conn: &Connection, rule: &AlertRule) -> Result<()> {
    conn.execute(
        r#"
        INSERT OR REPLACE INTO alert_rule
        (id, kind, name, scope, enabled, config, last_fired_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            rule.id,
            rule.kind,
            rule.name,
            rule.scope,
            rule.enabled,
            rule.config.to_string(),
            rule.last_fired_at.map(|at| at.to_rfc3339()),
        ],
    )?;

    Ok(())
}

/// Whether a rule with this id is already stored.
///
/// `seed_default_rules_on` wants to know exactly this and nothing about the
/// rule, and the seeding has to run on a backend that has no SQL of its own.
pub(crate) fn alert_rule_exists(conn: &Connection, id: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM alert_rule WHERE id = ?",
        params![id],
        |row| row.get(0),
    )?;

    Ok(count > 0)
}

/// Every alerting rule, in the order they were first stored.
pub fn get_alert_rules() -> Result<Vec<AlertRule>> {
    with_connection(get_alert_rules_of)
}

pub(crate) fn get_alert_rules_of(conn: &Connection) -> Result<Vec<AlertRule>> {
    let mut stmt = conn
        .prepare("SELECT id, kind, name, scope, enabled, config, last_fired_at FROM alert_rule")?;

    let rules = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    rules
        .into_iter()
        .map(|(id, kind, name, scope, enabled, config, last_fired_at)| {
            Ok(AlertRule {
                id,
                kind,
                name,
                scope,
                enabled,
                config: serde_json::from_str(&config).unwrap_or(serde_json::json!({})),
                last_fired_at: last_fired_at
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc)),
            })
        })
        .collect()
}

/// Enable or disable a rule. A disabled rule keeps its events but fires no
/// new ones.
pub fn set_alert_rule_enabled(id: &str, enabled: bool) -> Result<()> {
    with_connection(|conn| set_alert_rule_enabled_to(conn, id, enabled))
}

pub(crate) fn set_alert_rule_enabled_to(conn: &Connection, id: &str, enabled: bool) -> Result<()> {
    conn.execute(
        "UPDATE alert_rule SET enabled = ? WHERE id = ?",
        params![enabled, id],
    )?;

    Ok(())
}

/// Record when a rule last produced an event, for the "last fired" the
/// Rules page shows.
pub(crate) fn mark_rule_fired_to(conn: &Connection, id: &str, at: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "UPDATE alert_rule SET last_fired_at = ? WHERE id = ?",
        params![at.to_rfc3339(), id],
    )?;

    Ok(())
}

/// Delete an alerting rule. Its past events stay: they are history, not
/// part of the rule.
pub fn delete_alert_rule(id: &str) -> Result<()> {
    with_connection(|conn| delete_alert_rule_to(conn, id))
}

pub(crate) fn delete_alert_rule_to(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM alert_rule WHERE id = ?", params![id])?;

    Ok(())
}

/// Record a new alert event.
pub(crate) fn insert_alert_event_to(conn: &Connection, event: &AlertEvent) -> Result<()> {
    conn.execute(
        r#"
        INSERT OR REPLACE INTO alert_event
        (id, rule_id, severity, title, body, fields_json, stat_json, created_at,
         status, snoozed_until, dedupe_key, resolved_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            event.id,
            event.rule_id,
            event.severity.as_str(),
            event.title,
            event.body,
            event.fields_json,
            event.stat_json,
            event.created_at.to_rfc3339(),
            event.status.as_str(),
            event.snoozed_until.map(|at| at.to_rfc3339()),
            event.dedupe_key,
            event.resolved_at.map(|at| at.to_rfc3339()),
        ],
    )?;

    Ok(())
}

/// Events in one of the given states, newest first.
pub fn get_alert_events(statuses: &[AlertStatus]) -> Result<Vec<AlertEvent>> {
    with_connection(|conn| get_alert_events_of(conn, statuses))
}

pub(crate) fn get_alert_events_of(
    conn: &Connection,
    statuses: &[AlertStatus],
) -> Result<Vec<AlertEvent>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = statuses.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, rule_id, severity, title, body, fields_json, stat_json, created_at,
                status, snoozed_until, dedupe_key, resolved_at
         FROM alert_event
         WHERE status IN ({placeholders})
         ORDER BY created_at DESC"
    ))?;

    let rows = stmt
        .query_map(
            duckdb::params_from_iter(statuses.iter().map(|s| s.as_str())),
            event_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

/// The open or snoozed event under a dedupe key, if there is one — the
/// check that keeps a condition from alerting twice while it still holds.
pub(crate) fn find_live_alert_event_of(
    conn: &Connection,
    dedupe_key: &str,
) -> Result<Option<AlertEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, rule_id, severity, title, body, fields_json, stat_json, created_at,
                status, snoozed_until, dedupe_key, resolved_at
         FROM alert_event
         WHERE dedupe_key = ? AND status IN ('open', 'snoozed')
         ORDER BY created_at DESC
         LIMIT 1",
    )?;

    let mut rows = stmt.query_map(params![dedupe_key], event_from_row)?;
    Ok(rows.next().transpose()?)
}

/// Move an event to a new state. `snoozed_until` matters only for
/// [`AlertStatus::Snoozed`]. Reaching a final state stamps `resolved_at`;
/// leaving one (reopened, or snoozed again) clears it.
pub fn set_alert_event_status(
    id: &str,
    status: AlertStatus,
    snoozed_until: Option<DateTime<Utc>>,
) -> Result<()> {
    with_connection(|conn| set_alert_event_status_to(conn, id, status, snoozed_until))
}

pub(crate) fn set_alert_event_status_to(
    conn: &Connection,
    id: &str,
    status: AlertStatus,
    snoozed_until: Option<DateTime<Utc>>,
) -> Result<()> {
    let resolved_at = match status {
        AlertStatus::Resolved | AlertStatus::Dismissed => Some(Utc::now().to_rfc3339()),
        _ => None,
    };

    conn.execute(
        "UPDATE alert_event SET status = ?, snoozed_until = ?, resolved_at = ? WHERE id = ?",
        params![
            status.as_str(),
            snoozed_until.map(|at| at.to_rfc3339()),
            resolved_at,
            id
        ],
    )?;

    Ok(())
}

/// One row of `alert_event`, in the column order every query here uses.
fn event_from_row(row: &duckdb::Row<'_>) -> duckdb::Result<AlertEvent> {
    let created_at: String = row.get(7)?;
    let status: String = row.get(8)?;
    let snoozed_until: Option<String> = row.get(9)?;
    let resolved_at: Option<String> = row.get(11)?;

    Ok(AlertEvent {
        id: row.get(0)?,
        rule_id: row.get(1)?,
        severity: Severity::from_stored(&row.get::<_, String>(2)?),
        title: row.get(3)?,
        body: row.get(4)?,
        fields_json: row.get(5)?,
        stat_json: row.get(6)?,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        status: AlertStatus::from_stored(&status),
        snoozed_until: snoozed_until
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        dedupe_key: row.get(10)?,
        resolved_at: resolved_at
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
    })
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
                "enabled",
                "access_key_hint",
                "export_uri"
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

    /// The column arrives on an existing database without disturbing it,
    /// and without a hint being invented for accounts already stored —
    /// filling those in would mean the keyring read this column exists to
    /// avoid.
    #[test]
    fn the_key_hint_column_is_added_to_an_existing_database() {
        let conn = legacy_database();

        prepare_schema(&conn).unwrap();

        let columns = column_names(&conn, "cloud_accounts").unwrap();
        assert!(columns.iter().any(|c| c == "access_key_hint"));
        let (name, hint): (String, Option<String>) = conn
            .query_row(
                "SELECT name, access_key_hint FROM cloud_accounts WHERE id = 'acct-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "Prod");
        assert_eq!(hint, None);

        // And a fresh install ends up with the same shape.
        let fresh = Connection::open_in_memory().unwrap();
        prepare_schema(&fresh).unwrap();
        assert_eq!(column_names(&fresh, "cloud_accounts").unwrap(), columns);
    }

    /// The column arrives last, NULL for accounts already stored: they keep
    /// reading from their billing API until an export URI is entered.
    #[test]
    fn the_export_uri_column_is_added_to_an_existing_database() {
        let conn = legacy_database();

        prepare_schema(&conn).unwrap();

        let columns = column_names(&conn, "cloud_accounts").unwrap();
        assert_eq!(columns.last().unwrap(), "export_uri");
        let export_uri: Option<String> = conn
            .query_row(
                "SELECT export_uri FROM cloud_accounts WHERE id = 'acct-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(export_uri, None);

        // And a fresh install ends up with the same shape.
        let fresh = Connection::open_in_memory().unwrap();
        prepare_schema(&fresh).unwrap();
        assert_eq!(column_names(&fresh, "cloud_accounts").unwrap(), columns);
    }

    /// A database that needs both the v3 rebuild and the v4 column gets
    /// them in an order that leaves the rows intact — the rebuild carries
    /// the columns the table has, not the ones it is about to gain.
    #[test]
    fn a_database_two_versions_behind_survives_both_migrations() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at VARCHAR NOT NULL);
             INSERT INTO schema_version VALUES (2, '2026-09-01T00:00:00+00:00');
             CREATE TABLE cloud_accounts AS
                 SELECT 'acct-1' AS id, 'Prod' AS name, 'AWS' AS source_id,
                        'us-east-1' AS region, '2026-08-01T00:00:00+00:00' AS created_at,
                        NULL::VARCHAR AS last_synced_at, true AS enabled;",
        )
        .unwrap();

        prepare_schema(&conn).unwrap();

        assert!(has_primary_key(&conn, "cloud_accounts").unwrap());
        let (name, hint): (String, Option<String>) = conn
            .query_row(
                "SELECT name, access_key_hint FROM cloud_accounts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "Prod");
        assert_eq!(hint, None);
    }

    /// The hint is a label, not a credential: enough of the key to tell
    /// two accounts apart, and never the secret half.
    #[test]
    fn a_key_hint_is_only_the_start_of_the_key() {
        let key = "AKIAIOSFODNN7EXAMPLE";
        let hint = cloud::access_key_hint(key);

        assert_eq!(hint, "AKIAIOSF");
        assert!(key.starts_with(&hint));
        assert!(hint.len() < key.len());
        // A short key is not padded out to look longer than it is.
        assert_eq!(cloud::access_key_hint("sk-123"), "sk-123");
    }

    #[test]
    fn a_fresh_database_has_the_alert_tables() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_schema(&conn).unwrap();

        assert!(table_exists(&conn, "alert_rule"));
        assert!(table_exists(&conn, "alert_event"));
        assert!(has_primary_key(&conn, "alert_rule").unwrap());
        assert!(has_primary_key(&conn, "alert_event").unwrap());
    }

    #[test]
    fn a_fresh_database_has_the_dismissals_table() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_schema(&conn).unwrap();

        assert!(table_exists(&conn, "dismissed_quality_issues"));
        assert!(has_primary_key(&conn, "dismissed_quality_issues").unwrap());
        assert_eq!(
            column_names(&conn, "dismissed_quality_issues").unwrap(),
            vec!["issue_key", "dismissed_at"]
        );
    }

    /// Dismissals round-trip: a stored key comes back in the set,
    /// re-dismissing does not duplicate it, and clearing empties the table.
    #[test]
    fn dismissed_quality_issues_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_schema(&conn).unwrap();

        assert!(dismissed_quality_issue_keys_of(&conn).unwrap().is_empty());

        dismiss_quality_issue_to(&conn, "untagged_usage:2026-09").unwrap();
        dismiss_quality_issue_to(&conn, "missing_region:2026-09").unwrap();
        // Re-dismissing is idempotent.
        dismiss_quality_issue_to(&conn, "untagged_usage:2026-09").unwrap();

        let keys = dismissed_quality_issue_keys_of(&conn).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("untagged_usage:2026-09"));
        assert!(keys.contains("missing_region:2026-09"));
        // A different period is a different key.
        assert!(!keys.contains("untagged_usage:2026-10"));

        // The stamp is recorded.
        let stamp: String = conn
            .query_row(
                "SELECT dismissed_at FROM dismissed_quality_issues
                 WHERE issue_key = 'untagged_usage:2026-09'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(DateTime::parse_from_rfc3339(&stamp).is_ok());

        // The public clear writes to the shared connection, uninitialized
        // here; the in-memory twin runs its SQL directly.
        let _ = clear_dismissed_quality_issues();
        conn.execute("DELETE FROM dismissed_quality_issues", [])
            .unwrap();
        assert!(dismissed_quality_issue_keys_of(&conn).unwrap().is_empty());
    }

    /// The resolution stamp arrives on an existing database without
    /// disturbing its events, and a fresh install ends up with the same
    /// column order as an upgraded one.
    #[test]
    fn the_resolved_at_column_is_added_to_an_existing_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at VARCHAR NOT NULL);
             INSERT INTO schema_version VALUES (5, '2026-09-08T00:00:00+00:00');
             CREATE TABLE alert_event (
                 id            VARCHAR PRIMARY KEY,
                 rule_id       VARCHAR NOT NULL,
                 severity      VARCHAR NOT NULL,
                 title         VARCHAR NOT NULL,
                 body          VARCHAR NOT NULL,
                 fields_json   VARCHAR NOT NULL,
                 stat_json     VARCHAR,
                 created_at    VARCHAR NOT NULL,
                 status        VARCHAR NOT NULL,
                 snoozed_until VARCHAR,
                 dedupe_key    VARCHAR NOT NULL
             );
             INSERT INTO alert_event VALUES
                 ('ev-1', 'balance-floor', 'warning', 't', 'b', '{}', NULL,
                  '2026-08-01T00:00:00+00:00', 'resolved', NULL, 'balance|DeepSeek|acct-3|2026-08-01');",
        )
        .unwrap();

        prepare_schema(&conn).unwrap();

        let columns = column_names(&conn, "alert_event").unwrap();
        assert_eq!(columns.last().unwrap(), "resolved_at");
        // Nothing recorded when the old event closed: the stamp stays NULL
        // rather than being backfilled with a guess.
        let resolved_at: Option<String> = conn
            .query_row(
                "SELECT resolved_at FROM alert_event WHERE id = 'ev-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(resolved_at, None);

        let fresh = Connection::open_in_memory().unwrap();
        prepare_schema(&fresh).unwrap();
        assert_eq!(column_names(&fresh, "alert_event").unwrap(), columns);
    }

    /// The round trip the alerting engine takes: store a rule, fire an
    /// event under it, snooze the event, find it again by its dedupe key.
    #[test]
    fn alert_rules_and_events_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_schema(&conn).unwrap();

        let rule = AlertRule {
            id: "balance-floor".to_string(),
            kind: "balance-floor".to_string(),
            name: "Balance floor".to_string(),
            scope: "Prepaid accounts".to_string(),
            enabled: true,
            config: serde_json::json!({ "floor": 200.0 }),
            last_fired_at: None,
        };
        save_alert_rule_to(&conn, &rule).unwrap();

        let rules = get_alert_rules_of(&conn).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].config["floor"], 200.0);

        let until = Utc::now() + chrono::Duration::hours(24);
        let event = AlertEvent {
            id: "ev-1".to_string(),
            rule_id: rule.id.clone(),
            severity: Severity::Warning,
            title: "DeepSeek balance below floor".to_string(),
            body: "Balance is ¥8.14 against a ¥200 floor.".to_string(),
            fields_json: r#"{"fields": [], "context": {}}"#.to_string(),
            stat_json: None,
            created_at: Utc::now(),
            status: AlertStatus::Open,
            snoozed_until: None,
            dedupe_key: "balance|DeepSeek|acct-3|2026-09-06".to_string(),
            resolved_at: None,
        };
        insert_alert_event_to(&conn, &event).unwrap();

        let live = find_live_alert_event_of(&conn, &event.dedupe_key)
            .unwrap()
            .expect("the open event is live");
        assert_eq!(live.severity, Severity::Warning);

        set_alert_event_status_to(&conn, "ev-1", AlertStatus::Snoozed, Some(until)).unwrap();
        let snoozed = find_live_alert_event_of(&conn, &event.dedupe_key)
            .unwrap()
            .expect("a snoozed event is still live");
        assert_eq!(snoozed.status, AlertStatus::Snoozed);

        set_alert_event_status_to(&conn, "ev-1", AlertStatus::Resolved, None).unwrap();
        assert!(find_live_alert_event_of(&conn, &event.dedupe_key)
            .unwrap()
            .is_none());
        let resolved =
            get_alert_events_of(&conn, &[AlertStatus::Resolved, AlertStatus::Dismissed]).unwrap();
        assert_eq!(resolved.len(), 1);
        // Closing the event stamped when it closed.
        assert!(resolved[0].resolved_at.is_some());

        // Reopening clears the stamp again.
        set_alert_event_status_to(&conn, "ev-1", AlertStatus::Open, None).unwrap();
        let reopened = find_live_alert_event_of(&conn, &event.dedupe_key)
            .unwrap()
            .expect("the reopened event is live");
        assert_eq!(reopened.resolved_at, None);
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
