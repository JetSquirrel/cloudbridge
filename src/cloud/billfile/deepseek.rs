//! DeepSeek cost export.
//!
//! DeepSeek's billing API reports only a balance, so this file channel is
//! the source's entire view of spend: the console's usage download is a zip
//! of two CSVs, and this is the `cost-*.csv` one — one row per day per
//! model with what it cost, in CNY. Read through [`usage_export`], whose
//! shape it shares with OpenAI and Anthropic.
//!
//! The zip's other half, `amount-*.csv`, is refused: its token counts are
//! one *row* per token type with the count in a column named `amount`, and
//! reading that column as money would book token counts as yuan. The cost
//! layout names its money column `cost` and nothing else, so the amount
//! export fails "neither a cost column nor a token count" instead of
//! totalling wrong.

use anyhow::Result;

use super::usage_export::{self, Layout};
use super::BillFileFormat;
use crate::cloud::{BillingPeriod, Normalized, RawBatch};

/// Name the imported file is stored under in a raw batch.
const PART: &str = "deepseek_cost_export";

pub static FORMAT: BillFileFormat = BillFileFormat {
    display_name: "Cost export (cost-*.csv)",
    origin_hint:
        "DeepSeek console → Usage → download; the zip or the cost-*.csv inside it, not amount-*.csv",
    extensions: &["csv", "zip"],
    // The download bundles a cost and an amount CSV; only the cost one is
    // money, and `amount` must never be read as a price.
    zip_member: Some("cost-"),
    part: PART,
    periods,
    normalize,
};

static LAYOUT: Layout = Layout {
    service_name: "DeepSeek",
    date: &["start_time_iso", "start_time", "date", "day"],
    // Exactly `cost`: the amount export's token-count column is named
    // `amount`, and its per-token price `price`; neither is money spent.
    cost: &["cost"],
    currency: &["currency"],
    default_currency: "CNY",
    model: &["model"],
    description: &["type", "wallet_type"],
    scope_name: &["api_key_name"],
    scope_id: &["user_id"],
    tokens: &[],
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
    const COSTS: &str = include_str!("../testdata/deepseek_costs.csv");

    /// The token export from the same download.
    const AMOUNTS: &str = "\u{feff}user_id,start_time_iso,end_time_iso,model,api_key_name,api_key,type,price,amount\n\
        066fa7d6-f3ba-47c6-bf97-f60ae4ca43e0,2026-08-17T00:00:00+08:00,2026-08-18T00:00:00+08:00,deepseek-v4-flash,dsh,sk-***32f6,input_cache_miss_tokens,0.0000015,10787\n";

    fn charges(text: &str, period: BillingPeriod) -> Vec<Charge> {
        let batch = RawBatch {
            provider: "DeepSeek".to_string(),
            account_id: "acct-1".to_string(),
            period,
            batch_id: "b-1".to_string(),
            fetched_at: "2026-09-10T02:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART, "file", text)],
        };
        normalize(&batch).unwrap().charges
    }

    #[test]
    fn a_cost_row_becomes_one_authoritative_charge_per_day_per_model() {
        let charges = charges(COSTS, BillingPeriod::new(2026, 8));
        let charge = &charges[0];

        assert_eq!(charge.service_name.as_deref(), Some("DeepSeek"));
        assert_eq!(
            charge.service_category.as_deref(),
            Some("deepseek-v4-flash")
        );
        assert_eq!(charge.billed_cost, Some(0.0394352));
        assert_eq!(charge.billing_currency, "CNY");
        assert_eq!(charge.charge_category, ChargeCategory::Usage);
        assert_eq!(charge.cost_basis, CostBasis::Authoritative);
        // The console user is the provider-side account it was billed to.
        assert_eq!(
            charge.billing_account_id.as_deref(),
            Some("066fa7d6-f3ba-47c6-bf97-f60ae4ca43e0")
        );
    }

    #[test]
    fn a_row_is_dated_to_its_own_day() {
        let charge = &charges(COSTS, BillingPeriod::new(2026, 8))[0];

        assert_eq!(
            charge.charge_period_start.to_rfc3339(),
            "2026-08-17T00:00:00+00:00"
        );
        assert_eq!(
            charge.charge_period_end.to_rfc3339(),
            "2026-08-18T00:00:00+00:00"
        );
    }

    #[test]
    fn a_file_spanning_two_months_reports_both_and_splits_rows_between_them() {
        assert_eq!(
            periods(COSTS).unwrap(),
            vec![BillingPeriod::new(2026, 8), BillingPeriod::new(2026, 9)]
        );

        let august = charges(COSTS, BillingPeriod::new(2026, 8));
        assert_eq!(august.len(), 6);
        let total: f64 = august.iter().filter_map(|charge| charge.billed_cost).sum();
        assert!((total - 11.5783089).abs() < 1e-9, "{}", total);

        assert_eq!(charges(COSTS, BillingPeriod::new(2026, 9)).len(), 1);
    }

    /// The token export's count column is named `amount`: mistaking it for
    /// the money column would book 10,787 tokens as ¥10,787.
    #[test]
    fn the_amount_export_is_refused_rather_than_misread() {
        let error = periods(AMOUNTS).unwrap_err().to_string();
        assert!(error.contains("neither a cost column"), "{}", error);
    }
}
