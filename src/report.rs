//! The monthly cost review: one self-contained HTML file a person can save,
//! print and pass on.
//!
//! This is composition only. Every figure arrives in [`ReportData`] from the
//! queries the pages already read, so a report cannot disagree with the screen
//! and there is no second calculation to keep in step. Rendering is pure: the
//! same data and the same `generated_at` give the same bytes.
//!
//! A report states its limits rather than implying a clean bill. "No inventory
//! was scanned" and "no findings" are different sentences here, on purpose.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};

use crate::i18n::bucket_label;
use crate::model::{BudgetStatus, Channel, DataQualityIssue, InsightFinding, IssueSeverity};
use crate::ui::fmt;

/// How many services the contributor table lists before folding the rest.
pub const TOP_SERVICES: usize = 15;

/// How many Insights findings a report lists.
pub const TOP_FINDINGS: usize = 10;

/// One row of the contributor table: this month against the one before.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceRow {
    pub service: String,
    pub current: f64,
    pub previous: f64,
}

/// One billing source behind the month.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRow {
    pub provider: String,
    pub account: String,
    pub channel: Channel,
    pub last_ingest: Option<DateTime<Utc>>,
}

/// What the report can say about Insights.
#[derive(Debug, Clone, PartialEq)]
pub enum InsightsSection {
    /// Nothing was scanned, so nothing was judged.
    NotScanned,
    Evaluated {
        scanned_at: DateTime<Utc>,
        /// Some part of the scan failed, so a resource may just be unreached.
        partial: bool,
        resources: usize,
        total_findings: usize,
        /// The first, in the Insights page's order, at most [`TOP_FINDINGS`].
        findings: Vec<InsightFinding>,
    },
}

/// The month-end forecast, for a month still in progress.
#[derive(Debug, Clone, PartialEq)]
pub struct ForecastLine {
    pub forecast: f64,
    pub days_elapsed: i64,
    pub days_in_month: i64,
}

/// Everything a report shows. Nothing here is a credential or a raw payload.
pub struct ReportData {
    pub period: String,
    pub currency: String,
    pub generated_at: DateTime<Utc>,
    pub period_open: bool,
    /// Demo accounts are configured, so this may not be a real bill.
    pub has_demo_accounts: bool,
    pub net: f64,
    pub previous_net: f64,
    pub usage: f64,
    pub credits: f64,
    /// By service, largest first, whole: the render folds the tail. The query
    /// behind it leaves out a net-negative service, so these need not add up
    /// to `net`; the table shows what is left as a row of its own.
    pub services: Vec<ServiceRow>,
    /// Net by region, largest first.
    pub regions: Vec<(String, f64)>,
    /// Usage by business line, largest first.
    pub business_lines: Vec<(String, f64)>,
    pub forecast: Option<ForecastLine>,
    pub budgets: Vec<BudgetStatus>,
    pub data_quality: Vec<DataQualityIssue>,
    pub insights: InsightsSection,
    pub sources: Vec<SourceRow>,
    /// Charges with no exchange rate, which are in no total above.
    pub unconverted_charges: i64,
    /// Usage rows with no amount, which are in no total above.
    pub rows_without_amount: i64,
}

impl ReportData {
    /// Whether the ledger holds anything for this month. Without this, an
    /// empty month would read as one that cost nothing.
    pub fn has_data(&self) -> bool {
        !self.sources.is_empty()
    }
}

/// Percent change, unless the base is too small for a ratio to mean anything.
/// The rule the Overview's headline uses.
pub fn change_pct(current: f64, previous: f64) -> Option<f64> {
    (previous.abs() >= fmt::DUST_THRESHOLD).then(|| (current - previous) / previous.abs() * 100.0)
}

