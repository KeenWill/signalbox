//! Error wire representations and validation.

use crate::delegation::DelegationToolRequestState;
use crate::goal::{GoalCommandRejection, SessionLifecycleCommandRejection};
use crate::scalars::{
    CanonicalBlobDigest, CanonicalU64, CanonicalUuid, ConversationImportRejectionClass,
    FrameValidationError, MAX_BLOB_READ_BYTES, PositiveCanonicalU64, deserialize_required_nullable,
};
use crate::settings::{ReasoningLevel, ServiceTier};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Stable server error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// JSON, UTF-8, framing, field, or size validation failed.
    MalformedFrame,
    /// Frame version is not admitted by this implementation.
    UnsupportedVersion,
    /// A boundary value cannot construct the application input.
    InvalidRequest,
    /// A read target does not exist.
    NotFound,
    /// Every recorded replica was proven absent.
    BlobMissing,
    /// Every usable recorded replica failed content verification.
    BlobCorrupt,
    /// A durable identity already names different intent.
    ConflictingReuse,
    /// Canonical command handling recorded a typed rejection.
    Rejected,
    /// A follower fell behind bounded fan-out.
    ResyncRequired,
    /// Infrastructure prevented completion.
    Unavailable,
    /// A remote store may have accepted a deterministic publication.
    PublicationAmbiguous,
    /// Infrastructure obscured whether a requested mutation committed.
    CommitAmbiguous,
    /// Fail-closed corruption or a hub defect stopped the request.
    Internal,
}

/// Closed connection-local holder of the process-wide bulk-ingest permit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkIngestKind {
    ConversationImport,
    BlobUpload,
}

impl BulkIngestKind {
    /// Returns the exact lowercase wire token for terminal diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConversationImport => "conversation_import",
            Self::BlobUpload => "blob_upload",
        }
    }
}

