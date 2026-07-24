//! Lossless imported-conversation records.
//!
//! The normative specification is `docs/spec/conversation-import.md`.
//! Imported entries retain exact source facts without carrying native execution
//! authority.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use sha2::{Digest, Sha256};

use crate::{ImportedConversationId, ImportedTranscriptEntryId};

const SOURCE_DIGEST_DOMAIN: &[u8] = b"signalbox.imported-conversation.source-digest.v1";

/// One source format interpreted by one fixed Signalbox converter version.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedConversationFormat {
    /// Claude Code session JSONL interpreted by converter version 1.
    ClaudeCodeSessionJsonlV1,
}

impl ImportedConversationFormat {
    fn digest_tag(self) -> &'static [u8] {
        match self {
            Self::ClaudeCodeSessionJsonlV1 => b"claude-code-session-jsonl-v1",
        }
    }
}

/// SHA-256 of one exact raw source-record byte sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImportedRawRecordHash([u8; 32]);

impl ImportedRawRecordHash {
    /// Reconstitutes one stored digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hashes exact source-record bytes.
    pub fn digest(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

/// Domain-separated SHA-256 of a format and ordered raw-record hashes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImportedConversationSourceDigest([u8; 32]);

impl ImportedConversationSourceDigest {
    /// Reconstitutes one stored source digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn derive(
        format: ImportedConversationFormat,
        records: &[ImportedRawSourceRecordReconstitutionInput],
    ) -> Self {
        let mut digest = Sha256::new();
        update_length_framed(&mut digest, SOURCE_DIGEST_DOMAIN);
        update_length_framed(&mut digest, format.digest_tag());
        digest.update(
            u64::try_from(records.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for record in records {
            update_length_framed(&mut digest, record.stored_hash.as_bytes());
        }
        Self(digest.finalize().into())
    }
}

fn update_length_framed(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

/// What an external source asserted about one field.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedSourceAttestation<Value> {
    /// The source supplied this exact value.
    Attested(Value),
    /// The source supplied an explicit null value.
    AttestedAbsent,
    /// The source did not supply the field.
    NotAttested,
}

/// Exact decoded imported text, including empty text and U+0000.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ImportedText(String);

impl ImportedText {
    /// Preserves one decoded Unicode scalar sequence without rewriting it.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrows the exact decoded text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact decoded text.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for ImportedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedText")
            .field("utf8_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// One checked JSON number spelling in the source-neutral structured algebra.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ImportedJsonNumber(String);

impl ImportedJsonNumber {
    /// Checks the complete RFC 8259 JSON number grammar.
    pub fn try_new(value: String) -> Result<Self, ImportedJsonNumberError> {
        if is_json_number(&value) {
            Ok(Self(value))
        } else {
            Err(ImportedJsonNumberError { value })
        }
    }

    /// Borrows the checked number spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the checked number spelling.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for ImportedJsonNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedJsonNumber")
            .field("utf8_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// A rejected imported JSON number retaining its exact spelling.
#[derive(Clone, Eq, PartialEq)]
pub struct ImportedJsonNumberError {
    value: String,
}

impl ImportedJsonNumberError {
    /// Borrows the rejected number spelling.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the rejected number spelling.
    pub fn into_value(self) -> String {
        self.value
    }
}

impl fmt::Debug for ImportedJsonNumberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedJsonNumberError")
            .field("utf8_len", &self.value.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ImportedJsonNumberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("imported JSON number has invalid syntax")
    }
}

impl Error for ImportedJsonNumberError {}

fn is_json_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => {
            index += 1;
            while matches!(bytes.get(index), Some(b'0'..=b'9')) {
                index += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
        if index == start {
            return false;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let start = index;
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
        if index == start {
            return false;
        }
    }
    index == bytes.len()
}

/// One ordered object member in the source-neutral structured algebra.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImportedStructuredObjectMember {
    name: ImportedText,
    value: ImportedStructuredValue,
}

impl ImportedStructuredObjectMember {
    /// Preserves one object member and its physical member position.
    pub fn new(name: ImportedText, value: ImportedStructuredValue) -> Self {
        Self { name, value }
    }

    /// Borrows the exact decoded member name.
    pub const fn name(&self) -> &ImportedText {
        &self.name
    }

    /// Borrows the member value.
    pub const fn value(&self) -> &ImportedStructuredValue {
        &self.value
    }
}

/// Source-neutral decoded JSON values.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedStructuredValue {
    /// JSON null.
    Null,
    /// JSON boolean.
    Boolean(bool),
    /// Checked JSON number.
    Number(ImportedJsonNumber),
    /// Exact decoded JSON string.
    String(ImportedText),
    /// Ordered JSON array.
    Array(Box<[ImportedStructuredValue]>),
    /// Ordered JSON object members, including repeated names.
    Object(Box<[ImportedStructuredObjectMember]>),
}

/// Source-attested conversational speaker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedSpeaker {
    /// External user-authored content.
    User,
    /// External assistant-authored content.
    Assistant,
}

/// Source-envelope attestations retained independently for one imported entry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImportedSourceMetadata {
    record_id: ImportedSourceAttestation<ImportedText>,
    parent_record_id: ImportedSourceAttestation<ImportedText>,
    source_session_id: ImportedSourceAttestation<ImportedText>,
    timestamp: ImportedSourceAttestation<ImportedText>,
    sidechain: ImportedSourceAttestation<bool>,
    metadata: ImportedSourceAttestation<bool>,
    message_role: ImportedSourceAttestation<ImportedSpeaker>,
}

impl ImportedSourceMetadata {
    /// Supplies every modeled source attestation without deriving missing data.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        record_id: ImportedSourceAttestation<ImportedText>,
        parent_record_id: ImportedSourceAttestation<ImportedText>,
        source_session_id: ImportedSourceAttestation<ImportedText>,
        timestamp: ImportedSourceAttestation<ImportedText>,
        sidechain: ImportedSourceAttestation<bool>,
        metadata: ImportedSourceAttestation<bool>,
        message_role: ImportedSourceAttestation<ImportedSpeaker>,
    ) -> Self {
        Self {
            record_id,
            parent_record_id,
            source_session_id,
            timestamp,
            sidechain,
            metadata,
            message_role,
        }
    }

    /// Borrows the source record-identity attestation.
    pub const fn record_id(&self) -> &ImportedSourceAttestation<ImportedText> {
        &self.record_id
    }

    /// Borrows the source parent-record attestation.
    pub const fn parent_record_id(&self) -> &ImportedSourceAttestation<ImportedText> {
        &self.parent_record_id
    }

    /// Borrows the source session-identity attestation.
    pub const fn source_session_id(&self) -> &ImportedSourceAttestation<ImportedText> {
        &self.source_session_id
    }

    /// Borrows the source timestamp attestation.
    pub const fn timestamp(&self) -> &ImportedSourceAttestation<ImportedText> {
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

    /// Borrows the nested message-role attestation.
    pub const fn message_role(&self) -> &ImportedSourceAttestation<ImportedSpeaker> {
        &self.message_role
    }
}

/// Why a message record has no source content entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImportedMessageContentAbsence {
    /// The source omitted the complete message envelope.
    MessageNotAttested,
    /// The source supplied an explicit null message envelope.
    MessageAttestedAbsent,
    /// The source omitted content from an object-valued message.
    ContentNotAttested,
    /// The source supplied explicit null message content.
    ContentAttestedAbsent,
    /// The source supplied an empty content-block array.
    EmptyBlockArray,
}

/// Source-attested media data used by documents and image results.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImportedMediaSource {
    kind: ImportedSourceAttestation<ImportedText>,
    media_type: ImportedSourceAttestation<ImportedText>,
    data: ImportedSourceAttestation<ImportedText>,
}

impl ImportedMediaSource {
    /// Supplies every media-source attestation.
    pub const fn new(
        kind: ImportedSourceAttestation<ImportedText>,
        media_type: ImportedSourceAttestation<ImportedText>,
        data: ImportedSourceAttestation<ImportedText>,
    ) -> Self {
        Self {
            kind,
            media_type,
            data,
        }
    }

