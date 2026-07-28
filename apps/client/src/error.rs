use std::{error::Error, fmt, io};

use signalbox_process_protocol::{
    ErrorCode, ErrorDetail, FrameDecodeError, FrameEncodeError, RejectionDetail,
};

#[derive(Debug)]
pub(crate) enum ClientError {
    Io(io::Error),
    SourceFile(io::Error),
    SystemPromptFile(io::Error),
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
    TurnFailed,
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
            | Self::TurnFailed
            | Self::TurnRefused
            | Self::TurnCancelled
            | Self::TurnReconciliationRequired => Self::AmbiguousMutation,
        }
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
            Self::TurnFailed => formatter.write_str("the submitted turn failed"),
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
            | Self::ScanDirectory(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Protocol(_)
            | Self::Remote { .. }
            | Self::AmbiguousMutation
            | Self::Input(_)
            | Self::SourceExceedsFrame
            | Self::ScanIncomplete { .. }
            | Self::TurnRecoveryRequired
            | Self::TurnFailed
            | Self::TurnRefused
            | Self::TurnCancelled
            | Self::TurnReconciliationRequired => None,
        }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_process_protocol::{ErrorCode, ErrorDetail};

    use super::ClientError;

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
