//! Immutable imported-conversation records.
//!
//! The normative specification is `docs/spec/conversation-import.md`.
//! Imported entries retain source attestations and content while carrying no
//! native execution authority.

use std::collections::BTreeSet;

use crate::{ImportedConversationId, ImportedTranscriptEntryId, NonEmptyUnicodeText};

/// One source format interpreted by one fixed Signalbox converter version.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedConversationFormat {
    /// Claude Code session JSONL interpreted by converter version 1.
    ClaudeCodeSessionJsonlV1,
}

/// What an external source asserted about one metadata field.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedSourceAttestation<Value> {
    /// The source supplied this exact value.
    Attested(Value),
    /// The source supplied an explicit null value.
    AttestedAbsent,
    /// The source did not supply the field.
    NotAttested,
}

/// Source-envelope attestations retained independently for one imported entry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImportedSourceMetadata {
    record_id: ImportedSourceAttestation<NonEmptyUnicodeText>,
    parent_record_id: ImportedSourceAttestation<NonEmptyUnicodeText>,
    source_session_id: ImportedSourceAttestation<NonEmptyUnicodeText>,
    timestamp: ImportedSourceAttestation<NonEmptyUnicodeText>,
    sidechain: ImportedSourceAttestation<bool>,
    metadata: ImportedSourceAttestation<bool>,
}

impl ImportedSourceMetadata {
    /// Supplies every modeled source attestation without deriving missing data.
    pub const fn new(
        record_id: ImportedSourceAttestation<NonEmptyUnicodeText>,
        parent_record_id: ImportedSourceAttestation<NonEmptyUnicodeText>,
        source_session_id: ImportedSourceAttestation<NonEmptyUnicodeText>,
        timestamp: ImportedSourceAttestation<NonEmptyUnicodeText>,
        sidechain: ImportedSourceAttestation<bool>,
        metadata: ImportedSourceAttestation<bool>,
    ) -> Self {
        Self {
            record_id,
            parent_record_id,
            source_session_id,
            timestamp,
            sidechain,
            metadata,
        }
    }

    /// Borrows the source record-identity attestation.
    pub const fn record_id(&self) -> &ImportedSourceAttestation<NonEmptyUnicodeText> {
        &self.record_id
    }

    /// Borrows the source parent-record attestation.
    pub const fn parent_record_id(&self) -> &ImportedSourceAttestation<NonEmptyUnicodeText> {
        &self.parent_record_id
    }

    /// Borrows the source session-identity attestation.
    pub const fn source_session_id(&self) -> &ImportedSourceAttestation<NonEmptyUnicodeText> {
        &self.source_session_id
    }

    /// Borrows the source timestamp attestation.
    pub const fn timestamp(&self) -> &ImportedSourceAttestation<NonEmptyUnicodeText> {
        &self.timestamp
    }

    /// Borrows the source sidechain attestation.
    pub const fn sidechain(&self) -> &ImportedSourceAttestation<bool> {
        &self.sidechain
    }

    /// Borrows the source metadata-record attestation.
    pub const fn metadata(&self) -> &ImportedSourceAttestation<bool> {
        &self.metadata
    }
}

/// The source-attested conversational speaker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedSpeaker {
    /// External user-authored message content.
    User,
    /// External assistant-authored message content.
    Assistant,
}

/// Why one source content block has no imported text representation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedContentUnavailable {
    /// The source supplied an empty text block.
    EmptyText,
    /// The source supplied an assistant tool-use block.
    ToolUse,
    /// The source supplied a user tool-result block.
    ToolResult,
    /// The source supplied a thinking block.
    Thinking,
    /// The source supplied a redacted-thinking block.
    RedactedThinking,
}

/// Exact imported text or a typed statement that text is unavailable.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedTranscriptContent {
    /// Exact nonempty source text.
    Text(NonEmptyUnicodeText),
    /// A source content class deliberately not represented as text.
    Unavailable(ImportedContentUnavailable),
}