    /// Borrows the source kind attestation.
    pub const fn kind(&self) -> &ImportedSourceAttestation<ImportedText> {
        &self.kind
    }

    /// Borrows the media-type attestation.
    pub const fn media_type(&self) -> &ImportedSourceAttestation<ImportedText> {
        &self.media_type
    }

    /// Borrows the exact media-data attestation.
    pub const fn data(&self) -> &ImportedSourceAttestation<ImportedText> {
        &self.data
    }
}

/// One ordered rich block inside a tool result.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedToolResultBlock {
    /// Exact or absent result text.
    Text(ImportedSourceAttestation<ImportedText>),
    /// Exact or absent source-attested image data.
    Image(ImportedSourceAttestation<ImportedMediaSource>),
    /// A source tool reference.
    ToolReference {
        /// Exact or absent tool name.
        tool_name: ImportedSourceAttestation<ImportedText>,
    },
}

/// One present tool-result content value.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedToolResultValue {
    /// Exact string-valued result content.
    Text(ImportedText),
    /// Exact ordered array-valued result content.
    Blocks(Box<[ImportedToolResultBlock]>),
}

/// Maximum-fidelity normalized imported entry content.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ImportedTranscriptContent {
    /// One non-message record and its source type.
    SourceEvent {
        /// Exact, explicit-null, or omitted top-level source type.
        source_type: ImportedSourceAttestation<ImportedText>,
    },
    /// One source-defined block inside a user or assistant message.
    SourceMessageBlock {
        /// Exact source block type.
        source_type: ImportedSourceAttestation<ImportedText>,
    },
    /// Exact or absent decoded user or assistant text.
    Text(ImportedSourceAttestation<ImportedText>),
    /// One source tool call.
    ToolCall {
        /// Source call identity.
        source_call_id: ImportedSourceAttestation<ImportedText>,
        /// Source tool name.
        name: ImportedSourceAttestation<ImportedText>,
        /// Source structured input.
        input: ImportedSourceAttestation<ImportedStructuredValue>,
        /// Source caller metadata.
        caller: ImportedSourceAttestation<ImportedStructuredValue>,
    },
    /// One source tool result.
    ToolResult {
        /// Source call identity being answered.
        source_call_id: ImportedSourceAttestation<ImportedText>,
        /// Source result content.
        content: ImportedSourceAttestation<ImportedToolResultValue>,
        /// Source error flag.
        is_error: ImportedSourceAttestation<bool>,
    },
    /// Source-visible thinking plus signature.
    Thinking {
        /// Exact source thinking.
        thinking: ImportedSourceAttestation<ImportedText>,
        /// Exact source signature.
        signature: ImportedSourceAttestation<ImportedText>,
    },
    /// Source redacted-thinking data.
    RedactedThinking {
        /// Exact source redacted data.
        data: ImportedSourceAttestation<ImportedText>,
    },
    /// One source document block.
    Document {
        /// Exact source-attested media data.
        source: ImportedSourceAttestation<ImportedMediaSource>,
    },
    /// One precisely classified absent message content.
    MessageContentAbsent(ImportedMessageContentAbsence),
}

