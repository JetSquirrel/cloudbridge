//! AWS Cloud Service Implementation - Using ureq + AWS Signature V4

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::raw::RawPart;
use super::s3::{S3Client, S3Uri};
use super::{aws_focus, BillingPeriod, BillingSource, Fetched, Normalized, PayloadFile, RawBatch};
use crate::ledger::{Charge, ChargeCategory};

type HmacSha256 = Hmac<Sha256>;

/// Name the S3 object listing is stored under in a raw batch, when the
/// account is backed by a Data Exports S3 URI.
const PART_EXPORT_LISTING: &str = "export_listing";
const PART_EXPORT_MANIFEST: &str = "export_manifest";

/// AWS Cloud Service
pub struct AwsCloudService {
    access_key_id: String,
    secret_access_key: String,
    region: String,
    /// `s3://bucket/prefix` of this account's Data Exports (FOCUS) export;
    /// when set, the export is the bill and Cost Explorer is not called.
    export_uri: Option<String>,
}

/// One request to sign: everything SigV4 needs beyond the service's own
/// credentials. `region` is the account's own, except for the services —
/// Cost Explorer — whose endpoint exists in us-east-1 alone.
struct SigningRequest<'a> {
    method: &'a str,
    service: &'a str,
    region: &'a str,
    host: &'a str,
    uri: &'a str,
    query_string: &'a str,
    headers: &'a [(String, String)],
    payload: &'a str,
    timestamp: DateTime<Utc>,
}

impl AwsCloudService {
    pub fn new(
        access_key_id: String,
        secret_access_key: String,
        region: Option<String>,
        export_uri: Option<String>,
    ) -> Self {
        Self {
            access_key_id,
            secret_access_key,
            region: region.unwrap_or_else(|| "us-east-1".to_string()),
            export_uri: export_uri.filter(|uri| !uri.trim().is_empty()),
        }
    }

    fn s3_client(&self) -> S3Client {
        S3Client::new(
            self.access_key_id.clone(),
            self.secret_access_key.clone(),
            Some(self.region.clone()),
        )
    }

    /// Find and download the export objects for one billing period.
    ///
    /// An export partitions by `BILLING_PERIOD=<label>` below a `data/`
    /// directory — upper case for CUR 2.0, `billing_period=` for FOCUS —
    /// and the URI a user copies may point anywhere above it, so the
    /// candidates run from the most to the least specific. S3 prefixes are
    /// case-sensitive, so each spelling is its own candidate. The data is
    /// Parquet or gzipped CSV, as the export was configured.
    ///
    /// What is read is what the period's manifest names, not everything
    /// under the partition: an export set to "create new" keeps every
    /// refresh in its own `<timestamp>-<execution-id>/` folder, and an
    /// "overwrite" one can leave emptied chunks behind, so the listing
    /// alone would count a month several times over. AWS writes the
    /// manifest only once a delivery is complete.
    ///
    /// A period the export has not produced yet — no data, or no manifest
    /// for it yet — is [`aws_focus::ExportNotReady`] rather than an empty
    /// batch: writing nothing here must never replace a month's ledger rows
    /// with zero rows.
    ///
    /// `previous_listing` is the listing part of the period's last complete
    /// fetch. When the export still names exactly those files — same keys,
    /// sizes and ETags — nothing is downloaded and the result is `None`: a
    /// month AWS has finished delivering would otherwise be bought again,
    /// in S3 requests and transfer, every refresh.
    fn fetch_focus_export(
        &self,
        period: &BillingPeriod,
        previous_listing: Option<&str>,
    ) -> Result<Option<Fetched>> {
        let uri = S3Uri::parse(self.export_uri.as_deref().unwrap_or_default())?;
        let client = self.s3_client();
        let label = period.label();

        let mut objects = Vec::new();
        for prefix in &export_prefixes(&uri.prefix, &label) {
            let found: Vec<_> = client
                .list_objects(&uri.bucket, prefix)?
                .into_iter()
                .filter(|object| is_period_data(&object.key, &label))
                .collect();
            if !found.is_empty() {
                objects = found;
                break;
            }
        }
        let not_ready = || -> anyhow::Error {
            aws_focus::ExportNotReady {
                uri: self.export_uri.clone().unwrap_or_default(),
                period: label.clone(),
            }
            .into()
        };
        if objects.is_empty() {
            return Err(not_ready());
        }

        let manifest_key = manifest_key(&objects[0].key, &label).ok_or_else(|| {
            anyhow!(
                "{} is not under an export's data/ directory, so its manifest cannot be found",
                objects[0].key
            )
        })?;
        let Some(manifest) = client.get_object_opt(&uri.bucket, &manifest_key)? else {
            return Err(not_ready());
        };
        let manifest = String::from_utf8(manifest)
            .map_err(|e| anyhow!("Manifest {} is not UTF-8: {}", manifest_key, e))?;
        let named = manifest_data_keys(&manifest, &uri.bucket)?;
        let objects = files_named_by_manifest(objects, &named).map_err(|missing| {
            anyhow!(
                "Manifest {} names {} file(s) that are not in the bucket, e.g. {}",
                manifest_key,
                missing.len(),
                missing[0]
            )
        })?;

        // The listing rides as the metadata part: which keys, sizes and
        // ETags the batch was built from, for reproducing the fetch later.
        let listing: Vec<serde_json::Value> = objects
            .iter()
            .map(|object| {
                serde_json::json!({
                    "key": object.key,
                    "size": object.size,
                    "etag": object.etag,
                })
            })
            .collect();
        let listing_json = serde_json::to_string_pretty(&listing)?;
        if previous_listing.is_some_and(|previous| same_listing(previous, &listing_json)) {
            return Ok(None);
        }

        let mut payload_files = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            let bytes = client.get_object(&uri.bucket, &object.key)?;
            let format = aws_focus::ExportFormat::of_key(&object.key).expect("filtered above");
            payload_files.push(PayloadFile {
                name: format!("focus-{}{}", index, format.payload_suffix(&object.key)),
                bytes,
            });
        }

