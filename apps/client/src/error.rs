use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    string::FromUtf8Error,
};

use signalbox_process_protocol::{
    AnthropicServiceTier, CodexCliServiceTier, ConversationImportRejectionClass, ErrorCode,
    ErrorDetail, FailedModelCallCause, FrameDecodeError, FrameEncodeError, GoalCommandRejection,
    OpenAiServiceTier, ReasoningLevel, RejectionDetail, RunnerNonLostConnectionState,
    RunnerPlacementRecoveryState, ServiceTier,
};

#[derive(Debug)]
pub(crate) enum ClientError {
    Io(io::Error),
    SourceFile(io::Error),
    SystemPromptFile(io::Error),
    GoalTextFile {
        path: PathBuf,
        source: io::Error,
    },
    DelegationContentFile {
        path: PathBuf,
        source: io::Error,
    },
    DelegationContentFileUtf8 {
        path: PathBuf,
        source: FromUtf8Error,
    },
    ReviewInputFile(io::Error),
    ReviewInputJson(serde_json::Error),
    ReviewInputExceedsFrame,
    ScanDirectory(io::Error),
    SourceExceedsFrame,
    ScanIncomplete {
        skipped_files: usize,
    },
    Encode(FrameEncodeError),
    Decode(FrameDecodeError),
    Protocol(&'static str),
    Remote {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    AmbiguousMutation,
    Input(&'static str),
    TurnRecoveryRequired,
    RunnerRecoveryRequired,
    TurnFailed(Option<FailedModelCallCause>),
    TurnRefused,
    TurnCancelled,
    TurnReconciliationRequired,
}

impl ClientError {
    pub(crate) const fn remote(code: ErrorCode, message: String, detail: ErrorDetail) -> Self {
        Self::Remote {
            code,
            message,
            detail,
        }
    }

    pub(crate) fn source_file(error: io::Error) -> Self {
        Self::SourceFile(error)
    }

    pub(crate) fn system_prompt_file(error: io::Error) -> Self {
        Self::SystemPromptFile(error)
    }

    pub(crate) fn goal_text_file(path: &Path, source: io::Error) -> Self {
        Self::GoalTextFile {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn delegation_content_file(path: &Path, source: io::Error) -> Self {
        Self::DelegationContentFile {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn delegation_content_file_utf8(path: &Path, source: FromUtf8Error) -> Self {
        Self::DelegationContentFileUtf8 {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn review_input_file(error: io::Error) -> Self {
        Self::ReviewInputFile(error)
    }

    pub(crate) fn review_input_json(error: serde_json::Error) -> Self {
        Self::ReviewInputJson(error)
    }

    pub(crate) fn scan_directory(error: io::Error) -> Self {
        Self::ScanDirectory(error)
    }

    pub(crate) fn mutation(self) -> Self {
        match self {
            Self::Remote {
                code: ErrorCode::CommitAmbiguous,
                ..
            } => Self::AmbiguousMutation,
            Self::Remote { .. }
            | Self::SourceFile(_)
            | Self::SystemPromptFile(_)
            | Self::GoalTextFile { .. }
            | Self::DelegationContentFile { .. }
            | Self::DelegationContentFileUtf8 { .. }
            | Self::ReviewInputFile(_)
            | Self::ReviewInputJson(_)
            | Self::ReviewInputExceedsFrame
            | Self::SourceExceedsFrame => self,
            Self::Io(_)
            | Self::ScanDirectory(_)
            | Self::ScanIncomplete { .. }
            | Self::Encode(_)
            | Self::Decode(_)
            | Self::Protocol(_)
            | Self::AmbiguousMutation
            | Self::Input(_)
            | Self::TurnRecoveryRequired
            | Self::RunnerRecoveryRequired
            | Self::TurnFailed(_)
            | Self::TurnRefused
            | Self::TurnCancelled
            | Self::TurnReconciliationRequired => Self::AmbiguousMutation,
        }
    }

    pub(crate) const fn is_ambiguous_mutation(&self) -> bool {
        matches!(self, Self::AmbiguousMutation)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("local process communication failed"),
            Self::SourceFile(_) => {
                formatter.write_str("the conversation import source file could not be read")
            }
            Self::SystemPromptFile(_) => {
                formatter.write_str("the system prompt file could not be read")
            }
            Self::GoalTextFile { path, source } => write!(
                formatter,
                "the goal text file '{}' could not be read: {source}",
                path.display()
            ),
            Self::DelegationContentFile { path, source } => write!(
                formatter,
                "the delegation content file '{}' could not be read: {source}",
                path.display()
            ),
            Self::DelegationContentFileUtf8 { path, source } => write!(
                formatter,
                "the delegation content file '{}' is not valid UTF-8: {source}",
                path.display()
            ),
            Self::ReviewInputFile(_) => {
                formatter.write_str("the review JSON input file could not be read")
            }
            Self::ReviewInputJson(_) => {
                formatter.write_str("the review JSON input file is not an exact admitted shape")
            }
            Self::ReviewInputExceedsFrame => {
                formatter.write_str("the review JSON input exceeds the process frame bound")
            }
            Self::ScanDirectory(_) => {
                formatter.write_str("the conversation import scan directory could not be read")
            }
            Self::SourceExceedsFrame => formatter.write_str(
                "the conversation import source cannot fit within the process frame bound",
            ),
            Self::ScanIncomplete { skipped_files } => write!(
                formatter,
                "the conversation import scan completed with {skipped_files} skipped file(s)"
            ),
            Self::Encode(_) => formatter.write_str("the client could not encode its request"),
            Self::Decode(_) => formatter.write_str("the server violated the process protocol"),
            Self::Protocol(message) => write!(
                formatter,
                "the server violated the process protocol: {message}"
            ),
            Self::Remote {
                code,
                message,
                detail,
            } => {
                write!(formatter, "{}: {message}", error_code_name(*code))?;
                if let Some(detail) = detail.value() {
                    write!(formatter, " ({})", RejectionDisplay(detail))?;
                }
                Ok(())
            }
            Self::AmbiguousMutation => formatter.write_str(
                "the mutation outcome may be ambiguous; retry the original command with the same \
                 arguments and exact input, using any printed recovery values",
            ),
            Self::Input(message) => formatter.write_str(message),
            Self::TurnRecoveryRequired => formatter.write_str(
                "the submitted turn requires model-call recovery that the terminal cannot perform",
            ),
            Self::RunnerRecoveryRequired => formatter.write_str(
                "the submitted turn awaits lost-runner replacement or stop_turn before \
                     abandonment",
            ),
            Self::TurnFailed(None) => formatter.write_str("the submitted turn failed"),
            Self::TurnFailed(Some(cause)) => write!(
                formatter,
                "the submitted turn failed: {}",
                failed_model_call_cause(*cause)
            ),
            Self::TurnRefused => formatter.write_str("the submitted turn was refused"),
            Self::TurnCancelled => formatter.write_str("the submitted turn was cancelled"),
            Self::TurnReconciliationRequired => {
                formatter.write_str("the submitted turn requires external reconciliation")
            }
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error)
            | Self::SourceFile(error)
            | Self::SystemPromptFile(error)
            | Self::GoalTextFile { source: error, .. }
            | Self::DelegationContentFile { source: error, .. }
            | Self::ReviewInputFile(error)
            | Self::ScanDirectory(error) => Some(error),
            Self::DelegationContentFileUtf8 { source, .. } => Some(source),
            Self::ReviewInputJson(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Protocol(_)
            | Self::Remote { .. }
            | Self::AmbiguousMutation
            | Self::Input(_)
            | Self::ReviewInputExceedsFrame
            | Self::SourceExceedsFrame
            | Self::ScanIncomplete { .. }
            | Self::TurnRecoveryRequired
            | Self::RunnerRecoveryRequired
            | Self::TurnFailed(_)
            | Self::TurnRefused
            | Self::TurnCancelled
            | Self::TurnReconciliationRequired => None,
        }
    }
}

const fn failed_model_call_cause(cause: FailedModelCallCause) -> &'static str {
    match cause {
        FailedModelCallCause::CredentialRejected => "the provider rejected the credential",
        FailedModelCallCause::PermissionDenied => "the credential lacks permission",
        FailedModelCallCause::InvalidRequest => "the provider rejected the request as invalid",
        FailedModelCallCause::TargetNotFound => "the requested model or resource was not found",
        FailedModelCallCause::RequestTooLarge => "the request exceeded a provider size limit",
        FailedModelCallCause::RateLimited => "the provider rate-limited the request; retry later",
        FailedModelCallCause::QuotaExhausted => "the provider quota is exhausted",
        FailedModelCallCause::Overloaded => "the provider is overloaded; retry later",
        FailedModelCallCause::ProviderInternal => "the provider reported an internal error",
        FailedModelCallCause::Unrecognized => "the provider reported an unrecognized error",
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FrameEncodeError> for ClientError {
    fn from(error: FrameEncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<FrameDecodeError> for ClientError {
    fn from(error: FrameDecodeError) -> Self {
        Self::Decode(error)
    }
}

const fn error_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::MalformedFrame => "malformed_frame",
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::InvalidRequest => "invalid_request",
        ErrorCode::NotFound => "not_found",
        ErrorCode::ConflictingReuse => "conflicting_reuse",
        ErrorCode::Rejected => "rejected",
        ErrorCode::ResyncRequired => "resync_required",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::CommitAmbiguous => "commit_ambiguous",
        ErrorCode::Internal => "internal",
    }
}

const fn goal_command_rejection_name(reason: GoalCommandRejection) -> &'static str {
    match reason {
        GoalCommandRejection::SessionNotFound => "session_not_found",
        GoalCommandRejection::GoalAlreadyAttached => "goal_already_attached",
        GoalCommandRejection::GoalNotAttached => "goal_not_attached",
        GoalCommandRejection::UnknownModelAlias => "unknown_model_alias",
        GoalCommandRejection::AcceptancePositionExhausted => "acceptance_position_exhausted",
        GoalCommandRejection::RequiresBlocked => "requires_blocked",
        GoalCommandRejection::RequiresPursuingOrBlocked => "requires_pursuing_or_blocked",
        GoalCommandRejection::GenerationExhausted => "generation_exhausted",
        GoalCommandRejection::EventOrdinalExhausted => "event_ordinal_exhausted",
    }
}

const fn runner_non_lost_connection_state_name(
    state: RunnerNonLostConnectionState,
) -> &'static str {
    match state {
        RunnerNonLostConnectionState::Connected => "connected",
        RunnerNonLostConnectionState::Suspect => "suspect",
        RunnerNonLostConnectionState::Shutdown => "shutdown",
    }
}

const fn runner_placement_recovery_state_name(state: RunnerPlacementRecoveryState) -> &'static str {
    match state {
        RunnerPlacementRecoveryState::Unpinned => "unpinned",
        RunnerPlacementRecoveryState::Pinned => "pinned",
        RunnerPlacementRecoveryState::RunnerAbandoned => "runner_abandoned",
    }
}

struct RejectionDisplay(RejectionDetail);

impl fmt::Display for RejectionDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            RejectionDetail::UnsupportedReasoningLevel {
                selection_id,
                requested,
            } => write!(
                formatter,
                "unsupported_reasoning_level selection={selection_id} reasoning_level={}",
                reasoning_level_name(requested)
            ),
            RejectionDetail::UnsupportedFastMode { selection_id } => {
                write!(formatter, "unsupported_fast_mode selection={selection_id}")
            }
            RejectionDetail::UnsupportedServiceTier {
                selection_id,
                requested,
            } => write!(
                formatter,
                "unsupported_service_tier selection={selection_id} service_tier={}",
                service_tier_name(requested)
            ),
            RejectionDetail::SessionNotFound { session_id } => {
                write!(formatter, "session_not_found session={session_id}")
            }
            RejectionDetail::RunnerPlacementNotFound { session_id } => {
                write!(formatter, "runner_placement_not_found session={session_id}")
            }
            RejectionDetail::PlacementRevisionMismatch {
                session_id,
                expected,
                current,
            } => write!(
                formatter,
                "placement_revision_mismatch session={session_id} expected={} current={}",
                expected.value(),
                current.value()
            ),
            RejectionDetail::PlacementNotLost {
                session_id,
                placement_revision,
                state,
            } => write!(
                formatter,
                "placement_not_lost session={session_id} placement_revision={} state={}",
                placement_revision.value(),
                runner_placement_recovery_state_name(state)
            ),
            RejectionDetail::ActiveTurnRequiresExistingControl {
                session_id,
                active_turn_id,
            } => write!(
                formatter,
                "active_turn_requires_existing_control session={session_id} \
                 active_turn={active_turn_id}"
            ),
            RejectionDetail::NoPendingRunnerEnrollment {} => {
                formatter.write_str("no_pending_runner_enrollment")
            }
            RejectionDetail::PendingRequestMismatch { pending_request_id } => write!(
                formatter,
                "pending_request_mismatch pending_request={pending_request_id}"
            ),
            RejectionDetail::PendingRequestDisconnected { pending_request_id } => write!(
                formatter,
                "pending_request_disconnected pending_request={pending_request_id}"
            ),
            RejectionDetail::ActiveRunnerNotLost {
                runner_id,
                connection_state,
            } => write!(
                formatter,
                "active_runner_not_lost runner={runner_id} connection_state={}",
                runner_non_lost_connection_state_name(connection_state)
            ),
            RejectionDetail::SessionPlacementCurrentVersionMismatch {
                session_id,
                expected_placement_version,
                current_placement_version,
            } => write!(
                formatter,
                "session_placement_current_version_mismatch session={session_id} \
                 expected_placement_version={} current_placement_version={}",
                expected_placement_version.value(),
                current_placement_version.value()
            ),
            RejectionDetail::SessionPlacementVersionExhausted {
                session_id,
                current_placement_version,
            } => write!(
                formatter,
                "session_placement_version_exhausted session={session_id} \
                 current_placement_version={}",
                current_placement_version.value()
            ),
            RejectionDetail::GoalCommandRejected { session_id, reason } => write!(
                formatter,
                "goal_command_rejected session={session_id} reason={}",
                goal_command_rejection_name(reason)
            ),
            RejectionDetail::ActiveTurnPresent {
                session_id,
                active_turn_id,
            } => write!(
                formatter,
                "active_turn_present session={session_id} active_turn={active_turn_id}"
            ),
            RejectionDetail::ActiveTurnMismatch {
                session_id,
                expected_active_turn_id,
                active_turn_id,
            } => write!(
                formatter,
                "active_turn_mismatch session={session_id} \
                 expected_active_turn={expected_active_turn_id} active_turn={active_turn_id}"
            ),
            RejectionDetail::NoActiveTurn {
                session_id,
                expected_active_turn_id,
            } => write!(
                formatter,
                "no_active_turn session={session_id} \
                 expected_active_turn={expected_active_turn_id}"
            ),
            RejectionDetail::TurnNotAwaitingReconciliation {
                session_id,
                turn_id,
            } => write!(
                formatter,
                "turn_not_awaiting_reconciliation session={session_id} turn={turn_id}"
            ),
            RejectionDetail::InterruptAlreadyApplied {
                session_id,
                active_turn_id,
                existing_command_id,
            } => write!(
                formatter,
                "interrupt_already_applied session={session_id} active_turn={active_turn_id} \
                 existing_command={existing_command_id}"
            ),
            RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
                session_id,
                active_turn_id,
            } => write!(
                formatter,
                "interrupt_unavailable_while_awaiting_approval session={session_id} \
                 active_turn={active_turn_id}; deny the pending tool request first"
            ),
            RejectionDetail::SafePointUnavailableWhileStopping {
                session_id,
                active_turn_id,
                existing_command_id,
            } => write!(
                formatter,
                "safe_point_unavailable_while_stopping session={session_id} \
                 active_turn={active_turn_id} existing_command={existing_command_id}"
            ),
            RejectionDetail::ToolRequestNotFound { tool_request_id } => {
                write!(
                    formatter,
                    "tool_request_not_found request={tool_request_id}"
                )
            }
            RejectionDetail::ToolRequestAlreadyResolved { tool_request_id } => write!(
                formatter,
                "tool_request_already_resolved request={tool_request_id}"
            ),
            RejectionDetail::ToolRequestNotEarliestUndecided {
                tool_request_id,
                earliest_tool_request_id,
            } => write!(
                formatter,
                "tool_request_not_earliest_undecided request={tool_request_id} \
                 earliest={earliest_tool_request_id}"
            ),
            RejectionDetail::ToolRequestNotInSession {
                session_id,
                tool_request_id,
            } => write!(
                formatter,
                "tool_request_not_in_session session={session_id} request={tool_request_id}"
            ),
            RejectionDetail::DelegationRequestNotInTurn {
                session_id,
                turn_id,
                tool_request_id,
            } => write!(
                formatter,
                "delegation_request_not_in_turn session={session_id} turn={turn_id} \
                 request={tool_request_id}"
            ),
            RejectionDetail::DelegationToolRequestNotExecutable {
                tool_request_id,
                state,
            } => write!(
                formatter,
                "delegation_tool_request_not_executable request={tool_request_id} state={}",
                state.as_str()
            ),
            RejectionDetail::DelegationSpawnConflict { tool_request_id } => write!(
                formatter,
                "delegation_spawn_conflict request={tool_request_id}"
            ),
            RejectionDetail::DelegatedChildIdentityCollision { child_session_id } => write!(
                formatter,
                "delegated_child_identity_collision child_session={child_session_id}"
            ),
            RejectionDetail::DelegationRelationNotFound {
                session_id,
                peer_session_id,
            } => write!(
                formatter,
                "delegation_relation_not_found session={session_id} peer_session={peer_session_id}"
            ),
            RejectionDetail::DelegationAwaitConflict { tool_request_id } => write!(
                formatter,
                "delegation_await_conflict request={tool_request_id}"
            ),
            RejectionDetail::DelegationMessageConflict { tool_request_id } => write!(
                formatter,
                "delegation_message_conflict request={tool_request_id}"
            ),
            RejectionDetail::DelegationMessageIdentityCollision { message_id } => write!(
                formatter,
                "delegation_message_identity_collision message={message_id}"
            ),
            RejectionDetail::DelegationEventOrdinalExhausted {
                spawning_request_id,
                last,
            } => write!(
                formatter,
                "delegation_event_ordinal_exhausted spawning_request={spawning_request_id} last={}",
                last.value()
            ),
            RejectionDetail::DelegationDeliverySequenceExhausted {
                recipient_session_id,
                last,
            } => write!(
                formatter,
                "delegation_delivery_sequence_exhausted recipient_session={recipient_session_id} \
                 last={}",
                last.value()
            ),
            RejectionDetail::DefaultsVersionMismatch {
                session_id,
                expected,
                current,
            } => write!(
                formatter,
                "defaults_version_mismatch session={session_id} expected={} current={}",
                expected.value(),
                current.value()
            ),
            RejectionDetail::UnknownModelAlias {
                session_id,
                alias_id,
            } => write!(
                formatter,
                "unknown_model_alias session={session_id} alias={alias_id}"
            ),
            RejectionDetail::AcceptancePositionExhausted { session_id, last } => write!(
                formatter,
                "acceptance_position_exhausted session={session_id} last={}",
                last.value()
            ),
            RejectionDetail::DefaultsVersionExhausted {
                session_id,
                current,
            } => write!(
                formatter,
                "defaults_version_exhausted session={session_id} current={}",
                current.value()
            ),
            RejectionDetail::ImportedConversationNotFound {
                imported_conversation_id,
            } => write!(
                formatter,
                "imported_conversation_not_found \
                 imported_conversation={imported_conversation_id}"
            ),
            RejectionDetail::ImportedFrontierPositionOutOfRange {
                imported_conversation_id,
                requested_position,
                last_position,
            } => write!(
                formatter,
                "imported_frontier_position_out_of_range \
                 imported_conversation={imported_conversation_id} requested={} \
                 first_position=1 last_position={}",
                requested_position.value(),
                last_position.value()
            ),
            RejectionDetail::ConversationImportAlreadyInProgress {} => {
                formatter.write_str("conversation_import_already_in_progress")
            }
            RejectionDetail::ConversationImportNotInProgress {} => {
                formatter.write_str("conversation_import_not_in_progress")
            }
            RejectionDetail::ConversationImportSourceTooLarge {
                limit_bytes,
                declared_size_bytes,
                actual_size_bytes: None,
            } => write!(
                formatter,
                "conversation_import_source_too_large limit_bytes={} declared_size_bytes={}",
                limit_bytes.value(),
                declared_size_bytes.value()
            ),
            RejectionDetail::ConversationImportSourceTooLarge {
                limit_bytes,
                declared_size_bytes,
                actual_size_bytes: Some(actual_size_bytes),
            } => write!(
                formatter,
                "conversation_import_source_too_large limit_bytes={} declared_size_bytes={} \
                 actual_size_bytes={}",
                limit_bytes.value(),
                declared_size_bytes.value(),
                actual_size_bytes.value()
            ),
            RejectionDetail::ConversationImportSourceSizeMismatch {
                declared_size_bytes,
                actual_size_bytes,
            } => write!(
                formatter,
                "conversation_import_source_size_mismatch declared_size_bytes={} \
                 actual_size_bytes={}",
                declared_size_bytes.value(),
                actual_size_bytes.value()
            ),
            RejectionDetail::ConversationImportConversionFailed {
                class,
                record_ordinal: None,
            } => write!(
                formatter,
                "conversation_import_conversion_failed class={}",
                conversation_import_rejection_class_name(class)
            ),
            RejectionDetail::ConversationImportConversionFailed {
                class,
                record_ordinal: Some(record_ordinal),
            } => write!(
                formatter,
                "conversation_import_conversion_failed class={} record_ordinal={}",
                conversation_import_rejection_class_name(class),
                record_ordinal.value()
            ),
        }
    }
}

const fn reasoning_level_name(value: ReasoningLevel) -> &'static str {
    match value {
        ReasoningLevel::None => "none",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    }
}

const fn service_tier_name(value: ServiceTier) -> &'static str {
    match value {
        ServiceTier::Anthropic(AnthropicServiceTier::Auto) => "anthropic:auto",
        ServiceTier::Anthropic(AnthropicServiceTier::StandardOnly) => "anthropic:standard_only",
        ServiceTier::OpenAi(OpenAiServiceTier::Auto) => "openai:auto",
        ServiceTier::OpenAi(OpenAiServiceTier::Default) => "openai:default",
        ServiceTier::OpenAi(OpenAiServiceTier::Flex) => "openai:flex",
        ServiceTier::OpenAi(OpenAiServiceTier::Scale) => "openai:scale",
        ServiceTier::OpenAi(OpenAiServiceTier::Priority) => "openai:priority",
        ServiceTier::OpenAi(OpenAiServiceTier::Fast) => "openai:fast",
        ServiceTier::CodexCli(CodexCliServiceTier::Default) => "codex_cli:default",
        ServiceTier::CodexCli(CodexCliServiceTier::Priority) => "codex_cli:priority",
        ServiceTier::CodexCli(CodexCliServiceTier::Flex) => "codex_cli:flex",
    }
}

