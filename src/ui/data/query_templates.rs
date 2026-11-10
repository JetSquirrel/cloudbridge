use super::*;

// ==================== Query ====================

/// One starter query of the Query page's template picker. The SQL is what
/// runs; the category, title, and description only organize the picker.
/// A built-in template's category, title or description as shown. The
/// English text is what [`QUERY_TEMPLATES`] holds and what a template is
/// found by; this translates it for display only.
pub fn template_text(text: &'static str) -> SharedString {
    let key = match text {
        "Spend overview" => "query_tpl.cat_spend",
        "Forecast & trends" => "query_tpl.cat_forecast",
        "Composition" => "query_tpl.cat_composition",
        "Data health" => "query_tpl.cat_health",
        "Monthly spend" => "query_tpl.t_monthly",
        "Spend by provider × month" => "query_tpl.t_provider_month",
        "Daily spend, last 30 days" => "query_tpl.t_daily_30",
        "Top 20 services this month" => "query_tpl.t_top_services",
        "Spend by business line" => "query_tpl.t_business_line",
        "Unallocated spend by service" => "query_tpl.t_unallocated",
        "Month-end forecast" => "query_tpl.t_forecast",
        "Month over month by service" => "query_tpl.t_mom",
        "Spend by region this month" => "query_tpl.t_region",
        "Spend by service category this month" => "query_tpl.t_category",
        "Top 20 resources this month" => "query_tpl.t_top_resources",
        "Discount vs list price" => "query_tpl.t_discount",
        "Recent ingest batches" => "query_tpl.t_ingest",
        "Latest balance snapshots" => "query_tpl.t_balances",
        "Total billed spend per billing month, in reporting currency." => "query_tpl.d_monthly",
        "How each month's spend splits across providers, in reporting currency." => "query_tpl.d_provider_month",
        "Day-by-day spend over the last 30 days, in reporting currency." => "query_tpl.d_daily_30",
        "Which services cost the most this billing month, in reporting currency." => "query_tpl.d_top_services",
        "How spend splits across business lines, in reporting currency; charges without the business_line tag count as Unallocated." => "query_tpl.d_business_line",
        "Which services carry no business_line tag, and what they cost, in reporting currency." => "query_tpl.d_unallocated",
        "This month's spend to date, the daily run rate since the first charge, and the projected month-end total, in reporting currency." => "query_tpl.d_forecast",
        "This month vs last month per service, in reporting currency, biggest movers first." => "query_tpl.d_mom",
        "How this billing month's spend splits across regions, in reporting currency; charges with no region count as Other." => "query_tpl.d_region",
        "How this billing month's spend splits across FOCUS service categories — the same categories on every cloud — in reporting currency; charges no mapping places count as Uncategorized." => "query_tpl.d_category",
        "The individual resources costing the most this billing month, in reporting currency." => "query_tpl.d_top_resources",
        "List-price spend vs actually billed spend per service this month, in reporting currency — how much discounts and credits shave off." => "query_tpl.d_discount",
        "The 20 most recent ingest runs, with their channel, status, and row count." => "query_tpl.d_ingest",
        "Each account's most recently observed balance, one row per currency at that instant." => "query_tpl.d_balances",
        other => return SharedString::from(other),
    };
    tr!(key)
}

pub struct QueryTemplate {
    pub category: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub sql: &'static str,
}

