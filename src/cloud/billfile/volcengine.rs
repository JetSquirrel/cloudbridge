//! Volcengine bill detail export (火山引擎 账单明细).
//!
//! Added for **Ark (火山方舟)**, whose model spend the bill reports per
//! endpoint and per metered token type. Volcengine publishes the same table
//! Alibaba Cloud does, under its own column names, so the reading is
//! [`detail_export`]'s and only the names are here.
//!
//! The export covers the whole account, not only Ark. That is deliberate,
//! and the same caveat applies as for Alibaba Cloud: an import replaces the
//! month it covers, so an export narrowed to one product in the console
//! replaces that month with that one product.
//!
//! One trap worth naming, because it would misreport a discount as spend:
//! Volcengine's `PreferentialBillAmount` (优惠后金额) is a *subtotal* —
//! the list price with the discount already taken off — not a deduction.
//! It is deliberately absent from `deductions` below; the deductions are
//! `DiscountBillAmount` and `CouponAmount`, which are what actually came
//! off between `OriginalBillAmount` and `PayableAmount`.

use anyhow::Result;

use super::detail_export::{self, Layout};
use super::BillFileFormat;
use crate::cloud::{BillingPeriod, Normalized, RawBatch};

/// Name the imported file is stored under in a raw batch.
const PART: &str = "volcengine_bill_detail";

pub static FORMAT: BillFileFormat = BillFileFormat {
    display_name: "Bill detail export (账单明细)",
    origin_hint: "Volcengine console → Billing → Bill Details → Export",
    extensions: &["csv", "txt"],
    zip_member: None,
    part: PART,
    periods,
    normalize,
};

/// Column names across the Chinese console and the `ListBillDetail` field
/// names, either of which an export can carry.
static LAYOUT: Layout = Layout {
    anchors: &[
        "账期",
        "BillPeriod",
        "账单月份",
        "计费周期",
        "费用时间",
        "ExpenseTime",
        "ExpenseDate",
        "账单日期",
        "消费日期",
    ],
    billing_cycle: &["账期", "BillPeriod", "账单月份", "计费周期"],
    billing_date: &[
        "费用时间",
        "ExpenseTime",
        "ExpenseDate",
        "账单日期",
        "消费日期",
    ],
    // `ProductZh` is the Chinese name, `Product` the stable code.
    product_name: &["产品名称", "ProductZh", "产品", "商品名称"],
    product_code: &["Product", "产品代码", "ProductCode"],
    product_detail: &[
        "子产品",
        "ProductDetail",
        "产品明细",
        "配置",
        "ConfigName",
        "二级商品",
    ],
    billing_item: &["计费项", "Element", "计费项目", "计费因子", "Factor"],
    instance_id: &["实例ID", "InstanceNo", "InstanceID", "资源ID"],
    instance_name: &["实例名称", "InstanceName", "资源名称"],
    region: &["地域", "Region", "RegionCode", "地域名称"],
    currency: &["币种", "Currency"],
    default_currency: "CNY",
    usage: &["用量", "Count", "使用量", "DeductionCount", "UseNum"],
    usage_unit: &["单位", "Unit", "用量单位", "计量单位"],
    tag: &["标签", "Tag", "Tags"],
    gross: &["原价", "OriginalBillAmount", "官网价", "原始金额"],
    net: &["应付金额", "PayableAmount", "应付", "应付金额(元)"],
    deductions: &[
        (
            "DiscountBillAmount",
            &["折扣金额", "DiscountBillAmount", "优惠金额", "折扣"],
        ),
        (
            "CouponAmount",
            &["代金券抵扣", "CouponAmount", "代金券", "代金券金额"],
        ),
    ],
};

fn periods(text: &str) -> Result<Vec<BillingPeriod>> {
    detail_export::periods(&LAYOUT, text)
}

