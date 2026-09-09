//! Program cancellation through the versioned process socket.
use super::*;
use signalbox_domain::ProgramRunId;
use signalbox_persistence::program_journal::ProgramJournalRepository;
use signalbox_process_protocol::{ProgramRunCancellationOutcome, ProgramRunCancelledState};

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_cancellation_replays_its_committed_receipt() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    ProgramJournalRepository::new(runtime.pool.clone())
        .create_stream(ProgramRunId::from_uuid(run_id.into_uuid()))
        .await?;
    let command_id = command()?;
    let request = ClientRequest::CancelProgramRun { command_id, run_id };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(ProtocolVersion::One, 1, request.clone())
        .await?;
    let first = response_within(&mut connection).await?;
    assert_eq!(
        first.message(),
        &ServerMessage::ProgramRunCancellationReceipt {
            command_id,
            run_id,
            outcome: ProgramRunCancellationOutcome::Applied {
                terminal_state: ProgramRunCancelledState::Cancelled,
                result: ()
            },
        }
    );
    connection
        .request_version(ProtocolVersion::One, 2, request)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        first.message()
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn cancellation_of_success_returns_exact_result_on_every_retry() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run = ProgramRunId::from_uuid(run_id.into_uuid());
    let journal = ProgramJournalRepository::new(runtime.pool.clone());
    journal.create_stream(run).await?;
    let result = b"retained output".to_vec();
    journal
        .complete_if_tail(
            run,
            0,
            signalbox_domain::InlineFramePayload::new(result.clone()),
        )
        .await?
        .expect("success");
    let command_id = command()?;
    let request = ClientRequest::CancelProgramRun { command_id, run_id };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(ProtocolVersion::One, 1, request.clone())
        .await?;
    let first = response_within(&mut connection).await?;
    assert_eq!(
        first.message(),
        &ServerMessage::ProgramRunCancellationReceipt {
            command_id,
            run_id,
            outcome: ProgramRunCancellationOutcome::AlreadyTerminal(
                signalbox_process_protocol::ProgramRunTerminalState::Succeeded { result },
            ),
        }
    );
    connection
        .request_version(ProtocolVersion::One, 2, request)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        first.message()
    );
    drop(connection);
    runtime.stop().await
}
