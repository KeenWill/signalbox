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
        last_workspace_recorded: None,
        last_provision_failure: None,
        last_release_recorded: None,
        startup_report: leaks::StartupReport::Disabled,
        leak_sent: false,
        last_leak_recorded: None,
        deferred_dispatch: None,
        deferred_release: None,
        offer_claimed: false,
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
async fn startup_report_without_a_session_is_retained_until_its_exact_page_is_recorded() {
    use signalbox_runner_wire::WorkspaceLeakRecorded;
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(directory.path().join("state/sessions"))
        .expect("private sessions directory");
    std::fs::write(directory.path().join("state/sessions/orphan"), b"unknown")
        .expect("unowned entry");
    for index in 0..64 {
        std::fs::write(
            directory
                .path()
                .join(format!("state/sessions/orphan-{index}")),
            b"unknown",
        )
        .expect("page-spanning unknown entries");
    }
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt).with_configuration(configuration());
    let mut hub = BufReader::new(hub);
    runner.serve_one(&mut state).await.expect("scan completes");
    runner
        .advance_local(&mut state)
        .await
        .expect("publish page");
    let Message::WorkspaceLeakPage(report) = receive_message(&mut hub).await.expect("page") else {
        panic!("leak page")
    };
    assert_eq!(report.page.facts.len(), 64);
    assert!(report.page.facts[0].session.is_none());
    assert!(!runner.startup_report.complete());
    let mut wrong = WorkspaceLeakRecorded {
        correlation: report.page.correlation.clone(),
        page_digest: signalbox_runner_wire::Digest::try_new("f".repeat(64))
            .expect("different digest"),
    };
    assert!(
        runner
            .serve_message(&mut state, Message::WorkspaceLeakRecorded(wrong.clone()))
            .await
            .is_err()
    );
    assert!(state.retained_leak_page().is_some());
    wrong.page_digest = report.page.page_digest;
    runner
        .serve_message(&mut state, Message::WorkspaceLeakRecorded(wrong.clone()))
        .await
        .expect("exact acknowledgement");
    runner
        .advance_local(&mut state)
        .await
        .expect("startup settled");
    assert!(!runner.startup_report.complete());
    let Message::WorkspaceLeakPage(next) = receive_message(&mut hub).await.expect("next page")
    else {
        panic!("second page")
    };
    assert_eq!(next.page.facts.len(), 1);
    runner
        .serve_message(&mut state, Message::WorkspaceLeakRecorded(wrong))
        .await
        .expect("duplicate prior page acknowledgement");
    assert_eq!(state.retained_leak_page(), Some(&next.page));
    runner
        .serve_message(
            &mut state,
            Message::WorkspaceLeakRecorded(WorkspaceLeakRecorded {
                correlation: next.page.correlation,
                page_digest: next.page.page_digest,
            }),
        )
        .await
        .expect("last page acknowledged");
    runner
        .advance_local(&mut state)
        .await
        .expect("complete startup report");
    assert!(runner.startup_report.complete());
    assert!(state.retained_leak_page().is_none());
}

#[tokio::test]
async fn cleanup_failure_is_journaled_and_acknowledgement_preserves_the_survivor() {
    use signalbox_runner_wire::{
        FailureCategory, OperationCorrelation, OperationFailureRecorded, ReleaseCorrelation,
    };
    use std::os::unix::fs::DirBuilderExt;
    let directory = tempfile::tempdir().expect("temporary parent");
    let mut state = enrolled(&directory);
    let receipt = state.state().receipt().expect("receipt").clone();
    let checked =
        crate::workspace::provision::CheckedProvision::check(&configuration(), provision(&receipt))
            .expect("private provision");
    let ready = checked
        .prepare(state.workspace_store().expect("store"))
        .await
        .expect("ready");
    let correlation = ReleaseCorrelation {
        session_id: ready.correlation.session_id,
        placement_revision: ready.correlation.placement_revision,
        runner_id: receipt.runner_id(),
        manifest_id: ready.ready.manifest.manifest_id,
    };
    let trash = directory.path().join("state/trash");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&trash)
        .expect("private trash");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(trash.join(correlation.manifest_id.to_string()))
        .expect("conflicting release destination");
    state
        .record_release(correlation.clone())
        .expect("accepted release");
    let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt.clone());
    let mut hub = BufReader::new(hub);
    runner
        .serve_one(&mut state)
        .await
        .expect("failed cleanup is evidence");
    let Message::OperationFailed(failed) = receive_message(&mut hub).await.expect("failure frame")
    else {
        panic!("cleanup failure")
    };
    assert_eq!(
        failed.failure.category,
        FailureCategory::WorkspaceCleanupFailed
    );
    assert_eq!(
        state.reconnect_inventory().operation_failure,
        Some(failed.failure.clone())
    );
    drop(runner);
    drop(state);
    let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("restart");
    assert_eq!(
        state.reconnect_inventory().operation_failure,
        Some(failed.failure)
    );
    let (stream, _hub) = tokio::io::duplex(MAX_FRAME_BYTES);
    let mut runner = connection(stream, receipt);
    runner
        .serve_message(
            &mut state,
            Message::OperationFailureRecorded(OperationFailureRecorded {
                correlation: OperationCorrelation::Release(correlation),
            }),
        )
        .await
        .expect("durable failure acknowledgement");
    assert!(state.reconnect_inventory().workspace_operation.is_none());
    assert!(Path::new(&ready.working_directory).is_dir());
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