/// Typed durable submit rejection details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RejectionDetail {
    /// Another chunked bulk-ingest kind already owns this connection.
    BulkIngestAlreadyInProgress { active_kind: BulkIngestKind },
    /// An explicit reasoning value is unsupported by the selected model.
    UnsupportedReasoningLevel {
        selection_id: CanonicalUuid,
        requested: ReasoningLevel,
    },
    /// Enabled fast mode is unsupported by the selected model.
    UnsupportedFastMode { selection_id: CanonicalUuid },
    /// An explicit service tier is unsupported by the selected model.
    UnsupportedServiceTier {
        selection_id: CanonicalUuid,
        requested: ServiceTier,
    },
    /// The target session did not exist at command handling.
    SessionNotFound {
        /// Absent target.
        session_id: CanonicalUuid,
    },
    /// An attachment digest had no catalogued verified replica.
    AttachmentBlobNotFound {
        /// The unavailable immutable byte identity.
        digest: CanonicalBlobDigest,
    },
    /// Distinct attachment bytes exceeded the deployment admission ceiling.
    AttachmentByteBudgetExceeded {
        /// Configured maximum aggregate byte count.
        maximum_bytes: PositiveCanonicalU64,
    },
    /// The placement head advanced beyond the caller-observed version.
    SessionPlacementCurrentVersionMismatch {
        session_id: CanonicalUuid,
        expected_placement_version: CanonicalU64,
        current_placement_version: CanonicalU64,
    },
    /// The positive placement-version space was exhausted.
    SessionPlacementVersionExhausted {
        session_id: CanonicalUuid,
        current_placement_version: CanonicalU64,
    },
    /// A durable goal command was rejected by current goal state.
    GoalCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed goal-specific reason.
        reason: GoalCommandRejection,
    },
    /// A turn already held the session slot.
    ActiveTurnPresent {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// A commissioned target already has a live session.
    CommissionTargetBusy {
        /// Authoritative live session currently owning the target.
        session_id: CanonicalUuid,
    },
    /// The caller named a turn that no longer holds the session slot.
    ActiveTurnMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// No turn held the session slot when the caller named one.
    NoActiveTurn {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
    },
    /// The named turn is not parked on the model-call recovery wait, so no
    /// reconciliation decision is owed for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// caller that races the authoritative state instead receives one of the
    /// recorded rejections above.
    TurnNotAwaitingReconciliation {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
    },
    /// A distinct earlier stop was already applied to the active turn.
    InterruptAlreadyApplied {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
    },
    /// The active turn is parked on a tool-approval wait, which a stop can
    /// neither decide nor bypass; the caller denies the pending request first.
    InterruptUnavailableWhileAwaitingApproval {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// A next-safe-point input targeted a turn that is already stopping.
    SafePointUnavailableWhileStopping {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative stopping turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
    },
    /// No logical tool request had the named identity.
    ToolRequestNotFound {
        /// Absent logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named tool request already had a terminal approval resolution.
    ToolRequestAlreadyResolved {
        /// Resolved logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// An earlier request in the same batch still awaited its decision.
    ToolRequestNotEarliestUndecided {
        /// Named logical tool request.
        tool_request_id: CanonicalUuid,
        /// Earliest undecided request owed a decision first.
        earliest_tool_request_id: CanonicalUuid,
    },
    /// The named tool request is not owned by the named session, so no
    /// decision is admitted for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// correctly correlated request instead reaches the canonical decision
    /// command and its recorded rejections above.
    ToolRequestNotInSession {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Tool request the caller named.
        tool_request_id: CanonicalUuid,
    },
    /// The named tool request carries no delegate denial, so no override is
    /// admitted for it.
    ToolRequestNotDelegateDenied {
        /// Tool request without a delegate denial.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegate denial has not reached its terminal denied result.
    ToolRequestNotTerminallyDenied {
        /// Tool request whose denial is still resolving.
        tool_request_id: CanonicalUuid,
    },
    /// An override is already recorded for the named delegate denial.
    ToolDenialAlreadyOverridden {
        /// Already-overridden tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegation request belongs to another turn.
    DelegationRequestNotInTurn {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
        /// Delegation request owned by another turn.
        tool_request_id: CanonicalUuid,
    },
    /// A first execution named a request without executable attempt authority.
    DelegationToolRequestNotExecutable {
        /// Logical delegation tool request.
        tool_request_id: CanonicalUuid,
        /// Exact durable state that prevented first execution.
        state: DelegationToolRequestState,
    },
    /// A spawn request replay changed its immutable arguments.
    DelegationSpawnConflict {
        /// Conflicting logical spawn request.
        tool_request_id: CanonicalUuid,
    },
    /// A generated child identity was already occupied.
    DelegatedChildIdentityCollision {
        /// Colliding child identity.
        child_session_id: CanonicalUuid,
    },
    /// No delegation relationship joined the named session and peer.
    DelegationRelationNotFound {
        /// Invoking session.
        session_id: CanonicalUuid,
        /// Named related peer.
        peer_session_id: CanonicalUuid,
    },
    /// An await request replay changed its immutable arguments.
    DelegationAwaitConflict {
        /// Conflicting logical await request.
        tool_request_id: CanonicalUuid,
    },
    /// A message request replay changed its immutable arguments.
    DelegationMessageConflict {
        /// Conflicting logical message request.
        tool_request_id: CanonicalUuid,
    },
    /// A daemon-minted message identity was already claimed.
    DelegationMessageIdentityCollision {
        /// Colliding message identity.
        message_id: CanonicalUuid,
    },
    /// A relationship cannot allocate another positive event ordinal.
    DelegationEventOrdinalExhausted {
        /// Relationship's spawning request identity.
        spawning_request_id: CanonicalUuid,
        /// Last representable event ordinal.
        last: CanonicalU64,
    },
    /// A recipient cannot allocate another positive delivery sequence.
    DelegationDeliverySequenceExhausted {
        /// Recipient whose delivery sequence is exhausted.
        recipient_session_id: CanonicalUuid,
        /// Last representable delivery sequence.
        last: CanonicalU64,
    },
    /// The caller observed stale defaults.
    DefaultsVersionMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Caller version.
        expected: CanonicalU64,
        /// Current authoritative version.
        current: CanonicalU64,
    },
    /// The selected alias had no current definition.
    UnknownModelAlias {
        /// Target session.
        session_id: CanonicalUuid,
        /// Unknown alias.
        alias_id: CanonicalUuid,
    },
    /// The session acceptance ordinal was exhausted.
    AcceptancePositionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable position.
        last: CanonicalU64,
    },
    /// The session defaults epoch ordinal was exhausted.
    DefaultsVersionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable epoch.
        current: CanonicalU64,
    },
    /// No imported conversation had the named identity.
    ///
    /// The absent target is an imported conversation, never a session: an
    /// imported conversation is durable record and creates no session.
    ImportedConversationNotFound {
        /// Absent imported conversation.
        imported_conversation_id: CanonicalUuid,
    },
    /// The named imported conversation exists but has no such position.
    ///
    /// Imported positions are the one-based contiguous sequence
    /// `1..=last_position`; the identity was valid and only the ordinal was
    /// outside it.
    ImportedFrontierPositionOutOfRange {
        /// Imported conversation whose positions bound the request.
        imported_conversation_id: CanonicalUuid,
        /// Exact position the caller named.
        requested_position: CanonicalU64,
        /// Greatest selectable position on that conversation.
        last_position: CanonicalU64,
    },
    /// This connection already has one in-progress conversation import.
    ConversationImportAlreadyInProgress {},
    /// This connection has no in-progress conversation import.
    ConversationImportNotInProgress {},
    /// The declared or observed source size exceeds the configured total bound.
    ConversationImportSourceTooLarge {
        /// Configured maximum assembled source size.
        limit_bytes: CanonicalU64,
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact observed size at append or commit, or null at begin.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        actual_size_bytes: Option<CanonicalU64>,
    },
    /// The observed source size did not equal the size declared at begin.
    ConversationImportSourceSizeMismatch {
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact number of source bytes observed across append requests.
        actual_size_bytes: CanonicalU64,
    },
    /// A converter rejected the complete source with content-silent evidence.
    ConversationImportConversionFailed {
        /// Closed converter failure class.
        class: ConversationImportRejectionClass,
        /// One-based offending physical record, or null when not applicable.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        record_ordinal: Option<CanonicalU64>,
    },
    /// This connection already has one in-progress blob upload.
    BlobUploadAlreadyInProgress {},
    /// This connection has no in-progress blob upload.
    BlobUploadNotInProgress {},
    /// The declared blob length fell outside the configured inclusive range.
    BlobUploadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        declared_length_bytes: CanonicalU64,
    },
    /// Appending the chunk would exceed the length declared at begin.
    BlobUploadSizeExceeded {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The appended byte count differed from the length declared at begin.
    BlobUploadLengthMismatch {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The assembled bytes differed from the digest declared at begin.
    BlobUploadDigestMismatch {
        expected_digest: CanonicalBlobDigest,
        actual_digest: CanonicalBlobDigest,
    },
    /// The requested direct-read length fell outside the inclusive wire bound.
    BlobReadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        requested_length_bytes: CanonicalU64,
    },
    /// The requested exact half-open range is not contained by the blob.
    BlobReadRangeOutOfBounds {
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
        blob_length_bytes: CanonicalU64,
    },
    /// A durable session-lifecycle command was rejected by current state.
    SessionLifecycleCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed reason.
        reason: SessionLifecycleCommandRejection,
    },
}

impl RejectionDetail {
    pub(crate) const fn is_bulk_ingest(self) -> bool {
        matches!(self, Self::BulkIngestAlreadyInProgress { .. })
    }

    pub(crate) const fn is_blob_upload(self) -> bool {
        matches!(
            self,
            Self::BlobUploadAlreadyInProgress {}
                | Self::BlobUploadNotInProgress {}
                | Self::BlobUploadLengthOutOfRange { .. }
                | Self::BlobUploadSizeExceeded { .. }
                | Self::BlobUploadLengthMismatch { .. }
                | Self::BlobUploadDigestMismatch { .. }
        )
    }

    pub(crate) const fn is_blob_read(self) -> bool {
        matches!(
            self,
            Self::BlobReadLengthOutOfRange { .. } | Self::BlobReadRangeOutOfBounds { .. }
        )
    }

    pub(crate) const fn is_conversation_import(self) -> bool {
        match self {
            Self::ConversationImportAlreadyInProgress {}
            | Self::ConversationImportNotInProgress {}
            | Self::ConversationImportSourceTooLarge { .. }
            | Self::ConversationImportSourceSizeMismatch { .. }
            | Self::ConversationImportConversionFailed { .. } => true,
            Self::BlobUploadAlreadyInProgress {}
            | Self::BlobUploadNotInProgress {}
            | Self::BlobUploadLengthOutOfRange { .. }
            | Self::BlobUploadSizeExceeded { .. }
            | Self::BlobUploadLengthMismatch { .. }
            | Self::BlobUploadDigestMismatch { .. }
            | Self::BlobReadLengthOutOfRange { .. }
            | Self::BlobReadRangeOutOfBounds { .. }
            | Self::BulkIngestAlreadyInProgress { .. }
            | Self::SessionNotFound { .. }
            | Self::AttachmentBlobNotFound { .. }
            | Self::AttachmentByteBudgetExceeded { .. }
            | Self::UnsupportedReasoningLevel { .. }
            | Self::UnsupportedFastMode { .. }
            | Self::UnsupportedServiceTier { .. }
            | Self::SessionPlacementCurrentVersionMismatch { .. }
            | Self::SessionPlacementVersionExhausted { .. }
            | Self::GoalCommandRejected { .. }
            | Self::SessionLifecycleCommandRejected { .. }
            | Self::ActiveTurnPresent { .. }
            | Self::CommissionTargetBusy { .. }
            | Self::ActiveTurnMismatch { .. }
            | Self::NoActiveTurn { .. }
            | Self::TurnNotAwaitingReconciliation { .. }
            | Self::InterruptAlreadyApplied { .. }
            | Self::InterruptUnavailableWhileAwaitingApproval { .. }
            | Self::SafePointUnavailableWhileStopping { .. }
            | Self::ToolRequestNotFound { .. }
            | Self::ToolRequestAlreadyResolved { .. }
            | Self::ToolRequestNotEarliestUndecided { .. }
            | Self::ToolRequestNotInSession { .. }
            | Self::ToolRequestNotDelegateDenied { .. }
            | Self::ToolRequestNotTerminallyDenied { .. }
            | Self::ToolDenialAlreadyOverridden { .. }
            | Self::DelegationRequestNotInTurn { .. }
            | Self::DelegationToolRequestNotExecutable { .. }
            | Self::DelegationSpawnConflict { .. }
            | Self::DelegatedChildIdentityCollision { .. }
            | Self::DelegationRelationNotFound { .. }
            | Self::DelegationAwaitConflict { .. }
            | Self::DelegationMessageConflict { .. }
            | Self::DelegationMessageIdentityCollision { .. }
            | Self::DelegationEventOrdinalExhausted { .. }
            | Self::DelegationDeliverySequenceExhausted { .. }
            | Self::DefaultsVersionMismatch { .. }
            | Self::UnknownModelAlias { .. }
            | Self::AcceptancePositionExhausted { .. }
            | Self::DefaultsVersionExhausted { .. }
            | Self::ImportedConversationNotFound { .. }
            | Self::ImportedFrontierPositionOutOfRange { .. } => false,
        }
    }
}

#[derive(signalbox_derive::Accessors)]
/// Presence-checked rejection detail on an error message.
///
/// An absent value omits the JSON member. A present JSON `null` is rejected
/// rather than being treated as absence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorDetail(
    /// Returns the typed rejection detail when present.
    #[get(copy, as = "value")]
    Option<RejectionDetail>,
);

