//! Alibaba Cloud bill detail export (账单明细).
//!
//! The reason this format exists beside the billing API: `QueryBillOverview`
//! reports one row per product per month, so **Model Studio (百炼)** arrives
//! as a single figure with no model in it. The console's own bill detail
//! export carries the billing item, the instance and the usage quantity, so
//! the same month becomes one row per model — and `pricing_unit` finally
//! holds `Tokens`, which the ledger schema was widened to accept.
//!
//! Importing this file **replaces** the month it covers, exactly as a fetch
//! does. That is deliberate: the export is the same bill at finer grain, so
//! adding it to an API reading of the same month would double the total. The
//! corollary is worth knowing — an export narrowed to one product in the
//! console replaces the whole month with that one product, so export the
//! full bill unless a single product is genuinely all that is wanted.
//!
//! The table itself is read by [`detail_export`], which Volcengine shares.

use anyhow::Result;

use super::detail_export::{self, Layout};
use super::BillFileFormat;
use crate::cloud::{BillingPeriod, Normalized, RawBatch};

/// Name the imported file is stored under in a raw batch.
const PART: &str = "aliyun_bill_detail";

pub static FORMAT: BillFileFormat = BillFileFormat {
    display_name: "Bill detail export (账单明细)",
    origin_hint: "Alibaba Cloud console → Expenses and Costs → Bill Details → Export",
    extensions: &["csv", "txt"],
    part: PART,
    periods,
    normalize,
};