/// Whether and why one imported entry participates in a session seed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedSeedDisposition {
    /// Exact text participates in seed context.
    Included,
    /// Exact text is retained but excluded by explicit source flags.
    ExcludedBySource {
        /// The source explicitly marked this record as a sidechain.
        sidechain: bool,
        /// The source explicitly marked this record as metadata.
        metadata: bool,
    },
    /// Non-text source content is retained as typed absence and cannot render.
    ExcludedUnavailable {
        /// The unavailable source content class.
        reason: ImportedContentUnavailable,
    },
}

/// One positive position in physical source record/block order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImportedTranscriptPosition(u64);

impl ImportedTranscriptPosition {
    /// Reconstitutes a position from a positive ordinal.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the positive ordinal.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns the first imported position.
    pub const fn first() -> Self {
        Self(1)
    }

    /// Returns the next position or `None` after `u64::MAX`.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// Raw typed fields for one imported entry at a reconstitution boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedTranscriptEntryReconstitutionInput {
    identity: ImportedTranscriptEntryId,
    conversation: ImportedConversationId,
    position: ImportedTranscriptPosition,
    speaker: ImportedSpeaker,
    content: ImportedTranscriptContent,
    source: ImportedSourceMetadata,
    stored_seed_disposition: ImportedSeedDisposition,
}

impl ImportedTranscriptEntryReconstitutionInput {
    /// Supplies the complete typed imported-entry projection.
    pub const fn new(
        identity: ImportedTranscriptEntryId,
        conversation: ImportedConversationId,
        position: ImportedTranscriptPosition,
        speaker: ImportedSpeaker,
        content: ImportedTranscriptContent,
        source: ImportedSourceMetadata,
        stored_seed_disposition: ImportedSeedDisposition,
    ) -> Self {
        Self {
            identity,
            conversation,
            position,
            speaker,
            content,
            source,
            stored_seed_disposition,
        }
    }

    /// Returns the imported entry identity.
    pub const fn identity(&self) -> ImportedTranscriptEntryId {
        self.identity
    }

    /// Returns the claimed owning conversation.
    pub const fn conversation(&self) -> ImportedConversationId {
        self.conversation
    }

    /// Returns the source-order position.
    pub const fn position(&self) -> ImportedTranscriptPosition {
        self.position
    }

    /// Returns the source-attested speaker.
    pub const fn speaker(&self) -> ImportedSpeaker {
        self.speaker
    }

    /// Borrows the exact text or typed content absence.
    pub const fn content(&self) -> &ImportedTranscriptContent {
        &self.content
    }

    /// Borrows the complete source metadata projection.
    pub const fn source(&self) -> &ImportedSourceMetadata {
        &self.source
    }

    /// Returns the seed disposition stored with this entry.
    pub const fn stored_seed_disposition(&self) -> ImportedSeedDisposition {
        self.stored_seed_disposition
    }
}

/// One immutable imported transcript entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedTranscriptEntry {
    identity: ImportedTranscriptEntryId,
    conversation: ImportedConversationId,
    position: ImportedTranscriptPosition,
    speaker: ImportedSpeaker,
    content: ImportedTranscriptContent,
    source: ImportedSourceMetadata,
    seed_disposition: ImportedSeedDisposition,
}

impl ImportedTranscriptEntry {
    /// Returns the imported entry identity.
    pub const fn identity(&self) -> ImportedTranscriptEntryId {
        self.identity
    }

    /// Returns the immutable owning conversation.
    pub const fn conversation(&self) -> ImportedConversationId {
        self.conversation
    }

    /// Returns the source-order position.
    pub const fn position(&self) -> ImportedTranscriptPosition {
        self.position
    }

    /// Returns the source-attested speaker.
    pub const fn speaker(&self) -> ImportedSpeaker {
        self.speaker
    }

    /// Borrows the exact text or typed content absence.
    pub const fn content(&self) -> &ImportedTranscriptContent {
        &self.content
    }

