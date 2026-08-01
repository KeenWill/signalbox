use std::{error::Error, fmt, io};

use signalbox_process_protocol::{
    ConversationImportRejectionClass, ErrorCode, ErrorDetail, FailedModelCallCause,
    FrameDecodeError, FrameEncodeError, RejectionDetail,
};

#[derive(Debug)]
pub(crate) enum ClientError {
    Io(io::Error),
    SourceFile(io::Error),
    SystemPromptFile(io::Error),
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
            | Self::ReviewInputFile(error)
            | Self::ScanDirectory(error) => Some(error),
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

struct RejectionDisplay(RejectionDetail);

impl fmt::Display for RejectionDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            RejectionDetail::SessionNotFound { session_id } => {
                write!(formatter, "session_not_found session={session_id}")
            }
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
        CanonicalU64, ConversationImportRejectionClass, ErrorCode, ErrorDetail,
        FailedModelCallCause, RejectionDetail,
    };

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