const fn conversation_import_rejection_class_name(
    class: ConversationImportRejectionClass,
) -> &'static str {
    match class {
        ConversationImportRejectionClass::EmptySource => "empty_source",
        ConversationImportRejectionClass::BlankLine => "blank_line",
        ConversationImportRejectionClass::InvalidUtf8 => "invalid_utf8",
        ConversationImportRejectionClass::InvalidJson => "invalid_json",
        ConversationImportRejectionClass::JsonDepthExceeded => "json_depth_exceeded",
        ConversationImportRejectionClass::TopLevelNotObject => "top_level_not_object",
        ConversationImportRejectionClass::InvalidRecordType => "invalid_record_type",
        ConversationImportRejectionClass::InvalidSourceMetadata => "invalid_source_metadata",
        ConversationImportRejectionClass::InvalidMessageEnvelope => "invalid_message_envelope",
        ConversationImportRejectionClass::InvalidMessageRole => "invalid_message_role",
        ConversationImportRejectionClass::MessageRoleMismatch => "message_role_mismatch",
        ConversationImportRejectionClass::InvalidMessageContent => "invalid_message_content",
        ConversationImportRejectionClass::InvalidContentBlock => "invalid_content_block",
        ConversationImportRejectionClass::InvalidToolResultBlock => "invalid_tool_result_block",
        ConversationImportRejectionClass::InvalidReasoning => "invalid_reasoning",
        ConversationImportRejectionClass::InvalidToolCall => "invalid_tool_call",
        ConversationImportRejectionClass::InvalidToolResult => "invalid_tool_result",
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_process_protocol::{
        CanonicalU64, CanonicalUuid, ConversationImportRejectionClass, ErrorCode, ErrorDetail,
        FailedModelCallCause, RejectionDetail,
    };
    use uuid::Uuid;

    use super::ClientError;

    #[test]
    fn provider_failure_cause_is_rendered_for_the_user() {
        assert_eq!(
            ClientError::TurnFailed(Some(FailedModelCallCause::QuotaExhausted)).to_string(),
            "the submitted turn failed: the provider quota is exhausted"
        );
    }

    #[test]
    fn conversation_import_conversion_evidence_names_only_class_and_ordinal() {
        let error = ClientError::remote(
            ErrorCode::InvalidRequest,
            "conversation import was rejected".to_owned(),
            ErrorDetail::invalid_request(RejectionDetail::ConversationImportConversionFailed {
                class: ConversationImportRejectionClass::InvalidToolResult,
                record_ordinal: Some(CanonicalU64::new(17)),
            }),
        );

        expect![[r#"
            invalid_request: conversation import was rejected (conversation_import_conversion_failed class=invalid_tool_result record_ordinal=17)"#]]
        .assert_eq(&error.to_string());
    }

    #[test]
    fn conversation_import_bound_evidence_names_limit_and_both_sizes() {
        let error = ClientError::remote(
            ErrorCode::InvalidRequest,
            "conversation import was rejected".to_owned(),
            ErrorDetail::invalid_request(RejectionDetail::ConversationImportSourceTooLarge {
                limit_bytes: CanonicalU64::new(8),
                declared_size_bytes: CanonicalU64::new(7),
                actual_size_bytes: Some(CanonicalU64::new(9)),
            }),
        );

        expect![[r#"
            invalid_request: conversation import was rejected (conversation_import_source_too_large limit_bytes=8 declared_size_bytes=7 actual_size_bytes=9)"#]]
        .assert_eq(&error.to_string());
    }

    #[test]
    fn delegation_delivery_sequence_exhaustion_names_the_recipient_and_counter() {
        let recipient_session_id = CanonicalUuid::from_uuid(Uuid::from_u128(17));
        let error = ClientError::remote(
            ErrorCode::Rejected,
            "delegation delivery sequence exhausted".to_owned(),
            ErrorDetail::rejected(RejectionDetail::DelegationDeliverySequenceExhausted {
                recipient_session_id,
                last: CanonicalU64::new(u64::MAX),
            }),
        );

        assert_eq!(
            error.to_string(),
            format!(
                "rejected: delegation delivery sequence exhausted \
                 (delegation_delivery_sequence_exhausted \
                 recipient_session={recipient_session_id} last={})",
                u64::MAX
            )
        );
    }

    #[test]
    fn delegation_message_identity_collision_names_the_message() {
        let message_id = CanonicalUuid::from_uuid(Uuid::from_u128(18));
        let error = ClientError::remote(
            ErrorCode::Rejected,
            "delegation message identity collision".to_owned(),
            ErrorDetail::rejected(RejectionDetail::DelegationMessageIdentityCollision {
                message_id,
            }),
        );

        assert_eq!(
            error.to_string(),
            format!(
                "rejected: delegation message identity collision \
                 (delegation_message_identity_collision message={message_id})"
            )
        );
    }

    #[test]
    fn commit_ambiguous_mutation_names_the_complete_replay_inputs() {
        let error = ClientError::remote(
            ErrorCode::CommitAmbiguous,
            "the commit response was lost".to_owned(),
            ErrorDetail::none(),
        )
        .mutation();

        expect![[r#"
            the mutation outcome may be ambiguous; retry the original command with the same arguments and exact input, using any printed recovery values"#]]
        .assert_eq(&error.to_string());
    }
}
