//! Imported conversation aggregate and display titles for `docs/spec/conversation-import.md`.

use super::digest::ImportedConversationSourceDigest;
use super::entry::ImportedTranscriptEntry;
use super::entry::ImportedTranscriptFrontier;
use super::format::ImportedConversationFormat;
use super::position::ImportedRawRecordPosition;
use super::reconstitution::ImportedConversationReconstitutionError;
use super::reconstitution::ImportedConversationReconstitutionFailure;
use super::reconstitution::ImportedConversationReconstitutionInput;
use super::record::ImportedRawSourceRecord;
use super::record::ImportedRawSourceRecordReconstitutionInput;
use super::record::ImportedTranscriptEntryInput;
use super::validation::attested_user_text_candidates;
use super::validation::conversion_error;
use super::validation::typed_record_string_candidates;
use crate::ImportedConversationId;
use crate::ImportedTranscriptEntryId;
use std::error::Error;
use std::fmt;
use std::hash::Hash;

/// One complete immutable, lossless imported conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversation {
    id: ImportedConversationId,
    format: ImportedConversationFormat,
    source_digest: ImportedConversationSourceDigest,
    raw_records: Box<[ImportedRawSourceRecord]>,
    entries: Box<[ImportedTranscriptEntry]>,
}

impl ImportedConversation {
    /// Checks and assembles one completely converted aggregate.
    pub fn from_converted_records(
        id: ImportedConversationId,
        format: ImportedConversationFormat,
        raw_records: Vec<ImportedRawSourceRecord>,
        entries: Vec<ImportedTranscriptEntryInput>,
    ) -> Result<Self, ImportedConversationReconstitutionError> {
        let mut position = ImportedRawRecordPosition::first();
        let raw_record_count = raw_records.len();
        let mut reconstitution_records = Vec::with_capacity(raw_records.len());
        for (index, record) in raw_records.into_iter().enumerate() {
            reconstitution_records.push(ImportedRawSourceRecordReconstitutionInput {
                position,
                stored_hash: record.content_hash,
                stored_conversion_digest: record.conversion_digest,
                bytes: record.bytes,
                normalized: record.normalized,
            });
            if index + 1 < raw_record_count {
                let Some(next) = position.checked_next() else {
                    return Err(conversion_error(
                        id,
                        format,
                        reconstitution_records,
                        entries,
                        ImportedConversationReconstitutionFailure::PositionExhausted,
                    ));
                };
                position = next;
            }
        }
        let source_digest =
            ImportedConversationSourceDigest::derive(format, &reconstitution_records);
        let declared_raw_record_count =
            u64::try_from(reconstitution_records.len()).unwrap_or(u64::MAX);
        let declared_entry_count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
        ImportedConversationReconstitutionInput::new(
            id,
            id,
            format,
            source_digest,
            declared_raw_record_count,
            reconstitution_records,
            declared_entry_count,
            entries,
        )
        .reconstitute()
    }

    /// Returns the hub-minted imported-conversation identity.
    pub const fn id(&self) -> ImportedConversationId {
        self.id
    }

    /// Returns the closed source format and converter version.
    pub const fn format(&self) -> ImportedConversationFormat {
        self.format
    }

    /// Returns the idempotency digest for exact ordered source content.
    pub const fn source_digest(&self) -> ImportedConversationSourceDigest {
        self.source_digest
    }

    /// Borrows every raw source record in physical order.
    pub fn raw_records(&self) -> &[ImportedRawSourceRecord] {
        &self.raw_records
    }

    /// Borrows every normalized entry in exact imported order.
    pub fn entries(&self) -> &[ImportedTranscriptEntry] {
        &self.entries
    }

