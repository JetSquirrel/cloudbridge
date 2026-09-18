//! OpenAI cost and usage export.
//!
//! Read through [`usage_export`], which Anthropic shares: both consoles
//! publish one row per day per model, and both offer a cost export and a
//! usage export with the same shape. Whichever the user downloaded is
//! detected from the columns, not asked about.
//!
//! Spend is attributed to the **project** — recorded as
//! `billing_account_id`, since a project is the provider-side account the
//! charge was billed against — so a project's share of the month is a
//! plain group-by rather than something to reconstruct from key names.
//!
//! The column names below cover the console export and the field names the
//! organization Costs and Usage endpoints return, because an export taken
//! through the API carries those instead. None of them is documented as a
//! file format; a column that matches nothing is an error naming what the
//! file does have.

use anyhow::Result;

use super::usage_export::{self, Layout};
use super::BillFileFormat;
use crate::cloud::{BillingPeriod, Normalized, RawBatch};

/// Name the imported file is stored under in a raw batch.
const PART: &str = "openai_usage_export";

pub static FORMAT: BillFileFormat = BillFileFormat {
    display_name: "Cost or usage export (CSV)",
    origin_hint: "OpenAI platform → Usage → Export, or the organization Costs endpoint",
    extensions: &["csv"],
    zip_member: None,
    part: PART,
    periods,
    normalize,
};