#[tokio::test]
async fn stale_release_reconciliation_survives_restart_and_preserves_the_leak_page() {
    use signalbox_runner_wire::{
        DetailName, FailureCategory, FailureDetail, OperationFailure, ReleaseCorrelation,
        ReleasePhase,
    };
    for (phase, failed) in [
        (ReleasePhase::ReleaseAccepted, false),
        (ReleasePhase::ReleaseCompleted, false),
        (ReleasePhase::ReleaseAccepted, true),
    ] {
        let directory = tempfile::tempdir().expect("temporary parent");
        let mut state = enrolled(&directory);
        let receipt = state.state().receipt().expect("receipt").clone();
        let correlation = ReleaseCorrelation {
            session_id: identity(),
            placement_revision: positive(),
            runner_id: receipt.runner_id(),
            manifest_id: identity(),
        };
        state
            .record_release(correlation.clone())
            .expect("accepted release");
        if phase == ReleasePhase::ReleaseCompleted {
            state
                .complete_release(&correlation)
                .expect("completed release");
        }
        if failed {
            state
                .fail_release(OperationFailure {
                    correlation: OperationCorrelation::Release(correlation.clone()),
                    category: FailureCategory::WorkspaceCleanupFailed,
                    detail: FailureDetail::try_new(
                        DetailName::try_new("cleanup-refused".to_owned()).expect("detail name"),
                        "cleanup refused".to_owned(),
                        serde_json::json!({}),
                    )
                    .expect("bounded detail"),
                })
                .expect("failed release");
        }
        let page = crate::workspace::leaks::pages(receipt.registration_revision(), &[])
            .expect("empty startup report")
            .pop_front()
            .expect("final page");
        state
            .record_leak_page(page.clone())
            .expect("independent report");
        drop(state);
        let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("restart");
        let operation = OperationCorrelation::Release(correlation.clone());
        for wrong_correlation in [true, false] {
            let (stream, hub) = tokio::io::duplex(MAX_FRAME_BYTES);
            let mut hub = BufReader::new(hub);
            let config = configuration();
            let resume = RunnerConnection::establish(stream, &mut state, config.advertisement());
            let daemon = async {
                let Message::Resume(request) = receive_message(&mut hub).await.expect("resume")
                else {
                    panic!("resume request")
                };
                assert!(request.inventory.workspace_operation.is_some());
                let mut exact = correlation.clone();
                if wrong_correlation {
                    exact.manifest_id = identity();
                }
                send_message(
                    &mut hub,
                    Message::Resumed(Box::new(Resumed {
                        registration_revision: receipt.registration_revision(),
                        connection_epoch: positive(),
                        directives: ReconnectDirectives {
                            workspace_operation: Some(Directive {
                                correlation: OperationCorrelation::Release(exact),
                                action: DirectiveAction::FailStale,
                            }),
                            operation_failure: failed.then(|| Directive {
                                correlation: operation.clone(),
                                action: DirectiveAction::FailStale,
                            }),
                            leak_page: Some(Directive {
                                correlation: page.correlation.clone(),
                                action: DirectiveAction::Resend,
                            }),
                            ..Default::default()
                        },
                    })),
                )
                .await
                .expect("stale release decision");
            };
            let (runner, ()) = tokio::join!(resume, daemon);
            if wrong_correlation {
                assert!(runner.is_err());
                assert_eq!(
                    state.retained_release().expect("exact release retained").0,
                    &correlation
                );
            } else {
                assert!(runner.is_ok(), "stale release must not prevent reconnect");
                assert!(state.retained_release().is_none());
            }
            assert_eq!(state.retained_leak_page(), Some(&page));
        }
        drop(state);
        let state = RunnerStateRoot::open(&directory.path().join("state")).expect("second restart");
        assert!(state.reconnect_inventory().workspace_operation.is_none());
        assert!(state.reconnect_inventory().operation_failure.is_none());
        assert_eq!(state.retained_leak_page(), Some(&page));
    }
}