fn normalize(batch: &RawBatch) -> Result<Normalized> {
    detail_export::normalize(&LAYOUT, batch, PART)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::raw::RawPart;
    use crate::ledger::{Charge, ChargeCategory, CostBasis};

    /// One recorded bill detail export, as the Chinese console writes it.
    const BILL_DETAIL: &str = include_str!("../testdata/volcengine_bill_detail.csv");

    fn recorded_batch(text: &str, period: BillingPeriod) -> RawBatch {
        RawBatch {
            provider: "Volcengine".to_string(),
            account_id: "acct-4".to_string(),
            period,
            batch_id: "b-1".to_string(),
            fetched_at: "2026-09-02T02:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART, "file", text)],
        }
    }

    fn charges(text: &str, period: BillingPeriod) -> Vec<Charge> {
        normalize(&recorded_batch(text, period)).unwrap().charges
    }

    /// The one usage row whose billing item is `name`.
    fn item<'a>(charges: &'a [Charge], name: &str) -> &'a Charge {
        charges
            .iter()
            .find(|charge| charge.charge_description.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no row for {}", name))
    }

    fn total(charges: &[Charge]) -> f64 {
        charges.iter().filter_map(|charge| charge.billed_cost).sum()
    }

    #[test]
    fn an_ark_line_carries_the_model_the_endpoint_and_its_tokens() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));
        let charge = item(&charges, "doubao-pro-32k 输入tokens");

        assert_eq!(
            charge.service_name.as_deref(),
            Some("火山方舟大模型服务平台")
        );
        assert_eq!(charge.service_category.as_deref(), Some("ark"));
        assert_eq!(charge.billed_cost, Some(20.00));
        assert_eq!(charge.list_cost, Some(20.00));
        assert_eq!(charge.billing_currency, "CNY");
        assert_eq!(charge.charge_category, ChargeCategory::Usage);
        assert_eq!(charge.cost_basis, CostBasis::Authoritative);
        assert_eq!(charge.pricing_unit.as_deref(), Some("Tokens"));
        assert_eq!(charge.pricing_quantity, Some(2_000_000.0));
        // The endpoint is what an Ark charge is actually attributable to.
        assert_eq!(charge.resource_id.as_deref(), Some("ep-20260809-abcde"));
        assert_eq!(charge.tags.as_deref(), Some(r#"{"env":"prod"}"#));
    }

    #[test]
    fn a_discount_is_a_credit_and_the_rows_sum_to_what_was_charged() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));

        let credit = charges
            .iter()
            .find(|charge| charge.charge_category == ChargeCategory::Credit)
            .expect("the discount is recorded");
        assert_eq!(
            credit.charge_description.as_deref(),
            Some("DiscountBillAmount")
        );
        assert_eq!(credit.billed_cost, Some(-2.00));

        // 20.00 - 2.00 charged on one line, 8.00 on the other.
        assert!(
            (total(&charges) - 26.00).abs() < 1e-9,
            "{}",
            total(&charges)
        );
    }

    #[test]
    fn a_coupon_is_a_credit_of_its_own() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 9));

        let credit = charges
            .iter()
            .find(|charge| charge.charge_category == ChargeCategory::Credit)
            .expect("the coupon is recorded");
        assert_eq!(credit.charge_description.as_deref(), Some("CouponAmount"));
        assert_eq!(credit.billed_cost, Some(-0.50));
        assert!((total(&charges) - 1.00).abs() < 1e-9);
    }

    #[test]
    fn a_daily_export_dates_a_row_to_its_own_day() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));
        let charge = item(&charges, "doubao-pro-32k 输入tokens");

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
    fn the_total_line_the_console_appends_is_not_a_charge() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));
        assert!(charges
            .iter()
            .all(|charge| charge.service_name.as_deref() != Some("合计")));
    }

    #[test]
    fn a_file_spanning_two_months_reports_both_oldest_first() {
        assert_eq!(
            periods(BILL_DETAIL).unwrap(),
            vec![BillingPeriod::new(2026, 8), BillingPeriod::new(2026, 9)]
        );
    }

    /// `ListBillDetail`'s own field names, which an export taken from the
    /// API rather than the console carries.
    #[test]
    fn the_api_field_names_read_identically() {
        let english = "BillPeriod,ExpenseTime,Product,ProductZh,Element,InstanceNo,Region,\
                       OriginalBillAmount,DiscountBillAmount,CouponAmount,PayableAmount,\
                       Currency,Count,Unit\n\
                       2026-08,2026-08-09,ark,Ark,doubao-pro-32k input,ep-1,cn-beijing,\
                       20.00,2.00,0.00,18.00,CNY,2000000,Tokens\n";

        let charges = charges(english, BillingPeriod::new(2026, 8));
        assert_eq!(charges[0].service_category.as_deref(), Some("ark"));
        assert_eq!(charges[0].billed_cost, Some(20.00));
        assert!((total(&charges) - 18.00).abs() < 1e-9);
    }

    /// The subtotal must not be read as a deduction, or a discounted line
    /// would have the discount taken off it twice.
    #[test]
    fn the_preferential_subtotal_is_not_treated_as_a_deduction() {
        let with_subtotal = "BillPeriod,Product,Element,OriginalBillAmount,\
                             PreferentialBillAmount,DiscountBillAmount,PayableAmount,Currency\n\
                             2026-08,ark,doubao input,20.00,18.00,2.00,18.00,CNY\n";

        let charges = charges(with_subtotal, BillingPeriod::new(2026, 8));
        assert_eq!(charges.len(), 2, "one usage row and one discount");
        assert!(
            (total(&charges) - 18.00).abs() < 1e-9,
            "{}",
            total(&charges)
        );
    }

    #[test]
    fn a_file_that_is_not_a_bill_is_refused() {
        assert!(periods("id,name\n1,ark\n").is_err());
    }
}
