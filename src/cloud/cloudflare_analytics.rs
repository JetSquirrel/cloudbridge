//! Splitting a Cloudflare bill across the resources that ran it.
//!
//! The billable-usage API stops at the account: "R2 Class B operations,
//! 734 on 3 October". Which bucket made them is only in the GraphQL
//! Analytics API, which counts usage per bucket, Worker, D1 database and
//! Durable Object namespace per day. Each of the bill's day-rows is split
//! across the resources in proportion to their share of that day's usage
//! of the same meter, so the rows still add up to what Cloudflare stated —
//! and a runaway reads as *this* namespace, not as "Durable Objects".
//!
//! An allocation, not a bill: analytics are sampled, and a free allowance
//! is the account's, not a resource's, so each split row is
//! [`CostBasis::Estimated`]. A row whose meter has no analytics — an
//! unrecognized service, a query the token may not make — is kept whole,
//! at the account.
//!
//! Like the rest of the source, fetching and splitting are apart: the
//! analytics responses ride in the raw batch, and a mapping fix replays
//! over them without asking Cloudflare again.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::NaiveDate;
use serde_json::Value;

use super::raw::RawPart;
use crate::model::{Charge, CostBasis};

/// Prefix of the analytics payloads in a raw batch; the dataset's key
/// follows it.
pub const PART_ANALYTICS_PREFIX: &str = "analytics:";

/// One billed meter whose usage the analytics can tell apart by resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Meter {
    R2ClassA,
    R2ClassB,
    R2Storage,
    /// Requests and CPU time alike: the analytics give each Worker's
    /// requests, and CPU is split by them too.
    WorkersRequests,
    D1RowsRead,
    D1RowsWritten,
    D1Storage,
    DurableObjectRequests,
    DurableObjectDuration,
    DurableObjectRowsRead,
    DurableObjectRowsWritten,
    DurableObjectStorage,
}

/// Which meter a bill row is for, from its family and service name, or
/// `None` for one the analytics cannot split.
///
/// Read from the name because that is all a row says. R2's names are the
/// ones a real bill carries ("R2 Storage Class B Operations (First 10M
/// included)"); the others follow Cloudflare's pricing pages, and a name
/// that does not match is simply not split.
pub fn meter_of(family: Option<&str>, service: Option<&str>) -> Option<Meter> {
    let family = family.unwrap_or_default().to_lowercase();
    let service = service.unwrap_or_default().to_lowercase();
    let either = |needle: &str| family.contains(needle) || service.contains(needle);

    if family == "r2" || service.starts_with("r2 ") {
        return if service.contains("class a") {
            Some(Meter::R2ClassA)
        } else if service.contains("class b") {
            Some(Meter::R2ClassB)
        } else if service.contains("storage") {
            Some(Meter::R2Storage)
        } else {
            None
        };
    }
    if either("durable object") {
        return if service.contains("request") {
            Some(Meter::DurableObjectRequests)
        } else if service.contains("duration") || service.contains("gb-s") {
            Some(Meter::DurableObjectDuration)
        } else if service.contains("read") {
            Some(Meter::DurableObjectRowsRead)
        } else if service.contains("writ") {
            Some(Meter::DurableObjectRowsWritten)
        } else if service.contains("storage") {
            Some(Meter::DurableObjectStorage)
        } else {
            None
        };
    }
    if family == "d1" || service.starts_with("d1 ") {
        return if service.contains("read") {
            Some(Meter::D1RowsRead)
        } else if service.contains("writ") {
            Some(Meter::D1RowsWritten)
        } else if service.contains("storage") {
            Some(Meter::D1Storage)
        } else {
            None
        };
    }
    // KV and Queues bill under the Workers family too; their usage is not
    // a Worker's.
    if either("kv") || either("queue") {
        return None;
    }
    if either("worker") {
        return Some(Meter::WorkersRequests);
    }
    None
}

/// How a dataset is filtered by time: by instant, or by day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeFilter {
    Datetime,
    Date,
}

