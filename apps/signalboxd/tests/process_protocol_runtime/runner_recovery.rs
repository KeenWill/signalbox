//! Durable recovery receipts retain their subject and original rejection on replay.

use super::*;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn runner_recovery_receipts_replay_and_reject_cross_kind_reuse() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let command_id = command()?;
    for request_id in [1, 2] {
        connection
            .request(
                request_id,
                ClientRequest::AbandonLostRunner {
                    command_id,
                    session_id,
                },
            )
            .await?;
        assert!(
            matches!(response_within(&mut connection).await?.message(), ServerMessage::RunnerAbandonmentReceipt {
            command_id: actual_command, session_id: actual_session,
            outcome: signalbox_process_protocol::RunnerAbandonmentOutcome::Rejected { reason: signalbox_process_protocol::RunnerRecoveryRejection::SessionNotFound },
        } if *actual_command == command_id && *actual_session == session_id)
        );
    }
    connection
        .request(
            3,
            ClientRequest::ReplaceLostRunner {
                command_id,
                session_id,
                revision: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));
    let replace_id = command()?;
    connection
        .request(
            4,
            ClientRequest::ReplaceLostRunner {
                command_id: replace_id,
                session_id,
                revision: None,
            },
        )
        .await?;
    assert!(
        matches!(response_within(&mut connection).await?.message(), ServerMessage::RunnerReplacementReceipt {
        command_id: actual_command, session_id: actual_session,
        outcome: signalbox_process_protocol::RunnerReplacementOutcome::Rejected { reason: signalbox_process_protocol::RunnerRecoveryRejection::SessionNotFound },
    } if *actual_command == replace_id && *actual_session == session_id)
    );
    let promote_id = command()?;
    let enrollment_request_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    connection
        .request(
            5,
            ClientRequest::PromotePendingRunner {
                command_id: promote_id,
                enrollment_request_id,
            },
        )
        .await?;
    assert!(
        matches!(response_within(&mut connection).await?.message(), ServerMessage::RunnerPromotionReceipt {
        command_id: actual_command, enrollment_request_id: actual_enrollment,
        outcome: signalbox_process_protocol::RunnerPromotionOutcome::Rejected { reason: signalbox_process_protocol::RunnerRecoveryRejection::PendingRunnerNotFound },
    } if *actual_command == promote_id && *actual_enrollment == enrollment_request_id)
    );
    drop(connection);
    runtime.stop().await
}
