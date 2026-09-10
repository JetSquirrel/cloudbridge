//! Anthropic (Claude) cost and usage export.
//!
//! Read through [`usage_export`], which OpenAI shares. Spend is attributed
//! to the **workspace**, recorded as `billing_account_id` for the same
//! reason an OpenAI project is: it is the provider-side account the charge
//! was billed against.
//!
//! Claude's usage export is the one where the token columns earn their
//! keep. Cache reads and cache writes are priced differently from ordinary
//! input, so they arrive as their own `pricing_unit` rather than being
//! folded into an input-token total that would then be priced wrongly by
//! anything reading it.
//!
//! The names below cover the console export and the fields the organization
//! Cost and Usage report endpoints return, since an export taken through
//! the API carries those instead.

use anyhow::Result;

use super::usage_export::{self, Layout};
use super::BillFileFormat;
use crate::cloud::{BillingPeriod, Normalized, RawBatch};

/// Name the imported file is stored under in a raw batch.
const PART: &str = "anthropic_usage_export";

pub static FORMAT: BillFileFormat = BillFileFormat {
    display_name: "Cost or usage export (CSV)",
    origin_hint: "Claude Console → Usage or Cost → Export, or the organization Cost report",
    extensions: &["csv"],
    part: PART,
    periods,
    normalize,
};

static LAYOUT: Layout = Layout {
    service_name: "Anthropic",
    date: &[
        "date",
        "day",
        "starting_at",
        "start_time",
        "usage_date",
        "bucket_start",
        "period",
    ],
    cost: &[
        "cost",
        "cost_usd",
        "amount",
        "amount_usd",
        "total_cost",
        "spend",
        "cost(usd)",
        "amount(usd)",
    ],
    currency: &["currency"],
    default_currency: "USD",
    model: &["model", "model_name"],
    description: &[
        "description",
        "cost_type",
        "token_type",
        "line_item",
        "item",
        "service_tier",
    ],
    scope_name: &["workspace", "workspace_name", "api_key_name", "key_name"],
    scope_id: &["workspace_id", "api_key_id"],
    tokens: &[
        (
            "Input Tokens",
            &[
                "input_tokens",
                "uncached_input_tokens",
                "prompt_tokens",
                "uncached_input",
            ],
        ),
        ("Output Tokens", &["output_tokens", "completion_tokens"]),
        (
            "Cache Read Tokens",
            &["cache_read_input_tokens", "cache_read_tokens", "cache_read"],
        ),
        (
            "Cache Creation Tokens",
            &[
                "cache_creation_input_tokens",
                "cache_creation_tokens",
                "cache_creation",
                "cache_write_tokens",
            ],
        ),
        ("Web Search Requests", &["web_search_requests"]),
    ],
};

fn periods(text: &str) -> Result<Vec<BillingPeriod>> {
    usage_export::periods(&LAYOUT, text)
}

