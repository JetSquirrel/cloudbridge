//! S3 over plain HTTPS with hand-rolled SigV4 signing.
//!
//! The AWS Data Exports channel lists and downloads the FOCUS Parquet
//! objects a billing export lands in a bucket. DuckDB's httpfs could read
//! `s3://` URIs but cannot enumerate them, and pulling the whole extension
//! in for two REST calls is not worth it — the signing here is the same
//! SigV4 the Cost Explorer client already uses, adapted from the DuckLocal
//! project's standalone S3 client.
//!
//! All functions are blocking; callers run them off the UI thread like
//! every other fetch.

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::io::Read;

/// Safety bound on a single listing; a billing export partition has a
/// handful of objects, so hitting this means the prefix is wrong.
const MAX_LIST_OBJECTS: usize = 10_000;

/// A parsed `s3://bucket/prefix` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Uri {
    pub bucket: String,
    /// Everything after the bucket, no leading or trailing slash.
    pub prefix: String,
}

impl S3Uri {
    pub fn parse(value: &str) -> Result<Self> {
        let rest = value
            .strip_prefix("s3://")
            .ok_or_else(|| anyhow!("An S3 URI starts with s3://, got {:?}", value))?;
        let (bucket, prefix) = match rest.split_once('/') {
            Some((bucket, prefix)) => (bucket, prefix),
            None => (rest, ""),
        };
        if bucket.is_empty() {
            return Err(anyhow!("The S3 URI {:?} has no bucket", value));
        }
        Ok(Self {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
        })
    }
}

/// One object in a listing.
#[derive(Debug, Clone)]
pub struct S3Object {
    pub key: String,
    pub size: i64,
    pub etag: Option<String>,
}

/// Static-credentials S3 client. Temporary (session-token) credentials are
/// not supported; the account model stores a key pair, not a session.
#[derive(Debug, Clone)]
pub struct S3Client {
    access_key_id: String,
    secret_access_key: String,
    region: String,
}

impl S3Client {
    pub fn new(access_key_id: String, secret_access_key: String, region: Option<String>) -> Self {
        Self {
            access_key_id,
            secret_access_key,
            region: region.unwrap_or_else(|| "us-east-1".to_string()),
        }
    }

    /// Every object under `prefix`, recursively, pagination unfolded.
    pub fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<S3Object>> {
        let mut objects = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), prefix.to_string()),
                ("max-keys".to_string(), "1000".to_string()),
            ];
            if let Some(token) = &token {
                query.push(("continuation-token".to_string(), token.clone()));
            }
            let body = self.get_text(bucket, "/", &query)?;
            let page: ListBucketResult = quick_xml::de::from_str(&body)?;
            objects.extend(page.contents.into_iter().map(|c| S3Object {
                key: c.key,
                size: c.size,
                etag: c.etag,
            }));
            if !page.is_truncated || objects.len() >= MAX_LIST_OBJECTS {
                break;
            }
            token = page.next_continuation_token;
            if token.is_none() {
                break;
            }
        }
        Ok(objects)
    }

    /// Whether the bucket answers a listing at all — the credential check
    /// for an export-backed account.
    pub fn bucket_is_readable(&self, bucket: &str) -> Result<()> {
        let query = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), "1".to_string()),
        ];
        self.get_text(bucket, "/", &query)?;
        Ok(())
    }

    /// One object's bytes.
    pub fn get_object(&self, bucket: &str, key: &str) -> Result<Vec<u8>> {
        self.get_bytes(bucket, &format!("/{}", uri_encode_path(key)), &[])
    }

    /// A signed GET, with one region-redirect retry, decoded as text.
    fn get_text(&self, bucket: &str, path: &str, query: &[(String, String)]) -> Result<String> {
        String::from_utf8(self.get_bytes(bucket, path, query)?)
            .map_err(|e| anyhow!("S3 returned a non-UTF-8 response: {}", e))
    }

    /// A signed GET against `bucket`. A bucket outside the configured
    /// region answers 301/400 with its actual region in the
    /// `x-amz-bucket-region` header (and `<Region>` in the body) — retry
    /// once signed for that region.
    fn get_bytes(&self, bucket: &str, path: &str, query: &[(String, String)]) -> Result<Vec<u8>> {
        let mut region = self.region.clone();
        let mut retried = false;
        loop {
            match self.get_once(&region, bucket, path, query)? {
                S3Response::Ok(body) => return Ok(body),
                S3Response::Failed {
                    message,
                    region_hint,
                } => {
                    if !retried {
                        if let Some(actual) = region_hint.filter(|hint| *hint != region) {
                            region = actual;
                            retried = true;
                            continue;
                        }
                    }
                    return Err(anyhow!(message));
                }
            }
        }
    }

    fn get_once(
        &self,
        region: &str,
        bucket: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<S3Response> {
        let host = format!("{}.s3.amazonaws.com", bucket);
        let url = format!("https://{}{}{}", host, path, encode_query(query));
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let headers = vec![
            ("host".to_string(), host),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        let auth = sigv4_authorization(
            "s3",
            region,
            &self.access_key_id,
            &self.secret_access_key,
            "GET",
            path,
            query,
            &headers,
            &amz_date,
            "UNSIGNED-PAYLOAD",
        );

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .into();
        let mut response = agent
            .get(&url)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .header("Authorization", &auth)
            .call()?;
        let status = response.status();
        let region_hint = response
            .headers()
            .get("x-amz-bucket-region")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        if status.is_success() {
            let mut body = Vec::new();
            response.body_mut().as_reader().read_to_end(&mut body)?;
            return Ok(S3Response::Ok(body));
        }
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let parsed = quick_xml::de::from_str::<ErrorResponse>(&body).ok();
        let reason = parsed
            .as_ref()
            .and_then(|e| e.message.clone())
            .unwrap_or_else(|| body.chars().take(200).collect());
        Ok(S3Response::Failed {
            message: format!(
                "S3 GET {} failed (HTTP {}): {}",
                path,
                status.as_u16(),
                reason
            ),
            region_hint: region_hint.or_else(|| parsed.and_then(|e| e.region)),
        })
    }
}

enum S3Response {
    Ok(Vec<u8>),
    Failed {
        message: String,
        region_hint: Option<String>,
    },
}

fn encode_query(query: &[(String, String)]) -> String {
    if query.is_empty() {
        return String::new();
    }
    let pairs: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v)))
        .collect();
    format!("?{}", pairs.join("&"))
}