macro_rules! positive_position {
    ($(#[$documentation:meta])* $name:ident) => {
        $(#[$documentation])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            /// Reconstitutes a position from a positive ordinal.
            pub const fn try_from_u64(value: u64) -> Option<Self> {
                if value == 0 { None } else { Some(Self(value)) }
            }

            /// Returns the positive ordinal.
            pub const fn as_u64(self) -> u64 {
                self.0
            }

            /// Returns the first position.
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
    };
}

positive_position!(
    /// One physical raw source-record position.
    ImportedRawRecordPosition
);
positive_position!(
    /// One normalized entry position inside a raw source record.
    ImportedRecordEntryPosition
);
positive_position!(
    /// One normalized imported entry position across the conversation.
    ImportedTranscriptPosition
);

/// One converted raw record with exact bytes and complete normalized JSON.
#[derive(Clone, Eq, PartialEq)]
pub struct ImportedRawSourceRecord {
    content_hash: ImportedRawRecordHash,
    bytes: Box<[u8]>,
    normalized: ImportedStructuredValue,
}

impl ImportedRawSourceRecord {
    /// Hashes and retains one exact converted source record.
    pub fn from_converted(bytes: Vec<u8>, normalized: ImportedStructuredValue) -> Self {
        Self {
            content_hash: ImportedRawRecordHash::digest(&bytes),
            bytes: bytes.into_boxed_slice(),
            normalized,
        }
    }

    /// Returns the exact-byte content hash.
    pub const fn content_hash(&self) -> ImportedRawRecordHash {
        self.content_hash
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
            .field("byte_len", &self.bytes.len())
            .field("normalized", &"<redacted>")
            .finish()
    }
}

/// Stored fields for one raw-record reconstitution boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct ImportedRawSourceRecordReconstitutionInput {
    position: ImportedRawRecordPosition,
    stored_hash: ImportedRawRecordHash,
    bytes: Box<[u8]>,
    normalized: ImportedStructuredValue,
}

impl ImportedRawSourceRecordReconstitutionInput {
    /// Supplies one complete stored raw record.
    pub fn new(
        position: ImportedRawRecordPosition,
        stored_hash: ImportedRawRecordHash,
        bytes: Vec<u8>,
        normalized: ImportedStructuredValue,
    ) -> Self {
        Self {
            position,
            stored_hash,
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
            .field("byte_len", &self.bytes.len())
            .field("normalized", &"<redacted>")
            .finish()
    }
}

/// Complete typed fields for one normalized imported entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedTranscriptEntryInput {
    identity: ImportedTranscriptEntryId,
    conversation: ImportedConversationId,
    position: ImportedTranscriptPosition,
    raw_record_position: ImportedRawRecordPosition,
    record_entry_position: ImportedRecordEntryPosition,
    source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
    content: ImportedTranscriptContent,
    source: ImportedSourceMetadata,
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

/// One immutable normalized imported entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedTranscriptEntry {
    identity: ImportedTranscriptEntryId,
    conversation: ImportedConversationId,
    position: ImportedTranscriptPosition,
    raw_record_position: ImportedRawRecordPosition,
    record_entry_position: ImportedRecordEntryPosition,
    source_speaker: ImportedSourceAttestation<ImportedSpeaker>,
    content: ImportedTranscriptContent,
    source: ImportedSourceMetadata,
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
    conversation: ImportedConversationId,
    through_entry: ImportedTranscriptEntryId,
    through_position: ImportedTranscriptPosition,
}

impl ImportedTranscriptFrontier {
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

/// Complete stored fields for imported-conversation reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationReconstitutionInput {
    requested_conversation: ImportedConversationId,
    stored_conversation: ImportedConversationId,
    format: ImportedConversationFormat,
    stored_source_digest: ImportedConversationSourceDigest,
    declared_raw_record_count: u64,
    raw_records: Vec<ImportedRawSourceRecordReconstitutionInput>,
    declared_entry_count: u64,
    entries: Vec<ImportedTranscriptEntryInput>,
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
    /// A raw JSONL record did not normalize to one object.
    RawRecordNormalizedValueNotObject {
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
    /// A required position could not advance beyond `u64::MAX`.
    PositionExhausted,
}

/// A failed reconstitution retaining every typed input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationReconstitutionError {
    input: Box<ImportedConversationReconstitutionInput>,
    failure: ImportedConversationReconstitutionFailure,
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

fn conversion_error(
    id: ImportedConversationId,
    format: ImportedConversationFormat,
    raw_records: Vec<ImportedRawSourceRecordReconstitutionInput>,
    entries: Vec<ImportedTranscriptEntryInput>,
    failure: ImportedConversationReconstitutionFailure,
) -> ImportedConversationReconstitutionError {
    let stored_source_digest = ImportedConversationSourceDigest::derive(format, &raw_records);
    ImportedConversationReconstitutionError {
        input: Box::new(ImportedConversationReconstitutionInput::new(
            id,
            id,
            format,
            stored_source_digest,
            u64::try_from(raw_records.len()).unwrap_or(u64::MAX),
            raw_records,
            u64::try_from(entries.len()).unwrap_or(u64::MAX),
            entries,
        )),
        failure,
    }
}

fn validate_reconstitution(
    input: &ImportedConversationReconstitutionInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    if input.requested_conversation != input.stored_conversation {
        return Err(ImportedConversationReconstitutionFailure::RequestedConversationMismatch);
    }
    validate_raw_records(input)?;
    validate_entries(input)
}

fn validate_raw_records(
    input: &ImportedConversationReconstitutionInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    if input.raw_records.is_empty() {
        return Err(ImportedConversationReconstitutionFailure::EmptyRawRecords);
    }
    if u64::try_from(input.raw_records.len()).ok() != Some(input.declared_raw_record_count) {
        return Err(
            ImportedConversationReconstitutionFailure::DeclaredRawRecordCountMismatch {
                declared: input.declared_raw_record_count,
                actual: input.raw_records.len(),
            },
        );
    }
    let mut expected = ImportedRawRecordPosition::first();
    let mut bytes_by_hash = BTreeMap::new();
    for (index, record) in input.raw_records.iter().enumerate() {
        if record.position != expected {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordPositionMismatch {
                    expected,
                    actual: record.position,
                },
            );
        }
        if ImportedRawRecordHash::digest(&record.bytes) != record.stored_hash {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordHashMismatch {
                    position: record.position,
                },
            );
        }
        if record.bytes.is_empty() {
            return Err(ImportedConversationReconstitutionFailure::EmptyRawRecord {
                position: record.position,
            });
        }
        if let Some(existing_bytes) = bytes_by_hash.insert(record.stored_hash, &record.bytes)
            && existing_bytes != &record.bytes
        {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordHashCollision {
                    position: record.position,
                },
            );
        }
        if !matches!(&record.normalized, ImportedStructuredValue::Object(_)) {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordNormalizedValueNotObject {
                    position: record.position,
                },
            );
        }
        if index + 1 < input.raw_records.len() {
            expected = expected
                .checked_next()
                .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;
        }
    }
    let expected_digest =
        ImportedConversationSourceDigest::derive(input.format, &input.raw_records);
    if input.stored_source_digest != expected_digest {
        return Err(
            ImportedConversationReconstitutionFailure::SourceDigestMismatch {
                expected: expected_digest,
                actual: input.stored_source_digest,
            },
        );
    }
    Ok(())
}

