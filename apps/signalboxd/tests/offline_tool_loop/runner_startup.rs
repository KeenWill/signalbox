//! Retained runner execution settles before generic startup classification.

use super::*;
use signalbox_runner_wire::{
    Advertisement, CanonicalUuid, CapabilityName, DIGEST_VERSION, DirectiveAction, Enroll,
    LeaseClaim, LeasePhase, LeasePhaseKind, ReconnectInventory, Resume, RetainedResult,
    SandboxProfile, TerminalResult, WireToolName,
};
use signalboxd::runner_protocol_runtime::{
    PostgresRunnerRegistrationService, RunnerEnrollmentResponse, RunnerRegistrationService as _,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retained_runner_result_settles_before_generic_startup_scan() -> Result<(), Box<dyn Error>>
{
    tokio::time::timeout(Duration::from_secs(60), recover_retained_result(true)).await?
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn startup_rechecks_a_settled_lease_without_a_dispatch_notification()
-> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(60), recover_retained_result(false)).await?
}

async fn recover_retained_result(notify_recovery: bool) -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let fixture = ToolLoopFixture::with_creation_placement(
        DangerousToolAutoApproval::Disabled,
        None,
        migrated_postgres().await?,
        Some(super::runner_execution::placement(
            directory.path().to_owned(),
        )),
    )
    .await?;
    let advertisement = Advertisement {
        default_working_directory: None,
        capability_classes: vec![CapabilityName::try_new("echo".to_owned())?],
        tools: vec![WireToolName::try_new("echo".to_owned())?],
        workspace_capabilities: Vec::new(),
        sandbox_profiles: vec![SandboxProfile::Ambient],
        credential_profiles: Vec::new(),
        repositories: Vec::new(),
    };
    let original =
        PostgresRunnerRegistrationService::local(fixture.pool.clone()).expect("compiled catalog");
    let RunnerEnrollmentResponse::Active(receipt) = original
        .enroll(Enroll {
            request_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
            digest_version: DIGEST_VERSION,
            advertisement: advertisement.clone(),
        })
        .await?
    else {
        panic!("initial enrollment is active")
    };
    let (catalog, executor) = offline_daemon_tools(
        OfflineWebTransport::unused(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let arguments = serde_json::json!({"text": "retained before daemon restart"}).to_string();
    let dispatch = original.dispatch_service();
    let (execution, _) = fixture.execution(
        [tool_use_script(&[("echo", arguments.as_str())])],
        catalog,
        executor.with_runner_dispatch(dispatch.clone()),
    );
    let execution = execution.with_runner_dispatch(dispatch);
    let mut executing = Box::pin(execution.execute(Box::new(fixture.activated.clone())));
    let offer = tokio::select! {
        outcome = &mut executing => panic!("execution ended before claim: {outcome:?}"),
        offer = async {
            loop {
                if let Some(offer) = original.pending_tool_offer(receipt.enrollment_id, receipt.connection_epoch).await? {
                    break Ok::<_, Box<dyn Error>>(offer);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => offer?,
    };
    original
        .claim_tool_offer(
            receipt.enrollment_id,
            receipt.connection_epoch,
            LeaseClaim {
                correlation: offer.correlation.clone(),
            },
        )
        .await?;
    drop(executing);
    drop(execution);
    drop(original);

    let restarted = PostgresRunnerRegistrationService::local(fixture.pool.clone())
        .expect("same compiled catalog")
        .with_recovery_only();
    let store = restarted.recovery_store();
    let prior_connections = store.load_nonterminal_connection_heads().await?;
    assert!(store.has_unsettled_execution().await?);
    let repository = PostgresStartupScanRepository::new(fixture.pool.clone());
    assert!(!repository.sessions().await?.contains(&fixture.session));
    let recover_then_scan = async {
        let transitions = restarted.reconcile_startup(prior_connections).await?;
        assert!(
            transitions.is_empty(),
            "the resumed physical epoch is not orphaned"
        );
        assert!(!store.has_unsettled_execution().await?);
        assert_eq!(
            store
                .load_attempt_lease(signalbox_domain::ToolAttemptId::from_uuid(
                    offer.correlation.tool_attempt_id.into_uuid()
                ))
                .await?
                .expect("canonical completed lease")
                .state(),
            signalbox_domain::RunnerLeaseState::Completed
        );
        assert!(repository.sessions().await?.contains(&fixture.session));
        let mut scan = StartupScanService::new(UuidV7StartupScanIdGenerator, repository);
        let scanned = scan.execute().await?;
        assert_eq!(scanned.recovered_turn_count(), 0);
        Ok::<_, Box<dyn Error>>(())
    };
    let mut recover_then_scan = Box::pin(recover_then_scan);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut recover_then_scan)
            .await
            .is_err(),
        "recovery waits for the retained claimed lease"
    );
    let resume_service = if notify_recovery {
        restarted.clone()
    } else {
        PostgresRunnerRegistrationService::local(fixture.pool.clone())
            .expect("independent resume service")
            .with_recovery_only()
    };
    let notifications = restarted.lease_changes().expect("dispatch notifications");
    let resume = async {
        let resumed = resume_service
            .resume(Resume {
                request_id: receipt.request_id,
                digest_version: DIGEST_VERSION,
                enrollment_id: receipt.enrollment_id,
                runner_id: receipt.runner_id,
                authentication_id: receipt.authentication_id,
                prior_registration_revision: receipt.registration_revision,
                advertisement,
                inventory: ReconnectInventory {
                    lease: Some(LeasePhase {
                        correlation: offer.correlation.clone(),
                        phase: LeasePhaseKind::ExecutionMayHaveStarted,
                    }),
                    result: Some(RetainedResult {
                        correlation: offer.correlation.clone(),
                        result: TerminalResult::Success { text: arguments },
                    }),
                    ..Default::default()
                },
            })
            .await?;
        assert!(resumed.connection_epoch > receipt.connection_epoch);
        assert_eq!(
            resumed.directives.result.expect("recorded result").action,
            DirectiveAction::DiscardAsRecorded
        );
        if !notify_recovery {
            assert!(
                !notifications.has_changed()?,
                "durable settlement produces no local wake"
            );
        }
        Ok::<_, Box<dyn Error>>(())
    };
    let (recovered, resumed) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), recover_then_scan),
        resume
    );
    recovered??;
    resumed?;
    assert_eq!(
        store
            .load_connection(signalbox_domain::RunnerEnrollmentId::from_uuid(
                receipt.enrollment_id.into_uuid()
            ))
            .await?
            .expect("new connection")
            .state(),
        signalbox_persistence::runner_protocol::RunnerConnectionState::Connected
    );
    Ok(())
}
