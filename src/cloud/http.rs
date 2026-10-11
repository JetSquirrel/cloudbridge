//! The HTTP clients the billing sources make their requests with.
//!
//! Every client has a timeout: a provider that stops answering has to fail
//! the refresh rather than hold it, and with it the period's overlap guard,
//! indefinitely.

use std::time::Duration;

use ureq::Agent;

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
