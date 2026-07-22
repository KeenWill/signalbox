//! Adapter configuration.

use std::time::Duration;

/// Configuration for [`crate::OpenAiRuntime`].
///
/// Carries no credential: the operation pins a non-secret
/// `CredentialReference`, and the runtime resolves its current value through
/// the caller-supplied `CredentialAccess` implementation during send
/// preparation of each physical request (ADR-0017).
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Base URL of the API; the adapter appends `/v1/chat/completions`. The
    /// scheme must be `http` or `https`, with no user information, query, or
    /// fragment.
    pub base_url: String,
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

impl OpenAiConfig {
    /// The documented defaults: public API base URL, no timeouts, 8 MiB SSE
    /// record limit.
    pub fn new() -> Self {
        Self {
            base_url: "https://api.openai.com".to_string(),
            connect_timeout: None,
            exchange_timeout: None,
            sse_record_limit: 8 * 1024 * 1024,
        }
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self::new()
    }
}
