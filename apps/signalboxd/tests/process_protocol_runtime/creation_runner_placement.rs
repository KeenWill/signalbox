//! Runner placement admission and retention through all session creation verbs.

use super::*;
use signalbox_domain::{
    RunnerAdvertisement, RunnerAuthenticationId, RunnerCapabilityClass, RunnerEnrollmentId,
    RunnerEnrollmentRequestId, RunnerId,
};
use signalbox_persistence::runner_protocol::{
    IssuedRunnerEnrollmentIdentities, PristineRunnerEnrollmentRequest,
};
use signalbox_process_protocol::{
    RunnerPlacementRequest, RunnerSandboxProfile, RunnerToolPermission,
    RunnerToolPermissionOverride,
};

async fn enroll_echo(runtime: &RunningRuntime) -> Result<RunnerEnrollmentId, Box<dyn Error>> {
    let store = signalboxd::runner_protocol_runtime::PostgresRunnerRegistrationService::local(
        runtime.pool.clone(),
    )
    .expect("compiled local runner catalog")
    .recovery_store();
    let class = RunnerCapabilityClass::try_new("echo".to_owned()).expect("echo capability class");
    let receipt = store
        .enroll_pristine(PristineRunnerEnrollmentRequest::new(
            RunnerEnrollmentRequestId::from_uuid(Uuid::now_v7()),
            IssuedRunnerEnrollmentIdentities::new(
                RunnerEnrollmentId::from_uuid(Uuid::now_v7()),
                RunnerId::from_uuid(Uuid::now_v7()),
                RunnerAuthenticationId::from_uuid(Uuid::now_v7()),
            ),
            [class.clone()],
            RunnerAdvertisement::new(
                [class],
                [ToolName::try_new("echo".to_owned()).expect("echo tool name")],
                [
                    signalbox_domain::CredentialProfileName::try_new("github-runner".to_owned())
                        .expect("compiled credential profile"),
                ],
                [],
                [signalbox_domain::RunnerSandboxProfile::Ambient],
                [],
            ),
        ))
        .await?
        .into_receipt();
    Ok(receipt.identities().enrollment())
}

fn placement() -> RunnerPlacementRequest {
    serde_json::from_value(serde_json::json!({
        "selector": {"type": "capability_class", "name": "echo"},
        "working_directory": {"type": "exact", "directory": "/tmp/runner-work"},
        "credential_profile": null,
        "workspace": {"type": "none"},
        "sandbox": "ambient",
        "permission_overrides": [{"tool_name": "echo", "permission": "confirm"}]
    }))
    .expect("explicit runner placement fixture")
}

