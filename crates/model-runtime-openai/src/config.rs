//! Adapter configuration.

use std::time::Duration;

/// Configuration for [`crate::OpenAiRuntime`].
///
/// Carries no credential: the operation pins a non-secret
/// `CredentialReference`, and the runtime resolves its current value through
/// the caller-supplied `CredentialAccess` implementation during send
/// preparation of each physical request
/// (`docs/spec/configuration-and-credentials.md`).
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Base URL of the API; the adapter appends `/v1/chat/completions`. The
    /// scheme must be `https`, except that `http` is admitted for a literal
    /// loopback IP host. User information, query, and fragment are rejected.
    pub base_url: String,
    /// Connection-establishment timeout, when the caller sets one. A connect
    /// timeout fires before any request byte is written, so it classifies as
    /// proven-unsent.
    pub connect_timeout: Option<Duration>,
    /// Positive whole-exchange timeout. It covers the full exchange
    /// including body or stream delivery; firing after send is boundary-loss
    /// evidence under the timeout rule in `docs/spec/runtime-substrate.md`.
    pub exchange_timeout: Duration,
    /// Upper bound on one SSE record's size; larger records are
    /// stream-protocol-violation evidence. Must be greater than zero.
    pub sse_record_limit: usize,
}

impl OpenAiConfig {
    /// The documented defaults: public API base URL, no connect timeout,
    /// 10-minute exchange timeout, 8 MiB SSE record limit.
    pub fn new() -> Self {
        Self {
            base_url: "https://api.openai.com".to_string(),
            connect_timeout: None,
            exchange_timeout: Duration::from_secs(10 * 60),
            sse_record_limit: 8 * 1024 * 1024,
        }
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self::new()
    }
}
