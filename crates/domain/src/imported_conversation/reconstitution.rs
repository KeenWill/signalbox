//! Imported conversation reconstitution inputs and failures for `docs/spec/conversation-import.md`.

use super::conversation::ImportedConversation;
use super::conversation::build_conversation;
use super::digest::ImportedConversationSourceDigest;
use super::format::ImportedConversationFormat;
use super::position::ImportedRawRecordPosition;
use super::position::ImportedRecordEntryPosition;
use super::position::ImportedTranscriptPosition;
use super::record::ImportedRawSourceRecordReconstitutionInput;
use super::record::ImportedTranscriptEntryInput;
use super::validation::validate_reconstitution;
use crate::ImportedConversationId;
use crate::ImportedTranscriptEntryId;

/// Complete stored fields for imported-conversation reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationReconstitutionInput {
    pub(super) requested_conversation: ImportedConversationId,
    pub(super) stored_conversation: ImportedConversationId,
    pub(super) format: ImportedConversationFormat,
    pub(super) stored_source_digest: ImportedConversationSourceDigest,
    pub(super) declared_raw_record_count: u64,
    pub(super) raw_records: Vec<ImportedRawSourceRecordReconstitutionInput>,
    pub(super) declared_entry_count: u64,
    pub(super) entries: Vec<ImportedTranscriptEntryInput>,
}

impl ImportedConversationReconstitutionInput {
    /// Supplies one complete stored imported-conversation projection.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        requested_conversation: ImportedConversationId,
        stored_conversation: ImportedConversationId,
        format: ImportedConversationFormat,
        stored_source_digest: ImportedConversationSourceDigest,
        declared_raw_record_count: u64,
        raw_records: Vec<ImportedRawSourceRecordReconstitutionInput>,
        declared_entry_count: u64,
        entries: Vec<ImportedTranscriptEntryInput>,
    ) -> Self {
        Self {
            requested_conversation,
            stored_conversation,
            format,
            stored_source_digest,
            declared_raw_record_count,
            raw_records,
            declared_entry_count,
            entries,
        }
    }

    /// Returns the conversation requested by the caller.
    pub const fn requested_conversation(&self) -> ImportedConversationId {
        self.requested_conversation
    }

    /// Returns the identity stored on the header.
    pub const fn stored_conversation(&self) -> ImportedConversationId {
        self.stored_conversation
    }

    /// Returns the closed source format and converter version.
    pub const fn format(&self) -> ImportedConversationFormat {
        self.format
    }

    /// Returns the stored ordered-source digest.
    pub const fn stored_source_digest(&self) -> ImportedConversationSourceDigest {
        self.stored_source_digest
    }

    /// Returns the header's raw-record count.
    pub const fn declared_raw_record_count(&self) -> u64 {
        self.declared_raw_record_count
    }

    /// Borrows every complete stored raw record.
    pub fn raw_records(&self) -> &[ImportedRawSourceRecordReconstitutionInput] {
        &self.raw_records
    }

    /// Returns the header's normalized-entry count.
    pub const fn declared_entry_count(&self) -> u64 {
        self.declared_entry_count
    }

    /// Borrows every complete stored entry.
    pub fn entries(&self) -> &[ImportedTranscriptEntryInput] {
        &self.entries
    }

    /// Reconstructs one complete immutable imported conversation.
    pub fn reconstitute(
        self,
    ) -> Result<ImportedConversation, ImportedConversationReconstitutionError> {
        if let Err(failure) = validate_reconstitution(&self) {
            return Err(ImportedConversationReconstitutionError {
                input: Box::new(self),
                failure,
            });
        }
        Ok(build_conversation(self))
    }
}