    /// Borrows the complete source metadata projection.
    pub const fn source(&self) -> &ImportedSourceMetadata {
        &self.source
    }

    /// Returns whether and why the entry participates in seed context.
    pub const fn seed_disposition(&self) -> ImportedSeedDisposition {
        self.seed_disposition
    }
}

/// Complete typed inputs for one imported-conversation reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationReconstitutionInput {
    requested_conversation: ImportedConversationId,
    stored_conversation: ImportedConversationId,
    format: ImportedConversationFormat,
    declared_entry_count: u64,
    entries: Vec<ImportedTranscriptEntryReconstitutionInput>,
}

impl ImportedConversationReconstitutionInput {
    /// Supplies a complete imported aggregate projection.
    pub fn new(
        requested_conversation: ImportedConversationId,
        stored_conversation: ImportedConversationId,
        format: ImportedConversationFormat,
        declared_entry_count: u64,
        entries: Vec<ImportedTranscriptEntryReconstitutionInput>,
    ) -> Self {
        Self {
            requested_conversation,
            stored_conversation,
            format,
            declared_entry_count,
            entries,
        }
    }

    /// Returns the conversation requested by the caller.
    pub const fn requested_conversation(&self) -> ImportedConversationId {
        self.requested_conversation
    }

    /// Returns the identity stored on the aggregate header.
    pub const fn stored_conversation(&self) -> ImportedConversationId {
        self.stored_conversation
    }

    /// Returns the closed source format and converter version.
    pub const fn format(&self) -> ImportedConversationFormat {
        self.format
    }

    /// Returns the member count declared by the aggregate header.
    pub const fn declared_entry_count(&self) -> u64 {
        self.declared_entry_count
    }

    /// Borrows the complete stored entry projections.
    pub fn entries(&self) -> &[ImportedTranscriptEntryReconstitutionInput] {
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

        let entries = self
            .entries
            .into_iter()
            .map(|entry| ImportedTranscriptEntry {
                identity: entry.identity,
                conversation: entry.conversation,
                position: entry.position,
                speaker: entry.speaker,
                content: entry.content,
                source: entry.source,
                seed_disposition: entry.stored_seed_disposition,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(ImportedConversation {
            id: self.stored_conversation,
            format: self.format,
            entries,
        })
    }
}

/// Why complete typed records cannot reconstruct an imported conversation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedConversationReconstitutionFailure {
    /// The requested identity differs from the stored header.
    RequestedConversationMismatch,
    /// An imported conversation contained no transcript entries.
    EmptyConversation,
    /// The header count differs from the supplied entry count.
    DeclaredEntryCountMismatch {
        /// The durable header count.
        declared: u64,
        /// The number of supplied member records.
        actual: usize,
    },
    /// One member names another imported conversation.
    EntryConversationMismatch {
        /// The cross-wired entry.
        entry: ImportedTranscriptEntryId,
    },
    /// One member does not occupy the next contiguous source position.
    EntryPositionMismatch {
        /// The mispositioned entry.
        entry: ImportedTranscriptEntryId,
        /// The required contiguous position.
        expected: ImportedTranscriptPosition,
        /// The supplied position.
        actual: ImportedTranscriptPosition,
    },
    /// The same imported-entry identity appeared more than once.
    DuplicateEntry {
        /// The duplicated entry identity.
        entry: ImportedTranscriptEntryId,
    },
    /// A stored seed disposition contradicts content and source flags.
    SeedDispositionMismatch {
        /// The contradicted entry.
        entry: ImportedTranscriptEntryId,
        /// The disposition derived from immutable source facts.
        expected: ImportedSeedDisposition,
        /// The inconsistent stored disposition.
        actual: ImportedSeedDisposition,
    },
    /// More entries followed the maximum representable position.
    PositionExhausted,
}

/// A failed imported-conversation reconstitution retaining every input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationReconstitutionError {
    input: Box<ImportedConversationReconstitutionInput>,
    failure: ImportedConversationReconstitutionFailure,
}