fn normalize(batch: &RawBatch) -> Result<Normalized> {
    usage_export::normalize(&LAYOUT, batch, PART)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::raw::RawPart;
    use crate::ledger::{Charge, ChargeCategory, CostBasis};

    /// One recorded cost export.
    const COSTS: &str = include_str!("../testdata/anthropic_costs.csv");

    fn recorded_batch(text: &str, period: BillingPeriod) -> RawBatch {
        RawBatch {
            provider: "Anthropic".to_string(),
            account_id: "acct-6".to_string(),
            period,
            batch_id: "b-1".to_string(),
            fetched_at: "2026-09-02T02:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART, "file", text)],
        }
    }

    fn charges(text: &str, period: BillingPeriod) -> Vec<Charge> {
        normalize(&recorded_batch(text, period)).unwrap().charges
    }

    fn total(charges: &[Charge]) -> f64 {
        charges.iter().filter_map(|charge| charge.billed_cost).sum()
    }

    #[test]
    fn a_cost_row_becomes_one_authoritative_charge_against_its_workspace() {
        let charges = charges(COSTS, BillingPeriod::new(2026, 8));
        let charge = &charges[0];

        assert_eq!(charge.service_name.as_deref(), Some("Anthropic"));
        assert_eq!(charge.service_category.as_deref(), Some("claude-opus-5"));
        assert_eq!(charge.charge_description.as_deref(), Some("Input tokens"));
        assert_eq!(charge.billed_cost, Some(30.00));
        assert_eq!(charge.charge_category, ChargeCategory::Usage);
        assert_eq!(charge.cost_basis, CostBasis::Authoritative);
        assert_eq!(charge.billing_account_id.as_deref(), Some("wrkspc_1"));
        assert_eq!(charge.resource_name.as_deref(), Some("Default"));
        assert_eq!(charge.billing_currency, "USD");
        assert_eq!(charge.pricing_quantity, Some(2_000_000.0));
        assert_eq!(charge.pricing_unit.as_deref(), Some("Input Tokens"));
    }

    #[test]
    fn only_the_month_being_imported_is_read() {
        let august = charges(COSTS, BillingPeriod::new(2026, 8));
        assert_eq!(august.len(), 3);
        assert!((total(&august) - 76.20).abs() < 1e-9, "{}", total(&august));

        let september = charges(COSTS, BillingPeriod::new(2026, 9));
        assert_eq!(september.len(), 1);
        assert_eq!(september[0].billed_cost, Some(2.00));
        assert_eq!(
            september[0].service_category.as_deref(),
            Some("claude-sonnet-5")
        );
    }

    #[test]
    fn a_cost_row_with_no_token_count_is_still_a_charge() {
        let charges = charges(COSTS, BillingPeriod::new(2026, 8));
        let cache = charges
            .iter()
            .find(|charge| charge.charge_description.as_deref() == Some("Cache read"))
            .expect("the cache line is recorded");

        assert_eq!(cache.billed_cost, Some(1.20));
        assert_eq!(cache.pricing_quantity, None);
    }

    #[test]
    fn a_file_spanning_two_months_reports_both_oldest_first() {
        assert_eq!(
            periods(COSTS).unwrap(),
            vec![BillingPeriod::new(2026, 8), BillingPeriod::new(2026, 9)]
        );
    }

    /// Cache reads and cache writes are priced differently from ordinary
    /// input, so each is its own priced unit rather than one input total.
    #[test]
    fn a_usage_export_keeps_the_cache_token_types_apart() {
        let usage = "date,workspace,model,input_tokens,output_tokens,\
                     cache_read_input_tokens,cache_creation_input_tokens\n\
                     2026-08-09,Default,claude-opus-5,1000,200,50000,7000\n";
        let charges = charges(usage, BillingPeriod::new(2026, 8));

        let units: Vec<(&str, Option<f64>)> = charges
            .iter()
            .map(|charge| {
                (
                    charge.pricing_unit.as_deref().unwrap(),
                    charge.pricing_quantity,
                )
            })
            .collect();
        assert_eq!(
            units,
            vec![
                ("Input Tokens", Some(1000.0)),
                ("Output Tokens", Some(200.0)),
                ("Cache Read Tokens", Some(50000.0)),
                ("Cache Creation Tokens", Some(7000.0)),
            ]
        );

        // Consumption, with no claim about what it cost.
        assert!(charges
            .iter()
            .all(|charge| charge.billed_cost.is_none() && charge.cost_basis == CostBasis::Absent));
    }

    /// A token column the row leaves at zero is not a charge of its own.
    #[test]
    fn an_unused_token_type_is_not_a_row() {
        let usage = "date,model,input_tokens,output_tokens,cache_read_input_tokens\n\
                     2026-08-09,claude-haiku-4-5,1000,200,0\n";
        let charges = charges(usage, BillingPeriod::new(2026, 8));

        assert_eq!(charges.len(), 2);
    }

    /// The cost report endpoint's own field names.
    #[test]
    fn the_api_field_names_read_identically() {
        let report = "starting_at,workspace_id,model,token_type,amount,currency\n\
                      2026-08-09T00:00:00Z,wrkspc_1,claude-opus-5,output_tokens,45.00,usd\n";
        let charge = &charges(report, BillingPeriod::new(2026, 8))[0];

        assert_eq!(charge.billed_cost, Some(45.00));
        assert_eq!(charge.charge_description.as_deref(), Some("output_tokens"));
        assert_eq!(charge.billing_currency, "USD");
    }

    #[test]
    fn a_file_that_is_not_an_export_is_refused() {
        assert!(periods("id,name\n1,claude\n").is_err());
    }
}