    /// Iterates every immutable addressable entry boundary.
    pub fn frontiers(&self) -> impl Iterator<Item = ImportedTranscriptFrontier> + '_ {
        self.entries.iter().map(|entry| ImportedTranscriptFrontier {
            conversation: self.id,
            through_entry: entry.identity,
            through_position: entry.position,
        })
    }

    /// Resolves one entry identity to its immutable frontier.
    pub fn frontier_for_entry(
        &self,
        entry: ImportedTranscriptEntryId,
    ) -> Option<ImportedTranscriptFrontier> {
        self.entries
            .iter()
            .find(|candidate| candidate.identity == entry)
            .map(|candidate| ImportedTranscriptFrontier {
                conversation: self.id,
                through_entry: candidate.identity,
                through_position: candidate.position,
            })
    }

    /// Resolves a frontier to the exact inclusive imported prefix.
    pub fn prefix(
        &self,
        frontier: ImportedTranscriptFrontier,
    ) -> Option<&[ImportedTranscriptEntry]> {
        if frontier.conversation != self.id {
            return None;
        }
        let length = usize::try_from(frontier.through_position.as_u64()).ok()?;
        let entry = self.entries.get(length.checked_sub(1)?)?;
        if entry.identity != frontier.through_entry {
            return None;
        }
        self.entries.get(..length)
    }
}

/// One bounded source-derived display title for an imported conversation.
///
/// The value is presentation evidence derived once from the preserved source
/// records by [`Self::derive`]; it never participates in the source digest,
/// the imported-conversation identity, or the unique source-identity
/// constraint. Construction admits exactly the shape derivation emits:
/// nonempty single-line text without U+0000, carrying at most
/// [`Self::MAX_SCALARS`] Unicode scalars and no leading or trailing ASCII
/// space or tab.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImportedConversationDisplayTitle(String);

impl ImportedConversationDisplayTitle {
    /// Maximum Unicode scalars in one derived display title.
    pub const MAX_SCALARS: usize = 256;

    /// Validates one exact stored display-title value.
    pub fn try_new(value: String) -> Result<Self, ImportedConversationDisplayTitleError> {
        if value.is_empty() {
            return Err(ImportedConversationDisplayTitleError::Empty);
        }
        if value.contains('\0') {
            return Err(ImportedConversationDisplayTitleError::ContainsNul);
        }
        if value.contains(['\n', '\r']) {
            return Err(ImportedConversationDisplayTitleError::ContainsLineBreak);
        }
        let scalars = value.chars().count();
        if scalars > Self::MAX_SCALARS {
            return Err(ImportedConversationDisplayTitleError::ExceedsMaxScalars { scalars });
        }
        if value.starts_with([' ', '\t']) || value.ends_with([' ', '\t']) {
            return Err(ImportedConversationDisplayTitleError::UntrimmedEdgeWhitespace);
        }
        Ok(Self(value))
    }

    /// Derives the display title for one complete imported conversation.
    ///
    /// Candidate strings are tried in a fixed per-format order and the first
    /// candidate that shapes to a nonempty title
    /// wins; a conversation with no shapeable candidate has no display title:
    ///
    /// - Claude Code versions 1 and 2: for every raw record in physical order
    ///   whose normalized value is an object whose first `type` member is the
    ///   string `summary`, the string value of its first `summary` member;
    ///   then every attested-text entry with an attested `user` speaker, in
    ///   imported order.
    /// - Codex rollout version 1: for every raw record in physical order
    ///   whose normalized value is an object whose first `type` member is the
    ///   string `session_meta` and whose first `payload` member is an object,
    ///   the string value of the payload's first `title` member; then the
    ///   string value of each such payload's first `instructions` member;
    ///   then every attested-text entry with an attested `user` speaker, in
    ///   imported order.
    ///
    /// The derivation reads only preserved source evidence, never a filename,
    /// wall clock, or import-time context, so re-deriving from the same
    /// immutable aggregate always returns the same value.
    pub fn derive(conversation: &ImportedConversation) -> Option<Self> {
        match conversation.format() {
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1
            | ImportedConversationFormat::ClaudeCodeSessionJsonlV2 => {
                typed_record_string_candidates(conversation, "summary", &["summary"])
                    .filter_map(Self::shape_candidate)
                    .next()
                    .or_else(|| {
                        attested_user_text_candidates(conversation)
                            .filter_map(Self::shape_candidate)
                            .next()
                    })
            }
            ImportedConversationFormat::CodexRolloutJsonlV1 => {
                typed_record_string_candidates(conversation, "session_meta", &["payload", "title"])
                    .filter_map(Self::shape_candidate)
                    .next()
                    .or_else(|| {
                        typed_record_string_candidates(
                            conversation,
                            "session_meta",
                            &["payload", "instructions"],
                        )
                        .filter_map(Self::shape_candidate)
                        .next()
                    })
                    .or_else(|| {
                        attested_user_text_candidates(conversation)
                            .filter_map(Self::shape_candidate)
                            .next()
                    })
            }
        }
    }

