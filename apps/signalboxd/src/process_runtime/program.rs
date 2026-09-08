//! User-authorized cancellation of retained program journals.

use super::*;
use signalbox_domain::ProgramRunId;
use signalbox_persistence::program_cancellation::{
    self as store, ProgramCancellationOutcome as Outcome, ProgramCancellationResult as Result,
    ProgramTerminalState as State,
};
use signalbox_process_protocol::{
    ProgramRunCancellationOutcome as WireOutcome, ProgramRunCancelledState, ProgramRunTerminalState,
};

pub(super) async fn handle_cancel_program_run<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    command_id: signalbox_process_protocol::CommandId,
    run_id: CanonicalUuid,
    services: &ConnectionServices,
) -> std::result::Result<(), ProcessConnectionError> {
    let command = store::CancelProgramRun {
        command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
        run_id: ProgramRunId::from_uuid(run_id.into_uuid()),
    };
    let outcome = match store::cancel(&services.pool, command).await {
        Ok(Result::Recorded(Outcome::Applied)) => WireOutcome::Applied {
            terminal_state: ProgramRunCancelledState::Cancelled,
            result: (),
        },
        Ok(Result::Recorded(Outcome::NotFound)) => WireOutcome::NotFound {},
        Ok(Result::Recorded(Outcome::AlreadyTerminal(state))) => WireOutcome::AlreadyTerminal {
            terminal_state: match state {
                State::Cancelled => ProgramRunTerminalState::Cancelled,
                State::Faulted => ProgramRunTerminalState::Faulted,
            },
            result: (),
        },
        Ok(Result::ConflictingReuse) => {
            return write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(ErrorCode::ConflictingReuse),
            )
            .await;
        }
        Err(error) => {
            let protocol = match error {
                store::ProgramCancellationError::CommitAmbiguous(_) => ProtocolError::mutation_commit_ambiguous(),
                store::ProgramCancellationError::Database(_) => unavailable_protocol_error(InternalDiagnostic::ProgramCancellationDatabase),
                store::ProgramCancellationError::Journal(signalbox_persistence::program_journal::ProgramJournalRepositoryError::Database { .. }) => unavailable_protocol_error(InternalDiagnostic::ProgramCancellationDatabase),
                store::ProgramCancellationError::Journal(_) | store::ProgramCancellationError::Corruption => internal_protocol_error(None, InternalDiagnostic::ProgramCancellationCorruption),
            };
            return write_error(writer, version, request_id, protocol).await;
        }
    };
    write_message(
        writer,
        version,
        request_id,
        ServerMessage::ProgramRunCancellationReceipt {
            command_id,
            run_id,
            outcome,
        },
    )
    .await
}