/// The built-in starter queries. Every amount query reads
/// `v_charge_normalized`, so amounts come out in the reporting currency —
/// noted in the description or as a `currency` column, since the query
/// page has no other place that says what the numbers mean.
const QUERY_TEMPLATES: [QueryTemplate; 14] = [
    QueryTemplate {
        category: "Spend overview",
        title: "Monthly spend",
        description: "Total billed spend per billing month, in reporting currency.",
        sql: "SELECT billing_period,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
GROUP BY billing_period, reporting_currency
ORDER BY billing_period",
    },
    QueryTemplate {
        category: "Spend overview",
        title: "Spend by provider × month",
        description: "How each month's spend splits across providers, in reporting currency.",
        sql: "SELECT billing_period,
       provider,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
GROUP BY billing_period, provider, reporting_currency
ORDER BY billing_period, spend DESC",
    },
    QueryTemplate {
        category: "Spend overview",
        title: "Daily spend, last 30 days",
        description: "Day-by-day spend over the last 30 days, in reporting currency.",
        sql: "SELECT CAST(charge_period_start AS DATE) AS day,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE charge_period_start >= CURRENT_DATE - INTERVAL 30 DAY
GROUP BY day, reporting_currency
ORDER BY day",
    },
    QueryTemplate {
        category: "Composition",
        title: "Top 20 services this month",
        description: "Which services cost the most this billing month, in reporting currency.",
        sql: "SELECT provider,
       service_name,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
GROUP BY provider, service_name, reporting_currency
ORDER BY spend DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Composition",
        title: "Spend by business line",
        description: "How spend splits across business lines, in reporting currency; charges without the business_line tag count as Unallocated.",
        sql: "SELECT coalesce(nullif(json_extract_string(tags, 'business_line'), ''), 'Unallocated') AS business_line,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
GROUP BY business_line, reporting_currency
ORDER BY spend DESC",
    },
    QueryTemplate {
        category: "Composition",
        title: "Unallocated spend by service",
        description: "Which services carry no business_line tag, and what they cost, in reporting currency.",
        sql: "SELECT provider,
       service_name,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE coalesce(nullif(json_extract_string(tags, 'business_line'), ''), '') = ''
GROUP BY provider, service_name, reporting_currency
ORDER BY spend DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Forecast & trends",
        title: "Month-end forecast",
        description: "This month's spend to date, the daily run rate since the first charge, and the projected month-end total, in reporting currency.",
        sql: "WITH mtd AS (
    SELECT SUM(billed_cost_base) AS cost,
           MIN(charge_period_start::DATE) AS first_day
    FROM v_charge_normalized
    WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
)
SELECT ROUND(cost, 2) AS month_to_date,
       ROUND(cost / (CURRENT_DATE - greatest(date_trunc('month', CURRENT_DATE)::DATE, first_day) + 1), 2) AS daily_rate,
       ROUND(cost + cost / (CURRENT_DATE - greatest(date_trunc('month', CURRENT_DATE)::DATE, first_day) + 1)
             * date_diff('day', CURRENT_DATE, last_day(CURRENT_DATE)), 2) AS month_end_forecast
FROM mtd",
    },
    QueryTemplate {
        category: "Forecast & trends",
        title: "Month over month by service",
        description: "This month vs last month per service, in reporting currency, biggest movers first.",
        sql: "SELECT service_name,
       reporting_currency AS currency,
       ROUND(SUM(CASE WHEN billing_period = strftime(CURRENT_DATE, '%Y-%m') THEN billed_cost_base END), 2) AS this_month,
       ROUND(SUM(CASE WHEN billing_period = strftime(CURRENT_DATE - INTERVAL 1 MONTH, '%Y-%m') THEN billed_cost_base END), 2) AS last_month,
       ROUND(coalesce(this_month, 0) - coalesce(last_month, 0), 2) AS change
FROM v_charge_normalized
WHERE billing_period IN (strftime(CURRENT_DATE, '%Y-%m'),
                         strftime(CURRENT_DATE - INTERVAL 1 MONTH, '%Y-%m'))
GROUP BY service_name, reporting_currency
ORDER BY greatest(coalesce(this_month, 0), coalesce(last_month, 0)) DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Composition",
        title: "Spend by region this month",
        description: "How this billing month's spend splits across regions, in reporting currency; charges with no region count as Other.",
        sql: "SELECT coalesce(region_id, 'Other') AS region,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
GROUP BY region, reporting_currency
ORDER BY spend DESC",
    },
    QueryTemplate {
        category: "Composition",
        title: "Spend by service category this month",
        description: "How this billing month's spend splits across FOCUS service categories — the same categories on every cloud — in reporting currency; charges no mapping places count as Uncategorized.",
        sql: "SELECT coalesce(service_category, 'Uncategorized') AS category,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
GROUP BY category, reporting_currency
ORDER BY spend DESC",
    },
    QueryTemplate {
        category: "Composition",
        title: "Top 20 resources this month",
        description: "The individual resources costing the most this billing month, in reporting currency.",
        sql: "SELECT provider,
       service_name,
       coalesce(resource_name, resource_id) AS resource,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
  AND resource_id IS NOT NULL
GROUP BY provider, service_name, resource, reporting_currency
ORDER BY spend DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Composition",
        title: "Discount vs list price",
        description: "List-price spend vs actually billed spend per service this month, in reporting currency — how much discounts and credits shave off.",
        sql: "SELECT provider,
       service_name,
       reporting_currency AS currency,
       ROUND(SUM(list_cost * fx_rate), 2) AS list_spend,
       ROUND(SUM(billed_cost_base), 2) AS billed_spend,
       ROUND(SUM(list_cost * fx_rate) - SUM(billed_cost_base), 2) AS discount
FROM v_charge_normalized
WHERE billing_period = strftime(CURRENT_DATE, '%Y-%m')
  AND list_cost IS NOT NULL
  AND fx_rate IS NOT NULL
GROUP BY provider, service_name, reporting_currency
HAVING SUM(list_cost * fx_rate) > 0
ORDER BY discount DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Data health",
        title: "Recent ingest batches",
        description: "The 20 most recent ingest runs, with their channel, status, and row count.",
        sql: "SELECT provider,
       account_id,
       billing_period,
       channel,
       status,
       row_count,
       started_at
FROM ingest_batch
ORDER BY started_at DESC
LIMIT 20",
    },
    QueryTemplate {
        category: "Data health",
        title: "Latest balance snapshots",
        description: "Each account's most recently observed balance, one row per currency at that instant.",
        sql: "SELECT s.provider,
       s.account_id,
       s.observed_at,
       s.balance,
       s.currency
FROM fct_balance_snapshot s
JOIN (
    SELECT provider, account_id, MAX(observed_at) AS observed_at
    FROM fct_balance_snapshot
    GROUP BY provider, account_id
) latest
  ON s.provider = latest.provider
 AND s.account_id = latest.account_id
 AND s.observed_at = latest.observed_at
ORDER BY s.provider, s.account_id",
    },
];

