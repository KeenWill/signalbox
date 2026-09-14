//! Ready receipts survive transport loss and are cleared only by an exact acknowledgement.
use super::*;
use signalbox_runner_wire::{
    Directive, OperationCorrelation, ProvisionCorrelation, ReconnectDirectives, Resumed,
    WorkspaceProvision, WorkspaceRecorded,
};
use tempfile::TempDir;
use uuid::Uuid;

fn identity() -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::now_v7())
}
fn positive() -> PositiveU64 {
    PositiveU64::try_new(1).expect("initial revision")
}
fn configuration() -> crate::RunnerConfiguration {
    crate::RunnerConfiguration::parse(include_str!(
        "../../../../config/signalbox-runner.example.toml"
    ))
    .expect("checked example config; no credential bytes read")
}
fn connection(
    stream: tokio::io::DuplexStream,
    receipt: EnrollmentReceipt,
) -> RunnerConnection<tokio::io::DuplexStream> {
    let config = configuration();
    RunnerConnection {
        io: BufReader::new(stream),
        receive_buffer: Vec::new(),
        pending_offer: None,
        resumed_lease: None,
        execution: None,
        last_recorded: None,
        workspace: None,
        last_release_recorded: None,
        last_workspace_recorded: None,
        last_provision_failure: None,
        receipt,
        advertisement: config.advertisement().clone(),
        configuration: Some(config),
        outcome: EnrollmentOutcome::Enrolled,
        connection_epoch: positive(),
        heartbeat: None,
    }
}
fn enrolled(directory: &TempDir) -> RunnerStateRoot {
    enrolled_with_configuration(directory, &configuration())
}
fn enrolled_with_configuration(
    directory: &TempDir,
    config: &crate::RunnerConfiguration,
) -> RunnerStateRoot {
    let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("private root");
    state
        .record_receipt(EnrollmentReceipt::new(
            state.state().request_id(),
            identity(),
            identity(),
            identity(),
            positive(),
            advertisement_digest(config.advertisement()).expect("digest"),
            EnrollmentAuthority::Active,
        ))
        .expect("enrollment receipt");
    state
}
fn provision(receipt: &EnrollmentReceipt) -> WorkspaceProvision {
    WorkspaceProvision {
        correlation: ProvisionCorrelation {
            authorization_id: identity(),
            session_id: identity(),
            placement_revision: positive(),
            runner_id: receipt.runner_id(),
            registration_revision: receipt.registration_revision(),
            repository: None,
            sandbox_profile: SandboxProfile::Ambient,
            credential_profile: None,
        },
        recovery: None,
    }
}

#[tokio::test]
async fn private_workspace_reconnect_replays_receipt_and_activates_preserved_files() {
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    let operation = provision(&receipt);
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt.clone());
    let mut hub = BufReader::new(hub);
    let receive_ready = async {
        send_message(&mut hub, Message::WorkspaceProvision(operation.clone()))
            .await
            .expect("send provision");
        let Message::WorkspaceReady(ready) = receive_message(&mut hub).await.expect("ready frame")
        else {
            panic!("ready receipt")
        };
        ready
    };
    let produce = async {
        runner
            .serve_one(&mut state)
            .await
            .expect("accept operation");
        runner.serve_one(&mut state).await.expect("publish ready");
    };
    let (ready, ()) = tokio::join!(receive_ready, produce);
    std::fs::write(
        Path::new(&ready.working_directory).join("session-data"),
        b"retained",
    )
    .expect("session data");
    drop(runner);
    drop(hub);
    drop(state);
    let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("restart");
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut hub = BufReader::new(hub);
    let resumed_runner = async {
        let config = configuration();
        let mut runner = RunnerConnection::establish(stream, &mut state, config.advertisement())
            .await
            .expect("resume retained provision")
            .with_configuration(config);
        runner
            .serve_one(&mut state)
            .await
            .expect("authenticate and replay ready");
        runner
            .serve_one(&mut state)
            .await
            .expect("activate and clear journal");
    };
    let resumed_hub = async {
        let Message::Resume(resume) = receive_message(&mut hub).await.expect("resume frame") else {
            panic!("resume")
        };
        assert!(resume.inventory.workspace_operation.is_some());
        send_message(
            &mut hub,
            Message::Resumed(Box::new(Resumed {
                registration_revision: receipt.registration_revision(),
                connection_epoch: positive(),
                directives: ReconnectDirectives {
                    workspace_operation: Some(Directive {
                        correlation: OperationCorrelation::Provision(operation.correlation.clone()),
                        action: DirectiveAction::Await,
                    }),
                    ..Default::default()
                },
            })),
        )
        .await
        .expect("await exact receipt");
        assert_eq!(
            receive_message(&mut hub).await.expect("replayed frame"),
            Message::WorkspaceReady(ready.clone())
        );
        send_message(
            &mut hub,
            Message::WorkspaceRecorded(WorkspaceRecorded {
                correlation: ready.correlation.clone(),
                manifest_id: ready.ready.manifest.manifest_id,
                manifest_digest: ready.ready.manifest_digest.clone(),
            }),
        )
        .await
        .expect("ack receipt");
    };
    tokio::join!(resumed_runner, resumed_hub);
    assert!(state.reconnect_inventory().workspace_operation.is_none());
    assert_eq!(
        std::fs::read(Path::new(&ready.working_directory).join("session-data"))
            .expect("preserved file"),
        b"retained"
    );
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            Path::new(&ready.working_directory)
                .parent()
                .expect("placement")
                .join("workspace-manifest.json"),
        )
        .expect("manifest"),
    )
    .expect("JSON manifest");
    assert_eq!(document["manifest"]["lifecycle"], "active");
}