        Ok(Some(Fetched {
            parts: vec![
                RawPart::new(
                    PART_EXPORT_LISTING,
                    format!("s3://{}/{}", uri.bucket, uri.prefix),
                    listing_json,
                ),
                RawPart::new(
                    PART_EXPORT_MANIFEST,
                    format!("s3://{}/{}", uri.bucket, manifest_key),
                    manifest,
                ),
            ],
            payload_files,
        }))
    }

    /// Calculate SHA256 hash
    fn sha256_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Calculate HMAC-SHA256
    fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// Create AWS Signature V4 signature
    fn sign_request(&self, request: &SigningRequest<'_>) -> Result<String> {
        let amz_date = request.timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = request.timestamp.format("%Y%m%d").to_string();

        // 1. Create canonical request
        let payload_hash = Self::sha256_hash(request.payload.as_bytes());

        // Collect all headers (including host and x-amz-date)
        let mut all_headers: Vec<(String, String)> = request.headers.to_vec();
        all_headers.push(("host".to_string(), request.host.to_string()));
        all_headers.push(("x-amz-date".to_string(), amz_date.clone()));
        all_headers.push(("x-amz-content-sha256".to_string(), payload_hash.clone()));

        // Sort by lowercase key
        all_headers.sort_by_key(|(name, _)| name.to_lowercase());

        let canonical_headers: String = all_headers
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k.to_lowercase(), v.trim()))
            .collect();

        let signed_headers: String = all_headers
            .iter()
            .map(|(k, _)| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            request.method,
            request.uri,
            request.query_string,
            canonical_headers,
            signed_headers,
            payload_hash
        );

        // 2. Create string to sign
        let credential_scope = format!(
            "{}/{}/{}/aws4_request",
            date_stamp, request.region, request.service
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            Self::sha256_hash(canonical_request.as_bytes())
        );

        // 3. Calculate signature
        let k_date = Self::hmac_sha256(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = Self::hmac_sha256(&k_date, request.region.as_bytes());
        let k_service = Self::hmac_sha256(&k_region, request.service.as_bytes());
        let k_signing = Self::hmac_sha256(&k_service, b"aws4_request");
        let signature = hex::encode(Self::hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        // 4. Create authorization header
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key_id, credential_scope, signed_headers, signature
        );

        Ok(authorization)
    }

    /// Call STS GetCallerIdentity API
    fn call_sts_get_caller_identity(&self) -> Result<StsCallerIdentity> {
        let timestamp = Utc::now();
        let host = format!("sts.{}.amazonaws.com", self.region);
        let uri = "/";
        let query_string = "Action=GetCallerIdentity&Version=2011-06-15";

        let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let payload_hash = Self::sha256_hash(b"");

        let authorization = self.sign_request(&SigningRequest {
            method: "GET",
            service: "sts",
            region: &self.region,
            host: &host,
            uri,
            query_string,
            headers: &[],
            payload: "",
            timestamp,
        })?;

        let url = format!("https://{}{}?{}", host, uri, query_string);

        // ureq waits forever by default; a stalled STS would leave the
        // credential test spinning with nothing to report.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .into();

        let response = agent
            .get(&url)
            .header("Authorization", &authorization)
            .header("X-Amz-Date", &amz_date)
            .header("X-Amz-Content-Sha256", &payload_hash)
            .header("Host", &host)
            .call()
            .map_err(|e| anyhow!("STS request failed: {}", e))?;

        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;

        // Parse XML response
        parse_sts_response(&body)
    }

    /// Ask Cost Explorer for one time range and return the response body
    /// unchanged.
    ///
    /// The only place in this file that talks to Cost Explorer. Each call
    /// is billed, so callers ask for everything they need in one request.
    ///
    /// Note: the Cost Explorer endpoint only exists in us-east-1.
    fn cost_and_usage_raw(&self, request: &serde_json::Value) -> Result<String> {
        let timestamp = Utc::now();
        let ce_region = "us-east-1";
        let host = format!("ce.{}.amazonaws.com", ce_region);
        let uri = "/";

        let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
        let payload = serde_json::to_string(request)?;
        let payload_hash = Self::sha256_hash(payload.as_bytes());

        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-amz-json-1.1".to_string(),
            ),
            (
                "x-amz-target".to_string(),
                "AWSInsightsIndexService.GetCostAndUsage".to_string(),
            ),
        ];

        let authorization = self.sign_request(&SigningRequest {
            method: "POST",
            service: "ce",
            region: ce_region,
            host: &host,
            uri,
            query_string: "",
            headers: &headers,
            payload: &payload,
            timestamp,
        })?;

        let url = format!("https://{}{}", host, uri);

        // Do not treat a 4xx/5xx as a transport error, so the response body
        // makes it into the log — Cost Explorer explains itself there.
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .new_agent();

        tracing::debug!("Sending Cost Explorer request: {}", url);

        let response = agent
            .post(&url)
            .header("Authorization", &authorization)
            .header("X-Amz-Date", &amz_date)
            .header("X-Amz-Content-Sha256", &payload_hash)
            .header("Host", &host)
            .header("Content-Type", "application/x-amz-json-1.1")
            .header("X-Amz-Target", "AWSInsightsIndexService.GetCostAndUsage")
            .send(&payload)
            .map_err(|e| {
                tracing::error!("Cost Explorer request error details: {:?}", e);
                anyhow!("Cost Explorer request failed: {}", e)
            })?;

        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;

        if status >= 400 {
            tracing::error!("Cost Explorer error response (HTTP {}): {}", status, body);
            return Err(cost_explorer_error(status, &body));
        }

        Ok(body)
    }
}

