//! Adapter configuration.

use std::{collections::BTreeSet, time::Duration};

/// Configuration for [`crate::AnthropicRuntime`].
///
/// Carries no credential: the operation pins a non-secret
/// `CredentialReference`, and the runtime resolves its current value through
/// the caller-supplied `CredentialAccess` implementation during send
/// preparation of each physical request
/// (`docs/spec/configuration-and-credentials.md`).
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Exact per-model reasoning, fast-mode, and service-tier capabilities.
    pub model_capabilities: signalbox_model_runtime::ModelCapabilityCatalog,
    /// Exact resolved provider-model targets that admit Anthropic server
    /// compaction and replay of its opaque response blocks.
    pub provider_compaction_targets: BTreeSet<String>,
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
    /// Optional positive whole-exchange timeout. It covers the full exchange
    /// including body or stream delivery; firing after send is boundary-loss
    /// evidence under the timeout rule in `docs/spec/runtime-substrate.md`.
    pub exchange_timeout: Option<Duration>,
    /// Positive upper bound on one SSE record's size; zero is rejected at
    /// construction and larger records are stream-protocol-violation
    /// evidence.
    pub sse_record_limit: usize,
    /// Maximum retained provider-native diagnostic message bytes, or
    /// unbounded when explicitly configured as `none`.
    pub native_message_limit: Option<usize>,
}

impl AnthropicConfig {
    /// Builds the baseline provider configuration with the caller-supplied
    /// native-message retention policy.
    pub fn new(native_message_limit: Option<usize>) -> Self {
        Self {
            model_capabilities: signalbox_model_runtime::ModelCapabilityCatalog::empty(),
            provider_compaction_targets: BTreeSet::new(),
            base_url: "https://api.anthropic.com".to_string(),
            anthropic_version: "2023-06-01".to_string(),
            connect_timeout: None,
            exchange_timeout: None,
            sse_record_limit: 8 * 1024 * 1024,
            native_message_limit,
        }
    }
}