/// Pair each service's two months, largest this month first.
pub fn service_rows(current: &[(String, f64)], previous: &[(String, f64)]) -> Vec<ServiceRow> {
    let mut rows: Vec<ServiceRow> = current
        .iter()
        .map(|(service, amount)| ServiceRow {
            service: service.clone(),
            current: *amount,
            previous: previous
                .iter()
                .find(|(other, _)| other == service)
                .map_or(0.0, |(_, amount)| *amount),
        })
        .collect();
    rows.extend(
        previous
            .iter()
            .filter(|(service, _)| !current.iter().any(|(other, _)| other == service))
            .map(|(service, amount)| ServiceRow {
                service: service.clone(),
                current: 0.0,
                previous: *amount,
            }),
    );
    rows.sort_by(|a, b| {
        b.current
            .abs()
            .total_cmp(&a.current.abs())
            .then_with(|| a.service.cmp(&b.service))
    });
    rows
}

/// Text placed into HTML. Provider data is not ours to trust: a service or
/// resource name reaches the page only through this.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn tr(key: &str) -> String {
    rust_i18n::t!(key).to_string()
}

fn at_text(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

const STYLE: &str = "\
:root{color-scheme:light}\
*{box-sizing:border-box}\
body{margin:0 auto;max-width:60rem;padding:2rem;color:#1f2328;background:#fff;\
font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'PingFang SC',\
'Microsoft YaHei',sans-serif}\
h1{font-size:1.5rem;margin:0}\
h2{font-size:1.05rem;margin:1.75rem 0 .5rem}\
p{margin:.25rem 0}\
.context{color:#59636e;margin:.25rem 0 1rem}\
.summary{font-size:1rem}\
.notice{border:1px solid #d0d7de;border-left:3px solid #9a6700;padding:.5rem .75rem;margin:.75rem 0}\
ul{margin:.25rem 0;padding-left:1.25rem}\
table{width:100%;border-collapse:collapse;margin:.25rem 0}\
th,td{padding:.3rem .5rem;border-bottom:1px solid #d0d7de;text-align:left;vertical-align:top}\
th{color:#59636e;font-weight:600;font-size:.85rem}\
.n{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}\
.muted{color:#59636e}\
tr{break-inside:avoid}\
h2{break-after:avoid}\
@media print{body{max-width:none;padding:0}}";

/// Render the report as one HTML document with nothing to fetch.
pub fn render_html(data: &ReportData) -> String {
    let currency = data.currency.as_str();
    let money = |value: f64| escape(&fmt::exact_amount(value, currency));
    // A month in progress against a whole one is not a change: three days
    // would read as a collapse.
    let change_cell = |current: f64, previous: f64| {
        if data.period_open {
            "—".to_string()
        } else {
            escape(&fmt::signed_exact_amount(current - previous, currency))
        }
    };

    let mut html = String::with_capacity(16 * 1024);
    let _ = write!(
        html,
        "<!doctype html><html lang=\"{lang}\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{STYLE}</style></head><body>",
        lang = escape(&rust_i18n::locale()),
        title = escape(&rust_i18n::t!(
            "report.document_title",
            period = data.period
        )),
    );
    let _ = write!(
        html,
        "<h1>{}</h1><p class=\"context\">{}</p>",
        escape(&rust_i18n::t!("report.heading", period = data.period)),
        escape(&rust_i18n::t!(
            "report.context",
            currency = currency,
            at = at_text(data.generated_at)
        )),
    );

    if data.has_demo_accounts {
        let _ = write!(
            html,
            "<p class=\"notice\">{}</p>",
            escape(&tr("report.demo_notice"))
        );
    }

    if !data.has_data() {
        let _ = write!(
            html,
            "<p class=\"notice\">{}</p>",
            escape(&rust_i18n::t!("report.no_data", period = data.period))
        );
        html.push_str(&render_limits(data));
        html.push_str("</body></html>");
        return html;
    }

    // Summary.
    let change = change_pct(data.net, data.previous_net).filter(|_| !data.period_open);
    let summary = match change {
        Some(pct) => rust_i18n::t!(
            "report.summary_change",
            net = fmt::exact_amount(data.net, currency),
            change = fmt::change_pct(pct),
            previous = fmt::exact_amount(data.previous_net, currency),
            usage = fmt::exact_amount(data.usage, currency),
            credits = fmt::exact_amount(data.credits, currency)
        ),
        None if data.period_open => rust_i18n::t!(
            "report.summary_open",
            net = fmt::exact_amount(data.net, currency),
            previous = fmt::exact_amount(data.previous_net, currency),
            usage = fmt::exact_amount(data.usage, currency),
            credits = fmt::exact_amount(data.credits, currency)
        ),
        None => rust_i18n::t!(
            "report.summary_no_change",
            net = fmt::exact_amount(data.net, currency),
            usage = fmt::exact_amount(data.usage, currency),
            credits = fmt::exact_amount(data.credits, currency)
        ),
    };
    let _ = write!(html, "<p class=\"summary\">{}</p>", escape(&summary));
    html.push_str(&render_limits(data));

    // Investigation: where the money went.
    let _ = write!(
        html,
        "<h2>{}</h2><table><thead><tr><th>{}</th><th class=\"n\">{}</th><th class=\"n\">{}</th>\
         <th class=\"n\">{}</th></tr></thead><tbody>",
        escape(&tr("report.services")),
        escape(&tr("report.col_service")),
        escape(&data.period),
        escape(&tr("report.col_previous")),
        escape(&tr("report.col_change")),
    );
    for row in data.services.iter().take(TOP_SERVICES) {
        let _ = write!(
            html,
            "<tr><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td></tr>",
            escape(&bucket_label(&row.service)),
            money(row.current),
            money(row.previous),
            change_cell(row.current, row.previous),
        );
    }
    if data.services.len() > TOP_SERVICES {
        let rest = &data.services[TOP_SERVICES..];
        let current: f64 = rest.iter().map(|row| row.current).sum();
        let previous: f64 = rest.iter().map(|row| row.previous).sum();
        let _ = write!(
            html,
            "<tr><td class=\"muted\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td>\
             <td class=\"n\">{}</td></tr>",
            escape(&rust_i18n::t!("report.other_services", n = rest.len())),
            money(current),
            money(previous),
            change_cell(current, previous),
        );
    }
    // What the service rows do not reach: credits and any other net-negative
    // service. Without this the column would not add up to the total under it.
    let listed_current: f64 = data.services.iter().map(|row| row.current).sum();
    let listed_previous: f64 = data.services.iter().map(|row| row.previous).sum();
    let (rest_current, rest_previous) = (
        data.net - listed_current,
        data.previous_net - listed_previous,
    );
    let tolerance = fmt::DUST_THRESHOLD;
    if rest_current.abs() >= tolerance || rest_previous.abs() >= tolerance {
        let _ = write!(
            html,
            "<tr><td class=\"muted\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td>\
             <td class=\"n\">{}</td></tr>",
            escape(&tr("report.not_by_service")),
            money(rest_current),
            money(rest_previous),
            change_cell(rest_current, rest_previous),
        );
    }
    let _ = write!(
        html,
        "<tr><th>{}</th><th class=\"n\">{}</th><th class=\"n\">{}</th><th class=\"n\">{}</th></tr>\
         </tbody></table>",
        escape(&tr("report.net_total")),
        money(data.net),
        money(data.previous_net),
        change_cell(data.net, data.previous_net),
    );

    html.push_str(&amount_table(
        &tr("report.regions"),
        &tr("report.col_region"),
        &data.regions,
        currency,
    ));
    html.push_str(&amount_table(
        &tr("report.business_lines"),
        &tr("report.col_business_line"),
        &data.business_lines,
        currency,
    ));

    html.push_str(&render_outlook(data));

    // Evidence.
    html.push_str(&render_data_quality(data));
    html.push_str(&render_insights(data));
    html.push_str(&render_sources(data));
    html.push_str("</body></html>");
    html
}

fn amount_table(title: &str, column: &str, rows: &[(String, f64)], currency: &str) -> String {
    let mut html = format!("<h2>{}</h2>", escape(title));
    if rows.is_empty() {
        let _ = write!(
            html,
            "<p class=\"muted\">{}</p>",
            escape(&tr("report.no_rows"))
        );
        return html;
    }
    let _ = write!(
        html,
        "<table><thead><tr><th>{}</th><th class=\"n\">{}</th></tr></thead><tbody>",
        escape(column),
        escape(&tr("report.col_amount")),
    );
    for (label, amount) in rows {
        let _ = write!(
            html,
            "<tr><td>{}</td><td class=\"n\">{}</td></tr>",
            escape(&bucket_label(label)),
            escape(&fmt::exact_amount(*amount, currency)),
        );
    }
    html.push_str("</tbody></table>");
    html
}

/// What this report's figures leave out. Said up front, beside the summary.
fn render_limits(data: &ReportData) -> String {
    let mut limits = Vec::new();
    if data.period_open {
        limits.push(tr("report.limit_open"));
    }
    if data.unconverted_charges > 0 {
        limits.push(
            rust_i18n::t!(
                "report.limit_unconverted",
                rows = data.unconverted_charges,
                currency = data.currency
            )
            .to_string(),
        );
    }
    if data.rows_without_amount > 0 {
        limits.push(
            rust_i18n::t!("report.limit_absent", rows = data.rows_without_amount).to_string(),
        );
    }
    match &data.insights {
        InsightsSection::NotScanned => limits.push(tr("report.limit_not_scanned")),
        InsightsSection::Evaluated { partial: true, .. } => {
            limits.push(tr("report.limit_partial_scan"))
        }
        InsightsSection::Evaluated { .. } => {}
    }
    if limits.is_empty() {
        return String::new();
    }
    let mut html = format!("<h2>{}</h2><ul>", escape(&tr("report.limits")));
    for limit in limits {
        let _ = write!(html, "<li>{}</li>", escape(&limit));
    }
    html.push_str("</ul>");
    html
}

/// Forecast and budgets, which only mean anything for a month in progress.
fn render_outlook(data: &ReportData) -> String {
    let currency = data.currency.as_str();
    let mut html = format!("<h2>{}</h2>", escape(&tr("report.outlook")));
    if !data.period_open {
        let _ = write!(
            html,
            "<p class=\"muted\">{}</p>",
            escape(&tr("report.outlook_skipped"))
        );
        return html;
    }
    match &data.forecast {
        Some(forecast) => {
            let _ = write!(
                html,
                "<p>{}</p>",
                escape(&rust_i18n::t!(
                    "report.forecast",
                    forecast = fmt::exact_amount(forecast.forecast, currency),
                    days = forecast.days_elapsed,
                    total = forecast.days_in_month
                ))
            );
        }
        None => {
            let _ = write!(
                html,
                "<p class=\"muted\">{}</p>",
                escape(&tr("report.forecast_missing"))
            );
        }
    }
    if data.budgets.is_empty() {
        let _ = write!(
            html,
            "<p class=\"muted\">{}</p>",
            escape(&tr("report.no_budgets"))
        );
        return html;
    }
    let _ = write!(
        html,
        "<table><thead><tr><th>{}</th><th class=\"n\">{}</th><th class=\"n\">{}</th>\
         <th class=\"n\">{}</th></tr></thead><tbody>",
        escape(&tr("report.col_account")),
        escape(&tr("report.col_budget")),
        escape(&tr("report.col_spent")),
        escape(&tr("report.col_used")),
    );
    for budget in &data.budgets {
        let _ = write!(
            html,
            "<tr><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{:.0}%</td></tr>",
            escape(&budget.account_name),
            escape(&fmt::exact_amount(budget.monthly_budget, &budget.currency)),
            escape(&fmt::exact_amount(budget.current_cost, &budget.currency)),
            budget.percentage_used,
        );
    }
    html.push_str("</tbody></table>");
    html
}

fn severity_text(severity: IssueSeverity) -> String {
    match severity {
        IssueSeverity::Critical => tr("report.severity_critical"),
        IssueSeverity::Warning => tr("report.severity_warning"),
        IssueSeverity::Info => tr("report.severity_info"),
    }
}

fn render_data_quality(data: &ReportData) -> String {
    let mut html = format!("<h2>{}</h2>", escape(&tr("report.data_health")));
    if data.data_quality.is_empty() {
        let _ = write!(
            html,
            "<p class=\"muted\">{}</p>",
            escape(&tr("report.data_health_clean"))
        );
        return html;
    }
    html.push_str("<ul>");
    for issue in &data.data_quality {
        let _ = write!(
            html,
            "<li><strong>{}:</strong> {}</li>",
            escape(&severity_text(issue.severity)),
            escape(&issue.message),
        );
    }
    html.push_str("</ul>");
    html
}

fn render_insights(data: &ReportData) -> String {
    let currency = data.currency.as_str();
    let mut html = format!("<h2>{}</h2>", escape(&tr("report.insights")));
    let InsightsSection::Evaluated {
        scanned_at,
        partial,
        resources,
        total_findings,
        findings,
    } = &data.insights
    else {
        let _ = write!(
            html,
            "<p class=\"notice\">{}</p>",
            escape(&tr("report.insights_unavailable"))
        );
        return html;
    };
    let _ = write!(
        html,
        "<p class=\"muted\">{}</p>",
        escape(&rust_i18n::t!(
            "report.insights_scope",
            resources = resources,
            at = at_text(*scanned_at)
        ))
    );
    if findings.is_empty() {
        let key = if *partial {
            "report.insights_none_partial"
        } else {
            "report.insights_none"
        };
        let _ = write!(html, "<p>{}</p>", escape(&tr(key)));
        return html;
    }
    let _ = write!(
        html,
        "<table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th class=\"n\">{}</th></tr></thead><tbody>",
        escape(&tr("report.col_resource")),
        escape(&tr("report.col_type")),
        escape(&tr("report.col_evidence")),
        escape(&tr("report.col_amount")),
    );
    for finding in findings {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"n\">{}</td></tr>",
            escape(finding.name.as_deref().unwrap_or(&finding.resource_id)),
            escape(&finding.resource_kind),
            escape(&finding.evidence),
            escape(&fmt::exact_amount(finding.cost, currency)),
        );
    }
    html.push_str("</tbody></table>");
    if total_findings > &findings.len() {
        let _ = write!(
            html,
            "<p class=\"muted\">{}</p>",
            escape(&rust_i18n::t!(
                "report.insights_more",
                shown = findings.len(),
                total = total_findings
            ))
        );
    }
    html
}