/// One GraphQL Analytics dataset, as CloudBridge queries it.
#[derive(Debug)]
pub struct Dataset {
    /// The part key it is stored under.
    pub key: &'static str,
    /// The dataset's field under `viewer.accounts`.
    name: &'static str,
    /// `sum` for counts, `max` for a level such as bytes stored.
    aggregate: &'static str,
    fields: &'static [&'static str],
    /// `date` first, then the resource dimension and any other.
    dimensions: &'static [&'static str],
    filter: TimeFilter,
}

const DATASETS: &[Dataset] = &[
    Dataset {
        key: "r2_operations",
        name: "r2OperationsAdaptiveGroups",
        aggregate: "sum",
        fields: &["requests"],
        dimensions: &["date", "bucketName", "actionType"],
        filter: TimeFilter::Datetime,
    },
    Dataset {
        key: "r2_storage",
        name: "r2StorageAdaptiveGroups",
        aggregate: "max",
        fields: &["payloadSize"],
        dimensions: &["date", "bucketName"],
        filter: TimeFilter::Datetime,
    },
    Dataset {
        key: "workers",
        name: "workersInvocationsAdaptive",
        aggregate: "sum",
        fields: &["requests"],
        dimensions: &["date", "scriptName"],
        filter: TimeFilter::Datetime,
    },
    Dataset {
        key: "d1",
        name: "d1AnalyticsAdaptiveGroups",
        aggregate: "sum",
        fields: &["rowsRead", "rowsWritten"],
        dimensions: &["date", "databaseId"],
        filter: TimeFilter::Date,
    },
    Dataset {
        key: "d1_storage",
        name: "d1StorageAdaptiveGroups",
        aggregate: "max",
        fields: &["databaseSizeBytes"],
        dimensions: &["date", "databaseId"],
        filter: TimeFilter::Date,
    },
    Dataset {
        key: "durable_object_invocations",
        name: "durableObjectsInvocationsAdaptiveGroups",
        aggregate: "sum",
        fields: &["requests"],
        dimensions: &["date", "namespaceId"],
        filter: TimeFilter::Date,
    },
    Dataset {
        key: "durable_object_periodic",
        name: "durableObjectsPeriodicGroups",
        aggregate: "sum",
        fields: &["activeTime", "rowsRead", "rowsWritten"],
        dimensions: &["date", "namespaceId"],
        filter: TimeFilter::Date,
    },
    Dataset {
        key: "durable_object_storage",
        name: "durableObjectsStorageGroups",
        aggregate: "max",
        fields: &["storedBytes"],
        dimensions: &["date", "namespaceId"],
        filter: TimeFilter::Date,
    },
];

/// Where a meter's usage is read: which dataset, which figure, which
/// dimension names the resource, and — for R2's operations — which action
/// types count.
struct Source {
    dataset: &'static str,
    field: &'static str,
    resource: &'static str,
    actions: Option<&'static [&'static str]>,
}

/// R2's Class A operations, as its pricing page lists them.
const R2_CLASS_A: &[&str] = &[
    "ListBuckets",
    "PutBucket",
    "ListObjects",
    "PutObject",
    "CopyObject",
    "CompleteMultipartUpload",
    "CreateMultipartUpload",
    "LifecycleStorageTierTransition",
    "ListMultipartUploads",
    "UploadPart",
    "UploadPartCopy",
    "ListParts",
    "PutBucketEncryption",
    "PutBucketCors",
    "PutBucketLifecycleConfiguration",
];

/// R2's Class B operations, likewise.
const R2_CLASS_B: &[&str] = &[
    "HeadBucket",
    "HeadObject",
    "GetObject",
    "UsageSummary",
    "GetBucketEncryption",
    "GetBucketLocation",
    "GetBucketCors",
    "GetBucketLifecycleConfiguration",
];

