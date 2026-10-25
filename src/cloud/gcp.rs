//! Google Cloud — the Cloud Billing export to BigQuery, read with a query.
//!
//! Google Cloud has no API that answers "what did this month cost": the
//! billing data is the export a billing account writes to a BigQuery
//! dataset, and reading it means running a query job. So the account names
//! the export table, and the credential is a key that may query it — a
//! service account with BigQuery Job User on the table's project and Data
//! Viewer on its dataset, or the user credentials `gcloud auth
//! application-default login` writes.
//!
//! The key is given as the path of its JSON file, as
//! `GOOGLE_APPLICATION_CREDENTIALS` gives it, and read each time: the file
//! stays where the user keeps it, and a key rotated on disk is picked up.
//! The JSON itself is accepted too, for a keyring that holds it.
//!
//! One query per month, grouped by day, service, SKU, project, region and
//! cost type, summed. Credits come back as their own column and become their
//! own `Credit` rows, so usage stays gross and what credits took off it is
//! visible, as for the other clouds.

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::http::{self, FailureKind};
use super::raw::RawPart;
use super::{BillingPeriod, BillingSource, Fetched, Normalized, RawBatch};
use crate::ledger::{Charge, ChargeCategory};

const BIGQUERY: &str = "https://bigquery.googleapis.com/bigquery/v2";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Running a query job needs the full BigQuery scope; the job only reads.
const SCOPE: &str = "https://www.googleapis.com/auth/bigquery";

/// Prefix of the result payloads, one per page; the page number follows.
const PART_RESULT_PREFIX: &str = "bigquery_result:";

/// Rows per result page, and how many pages one month may take.
const PAGE_ROWS: usize = 10_000;
const MAX_PAGES: usize = 200;

pub struct GcpService {
    /// A key file's path, or the key's JSON.
    key: String,
    /// What the account's scope field holds: `project.dataset.table`.
    table: String,
}

impl GcpService {
    pub fn new(key: String, _secret: String, table: Option<String>) -> Self {
        Self {
            key: key.trim().to_string(),
            table: table.unwrap_or_default().trim().to_string(),
        }
    }

    /// An access token for the key, by whichever grant its type uses.
    fn token(&self) -> Result<String> {
        let key = read_key(&self.key)?;
        let token_uri = key.token_uri.as_deref().unwrap_or(DEFAULT_TOKEN_URI);
        let agent = http::agent(http::API_TIMEOUT);
        let response = match key.kind.as_str() {
            "service_account" => {
                let assertion = signed_assertion(&key, token_uri, Utc::now())?;
                agent.post(token_uri).send_form([
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", assertion.as_str()),
                ])
            }
            "authorized_user" => {
                let field = |value: &Option<String>, name: &str| {
                    value
                        .clone()
                        .ok_or_else(|| anyhow!("The credentials file has no {name}"))
                };
                let client_id = field(&key.client_id, "client_id")?;
                let client_secret = field(&key.client_secret, "client_secret")?;
                let refresh_token = field(&key.refresh_token, "refresh_token")?;
                agent.post(token_uri).send_form([
                    ("grant_type", "refresh_token"),
                    ("client_id", client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("refresh_token", refresh_token.as_str()),
                ])
            }
            other => {
                return Err(http::failure(
                    Some(FailureKind::Denied),
                    format!(
                        "A '{other}' credentials file can't query BigQuery here. Use a \
                         service account key, or the file `gcloud auth \
                         application-default login` writes"
                    ),
                ))
            }
        }
        .map_err(|e| http::request_failed("Google sign-in failed", e))?;

        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow!("Failed to read the Google sign-in response: {}", e))?;
        let token: TokenResponse = serde_json::from_str(&body).map_err(|_| {
            anyhow!(
                "Google sign-in returned HTTP {} with an unreadable body",
                status
            )
        })?;
        match token.access_token {
            Some(token) if status < 400 => Ok(token),
            _ => Err(http::failure(
                // invalid_grant: a revoked key or refresh token.
                match status {
                    400 | 401 | 403 => Some(FailureKind::Denied),
                    status => http::status_kind(status),
                },
                format!(
                    "Google sign-in failed: HTTP {} - {}",
                    status,
                    token
                        .error_description
                        .or(token.error)
                        .unwrap_or_else(|| "no reason given".to_string())
                ),
            )),
        }
    }