impl ImportedConversationReconstitutionError {
    /// Returns why reconstitution failed.
    pub const fn failure(&self) -> ImportedConversationReconstitutionFailure {
        self.failure
    }

    /// Borrows the unchanged complete input.
    pub const fn input(&self) -> &ImportedConversationReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged input and precise failure.
    pub fn into_parts(
        self,
    ) -> (
        ImportedConversationReconstitutionInput,
        ImportedConversationReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One complete immutable imported conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversation {
    id: ImportedConversationId,
    format: ImportedConversationFormat,
    entries: Box<[ImportedTranscriptEntry]>,
}

impl ImportedConversation {
    /// Returns the hub-minted imported-conversation identity.
    pub const fn id(&self) -> ImportedConversationId {
        self.id
    }

    /// Returns the closed source format and converter version.
    pub const fn format(&self) -> ImportedConversationFormat {
        self.format
    }

    /// Borrows the nonempty entries in exact source order.
    pub fn entries(&self) -> &[ImportedTranscriptEntry] {
        &self.entries
    }

    /// Returns the seed-included exact-text entries in source order.
    pub fn seed_entries(&self) -> impl Iterator<Item = &ImportedTranscriptEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.seed_disposition == ImportedSeedDisposition::Included)
    }
}

fn validate_reconstitution(
    input: &ImportedConversationReconstitutionInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    if input.requested_conversation != input.stored_conversation {
        return Err(ImportedConversationReconstitutionFailure::RequestedConversationMismatch);
    }
    if input.entries.is_empty() {
        return Err(ImportedConversationReconstitutionFailure::EmptyConversation);
    }
    if u64::try_from(input.entries.len()).ok() != Some(input.declared_entry_count) {
        return Err(
            ImportedConversationReconstitutionFailure::DeclaredEntryCountMismatch {
                declared: input.declared_entry_count,
                actual: input.entries.len(),
            },
        );
    }

    let mut expected_position = ImportedTranscriptPosition::first();
    let mut identities = BTreeSet::new();
    for (index, entry) in input.entries.iter().enumerate() {
        if entry.conversation != input.stored_conversation {
            return Err(
                ImportedConversationReconstitutionFailure::EntryConversationMismatch {
                    entry: entry.identity,
                },
            );
        }
        if entry.position != expected_position {
            return Err(
                ImportedConversationReconstitutionFailure::EntryPositionMismatch {
                    entry: entry.identity,
                    expected: expected_position,
                    actual: entry.position,
                },
            );
        }
        if !identities.insert(entry.identity) {
            return Err(ImportedConversationReconstitutionFailure::DuplicateEntry {
                entry: entry.identity,
            });
        }
        let expected_seed_disposition = derive_seed_disposition(&entry.content, &entry.source);
        if entry.stored_seed_disposition != expected_seed_disposition {
            return Err(
                ImportedConversationReconstitutionFailure::SeedDispositionMismatch {
                    entry: entry.identity,
                    expected: expected_seed_disposition,
                    actual: entry.stored_seed_disposition,
                },
            );
        }
        if index + 1 < input.entries.len() {
            expected_position = expected_position
                .checked_next()
                .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;
        }
    }
    Ok(())
}

