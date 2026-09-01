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
pub const SCHEMA_VERSION: i32 = 1;

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
            source_ref     VARCHAR
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
        "#,
    )?;

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

    Ok(())
}