#[tokio::test]
async fn unknown_credential_profile_is_rejected_before_journaling_provision() {
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    let mut operation = provision(&receipt);
    operation.correlation.repository = Some(
        signalbox_runner_wire::RepositoryKey::try_new("signalbox".to_owned())
            .expect("configured repository"),
    );
    operation.correlation.credential_profile = Some(
        signalbox_runner_wire::ProfileName::try_new("unknown".to_owned())
            .expect("unknown fixture profile"),
    );
    operation.recovery = Some(signalbox_runner_wire::Recovery::UnbornBranch {
        name: "main".to_owned(),
    });
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt);
    let mut hub = BufReader::new(hub);
    send_message(&mut hub, Message::WorkspaceProvision(operation))
        .await
        .expect("well-shaped frame");
    assert!(matches!(
        runner.serve_one(&mut state).await,
        Err(RunnerConnectionError::Workspace(
            crate::WorkspaceProvisionError::CredentialUnavailable
        ))
    ));
    assert!(state.retained_provision().is_none());
}

#[tokio::test]
async fn anonymous_clone_failure_is_retained_while_the_runner_keeps_serving() {
    failed_anonymous_clone(false).await;
}

#[tokio::test]
async fn missing_repository_revision_is_a_retained_provisioning_refusal() {
    failed_anonymous_clone(true).await;
}