/// Name the Cost Explorer payload is stored under in a raw batch.
const PART_COST_AND_USAGE: &str = "cost_and_usage";

/// Name of the n-th page after the first (counting from 2), when Cost
/// Explorer splits a response. The first page keeps the bare name, so a
/// batch recorded before pages were followed replays unchanged.
fn page_part_name(page: usize) -> String {
    format!("{PART_COST_AND_USAGE}.page{page}")
}

/// Marks a batch whose pages ran past [`MAX_COST_EXPLORER_PAGES`]: what
/// was fetched is kept, but it is not a whole month and is not recorded.
const PART_PAGES_TRUNCATED: &str = "cost_and_usage.truncated";

/// The most pages one period's request is followed for. Each page is a
/// billed request; a month grouped by service and record type fits in a
/// handful, so a response that keeps paging past this is not one to keep
/// paying for.
const MAX_COST_EXPLORER_PAGES: usize = 20;

/// What was actually charged.
const METRIC_UNBLENDED: &str = "UnblendedCost";
/// The same spend with commitment fees spread over the term they cover.
const METRIC_AMORTIZED: &str = "AmortizedCost";
/// How much was consumed, when the grouping leaves one meaningful unit.
const METRIC_USAGE_QUANTITY: &str = "UsageQuantity";

/// Cost Explorer returns this unit when a group mixes usage types, which
/// grouping by service usually does. A quantity in mixed units cannot be
/// added to anything, so it is not stored.
const UNIT_NOT_APPLICABLE: &str = "N/A";

const DIMENSION_SERVICE: &str = "SERVICE";
const DIMENSION_RECORD_TYPE: &str = "RECORD_TYPE";

/// The prefixes to list for one period's export objects, most specific
/// first, ending with the URI's own prefix for the layouts in between.
fn export_prefixes(prefix: &str, label: &str) -> Vec<String> {
    let mut prefixes: Vec<String> = [
        format!("BILLING_PERIOD={label}"),
        format!("billing_period={label}"),
    ]
    .iter()
    .flat_map(|marker| {
        [
            format!("{prefix}/data/{marker}/"),
            format!("{prefix}/{marker}/"),
        ]
    })
    .collect();
    prefixes.push(prefix.to_string());
    prefixes
}

/// Whether an object is one of the period's data files, as opposed to
/// another period's, or a manifest in the same partition.
fn is_period_data(key: &str, label: &str) -> bool {
    key.to_ascii_lowercase()
        .contains(&format!("billing_period={label}"))
        && aws_focus::ExportFormat::of_key(key).is_some()
}

/// Whether two export listings name the same files, byte for byte as far
/// as S3 can say: the same keys with the same sizes and ETags. Compared as
/// JSON, so a change in how a listing is printed is not a change in data;
/// a listing that does not parse matches nothing.
fn same_listing(previous: &str, current: &str) -> bool {
    match (
        serde_json::from_str::<serde_json::Value>(previous),
        serde_json::from_str::<serde_json::Value>(current),
    ) {
        (Ok(previous), Ok(current)) => previous == current,
        _ => false,
    }
}

/// Where the manifest of the delivery `data_key` belongs to lives:
/// `<root>/<export-name>/data/<partition>/[<run>/]<file>` has its latest
/// manifest at `<root>/<export-name>/metadata/<partition>/<export-name>-Manifest.json`.
/// `None` for a key that is not under a `data/<partition>/` directory.
fn manifest_key(data_key: &str, label: &str) -> Option<String> {
    let marker = format!("billing_period={label}");
    let segments: Vec<&str> = data_key.split('/').collect();
    let partition = segments
        .iter()
        .position(|segment| segment.to_ascii_lowercase() == marker)?;
    if partition < 2 || segments[partition - 1] != "data" {
        return None;
    }
    let export_name = segments[partition - 2];
    let root = segments[..partition - 1].join("/");
    Some(format!(
        "{root}/metadata/{}/{export_name}-Manifest.json",
        segments[partition]
    ))
}

/// The object keys a manifest's `dataFiles` names in `bucket`.
fn manifest_data_keys(manifest: &str, bucket: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        #[serde(rename = "dataFiles")]
        data_files: Vec<String>,
    }
    let manifest: Manifest = serde_json::from_str(manifest)
        .map_err(|e| anyhow!("Export manifest has no readable dataFiles list: {}", e))?;
    let prefix = format!("s3://{bucket}/");
    manifest
        .data_files
        .iter()
        .map(|uri| {
            uri.strip_prefix(&prefix)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("Export manifest names a file outside {}: {}", bucket, uri))
        })
        .collect()
}

/// The listed objects the manifest names, in the manifest's order. `Err`
/// carries the named keys that were not listed: a manifest describes a
/// complete delivery, so a missing file is not one to read around.
fn files_named_by_manifest(
    listed: Vec<crate::cloud::s3::S3Object>,
    named: &[String],
) -> std::result::Result<Vec<crate::cloud::s3::S3Object>, Vec<String>> {
    let mut by_key: std::collections::HashMap<String, crate::cloud::s3::S3Object> = listed
        .into_iter()
        .map(|object| (object.key.clone(), object))
        .collect();
    let mut kept = Vec::with_capacity(named.len());
    let mut missing = Vec::new();
    for key in named {
        match by_key.remove(key) {
            Some(object) => kept.push(object),
            None => missing.push(key.clone()),
        }
    }
    if missing.is_empty() {
        Ok(kept)
    } else {
        Err(missing)
    }
}