    /// Send one BigQuery request, returning the body once the status says
    /// it succeeded.
    fn call(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithBody>,
        token: &str,
        body: &str,
    ) -> Result<String> {
        let response = request
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|e| http::request_failed("BigQuery request failed", e))?;
        read_bigquery(response)
    }

    fn get(&self, url: &str, token: &str) -> Result<String> {
        let response = http::agent(http::API_TIMEOUT)
            .get(url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|e| http::request_failed("BigQuery request failed", e))?;
        read_bigquery(response)
    }

    /// Every result page of the period's query, as raw parts.
    fn query(&self, period: &BillingPeriod) -> Result<Vec<RawPart>> {
        let table = Table::parse(&self.table)?;
        let token = self.token()?;
        let request = query_request(&table, period);
        let url = format!("{BIGQUERY}/projects/{}/queries", table.project);

        let mut parts = Vec::new();
        let first = self.call(http::agent(http::API_TIMEOUT).post(&url), &token, &request)?;
        let mut page: ResultPage = serde_json::from_str(&first)
            .map_err(|e| anyhow!("Failed to parse the BigQuery response: {}", e))?;
        parts.push(RawPart::new(
            format!("{PART_RESULT_PREFIX}0"),
            format!("POST {url}\n{request}"),
            first,
        ));

        // A job still running, or a result longer than one page, is read
        // back by its job id until it is complete and has no next page.
        for number in 1..MAX_PAGES {
            let more = !page.job_complete || page.page_token.is_some();
            if !more {
                return Ok(parts);
            }
            let job = page.job_reference.as_ref().ok_or_else(|| {
                anyhow!("BigQuery returned no job reference to read the result by")
            })?;
            let mut next = format!(
                "{BIGQUERY}/projects/{}/queries/{}?maxResults={PAGE_ROWS}&timeoutMs=30000",
                job.project_id, job.job_id
            );
            if let Some(location) = &job.location {
                next.push_str(&format!("&location={location}"));
            }
            if let Some(token) = &page.page_token {
                next.push_str(&format!("&pageToken={token}"));
            }
            let body = self.get(&next, &token)?;
            page = serde_json::from_str(&body)
                .map_err(|e| anyhow!("Failed to parse the BigQuery response: {}", e))?;
            // A page re-read while the job was still running has no rows;
            // it is not kept, so the normalizer sees each row once.
            if page.job_complete {
                parts.push(RawPart::new(
                    format!("{PART_RESULT_PREFIX}{number}"),
                    format!("GET {next}"),
                    body,
                ));
            }
        }
        Err(anyhow!(
            "The BigQuery result for {} ran past {MAX_PAGES} pages",
            period.label()
        ))
    }
}

impl BillingSource for GcpService {
    fn validate_credentials(&self) -> Result<bool> {
        let period = BillingPeriod::containing(Utc::now());
        match self.query(&period) {
            Ok(_) => Ok(true),
            Err(e) => {
                tracing::warn!("Google Cloud credential validation failed: {}", e);
                Ok(false)
            }
        }
    }

    fn fetch(&self, period: &BillingPeriod) -> Result<Fetched> {
        Ok(Fetched::parts_only(self.query(period)?))
    }

    fn normalize(&self, batch: &RawBatch) -> Result<Normalized> {
        normalize(batch)
    }
}

/// A BigQuery response's body, or an error carrying its message.
fn read_bigquery(response: ureq::http::Response<ureq::Body>) -> Result<String> {
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .read_to_string()
        .map_err(|e| anyhow!("Failed to read the BigQuery response: {}", e))?;
    if status >= 400 {
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| body.chars().take(300).collect());
        return Err(http::status_failed(
            status,
            format!("BigQuery request failed: HTTP {status} - {message}"),
        ));
    }
    Ok(body)
}

/// The fields of a credentials file this client reads.
#[derive(Deserialize)]
struct Key {
    #[serde(rename = "type")]
    kind: String,
    client_email: Option<String>,
    private_key: Option<String>,
    token_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
}

/// The key the account names: its JSON, or the file at its path.
fn read_key(key: &str) -> Result<Key> {
    let json = if key.starts_with('{') {
        key.to_string()
    } else if key.is_empty() {
        return Err(http::failure(
            Some(FailureKind::Denied),
            "Enter the path of the service account key file".to_string(),
        ));
    } else {
        std::fs::read_to_string(key)
            .with_context(|| format!("Couldn't read the Google Cloud credentials file at {key}"))?
    };
    serde_json::from_str(&json)
        .map_err(|e| anyhow!("The Google Cloud credentials file isn't valid JSON: {}", e))
}