fn source(meter: Meter) -> Source {
    let of = |dataset, field, resource| Source {
        dataset,
        field,
        resource,
        actions: None,
    };
    match meter {
        Meter::R2ClassA => Source {
            actions: Some(R2_CLASS_A),
            ..of("r2_operations", "requests", "bucketName")
        },
        Meter::R2ClassB => Source {
            actions: Some(R2_CLASS_B),
            ..of("r2_operations", "requests", "bucketName")
        },
        Meter::R2Storage => of("r2_storage", "payloadSize", "bucketName"),
        Meter::WorkersRequests => of("workers", "requests", "scriptName"),
        Meter::D1RowsRead => of("d1", "rowsRead", "databaseId"),
        Meter::D1RowsWritten => of("d1", "rowsWritten", "databaseId"),
        Meter::D1Storage => of("d1_storage", "databaseSizeBytes", "databaseId"),
        Meter::DurableObjectRequests => of("durable_object_invocations", "requests", "namespaceId"),
        Meter::DurableObjectDuration => of("durable_object_periodic", "activeTime", "namespaceId"),
        Meter::DurableObjectRowsRead => of("durable_object_periodic", "rowsRead", "namespaceId"),
        Meter::DurableObjectRowsWritten => {
            of("durable_object_periodic", "rowsWritten", "namespaceId")
        }
        Meter::DurableObjectStorage => of("durable_object_storage", "storedBytes", "namespaceId"),
    }
}

/// The datasets that cover `meters`, each once.
pub fn datasets_for(meters: &BTreeSet<Meter>) -> Vec<&'static Dataset> {
    let keys: BTreeSet<&str> = meters.iter().map(|m| source(*m).dataset).collect();
    DATASETS
        .iter()
        .filter(|dataset| keys.contains(dataset.key))
        .collect()
}

/// The GraphQL request for one dataset over the days `[start, end)`, as
/// the JSON body to POST.
pub fn request_body(
    dataset: &Dataset,
    account_tag: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> String {
    let (types, filter, variables) = match dataset.filter {
        TimeFilter::Datetime => (
            "$start: Time!, $end: Time!",
            "datetime_geq: $start, datetime_lt: $end",
            serde_json::json!({
                "accountTag": account_tag,
                "start": format!("{start}T00:00:00Z"),
                "end": format!("{end}T00:00:00Z"),
            }),
        ),
        TimeFilter::Date => (
            "$start: Date!, $end: Date!",
            "date_geq: $start, date_leq: $end",
            serde_json::json!({
                "accountTag": account_tag,
                "start": start.to_string(),
                // `date_leq` is inclusive: the last day, not the next.
                "end": end.pred_opt().unwrap_or(end).to_string(),
            }),
        ),
    };
    let query = format!(
        "query CloudBridgeUsage($accountTag: string!, {types}) {{ viewer {{ \
         accounts(filter: {{accountTag: $accountTag}}) {{ \
         {name}(limit: 10000, filter: {{{filter}}}) {{ \
         {aggregate} {{ {fields} }} dimensions {{ {dimensions} }} }} }} }} }}",
        name = dataset.name,
        aggregate = dataset.aggregate,
        fields = dataset.fields.join(" "),
        dimensions = dataset.dimensions.join(" "),
    );
    serde_json::json!({ "query": query, "variables": variables }).to_string()
}

/// A resource's usage of one meter on one day.
#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    /// The dimension's value: a bucket name, script name, database or
    /// namespace id.
    pub key: String,
    pub amount: f64,
}

/// Usage by meter and day, each resource's share of it.
pub type Usage = HashMap<(Meter, NaiveDate), Vec<Share>>;

