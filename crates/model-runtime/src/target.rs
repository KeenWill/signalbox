//! The three separate provider-target facts.
//!
//! ADR-0005 requires that the requested selection, the hub-resolved exact
//! pinned target, and the provider-reported model identity remain three
//! separate facts. These newtypes keep them from being confused in
//! signatures. The runtime reports a [`ProviderReportedModel`] when one is
//! observable and fabricates neither a match nor a mismatch when none is;
//! comparing it against the resolved target is the caller's classification
//! work, never the runtime's.

/// The caller's original model selection, before hub resolution.
///
/// Carried verbatim for provenance; adapters never send this value to a
/// provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedTarget(String);

impl RequestedTarget {
    /// Wraps the caller's requested selection exactly as stated.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The requested selection as stated by the caller.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The hub-resolved exact provider model identifier this operation must use.
///
/// Adapters send exactly this value as the provider's model parameter and
/// never substitute another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget(String);

impl ResolvedTarget {
    /// Wraps the exact resolved provider model identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The exact model identifier sent to the provider.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The model identity the provider itself reported in a response.
///
/// Evidence only: recorded exactly as observed, absent when the provider
/// reported none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReportedModel(String);

impl ProviderReportedModel {
    /// Wraps the provider-reported identity exactly as observed.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The provider-reported identity as observed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