fn render_sources(data: &ReportData) -> String {
    let mut html = format!(
        "<h2>{}</h2><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
        escape(&tr("report.sources")),
        escape(&tr("report.col_source")),
        escape(&tr("report.col_account")),
        escape(&tr("report.col_channel")),
        escape(&tr("report.col_last_ingest")),
    );
    for source in &data.sources {
        let channel = match source.channel {
            Channel::Api => tr("report.channel_api"),
            Channel::File => tr("report.channel_file"),
        };
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&source.provider),
            escape(&source.account),
            escape(&channel),
            source.last_ingest.map_or_else(|| "—".to_string(), at_text),
        );
    }
    html.push_str("</tbody></table>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataQualityKind, InsightKind};
    use chrono::TimeZone;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap()
    }

    fn data() -> ReportData {
        ReportData {
            period: "2026-09".to_string(),
            currency: "USD".to_string(),
            generated_at: at(),
            period_open: false,
            has_demo_accounts: false,
            net: 880.0,
            previous_net: 800.0,
            usage: 1000.0,
            credits: -120.0,
            services: service_rows(
                &[("EC2".to_string(), 700.0), ("S3".to_string(), 180.0)],
                &[("EC2".to_string(), 640.0), ("Glue".to_string(), 160.0)],
            ),
            regions: vec![("us-east-1".to_string(), 880.0)],
            business_lines: vec![("Platform".to_string(), 1000.0)],
            forecast: None,
            budgets: Vec::new(),
            data_quality: Vec::new(),
            insights: InsightsSection::NotScanned,
            sources: vec![SourceRow {
                provider: "AWS".to_string(),
                account: "Prod".to_string(),
                channel: Channel::File,
                last_ingest: Some(at()),
            }],
            unconverted_charges: 0,
            rows_without_amount: 0,
        }
    }

    fn finding(resource: &str, evidence: &str) -> InsightFinding {
        InsightFinding {
            kind: InsightKind::Unclaimed,
            provider: "AWS".to_string(),
            cloud_account_id: None,
            resource_id: resource.to_string(),
            name: None,
            resource_kind: "ec2:volume".to_string(),
            region: None,
            cost: 12.5,
            evidence: evidence.to_string(),
        }
    }

    #[test]
    fn a_report_carries_totals_contributors_and_attribution() {
        let html = render_html(&data());
        assert!(html.contains("$880.00"));
        assert!(html.contains("$800.00"));
        assert!(html.contains("+10.0%") || html.contains("10.0%"));
        assert!(html.contains("EC2"));
        assert!(
            html.contains("Glue"),
            "a service gone this month still shows"
        );
        assert!(html.contains("us-east-1"));
        assert!(html.contains("Platform"));
    }

    #[test]
    fn credits_stay_negative_and_the_net_is_the_ledgers() {
        let html = render_html(&data());
        assert!(html.contains("-$120.00"));
        assert!(html.contains("$1,000.00"));
    }

    #[test]
    fn no_inventory_says_insights_were_unavailable_not_that_nothing_was_found() {
        let html = render_html(&data());
        assert!(html.contains(&escape(&tr("report.insights_unavailable"))));
        assert!(!html.contains(&escape(&tr("report.insights_none"))));
        assert!(html.contains(&escape(&tr("report.limit_not_scanned"))));
    }

    #[test]
    fn a_partial_scan_with_no_findings_does_not_read_as_clean() {
        let mut data = data();
        data.insights = InsightsSection::Evaluated {
            scanned_at: at(),
            partial: true,
            resources: 4,
            total_findings: 0,
            findings: Vec::new(),
        };
        let html = render_html(&data);
        assert!(html.contains(&escape(&tr("report.insights_none_partial"))));
        assert!(html.contains(&escape(&tr("report.limit_partial_scan"))));
        assert!(!html.contains(&escape(&tr("report.insights_none"))));
    }

    #[test]
    fn findings_are_listed_with_their_evidence_and_a_count_of_the_rest() {
        let mut data = data();
        data.insights = InsightsSection::Evaluated {
            scanned_at: at(),
            partial: false,
            resources: 40,
            total_findings: 25,
            findings: vec![finding("vol-1", "Unattached for 30 days")],
        };
        let html = render_html(&data);
        assert!(html.contains("vol-1"));
        assert!(html.contains("Unattached for 30 days"));
        assert!(html.contains("$12.50"));
        assert!(html.contains("25"));
    }

    #[test]
    fn missing_rates_and_usage_only_rows_are_called_out() {
        let mut data = data();
        data.unconverted_charges = 3;
        data.rows_without_amount = 7;
        let html = render_html(&data);
        assert!(html.contains(&escape(&rust_i18n::t!(
            "report.limit_unconverted",
            rows = 3,
            currency = "USD"
        ))));
        assert!(html.contains(&escape(&rust_i18n::t!("report.limit_absent", rows = 7))));
    }

    #[test]
    fn an_empty_month_says_so_instead_of_showing_zero() {
        let mut data = data();
        data.sources.clear();
        data.net = 0.0;
        data.previous_net = 0.0;
        data.services.clear();
        let html = render_html(&data);
        assert!(html.contains(&escape(&rust_i18n::t!(
            "report.no_data",
            period = "2026-09"
        ))));
        assert!(!html.contains("$0.00"));
    }

    #[test]
    fn data_health_warnings_appear_and_an_open_month_is_flagged() {
        let mut data = data();
        data.period_open = true;
        data.forecast = Some(ForecastLine {
            forecast: 1200.0,
            days_elapsed: 10,
            days_in_month: 30,
        });
        data.data_quality = vec![DataQualityIssue {
            kind: DataQualityKind::MissingRegion,
            severity: IssueSeverity::Warning,
            message: "4 charges have no region".to_string(),
            affected_amount: None,
            affected_count: 4,
        }];
        let html = render_html(&data);
        assert!(html.contains("4 charges have no region"));
        assert!(html.contains("$1,200.00"));
        assert!(html.contains(&escape(&tr("report.limit_open"))));
    }

    #[test]
    fn a_month_in_progress_is_not_compared_as_a_change_with_a_whole_month() {
        let mut data = data();
        data.period_open = true;
        data.net = 100.0;
        data.previous_net = 800.0;
        data.usage = 100.0;
        data.credits = 0.0;
        data.services = service_rows(&[("EC2".to_string(), 100.0)], &[("EC2".to_string(), 800.0)]);
        let html = render_html(&data);
        assert!(!html.contains("-87.5%"));
        assert!(!html.contains("-$700.00"));
        assert!(html.contains("$800.00"));
    }

    #[test]
    fn credits_the_service_rows_leave_out_get_a_row_so_the_table_adds_up() {
        let mut data = data();
        data.services = service_rows(
            &[("EC2".to_string(), 1000.0)],
            &[("EC2".to_string(), 800.0)],
        );
        let html = render_html(&data);
        assert!(html.contains(&escape(&tr("report.not_by_service"))));
        assert!(html.contains("-$120.00"));

        data.net = 1000.0;
        data.previous_net = 800.0;
        assert!(!render_html(&data).contains(&escape(&tr("report.not_by_service"))));
    }

    #[test]
    fn a_closed_month_says_the_forecast_was_skipped() {
        let html = render_html(&data());
        assert!(html.contains(&escape(&tr("report.outlook_skipped"))));
    }

    #[test]
    fn a_long_tail_folds_and_the_table_still_reconciles() {
        let mut data = data();
        let current: Vec<(String, f64)> = (0..20)
            .map(|index| (format!("svc-{index:02}"), 10.0))
            .collect();
        data.services = service_rows(&current, &[]);
        let html = render_html(&data);
        assert!(html.contains("svc-14"));
        assert!(!html.contains("svc-15"));
        assert!(html.contains("$50.00"), "the five folded services");
    }

    #[test]
    fn a_small_previous_month_gives_no_percent() {
        assert_eq!(change_pct(100.0, 0.0), None);
        assert_eq!(change_pct(100.0, 50.0), Some(100.0));
        assert_eq!(change_pct(50.0, 100.0), Some(-50.0));
        // A credit-heavy base keeps the direction of the move.
        assert_eq!(change_pct(-50.0, -100.0), Some(50.0));
    }

    #[test]
    fn provider_text_cannot_inject_markup() {
        let mut data = data();
        data.services[0].service = "<script>alert('x')</script>".to_string();
        data.sources[0].account = "A&B \"prod\"".to_string();
        data.insights = InsightsSection::Evaluated {
            scanned_at: at(),
            partial: false,
            resources: 1,
            total_findings: 1,
            findings: vec![finding("<img src=x>", "<b>bold</b>")],
        };
        let html = render_html(&data);
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("<b>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("A&amp;B &quot;prod&quot;"));
    }

    #[test]
    fn the_same_data_renders_the_same_bytes() {
        assert_eq!(render_html(&data()), render_html(&data()));
    }

    #[test]
    fn the_file_fetches_nothing() {
        let html = render_html(&data());
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("<link"));
        assert!(!html.contains("src="));
    }

    #[test]
    fn a_demo_ledger_is_labelled() {
        let mut data = data();
        data.has_demo_accounts = true;
        assert!(render_html(&data).contains(&escape(&tr("report.demo_notice"))));
    }
}
