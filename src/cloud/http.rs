//! The HTTP clients the billing sources make their requests with, and what
//! their failures mean to the caller.
//!
//! Every client has a timeout: a provider that stops answering has to fail
//! the refresh rather than hold it, and with it the period's overlap guard,
//! indefinitely.

use std::fmt;
use std::time::Duration;

use ureq::Agent;

/// What a caller should do about a failed provider call.
///
/// Sorted by the remedy, not by which provider or library failed: the
/// background schedule retries a [`Temporary`](Self::Temporary) failure
/// soon, while a [`Denied`](Self::Denied) one waits for someone to change
/// the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The provider refused the credentials, or what they are allowed to
    /// do. Asking again changes nothing until someone edits the account.
    Denied,
    /// The provider was unreachable, slow, throttling or failing on its own
    /// side. It clears without anyone doing anything.
    Temporary,
}

/// A provider call that failed in a way [`FailureKind`] names. Displays as
/// its message alone, so classifying a failure never changes what is shown.
#[derive(Debug)]
pub struct ProviderError {
    pub kind: FailureKind,
    message: String,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

/// An error reading `message`, classified as `kind` when there is one.
pub fn failure(kind: Option<FailureKind>, message: String) -> anyhow::Error {
    match kind {
        Some(kind) => ProviderError { kind, message }.into(),
        None => anyhow::Error::msg(message),
    }
}

/// A request that got no usable response, as `what: <ureq's reason>`.
pub fn request_failed(what: &str, error: ureq::Error) -> anyhow::Error {
    failure(transport_kind(&error), format!("{what}: {error}"))
}

/// A response with an error status, as `message`.
pub fn status_failed(status: u16, message: String) -> anyhow::Error {
    failure(status_kind(status), message)
}

/// What an HTTP error status means to the caller, when it says.
pub fn status_kind(status: u16) -> Option<FailureKind> {
    match status {
        401 | 403 => Some(FailureKind::Denied),
        408 | 429 | 500..=599 => Some(FailureKind::Temporary),
        _ => None,
    }
}

fn transport_kind(error: &ureq::Error) -> Option<FailureKind> {
    match error {
        ureq::Error::StatusCode(status) => status_kind(*status),
        ureq::Error::Timeout(_)
        | ureq::Error::Io(_)
        | ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::BodyStalled => Some(FailureKind::Temporary),
        _ => None,
    }
}

/// The kind of the first classified failure in `error`'s chain: a
/// [`ProviderError`], an S3 error status, or a ureq error kept as it was.
pub fn kind_of(error: &anyhow::Error) -> Option<FailureKind> {
    error.chain().find_map(|cause| {
        if let Some(e) = cause.downcast_ref::<ProviderError>() {
            Some(e.kind)
        } else if let Some(e) = cause.downcast_ref::<super::s3::S3Error>() {
            status_kind(e.status)
        } else {
            cause.downcast_ref::<ureq::Error>().and_then(transport_kind)
        }
    })
}

/// How long one provider API call may take.
pub const API_TIMEOUT: Duration = Duration::from_secs(30);

/// A client that hands back every response, error statuses included, so the
/// caller can read the provider's own error body.
pub fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .new_agent()
}

/// A client for which a non-2xx status is an `Err`, for callers with no
/// error body worth reading.
pub fn strict_agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .new_agent()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_says_who_has_to_act() {
        assert_eq!(status_kind(401), Some(FailureKind::Denied));
        assert_eq!(status_kind(403), Some(FailureKind::Denied));
        assert_eq!(status_kind(429), Some(FailureKind::Temporary));
        assert_eq!(status_kind(503), Some(FailureKind::Temporary));
        assert_eq!(status_kind(400), None);
        assert_eq!(status_kind(404), None);
    }

    #[test]
    fn a_classified_failure_reads_as_its_message() {
        let error = status_failed(403, "Cloudflare API request failed: HTTP 403".into());
        assert_eq!(error.to_string(), "Cloudflare API request failed: HTTP 403");
        assert_eq!(kind_of(&error), Some(FailureKind::Denied));
    }

    #[test]
    fn the_kind_survives_context() {
        let error =
            request_failed("STS request failed", ureq::Error::ConnectionFailed).context("Sep 2026");
        assert_eq!(kind_of(&error), Some(FailureKind::Temporary));
    }

    #[test]
    fn an_unclassified_failure_has_no_kind() {
        assert_eq!(kind_of(&anyhow::anyhow!("Failed to parse")), None);
        assert_eq!(
            kind_of(&request_failed("bad", ureq::Error::BadUri("x".into()))),
            None
        );
    }
}