fn derive_seed_disposition(
    content: &ImportedTranscriptContent,
    source: &ImportedSourceMetadata,
) -> ImportedSeedDisposition {
    match content {
        ImportedTranscriptContent::Unavailable(reason) => {
            ImportedSeedDisposition::ExcludedUnavailable { reason: *reason }
        }
        ImportedTranscriptContent::Text(_) => {
            let sidechain = matches!(source.sidechain, ImportedSourceAttestation::Attested(true));
            let metadata = matches!(source.metadata, ImportedSourceAttestation::Attested(true));
            if sidechain || metadata {
                ImportedSeedDisposition::ExcludedBySource {
                    sidechain,
                    metadata,
                }
            } else {
                ImportedSeedDisposition::Included
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ImportedContentUnavailable, ImportedConversationFormat,
        ImportedConversationReconstitutionFailure, ImportedConversationReconstitutionInput,
        ImportedSeedDisposition, ImportedSourceAttestation, ImportedSourceMetadata,
        ImportedSpeaker, ImportedTranscriptContent, ImportedTranscriptEntryReconstitutionInput,
        ImportedTranscriptPosition,
    };
    use crate::{ImportedConversationId, ImportedTranscriptEntryId, NonEmptyUnicodeText};
    use uuid::Uuid;

    fn conversation(value: u128) -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(value))
    }

    fn entry(value: u128) -> ImportedTranscriptEntryId {
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value))
    }

    fn text(value: &str) -> NonEmptyUnicodeText {
        NonEmptyUnicodeText::try_new(String::from(value)).expect("fixture text is admitted")
    }

    fn metadata(
        sidechain: ImportedSourceAttestation<bool>,
        metadata: ImportedSourceAttestation<bool>,
    ) -> ImportedSourceMetadata {
        ImportedSourceMetadata::new(
            ImportedSourceAttestation::Attested(text("source-record")),
            ImportedSourceAttestation::AttestedAbsent,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::Attested(text("2026-07-23T00:00:00Z")),
            sidechain,
            metadata,
        )
    }

    fn input_entry(
        identity: ImportedTranscriptEntryId,
        owner: ImportedConversationId,
        position: u64,
        content: ImportedTranscriptContent,
        source: ImportedSourceMetadata,
        disposition: ImportedSeedDisposition,
    ) -> ImportedTranscriptEntryReconstitutionInput {
        ImportedTranscriptEntryReconstitutionInput::new(
            identity,
            owner,
            ImportedTranscriptPosition::try_from_u64(position)
                .expect("fixture position is positive"),
            ImportedSpeaker::User,
            content,
            source,
            disposition,
        )
    }

    fn valid_input() -> ImportedConversationReconstitutionInput {
        let owner = conversation(1);
        ImportedConversationReconstitutionInput::new(
            owner,
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            2,
            vec![
                input_entry(
                    entry(2),
                    owner,
                    1,
                    ImportedTranscriptContent::Text(text("exact user text")),
                    metadata(
                        ImportedSourceAttestation::Attested(false),
                        ImportedSourceAttestation::NotAttested,
                    ),
                    ImportedSeedDisposition::Included,
                ),
                input_entry(
                    entry(3),
                    owner,
                    2,
                    ImportedTranscriptContent::Unavailable(ImportedContentUnavailable::ToolResult),
                    metadata(
                        ImportedSourceAttestation::NotAttested,
                        ImportedSourceAttestation::Attested(false),
                    ),
                    ImportedSeedDisposition::ExcludedUnavailable {
                        reason: ImportedContentUnavailable::ToolResult,
                    },
                ),
            ],
        )
    }

    /// INV-038: complete source-neutral records reconstruct without acquiring
    /// native execution identities or authority.
    #[test]
    fn inv038_reconstitutes_complete_imported_record_in_source_order() {
        let imported = valid_input()
            .reconstitute()
            .expect("complete imported records reconstruct");

        assert_eq!(imported.id(), conversation(1));
        assert_eq!(
            imported.format(),
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1
        );
        assert_eq!(
            imported
                .entries()
                .iter()
                .map(|entry| entry.identity())
                .collect::<Vec<_>>(),
            vec![entry(2), entry(3)]
        );
        assert_eq!(
            imported
                .seed_entries()
                .map(|entry| entry.identity())
                .collect::<Vec<_>>(),
            vec![entry(2)]
        );
    }

    /// INV-038: explicit source flags exclude retained text without changing
    /// its content or collapsing the two independent attestations.
    #[test]
    fn inv038_source_flags_derive_exact_seed_exclusion() {
        let owner = conversation(1);
        let input = ImportedConversationReconstitutionInput::new(
            owner,
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            1,
            vec![input_entry(
                entry(2),
                owner,
                1,
                ImportedTranscriptContent::Text(text("retained sidechain metadata")),
                metadata(
                    ImportedSourceAttestation::Attested(true),
                    ImportedSourceAttestation::Attested(true),
                ),
                ImportedSeedDisposition::ExcludedBySource {
                    sidechain: true,
                    metadata: true,
                },
            )],
        );

        let imported = input
            .reconstitute()
            .expect("exact source flags form a valid exclusion");
        assert_eq!(
            imported.entries()[0].seed_disposition(),
            ImportedSeedDisposition::ExcludedBySource {
                sidechain: true,
                metadata: true,
            }
        );
        assert_eq!(imported.seed_entries().count(), 0);
    }

    /// INV-038 / INV-002: every aggregate correlation and derived seed
    /// disposition fails closed while retaining the complete unchanged input.
    #[test]
    fn inv038_reconstitution_rejects_incomplete_or_cross_wired_records() {
        let cases = [
            (
                ImportedConversationReconstitutionInput::new(
                    conversation(9),
                    conversation(1),
                    ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                    2,
                    valid_input().entries().to_vec(),
                ),
                ImportedConversationReconstitutionFailure::RequestedConversationMismatch,
            ),
            (
                ImportedConversationReconstitutionInput::new(
                    conversation(1),
                    conversation(1),
                    ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                    0,
                    Vec::new(),
                ),
                ImportedConversationReconstitutionFailure::EmptyConversation,
            ),
            (
                ImportedConversationReconstitutionInput::new(
                    conversation(1),
                    conversation(1),
                    ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                    3,
                    valid_input().entries().to_vec(),
                ),
                ImportedConversationReconstitutionFailure::DeclaredEntryCountMismatch {
                    declared: 3,
                    actual: 2,
                },
            ),
        ];

        for (input, expected) in cases {
            let retained = input.clone();
            let error = input
                .reconstitute()
                .expect_err("invalid aggregate facts must fail closed");
            assert_eq!(error.failure(), expected);
            assert_eq!(error.input(), &retained);
            assert_eq!(error.into_parts(), (retained, expected));
        }

        let mut wrong_owner = valid_input();
        wrong_owner.entries[0].conversation = conversation(8);
        assert_rejects(
            wrong_owner,
            ImportedConversationReconstitutionFailure::EntryConversationMismatch {
                entry: entry(2),
            },
        );

        let mut gap = valid_input();
        gap.entries[1].position =
            ImportedTranscriptPosition::try_from_u64(3).expect("fixture position is positive");
        assert_rejects(
            gap,
            ImportedConversationReconstitutionFailure::EntryPositionMismatch {
                entry: entry(3),
                expected: ImportedTranscriptPosition::try_from_u64(2)
                    .expect("fixture position is positive"),
                actual: ImportedTranscriptPosition::try_from_u64(3)
                    .expect("fixture position is positive"),
            },
        );

        let mut duplicate = valid_input();
        duplicate.entries[1].identity = entry(2);
        assert_rejects(
            duplicate,
            ImportedConversationReconstitutionFailure::DuplicateEntry { entry: entry(2) },
        );

        let mut mismatched_seed = valid_input();
        mismatched_seed.entries[0].stored_seed_disposition =
            ImportedSeedDisposition::ExcludedBySource {
                sidechain: true,
                metadata: false,
            };
        assert_rejects(
            mismatched_seed,
            ImportedConversationReconstitutionFailure::SeedDispositionMismatch {
                entry: entry(2),
                expected: ImportedSeedDisposition::Included,
                actual: ImportedSeedDisposition::ExcludedBySource {
                    sidechain: true,
                    metadata: false,
                },
            },
        );
    }

    #[track_caller]
    fn assert_rejects(
        input: ImportedConversationReconstitutionInput,
        expected: ImportedConversationReconstitutionFailure,
    ) {
        let retained = input.clone();
        let error = input
            .reconstitute()
            .expect_err("invalid imported facts must fail closed");
        assert_eq!(error.failure(), expected);
        assert_eq!(error.input(), &retained);
    }
}
