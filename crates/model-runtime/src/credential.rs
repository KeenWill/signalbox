//! Provider-neutral credential access boundary (ADR-0017).

use std::future::Future;

/// The non-secret durable name of one provider credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialReference(String);

impl CredentialReference {
    /// Wraps the reference exactly as pinned by the caller.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The non-secret reference text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CredentialReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Secret credential bytes scoped to authenticating one physical request.
///
/// This boundary value deliberately implements neither `Display` nor
/// serialization. Its `Debug` representation is always redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialValue(Vec<u8>);

impl CredentialValue {
    /// Wraps bytes read by a credential-access implementation.
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    /// Exposes the bytes to the adapter constructing request authentication.
    pub fn expose_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for CredentialValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialValue([REDACTED])")
    }
}

/// Why a credential reference could not be resolved during preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialAccessFailure {
    /// No delivery mapping exists for the reference.
    Unmapped,
    /// The delivery artifact is currently absent or inaccessible.
    Unavailable,
    /// The delivery artifact was present but could not be read as a value.
    Unreadable,
}

/// A reference-only credential-access failure; never contains secret bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialAccessError {
    /// The non-secret reference that failed resolution.
    pub reference: CredentialReference,
    /// The typed failure class.
    pub failure: CredentialAccessFailure,
}

impl CredentialAccessError {
    /// Creates a sanitized access failure for one reference.
    pub fn new(reference: CredentialReference, failure: CredentialAccessFailure) -> Self {
        Self { reference, failure }
    }
}

impl std::fmt::Display for CredentialAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "credential reference `{}` could not be resolved: {:?}",
            self.reference, self.failure
        )
    }
}

impl std::error::Error for CredentialAccessError {}

/// Resolves a pinned credential reference during preparation of one physical
/// request.
///
/// Adapters call this for every request rather than caching a value, so
/// mounted-secret rotation is visible without restarting the hub (ADR-0017).
pub trait CredentialAccess: Send + Sync {
    /// Reads the current value for `reference`.
    fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> impl Future<Output = Result<CredentialValue, CredentialAccessError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::{
        CredentialAccessError, CredentialAccessFailure, CredentialReference, CredentialValue,
    };

    /// INV-035: credential boundary values have a redacted diagnostic shape.
    #[test]
    fn inv_035_credential_value_debug_is_redacted() {
        let secret = CredentialValue::new(b"do-not-print".to_vec());

        let diagnostic = format!("{secret:?}");

        assert_eq!(diagnostic, "CredentialValue([REDACTED])");
    }

    /// INV-035: access errors carry only the safe reference and failure class.
    #[test]
    fn inv_035_access_error_is_reference_only() {
        let reference = CredentialReference::new("anthropic-primary");
        let error =
            CredentialAccessError::new(reference.clone(), CredentialAccessFailure::Unavailable);

        assert_eq!(error.reference, reference);
        assert_eq!(error.failure, CredentialAccessFailure::Unavailable);
    }
}
