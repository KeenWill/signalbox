//! Adapter configuration.

use std::time::Duration;

/// An Anthropic API key.
///
/// Debug output is redacted and the value is only ever attached to requests
/// as a sensitivity-marked header; this crate never logs (ADR-0017's
/// credential boundary).
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wraps a key value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn value(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(redacted)")
    }
}

/// Configuration for [`crate::AnthropicRuntime`].
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// The API key sent as the `x-api-key` header.
    pub api_key: ApiKey,
    /// Base URL of the API; the adapter appends `/v1/messages`.
    pub base_url: String,
    /// The `anthropic-version` header value.
    pub anthropic_version: String,
    /// Connection-establishment timeout, when the caller sets one. A connect
    /// timeout fires before any request byte is written, so it classifies as
    /// proven-unsent.
    pub connect_timeout: Option<Duration>,
    /// Whole-exchange timeout, when the caller sets one. It covers the full
    /// exchange including body or stream delivery; firing after send is
    /// boundary-loss evidence (ADR-0043 timeout rule). ADR-0043 selects no
    /// timeout budget, so the default is none and the caller owns any budget.
    pub exchange_timeout: Option<Duration>,
    /// Upper bound on one SSE record's size; larger records are
    /// stream-protocol-violation evidence.
    pub sse_record_limit: usize,
}

impl AnthropicConfig {
    /// Configuration carrying the required key; every other field takes the
    /// documented default (public API base URL, version `2023-06-01`, no
    /// timeouts, 8 MiB SSE record limit).
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            api_key,
            base_url: "https://api.anthropic.com".to_string(),
            anthropic_version: "2023-06-01".to_string(),
            connect_timeout: None,
            exchange_timeout: None,
            sse_record_limit: 8 * 1024 * 1024,
        }
    }
}