impl ErrorDetail {
    /// Omits rejection detail from a non-rejection error.
    pub const fn none() -> Self {
        Self(None)
    }

    /// Includes exact durable-rejection detail.
    pub const fn rejected(detail: RejectionDetail) -> Self {
        Self(Some(detail))
    }

    /// Includes typed import evidence on an invalid request.
    pub const fn invalid_request(detail: RejectionDetail) -> Self {
        Self(Some(detail))
    }

    pub(crate) const fn is_absent(&self) -> bool {
        self.0.is_none()
    }
}

impl Serialize for ErrorDetail {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        match self.0 {
            Some(detail) => detail.serialize(serializer),
            None => serializer.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for ErrorDetail {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        RejectionDetail::deserialize(deserializer).map(Self::rejected)
    }
}

pub(crate) fn validate_rejection_detail(
    detail: RejectionDetail,
) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::SessionPlacementCurrentVersionMismatch {
            expected_placement_version,
            current_placement_version,
            ..
        } => {
            expected_placement_version.value() > 0
                && current_placement_version.value() > 0
                && expected_placement_version != current_placement_version
        }
        RejectionDetail::SessionPlacementVersionExhausted {
            current_placement_version,
            ..
        } => current_placement_version.value() == u64::MAX,
        RejectionDetail::DelegationEventOrdinalExhausted { last, .. } => last.value() == u64::MAX,
        RejectionDetail::DelegationDeliverySequenceExhausted { last, .. } => {
            last.value() == u64::MAX
        }
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => true,
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {}
        | RejectionDetail::ConversationImportSourceTooLarge { .. }
        | RejectionDetail::ConversationImportSourceSizeMismatch { .. }
        | RejectionDetail::ConversationImportConversionFailed { .. }
        | RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ErrorDetailShape)
    }
}