fn validate_entries(
    input: &ImportedConversationReconstitutionInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    if input.entries.is_empty() {
        return Err(ImportedConversationReconstitutionFailure::EmptyEntries);
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
    let mut expected_raw_position = ImportedRawRecordPosition::first();
    let mut expected_within_position = ImportedRecordEntryPosition::first();
    let mut identities = BTreeSet::new();
    let last_raw_position = input
        .raw_records
        .last()
        .map(ImportedRawSourceRecordReconstitutionInput::position)
        .ok_or(ImportedConversationReconstitutionFailure::EmptyRawRecords)?;
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
        if entry.raw_record_position > last_raw_position {
            return Err(
                ImportedConversationReconstitutionFailure::EntryRawRecordNotFound {
                    entry: entry.identity,
                    position: entry.raw_record_position,
                },
            );
        }
        if index == 0 && entry.raw_record_position != expected_raw_position {
            return Err(
                ImportedConversationReconstitutionFailure::RawRecordWithoutEntry {
                    position: expected_raw_position,
                },
            );
        }

        if entry.raw_record_position != expected_raw_position {
            let next_raw = expected_raw_position.checked_next();
            if next_raw == Some(entry.raw_record_position) {
                expected_raw_position = entry.raw_record_position;
                expected_within_position = ImportedRecordEntryPosition::first();
            } else {
                return Err(
                    ImportedConversationReconstitutionFailure::EntryRawRecordPositionMismatch {
                        entry: entry.identity,
                        expected: next_raw.unwrap_or(expected_raw_position),
                        actual: entry.raw_record_position,
                    },
                );
            }
        }
        if entry.record_entry_position != expected_within_position {
            return Err(
                ImportedConversationReconstitutionFailure::EntryWithinRecordPositionMismatch {
                    entry: entry.identity,
                    expected: expected_within_position,
                    actual: entry.record_entry_position,
                },
            );
        }
        validate_speaker(input, entry)?;

        if let Some(next_entry) = input.entries.get(index + 1) {
            expected_position = expected_position
                .checked_next()
                .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;
            if next_entry.raw_record_position == expected_raw_position {
                expected_within_position = expected_within_position
                    .checked_next()
                    .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?;
            }
        }
    }

    if expected_raw_position != last_raw_position {
        return Err(
            ImportedConversationReconstitutionFailure::RawRecordWithoutEntry {
                position: expected_raw_position
                    .checked_next()
                    .ok_or(ImportedConversationReconstitutionFailure::PositionExhausted)?,
            },
        );
    }
    Ok(())
}

