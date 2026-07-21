//! Adapter configuration.

use std::sync::Arc;
use std::time::Duration;

/// An Anthropic API key value.
///
/// Debug output is redacted and the value is only ever attached to one
/// request as a sensitivity-marked header; this crate never logs (ADR-0017's
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

/// Why a credential value could not be read. Never carries the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialUnavailable {
    /// Why the read failed.
    pub detail: String,
}

/// Supplies the API key during send preparation of one operation.
///
/// ADR-0017: the credential value flows to the adapter only during send
/// preparation of a specific call. The runtime therefore resolves the key
/// through this source once per executed operation, scopes the value to
/// that request, and caches nothing — after a rotation, the next operation
/// reads the current value. A failed read is typed proven-unsent
/// preparation-failure evidence, never a retry.
pub trait ApiKeySource: Send + Sync + std::fmt::Debug {
    /// The current key value, read for exactly one request.
    fn current(&self) -> Result<ApiKey, CredentialUnavailable>;
}

/// A fixed API key, for compositions whose key lives for the process (and
/// for tests).
#[derive(Debug, Clone)]
pub struct StaticApiKey(ApiKey);

impl StaticApiKey {
    /// Wraps a fixed key.
    pub fn new(api_key: ApiKey) -> Self {
        Self(api_key)
    }
}

impl ApiKeySource for StaticApiKey {
    fn current(&self) -> Result<ApiKey, CredentialUnavailable> {
        Ok(self.0.clone())
    }
}

/// Configuration for [`crate::AnthropicRuntime`].
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Source of the `x-api-key` header value, read once per operation
    /// during send preparation (ADR-0017).
    pub credentials: Arc<dyn ApiKeySource>,
    /// Base URL of the API; the adapter appends `/v1/messages`. The scheme
    /// must be `http` or `https`.
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
    /// Configuration carrying a fixed key; every other field takes the
    /// documented default (public API base URL, version `2023-06-01`, no
    /// timeouts, 8 MiB SSE record limit).
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            credentials: Arc::new(StaticApiKey::new(api_key)),
            base_url: "https://api.anthropic.com".to_string(),
            anthropic_version: "2023-06-01".to_string(),
            connect_timeout: None,
            exchange_timeout: None,
            sse_record_limit: 8 * 1024 * 1024,
        }
    }
}