async fn failed_anonymous_clone(missing_revision: bool) {
    use signalbox_runner_wire::{FailureCategory, Heartbeat, OperationFailureRecorded};
    let directory = tempfile::tempdir().expect("temporary parent");
    let config = crate::RunnerConfiguration::parse(
        &include_str!("../../../../config/signalbox-runner.example.toml")
            .replace("credential_profile = \"github-runner\"", ""),
    )
    .expect("anonymous repository");
    let mut state = enrolled_with_configuration(&directory, &config);
    let receipt = state.state().receipt().expect("receipt").clone();
    let mut request = provision(&receipt);
    request.correlation.repository = Some(
        signalbox_runner_wire::RepositoryKey::try_new("signalbox".to_owned())
            .expect("advertised repository"),
    );
    request.recovery = Some(signalbox_runner_wire::Recovery::Commit {
        revision: "a".repeat(40),
    });
    let source = directory.path().join("source");
    if missing_revision {
        assert!(
            tokio::process::Command::new("git")
                .args(["init", "--bare"])
                .arg(&source)
                .output()
                .await
                .expect("local Git")
                .status
                .success()
        );
    }
    let checked = crate::workspace::provision::CheckedProvision::check(&config, request.clone())
        .expect("advertised anonymous acquisition")
        .with_local_clone_fixture(source.to_str().expect("fixture path").to_owned());
    state
        .record_provision(request.clone())
        .expect("durable authorization before clone");
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt.clone()).with_configuration(config.clone());
    runner.advertisement = config.advertisement().clone();
    runner.workspace = Some(workspaces::WorkspaceExecution::Preparing(tokio::spawn(
        checked.prepare(state.workspace_store().expect("store")),
    )));
    let mut hub = BufReader::new(hub);
    runner
        .serve_one(&mut state)
        .await
        .expect("expected clone failure keeps serving");
    let failed = receive_message(&mut hub).await.expect("typed refusal");
    let Message::OperationFailed(failure) = &failed else {
        panic!("operation failure")
    };
    assert_eq!(
        failure.failure.category,
        FailureCategory::RepositoryUnavailable
    );
    assert_eq!(
        state.reconnect_inventory().operation_failure.as_ref(),
        Some(&failure.failure)
    );
    send_message(
        &mut hub,
        Message::Heartbeat(Heartbeat {
            sequence: positive(),
            last_accepted_peer_sequence: 0,
        }),
    )
    .await
    .expect("heartbeat after failure");
    runner
        .serve_one(&mut state)
        .await
        .expect("runner still serves");
    assert!(matches!(
        receive_message(&mut hub)
            .await
            .expect("heartbeat acknowledgement"),
        Message::HeartbeatAck(_)
    ));
    assert_eq!(
        receive_message(&mut hub).await.expect("failure replay"),
        failed
    );
    drop(runner);
    drop(state);
    let mut state =
        RunnerStateRoot::open(&directory.path().join("state")).expect("restart retains failure");
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut hub = BufReader::new(hub);
    let resumed = async {
        RunnerConnection::establish(stream, &mut state, config.advertisement())
            .await
            .expect("recorded failure resumes")
            .with_configuration(config)
    };
    let daemon = async {
        let Message::Resume(resume) = receive_message(&mut hub).await.expect("resume inventory")
        else {
            panic!("resume")
        };
        assert_eq!(
            resume.inventory.operation_failure.as_ref(),
            Some(&failure.failure)
        );
        let directive = Directive {
            correlation: failure.failure.correlation.clone(),
            action: DirectiveAction::DiscardAsRecorded,
        };
        send_message(
            &mut hub,
            Message::Resumed(Box::new(Resumed {
                registration_revision: receipt.registration_revision(),
                connection_epoch: positive(),
                directives: ReconnectDirectives {
                    workspace_operation: Some(directive.clone()),
                    operation_failure: Some(directive),
                    ..Default::default()
                },
            })),
        )
        .await
        .expect("durably recorded failure");
    };
    let (mut runner, ()) = tokio::join!(resumed, daemon);
    assert!(state.reconnect_inventory().workspace_operation.is_none());
    let mut next = request;
    next.correlation.authorization_id = identity();
    next.correlation.session_id = identity();
    next.correlation.repository = None;
    next.recovery = None;
    runner
        .serve_message(&mut state, Message::WorkspaceProvision(next))
        .await
        .expect("next operation is accepted after refusal");
    runner
        .serve_one(&mut state)
        .await
        .expect("next private root ready");
    assert!(matches!(
        receive_message(&mut hub).await.expect("next receipt"),
        Message::WorkspaceReady(_)
    ));
    // A provision-success receipt cannot be retired with an unrelated failure acknowledgement.
    assert!(
        runner
            .serve_message(
                &mut state,
                Message::OperationFailureRecorded(OperationFailureRecorded {
                    correlation: failure.failure.correlation.clone()
                })
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn release_journal_survives_disconnect_and_clears_only_on_exact_acknowledgement() {
    use signalbox_runner_wire::{ReleaseCorrelation, ReleasePhase, WorkspaceReleaseRecorded};
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    let operation = provision(&receipt);
    let checked = crate::workspace::provision::CheckedProvision::check(&configuration(), operation)
        .expect("private provision");
    let ready = checked
        .prepare(state.workspace_store().expect("store"))
        .await
        .expect("ready");
    let release = ReleaseCorrelation {
        session_id: ready.correlation.session_id,
        placement_revision: ready.correlation.placement_revision,
        runner_id: receipt.runner_id(),
        manifest_id: ready.ready.manifest.manifest_id,
    };
    state
        .record_release(release.clone())
        .expect("accepted before cleanup");
    drop(state);
    let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("restart");
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt.clone());
    let mut hub = BufReader::new(hub);
    runner.serve_one(&mut state).await.expect("resume cleanup");
    let released = receive_message(&mut hub).await.expect("completed receipt");
    assert_eq!(
        released,
        Message::WorkspaceReleased(signalbox_runner_wire::WorkspaceReleased {
            correlation: release.clone()
        })
    );
    assert_eq!(
        state.retained_release().expect("retained").1,
        ReleasePhase::ReleaseCompleted
    );
    assert!(!Path::new(&ready.working_directory).exists());
    drop(runner);
    drop(state);
    let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("second restart");
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt);
    let mut hub = BufReader::new(hub);
    runner.ensure_workspace(&state).expect("terminal journal");
    runner
        .send_retained_workspace(&state)
        .await
        .expect("replay");
    assert_eq!(
        receive_message(&mut hub).await.expect("same receipt"),
        released
    );
    let mut wrong = release.clone();
    wrong.manifest_id = identity();
    assert!(
        runner
            .serve_message(
                &mut state,
                Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded { correlation: wrong })
            )
            .await
            .is_err()
    );
    assert!(state.retained_release().is_some());
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded {
                correlation: release,
            }),
        )
        .await
        .expect("exact acknowledgement");
    assert!(state.retained_release().is_none());
}