fn validate_speaker(
    input: &ImportedConversationReconstitutionInput,
    entry: &ImportedTranscriptEntryInput,
) -> Result<(), ImportedConversationReconstitutionFailure> {
    let record = input
        .raw_records
        .get(
            usize::try_from(entry.raw_record_position.as_u64() - 1)
                .map_err(|_| ImportedConversationReconstitutionFailure::PositionExhausted)?,
        )
        .ok_or(
            ImportedConversationReconstitutionFailure::EntryRawRecordNotFound {
                entry: entry.identity,
                position: entry.raw_record_position,
            },
        )?;
    let record_speaker = normalized_record_speaker(record.normalized()).map_err(|()| {
        ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
            entry: entry.identity,
        }
    })?;

    if let ImportedTranscriptContent::SourceEvent { source_type } = &entry.content {
        if entry.source_speaker != ImportedSourceAttestation::NotAttested {
            return Err(
                ImportedConversationReconstitutionFailure::SourceEventSpeakerMismatch {
                    entry: entry.identity,
                },
            );
        }
        let record_type = normalized_record_type(record.normalized()).map_err(|()| {
            ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
                entry: entry.identity,
            }
        })?;
        if record_speaker.is_some() || *source_type != record_type {
            return Err(
                ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
                    entry: entry.identity,
                },
            );
        }
        return Ok(());
    }

    let ImportedSourceAttestation::Attested(speaker) = entry.source_speaker else {
        return Err(
            ImportedConversationReconstitutionFailure::MessageSpeakerUnavailable {
                entry: entry.identity,
            },
        );
    };
    if record_speaker != Some(speaker) {
        return Err(
            ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch {
                entry: entry.identity,
            },
        );
    }
    if let ImportedSourceAttestation::Attested(message_role) = entry.source.message_role
        && message_role != speaker
    {
        return Err(
            ImportedConversationReconstitutionFailure::MessageRoleMismatch {
                entry: entry.identity,
            },
        );
    }
    Ok(())
}

fn normalized_record_type(
    normalized: &ImportedStructuredValue,
) -> Result<ImportedSourceAttestation<ImportedText>, ()> {
    let ImportedStructuredValue::Object(members) = normalized else {
        return Err(());
    };
    let mut matches = members
        .iter()
        .filter(|member| member.name().as_str() == "type");
    let value = matches.next();
    if matches.next().is_some() {
        return Err(());
    }
    match value.map(ImportedStructuredObjectMember::value) {
        None => Ok(ImportedSourceAttestation::NotAttested),
        Some(ImportedStructuredValue::Null) => Ok(ImportedSourceAttestation::AttestedAbsent),
        Some(ImportedStructuredValue::String(value)) => {
            Ok(ImportedSourceAttestation::Attested(value.clone()))
        }
        Some(_) => Err(()),
    }
}

fn normalized_record_speaker(
    normalized: &ImportedStructuredValue,
) -> Result<Option<ImportedSpeaker>, ()> {
    match normalized_record_type(normalized).map_err(|_| ())? {
        ImportedSourceAttestation::Attested(value) if value.as_str() == "user" => {
            Ok(Some(ImportedSpeaker::User))
        }
        ImportedSourceAttestation::Attested(value) if value.as_str() == "assistant" => {
            Ok(Some(ImportedSpeaker::Assistant))
        }
        ImportedSourceAttestation::Attested(_)
        | ImportedSourceAttestation::AttestedAbsent
        | ImportedSourceAttestation::NotAttested => Ok(None),
    }
}

