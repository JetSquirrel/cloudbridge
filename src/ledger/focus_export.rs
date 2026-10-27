//! The ledger written out as a FOCUS Cost and Usage dataset.
//!
//! The ledger's columns are named after FOCUS, so the export is mostly a
//! rename: `billed_cost` becomes `BilledCost`, `sku_price_details` becomes
//! `SkuPriceDetails`, and so on, in the billing currency each charge was
//! billed in. That includes FOCUS 1.5's description of a model's tokens —
//! `ConsumedQuantity` and `ConsumedUnit` "Tokens", `SkuMeter`, and the
//! model and token properties in `SkuPriceDetails` — so another FinOps
//! tool reads the token economics the Models page shows.
//!
//! It is not a conformant FOCUS dataset, and does not claim to be. Columns
//! no source here reports, such as `ChargeClass`, `PricingCategory` and
//! `ContractedCost`, are left out rather than filled with guesses, and a
//! row whose provider reported no amount keeps `BilledCost` empty instead
//! of a zero nobody was charged. What CloudBridge itself adds goes in
//! `x_`-prefixed columns, as FOCUS asks of custom columns.

use std::path::Path;

use anyhow::{anyhow, Result};
use duckdb::Connection;

/// Write every charge in the ledger to `path` as CSV, oldest first, and
/// return how many rows were written.
pub fn export_csv(path: &Path) -> Result<usize> {
    super::with_connection_ref(|conn| export_csv_from(conn, path))
}

pub(crate) fn export_csv_from(conn: &Connection, path: &Path) -> Result<usize> {
    let target = path
        .to_str()
        .ok_or_else(|| anyhow!("The export path is not valid UTF-8: {}", path.display()))?
        .replace('\'', "''");
    let rows = conn.execute(
        &format!(
            "COPY ({}) TO '{target}' (FORMAT CSV, HEADER true)",
            select_sql()
        ),
        [],
    )?;
    Ok(rows)
}

/// FOCUS's ISO 8601 form of a ledger timestamp, which is stored in UTC.
fn instant(column: &str) -> String {
    format!("strftime({column}, '%Y-%m-%dT%H:%M:%SZ')")
}

/// The provider's name as FOCUS spells a company — "Amazon Web Services",
/// not the registry's `AWS` — from the source registry, so a new source is
/// named without touching this file. A display name's parenthesis is a
/// gloss, not the company: "Anthropic (Claude)" is Anthropic.
fn provider_name_sql() -> String {
    let cases: String = crate::cloud::registry::all()
        .iter()
        .map(|source| {
            format!(
                " WHEN '{}' THEN '{}'",
                source.id.replace('\'', "''"),
                company(source.display_name).replace('\'', "''")
            )
        })
        .collect();
    format!("CASE provider{cases} ELSE provider END")
}

fn company(display_name: &str) -> &str {
    display_name
        .split(" (")
        .next()
        .unwrap_or(display_name)
        .trim()
}

fn select_sql() -> String {
    let provider = provider_name_sql();
    let period_start = "strptime(billing_period || '-01', '%Y-%m-%d')";
    format!(
        "SELECT
             BilledCost, BillingAccountId, BillingCurrency,
             BillingPeriodEnd, BillingPeriodStart,
             ChargeCategory, ChargeDescription, ChargePeriodEnd, ChargePeriodStart,
             ConsumedQuantity, ConsumedUnit, EffectiveCost,
             HostProviderName, InvoiceIssuerName, ListCost,
             PricingQuantity, PricingUnit, PrincipalId, CredentialId,
             RegionId, RequesterDetails, ResourceId, ResourceName,
             ServiceCategory, ServiceName, ServiceProviderName, ServiceSubcategory,
             SkuMeter, SkuPriceDetails, SubAccountId, SubAccountName, Tags,
             x_CloudBridgeAccountId, x_CostBasis, x_Model, x_Provider, x_ServiceCode
         FROM (
             SELECT
                 charge_period_start AS sort_start, charge_id AS sort_id,
                 billed_cost AS BilledCost,
                 coalesce(billing_account_id, account_id) AS BillingAccountId,
                 billing_currency AS BillingCurrency,
                 {billing_end} AS BillingPeriodEnd,
                 {billing_start} AS BillingPeriodStart,
                 charge_category AS ChargeCategory,
                 charge_description AS ChargeDescription,
                 {charge_end} AS ChargePeriodEnd,
                 {charge_start} AS ChargePeriodStart,
                 consumed_quantity AS ConsumedQuantity,
                 consumed_unit AS ConsumedUnit,
                 effective_cost AS EffectiveCost,
                 {provider} AS HostProviderName,
                 {provider} AS InvoiceIssuerName,
                 list_cost AS ListCost,
                 pricing_quantity AS PricingQuantity,
                 pricing_unit AS PricingUnit,
                 principal_id AS PrincipalId,
                 credential_id AS CredentialId,
                 region_id AS RegionId,
                 requester_details AS RequesterDetails,
                 resource_id AS ResourceId,
                 resource_name AS ResourceName,
                 service_category AS ServiceCategory,
                 service_name AS ServiceName,
                 {provider} AS ServiceProviderName,
                 service_subcategory AS ServiceSubcategory,
                 sku_meter AS SkuMeter,
                 sku_price_details AS SkuPriceDetails,
                 sub_account_id AS SubAccountId,
                 sub_account_name AS SubAccountName,
                 tags AS Tags,
                 account_id AS x_CloudBridgeAccountId,
                 cost_basis AS x_CostBasis,
                 x_model AS x_Model,
                 provider AS x_Provider,
                 x_service_code AS x_ServiceCode
             FROM fct_charge
         )
         ORDER BY sort_start, sort_id",
        billing_start = instant(period_start),
        billing_end = instant(&format!("({period_start} + INTERVAL 1 MONTH)")),
        charge_start = instant("charge_period_start"),
        charge_end = instant("charge_period_end"),
    )
}

