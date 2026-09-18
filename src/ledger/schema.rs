//! Physical schema of the billing ledger.
//!
//! Column names follow [FOCUS](https://focus.finops.org/) so that a later
//! ingest of a real CUR or Alibaba Cloud bill export needs no schema change.
//! Only the three concepts that carry their weight for a personal ledger are
//! implemented: `billed_cost`, `effective_cost` and `charge_category`.
//!
//! Timestamps are stored as `TIMESTAMP` in UTC. Values are bound as
//! `'%Y-%m-%d %H:%M:%S'` strings through an explicit `CAST`, and read back
//! through `CAST(col AS VARCHAR)`, so no DuckDB feature flag is needed to
//! move a `DateTime<Utc>` in or out.

use anyhow::Result;
use chrono::Utc;
use duckdb::{params, Connection};

/// Bumped whenever the statements below change shape.
///
/// v2 adds `ingest_batch.channel`: whether a period was fetched from a
/// billing API or imported from the provider's own bill export. The two
/// are not interchangeable — an export is the same month at instance
/// level — so a refresh has to be able to tell that a month was imported
/// and leave it alone.
///
/// v3 adds `daily_cost_rollup`, the derived day-grain read-model
/// [`crate::ledger::rollup`] maintains. A new table needs no `ALTER`: the
/// `CREATE TABLE IF NOT EXISTS` below is the whole migration, for a fresh
/// file and an upgraded one alike.
pub const SCHEMA_VERSION: i32 = 3;

/// The view the application reads: every charge with its amount also
/// expressed in the reporting currency.
pub const NORMALIZED_VIEW: &str = "v_charge_normalized";

/// Rates shipped with the build, as `(from, to, date, rate)`.
///
/// Static and approximate. They are dated because a rate gets corrected
/// and because a charge must be converted at a rate from its own time, not
/// from today's — so a real feed, when it arrives, only has to insert rows
/// with later dates. Nothing here is overwritten by it.
///
/// Only the currencies the sources actually bill in are covered: USD (AWS,
/// DeepSeek) and CNY (Alibaba Cloud, DeepSeek).
pub const BUILTIN_RATES: &[(&str, &str, &str, f64)] = &[
    ("USD", "CNY", "2026-01-01", 7.10),
    ("CNY", "USD", "2026-01-01", 0.1408),
];