fn build_conversation(input: ImportedConversationReconstitutionInput) -> ImportedConversation {
    let raw_records = input
        .raw_records
        .into_iter()
        .map(|record| ImportedRawSourceRecord {
            content_hash: record.stored_hash,
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

#[cfg(test)]
mod tests {
    use super::{
        ImportedConversation, ImportedConversationFormat,
        ImportedConversationReconstitutionFailure, ImportedConversationReconstitutionInput,
        ImportedConversationSourceDigest, ImportedJsonNumber, ImportedMessageContentAbsence,
        ImportedRawRecordHash, ImportedRawRecordPosition, ImportedRawSourceRecord,
        ImportedRawSourceRecordReconstitutionInput, ImportedRecordEntryPosition,
        ImportedSourceAttestation, ImportedSourceMetadata, ImportedSpeaker,
        ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
        ImportedTranscriptContent, ImportedTranscriptEntryInput, ImportedTranscriptPosition,
    };
    use crate::{ImportedConversationId, ImportedTranscriptEntryId};
    use uuid::Uuid;

    fn conversation(value: u128) -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(value))
    }

    fn entry(value: u128) -> ImportedTranscriptEntryId {
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value))
    }

    fn text(value: &str) -> ImportedText {
        ImportedText::new(String::from(value))
    }

    fn object(member: (&str, ImportedStructuredValue)) -> ImportedStructuredValue {
        ImportedStructuredValue::Object(
            vec![ImportedStructuredObjectMember::new(
                text(member.0),
                member.1,
            )]
            .into_boxed_slice(),
        )
    }

    fn metadata(role: ImportedSourceAttestation<ImportedSpeaker>) -> ImportedSourceMetadata {
        ImportedSourceMetadata::new(
            ImportedSourceAttestation::Attested(text("record")),
            ImportedSourceAttestation::AttestedAbsent,
            ImportedSourceAttestation::Attested(text("session")),
            ImportedSourceAttestation::Attested(text("timestamp")),
            ImportedSourceAttestation::Attested(true),
            ImportedSourceAttestation::NotAttested,
            role,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn input_entry(
        identity: u128,
        owner: ImportedConversationId,
        position: u64,
        raw_position: u64,
        within_position: u64,
        speaker: ImportedSourceAttestation<ImportedSpeaker>,
        content: ImportedTranscriptContent,
        source: ImportedSourceMetadata,
    ) -> ImportedTranscriptEntryInput {
        ImportedTranscriptEntryInput::new(
            entry(identity),
            owner,
            ImportedTranscriptPosition::try_from_u64(position)
                .expect("fixture global position is positive"),
            ImportedRawRecordPosition::try_from_u64(raw_position)
                .expect("fixture raw position is positive"),
            ImportedRecordEntryPosition::try_from_u64(within_position)
                .expect("fixture within-record position is positive"),
            speaker,
            content,
            source,
        )
    }

    fn converted() -> ImportedConversation {
        let owner = conversation(1);
        let raw_records = vec![
            ImportedRawSourceRecord::from_converted(
                br#"{"type":"system","content":"before\u0000after"}"#.to_vec(),
                object(("type", ImportedStructuredValue::String(text("system")))),
            ),
            ImportedRawSourceRecord::from_converted(
                br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":""},{"type":"tool_use","input":{"n":1}}]}}"#.to_vec(),
                object((
                    "type",
                    ImportedStructuredValue::String(text("assistant")),
                )),
            ),
        ];
        let entries = vec![
            input_entry(
                2,
                owner,
                1,
                1,
                1,
                ImportedSourceAttestation::NotAttested,
                ImportedTranscriptContent::SourceEvent {
                    source_type: ImportedSourceAttestation::Attested(text("system")),
                },
                metadata(ImportedSourceAttestation::NotAttested),
            ),
            input_entry(
                3,
                owner,
                2,
                2,
                1,
                ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(""))),
                metadata(ImportedSourceAttestation::Attested(
                    ImportedSpeaker::Assistant,
                )),
            ),
            input_entry(
                4,
                owner,
                3,
                2,
                2,
                ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                ImportedTranscriptContent::ToolCall {
                    source_call_id: ImportedSourceAttestation::NotAttested,
                    name: ImportedSourceAttestation::AttestedAbsent,
                    input: ImportedSourceAttestation::Attested(object((
                        "n",
                        ImportedStructuredValue::Number(
                            ImportedJsonNumber::try_new(String::from("1"))
                                .expect("fixture number is valid"),
                        ),
                    ))),
                    caller: ImportedSourceAttestation::NotAttested,
                },
                metadata(ImportedSourceAttestation::Attested(
                    ImportedSpeaker::Assistant,
                )),
            ),
        ];
        ImportedConversation::from_converted_records(
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            raw_records,
            entries,
        )
        .expect("complete converted fixture is valid")
    }

    /// INV-038: exact raw records, rich normalized entries, and every imported
    /// entry boundary survive one checked immutable aggregate.
    #[test]
    fn inv038_lossless_aggregate_exposes_every_addressable_prefix() {
        let imported = converted();
        assert_eq!(imported.raw_records().len(), 2);
        assert_eq!(
            imported.raw_records()[0].bytes(),
            br#"{"type":"system","content":"before\u0000after"}"#
        );
        assert_eq!(imported.entries().len(), 3);
        assert_eq!(
            imported.entries()[1].content(),
            &ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text("")))
        );

        let frontiers = imported.frontiers().collect::<Vec<_>>();
        assert_eq!(frontiers.len(), imported.entries().len());
        assert_eq!(
            imported
                .prefix(frontiers[1])
                .expect("aggregate-produced frontier resolves")
                .iter()
                .map(|entry| entry.position().as_u64())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            imported
                .frontier_for_entry(imported.entries()[2].identity())
                .and_then(|frontier| imported.prefix(frontier))
                .map(<[_]>::len),
            Some(3)
        );
    }

    /// INV-038: raw bytes and format/order jointly determine stable digests.
    #[test]
    fn inv038_content_hashes_and_source_digest_are_stable_and_ordered() {
        let imported = converted();
        let repeated = converted();
        assert_eq!(imported.source_digest(), repeated.source_digest());
        assert_eq!(
            imported.raw_records()[0].content_hash(),
            ImportedRawRecordHash::digest(imported.raw_records()[0].bytes())
        );

        let mut reversed = imported.raw_records().to_vec();
        reversed.reverse();
        let entries = vec![
            input_entry(
                20,
                conversation(9),
                1,
                1,
                1,
                ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                ImportedTranscriptContent::MessageContentAbsent(
                    ImportedMessageContentAbsence::ContentNotAttested,
                ),
                metadata(ImportedSourceAttestation::Attested(
                    ImportedSpeaker::Assistant,
                )),
            ),
            input_entry(
                21,
                conversation(9),
                2,
                2,
                1,
                ImportedSourceAttestation::NotAttested,
                ImportedTranscriptContent::SourceEvent {
                    source_type: ImportedSourceAttestation::Attested(text("system")),
                },
                metadata(ImportedSourceAttestation::NotAttested),
            ),
        ];
        let reversed = ImportedConversation::from_converted_records(
            conversation(9),
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            reversed,
            entries,
        )
        .expect("reversed source is independently valid");
        assert_ne!(imported.source_digest(), reversed.source_digest());
    }

    /// INV-002 / INV-038: raw-hash corruption fails closed while retaining all
    /// typed storage inputs.
    #[test]
    fn inv002_inv038_raw_hash_corruption_retains_complete_input() {
        let owner = conversation(1);
        let bytes = br#"{"type":"system"}"#.to_vec();
        let raw_records = vec![ImportedRawSourceRecordReconstitutionInput::new(
            ImportedRawRecordPosition::first(),
            ImportedRawRecordHash::digest(b"different"),
            bytes,
            object(("type", ImportedStructuredValue::String(text("system")))),
        )];
        let digest = ImportedConversationSourceDigest::derive(
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            &raw_records,
        );
        let entries = vec![input_entry(
            2,
            owner,
            1,
            1,
            1,
            ImportedSourceAttestation::NotAttested,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("system")),
            },
            metadata(ImportedSourceAttestation::NotAttested),
        )];
        let input = ImportedConversationReconstitutionInput::new(
            owner,
            owner,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            digest,
            1,
            raw_records,
            1,
            entries,
        );
        let retained = input.clone();
        let error = input
            .reconstitute()
            .expect_err("stored hash mismatch is corruption");
        assert_eq!(
            error.failure(),
            ImportedConversationReconstitutionFailure::RawRecordHashMismatch {
                position: ImportedRawRecordPosition::first(),
            }
        );
        assert_eq!(error.into_parts().0, retained);
    }

    #[test]
    fn inv038_message_content_without_source_speaker_fails_closed() {
        let owner = conversation(1);
        let raw = ImportedRawSourceRecord::from_converted(
            br#"{"type":"user","message":{"content":[]}}"#.to_vec(),
            object(("type", ImportedStructuredValue::String(text("user")))),
        );
        let source = metadata(ImportedSourceAttestation::Attested(ImportedSpeaker::User));
        let wrong_speaker = input_entry(
            2,
            owner,
            1,
            1,
            1,
            ImportedSourceAttestation::NotAttested,
            ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::EmptyBlockArray,
            ),
            source,
        );
        assert_eq!(
            ImportedConversation::from_converted_records(
                owner,
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                vec![raw],
                vec![wrong_speaker],
            )
            .expect_err("message content requires an attested source speaker")
            .failure(),
            ImportedConversationReconstitutionFailure::MessageSpeakerUnavailable {
                entry: entry(2),
            }
        );
    }

    #[test]
    fn inv038_reversed_raw_record_mapping_fails_closed() {
        let imported = converted();
        let mut entries = imported
            .entries()
            .iter()
            .map(|entry| {
                ImportedTranscriptEntryInput::new(
                    entry.identity(),
                    entry.conversation(),
                    entry.position(),
                    entry.raw_record_position(),
                    entry.record_entry_position(),
                    entry.source_speaker().clone(),
                    entry.content().clone(),
                    entry.source().clone(),
                )
            })
            .collect::<Vec<_>>();
        entries[2].raw_record_position = ImportedRawRecordPosition::first();
        let raw_records = imported.raw_records().to_vec();
        assert!(matches!(
            ImportedConversation::from_converted_records(
                conversation(1),
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                raw_records,
                entries,
            )
            .expect_err("entry cannot reverse to an earlier raw record")
            .failure(),
            ImportedConversationReconstitutionFailure::EntryRawRecordPositionMismatch { .. }
        ));
    }

    #[test]
    fn inv038_first_entry_cannot_skip_first_raw_record() {
        let owner = conversation(1);
        let raw_records = vec![
            ImportedRawSourceRecord::from_converted(
                br#"{"type":"system"}"#.to_vec(),
                object(("type", ImportedStructuredValue::String(text("system")))),
            ),
            ImportedRawSourceRecord::from_converted(
                br#"{"type":"summary"}"#.to_vec(),
                object(("type", ImportedStructuredValue::String(text("summary")))),
            ),
        ];
        let only_second_record = input_entry(
            2,
            owner,
            1,
            2,
            1,
            ImportedSourceAttestation::NotAttested,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("summary")),
            },
            metadata(ImportedSourceAttestation::NotAttested),
        );

        assert_eq!(
            ImportedConversation::from_converted_records(
                owner,
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                raw_records,
                vec![only_second_record],
            )
            .expect_err("the first raw record must produce an entry")
            .failure(),
            ImportedConversationReconstitutionFailure::RawRecordWithoutEntry {
                position: ImportedRawRecordPosition::first(),
            }
        );
    }

    #[test]
    fn inv038_source_event_rejects_a_message_record_type() {
        let owner = conversation(1);
        let raw = ImportedRawSourceRecord::from_converted(
            br#"{"type":"user"}"#.to_vec(),
            object(("type", ImportedStructuredValue::String(text("user")))),
        );
        let source_event = input_entry(
            2,
            owner,
            1,
            1,
            1,
            ImportedSourceAttestation::NotAttested,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("user")),
            },
            metadata(ImportedSourceAttestation::NotAttested),
        );

        assert_eq!(
            ImportedConversation::from_converted_records(
                owner,
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                vec![raw],
                vec![source_event],
            )
            .expect_err("message discriminators cannot reconstitute as source events")
            .failure(),
            ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch { entry: entry(2) }
        );
    }

    #[test]
    fn inv038_message_speaker_must_match_the_raw_record_type() {
        let owner = conversation(1);
        let raw = ImportedRawSourceRecord::from_converted(
            br#"{"type":"user","message":{"role":"assistant"}}"#.to_vec(),
            object(("type", ImportedStructuredValue::String(text("user")))),
        );
        let contradictory_message = input_entry(
            2,
            owner,
            1,
            1,
            1,
            ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
            ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::ContentNotAttested,
            ),
            metadata(ImportedSourceAttestation::Attested(
                ImportedSpeaker::Assistant,
            )),
        );

        assert_eq!(
            ImportedConversation::from_converted_records(
                owner,
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                vec![raw],
                vec![contradictory_message],
            )
            .expect_err("message speaker must agree with its raw record type")
            .failure(),
            ImportedConversationReconstitutionFailure::SourceRecordTypeMismatch { entry: entry(2) }
        );
    }

    #[test]
    fn inv038_empty_raw_source_record_fails_closed() {
        let owner = conversation(1);
        let raw = ImportedRawSourceRecord::from_converted(
            Vec::new(),
            object(("type", ImportedStructuredValue::String(text("system")))),
        );
        let source_event = input_entry(
            2,
            owner,
            1,
            1,
            1,
            ImportedSourceAttestation::NotAttested,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(text("system")),
            },
            metadata(ImportedSourceAttestation::NotAttested),
        );

        assert_eq!(
            ImportedConversation::from_converted_records(
                owner,
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
                vec![raw],
                vec![source_event],
            )
            .expect_err("a physical JSONL source record cannot be empty")
            .failure(),
            ImportedConversationReconstitutionFailure::EmptyRawRecord {
                position: ImportedRawRecordPosition::first(),
            }
        );
    }

    #[track_caller]
    fn assert_valid_json_number(value: &str) {
        assert_eq!(
            ImportedJsonNumber::try_new(String::from(value))
                .expect("fixture is valid")
                .as_str(),
            value
        );
    }

    #[track_caller]
    fn assert_invalid_json_number(value: &str) {
        let error =
            ImportedJsonNumber::try_new(String::from(value)).expect_err("fixture is invalid");
        assert_eq!(error.value(), value);
    }

    #[test]
    fn imported_json_number_checks_complete_grammar() {
        assert_valid_json_number("0");
        assert_valid_json_number("-0");
        assert_valid_json_number("12");
        assert_valid_json_number("-12.5");
        assert_valid_json_number("1e9");
        assert_valid_json_number("1E-9");

        let empty = ImportedJsonNumber::try_new(String::new()).expect_err("fixture is invalid");
        assert!(empty.value().is_empty());
        assert_invalid_json_number("01");
        assert_invalid_json_number("-");
        assert_invalid_json_number(".1");
        assert_invalid_json_number("1.");
        assert_invalid_json_number("1e");
        assert_invalid_json_number("+1");
        assert_invalid_json_number("NaN");
    }

    #[test]
    fn imported_json_number_debug_redacts_the_source_value() {
        let source_value = "1234567890123456789012345678901234567890e+";
        let error = ImportedJsonNumber::try_new(String::from(source_value))
            .expect_err("fixture has an incomplete exponent");
        assert!(!format!("{error:?}").contains(source_value));
    }
}