/// Column names across the Chinese console, the international console and
/// the `DescribeInstanceBill` field names — the three disagree about every
/// column, and an export can carry any of them.
static LAYOUT: Layout = Layout {
    anchors: &[
        "账期",
        "BillingCycle",
        "账期(月)",
        "账单月份",
        "账单日期",
        "BillingDate",
        "费用日期",
        "消费日期",
        "计费日期",
    ],
    billing_cycle: &["账期", "BillingCycle", "账期(月)", "账单月份"],
    billing_date: &[
        "账单日期",
        "BillingDate",
        "费用日期",
        "消费日期",
        "计费日期",
    ],
    product_name: &["产品名称", "ProductName", "产品"],
    product_code: &["产品代码", "ProductCode", "产品Code"],
    product_detail: &["产品明细", "ProductDetail", "产品明细名称", "明细"],
    billing_item: &["计费项", "BillingItem", "计费项目", "计费项名称"],
    instance_id: &["实例ID", "InstanceID", "资源实例ID", "实例编号"],
    instance_name: &["实例昵称", "NickName", "资源昵称", "实例名称", "资源名称"],
    region: &["地域", "Region", "所属地域", "地域名称"],
    currency: &["币种", "Currency"],
    default_currency: "CNY",
    usage: &["用量", "使用量", "Usage", "用量合计"],
    usage_unit: &["用量单位", "UsageUnit", "单位", "计量单位"],
    tag: &["标签", "Tag", "Tags", "资源标签"],
    gross: &[
        "官网价",
        "原价",
        "PretaxGrossAmount",
        "官网价格",
        "原价金额",
    ],
    net: &["应付金额", "PretaxAmount", "应付", "应付金额(元)"],
    deductions: &[
        (
            "InvoiceDiscount",
            &["优惠金额", "InvoiceDiscount", "优惠", "折扣金额"],
        ),
        (
            "DeductedByCoupons",
            &["代金券抵扣", "DeductedByCoupons", "代金券"],
        ),
        (
            "DeductedByCashCoupons",
            &[
                "优惠券抵扣",
                "DeductedByCashCoupons",
                "现金券抵扣",
                "优惠券",
            ],
        ),
        (
            "DeductedByPrepaidCard",
            &["储值卡抵扣", "DeductedByPrepaidCard", "储值卡"],
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
    use crate::cloud::deduction::UNRECONCILED;
    use crate::cloud::raw::RawPart;
    use crate::ledger::{Charge, ChargeCategory, CostBasis};

    /// One recorded bill detail export, as the Chinese console writes it.
    const BILL_DETAIL: &str = include_str!("../testdata/aliyun_bill_detail.csv");

    fn recorded_batch(text: &str, period: BillingPeriod) -> RawBatch {
        RawBatch {
            provider: "Aliyun".to_string(),
            account_id: "acct-2".to_string(),
            period,
            batch_id: "b-1".to_string(),
            fetched_at: "2026-09-01T02:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART, "file", text)],
        }
    }

    fn charges(text: &str, period: BillingPeriod) -> Vec<Charge> {
        normalize(&recorded_batch(text, period)).unwrap().charges
    }

    /// Every row for one product code, in the order the parser emitted them.
    ///
    /// Grouped by the code rather than the description because decomposing a
    /// line puts the deduction's own name on each credit row.
    fn product<'a>(charges: &'a [Charge], code: &str) -> Vec<&'a Charge> {
        charges
            .iter()
            .filter(|charge| charge.service_category.as_deref() == Some(code))
            .collect()
    }

    /// The one usage row whose billing item is `name`.
    fn item<'a>(charges: &'a [Charge], name: &str) -> &'a Charge {
        charges
            .iter()
            .find(|charge| charge.charge_description.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no row for {}", name))
    }

    fn total(charges: &[&Charge]) -> f64 {
        charges.iter().filter_map(|charge| charge.billed_cost).sum()
    }

    #[test]
    fn a_model_studio_line_carries_the_model_and_its_tokens() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));
        let charge = item(&charges, "qwen-max 输入token");

        assert_eq!(charge.service_name.as_deref(), Some("大模型服务平台百炼"));
        assert_eq!(charge.service_category.as_deref(), Some("bailian"));
        assert_eq!(charge.billed_cost, Some(12.34));
        assert_eq!(charge.billing_currency, "CNY");
        assert_eq!(charge.charge_category, ChargeCategory::Usage);
        assert_eq!(charge.cost_basis, CostBasis::Authoritative);
        // The unit the ledger schema was widened for.
        assert_eq!(charge.pricing_unit.as_deref(), Some("Tokens"));
        assert_eq!(charge.pricing_quantity, Some(1_234_567.0));
        assert_eq!(charge.resource_id.as_deref(), Some("bailian-workspace-1"));
    }

    /// The whole reason this format exists beside the billing API: the API
    /// reports Model Studio as one figure a month, this reports the models.
    #[test]
    fn model_studio_arrives_as_one_row_per_model() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));

        let mut models: Vec<&str> = product(&charges, "bailian")
            .iter()
            .filter(|charge| charge.charge_category == ChargeCategory::Usage)
            .filter_map(|charge| charge.charge_description.as_deref())
            .collect();
        models.sort();
        assert_eq!(models, vec!["qwen-max 输入token", "qwen-plus 输出token"]);
    }

    #[test]
    fn a_daily_export_dates_a_row_to_its_own_day() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));
        let charge = item(&charges, "qwen-max 输入token");

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
    fn each_deduction_becomes_its_own_credit_and_the_rows_sum_to_what_was_charged() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));
        let ecs = product(&charges, "ecs");

        assert_eq!(ecs[0].billed_cost, Some(320.50));
        assert_eq!(ecs[0].list_cost, Some(320.50));
        assert_eq!(ecs[0].charge_category, ChargeCategory::Usage);

        let credits: Vec<(&str, Option<f64>)> = ecs[1..]
            .iter()
            .map(|charge| {
                (
                    charge.charge_description.as_deref().unwrap(),
                    charge.billed_cost,
                )
            })
            .collect();
        // Named exactly as the billing API names them, so a credit is the
        // same thing whichever channel it arrived through.
        assert_eq!(
            credits,
            vec![
                ("InvoiceDiscount", Some(-22.05)),
                ("DeductedByCoupons", Some(-10.0)),
            ]
        );
        assert!(ecs[1..]
            .iter()
            .all(|charge| charge.charge_category == ChargeCategory::Credit));

        assert!((total(&ecs) - 288.45).abs() < 1e-9, "{}", total(&ecs));
    }

    /// The console appends a total line with no 账期. It carries amounts, so
    /// letting it through would double the month.
    #[test]
    fn the_total_line_the_console_appends_is_not_a_charge() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));

        assert!(charges
            .iter()
            .all(|charge| charge.service_name.as_deref() != Some("合计")));

        // 12.34 + 5.00 of Model Studio, plus 288.45 of ECS.
        let all: Vec<&Charge> = charges.iter().collect();
        assert!((total(&all) - 305.79).abs() < 1e-9, "{}", total(&all));
    }

    #[test]
    fn only_the_month_being_imported_is_read() {
        let august = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));
        let september = charges(BILL_DETAIL, BillingPeriod::new(2026, 9));

        assert_eq!(august.len(), 5, "two models, plus ECS and its two credits");
        assert_eq!(september.len(), 1);
        assert_eq!(september[0].billed_cost, Some(7.77));
        assert_eq!(
            september[0].charge_period_start.to_rfc3339(),
            "2026-09-01T00:00:00+00:00"
        );
    }

    #[test]
    fn a_file_spanning_two_months_reports_both_oldest_first() {
        assert_eq!(
            periods(BILL_DETAIL).unwrap(),
            vec![BillingPeriod::new(2026, 8), BillingPeriod::new(2026, 9)]
        );
    }

    #[test]
    fn tags_become_the_json_object_the_ledger_stores() {
        let charges = charges(BILL_DETAIL, BillingPeriod::new(2026, 8));

        assert_eq!(
            product(&charges, "ecs")[0].tags.as_deref(),
            Some(r#"{"env":"prod"}"#)
        );
        // A row whose tag cell is empty stores NULL, not an empty object.
        assert_eq!(item(&charges, "qwen-max 输入token").tags, None);
    }

    /// The English console names every column differently, and the same bill
    /// has to read the same way through either.
    #[test]
    fn the_english_export_reads_identically() {
        let english = "BillingCycle,BillingDate,ProductCode,ProductName,BillingItem,\
                       PretaxGrossAmount,InvoiceDiscount,PretaxAmount,Currency,Usage,UsageUnit\n\
                       2026-08,2026-08-09,bailian,Model Studio,qwen-max input,\
                       15.00,2.66,12.34,CNY,1234567,Tokens\n";

        let charges = charges(english, BillingPeriod::new(2026, 8));
        let usage = &charges[0];
        assert_eq!(usage.service_category.as_deref(), Some("bailian"));
        assert_eq!(usage.billed_cost, Some(15.00));
        assert_eq!(usage.pricing_unit.as_deref(), Some("Tokens"));
        // 15.00 - 2.66 is what was charged.
        let all: Vec<&Charge> = charges.iter().collect();
        assert!((total(&all) - 12.34).abs() < 1e-9);
    }

    #[test]
    fn an_export_without_a_list_price_is_read_at_what_was_charged() {
        let sparse = "账期,产品名称,计费项,应付金额\n2026-08,百炼,qwen-plus,3.50\n";
        let charges = charges(sparse, BillingPeriod::new(2026, 8));

        assert_eq!(charges.len(), 1);
        assert_eq!(charges[0].billed_cost, Some(3.50));
        assert!(charges
            .iter()
            .all(|charge| charge.charge_description.as_deref() != Some(UNRECONCILED)));
    }

    #[test]
    fn a_file_without_the_amount_column_says_what_it_does_have() {
        let error = periods("账期,产品名称\n2026-08,百炼\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("net amount"), "{}", error);
        assert!(error.contains("账期, 产品名称"), "{}", error);
    }

    #[test]
    fn a_file_that_is_not_a_bill_is_refused() {
        assert!(periods("id,name\n1,ecs\n").is_err());
    }
}
