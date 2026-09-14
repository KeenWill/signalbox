//! Lease execution capability, durable phase, and acknowledgement boundaries.

use super::*;
use signalbox_runner_wire::{
    Dispatch, LeaseClaimed, ResultBounds, ResultRecorded, WireToolName, WorkingDirectory,
};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::DuplexStream;

const CRASH_ROOT: &str = "SIGNALBOX_TEST_RUNNER_CRASH_ROOT";
const CRASH_EVIDENCE: &str = "SIGNALBOX_TEST_RUNNER_CRASH_EVIDENCE";
const CRASH_READY: &str = "runner journal phase durably published";

#[test]
#[ignore = "subprocess fixture for journal crash boundaries"]
fn journal_phase_crash_child() {
    let Some(root) = std::env::var_os(CRASH_ROOT) else {
        return;
    };
    let (target, result): (LeasePhase, Option<RetainedResult>) =
        serde_json::from_str(&std::env::var(CRASH_EVIDENCE).expect("crash fixture evidence"))
            .expect("typed fixture evidence");
    let mut state = RunnerStateRoot::open(Path::new(&root)).expect("child owns journal");
    for phase in [
        LeasePhaseKind::WaitingDispatch,
        LeasePhaseKind::DispatchReceived,
        LeasePhaseKind::ExecutionMayHaveStarted,
    ] {
        state
            .record_lease_phase(LeasePhase {
                correlation: target.correlation.clone(),
                phase,
            })
            .expect("phase fsynced by its production producer");
        if phase == target.phase {
            break;
        }
    }
    if let Some(result) = result {
        state
            .record_terminal_result(result)
            .expect("result durably retained");
    }
    println!("{CRASH_READY}");
    std::io::Write::flush(&mut std::io::stdout()).expect("parent observes crash boundary");
    loop {
        std::thread::park();
    }
}

async fn crash_at_journal_phase(
    parent: &TempDir,
    phase: LeasePhase,
    result: Option<RetainedResult>,
) -> RunnerStateRoot {
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "protocol::lease_tests::journal_phase_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env(CRASH_ROOT, parent.path().join("state"))
        .env(
            CRASH_EVIDENCE,
            serde_json::to_string(&(phase, result)).expect("fixture serialization"),
        )
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("journal producer subprocess");
    let mut output = BufReader::new(child.stdout.take().expect("child output")).lines();
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(line) = output.next_line().await.expect("child output readable") {
            if line.ends_with(CRASH_READY) {
                return;
            }
        }
        panic!("child exited before the durable boundary");
    })
    .await
    .expect("child reaches durable boundary");
    child.kill().await.expect("SIGKILL without journal cleanup");
    assert!(!child.wait().await.expect("child reaped").success());
    RunnerStateRoot::open(&parent.path().join("state")).expect("restart replays killed producer")
}

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
        resumed_lease: None,
        execution: None,
        last_recorded: None,
        configuration: None,
        workspace: None,
        last_workspace_recorded: None,
        last_provision_failure: None,
        last_release_recorded: None,
        startup_report: leaks::StartupReport::Disabled,
        leak_sent: false,
        last_leak_recorded: None,
        deferred_provision: None,
        deferred_dispatch: None,
        deferred_release: None,
        deferred_provision: None,
        offer_claimed: false,
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
async fn reconnect_replays_only_the_two_pre_execution_phases() {
    for phase in [
        LeasePhaseKind::WaitingDispatch,
        LeasePhaseKind::DispatchReceived,
    ] {
        let parent = TempDir::new().expect("fixture parent");
        let (state, connection, _, offer) = fixture(&parent);
        let advertisement = connection.advertisement.clone();
        drop(connection);
        drop(state);
        let mut state = crash_at_journal_phase(
            &parent,
            LeasePhase {
                correlation: offer.correlation.clone(),
                phase,
            },
            None,
        )
        .await;
        let (runner, hub) = tokio::io::duplex(64 * 1024);
        let mut hub = BufReader::new(hub);
        let exchange = async {
            let Message::Resume(request) = receive_message(&mut hub).await.expect("resume") else {
                panic!("resume required")
            };
            assert_eq!(
                request.inventory.lease.as_ref().expect("lease").phase,
                phase
            );
            send_message(
                &mut hub,
                Message::Resumed(Box::new(signalbox_runner_wire::Resumed {
                    registration_revision: request.prior_registration_revision,
                    connection_epoch: PositiveU64::try_new(2).expect("next epoch"),
                    directives: signalbox_runner_wire::ReconnectDirectives {
                        lease: Some(signalbox_runner_wire::Directive {
                            correlation: offer.correlation.clone(),
                            action: DirectiveAction::Await,
                        }),
                        ..Default::default()
                    },
                })),
            )
            .await
            .expect("canonical resume");
            assert_eq!(
                receive_message(&mut hub).await.expect("replayed claim"),
                Message::LeaseClaim(LeaseClaim {
                    correlation: offer.correlation.clone()
                })
            );
        };
        let (connection, ()) = tokio::join!(
            RunnerConnection::establish(runner, &mut state, &advertisement),
            exchange
        );
        let mut connection = connection.expect("authenticated resumed connection");
        assert!(connection.execution.is_none());
        connection
            .serve_message(
                &mut state,
                Message::LeaseClaimed(LeaseClaimed {
                    correlation: offer.correlation.clone(),
                }),
            )
            .await
            .expect("claim acknowledgement");
        assert_eq!(
            state
                .reconnect_inventory()
                .lease
                .expect("retained phase")
                .phase,
            phase
        );
        let dispatch = Message::Dispatch(Dispatch {
            correlation: offer.correlation.clone(),
            normalized_arguments: offer.normalized_arguments.clone(),
        });
        connection
            .serve_message(&mut state, dispatch.clone())
            .await
            .expect("first execution capability");
        assert_eq!(
            state
                .reconnect_inventory()
                .lease
                .expect("execution boundary")
                .phase,
            LeasePhaseKind::ExecutionMayHaveStarted
        );
        assert!(
            connection
                .serve_message(&mut state, dispatch)
                .await
                .is_err(),
            "duplicate dispatch cannot execute twice"
        );
    }
}