/// The GetCostAndUsage request the ledger is built from.
///
/// All three metrics ride in one request: Cost Explorer bills per request,
/// not per metric, so splitting them would triple the cost of an ingest
/// for nothing. `RECORD_TYPE` is what makes a credit distinguishable from
/// a charge — without it every line arrives as an unlabelled amount.
fn ledger_request(start_date: &str, end_date: &str) -> serde_json::Value {
    serde_json::json!({
        "TimePeriod": {
            "Start": start_date,
            "End": end_date
        },
        "Granularity": "DAILY",
        "Metrics": [METRIC_UNBLENDED, METRIC_AMORTIZED, METRIC_USAGE_QUANTITY],
        "GroupBy": [
            {
                "Type": "DIMENSION",
                "Key": DIMENSION_SERVICE
            },
            {
                "Type": "DIMENSION",
                "Key": DIMENSION_RECORD_TYPE
            }
        ]
    })
}

/// FOCUS category for an AWS `RECORD_TYPE`.
///
/// Discounts and negations are `Adjustment` rather than `Credit`: they
/// reduce what a charge costs, whereas AWS's own `Credit` record type is a
/// balance applied against the bill. An unrecognized type is also
/// `Adjustment`, and says so in the log — money moved, and filing it as
/// `Usage` would quietly inflate what looks like consumption.
fn charge_category(record_type: &str) -> ChargeCategory {
    match record_type {
        "Usage" | "DiscountedUsage" | "SavingsPlanCoveredUsage" => ChargeCategory::Usage,
        "Credit" => ChargeCategory::Credit,
        "Tax" => ChargeCategory::Tax,
        "Fee" | "RIFee" | "SavingsPlanUpfrontFee" | "SavingsPlanRecurringFee" | "Support" => {
            ChargeCategory::Purchase
        }
        "Refund"
        | "SavingsPlanNegation"
        | "BundledDiscount"
        | "PrivateRateDiscount"
        | "Enterprise Discount Program Discount"
        | "Solution Provider Program Discount" => ChargeCategory::Adjustment,
        other => {
            tracing::warn!(
                "Unrecognized Cost Explorer record type {:?}; filed as an Adjustment",
                other
            );
            ChargeCategory::Adjustment
        }
    }
}

/// Turn a fetched Cost Explorer payload into ledger rows.
///
/// Pure — every input is in `batch`.
///
/// `UnblendedCost` is what was actually charged, so it is `billed_cost`
/// and `cost_basis` is `authoritative`; `AmortizedCost` spreads commitment
/// fees over the term they cover, which is `effective_cost`. Amounts keep
/// the sign Cost Explorer gave them, so a credit stays negative and a
/// total comes out right by summation alone.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    #[derive(Deserialize)]
    struct CeResponse {
        #[serde(rename = "GroupDefinitions")]
        group_definitions: Option<Vec<GroupDefinition>>,
        #[serde(rename = "ResultsByTime")]
        results_by_time: Option<Vec<TimeResult>>,
    }

    #[derive(Deserialize)]
    struct GroupDefinition {
        #[serde(rename = "Key")]
        key: String,
    }

    #[derive(Deserialize)]
    struct TimeResult {
        #[serde(rename = "TimePeriod")]
        time_period: TimePeriod,
        #[serde(rename = "Groups")]
        groups: Option<Vec<CostGroup>>,
    }

    #[derive(Deserialize)]
    struct TimePeriod {
        #[serde(rename = "Start")]
        start: String,
        #[serde(rename = "End")]
        end: String,
    }

    #[derive(Deserialize)]
    struct CostGroup {
        #[serde(rename = "Keys")]
        keys: Vec<String>,
        #[serde(rename = "Metrics")]
        metrics: std::collections::HashMap<String, CostAmount>,
    }

    #[derive(Deserialize)]
    struct CostAmount {
        #[serde(rename = "Amount")]
        amount: String,
        #[serde(rename = "Unit")]
        unit: String,
    }

    impl CostAmount {
        fn value(&self) -> f64 {
            self.amount.parse().unwrap_or(0.0)
        }
    }

    if batch.part(PART_PAGES_TRUNCATED).is_some() {
        return Err(anyhow!(
            "Cost Explorer returned more than {MAX_COST_EXPLORER_PAGES} pages for one month; \
             the pages fetched are kept on disk, but not recorded as the whole month"
        ));
    }
    let first = batch
        .part(PART_COST_AND_USAGE)
        .ok_or_else(|| anyhow!("Raw batch has no '{}' payload", PART_COST_AND_USAGE))?;
    // The first page, then the rest in the order they were fetched.
    let later_pages = batch.parts.iter().filter(|part| {
        part.name
            .starts_with(&format!("{PART_COST_AND_USAGE}.page"))
    });

    let mut charges = Vec::new();
    for part in std::iter::once(first).chain(later_pages) {
        let response: CeResponse = serde_json::from_str(&part.body)
            .map_err(|e| anyhow!("Failed to parse Cost Explorer payload: {}", e))?;

        // Which key is which comes from the response itself rather than from
        // the request this build would have sent, so a payload recorded by an
        // older version still normalizes.
        let definitions = response.group_definitions.unwrap_or_default();
        let position = |dimension: &str| definitions.iter().position(|d| d.key == dimension);
        let service_at = position(DIMENSION_SERVICE).unwrap_or(0);
        let record_type_at = position(DIMENSION_RECORD_TYPE);

        for result in response.results_by_time.unwrap_or_default() {
            let start = parse_day(&result.time_period.start)?;
            let end = parse_day(&result.time_period.end)?;

            for group in result.groups.unwrap_or_default() {
                let unblended = group.metrics.get(METRIC_UNBLENDED);
                let amortized = group.metrics.get(METRIC_AMORTIZED);
                let billed_cost = unblended.map(CostAmount::value);
                let effective_cost = amortized.map(CostAmount::value);

                // Cost Explorer returns a row for every service in the account,
                // most of them zero on every metric. They carry no information
                // and would bloat the fact table by an order of magnitude. A
                // row that is zero unblended but non-zero amortized — usage a
                // commitment already paid for — is not one of them.
                if billed_cost.unwrap_or(0.0) == 0.0 && effective_cost.unwrap_or(0.0) == 0.0 {
                    continue;
                }

                // A quantity is only kept when the group leaves it in one unit.
                let quantity = group
                    .metrics
                    .get(METRIC_USAGE_QUANTITY)
                    .filter(|q| q.unit != UNIT_NOT_APPLICABLE && !q.unit.is_empty());

                // Without RECORD_TYPE in the grouping, credits and refunds are
                // already netted into each service's amount and there is
                // nothing left to label: such a payload is Usage throughout,
                // which is what it was read as before the dimension was added.
                let record_type = record_type_at.and_then(|at| group.keys.get(at));
                let category = record_type.map_or(ChargeCategory::Usage, |rt| charge_category(rt));

                charges.push(Charge {
                    service_name: group.keys.get(service_at).cloned(),
                    charge_description: record_type.cloned(),
                    billed_cost,
                    effective_cost,
                    pricing_quantity: quantity.map(|q| q.value()),
                    pricing_unit: quantity.map(|q| q.unit.clone()),
                    charge_category: category,
                    ..Charge::new(
                        start,
                        end,
                        unblended
                            .or(amortized)
                            .map_or_else(|| "USD".to_string(), |amount| amount.unit.clone()),
                    )
                });
            }
        }
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

/// The token for the next page of a Cost Explorer response, if it has one.
/// A body that does not parse has none: the request that produced it
/// failed, and is reported as such by whoever reads it.
fn next_page_token(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("NextPageToken")?
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Parse a Cost Explorer `YYYY-MM-DD` into an instant at UTC midnight.
fn parse_day(date: &str) -> Result<DateTime<Utc>> {
    let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|e| anyhow!("Unexpected Cost Explorer date {:?}: {}", date, e))?;
    Ok(crate::analytics::midnight(day))
}

/// STS Caller Identity
#[derive(Debug)]
struct StsCallerIdentity {
    account: String,
    arn: String,
}

/// Parse STS GetCallerIdentity XML response
fn parse_sts_response(xml: &str) -> Result<StsCallerIdentity> {
    // Simple XML parsing (avoid additional dependencies)
    let extract = |tag: &str| -> Option<String> {
        let start_tag = format!("<{}>", tag);
        let end_tag = format!("</{}>", tag);
        let start = xml.find(&start_tag)? + start_tag.len();
        let end = xml.find(&end_tag)?;
        Some(xml[start..end].to_string())
    };

    // Check for errors
    if xml.contains("<Error>") {
        let code = extract("Code").unwrap_or_else(|| "Unknown".to_string());
        let message = extract("Message").unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow!("AWS STS error: {} - {}", code, message));
    }

    Ok(StsCallerIdentity {
        account: extract("Account").unwrap_or_default(),
        arn: extract("Arn").unwrap_or_default(),
    })
}