    /// Shapes one candidate string into a valid display title, or exhausts it.
    ///
    /// The shape is the candidate's prefix up to its first line feed, carriage
    /// return, or U+0000, with leading and trailing ASCII space and tab
    /// removed, truncated to the first [`Self::MAX_SCALARS`] Unicode scalars,
    /// and finally stripped of any truncation-exposed trailing ASCII space or
    /// tab. An empty shape exhausts the candidate.
    fn shape_candidate(candidate: &str) -> Option<Self> {
        let first_line = candidate
            .split(['\n', '\r', '\0'])
            .next()
            .unwrap_or_default();
        let trimmed = first_line.trim_matches([' ', '\t']);
        let mut shaped: String = trimmed.chars().take(Self::MAX_SCALARS).collect();
        shaped.truncate(shaped.trim_end_matches([' ', '\t']).len());
        if shaped.is_empty() {
            return None;
        }
        Some(Self(shaped))
    }

    /// Borrows the exact title text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Transfers the exact title text out of the value.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// A stored display-title value violated the derived-shape contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedConversationDisplayTitleError {
    /// The value was empty.
    Empty,
    /// The value contained U+0000.
    ContainsNul,
    /// The value contained a line feed or carriage return.
    ContainsLineBreak,
    /// The value exceeded the scalar bound.
    ExceedsMaxScalars {
        /// The rejected scalar count.
        scalars: usize,
    },
    /// The value carried leading or trailing ASCII space or tab.
    UntrimmedEdgeWhitespace,
}

impl fmt::Display for ImportedConversationDisplayTitleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("imported display title is empty"),
            Self::ContainsNul => formatter.write_str("imported display title contains U+0000"),
            Self::ContainsLineBreak => {
                formatter.write_str("imported display title contains a line break")
            }
            Self::ExceedsMaxScalars { scalars } => write!(
                formatter,
                "imported display title carries {scalars} Unicode scalars; the bound is {}",
                ImportedConversationDisplayTitle::MAX_SCALARS
            ),
            Self::UntrimmedEdgeWhitespace => {
                formatter.write_str("imported display title carries edge ASCII whitespace")
            }
        }
    }
}

impl Error for ImportedConversationDisplayTitleError {}
pub(super) fn build_conversation(
    input: ImportedConversationReconstitutionInput,
) -> ImportedConversation {
    let raw_records = input
        .raw_records
        .into_iter()
        .map(|record| ImportedRawSourceRecord {
            content_hash: record.stored_hash,
            conversion_digest: record.stored_conversion_digest,
            bytes: record.bytes,
            normalized: record.normalized,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let entries = input
        .entries
        .into_iter()
        .map(|entry| ImportedTranscriptEntry {
            identity: entry.identity,
            conversation: entry.conversation,
            position: entry.position,
            raw_record_position: entry.raw_record_position,
            record_entry_position: entry.record_entry_position,
            source_speaker: entry.source_speaker,
            content: entry.content,
            source: entry.source,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    ImportedConversation {
        id: input.stored_conversation,
        format: input.format,
        source_digest: input.stored_source_digest,
        raw_records,
        entries,
    }
}