/// Format used for every `TIMESTAMP` bind and parse in this module.
pub const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Create the ledger tables and record the schema version.
///
/// Idempotent: safe to call on every start.
pub fn apply(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            applied_at TIMESTAMP NOT NULL
        );

        -- One ingest of one (provider, account, billing period). Whole-period
        -- replacement is keyed on the same triple, so a batch is the unit
        -- P3 month-end freezing will pin a period to.
        CREATE TABLE IF NOT EXISTS ingest_batch (
            batch_id       VARCHAR PRIMARY KEY,
            provider       VARCHAR NOT NULL,
            account_id     VARCHAR NOT NULL,
            billing_period VARCHAR NOT NULL,   -- YYYY-MM
            started_at     TIMESTAMP NOT NULL,
            completed_at   TIMESTAMP,
            status         VARCHAR NOT NULL,   -- complete | superseded
            row_count      BIGINT NOT NULL DEFAULT 0,
            -- Path of the raw payload this batch was normalized from.
            -- Filled in by PR3, once fetch persists Parquet.
            source_ref     VARCHAR,
            -- api | file. Last, because v2 adds it with ALTER TABLE to a
            -- ledger that already exists, and a fresh file should have the
            -- same column order as an upgraded one. Read through
            -- coalesce(): the added column is nullable, since DuckDB will
            -- not add a NOT NULL one to a table that already has rows.
            channel        VARCHAR NOT NULL DEFAULT 'api'
        );

        -- The fact table. One row per charge, in the currency the provider
        -- billed it in; conversion happens in a view (PR6), never here.
        CREATE TABLE IF NOT EXISTS fct_charge (
            charge_id           VARCHAR PRIMARY KEY,
            batch_id            VARCHAR NOT NULL,
            provider            VARCHAR NOT NULL,
            account_id          VARCHAR NOT NULL,
            billing_account_id  VARCHAR,
            billing_period      VARCHAR NOT NULL,   -- YYYY-MM
            charge_period_start TIMESTAMP NOT NULL,
            charge_period_end   TIMESTAMP NOT NULL,
            charge_category     VARCHAR NOT NULL,   -- Usage | Purchase | Credit | Tax | Adjustment
            charge_description  VARCHAR,
            service_name        VARCHAR,
            service_category    VARCHAR,
            resource_id         VARCHAR,
            resource_name       VARCHAR,
            region_id           VARCHAR,
            -- Nullable on purpose: a usage record with no authoritative
            -- amount is representable, and `cost_basis` says which kind of
            -- figure this is so the UI can mark a derived one.
            billed_cost         DOUBLE,
            effective_cost      DOUBLE,
            list_cost           DOUBLE,
            billing_currency    VARCHAR NOT NULL,
            cost_basis          VARCHAR NOT NULL,   -- authoritative | derived | estimated | absent
            pricing_quantity    DOUBLE,
            -- Not restricted to cloud units: holds GB-Mo and Hrs today,
            -- Tokens when model-provider usage lands.
            pricing_unit        VARCHAR,
            tags                VARCHAR,            -- JSON object text
            created_at          TIMESTAMP NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_charge_period
            ON fct_charge (provider, account_id, billing_period);
        CREATE INDEX IF NOT EXISTS idx_charge_batch
            ON fct_charge (batch_id);

        -- A balance is state, not a charge: sources that only report one
        -- (DeepSeek today) land here, and only their top-ups become charges.
        CREATE TABLE IF NOT EXISTS fct_balance_snapshot (
            provider          VARCHAR NOT NULL,
            account_id        VARCHAR NOT NULL,
            observed_at       TIMESTAMP NOT NULL,
            balance           DOUBLE NOT NULL,
            granted_balance   DOUBLE,
            topped_up_balance DOUBLE,
            currency          VARCHAR NOT NULL,
            created_at        TIMESTAMP NOT NULL,
            -- Currency is part of the key: an account can hold a balance in
            -- more than one, and they are observed at the same instant.
            PRIMARY KEY (provider, account_id, observed_at, currency)
        );

        -- Rates are dated because they get corrected, and the reporting
        -- currency is the user's to change. PR6 seeds this and reads it
        -- through an ASOF join.
        CREATE TABLE IF NOT EXISTS dim_fx_rate (
            from_ccy  VARCHAR NOT NULL,
            to_ccy    VARCHAR NOT NULL,
            rate_date DATE NOT NULL,
            rate      DOUBLE NOT NULL,
            source    VARCHAR NOT NULL,
            PRIMARY KEY (from_ccy, to_ccy, rate_date)
        );

        -- The derived day-grain read-model `ledger::rollup` maintains:
        -- sums of fct_charge per (provider, account, day, service, region,
        -- currency), so the hot analytics reads do not rescan raw line
        -- items. Amounts are stored twice, as the reading view exposes
        -- them: in the currency the provider billed, and converted to the
        -- reporting currency `reporting_currency` records — a change of
        -- that currency is what invalidates the table. No PRIMARY KEY:
        -- the grain holds NULLable columns, and uniqueness comes from the
        -- delete-then-aggregate write, not from a constraint.
        CREATE TABLE IF NOT EXISTS daily_cost_rollup (
            provider            VARCHAR NOT NULL,
            account_id          VARCHAR NOT NULL,
            day                 DATE NOT NULL,
            service_name        VARCHAR,
            region_id           VARCHAR,
            billing_currency    VARCHAR NOT NULL,
            billed_cost         DOUBLE,
            effective_cost      DOUBLE,
            list_cost           DOUBLE,
            billed_cost_base    DOUBLE,
            effective_cost_base DOUBLE,
            charge_count        BIGINT NOT NULL,
            reporting_currency  VARCHAR NOT NULL,
            built_at            TIMESTAMP NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_rollup_day
            ON daily_cost_rollup (provider, account_id, day);
        "#,
    )?;

    migrate(conn)?;
    seed_builtin_rates(conn)?;

    conn.execute(
        "INSERT OR REPLACE INTO schema_version (version, applied_at) VALUES (?, CAST(? AS TIMESTAMP))",
        params![
            SCHEMA_VERSION,
            Utc::now().format(TIMESTAMP_FORMAT).to_string()
        ],
    )?;

    Ok(())
}

/// Bring a ledger that already exists up to the current shape.
///
/// Every change to this schema has been additive so far, and this one is a
/// column with a default, so there is no rebuild machinery here as there is
/// in [`crate::db`] — an `ALTER TABLE` is the whole migration. Driven by
/// which columns are present, so it is a no-op on a fresh file and safe to
/// re-enter.
fn migrate(conn: &Connection) -> Result<()> {
    if !has_column(conn, "ingest_batch", "channel")? {
        tracing::info!("Adding channel to ingest_batch");
        // Existing rows are all API fetches: the file channel did not
        // exist when they were written.
        conn.execute_batch("ALTER TABLE ingest_batch ADD COLUMN channel VARCHAR DEFAULT 'api'")?;
    }

    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM duckdb_columns() WHERE table_name = ? AND column_name = ?",
        params![table, column],
        |row| row.get(0),
    )?;

    Ok(count > 0)
}