impl BillingSource for AwsCloudService {
    fn validate_credentials(&self) -> Result<bool> {
        if let Some(uri) = &self.export_uri {
            let uri = S3Uri::parse(uri)?;
            self.s3_client().bucket_is_readable(&uri.bucket)?;
            return Ok(true);
        }
        match self.call_sts_get_caller_identity() {
            Ok(identity) => {
                tracing::info!(
                    "AWS credential validation successful: Account={}, Arn={}",
                    identity.account,
                    identity.arn
                );
                Ok(true)
            }
            Err(e) => {
                tracing::error!("AWS credential validation failed: {}", e);
                Err(e)
            }
        }
    }

    fn fetch(&self, period: &BillingPeriod) -> Result<Fetched> {
        if self.export_uri.is_some() {
            return self.fetch_focus_export(period, None)?.ok_or_else(|| {
                anyhow!("an export fetch with nothing to compare always downloads")
            });
        }
        // A large month comes back in pages; every page is followed, or
        // the ledger would hold part of the month as though it were all.
        let mut parts = Vec::new();
        let mut next_token: Option<String> = None;
        for page in 1..=MAX_COST_EXPLORER_PAGES {
            let mut request = ledger_request(
                &period.start().to_string(),
                &period.end_exclusive().to_string(),
            );
            if let Some(token) = &next_token {
                request["NextPageToken"] = serde_json::Value::String(token.clone());
            }
            let body = self.cost_and_usage_raw(&request)?;
            next_token = next_page_token(&body);
            let name = if page == 1 {
                PART_COST_AND_USAGE.to_string()
            } else {
                page_part_name(page)
            };
            parts.push(RawPart::new(name, serde_json::to_string(&request)?, body));
            if next_token.is_none() {
                return Ok(Fetched::parts_only(parts));
            }
        }
        // Kept rather than dropped, so the pages already paid for are on
        // disk; normalizing refuses the batch, so it is never recorded as
        // the month.
        parts.push(RawPart::new(
            PART_PAGES_TRUNCATED,
            "",
            format!("more than {MAX_COST_EXPLORER_PAGES} pages"),
        ));
        Ok(Fetched::parts_only(parts))
    }

    fn fetch_changed(
        &self,
        period: &BillingPeriod,
        previous: &[RawPart],
    ) -> Result<Option<Fetched>> {
        if self.export_uri.is_some() {
            let listing = previous
                .iter()
                .find(|part| part.name == PART_EXPORT_LISTING)
                .map(|part| part.body.as_str());
            return self.fetch_focus_export(period, listing);
        }
        self.fetch(period).map(Some)
    }