/// The signed JWT a service account exchanges for an access token.
fn signed_assertion(key: &Key, token_uri: &str, now: DateTime<Utc>) -> Result<String> {
    let email = key
        .client_email
        .as_deref()
        .ok_or_else(|| anyhow!("The service account key has no client_email"))?;
    let pem = key
        .private_key
        .as_deref()
        .ok_or_else(|| anyhow!("The service account key has no private_key"))?;

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::json!({
            "iss": email,
            "scope": SCOPE,
            "aud": token_uri,
            "iat": now.timestamp(),
            "exp": (now + Duration::minutes(60)).timestamp(),
        })
        .to_string(),
    );
    let signing_input = format!("{header}.{claims}");

    let der = pem_body(pem)?;
    let pair = ring::signature::RsaKeyPair::from_pkcs8(&der)
        .map_err(|e| anyhow!("The service account's private key can't be read: {}", e))?;
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow!("Couldn't sign the Google Cloud token request"))?;

    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

/// The DER bytes of a PEM `PRIVATE KEY` block.
fn pem_body(pem: &str) -> Result<Vec<u8>> {
    let base64: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("-----"))
        .collect();
    STANDARD
        .decode(base64)
        .map_err(|e| anyhow!("The service account's private key isn't valid PEM: {}", e))
}

/// A BigQuery table id, as the console's "Copy ID" gives it.
#[derive(Debug, PartialEq, Eq)]
struct Table {
    project: String,
    dataset: String,
    table: String,
}

impl Table {
    /// `project.dataset.table`, with or without backticks, or the older
    /// `project:dataset.table`.
    fn parse(value: &str) -> Result<Self> {
        let value = value.trim().trim_matches('`');
        let value = value.replacen(':', ".", 1);
        let mut parts = value.splitn(3, '.');
        let (Some(project), Some(dataset), Some(table)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(anyhow!(
                "'{}' is not a BigQuery table. Copy the billing export table's ID from \
                 the BigQuery console: project.dataset.gcp_billing_export_v1_…",
                value
            ));
        };
        // These go into the query text, so only what an id can contain.
        let id_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if [project, dataset, table]
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(id_char))
        {
            return Err(anyhow!("'{}' is not a BigQuery table ID", value));
        }
        Ok(Self {
            project: project.to_string(),
            dataset: dataset.to_string(),
            table: table.to_string(),
        })
    }
}

/// The month's query: one row per day, service, SKU, project, region, cost
/// type and currency, with cost, credits and usage summed. The month is the
/// invoice month, which is what the bill is; a late row lands in it even
/// when its usage day was in the month before.
fn query_request(table: &Table, period: &BillingPeriod) -> String {
    let query = format!(
        "SELECT
           FORMAT_TIMESTAMP('%Y-%m-%d', usage_start_time) AS day,
           service.description AS service,
           service.id AS service_id,
           sku.description AS sku,
           project.id AS project_id,
           project.name AS project_name,
           location.region AS region,
           cost_type,
           currency,
           SUM(cost) AS cost,
           SUM(IFNULL((SELECT SUM(c.amount) FROM UNNEST(credits) AS c), 0)) AS credits,
           SUM(usage.amount) AS usage_amount,
           ANY_VALUE(usage.unit) AS usage_unit
         FROM `{}.{}.{}`
         WHERE invoice.month = @invoice_month
         GROUP BY day, service, service_id, sku, project_id, project_name, region, cost_type, currency",
        table.project, table.dataset, table.table
    );
    serde_json::json!({
        "query": query,
        "useLegacySql": false,
        "parameterMode": "NAMED",
        "queryParameters": [{
            "name": "invoice_month",
            "parameterType": { "type": "STRING" },
            "parameterValue": { "value": invoice_month(period) },
        }],
        "maxResults": PAGE_ROWS,
        "timeoutMs": 30000,
    })
    .to_string()
}

/// A billing period as BigQuery's `invoice.month`: `YYYYMM`.
fn invoice_month(period: &BillingPeriod) -> String {
    format!("{:04}{:02}", period.year, period.month)
}

