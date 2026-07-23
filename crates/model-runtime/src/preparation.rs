//! Typed outcomes from preparing one provider-bound request capability.

use crate::credential::CredentialAccessError;

/// The result of preparing one opaque, one-shot provider request capability.
///
/// Preparation performs every locally knowable operation before the caller
/// authorizes the physical interaction (docs/spec/runtime-substrate.md).
/// Only [`Prepared`](Self::Prepared) authorizes a later call to
/// [`crate::ModelRuntime::execute`].
#[must_use]
pub enum PreparationOutcome<C, P> {
    /// A complete, authenticated request capability ready for authorization.
    Prepared(P),
    /// The caller cancelled before a capability was created.
    Cancelled {
        /// The caller-supplied operation identity, returned verbatim.
        correlation: C,
    },
    /// A trustworthy ordinary preparation failure.
    Failed {
        /// The caller-supplied operation identity, returned verbatim.
        correlation: C,
        /// Why the operation could not be prepared.
        failure: PreparationFailure,
    },
    /// An adapter defect prevented trustworthy preparation.
    Defect {
        /// The caller-supplied operation identity, returned verbatim.
        correlation: C,
        /// The defect encountered before a capability could be created.
        defect: PreparationDefect,
    },
}

/// A trustworthy ordinary failure discovered before send authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparationFailure {
    /// The operation asks for something this adapter does not support.
    UnsupportedOperation {
        /// What the adapter does not support.
        detail: String,
    },
    /// The provider credential could not be read during request preparation.
    /// The reference-only access error is safe to return across the adapter
    /// boundary; it never contains credential material.
    CredentialUnavailable {
        /// The reference-only access failure.
        error: CredentialAccessError,
    },
    /// A resolved credential cannot authenticate the constructed request.
    CredentialUnusable {
        /// Why the value cannot be used. Never contains the value.
        detail: String,
    },
}

/// A local adapter defect discovered before send authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparationDefect {
    /// A provider wire value could not be serialized.
    SerializationFailed {
        /// The serializer's rendered description.
        detail: String,
    },
    /// The complete provider request or its adapter configuration could not
    /// be turned into a one-shot request capability.
    RequestConstructionFailed {
        /// The construction failure's rendered description.
        detail: String,
    },
}