    fn normalize(&self, batch: &RawBatch) -> Result<Normalized> {
        // Dispatch on what the batch holds, not on this client's config:
        // `normalize_with` builds the client with empty credentials and no
        // export URI, and a replayed export batch must still map as FOCUS.
        if !batch.payload_files.is_empty() {
            return aws_focus::normalize(batch);
        }
        normalize(batch)
    }
}

/// A Cost Explorer error response, as the user should read it. The two
/// errors a new account meets first — a key without the permission, and an
/// account whose Cost Explorer has no data yet — say what to do; anything
/// else keeps the service's own body.
fn cost_explorer_error(status: u16, body: &str) -> anyhow::Error {
    if body.contains("AccessDeniedException") {
        anyhow!(
            "this access key may not call Cost Explorer. Attach a policy allowing \
             ce:GetCostAndUsage to its IAM user — see \
             https://cloudbridge.jetsquirrel.cloud/policies.html#aws-cost-explorer"
        )
    } else if body.contains("DataUnavailableException") {
        anyhow!(
            "Cost Explorer has no data for this account yet. It is enabled from the \
             Billing console and takes up to 24 hours to prepare the first data."
        )
    } else {
        anyhow!("Cost Explorer request failed: HTTP {} - {}", status, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{ChargeCategory, CostBasis};

    /// The layout of a real FOCUS export under a user's prefix, and of a
    /// "create new" one whose runs have folders of their own: both have
    /// their latest manifest in the partition folder under metadata/.
    #[test]
    fn a_data_key_leads_to_its_partitions_manifest() {
        assert_eq!(
            manifest_key(
                "cloudbridgeFOCUS/cloudbridge/data/billing_period=2026-09/cloudbridge-00001.csv.gz",
                "2026-09"
            )
            .as_deref(),
            Some("cloudbridgeFOCUS/cloudbridge/metadata/billing_period=2026-09/cloudbridge-Manifest.json")
        );
        assert_eq!(
            manifest_key(
                "p/cur/data/BILLING_PERIOD=2026-09/20260930T0100Z-abc/cur-00001.snappy.parquet",
                "2026-09"
            )
            .as_deref(),
            Some("p/cur/metadata/BILLING_PERIOD=2026-09/cur-Manifest.json")
        );
        assert_eq!(
            manifest_key("loose/billing_period=2026-09/x.csv.gz", "2026-09"),
            None
        );
    }

    /// A "create new" export keeps every run: only the files the manifest
    /// names are read, so the month is counted once.
    #[test]
    fn only_the_files_the_manifest_names_are_read() {
        let object = |key: &str| crate::cloud::s3::S3Object {
            key: key.to_string(),
            size: 1,
            etag: None,
        };
        let listed = vec![
            object("p/cur/data/BILLING_PERIOD=2026-09/20260929-old/cur-00001.csv.gz"),
            object("p/cur/data/BILLING_PERIOD=2026-09/20260930-new/cur-00001.csv.gz"),
            object("p/cur/data/BILLING_PERIOD=2026-09/20260930-new/cur-00002.csv.gz"),
        ];
        let manifest = r#"{"executionId":"new","dataFiles":[
            "s3://bucket/p/cur/data/BILLING_PERIOD=2026-09/20260930-new/cur-00001.csv.gz",
            "s3://bucket/p/cur/data/BILLING_PERIOD=2026-09/20260930-new/cur-00002.csv.gz"
        ],"additionalOutputFiles":[]}"#;

        let named = manifest_data_keys(manifest, "bucket").unwrap();
        let kept = files_named_by_manifest(listed.clone(), &named).unwrap();
        let keys: Vec<&str> = kept.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "p/cur/data/BILLING_PERIOD=2026-09/20260930-new/cur-00001.csv.gz",
                "p/cur/data/BILLING_PERIOD=2026-09/20260930-new/cur-00002.csv.gz"
            ]
        );

        let missing = files_named_by_manifest(listed[..1].to_vec(), &named).unwrap_err();
        assert_eq!(missing.len(), 2);
        assert!(manifest_data_keys(manifest, "other-bucket").is_err());
    }

    /// A FOCUS export spells its partition in lower case; a CUR 2.0 one
    /// in upper. Both are the period's data, the manifests are not.
    #[test]
    fn export_keys_are_matched_to_their_period() {
        let focus =
            "cloudbridgeFOCUS/cloudbridge/data/billing_period=2026-09/cloudbridge-00001.csv.gz";
        assert!(is_period_data(focus, "2026-09"));
        assert!(is_period_data(
            "data/BILLING_PERIOD=2026-09/part-0.parquet",
            "2026-09"
        ));
        assert!(!is_period_data(focus, "2026-08"));
        assert!(!is_period_data(
            "cloudbridgeFOCUS/cloudbridge/metadata/billing_period=2026-09/cloudbridge-Manifest.json",
            "2026-09"
        ));
    }

    #[test]
    fn both_partition_spellings_are_listed_before_the_whole_prefix() {
        let prefixes = export_prefixes("exports/cb", "2026-09");
        assert!(prefixes.contains(&"exports/cb/data/billing_period=2026-09/".to_string()));
        assert!(prefixes.contains(&"exports/cb/data/BILLING_PERIOD=2026-09/".to_string()));
        assert_eq!(prefixes.last().map(String::as_str), Some("exports/cb"));
    }
    #[test]
    fn cost_explorer_errors_name_the_fix() {
        let denied = cost_explorer_error(
            400,
            r#"{"__type":"AccessDeniedException","Message":"User is not authorized"}"#,
        );
        assert!(denied.to_string().contains("ce:GetCostAndUsage"));

        let unavailable = cost_explorer_error(400, r#"{"__type":"DataUnavailableException"}"#);
        assert!(unavailable.to_string().contains("24 hours"));

        let other = cost_explorer_error(500, "boom");
        assert_eq!(
            other.to_string(),
            "Cost Explorer request failed: HTTP 500 - boom"
        );
    }

    /// A recorded GetCostAndUsage response as this build asks for it:
    /// three metrics, grouped by service and record type.
    const COST_AND_USAGE: &str = include_str!("testdata/aws_cost_and_usage_record_type.json");

    /// A response recorded before RECORD_TYPE was in the grouping, of the
    /// kind already sitting in the raw store.
    const LEGACY_COST_AND_USAGE: &str = include_str!("testdata/aws_cost_and_usage.json");

    fn charge<'a>(normalized: &'a Normalized, service: &str, description: &str) -> &'a Charge {
        normalized
            .charges
            .iter()
            .find(|charge| {
                charge.service_name.as_deref() == Some(service)
                    && charge.charge_description.as_deref() == Some(description)
            })
            .unwrap_or_else(|| panic!("no {} / {} charge", service, description))
    }

    fn recorded_batch(body: &str) -> RawBatch {
        RawBatch {
            provider: "AWS".to_string(),
            account_id: "acct-1".to_string(),
            period: BillingPeriod::new(2026, 8),
            batch_id: "b-1".to_string(),
            fetched_at: "2026-08-03T04:00:00Z".parse().unwrap(),
            parts: vec![RawPart::new(PART_COST_AND_USAGE, "{}", body)],
            payload_files: Vec::new(),
        }
    }

    /// One page of a paged response, for the pagination tests: a single
    /// service on one day.
    fn page(service: &str, amount: &str, next: Option<&str>) -> String {
        let next = next.map_or(String::new(), |token| {
            format!(r#","NextPageToken":"{token}""#)
        });
        format!(
            r#"{{"GroupDefinitions":[{{"Type":"DIMENSION","Key":"SERVICE"}},{{"Type":"DIMENSION","Key":"RECORD_TYPE"}}],
               "ResultsByTime":[{{"TimePeriod":{{"Start":"2026-08-01","End":"2026-08-02"}},
               "Groups":[{{"Keys":["{service}","Usage"],
                 "Metrics":{{"UnblendedCost":{{"Amount":"{amount}","Unit":"USD"}},
                             "AmortizedCost":{{"Amount":"{amount}","Unit":"USD"}}}}}}]}}]{next}}}"#
        )
    }

    /// A month Cost Explorer splits is read whole: every page's charges.
    #[test]
    fn every_page_of_a_paged_month_is_read() {
        let mut batch = recorded_batch(&page("Amazon S3", "1.5", Some("t2")));
        batch.parts.push(RawPart::new(
            page_part_name(2),
            "{}",
            page("AWS Lambda", "2.25", None),
        ));
        let normalized = normalize(&batch).unwrap();
        let services: Vec<_> = normalized
            .charges
            .iter()
            .map(|c| c.service_name.as_deref().unwrap())
            .collect();
        assert_eq!(services, ["Amazon S3", "AWS Lambda"]);
    }

    /// Pages past the cap are kept on disk but never recorded as the
    /// month: it would be part of a month reading as all of it.
    #[test]
    fn a_month_cut_off_by_the_page_cap_is_not_recorded() {
        let mut batch = recorded_batch(&page("Amazon S3", "1.5", Some("t2")));
        batch
            .parts
            .push(RawPart::new(PART_PAGES_TRUNCATED, "", "more than 20 pages"));
        let error = normalize(&batch).unwrap_err().to_string();
        assert!(error.contains("more than"), "{error}");
    }

    #[test]
    fn the_next_page_token_is_read_from_the_body() {
        assert_eq!(
            next_page_token(&page("Amazon S3", "1", Some("abc"))).as_deref(),
            Some("abc")
        );
        assert_eq!(next_page_token(&page("Amazon S3", "1", None)), None);
        assert_eq!(next_page_token(r#"{"NextPageToken":""}"#), None);
        assert_eq!(next_page_token("not json"), None);
    }

    /// The same files are the same listing however it was printed; a
    /// changed ETag, or a file more, is a new delivery.
    #[test]
    fn an_export_listing_is_unchanged_only_with_the_same_files() {
        let listing = r#"[{"key":"data/a.parquet","size":10,"etag":"\"e1\""}]"#;
        let pretty = "[\n  {\n    \"key\": \"data/a.parquet\",\n    \"size\": 10,\n    \"etag\": \"\\\"e1\\\"\"\n  }\n]";
        assert!(same_listing(listing, pretty));
        assert!(!same_listing(
            listing,
            r#"[{"key":"data/a.parquet","size":10,"etag":"\"e2\""}]"#
        ));
        assert!(!same_listing(
            listing,
            r#"[{"key":"data/a.parquet","size":10,"etag":"\"e1\""},{"key":"data/b.parquet","size":4,"etag":"\"e3\""}]"#
        ));
        assert!(!same_listing("", listing));
    }

    #[test]
    fn test_sha256_hash() {
        let hash = AwsCloudService::sha256_hash(b"test");
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA256 produces 32 bytes = 64 hex characters
    }

    #[test]
    fn a_recorded_response_normalizes_to_one_charge_per_service_day() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();

        // Eight non-zero groups across two days; the all-zero KMS row is
        // dropped.
        assert_eq!(normalized.charges.len(), 8);
        assert!(normalized.balances.is_empty());

        let ec2 = charge(
            &normalized,
            "Amazon Elastic Compute Cloud - Compute",
            "Usage",
        );
        assert_eq!(ec2.billed_cost, Some(12.45));
        assert_eq!(ec2.effective_cost, Some(10.20));
        assert_eq!(ec2.billing_currency, "USD");
        assert_eq!(ec2.cost_basis, CostBasis::Authoritative);
        assert_eq!(
            ec2.charge_period_start.to_rfc3339(),
            "2026-08-01T00:00:00+00:00"
        );
        assert_eq!(
            ec2.charge_period_end.to_rfc3339(),
            "2026-08-02T00:00:00+00:00"
        );
    }

    #[test]
    fn the_record_type_decides_the_charge_category() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();

        let category = |service: &str, record_type: &str| {
            charge(&normalized, service, record_type).charge_category
        };
        let ec2 = "Amazon Elastic Compute Cloud - Compute";

        assert_eq!(category(ec2, "Usage"), ChargeCategory::Usage);
        assert_eq!(
            category(ec2, "SavingsPlanCoveredUsage"),
            ChargeCategory::Usage
        );
        assert_eq!(category(ec2, "Credit"), ChargeCategory::Credit);
        assert_eq!(category(ec2, "Refund"), ChargeCategory::Adjustment);
        assert_eq!(category("Tax", "Tax"), ChargeCategory::Tax);
        assert_eq!(
            category("AWS Support (Developer)", "Fee"),
            ChargeCategory::Purchase
        );
        // A record type this build has never seen still moved money, so it
        // is kept and labelled as an adjustment rather than as usage.
        assert_eq!(
            category("Amazon Route 53", "SomeFutureRecordType"),
            ChargeCategory::Adjustment
        );
    }

    #[test]
    fn credits_and_refunds_keep_their_sign_so_the_total_nets_out() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();
        let ec2 = "Amazon Elastic Compute Cloud - Compute";

        assert_eq!(charge(&normalized, ec2, "Credit").billed_cost, Some(-3.0));
        assert_eq!(charge(&normalized, ec2, "Refund").billed_cost, Some(-1.5));

        let total: f64 = normalized
            .charges
            .iter()
            .filter_map(|charge| charge.billed_cost)
            .sum();
        // 12.45 + 0 + 0.75 + 29.00 - 3.00 - 1.50 + 2.10 + 1.23
        assert!((total - 41.03).abs() < 1e-9, "got {total}");
    }

    #[test]
    fn usage_a_commitment_already_paid_for_is_not_mistaken_for_an_empty_row() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();
        let covered = charge(
            &normalized,
            "Amazon Elastic Compute Cloud - Compute",
            "SavingsPlanCoveredUsage",
        );

        // Nothing was charged for it this day, but the amortized figure is
        // what the commitment cost — dropping the row would lose it.
        assert_eq!(covered.billed_cost, Some(0.0));
        assert_eq!(covered.effective_cost, Some(3.10));
    }

    #[test]
    fn a_quantity_is_only_kept_when_it_has_one_real_unit() {
        let normalized = normalize(&recorded_batch(COST_AND_USAGE)).unwrap();

        let ec2 = charge(
            &normalized,
            "Amazon Elastic Compute Cloud - Compute",
            "Usage",
        );
        assert_eq!(ec2.pricing_quantity, Some(24.0));
        assert_eq!(ec2.pricing_unit.as_deref(), Some("Hrs"));

        // Grouping by service mixes usage types, and Cost Explorer says so
        // with "N/A". A number in mixed units cannot be added to anything.
        let s3 = charge(&normalized, "Amazon Simple Storage Service", "Usage");
        assert_eq!(s3.pricing_quantity, None);
        assert_eq!(s3.pricing_unit, None);
    }

    #[test]
    fn a_payload_recorded_before_record_type_still_normalizes() {
        let normalized = normalize(&recorded_batch(LEGACY_COST_AND_USAGE)).unwrap();

        assert_eq!(normalized.charges.len(), 3);
        // Credits were already netted into each service's amount, so there
        // is nothing to label and nothing to amortize.
        assert!(normalized
            .charges
            .iter()
            .all(|charge| charge.charge_category == ChargeCategory::Usage));
        assert!(normalized
            .charges
            .iter()
            .all(|charge| charge.effective_cost.is_none()));
        assert_eq!(normalized.charges[0].billed_cost, Some(12.45));
    }

    #[test]
    fn normalizing_is_not_affected_by_when_it_runs() {
        let batch = recorded_batch(COST_AND_USAGE);
        let mut later = batch.clone();
        later.fetched_at = "2027-01-01T00:00:00Z".parse().unwrap();
        later.batch_id = "b-2".to_string();

        let first = normalize(&batch).unwrap();
        let second = normalize(&later).unwrap();

        assert_eq!(first.charges.len(), second.charges.len());
        for (a, b) in first.charges.iter().zip(&second.charges) {
            assert_eq!(a.billed_cost, b.billed_cost);
            assert_eq!(a.charge_period_start, b.charge_period_start);
            assert_eq!(a.service_name, b.service_name);
        }
    }

    #[test]
    fn a_batch_without_the_expected_payload_is_an_error() {
        let mut batch = recorded_batch(COST_AND_USAGE);
        batch.parts.clear();
        assert!(normalize(&batch).is_err());
    }

    #[test]
    fn the_ledger_request_carries_every_metric_in_one_call() {
        let request = ledger_request("2026-08-01", "2026-09-01");

        // Cost Explorer bills per request: three metrics, one call.
        let metrics = request["Metrics"].as_array().unwrap();
        assert_eq!(metrics.len(), 3);
        assert!(metrics.iter().any(|m| m == "UnblendedCost"));
        assert!(metrics.iter().any(|m| m == "AmortizedCost"));
        assert!(metrics.iter().any(|m| m == "UsageQuantity"));

        assert_eq!(request["GroupBy"][0]["Key"], "SERVICE");
        assert_eq!(request["GroupBy"][1]["Key"], "RECORD_TYPE");
        assert_eq!(request["Granularity"], "DAILY");
    }
}