/// Read the analytics payloads of a batch. A payload that carries errors —
/// a field the account's plan lacks, a token without Analytics · Read —
/// contributes nothing, and its meters stay unsplit.
pub fn usage_from(parts: &[RawPart]) -> Usage {
    let mut totals: BTreeMap<(Meter, NaiveDate, String), f64> = BTreeMap::new();
    for part in parts {
        let Some(key) = part.name.strip_prefix(PART_ANALYTICS_PREFIX) else {
            continue;
        };
        let Some(dataset) = DATASETS.iter().find(|d| d.key == key) else {
            continue;
        };
        let Ok(response) = serde_json::from_str::<Value>(&part.body) else {
            continue;
        };
        if response
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
        {
            tracing::warn!(
                "Cloudflare analytics {} returned errors; its usage stays unsplit: {}",
                dataset.name,
                response["errors"]
            );
            continue;
        }
        let rows = response
            .pointer("/data/viewer/accounts/0")
            .and_then(|account| account.get(dataset.name))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let meters = [
            Meter::R2ClassA,
            Meter::R2ClassB,
            Meter::R2Storage,
            Meter::WorkersRequests,
            Meter::D1RowsRead,
            Meter::D1RowsWritten,
            Meter::D1Storage,
            Meter::DurableObjectRequests,
            Meter::DurableObjectDuration,
            Meter::DurableObjectRowsRead,
            Meter::DurableObjectRowsWritten,
            Meter::DurableObjectStorage,
        ];
        for meter in meters.into_iter().filter(|m| source(*m).dataset == key) {
            let from = source(meter);
            for row in &rows {
                let dimensions = &row["dimensions"];
                let Some(day) = dimensions["date"]
                    .as_str()
                    .and_then(|d| d.parse::<NaiveDate>().ok())
                else {
                    continue;
                };
                let Some(resource) = dimensions[from.resource].as_str().filter(|r| !r.is_empty())
                else {
                    continue;
                };
                if let Some(actions) = from.actions {
                    let action = dimensions["actionType"].as_str().unwrap_or_default();
                    if !actions.contains(&action) {
                        continue;
                    }
                }
                let amount = row[dataset.aggregate][from.field].as_f64().unwrap_or(0.0);
                *totals
                    .entry((meter, day, resource.to_string()))
                    .or_insert(0.0) += amount;
            }
        }
    }

    let mut usage: Usage = HashMap::new();
    for ((meter, day, key), amount) in totals {
        if amount > 0.0 {
            usage
                .entry((meter, day))
                .or_default()
                .push(Share { key, amount });
        }
    }
    usage
}

/// The resource a share is filed under, as the scanner names it, so a
/// split row and the inventory meet: an R2 bucket is
/// `<account>/r2/<bucket>`; a Worker its script name; a database or
/// namespace its id.
fn resource_of(meter: Meter, key: &str, account: Option<&str>) -> (String, Option<String>) {
    match meter {
        Meter::R2ClassA | Meter::R2ClassB | Meter::R2Storage => (
            match account {
                Some(account) => format!("{account}/r2/{key}"),
                None => key.to_string(),
            },
            Some(key.to_string()),
        ),
        Meter::WorkersRequests => (key.to_string(), Some(key.to_string())),
        _ => (key.to_string(), None),
    }
}