#[test]
fn registration_advance_preserves_only_the_retained_older_lease() {
    let parent = TempDir::new().expect("fixture parent");
    let (mut state, connection, _, offer) = fixture(&parent);
    state
        .record_lease_phase(LeasePhase {
            correlation: offer.correlation.clone(),
            phase: LeasePhaseKind::WaitingDispatch,
        })
        .expect("durable claim");
    state
        .record_registration(
            PositiveU64::try_new(2).expect("successor registration"),
            advertisement_digest(&connection.advertisement).expect("checked advertisement"),
        )
        .expect("durable receipt advance");
    drop(connection);
    drop(state);
    let mut state = RunnerStateRoot::open(&parent.path().join("state"))
        .expect("old lease survives receipt advance");
    for phase in [
        LeasePhaseKind::DispatchReceived,
        LeasePhaseKind::ExecutionMayHaveStarted,
    ] {
        state
            .record_lease_phase(LeasePhase {
                correlation: offer.correlation.clone(),
                phase,
            })
            .expect("exact retained authority advances");
    }
    state
        .record_terminal_result(RetainedResult {
            correlation: offer.correlation.clone(),
            result: TerminalResult::Success {
                text: "retained result".to_owned(),
            },
        })
        .expect("retained result");
    state
        .acknowledge_terminal_result(&offer.correlation)
        .expect("canonical acknowledgement");
    assert!(
        state
            .record_lease_phase(LeasePhase {
                correlation: offer.correlation,
                phase: LeasePhaseKind::WaitingDispatch
            })
            .is_err(),
        "an old receipt cannot authorize a fresh lease"
    );
}

#[tokio::test]
async fn reconnect_discards_only_exact_canonical_terminal_directives() {
    for recorded in [false, true] {
        let parent = TempDir::new().expect("fixture parent");
        let (state, connection, _, offer) = fixture(&parent);
        let advertisement = connection.advertisement.clone();
        drop(connection);
        drop(state);
        let mut state = crash_at_journal_phase(
            &parent,
            LeasePhase {
                correlation: offer.correlation.clone(),
                phase: LeasePhaseKind::ExecutionMayHaveStarted,
            },
            recorded.then(|| RetainedResult {
                correlation: offer.correlation.clone(),
                result: TerminalResult::Success {
                    text: "recorded echo".to_owned(),
                },
            }),
        )
        .await;
        let (runner, hub) = tokio::io::duplex(64 * 1024);
        let mut hub = BufReader::new(hub);
        let exchange = async {
            let Message::Resume(request) = receive_message(&mut hub).await.expect("resume") else {
                panic!("resume required")
            };
            assert_eq!(request.inventory.result.is_some(), recorded);
            let directive = signalbox_runner_wire::Directive {
                correlation: offer.correlation.clone(),
                action: if recorded {
                    DirectiveAction::DiscardAsRecorded
                } else {
                    DirectiveAction::FailStale
                },
            };
            send_message(
                &mut hub,
                Message::Resumed(Box::new(signalbox_runner_wire::Resumed {
                    registration_revision: request.prior_registration_revision,
                    connection_epoch: PositiveU64::try_new(2).expect("next epoch"),
                    directives: signalbox_runner_wire::ReconnectDirectives {
                        lease: Some(directive.clone()),
                        result: recorded.then_some(directive),
                        ..Default::default()
                    },
                })),
            )
            .await
            .expect("canonical disposition");
        };
        let (connection, ()) = tokio::join!(
            RunnerConnection::establish(runner, &mut state, &advertisement),
            exchange
        );
        assert!(connection.expect("resumed").execution.is_none());
        assert_eq!(state.reconnect_inventory(), Default::default());
        drop(state);
        assert_eq!(
            RunnerStateRoot::open(&parent.path().join("state"))
                .expect("acknowledgement survives restart")
                .reconnect_inventory(),
            Default::default()
        );
    }
}