#[tokio::test]
async fn rejected_replacement_after_workspace_ready_is_deleted_and_release_is_settled() {
    use signalbox_runner_wire::{
        ReleaseCorrelation, WorkspaceRelease, WorkspaceReleaseRecorded, WorkspaceReleased,
    };
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    let operation = provision(&receipt);
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt.clone());
    let mut hub = BufReader::new(hub);
    runner
        .serve_message(&mut state, Message::WorkspaceProvision(operation))
        .await
        .expect("accepted provisioning");
    runner
        .serve_one(&mut state)
        .await
        .expect("finished provisioning");
    let Message::WorkspaceReady(ready) = receive_message(&mut hub).await.expect("ready frame")
    else {
        panic!("ready receipt")
    };
    let work = Path::new(&ready.working_directory);
    std::fs::write(work.join("staged-data"), b"unconsumed workspace").expect("staged fixture");
    // The daemon records the ready receipt even when abandonment wins the
    // replacement race, then orders release of that exact staged manifest.
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceRecorded(WorkspaceRecorded {
                correlation: ready.correlation.clone(),
                manifest_id: ready.ready.manifest.manifest_id,
                manifest_digest: ready.ready.manifest_digest,
            }),
        )
        .await
        .expect("ready acknowledged");
    let correlation = ReleaseCorrelation {
        session_id: ready.correlation.session_id,
        placement_revision: ready.correlation.placement_revision,
        runner_id: receipt.runner_id(),
        manifest_id: ready.ready.manifest.manifest_id,
    };
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceRelease(WorkspaceRelease {
                correlation: correlation.clone(),
            }),
        )
        .await
        .expect("release accepted");
    assert!(state.retained_release().is_some());
    runner
        .serve_one(&mut state)
        .await
        .expect("cleanup completed");
    assert_eq!(
        receive_message(&mut hub).await.expect("release receipt"),
        Message::WorkspaceReleased(WorkspaceReleased {
            correlation: correlation.clone()
        })
    );
    assert!(!work.exists());
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded {
                correlation: correlation.clone(),
            }),
        )
        .await
        .expect("release settled");
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded { correlation }),
        )
        .await
        .expect("duplicate acknowledgement");
    assert!(state.reconnect_inventory().workspace_operation.is_none());
    runner
        .serve_message(&mut state, Message::WorkspaceProvision(provision(&receipt)))
        .await
        .expect("runner still serves provisioning");
}

#[tokio::test]
async fn reconnect_resumes_accepted_cleanup_and_discards_durably_recorded_completion() {
    use signalbox_runner_wire::{ReleaseCorrelation, ReleasePhase, WorkspaceOperation};
    for completed in [false, true] {
        let directory = tempfile::tempdir().expect("temporary parent");
        let mut state = enrolled(&directory);
        let receipt = state.state().receipt().expect("receipt").clone();
        let ready = crate::workspace::provision::CheckedProvision::check(
            &configuration(),
            provision(&receipt),
        )
        .expect("private provision")
        .prepare(state.workspace_store().expect("store"))
        .await
        .expect("ready");
        let correlation = ReleaseCorrelation {
            session_id: ready.correlation.session_id,
            placement_revision: ready.correlation.placement_revision,
            runner_id: receipt.runner_id(),
            manifest_id: ready.ready.manifest.manifest_id,
        };
        state
            .record_release(correlation.clone())
            .expect("accepted cleanup");
        if completed {
            state
                .workspace_store()
                .expect("store")
                .release(&state.accepted_release().expect("authority"))
                .expect("cleanup");
            state
                .complete_release(&correlation)
                .expect("completed journal");
        }
        drop(state);
        let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("restart");
        let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
        let mut hub = BufReader::new(hub);
        let config = configuration();
        let resume = RunnerConnection::establish(stream, &mut state, config.advertisement());
        let daemon = async {
            let Message::Resume(request) = receive_message(&mut hub).await.expect("resume") else {
                panic!("resume request")
            };
            assert_eq!(
                request.inventory.workspace_operation,
                Some(WorkspaceOperation::Release {
                    correlation: correlation.clone(),
                    phase: if completed {
                        ReleasePhase::ReleaseCompleted
                    } else {
                        ReleasePhase::ReleaseAccepted
                    },
                })
            );
            send_message(
                &mut hub,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: receipt.registration_revision(),
                    connection_epoch: positive(),
                    directives: ReconnectDirectives {
                        workspace_operation: Some(Directive {
                            correlation: OperationCorrelation::Release(correlation.clone()),
                            action: if completed {
                                DirectiveAction::DiscardAsRecorded
                            } else {
                                DirectiveAction::Await
                            },
                        }),
                        ..Default::default()
                    },
                })),
            )
            .await
            .expect("durable release decision");
        };
        let (runner, ()) = tokio::join!(resume, daemon);
        let mut runner = runner.expect("release resumed").with_configuration(config);
        if completed {
            assert!(state.retained_release().is_none());
        } else {
            runner
                .serve_one(&mut state)
                .await
                .expect("finish accepted cleanup");
            assert_eq!(
                receive_message(&mut hub).await.expect("released"),
                Message::WorkspaceReleased(signalbox_runner_wire::WorkspaceReleased {
                    correlation
                })
            );
        }
        assert!(!Path::new(&ready.working_directory).exists());
    }
}

