//! Imported transcript content and source attestations for `docs/spec/conversation-import.md`.

use super::structured_value::ImportedSourceAttestation;
use super::structured_value::ImportedStructuredValue;
use super::structured_value::ImportedText;
use std::hash::Hash;

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
    pub(super) message_role: ImportedSourceAttestation<ImportedSpeaker>,
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
    /// One source-defined result block without a more specific normalized
    /// variant.
    SourceResultBlock {
        /// Exact, explicit-null, or omitted source block type.
        source_type: ImportedSourceAttestation<ImportedText>,
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
    /// One record not normalized as a user or assistant message.
    SourceEvent {
        /// Exact, explicit-null, or omitted top-level source type.
        source_type: ImportedSourceAttestation<ImportedText>,
    },
    /// One source-defined block inside a user or assistant message.
    SourceMessageBlock {
        /// Exact, explicit-null, or omitted source block type.
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
