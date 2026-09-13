//! Lease execution capability, durable phase, and acknowledgement boundaries.

use super::*;
use signalbox_runner_wire::{
    Dispatch, LeaseClaimed, ResultBounds, ResultRecorded, WireToolName, WorkingDirectory,
};
use tempfile::TempDir;
use tokio::io::DuplexStream;

fn fixture(
    parent: &TempDir,
) -> (
    RunnerStateRoot,
    RunnerConnection<DuplexStream>,
    BufReader<DuplexStream>,
    LeaseOffer,
) {
    let mut state = RunnerStateRoot::open(&parent.path().join("state")).expect("private state");
    let first = PositiveU64::try_new(1).expect("first revision");
    // Arbitrary distinct identities for one complete execution correlation.
    let id = |value| CanonicalUuid::from_uuid(uuid::Uuid::from_u128(value));
    let correlation = LeaseCorrelation {
        registration_revision: first,
        placement_revision: first,
        lease_id: id(1),
        lease_generation: first,
        runner_id: id(2),
        working_directory: WorkingDirectory::try_new(
            parent.path().to_str().expect("fixture path").to_owned(),
        )
        .expect("exact path"),
        sandbox_profile: SandboxProfile::Ambient,
        tool_name: WireToolName::try_new("echo".to_owned()).expect("compiled tool"),
        session_id: id(3),
        turn_id: id(4),
        tool_request_id: id(5),
        tool_attempt_id: id(6),
        issuing_turn_attempt_id: id(7),
        tool_dispatch_generation: first,
    };
    let advertisement = Advertisement {
        tools: vec![correlation.tool_name.clone()],
        sandbox_profiles: vec![SandboxProfile::Ambient],
        default_working_directory: None,
        capability_classes: Vec::new(),
        workspace_capabilities: Vec::new(),
        credential_profiles: Vec::new(),
        repositories: Vec::new(),
    };
    let receipt = EnrollmentReceipt::new(
        state.state().request_id(),
        id(8),
        correlation.runner_id,
        id(9),
        first,
        advertisement_digest(&advertisement).expect("advertisement digest"),
        EnrollmentAuthority::Active,
    );
    state
        .record_receipt(receipt.clone())
        .expect("durable enrollment");
    let (runner, hub) = tokio::io::duplex(64 * 1024);
    let connection = RunnerConnection {
        io: BufReader::new(runner),
        receive_buffer: Vec::new(),
        pending_offer: None,
        execution: None,
        last_recorded: None,
        receipt,
        advertisement,
        outcome: EnrollmentOutcome::Enrolled,
        connection_epoch: first,
        heartbeat: None,
    };
    let offer = LeaseOffer {
        correlation,
        effect_class: EffectClass::Pure,
        credential_profile: None,
        grant_revision: None,
        normalized_arguments: serde_json::json!({"text": "echo"}),
        result_bounds: ResultBounds::version_one(),
    };
    (state, connection, BufReader::new(hub), offer)
}

#[tokio::test]
async fn offer_and_unacknowledged_claim_cannot_start_execution() {
    let parent = TempDir::new().expect("fixture parent");
    let (mut state, mut connection, mut hub, offer) = fixture(&parent);
    connection
        .serve_message(&mut state, Message::LeaseOffer(offer.clone()))
        .await
        .expect("offer accepted");
    assert_eq!(
        receive_message(&mut hub).await.expect("claim frame"),
        Message::LeaseClaim(LeaseClaim {
            correlation: offer.correlation.clone()
        })
    );
    assert_eq!(
        state.reconnect_inventory(),
        signalbox_runner_wire::ReconnectInventory::default()
    );
    assert!(connection.execution.is_none());
    assert!(
        connection
            .serve_message(
                &mut state,
                Message::Dispatch(Dispatch {
                    correlation: offer.correlation.clone(),
                    normalized_arguments: offer.normalized_arguments.clone()
                })
            )
            .await
            .is_err()
    );
    assert!(connection.execution.is_none());
    connection
        .serve_message(
            &mut state,
            Message::LeaseClaimed(LeaseClaimed {
                correlation: offer.correlation.clone(),
            }),
        )
        .await
        .expect("committed claim acknowledged");
    assert_eq!(
        state
            .reconnect_inventory()
            .lease
            .expect("durable claim")
            .phase,
        LeasePhaseKind::WaitingDispatch
    );
    assert!(connection.execution.is_none());
    assert!(
        connection
            .serve_message(
                &mut state,
                Message::Dispatch(Dispatch {
                    correlation: offer.correlation.clone(),
                    normalized_arguments: serde_json::json!({"text": "changed"})
                })
            )
            .await
            .is_err()
    );
    assert!(connection.execution.is_none());
    let mut second_offer = offer;
    second_offer.correlation.session_id = CanonicalUuid::from_uuid(uuid::Uuid::now_v7());
    second_offer.correlation.lease_id = CanonicalUuid::from_uuid(uuid::Uuid::now_v7());
    assert!(
        connection
            .serve_message(&mut state, Message::LeaseOffer(second_offer))
            .await
            .is_err(),
        "another session cannot take the runner serial slot"
    );
}