/// The starter queries the Query page's template picker lists.
/// The built-in query for the spend no business line claims.
pub fn unallocated_query() -> Option<&'static str> {
    query_templates()
        .iter()
        .find(|template| template.title == "Unallocated spend by service")
        .map(|template| template.sql)
}

/// The daily charges behind a cost alert: its provider and service, for the
/// 30 days up to the day it fired, so the spike shows against what came
/// before it. A past month's alert stays traceable this way, which the
/// Attribution page, showing only the current period, cannot do.
pub fn alert_trace_query(provider: &str, service: &str, fired: DateTime<Utc>) -> String {
    let quoted = |text: &str| text.replace('\'', "''");
    format!(
        "SELECT CAST(charge_period_start AS DATE) AS day,
       provider,
       service_name,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE provider = '{}'
  AND service_name = '{}'
  AND charge_period_start >= DATE '{}' - INTERVAL 30 DAY
  AND charge_period_start < DATE '{}' + INTERVAL 1 DAY
GROUP BY day, provider, service_name, reporting_currency
ORDER BY day DESC",
        quoted(provider),
        quoted(service),
        fired.format("%Y-%m-%d"),
        fired.format("%Y-%m-%d"),
    )
}

/// A resource's charges by month, what a finding on the Insights page rests
/// on. The bill can name it by a longer id than the scan, an ARN for one, so
/// it matches on the id's last part as the Insights arithmetic does.
pub fn resource_trace_query(provider: &str, resource_id: &str, resource_key: &str) -> String {
    let quoted = |text: &str| text.replace('\'', "''");
    let pattern = quoted(resource_key)
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!(
        "SELECT billing_period,
       service_name,
       charge_description,
       reporting_currency AS currency,
       ROUND(SUM(billed_cost_base), 2) AS spend
FROM v_charge_normalized
WHERE lower(provider) = lower('{}')
  AND (resource_id = '{}' OR resource_id LIKE '%{}' ESCAPE '\\')
GROUP BY billing_period, service_name, charge_description, reporting_currency
ORDER BY billing_period DESC, spend DESC",
        quoted(provider),
        quoted(resource_id),
        pattern,
    )
}

pub fn query_templates() -> &'static [QueryTemplate] {
    &QUERY_TEMPLATES
}

/// One ad-hoc query's result, ready to render: column names, which columns
/// are numeric (right-aligned), the rows as display strings, and whether
/// the row cap cut the tail off.
pub struct QueryResultData {
    pub columns: Vec<String>,
    pub numeric: Vec<bool>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// Run an ad-hoc SQL query against the ledger. Blocking; wrap in
/// `smol::unblock`. The error arrives as text because the page renders it
/// verbatim in the result pane.
pub fn run_adhoc_query(sql: String) -> std::result::Result<QueryResultData, String> {
    query::run_adhoc(&sql)
        .map(|result| QueryResultData {
            columns: result.columns,
            numeric: result.numeric,
            rows: result.rows,
            truncated: result.truncated,
            elapsed_ms: result.elapsed_ms,
        })
        .map_err(|e| e.to_string())
}
