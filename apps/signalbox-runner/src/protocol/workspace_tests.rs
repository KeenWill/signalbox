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
        receipt,
        advertisement: config.advertisement().clone(),
        configuration: Some(config),
        outcome: EnrollmentOutcome::Enrolled,
        connection_epoch: positive(),
        heartbeat: None,
    }
}
fn enrolled(directory: &TempDir) -> RunnerStateRoot {
    let mut state = RunnerStateRoot::open(&directory.path().join("state")).expect("private root");
    state
        .record_receipt(EnrollmentReceipt::new(
            state.state().request_id(),
            identity(),
            identity(),
            identity(),
            positive(),
            advertisement_digest(configuration().advertisement()).expect("digest"),
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