#[tokio::test]
async fn retained_result_is_resent_until_the_exact_durable_acknowledgement() {
    let parent = TempDir::new().expect("fixture parent");
    let (mut state, mut connection, mut hub, offer) = fixture(&parent);
    connection.pending_offer = Some(offer.clone());
    for phase in [
        LeasePhaseKind::WaitingDispatch,
        LeasePhaseKind::DispatchReceived,
        LeasePhaseKind::ExecutionMayHaveStarted,
    ] {
        state
            .record_lease_phase(LeasePhase {
                correlation: offer.correlation.clone(),
                phase,
            })
            .expect("ordered durable phase");
    }
    let result = RetainedResult {
        correlation: offer.correlation.clone(),
        result: TerminalResult::Success {
            text: "exact result".to_owned(),
        },
    };
    connection
        .serve_event(&mut state, RunnerEvent::Result(result.clone()))
        .await
        .expect("result published and sent");
    let expected = Message::Result(ResultFrame {
        correlation: result.correlation.clone(),
        result: result.result.clone(),
    });
    assert_eq!(
        receive_message(&mut hub).await.expect("first result"),
        expected
    );
    let first = PositiveU64::try_new(1).expect("first sequence");
    connection
        .serve_message(
            &mut state,
            Message::Heartbeat(Heartbeat {
                sequence: first,
                last_accepted_peer_sequence: 0,
            }),
        )
        .await
        .expect("heartbeat and resend");
    assert!(matches!(
        receive_message(&mut hub)
            .await
            .expect("heartbeat acknowledgement"),
        Message::HeartbeatAck(_)
    ));
    assert_eq!(
        receive_message(&mut hub).await.expect("repeated result"),
        expected
    );
    assert_eq!(state.reconnect_inventory().result, Some(result));
    let ack = Message::ResultRecorded(ResultRecorded {
        correlation: offer.correlation,
    });
    connection
        .serve_message(&mut state, ack.clone())
        .await
        .expect("durable acknowledgement");
    connection
        .serve_message(&mut state, ack)
        .await
        .expect("duplicate acknowledgement");
    drop(state);
    let state = RunnerStateRoot::open(&parent.path().join("state")).expect("reopen journal");
    assert_eq!(
        state.reconnect_inventory(),
        signalbox_runner_wire::ReconnectInventory::default()
    );
    assert!(connection.pending_offer.is_none());
}

#[tokio::test]
async fn local_shutdown_waits_for_the_durable_result_acknowledgement() {
    let parent = TempDir::new().expect("fixture parent");
    let (mut state, mut connection, mut hub, offer) = fixture(&parent);
    connection.pending_offer = Some(offer.clone());
    for phase in [
        LeasePhaseKind::WaitingDispatch,
        LeasePhaseKind::DispatchReceived,
        LeasePhaseKind::ExecutionMayHaveStarted,
    ] {
        state
            .record_lease_phase(LeasePhase {
                correlation: offer.correlation.clone(),
                phase,
            })
            .expect("ordered durable phase");
    }
    state
        .record_terminal_result(RetainedResult {
            correlation: offer.correlation.clone(),
            result: TerminalResult::Success {
                text: "exact result".to_owned(),
            },
        })
        .expect("durable result");

    let serving = connection.serve_until_shutdown(&mut state, async {});
    tokio::pin!(serving);
    tokio::select! {
        biased;
        outcome = &mut serving => panic!("shutdown completed before result acknowledgement: {outcome:?}"),
        () = tokio::task::yield_now() => {},
    }

    send_message(
        &mut hub,
        Message::ResultRecorded(ResultRecorded {
            correlation: offer.correlation,
        }),
    )
    .await
    .expect("result acknowledgement");
    assert_eq!(
        serving.await.expect("clean shutdown boundary"),
        ServeOutcome::ShutdownReady
    );
}

#[tokio::test]
async fn a_partial_daemon_frame_survives_a_child_completion_wakeup() {
    let (runner, mut hub) = tokio::io::duplex(4096);
    let mut reader = BufReader::new(runner);
    let mut buffer = Vec::new();
    let message = Message::Heartbeat(Heartbeat {
        sequence: PositiveU64::try_new(1).expect("first challenge"),
        last_accepted_peer_sequence: 0,
    });
    let frame = encode_line(&Frame::try_new(message.clone()).expect("valid heartbeat"))
        .expect("heartbeat frame");
    let split = frame.len() / 2;
    hub.write_all(&frame[..split]).await.expect("partial frame");
    tokio::select! {
        biased;
        _ = receive_message_buffered(&mut reader, &mut buffer) => panic!("an incomplete frame cannot finish"),
        () = tokio::task::yield_now() => {},
    }
    assert_eq!(buffer, frame[..split]);
    hub.write_all(&frame[split..])
        .await
        .expect("remaining frame");
    assert_eq!(
        receive_message_buffered(&mut reader, &mut buffer)
            .await
            .expect("whole frame retained"),
        message
    );
    assert!(buffer.is_empty());
}