#[tokio::test]
async fn local_shutdown_drains_execution_through_result_acknowledgement() {
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
            .expect("durable execution boundary");
    }
    let (finish, finished) = tokio::sync::oneshot::channel();
    connection.execution = Some(RunnerExecution {
        correlation: offer.correlation.clone(),
        task: tokio::spawn(async move {
            finished.await.expect("execution is allowed to finish");
            signalbox_domain::ToolAttemptEnd::Completed {
                result: signalbox_domain::ToolResultContent::Text(
                    signalbox_domain::ToolResultText::try_new("finished echo".to_owned())
                        .expect("fixture result"),
                ),
            }
        }),
    });
    let (signaled, signal_observed) = tokio::sync::oneshot::channel();
    let runner = async {
        assert_eq!(
            connection
                .serve_until_shutdown(&mut state, async {
                    signaled.send(()).expect("local shutdown observed");
                })
                .await
                .expect("execution drains"),
            ServeOutcome::ShutdownReady
        );
        assert_eq!(state.reconnect_inventory(), Default::default());
        connection.shutdown().await.expect("clean shutdown frame")
    };
    let daemon = async {
        signal_observed.await.expect("shutdown signal consumed");
        let first = PositiveU64::try_new(1).expect("first heartbeat");
        send_message(
            &mut hub,
            Message::Heartbeat(Heartbeat {
                sequence: first,
                last_accepted_peer_sequence: 0,
            }),
        )
        .await
        .expect("heartbeat while execution drains");
        assert!(matches!(
            receive_message(&mut hub).await.expect("still serving"),
            Message::HeartbeatAck(_)
        ));
        finish.send(()).expect("child has not been aborted");
        let result = receive_message(&mut hub).await.expect("retained result");
        assert_eq!(
            result,
            Message::Result(ResultFrame {
                correlation: offer.correlation.clone(),
                result: TerminalResult::Success {
                    text: "finished echo".to_owned(),
                },
            })
        );
        send_message(
            &mut hub,
            Message::Heartbeat(Heartbeat {
                sequence: PositiveU64::try_new(2).expect("next heartbeat"),
                last_accepted_peer_sequence: 1,
            }),
        )
        .await
        .expect("heartbeat before result acknowledgement");
        assert!(matches!(
            receive_message(&mut hub)
                .await
                .expect("still awaiting acknowledgement"),
            Message::HeartbeatAck(_)
        ));
        assert_eq!(
            receive_message(&mut hub).await.expect("result resend"),
            result
        );
        send_message(
            &mut hub,
            Message::ResultRecorded(ResultRecorded {
                correlation: offer.correlation,
            }),
        )
        .await
        .expect("durable result acknowledgement");
        assert!(matches!(
            receive_message(&mut hub)
                .await
                .expect("shutdown after acknowledgement"),
            Message::Shutdown(Shutdown {
                reason: ShutdownReason::RunnerShutdown,
                ..
            })
        ));
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(runner, daemon)
    })
    .await
    .expect("shutdown drains without stranding the lease");
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

#[tokio::test]
async fn startup_report_acknowledgement_precedes_a_queued_lease_claim() {
    let parent = TempDir::new().expect("fixture parent");
    let (mut state, mut connection, mut hub, offer) = fixture(&parent);
    connection.startup_report = leaks::StartupReport::Pending;
    connection
        .serve_message(&mut state, Message::LeaseOffer(offer.clone()))
        .await
        .expect("offer waits for startup");
    connection
        .serve_one(&mut state)
        .await
        .expect("startup scan completes");
    connection
        .advance_local(&mut state)
        .await
        .expect("report sent");
    let Message::WorkspaceLeakPage(report) =
        receive_message(&mut hub).await.expect("startup report")
    else {
        panic!("report precedes lease claim")
    };
    assert!(state.reconnect_inventory().lease.is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receive_message(&mut hub))
            .await
            .is_err(),
        "claim waits for report acknowledgement"
    );
    connection
        .serve_message(
            &mut state,
            Message::WorkspaceLeakRecorded(signalbox_runner_wire::WorkspaceLeakRecorded {
                correlation: report.page.correlation,
                page_digest: report.page.page_digest,
            }),
        )
        .await
        .expect("report recorded");
    connection
        .advance_local(&mut state)
        .await
        .expect("release queued claim");
    assert_eq!(
        receive_message(&mut hub).await.expect("claim after report"),
        Message::LeaseClaim(LeaseClaim {
            correlation: offer.correlation
        })
    );
}