/// Why typed records cannot reconstruct one imported conversation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedConversationReconstitutionFailure {
    /// Requested and stored conversation identities differ.
    RequestedConversationMismatch,
    /// The raw source-record sequence was empty.
    EmptyRawRecords,
    /// The normalized entry sequence was empty.
    EmptyEntries,
    /// The stored header raw-record count disagrees with supplied records.
    DeclaredRawRecordCountMismatch {
        /// Stored header count.
        declared: u64,
        /// Supplied record count.
        actual: usize,
    },
    /// The stored header entry count disagrees with supplied entries.
    DeclaredEntryCountMismatch {
        /// Stored header count.
        declared: u64,
        /// Supplied entry count.
        actual: usize,
    },
    /// One raw-record occurrence did not occupy the next position.
    RawRecordPositionMismatch {
        /// Required position.
        expected: ImportedRawRecordPosition,
        /// Supplied position.
        actual: ImportedRawRecordPosition,
    },
    /// Exact raw bytes disagree with their stored content hash.
    RawRecordHashMismatch {
        /// Corrupt raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// A raw source-record occurrence was empty.
    EmptyRawRecord {
        /// Empty raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// Equal raw-record hashes named different exact bytes.
    RawRecordHashCollision {
        /// Later conflicting raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// A normalized source record disagreed with its stored conversion digest.
    RawRecordConversionDigestMismatch {
        /// Corrupt raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// A raw JSONL record did not normalize to one object.
    RawRecordNormalizedValueNotObject {
        /// Corrupt raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// A normalized raw record exceeded the format's container-depth bound.
    RawRecordStructuredValueDepthExceeded {
        /// Corrupt raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// A normalized raw record cannot produce the stored format's entry projection.
    RawRecordProjectionInvalid {
        /// Corrupt raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// The header digest disagrees with the format and ordered raw records.
    SourceDigestMismatch {
        /// Derived digest.
        expected: ImportedConversationSourceDigest,
        /// Stored digest.
        actual: ImportedConversationSourceDigest,
    },
    /// One entry names another imported conversation.
    EntryConversationMismatch {
        /// Cross-wired entry.
        entry: ImportedTranscriptEntryId,
    },
    /// One entry did not occupy the next global imported position.
    EntryPositionMismatch {
        /// Mispositioned entry.
        entry: ImportedTranscriptEntryId,
        /// Required position.
        expected: ImportedTranscriptPosition,
        /// Supplied position.
        actual: ImportedTranscriptPosition,
    },
    /// The same imported-entry identity appeared more than once.
    DuplicateEntry {
        /// Duplicated identity.
        entry: ImportedTranscriptEntryId,
    },
    /// One entry skipped or reversed a raw-record occurrence.
    EntryRawRecordPositionMismatch {
        /// Mispositioned entry.
        entry: ImportedTranscriptEntryId,
        /// Required raw-record occurrence.
        expected: ImportedRawRecordPosition,
        /// Supplied raw-record occurrence.
        actual: ImportedRawRecordPosition,
    },
    /// One entry referenced no raw-record occurrence.
    EntryRawRecordNotFound {
        /// Cross-wired entry.
        entry: ImportedTranscriptEntryId,
        /// Missing raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// One entry skipped or reversed its within-record position.
    EntryWithinRecordPositionMismatch {
        /// Mispositioned entry.
        entry: ImportedTranscriptEntryId,
        /// Required within-record position.
        expected: ImportedRecordEntryPosition,
        /// Supplied within-record position.
        actual: ImportedRecordEntryPosition,
    },
    /// One raw record had no normalized entry.
    RawRecordWithoutEntry {
        /// Unrepresented raw-record occurrence.
        position: ImportedRawRecordPosition,
    },
    /// A source event falsely carried a conversational speaker.
    SourceEventSpeakerMismatch {
        /// Invalid source-event entry.
        entry: ImportedTranscriptEntryId,
    },
    /// An entry's kind or speaker contradicted its normalized record type.
    SourceRecordTypeMismatch {
        /// Entry contradicted by its owning raw record.
        entry: ImportedTranscriptEntryId,
    },
    /// A message content entry lacked an attested user or assistant speaker.
    MessageSpeakerUnavailable {
        /// Invalid message entry.
        entry: ImportedTranscriptEntryId,
    },
    /// Attested nested role contradicted the top-level source speaker.
    MessageRoleMismatch {
        /// Contradictory message entry.
        entry: ImportedTranscriptEntryId,
    },
    /// An entry's modeled fields disagreed with its complete normalized record.
    EntryProjectionMismatch {
        /// Entry contradicted by its owning normalized record.
        entry: ImportedTranscriptEntryId,
    },
    /// A raw record's stored entry count disagreed with its normalized projection.
    RawRecordEntryProjectionMismatch {
        /// Raw-record occurrence with an incomplete or excessive entry projection.
        position: ImportedRawRecordPosition,
    },
    /// An entry-carried structured value exceeded the format's depth bound.
    EntryStructuredValueDepthExceeded {
        /// Entry carrying the excessive value.
        entry: ImportedTranscriptEntryId,
    },
    /// A required position could not advance beyond `u64::MAX`.
    PositionExhausted,
}

/// A failed reconstitution retaining every typed input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationReconstitutionError {
    pub(super) input: Box<ImportedConversationReconstitutionInput>,
    pub(super) failure: ImportedConversationReconstitutionFailure,
}

impl ImportedConversationReconstitutionError {
    /// Returns the precise reconstitution failure.
    pub const fn failure(&self) -> ImportedConversationReconstitutionFailure {
        self.failure
    }

    /// Borrows every unchanged typed input.
    pub const fn input(&self) -> &ImportedConversationReconstitutionInput {
        &self.input
    }

    /// Returns every unchanged input plus the precise failure.
    pub fn into_parts(
        self,
    ) -> (
        ImportedConversationReconstitutionInput,
        ImportedConversationReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}