/// Turn fetched result pages into ledger rows. Pure — every input is in
/// `batch`.
pub fn normalize(batch: &RawBatch) -> Result<Normalized> {
    let pages: Vec<&RawPart> = batch
        .parts
        .iter()
        .filter(|part| part.name.starts_with(PART_RESULT_PREFIX))
        .collect();
    if pages.is_empty() {
        return Err(anyhow!("Raw batch has no BigQuery result payload"));
    }

    // The schema is on the first page; later pages carry rows only.
    let mut schema: Option<Vec<String>> = None;
    let mut charges = Vec::new();
    for part in pages {
        let page: ResultPage = serde_json::from_str(&part.body)
            .map_err(|e| anyhow!("Failed to parse the BigQuery result: {}", e))?;
        if let Some(fields) = page.schema {
            schema = Some(fields.fields.into_iter().map(|field| field.name).collect());
        }
        let Some(names) = schema.as_ref() else {
            return Err(anyhow!("The BigQuery result has no schema"));
        };
        let column = |name: &str| names.iter().position(|field| field == name);
        let required = |name: &str| {
            column(name).ok_or_else(|| anyhow!("The BigQuery result has no {name} column"))
        };
        let (day, cost) = (required("day")?, required("cost")?);

        for row in page.rows {
            let cell = |index: Option<usize>| {
                index
                    .and_then(|index| row.f.get(index))
                    .and_then(|cell| cell.v.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let number =
                |index: Option<usize>| cell(index).and_then(|value| value.parse::<f64>().ok());

            let date = cell(Some(day))
                .and_then(|value| NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok())
                .ok_or_else(|| anyhow!("A BigQuery billing row has no readable day"))?;
            let start = date.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
            let gross = number(Some(cost)).unwrap_or(0.0);
            let credits = number(column("credits")).unwrap_or(0.0);
            let base = Charge {
                charge_description: cell(column("sku")),
                service_name: cell(column("service")),
                x_service_code: cell(column("service_id")),
                sub_account_id: cell(column("project_id")),
                sub_account_name: cell(column("project_name")),
                region_id: cell(column("region")),
                ..Charge::new(
                    start,
                    start + Duration::days(1),
                    cell(column("currency")).unwrap_or_else(|| "USD".to_string()),
                )
            };

            if gross != 0.0 {
                charges.push(Charge {
                    charge_category: charge_category(cell(column("cost_type")).as_deref()),
                    billed_cost: Some(gross),
                    effective_cost: Some(gross),
                    list_cost: Some(gross),
                    pricing_quantity: number(column("usage_amount")),
                    pricing_unit: cell(column("usage_unit")),
                    ..base.clone()
                });
            }
            // Credits are negative already: what came off the row's cost.
            if credits != 0.0 {
                charges.push(Charge {
                    charge_category: ChargeCategory::Credit,
                    billed_cost: Some(credits),
                    effective_cost: Some(credits),
                    ..base
                });
            }
        }
    }

    Ok(Normalized {
        charges,
        balances: Vec::new(),
    })
}

/// The ledger category of a billing export `cost_type`.
fn charge_category(cost_type: Option<&str>) -> ChargeCategory {
    match cost_type {
        Some("tax") => ChargeCategory::Tax,
        Some("adjustment") | Some("rounding_error") => ChargeCategory::Adjustment,
        _ => ChargeCategory::Usage,
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResultPage {
    #[serde(default)]
    job_complete: bool,
    job_reference: Option<JobReference>,
    page_token: Option<String>,
    schema: Option<Schema>,
    #[serde(default)]
    rows: Vec<Row>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobReference {
    project_id: String,
    job_id: String,
    location: Option<String>,
}

#[derive(Deserialize)]
struct Schema {
    fields: Vec<Field>,
}

#[derive(Deserialize)]
struct Field {
    name: String,
}

#[derive(Deserialize)]
struct Row {
    f: Vec<Cell>,
}

#[derive(Deserialize)]
struct Cell {
    v: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(bodies: &[&str]) -> RawBatch {
        RawBatch {
            provider: "GCP".to_string(),
            account_id: "a".to_string(),
            period: BillingPeriod::new(2026, 9),
            batch_id: "b".to_string(),
            fetched_at: Utc::now(),
            parts: bodies
                .iter()
                .enumerate()
                .map(|(page, body)| {
                    RawPart::new(format!("{PART_RESULT_PREFIX}{page}"), "POST", *body)
                })
                .collect(),
            payload_files: Vec::new(),
        }
    }

    #[test]
    fn rows_become_charges_with_credits_apart() {
        let normalized =
            normalize(&batch(&[include_str!("testdata/gcp_billing_query.json")])).unwrap();
        let charges = &normalized.charges;

        // Compute (with a credit), storage, tax; the zero row is dropped.
        assert_eq!(charges.len(), 4, "{charges:#?}");
        let compute = &charges[0];
        assert_eq!(compute.service_name.as_deref(), Some("Compute Engine"));
        assert_eq!(compute.x_service_code.as_deref(), Some("6F81-5844-456A"));
        assert_eq!(
            compute.charge_description.as_deref(),
            Some("N2 Instance Core running in Americas")
        );
        assert_eq!(compute.sub_account_id.as_deref(), Some("shop-prod"));
        assert_eq!(compute.region_id.as_deref(), Some("us-central1"));
        assert_eq!(compute.billed_cost, Some(10.0));
        assert_eq!(compute.pricing_quantity, Some(240.0));
        assert_eq!(compute.charge_category, ChargeCategory::Usage);
        assert_eq!(
            compute.charge_period_start.date_naive(),
            NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()
        );

        let credit = &charges[1];
        assert_eq!(credit.charge_category, ChargeCategory::Credit);
        assert_eq!(credit.billed_cost, Some(-2.5));
        assert_eq!(credit.service_name.as_deref(), Some("Compute Engine"));

        assert_eq!(charges[3].charge_category, ChargeCategory::Tax);
    }

    #[test]
    fn a_later_page_is_read_against_the_first_pages_schema() {
        let first = include_str!("testdata/gcp_billing_query.json");
        let second = r#"{"jobComplete": true, "rows": [
            {"f": [{"v": "2026-09-30"}, {"v": "BigQuery"}, {"v": "24E6-581D-38E5"}, {"v": "Analysis"},
                   {"v": "shop-prod"}, {"v": "Shop"}, {"v": null}, {"v": "regular"}, {"v": "USD"},
                   {"v": "1.25"}, {"v": "0"}, {"v": "0.2"}, {"v": "tebibyte"}]}
        ]}"#;
        let normalized = normalize(&batch(&[first, second])).unwrap();
        let last = normalized.charges.last().unwrap();
        assert_eq!(last.service_name.as_deref(), Some("BigQuery"));
        assert_eq!(last.billed_cost, Some(1.25));
        assert_eq!(last.region_id, None);
    }

    #[test]
    fn the_table_id_reads_as_the_console_copies_it() {
        let table = Table {
            project: "billing-prj".to_string(),
            dataset: "billing".to_string(),
            table: "gcp_billing_export_v1_01AB23_45CD67_89EF01".to_string(),
        };
        assert_eq!(
            Table::parse("billing-prj.billing.gcp_billing_export_v1_01AB23_45CD67_89EF01").unwrap(),
            table
        );
        assert_eq!(
            Table::parse("`billing-prj.billing.gcp_billing_export_v1_01AB23_45CD67_89EF01`")
                .unwrap(),
            table
        );
        assert_eq!(
            Table::parse("billing-prj:billing.gcp_billing_export_v1_01AB23_45CD67_89EF01").unwrap(),
            table
        );
        assert!(Table::parse("billing").is_err());
        // Nothing that could close the quoted identifier in the query.
        assert!(Table::parse("p.d.t` WHERE 1=1 --").is_err());
    }

    #[test]
    fn the_query_names_the_table_and_the_invoice_month() {
        let table = Table::parse("p.d.t").unwrap();
        let request: Value =
            serde_json::from_str(&query_request(&table, &BillingPeriod::new(2026, 3))).unwrap();
        assert!(request["query"].as_str().unwrap().contains("FROM `p.d.t`"));
        assert_eq!(
            request["queryParameters"][0]["parameterValue"]["value"],
            "202603"
        );
        assert_eq!(request["useLegacySql"], false);
    }

    #[test]
    fn a_service_account_signs_its_own_assertion() {
        // A throwaway 2048-bit key, generated for this test only.
        let pem = include_str!("testdata/gcp_test_key.pem");
        let key = Key {
            kind: "service_account".to_string(),
            client_email: Some("reader@billing-prj.iam.gserviceaccount.com".to_string()),
            private_key: Some(pem.to_string()),
            token_uri: None,
            client_id: None,
            client_secret: None,
            refresh_token: None,
        };
        let now = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        let jwt = signed_assertion(&key, DEFAULT_TOKEN_URI, now).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "reader@billing-prj.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], DEFAULT_TOKEN_URI);
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            3600
        );
        // RS256 over a 2048-bit modulus is 256 bytes.
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[2]).unwrap().len(), 256);
    }

    #[test]
    fn a_key_is_its_json_or_the_file_at_a_path() {
        let json = r#"{"type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r"}"#;
        assert_eq!(read_key(json).unwrap().kind, "authorized_user");
        assert!(read_key("/no/such/key.json").is_err());
        assert!(read_key("").is_err());
    }
}