#[cfg(test)]
mod tests {
    use super::export_csv_from;
    use crate::ledger::{schema, write_period, Channel, Charge, PeriodKey};
    use chrono::{TimeZone, Utc};
    use duckdb::Connection;

    #[test]
    fn a_display_name_gloss_is_not_the_company() {
        assert_eq!(super::company("Anthropic (Claude)"), "Anthropic");
        assert_eq!(super::company("Volcengine (火山引擎)"), "Volcengine");
        assert_eq!(super::company("Amazon Web Services"), "Amazon Web Services");
    }

    #[test]
    fn the_ledger_is_written_with_focus_column_names() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        let day = |d: u32| Utc.with_ymd_and_hms(2026, 8, d, 0, 0, 0).unwrap();
        write_period(
            &mut conn,
            &PeriodKey::new("Anthropic", "acct-4", "2026-08"),
            "b-1",
            &[
                Charge {
                    service_name: Some("Anthropic".to_string()),
                    x_model: Some("claude-sonnet-4-5".to_string()),
                    pricing_quantity: Some(8_000_000.0),
                    pricing_unit: Some("Cache Read Tokens".to_string()),
                    billed_cost: Some(2.4),
                    ..Charge::new(day(2), day(3), "USD")
                },
                Charge {
                    service_name: Some("Anthropic".to_string()),
                    x_model: Some("claude-sonnet-4-5".to_string()),
                    pricing_quantity: Some(1_500_000.0),
                    pricing_unit: Some("Output Tokens".to_string()),
                    billed_cost: Some(22.5),
                    ..Charge::new(day(1), day(2), "USD")
                },
            ],
            None,
            Channel::File,
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!("cloudbridge-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("it's focus.csv");
        assert_eq!(export_csv_from(&conn, &path).unwrap(), 2);

        // Read back the way another tool would: by FOCUS column name.
        let read = Connection::open_in_memory().unwrap();
        let rows: Vec<(String, f64, String, String, String, String, String)> = read
            .prepare(&format!(
                "SELECT ChargePeriodStart, ConsumedQuantity, ConsumedUnit, SkuMeter,
                        json_extract_string(SkuPriceDetails, '$.TokenDirection'),
                        ServiceProviderName, BillingPeriodEnd
                 FROM read_csv('{}', all_varchar = true)",
                path.to_string_lossy().replace('\'', "''")
            ))
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, String>(1)?.parse().unwrap(),
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            rows,
            vec![
                (
                    "2026-08-01T00:00:00Z".to_string(),
                    1_500_000.0,
                    "Tokens".to_string(),
                    "Output Tokens".to_string(),
                    "Output".to_string(),
                    "Anthropic".to_string(),
                    "2026-09-01T00:00:00Z".to_string(),
                ),
                (
                    "2026-08-02T00:00:00Z".to_string(),
                    8_000_000.0,
                    "Tokens".to_string(),
                    "Cache Read Tokens".to_string(),
                    "Input".to_string(),
                    "Anthropic".to_string(),
                    "2026-09-01T00:00:00Z".to_string(),
                ),
            ]
        );
    }
}