/// RFC 3986 unreserved characters pass through; everything else is
/// percent-encoded. Query values never need literal `/` preserved.
fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Like [`uri_encode`] but for a canonical URI path, where `/` separates
/// segments and stays literal.
fn uri_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// AWS Signature Version 4, header-based auth. `headers` are the headers
/// that will be sent and signed (must include `host` and `x-amz-date`);
/// they are sorted internally. `payload_hash` is the hex SHA-256 of the
/// body, or `UNSIGNED-PAYLOAD` for S3 HTTPS requests.
#[allow(clippy::too_many_arguments)]
fn sigv4_authorization(
    service: &str,
    region: &str,
    key_id: &str,
    secret: &str,
    method: &str,
    path: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    amz_date: &str,
    payload_hash: &str,
) -> String {
    let date_stamp = &amz_date[..8];
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");

    let mut params: Vec<(String, String)> = query.to_vec();
    params.sort();
    let canonical_query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v)))
        .collect();

    let mut signed: Vec<(String, String)> = headers.to_vec();
    signed.sort();
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers: Vec<&str> = signed.iter().map(|(k, _)| k.as_str()).collect();
    let signed_headers = signed_headers.join(";");
    let canonical_request = format!(
        "{method}\n{path}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        canonical_query.join("&")
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

#[derive(serde::Deserialize)]
struct ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<Contents>,
    #[serde(rename = "IsTruncated", default)]
    is_truncated: bool,
    #[serde(rename = "NextContinuationToken")]
    next_continuation_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct Contents {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "ETag")]
    etag: Option<String>,
}

#[derive(serde::Deserialize)]
struct ErrorResponse {
    #[serde(rename = "Message")]
    message: Option<String>,
    /// Bucket's actual region, present on 301 `PermanentRedirect` errors.
    #[serde(rename = "Region")]
    region: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's published SigV4 example (GET iam.amazonaws.com/?Action=ListUsers)
    /// pins the canonical-request/signature pipeline end to end.
    #[test]
    fn sigv4_matches_aws_documented_signature() {
        let query = vec![
            ("Action".to_string(), "ListUsers".to_string()),
            ("Version".to_string(), "2010-05-08".to_string()),
        ];
        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ];
        let auth = sigv4_authorization(
            "iam",
            "us-east-1",
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "GET",
            "/",
            &query,
            &headers,
            "20150830T123600Z",
            &sha256_hex(b""),
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn parses_s3_uris() {
        let uri = S3Uri::parse("s3://my-bucket/exports/cloudbridge/").unwrap();
        assert_eq!(uri.bucket, "my-bucket");
        assert_eq!(uri.prefix, "exports/cloudbridge");

        let bare = S3Uri::parse("s3://my-bucket").unwrap();
        assert_eq!(bare.prefix, "");

        assert!(S3Uri::parse("https://my-bucket/x").is_err());
        assert!(S3Uri::parse("s3://").is_err());
    }

    #[test]
    fn uri_encode_leaves_unreserved_and_encodes_rest() {
        assert_eq!(uri_encode("abc-DEF_019.~"), "abc-DEF_019.~");
        assert_eq!(uri_encode("a/b c+d"), "a%2Fb%20c%2Bd");
        assert_eq!(
            uri_encode_path("exports/BILLING_PERIOD=2026-09/part 0.parquet"),
            "exports/BILLING_PERIOD%3D2026-09/part%200.parquet"
        );
    }

    #[test]
    fn parses_list_objects_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <IsTruncated>false</IsTruncated>
              <Contents><Key>data/BILLING_PERIOD=2026-09/a.parquet</Key><Size>1024</Size><ETag>"abc"</ETag></Contents>
              <Contents><Key>data/BILLING_PERIOD=2026-09/b.parquet</Key><Size>20</Size><ETag>"def"</ETag></Contents>
            </ListBucketResult>"#;
        let page: ListBucketResult = quick_xml::de::from_str(xml).unwrap();
        assert!(!page.is_truncated);
        assert_eq!(page.contents.len(), 2);
        assert_eq!(
            page.contents[0].key,
            "data/BILLING_PERIOD=2026-09/a.parquet"
        );
        assert_eq!(page.contents[0].size, 1024);
        assert_eq!(page.contents[0].etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn parses_error_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Error><Code>SignatureDoesNotMatch</Code><Message>nope</Message></Error>"#;
        let parsed: ErrorResponse = quick_xml::de::from_str(xml).unwrap();
        assert_eq!(parsed.message.as_deref(), Some("nope"));
        assert_eq!(parsed.region, None);

        // 301 PermanentRedirect carries the bucket's actual region.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Error><Code>PermanentRedirect</Code><Message>redirect</Message><Region>ap-east-1</Region></Error>"#;
        let parsed: ErrorResponse = quick_xml::de::from_str(xml).unwrap();
        assert_eq!(parsed.region.as_deref(), Some("ap-east-1"));
    }
}
