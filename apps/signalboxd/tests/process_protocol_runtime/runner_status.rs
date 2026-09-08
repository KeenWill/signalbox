use super::*;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn runner_status_empty_page_has_balanced_counts_over_the_socket() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::ReadRunnerStatus {
                page_size: 1,
                after: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::RunnerStatusStart {}
    );
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::RunnerStatusEnd {
            runner_count: CanonicalU64::new(0),
            failure_count: CanonicalU64::new(0),
            leak_count: CanonicalU64::new(0),
            next_after: None
        }
    );
    connection
        .request(
            2,
            ClientRequest::ReadRunnerStatus {
                page_size: 100,
                after: Some(
                    signalbox_process_protocol::RunnerStatusCursor::OperationFailure {
                        authorization_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
                    },
                ),
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::RunnerStatusStart {}
    );
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::RunnerStatusEnd {
            runner_count: CanonicalU64::new(0),
            failure_count: CanonicalU64::new(0),
            leak_count: CanonicalU64::new(0),
            next_after: None
        }
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn runner_status_exposes_pending_enrollment_identity_over_the_socket()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        RunnerAdvertisement, RunnerAuthenticationId, RunnerCatalog, RunnerEnrollmentId,
        RunnerEnrollmentRequestId, RunnerId,
    };
    use signalbox_persistence::runner_protocol::{
        IssuedRunnerEnrollmentIdentities, PristineRunnerEnrollmentRequest,
        RunnerConnectionTransition, RunnerProtocolStore,
    };
    use signalbox_process_protocol::{RunnerAuthorityState, RunnerStatusFact};
    let runtime = RunningRuntime::start().await?;
    let store = RunnerProtocolStore::new(
        runtime.pool.clone(),
        RunnerCatalog::try_new([], [], [], [], []).unwrap(),
    );
    let request = || {
        PristineRunnerEnrollmentRequest::new(
            RunnerEnrollmentRequestId::from_uuid(Uuid::now_v7()),
            IssuedRunnerEnrollmentIdentities::new(
                RunnerEnrollmentId::from_uuid(Uuid::now_v7()),
                RunnerId::from_uuid(Uuid::now_v7()),
                RunnerAuthenticationId::from_uuid(Uuid::now_v7()),
            ),
            [],
            RunnerAdvertisement::new([], [], [], [], [], []),
        )
    };
    let predecessor = store.enroll_pristine(request()).await?.into_receipt();
    let connected = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connected.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let candidate = store.enroll_pristine(request()).await?.into_receipt();
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::ReadRunnerStatus {
                page_size: 1,
                after: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::RunnerStatusStart {}
    );
    let first = response_within(&mut connection).await?;
    let second = response_within(&mut connection).await?;
    let pending = ServerMessage::RunnerStatus {
        status: RunnerStatusFact::Enrollment {
            runner_id: CanonicalUuid::from_uuid(candidate.identities().runner().into_uuid()),
            enrollment_request_id: CanonicalUuid::from_uuid(candidate.request().into_uuid()),
            authority: RunnerAuthorityState::Pending,
            connection_health: None,
        },
    };
    assert!([first.message(), second.message()].contains(&&pending));
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::RunnerStatusEnd {
            runner_count: CanonicalU64::new(2),
            failure_count: CanonicalU64::new(0),
            leak_count: CanonicalU64::new(0),
            next_after: None
        }
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn runner_status_out_of_range_page_size_rejects_before_page_start()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    for page_size in [0, 101] {
        let mut connection = Connection::connect(runtime.socket()).await?;
        let frame = serde_json::json!({"version":1, "request_id":"1", "request":{"type":"read_runner_status", "page_size":page_size, "after":null}});
        connection.raw_request(&format!("{frame}\n")).await?;
        assert!(matches!(
            response_within(&mut connection).await?.message(),
            ServerMessage::Error {
                code: ErrorCode::MalformedFrame,
                ..
            }
        ));
    }
    runtime.stop().await
}
