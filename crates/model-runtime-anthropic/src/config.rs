//! Adapter configuration.

use std::time::Duration;

/// Configuration for [`crate::AnthropicRuntime`].
///
/// Carries no credential: the operation pins a non-secret
/// `CredentialReference`, and the runtime resolves its current value through
/// the caller-supplied `CredentialAccess` implementation during send
/// preparation of each physical request
/// (`docs/spec/configuration-and-credentials.md`).
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Base URL of the API; the adapter appends `/v1/messages`. The scheme
    /// must be `https`, except that `http` is admitted for a literal
    /// loopback IP host. User information, query, and fragment are rejected.
    pub base_url: String,
    /// The `anthropic-version` header value.
    pub anthropic_version: String,
    /// Connection-establishment timeout, when the caller sets one. A connect
    /// timeout fires before any request byte is written, so it classifies as
    /// proven-unsent.
    pub connect_timeout: Option<Duration>,
    /// Positive whole-exchange timeout. It covers the full exchange
    /// including body or stream delivery; firing after send is boundary-loss
    /// evidence under the timeout rule in `docs/spec/runtime-substrate.md`.
    pub exchange_timeout: Duration,
    /// Positive upper bound on one SSE record's size; zero is rejected at
    /// construction and larger records are stream-protocol-violation
    /// evidence.
    pub sse_record_limit: usize,
}

impl AnthropicConfig {
    /// The documented defaults: public API base URL, version `2023-06-01`,
    /// no connect timeout, 10-minute exchange timeout, 8 MiB SSE record
    /// limit.
    pub fn new() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            anthropic_version: "2023-06-01".to_string(),
            connect_timeout: None,
            exchange_timeout: Duration::from_secs(10 * 60),
            sse_record_limit: 8 * 1024 * 1024,
        }
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self::new()
    }
}