static LAYOUT: Layout = Layout {
    service_name: "OpenAI",
    date: &[
        "date",
        "day",
        "timestamp",
        "usage_date",
        "start_time",
        "start_date",
        "bucket_start",
        "period",
    ],
    cost: &[
        "cost",
        "cost_usd",
        "amount",
        "amount_value",
        "total_cost",
        "spend",
        "cost(usd)",
        "amount(usd)",
    ],
    currency: &["currency", "amount_currency"],
    default_currency: "USD",
    model: &["model", "model_name", "snapshot_id"],
    description: &[
        "line_item",
        "description",
        "item",
        "sku",
        "operation",
        "name",
    ],
    scope_name: &["project_name", "project", "workspace"],
    scope_id: &["project_id"],
    tokens: &[
        (
            "Input Tokens",
            &[
                "input_tokens",
                "n_context_tokens_total",
                "context_tokens",
                "prompt_tokens",
            ],
        ),
        (
            "Output Tokens",
            &[
                "output_tokens",
                "n_generated_tokens_total",
                "generated_tokens",
                "completion_tokens",
            ],
        ),
        (
            "Cached Input Tokens",
            &["input_cached_tokens", "cached_tokens", "input_cached"],
        ),
        (
            "Requests",
            &["num_model_requests", "n_requests", "requests"],
        ),
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
    const COSTS: &str = include_str!("../testdata/openai_costs.csv");

    fn recorded_batch(text: &str, period: BillingPeriod) -> RawBatch {
        RawBatch {
            provider: "OpenAI".to_string(),
            account_id: "acct-5".to_string(),
            period,
            batch_id: "b-1".to_string(),
            fetched_at: "2026-09-02T02:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART, "file", text)],
            payload_files: Vec::new(),
        }
    }

    fn charges(text: &str, period: BillingPeriod) -> Vec<Charge> {
        normalize(&recorded_batch(text, period)).unwrap().charges
    }

    fn total(charges: &[Charge]) -> f64 {
        charges.iter().filter_map(|charge| charge.billed_cost).sum()
    }

    #[test]
    fn a_cost_row_becomes_one_authoritative_charge_against_its_project() {
        let charges = charges(COSTS, BillingPeriod::new(2026, 8));
        let charge = &charges[0];

        assert_eq!(charge.service_name.as_deref(), Some("OpenAI"));
        assert_eq!(charge.service_category.as_deref(), Some("gpt-5"));
        assert_eq!(charge.charge_description.as_deref(), Some("gpt-5 input"));
        assert_eq!(charge.billed_cost, Some(12.50));
        assert_eq!(charge.charge_category, ChargeCategory::Usage);
        assert_eq!(charge.cost_basis, CostBasis::Authoritative);
        // A project is the account the charge was billed against.
        assert_eq!(charge.billing_account_id.as_deref(), Some("proj_abc"));
        assert_eq!(charge.resource_name.as_deref(), Some("Prod API"));
        // A lowercase currency code still has to key the rate table.
        assert_eq!(charge.billing_currency, "USD");
        assert_eq!(charge.pricing_quantity, Some(1_250_000.0));
        assert_eq!(charge.pricing_unit.as_deref(), Some("Input Tokens"));
    }

    #[test]
    fn a_row_is_dated_to_its_own_day() {
        let charge = &charges(COSTS, BillingPeriod::new(2026, 8))[0];

        assert_eq!(
            charge.charge_period_start.to_rfc3339(),
            "2026-08-09T00:00:00+00:00"
        );
        assert_eq!(
            charge.charge_period_end.to_rfc3339(),
            "2026-08-10T00:00:00+00:00"
        );
    }

    #[test]
    fn only_the_month_being_imported_is_read() {
        let august = charges(COSTS, BillingPeriod::new(2026, 8));
        assert_eq!(august.len(), 3);
        assert!((total(&august) - 20.15).abs() < 1e-9, "{}", total(&august));

        let september = charges(COSTS, BillingPeriod::new(2026, 9));
        assert_eq!(september.len(), 1);
        assert_eq!(september[0].billed_cost, Some(3.00));
    }

    #[test]
    fn a_file_spanning_two_months_reports_both_oldest_first() {
        assert_eq!(
            periods(COSTS).unwrap(),
            vec![BillingPeriod::new(2026, 8), BillingPeriod::new(2026, 9)]
        );
    }

    /// The usage export has token counts and no money. Inventing an amount
    /// from a list price would put a number in `billed_cost` that nobody
    /// was charged.
    #[test]
    fn a_usage_export_records_consumption_and_claims_no_cost() {
        let usage = "date,project_name,model,input_tokens,output_tokens\n\
                     2026-08-09,Prod API,gpt-5,1250000,145000\n";
        let charges = charges(usage, BillingPeriod::new(2026, 8));

        assert_eq!(charges.len(), 2, "one row per token type");
        for charge in &charges {
            assert_eq!(charge.billed_cost, None);
            assert_eq!(charge.cost_basis, CostBasis::Absent);
        }

        let units: Vec<&str> = charges
            .iter()
            .filter_map(|charge| charge.pricing_unit.as_deref())
            .collect();
        assert_eq!(units, vec!["Input Tokens", "Output Tokens"]);
        assert_eq!(charges[0].pricing_quantity, Some(1_250_000.0));
        assert_eq!(charges[1].pricing_quantity, Some(145_000.0));
    }

    /// Input and output tokens are priced differently, so their sum is not
    /// a quantity the amount was charged for.
    #[test]
    fn a_cost_row_covering_two_token_types_stores_no_quantity() {
        let mixed = "date,model,cost,input_tokens,output_tokens\n\
                     2026-08-09,gpt-5,19.75,1250000,145000\n";
        let charge = &charges(mixed, BillingPeriod::new(2026, 8))[0];

        assert_eq!(charge.billed_cost, Some(19.75));
        assert_eq!(charge.pricing_quantity, None);
        assert_eq!(charge.pricing_unit, None);
    }

    /// The older activity export names its token columns differently.
    #[test]
    fn the_older_column_names_are_still_read() {
        let legacy = "timestamp,model,n_context_tokens_total,n_generated_tokens_total\n\
                      2026-08-09 13:45:00,gpt-4o,1000,200\n";
        let charges = charges(legacy, BillingPeriod::new(2026, 8));

        assert_eq!(charges.len(), 2);
        assert_eq!(charges[0].pricing_quantity, Some(1000.0));
        assert_eq!(charges[0].service_category.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn a_file_with_neither_a_cost_nor_a_token_count_is_refused() {
        let error = periods("date,model\n2026-08-09,gpt-5\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("neither a cost column"), "{}", error);
    }

    #[test]
    fn a_file_that_is_not_an_export_is_refused() {
        assert!(periods("id,name\n1,gpt-5\n").is_err());
    }
}