/// Split each bill row across the resources its meter's usage that day
/// came from. A row with no meter, no usage to go by, or already filed
/// against a resource (a zone) is kept as it is.
pub fn allocate(charges: Vec<Charge>, usage: &Usage) -> Vec<Charge> {
    let mut out = Vec::with_capacity(charges.len());
    for charge in charges {
        let meter = meter_of(
            charge.x_service_code.as_deref(),
            charge.service_name.as_deref(),
        );
        let shares = meter
            .filter(|_| charge.resource_id.is_none())
            .and_then(|m| usage.get(&(m, charge.charge_period_start.date_naive())));
        let (Some(meter), Some(shares)) = (meter, shares) else {
            out.push(charge);
            continue;
        };
        let total: f64 = shares.iter().map(|s| s.amount).sum();
        if total <= 0.0 {
            out.push(charge);
            continue;
        }
        for share in shares {
            let part = share.amount / total;
            let scale = |value: Option<f64>| value.map(|v| v * part);
            let (resource_id, resource_name) =
                resource_of(meter, &share.key, charge.billing_account_id.as_deref());
            out.push(Charge {
                resource_id: Some(resource_id),
                resource_name,
                billed_cost: scale(charge.billed_cost),
                effective_cost: scale(charge.effective_cost),
                list_cost: scale(charge.list_cost),
                pricing_quantity: scale(charge.pricing_quantity),
                cost_basis: CostBasis::Estimated,
                ..charge.clone()
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn at(day: &str) -> DateTime<Utc> {
        format!("{day}T00:00:00Z").parse().unwrap()
    }

    fn bill_row(service: &str, family: &str, day: &str, cost: f64, quantity: f64) -> Charge {
        Charge {
            service_name: Some(service.to_string()),
            x_service_code: Some(family.to_string()),
            billing_account_id: Some("af07b1637f2471f58ac47721c5c9c8cb".to_string()),
            billed_cost: Some(cost),
            effective_cost: Some(cost),
            pricing_quantity: Some(quantity),
            ..Charge::new(at(day), at(day), "USD")
        }
    }

    fn part(key: &str, body: &str) -> RawPart {
        RawPart::new(format!("{PART_ANALYTICS_PREFIX}{key}"), "", body)
    }

    #[test]
    fn a_bill_s_service_names_are_read_as_meters() {
        let r2 = |name| meter_of(Some("R2"), Some(name));
        assert_eq!(
            r2("R2 Storage Class A Operations (First 1M included)"),
            Some(Meter::R2ClassA)
        );
        assert_eq!(
            r2("R2 Storage Class B Operations (First 10M included)"),
            Some(Meter::R2ClassB)
        );
        assert_eq!(
            r2("R2 Data Storage (First 10GB-Month included)"),
            Some(Meter::R2Storage)
        );
        assert_eq!(
            meter_of(
                Some("Durable Objects"),
                Some("Durable Objects Rows Written")
            ),
            Some(Meter::DurableObjectRowsWritten)
        );
        assert_eq!(
            meter_of(Some("Workers"), Some("Workers Standard CPU ms")),
            Some(Meter::WorkersRequests)
        );
        assert_eq!(meter_of(Some("Workers"), Some("Workers KV Reads")), None);
        assert_eq!(meter_of(Some("Argo"), Some("Argo Smart Routing")), None);
    }

    /// Only the datasets the bill's meters need are asked for, each once.
    #[test]
    fn a_bill_asks_for_the_datasets_its_meters_need() {
        let meters = BTreeSet::from([Meter::R2ClassA, Meter::R2ClassB, Meter::R2Storage]);
        let keys: Vec<_> = datasets_for(&meters).iter().map(|d| d.key).collect();
        assert_eq!(keys, ["r2_operations", "r2_storage"]);
    }

    #[test]
    fn a_request_names_its_dataset_days_and_account() {
        let r2 = DATASETS.iter().find(|d| d.key == "r2_operations").unwrap();
        let body: Value = serde_json::from_str(&request_body(
            r2,
            "acct",
            "2026-10-01".parse().unwrap(),
            "2026-10-09".parse().unwrap(),
        ))
        .unwrap();
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("r2OperationsAdaptiveGroups(limit: 10000, filter: {datetime_geq: $start, datetime_lt: $end})"), "{query}");
        assert!(
            query.contains("sum { requests } dimensions { date bucketName actionType }"),
            "{query}"
        );
        assert_eq!(body["variables"]["end"], "2026-10-09T00:00:00Z");

        let d1 = DATASETS.iter().find(|d| d.key == "d1").unwrap();
        let body: Value = serde_json::from_str(&request_body(
            d1,
            "acct",
            "2026-10-01".parse().unwrap(),
            "2026-10-09".parse().unwrap(),
        ))
        .unwrap();
        assert_eq!(
            body["variables"]["end"], "2026-10-08",
            "date_leq is inclusive"
        );
    }

    const R2_OPERATIONS: &str = r#"{"data":{"viewer":{"accounts":[{"r2OperationsAdaptiveGroups":[
        {"sum":{"requests":600},"dimensions":{"date":"2026-10-03","bucketName":"aiops","actionType":"GetObject"}},
        {"sum":{"requests":134},"dimensions":{"date":"2026-10-03","bucketName":"assets","actionType":"HeadObject"}},
        {"sum":{"requests":9},"dimensions":{"date":"2026-10-03","bucketName":"assets","actionType":"PutObject"}},
        {"sum":{"requests":40},"dimensions":{"date":"2026-10-03","bucketName":"aiops","actionType":"DeleteObject"}}
    ]}]}},"errors":null}"#;

    /// Class B is GetObject and HeadObject: the bucket that served the
    /// most of them carries the most of the day's Class B row, and the two
    /// splits add up to the row.
    #[test]
    fn a_day_s_row_is_split_by_each_resource_s_share() {
        let usage = usage_from(&[part("r2_operations", R2_OPERATIONS)]);
        let rows = allocate(
            vec![bill_row(
                "R2 Storage Class B Operations (First 10M included)",
                "R2",
                "2026-10-03",
                0.36,
                734.0,
            )],
            &usage,
        );
        let split: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.resource_id.clone().unwrap(),
                    (r.pricing_quantity.unwrap() * 1e6).round() / 1e6,
                    r.cost_basis,
                )
            })
            .collect();
        assert_eq!(
            split,
            [
                (
                    "af07b1637f2471f58ac47721c5c9c8cb/r2/aiops".to_string(),
                    600.0,
                    CostBasis::Estimated
                ),
                (
                    "af07b1637f2471f58ac47721c5c9c8cb/r2/assets".to_string(),
                    134.0,
                    CostBasis::Estimated
                ),
            ]
        );
        let cost: f64 = rows.iter().filter_map(|r| r.billed_cost).sum();
        assert!((cost - 0.36).abs() < 1e-12);
        assert_eq!(rows[0].resource_name.as_deref(), Some("aiops"));
    }

    /// Class A takes PutObject only; a free DeleteObject counts for neither.
    #[test]
    fn an_operation_counts_for_its_own_class_only() {
        let usage = usage_from(&[part("r2_operations", R2_OPERATIONS)]);
        let day = "2026-10-03".parse().unwrap();
        assert_eq!(
            usage[&(Meter::R2ClassA, day)],
            [Share {
                key: "assets".to_string(),
                amount: 9.0
            }]
        );
    }

    /// A day the analytics say nothing about, a meter they cannot split,
    /// a payload with errors: each leaves the row whole.
    #[test]
    fn a_row_without_usage_to_go_by_is_kept_whole() {
        let refused = part(
            "r2_operations",
            r#"{"data":null,"errors":[{"message":"not authorized for that account"}]}"#,
        );
        let usage = usage_from(&[refused]);
        let rows = allocate(
            vec![
                bill_row(
                    "R2 Storage Class B Operations",
                    "R2",
                    "2026-10-03",
                    0.0,
                    734.0,
                ),
                bill_row("Argo Smart Routing", "Argo", "2026-10-03", 0.75, 7.5),
            ],
            &usage,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.resource_id.is_none()));
        assert!(rows
            .iter()
            .all(|r| r.cost_basis == CostBasis::Authoritative));
    }

    /// The tweet that started this: a namespace looping on alarms writes
    /// trillions of rows, and the split says which.
    #[test]
    fn a_durable_object_namespace_carries_its_own_rows_written() {
        let periodic = part(
            "durable_object_periodic",
            r#"{"data":{"viewer":{"accounts":[{"durableObjectsPeriodicGroups":[
                {"sum":{"activeTime":10,"rowsRead":5,"rowsWritten":2400000000},"dimensions":{"date":"2026-10-02","namespaceId":"ns-ticker"}},
                {"sum":{"activeTime":90,"rowsRead":50,"rowsWritten":10500000},"dimensions":{"date":"2026-10-02","namespaceId":"ns-chat"}}
            ]}]}}}"#,
        );
        let rows = allocate(
            vec![bill_row(
                "Durable Objects Rows Written",
                "Durable Objects",
                "2026-10-02",
                2410.5,
                2_410_500_000.0,
            )],
            &usage_from(&[periodic]),
        );
        let ticker = rows
            .iter()
            .find(|r| r.resource_id.as_deref() == Some("ns-ticker"))
            .unwrap();
        assert!(ticker.billed_cost.unwrap() > 2399.0);
    }

    /// A zone-level row is already a resource's; splitting it again would
    /// file it twice.
    #[test]
    fn a_row_already_filed_against_a_resource_is_not_split() {
        let mut zoned = bill_row(
            "R2 Storage Class B Operations",
            "R2",
            "2026-10-03",
            0.1,
            10.0,
        );
        zoned.resource_id = Some("zone-1".to_string());
        let rows = allocate(
            vec![zoned],
            &usage_from(&[part("r2_operations", R2_OPERATIONS)]),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource_id.as_deref(), Some("zone-1"));
    }
}
