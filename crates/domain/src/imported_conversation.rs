//! Lossless imported-conversation records.
//!
//! The normative specification is `docs/spec/conversation-import.md`.
//! Imported entries retain exact source facts without carrying native execution
//! authority.

mod content;
mod conversation;
mod digest;
mod entry;
mod format;
mod position;
mod projection;
mod reconstitution;
mod record;
mod structured_field;
mod structured_value;
mod validation;

#[cfg(test)]
mod tests;

pub use format::ImportedConversationFormat;

pub use digest::{
    ImportedConversationSourceDigest, ImportedConversationSourceDigestBuilder,
    ImportedRawRecordConversionDigest, ImportedRawRecordHash,
};

pub use structured_value::{
    ImportedJsonNumber, ImportedJsonNumberError, ImportedSourceAttestation,
    ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
};

pub use content::{
    ImportedMediaSource, ImportedMessageContentAbsence, ImportedSourceMetadata, ImportedSpeaker,
    ImportedToolResultBlock, ImportedToolResultValue, ImportedTranscriptContent,
};

pub use position::{
    ImportedRawRecordPosition, ImportedRecordEntryPosition, ImportedTranscriptPosition,
};

pub use record::{
    ImportedRawSourceRecord, ImportedRawSourceRecordReconstitutionInput,
    ImportedTranscriptEntryInput,
};

pub use entry::{ImportedTranscriptEntry, ImportedTranscriptFrontier};

pub use reconstitution::{
    ImportedConversationReconstitutionError, ImportedConversationReconstitutionFailure,
    ImportedConversationReconstitutionInput,
};

pub use conversation::{
    ImportedConversation, ImportedConversationDisplayTitle, ImportedConversationDisplayTitleError,
};

pub use structured_field::{
    ImportedStructuredFieldError, imported_bool_attestation,
    imported_string_structured_attestation, imported_structured_attestation,
    imported_text_attestation, unique_imported_structured_field,
};

pub(crate) use entry::imported_frontier_from_validated_parts;
#[cfg(test)]
pub(crate) use entry::test_imported_frontier;