pub(crate) fn validate_conversation_import_detail(
    detail: RejectionDetail,
) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {} => true,
        RejectionDetail::ConversationImportSourceTooLarge {
            limit_bytes,
            declared_size_bytes,
            actual_size_bytes,
        } => {
            limit_bytes.value() > 0
                && match actual_size_bytes {
                    Some(actual) => {
                        actual.value() > limit_bytes.value()
                            && (declared_size_bytes.value() <= limit_bytes.value()
                                || declared_size_bytes == actual)
                    }
                    None => declared_size_bytes.value() > limit_bytes.value(),
                }
        }
        RejectionDetail::ConversationImportSourceSizeMismatch {
            declared_size_bytes,
            actual_size_bytes,
        } => declared_size_bytes != actual_size_bytes,
        RejectionDetail::ConversationImportConversionFailed {
            class,
            record_ordinal,
        } => match class {
            ConversationImportRejectionClass::EmptySource => record_ordinal.is_none(),
            ConversationImportRejectionClass::BlankLine
            | ConversationImportRejectionClass::InvalidUtf8
            | ConversationImportRejectionClass::InvalidJson
            | ConversationImportRejectionClass::JsonDepthExceeded
            | ConversationImportRejectionClass::TopLevelNotObject
            | ConversationImportRejectionClass::InvalidRecordType
            | ConversationImportRejectionClass::InvalidSourceMetadata
            | ConversationImportRejectionClass::InvalidMessageEnvelope
            | ConversationImportRejectionClass::InvalidMessageRole
            | ConversationImportRejectionClass::MessageRoleMismatch
            | ConversationImportRejectionClass::InvalidMessageContent
            | ConversationImportRejectionClass::InvalidContentBlock
            | ConversationImportRejectionClass::InvalidToolResultBlock
            | ConversationImportRejectionClass::InvalidReasoning
            | ConversationImportRejectionClass::InvalidToolCall
            | ConversationImportRejectionClass::InvalidToolResult => {
                record_ordinal.is_some_and(|ordinal| ordinal.value() > 0)
            }
        },
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::SessionPlacementCurrentVersionMismatch { .. }
        | RejectionDetail::SessionPlacementVersionExhausted { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DelegationEventOrdinalExhausted { .. }
        | RejectionDetail::DelegationDeliverySequenceExhausted { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => false,
        RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ConversationImportShape)
    }
}

