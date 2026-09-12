//! Provider-neutral document references, bounded inputs, and presentation capabilities.

use std::{collections::BTreeSet, num::NonZeroU64, sync::Arc};

/// A rendered durable result naming validated immutable document bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentReference {
    /// Opaque caller-owned identity authenticating the durable result.
    pub authority: String,
    /// SHA-256 identity of the presented bytes.
    pub digest: [u8; 32],
    /// Exact encoded source length, checked before materialization.
    pub byte_length: NonZeroU64,
    /// Canonical type authenticated by the caller's persisted validation evidence.
    pub media_type: String,
}

/// Bounded document bytes materialized after reference authentication.
#[derive(Clone, Eq, PartialEq)]
pub struct DocumentInput {
    /// Canonical authenticated type; adapters compare it with their capability set.
    pub media_type: String,
    /// Immutable encoded document bytes, never decoded by a model adapter.
    pub bytes: Arc<[u8]>,
}

impl std::fmt::Debug for DocumentInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DocumentInput")
            .field("media_type", &self.media_type)
            .field("byte_length", &self.bytes.len())
            .finish()
    }
}

/// Accepted document types and finite materialized and complete-wire request bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentPresentationCapability {
    media_types: BTreeSet<String>,
    maximum_document_bytes: u64,
    maximum_request_bytes: usize,
    envelope_bytes: usize,
}

impl DocumentPresentationCapability {
    /// Constructs adapter limits with the largest serialized empty-document envelope.
    pub fn new(
        media_types: BTreeSet<String>,
        maximum_document_bytes: NonZeroU64,
        maximum_request_bytes: std::num::NonZeroUsize,
        envelope_bytes: usize,
    ) -> Self {
        Self {
            media_types,
            maximum_document_bytes: maximum_document_bytes.get(),
            maximum_request_bytes: maximum_request_bytes.get(),
            envelope_bytes,
        }
    }
    /// Lowers adapter limits by the configured deployment bounds.
    pub fn limited_by(mut self, document_bytes: u64, request_bytes: usize) -> Self {
        self.maximum_document_bytes = self.maximum_document_bytes.min(document_bytes);
        self.maximum_request_bytes = self.maximum_request_bytes.min(request_bytes);
        self
    }
    /// Checks one reference before source I/O, including base64 and document framing.
    pub fn admits(&self, media_type: &str, byte_length: u64) -> bool {
        self.media_types.contains(media_type)
            && byte_length > 0
            && byte_length <= self.maximum_document_bytes
            && byte_length
                .checked_add(2)
                .and_then(|length| (length / 3).checked_mul(4))
                .and_then(|length| length.checked_add(self.envelope_bytes as u64))
                .is_some_and(|length| length <= self.maximum_request_bytes as u64)
    }
    /// Returns the complete encoded provider request bound.
    pub const fn maximum_request_bytes(&self) -> usize {
        self.maximum_request_bytes
    }
    /// Returns the materialized-byte bound for one document.
    pub const fn maximum_document_bytes(&self) -> u64 {
        self.maximum_document_bytes
    }
}

/// Checks materialized inputs before encoding and returns the complete request limit.
pub fn document_request_byte_limit<C>(
    operation: &crate::ModelOperation<C>,
    adapter: &DocumentPresentationCapability,
) -> Result<Option<usize>, crate::PreparationFailure> {
    let unsupported = || crate::PreparationFailure::UnsupportedOperation {
        detail: String::from(
            "document presentation is unsupported or exceeds its configured bounds",
        ),
    };
    let mut limit = None;
    let mut aggregate = 0_u64;
    for part in operation.messages.iter().flat_map(|message| &message.parts) {
        match part {
            crate::MessagePart::DocumentReference(_) => return Err(unsupported()),
            crate::MessagePart::Document(document) => {
                let configured = operation
                    .document_presentation
                    .as_ref()
                    .ok_or_else(unsupported)?;
                if !configured.admits(&document.media_type, document.bytes.len() as u64)
                    || !adapter.admits(&document.media_type, document.bytes.len() as u64)
                {
                    return Err(unsupported());
                }
                let request_limit = configured
                    .maximum_request_bytes()
                    .min(adapter.maximum_request_bytes());
                aggregate = aggregate
                    .checked_add(
                        (document.bytes.len() as u64)
                            .checked_add(2)
                            .and_then(|length| (length / 3).checked_mul(4))
                            .ok_or_else(unsupported)?,
                    )
                    .ok_or_else(unsupported)?;
                if aggregate > request_limit as u64 {
                    return Err(unsupported());
                }
                limit = Some(request_limit);
            }
            _ => {}
        }
    }
    Ok(limit)
}
