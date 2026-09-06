//! Durable imported transcript entries and frontiers for `docs/spec/conversation-import.md`.

use super::content::ImportedSourceMetadata;
use super::content::ImportedSpeaker;
use super::content::ImportedTranscriptContent;
use super::position::ImportedRawRecordPosition;
use super::position::ImportedRecordEntryPosition;
use super::position::ImportedTranscriptPosition;
use super::structured_value::ImportedSourceAttestation;
use crate::ImportedConversationId;
use crate::ImportedTranscriptEntryId;
use std::hash::Hash;

/// One immutable normalized imported entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedTranscriptEntry {
    pub(super) identity: ImportedTranscriptEntryId,
    pub(super) conversation: ImportedConversationId,
    pub(super) position: ImportedTranscriptPosition,
    pub(super) raw_record_position: ImportedRawRecordPosition,
    pub(super) record_entry_position: ImportedRecordEntryPosition,
    pub(super) source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
    pub(super) content: ImportedTranscriptContent,
    pub(super) source: ImportedSourceMetadata,
}

impl ImportedTranscriptEntry {
    /// Returns the imported-entry identity.
    pub const fn identity(&self) -> ImportedTranscriptEntryId {
        self.identity
    }

    /// Returns the immutable owning conversation.
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

/// One immutable addressable imported entry boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ImportedTranscriptFrontier {
    pub(super) conversation: ImportedConversationId,
    pub(super) through_entry: ImportedTranscriptEntryId,
    pub(super) through_position: ImportedTranscriptPosition,
}

impl ImportedTranscriptFrontier {
    /// Constructs one caller-selected frontier for authoritative aggregate
    /// validation at the command boundary.
    pub const fn from_parts(
        conversation: ImportedConversationId,
        through_entry: ImportedTranscriptEntryId,
        through_position: ImportedTranscriptPosition,
    ) -> Self {
        Self {
            conversation,
            through_entry,
            through_position,
        }
    }

    /// Returns the immutable imported conversation.
    pub const fn conversation(self) -> ImportedConversationId {
        self.conversation
    }

    /// Returns the inclusive final imported entry.
    pub const fn through_entry(self) -> ImportedTranscriptEntryId {
        self.through_entry
    }

    /// Returns the inclusive final imported position.
    pub const fn through_position(self) -> ImportedTranscriptPosition {
        self.through_position
    }
}

pub(crate) const fn imported_frontier_from_validated_parts(
    conversation: ImportedConversationId,
    through_entry: ImportedTranscriptEntryId,
    through_position: ImportedTranscriptPosition,
) -> ImportedTranscriptFrontier {
    ImportedTranscriptFrontier::from_parts(conversation, through_entry, through_position)
}

#[cfg(test)]
pub(crate) const fn test_imported_frontier(
    conversation: ImportedConversationId,
    through_entry: ImportedTranscriptEntryId,
    through_position: ImportedTranscriptPosition,
) -> ImportedTranscriptFrontier {
    imported_frontier_from_validated_parts(conversation, through_entry, through_position)
}