pub(crate) fn validate_blob_upload_detail(
    detail: RejectionDetail,
) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {} => true,
        RejectionDetail::BlobUploadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            declared_length_bytes,
        } => {
            min_length_bytes.value() > 0
                && min_length_bytes.value() <= max_length_bytes.value()
                && (declared_length_bytes.value() < min_length_bytes.value()
                    || declared_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobUploadSizeExceeded {
            expected_length_bytes,
            actual_length_bytes,
        } => {
            expected_length_bytes.value() > 0
                && actual_length_bytes.value() > expected_length_bytes.value()
        }
        RejectionDetail::BlobUploadLengthMismatch {
            expected_length_bytes,
            actual_length_bytes,
        } => expected_length_bytes.value() > 0 && expected_length_bytes != actual_length_bytes,
        RejectionDetail::BlobUploadDigestMismatch {
            expected_digest,
            actual_digest,
        } => expected_digest != actual_digest,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobUploadShape)
    }
}

pub(crate) fn validate_blob_read_detail(
    detail: RejectionDetail,
) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobReadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            requested_length_bytes,
        } => {
            min_length_bytes.value() == 1
                && max_length_bytes.value() == MAX_BLOB_READ_BYTES as u64
                && (requested_length_bytes.value() < min_length_bytes.value()
                    || requested_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobReadRangeOutOfBounds {
            offset_bytes,
            length_bytes,
            blob_length_bytes,
            ..
        } => {
            (1..=MAX_BLOB_READ_BYTES as u64).contains(&length_bytes.value())
                && blob_length_bytes.value() > 0
                && (offset_bytes
                    .value()
                    .checked_add(length_bytes.value())
                    .is_none_or(|end| end > blob_length_bytes.value()))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobReadShape)
    }
}
