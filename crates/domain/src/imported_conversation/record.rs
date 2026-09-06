//! Converted raw records and transcript entry inputs for `docs/spec/conversation-import.md`.

use super::content::ImportedSourceMetadata;
use super::content::ImportedSpeaker;
use super::content::ImportedTranscriptContent;
use super::digest::ImportedRawRecordConversionDigest;
use super::digest::ImportedRawRecordHash;
use super::position::ImportedRawRecordPosition;
use super::position::ImportedRecordEntryPosition;
use super::position::ImportedTranscriptPosition;
use super::structured_value::ImportedSourceAttestation;
use super::structured_value::ImportedStructuredValue;
use crate::ImportedConversationId;
use crate::ImportedTranscriptEntryId;
use std::fmt;

/// One converted raw record with exact bytes and complete normalized JSON.
#[derive(Clone, Eq, PartialEq)]
pub struct ImportedRawSourceRecord {
    pub(super) content_hash: ImportedRawRecordHash,
    pub(super) conversion_digest: ImportedRawRecordConversionDigest,
    pub(super) bytes: Box<[u8]>,
    pub(super) normalized: ImportedStructuredValue,
}

impl ImportedRawSourceRecord {
    /// Hashes and retains one exact converted source record.
    pub fn from_converted(bytes: Vec<u8>, normalized: ImportedStructuredValue) -> Self {
        let content_hash = ImportedRawRecordHash::digest(&bytes);
        Self {
            content_hash,
            conversion_digest: ImportedRawRecordConversionDigest::derive(content_hash, &normalized),
            bytes: bytes.into_boxed_slice(),
            normalized,
        }
    }

    /// Returns the exact-byte content hash.
    pub const fn content_hash(&self) -> ImportedRawRecordHash {
        self.content_hash
    }

    /// Returns the raw-to-normalized conversion digest.
    pub const fn conversion_digest(&self) -> ImportedRawRecordConversionDigest {
        self.conversion_digest
    }

    /// Borrows the exact source-record bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrows the complete normalized source object.
    pub const fn normalized(&self) -> &ImportedStructuredValue {
        &self.normalized
    }
}

impl fmt::Debug for ImportedRawSourceRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedRawSourceRecord")
            .field("content_hash", &self.content_hash)
            .field("conversion_digest", &self.conversion_digest)
            .field("byte_len", &self.bytes.len())
            .field("normalized", &"<redacted>")
            .finish()
    }
}

/// Stored fields for one raw-record reconstitution boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct ImportedRawSourceRecordReconstitutionInput {
    pub(super) position: ImportedRawRecordPosition,
    pub(super) stored_hash: ImportedRawRecordHash,
    pub(super) stored_conversion_digest: ImportedRawRecordConversionDigest,
    pub(super) bytes: Box<[u8]>,
    pub(super) normalized: ImportedStructuredValue,
}

impl ImportedRawSourceRecordReconstitutionInput {
    /// Supplies one complete stored raw record.
    pub fn new(
        position: ImportedRawRecordPosition,
        stored_hash: ImportedRawRecordHash,
        stored_conversion_digest: ImportedRawRecordConversionDigest,
        bytes: Vec<u8>,
        normalized: ImportedStructuredValue,
    ) -> Self {
        Self {
            position,
            stored_hash,
            stored_conversion_digest,
            bytes: bytes.into_boxed_slice(),
            normalized,
        }
    }

    /// Returns the physical source-record position.
    pub const fn position(&self) -> ImportedRawRecordPosition {
        self.position
    }

    /// Returns the stored raw content hash.
    pub const fn stored_hash(&self) -> ImportedRawRecordHash {
        self.stored_hash
    }

    /// Returns the stored raw-to-normalized conversion digest.
    pub const fn stored_conversion_digest(&self) -> ImportedRawRecordConversionDigest {
        self.stored_conversion_digest
    }

    /// Borrows the exact stored record bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrows the complete normalized source object.
    pub const fn normalized(&self) -> &ImportedStructuredValue {
        &self.normalized
    }
}

impl fmt::Debug for ImportedRawSourceRecordReconstitutionInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedRawSourceRecordReconstitutionInput")
            .field("position", &self.position)
            .field("stored_hash", &self.stored_hash)
            .field("stored_conversion_digest", &self.stored_conversion_digest)
            .field("byte_len", &self.bytes.len())
            .field("normalized", &"<redacted>")
            .finish()
    }
}

/// Complete typed fields for one normalized imported entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedTranscriptEntryInput {
    pub(super) identity: ImportedTranscriptEntryId,
    pub(super) conversation: ImportedConversationId,
    pub(super) position: ImportedTranscriptPosition,
    pub(super) raw_record_position: ImportedRawRecordPosition,
    pub(super) record_entry_position: ImportedRecordEntryPosition,
    pub(super) source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
    pub(super) content: ImportedTranscriptContent,
    pub(super) source: ImportedSourceMetadata,
}

impl ImportedTranscriptEntryInput {
    /// Supplies one complete normalized imported entry.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        identity: ImportedTranscriptEntryId,
        conversation: ImportedConversationId,
        position: ImportedTranscriptPosition,
        raw_record_position: ImportedRawRecordPosition,
        record_entry_position: ImportedRecordEntryPosition,
        source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
        content: ImportedTranscriptContent,
        source: ImportedSourceMetadata,
    ) -> Self {
        Self {
            identity,
            conversation,
            position,
            raw_record_position,
            record_entry_position,
            source_speaker,
            content,
            source,
        }
    }

    /// Returns the imported-entry identity.
    pub const fn identity(&self) -> ImportedTranscriptEntryId {
        self.identity
    }

    /// Returns the claimed owning conversation.
    pub const fn conversation(&self) -> ImportedConversationId {
        self.conversation
    }

    /// Returns the global imported position.
    pub const fn position(&self) -> ImportedTranscriptPosition {
        self.position
    }

    /// Returns the owning raw-record occurrence.
    pub const fn raw_record_position(&self) -> ImportedRawRecordPosition {
        self.raw_record_position
    }

    /// Returns the position within that raw record.
    pub const fn record_entry_position(&self) -> ImportedRecordEntryPosition {
        self.record_entry_position
    }

    /// Borrows the source-speaker attestation.
    pub const fn source_speaker(&self) -> &ImportedSourceAttestation<ImportedSpeaker> {
        &self.source_speaker
    }

    /// Borrows the maximum-fidelity normalized content.
    pub const fn content(&self) -> &ImportedTranscriptContent {
        &self.content
    }

    /// Borrows the complete source metadata.
    pub const fn source(&self) -> &ImportedSourceMetadata {
        &self.source
    }
}