#[tokio::test]
async fn duplicate_release_acknowledgement_preserves_a_later_release() {
    use signalbox_runner_wire::{ReleaseCorrelation, WorkspaceRelease, WorkspaceReleaseRecorded};
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt.clone());
    let mut hub = BufReader::new(hub);
    let first = ReleaseCorrelation {
        session_id: identity(),
        placement_revision: positive(),
        runner_id: receipt.runner_id(),
        manifest_id: identity(),
    };
    let mut next = first.clone();
    next.manifest_id = identity();
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceRelease(WorkspaceRelease {
                correlation: first.clone(),
            }),
        )
        .await
        .expect("first release");
    runner
        .serve_one(&mut state)
        .await
        .expect("absent workspace released");
    assert!(matches!(
        receive_message(&mut hub).await.expect("completion"),
        Message::WorkspaceReleased(_)
    ));
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded {
                correlation: first.clone(),
            }),
        )
        .await
        .expect("first acknowledgement");
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceRelease(WorkspaceRelease {
                correlation: next.clone(),
            }),
        )
        .await
        .expect("next release");
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded { correlation: first }),
        )
        .await
        .expect("duplicate prior acknowledgement");
    assert_eq!(
        state.retained_release().expect("next remains pending").0,
        &next
    );
    runner
        .serve_one(&mut state)
        .await
        .expect("next cleanup completes");
    assert!(matches!(
        receive_message(&mut hub).await.expect("next completion"),
        Message::WorkspaceReleased(_)
    ));
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded { correlation: next }),
        )
        .await
        .expect("next acknowledgement");
    assert!(state.retained_release().is_none());
}

#[tokio::test]
async fn duplicate_provision_failure_acknowledgement_preserves_a_later_failure() {
    use signalbox_runner_wire::OperationFailureRecorded;
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    let (stream, _hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt.clone());
    state
        .record_provision(provision(&receipt))
        .expect("first admitted operation");
    runner
        .finish_provision(
            &mut state,
            Err(crate::WorkspaceProvisionError::RepositoryUnavailable),
        )
        .expect("first acquisition refusal");
    let first = state
        .retained_provision_failure()
        .expect("first failure")
        .correlation
        .clone();
    runner
        .serve_message(
            &mut state,
            Message::OperationFailureRecorded(OperationFailureRecorded {
                correlation: first.clone(),
            }),
        )
        .await
        .expect("first acknowledgement");
    state
        .record_provision(provision(&receipt))
        .expect("next admitted operation");
    runner
        .finish_provision(
            &mut state,
            Err(crate::WorkspaceProvisionError::RepositoryUnavailable),
        )
        .expect("next acquisition refusal");
    let next = state
        .retained_provision_failure()
        .expect("next failure")
        .clone();
    runner
        .serve_message(
            &mut state,
            Message::OperationFailureRecorded(OperationFailureRecorded { correlation: first }),
        )
        .await
        .expect("duplicate prior acknowledgement");
    assert_eq!(state.retained_provision_failure(), Some(&next));
    runner
        .serve_message(
            &mut state,
            Message::OperationFailureRecorded(OperationFailureRecorded {
                correlation: next.correlation,
            }),
        )
        .await
        .expect("next acknowledgement");
    assert!(state.retained_provision_failure().is_none());
}