/// Insert the rates that ship with the build, leaving any other row alone.
fn seed_builtin_rates(conn: &Connection) -> Result<()> {
    for (from_ccy, to_ccy, rate_date, rate) in BUILTIN_RATES {
        conn.execute(
            "INSERT OR REPLACE INTO dim_fx_rate (from_ccy, to_ccy, rate_date, rate, source)
             VALUES (?, ?, CAST(? AS DATE), ?, 'builtin')",
            params![from_ccy, to_ccy, rate_date, rate],
        )?;
    }

    Ok(())
}

/// (Re)create the reading view for a reporting currency.
///
/// Conversion happens here rather than at write time because a rate gets
/// corrected after the fact and because the user may change the currency
/// they want to read in — either would mean rewriting the fact table if
/// the amounts had been converted on the way in.
///
/// The join is ASOF: a charge takes the newest rate dated on or before the
/// charge itself, never a later one. A charge already in the reporting
/// currency needs no rate at all, and one for which no rate exists keeps a
/// NULL `billed_cost_base` — it is left out of a converted total rather
/// than silently counted at par.
///
/// The rollup is rebuilt when it does not match the new currency: every
/// `*_base` amount it stores was converted at the old one. The check is
/// cheap when nothing changed, which is the common case — this runs on
/// every start.
pub fn apply_reporting_currency(conn: &Connection, currency: &str) -> Result<()> {
    if !currency.chars().all(|c| c.is_ascii_alphabetic()) || currency.is_empty() {
        return Err(anyhow::anyhow!("Not a currency code: {:?}", currency));
    }

    conn.execute_batch(&format!(
        r#"
        CREATE OR REPLACE VIEW {NORMALIZED_VIEW} AS
        SELECT
            c.*,
            CASE WHEN c.billing_currency = '{currency}' THEN 1.0 ELSE f.rate END AS fx_rate,
            c.billed_cost
                * CASE WHEN c.billing_currency = '{currency}' THEN 1.0 ELSE f.rate END
                AS billed_cost_base,
            c.effective_cost
                * CASE WHEN c.billing_currency = '{currency}' THEN 1.0 ELSE f.rate END
                AS effective_cost_base,
            '{currency}' AS reporting_currency
        FROM fct_charge c
        ASOF LEFT JOIN dim_fx_rate f
          ON f.from_ccy = c.billing_currency
         AND f.to_ccy = '{currency}'
         AND f.rate_date <= c.charge_period_start::DATE;
        "#
    ))?;

    // After the view: the rebuild aggregates through it, so it converts at
    // the currency just applied.
    crate::ledger::rollup::rebuild_if_stale(conn, currency)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v1 shape, as a ledger written before the channel column existed.
    const V1_INGEST_BATCH: &str = "CREATE TABLE ingest_batch (
             batch_id       VARCHAR PRIMARY KEY,
             provider       VARCHAR NOT NULL,
             account_id     VARCHAR NOT NULL,
             billing_period VARCHAR NOT NULL,
             started_at     TIMESTAMP NOT NULL,
             completed_at   TIMESTAMP,
             status         VARCHAR NOT NULL,
             row_count      BIGINT NOT NULL DEFAULT 0,
             source_ref     VARCHAR
         );
         INSERT INTO ingest_batch VALUES
             ('b-1', 'Aliyun', 'acct-2', '2026-08',
              CAST('2026-08-01 00:00:00' AS TIMESTAMP),
              CAST('2026-08-01 00:00:00' AS TIMESTAMP), 'complete', 3, NULL);";

    #[test]
    fn a_fresh_ledger_starts_at_the_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();

        let version: i32 = conn
            .query_row("SELECT max(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(has_column(&conn, "ingest_batch", "channel").unwrap());
    }

    /// A ledger written before v2 keeps its rows, and they read as the API
    /// fetches they were.
    #[test]
    fn an_existing_ledger_gains_the_channel_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_INGEST_BATCH).unwrap();

        apply(&conn).unwrap();

        assert!(has_column(&conn, "ingest_batch", "channel").unwrap());
        let channel: Option<String> = conn
            .query_row("SELECT channel FROM ingest_batch", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            crate::ledger::Channel::from_stored(channel.as_deref()),
            crate::ledger::Channel::Api
        );
    }

    fn has_table(conn: &Connection, table: &str) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM duckdb_tables() WHERE table_name = ?",
            params![table],
            |row| row.get(0),
        )?;

        Ok(count > 0)
    }

    /// A ledger written before v3 gains the rollup table the same way a
    /// fresh file does: the CREATE is the migration.
    #[test]
    fn an_existing_ledger_gains_the_rollup_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_INGEST_BATCH).unwrap();

        apply(&conn).unwrap();

        assert!(has_table(&conn, "daily_cost_rollup").unwrap());
        let version: i32 = conn
            .query_row("SELECT max(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// `apply` runs on every start.
    #[test]
    fn applying_twice_changes_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();

        let columns: i64 = conn
            .query_row(
                "SELECT count(*) FROM duckdb_columns() WHERE table_name = 'ingest_batch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 10);
    }
}