fn requests(
    command_id: CommandId,
    runner_placement: Option<RunnerPlacementRequest>,
) -> [ClientRequest; 3] {
    [
        ClientRequest::CreateSession {
            command_id,
            initial_model_selection: ModelSelection::Alias {
                alias_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(None),
            placement: SessionPlacement::Pathless {},
            lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            runner_placement: runner_placement.clone(),
        },
        ClientRequest::CreateSessionFromTemplate {
            command_id,
            template_name: "merge-forward".to_owned(),
            placement: SessionPlacement::Pathless {},
            lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            runner_placement: runner_placement.clone(),
        },
        ClientRequest::CommissionSession {
            command_id,
            template_name: "merge-forward".to_owned(),
            fence: CommissionedSessionFence::Branch {
                repository: "sample/repository".to_owned(),
                branch: "main".to_owned(),
            },
            statement: "Review the change".to_owned(),
            content: InputContent::new("Review context".to_owned()),
            runner_placement,
        },
    ]
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn all_creation_verbs_retain_unpinned_placement_and_compare_it_on_replay()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let enrollment = enroll_echo(&runtime).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let mut named_profile = placement();
    named_profile.credential_profile = Some(
        signalbox_process_protocol::RunnerCredentialProfileName::try_new(
            "github-runner".to_owned(),
        )
        .unwrap(),
    );
    let store = signalboxd::runner_protocol_runtime::PostgresRunnerRegistrationService::local(
        runtime.pool.clone(),
    )
    .unwrap()
    .recovery_store();
    let mut expected_status = Vec::new();
    let mut replays = Vec::new();
    for requested in [None, Some(placement()), Some(named_profile)] {
        for kind in 0..3 {
            let command_id = command()?;
            let request = requests(command_id, requested.clone())[kind].clone();
            connection.request(1, request.clone()).await?;
            let first = response_within(&mut connection).await?.message().clone();
            let session_id = match &first {
                ServerMessage::SessionCreated { session_id, .. }
                | ServerMessage::SessionCommissioned { session_id, .. } => *session_id,
                other => panic!("creation failed: {other:?}"),
            };
            connection.request(2, request.clone()).await?;
            assert_eq!(response_within(&mut connection).await?.message(), &first);
            replays.push((request, first));
            let expected = requested
                .clone()
                .map(|placement| placement.try_into_domain().unwrap());
            let stored = store
                .load_placement(SessionId::from_uuid(session_id.into_uuid()))
                .await?;
            match (stored, &expected) {
                (Some(stored), Some(expected)) => {
                    assert_eq!(stored.placement().request(), expected);
                    assert_eq!(
                        stored.placement().state(),
                        &signalbox_domain::SessionRunnerPlacementState::Unpinned
                    );
                    assert!(stored.grant().is_none());
                    expected_status.push(session_id);
                }
                (None, None) => {}
                mismatch => panic!("placement absence changed: {mismatch:?}"),
            }
            let changed = if requested.is_some() {
                None
            } else {
                Some(placement())
            };
            connection
                .request(3, requests(command_id, changed)[kind].clone())
                .await?;
            assert_eq!(
                protocol_error_code(response_within(&mut connection).await?.message()),
                ErrorCode::ConflictingReuse
            );
        }
    }
    connection
        .request(
            4,
            ClientRequest::ReadRunnerStatus {
                page_size: 100,
                after: None,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::RunnerStatusStart {}
    );
    let mut observed = Vec::new();
    loop {
        match response_within(&mut connection).await?.message() {
            ServerMessage::RunnerStatus {
                status:
                    signalbox_process_protocol::RunnerStatusFact::Placement { session_id, runner },
            } => {
                let projection = serde_json::to_value(runner)?;
                assert_eq!(projection["state"], "unpinned");
                assert!(projection["runner_id"].is_null());
                assert!(projection["repository"].is_null());
                assert_eq!(projection["working_directory"], "/tmp/runner-work");
                observed.push(*session_id);
            }
            ServerMessage::RunnerStatus { .. } => {}
            ServerMessage::RunnerStatusEnd { .. } => break,
            other => panic!("unexpected status: {other:?}"),
        }
    }
    observed.sort_by_key(|id| id.into_uuid());
    expected_status.sort_by_key(|id| id.into_uuid());
    assert_eq!(observed, expected_status);
    let leases: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_lease_generation")
        .fetch_one(&runtime.pool)
        .await?;
    assert_eq!(leases, 0);
    let mut enrollment = store.load_enrollment(enrollment).await?.unwrap();
    store.revoke_enrollment(&mut enrollment).await?;
    for (request, expected) in replays {
        connection.request(5, request).await?;
        assert_eq!(response_within(&mut connection).await?.message(), &expected);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn all_creation_verbs_refuse_unadvertised_sandbox_and_unadmitted_override_before_claiming()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    enroll_echo(&runtime).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let mut unsupported_sandbox = placement();
    unsupported_sandbox.sandbox = RunnerSandboxProfile::WorkspaceRestricted;
    let mut unknown_override = placement();
    unknown_override.permission_overrides = vec![RunnerToolPermissionOverride {
        tool_name: "uncompiled".to_owned(),
        permission: RunnerToolPermission::Auto,
    }];
    for rejected in [unsupported_sandbox, unknown_override] {
        for kind in 0..3 {
            let command_id = command()?;
            connection
                .request(
                    1,
                    requests(command_id, Some(rejected.clone()))[kind].clone(),
                )
                .await?;
            assert_eq!(
                protocol_error_code(response_within(&mut connection).await?.message()),
                ErrorCode::InvalidRequest
            );
            let claimed: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM durable_command WHERE command_id = $1)",
            )
            .bind(command_id.into_uuid())
            .fetch_one(&runtime.pool)
            .await?;
            assert!(!claimed);
            connection
                .request(2, requests(command_id, Some(placement()))[kind].clone())
                .await?;
            assert!(matches!(
                response_within(&mut connection).await?.message(),
                ServerMessage::SessionCreated { .. } | ServerMessage::SessionCommissioned { .. }
            ));
        }
    }
    Ok(())
}
