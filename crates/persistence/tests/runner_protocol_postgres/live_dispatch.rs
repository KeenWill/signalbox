//! Committed claim and result boundaries fence every dispatch correlation member.

use super::*;
use signalbox_domain::{
    RunnerLeaseState, ToolAttemptObservation, ToolResultContent, ToolResultText,
};
use signalbox_persistence::runner_protocol::RunnerConnectionTransitionOutcome;

fn mismatches(correlation: &RunnerLeaseCorrelation) -> Vec<RunnerLeaseCorrelation> {
    let mut changed = Vec::new();
    macro_rules! field {
        ($field:ident, $value:expr) => {{
            let mut other = correlation.clone();
            other.$field = $value;
            changed.push(other);
        }};
    }
    field!(lease, RunnerLeaseId::from_uuid(Uuid::now_v7()));
    field!(
        generation,
        correlation
            .generation
            .checked_next()
            .expect("next generation")
    );
    field!(runner, RunnerId::from_uuid(Uuid::now_v7()));
    field!(tool, tool("other"));
    field!(
        registration_revision,
        correlation
            .registration_revision
            .checked_next()
            .expect("next registration")
    );
    field!(
        placement_revision,
        correlation
            .placement_revision
            .checked_next()
            .expect("next placement")
    );
    field!(
        working_directory,
        RunnerWorkingDirectory::try_new("/other".to_owned()).expect("absolute path")
    );
    field!(sandbox, RunnerSandboxProfile::WorkspaceRestricted);
    let dispatch = correlation.dispatch;
    for index in 0..6 {
        let other = ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session: if index == 0 {
                    SessionId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.session()
                },
                turn: if index == 1 {
                    TurnId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.turn()
                },
                issuing_attempt: if index == 2 {
                    TurnAttemptId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.issuing_attempt()
                },
                request: if index == 3 {
                    ToolRequestId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.request()
                },
                attempt: if index == 4 {
                    ToolAttemptId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.attempt()
                },
                generation: if index == 5 {
                    dispatch.generation().checked_next().expect("next dispatch")
                } else {
                    dispatch.generation()
                },
            },
        );
        field!(dispatch, other);
    }
    changed
}

fn success(text: &str) -> ToolAttemptObservation {
    ToolAttemptObservation::Completed {
        result: ToolResultContent::Text(
            ToolResultText::try_new(text.to_owned()).expect("bounded result"),
        ),
    }
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn generic_startup_scan_leaves_runner_owned_attempts_for_reconciliation()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, _, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let correlation = pin.lease.correlation();
    let repository = signalbox_persistence::startup::PostgresStartupScanRepository::new(pool);
    assert!(!store.has_unsettled_execution().await?);
    assert!(
        !repository
            .sessions()
            .await?
            .contains(&correlation.dispatch.session())
    );
    store
        .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
        .await?;
    assert!(store.has_unsettled_execution().await?);
    let mut scan = signalbox_application::StartupScanService::new(
        signalbox_application::UuidV7StartupScanIdGenerator,
        repository.clone(),
    );
    assert_eq!(scan.execute().await?.recovered_turn_count(), 0);
    assert_eq!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("recovered result")
            )
            .await?
            .state(),
        RunnerLeaseState::Completed,
        "the scan leaves the physical attempt live for its runner result"
    );
    assert!(!store.has_unsettled_execution().await?);
    assert!(
        repository
            .sessions()
            .await?
            .contains(&correlation.dispatch.session())
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_wait_reread_requires_the_exact_lost_dispatch_and_yielded_turn()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::ToolExecutionTransaction as _;
    use signalbox_persistence::tool_loop::PostgresToolLoopRepository;
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, _, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let correlation = pin.lease.correlation();
    let mut repository = PostgresToolLoopRepository::new(pool.clone());
    assert!(
        !repository
            .reread_durable_runner_wait(correlation.dispatch)
            .await?
    );
    store
        .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
        .await?;
    assert!(
        !repository
            .reread_durable_runner_wait(correlation.dispatch)
            .await?
    );
    store
        .transition_connection(
            enrollment.enrollment(),
            epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    assert!(
        !repository
            .reread_durable_runner_wait(correlation.dispatch)
            .await?,
        "connection loss alone does not prove the turn yielded"
    );
    let loss = store
        .load_current_connection_loss(enrollment.enrollment())
        .await?
        .expect("durable loss");
    store
        .propagate_connection_loss_session(loss, correlation.dispatch.session())
        .await?;
    assert!(
        repository
            .reread_durable_runner_wait(correlation.dispatch)
            .await?
    );
    for other in mismatches(&correlation)
        .into_iter()
        .filter(|other| other.dispatch != correlation.dispatch)
    {
        assert!(
            !matches!(
                repository.reread_durable_runner_wait(other.dispatch).await,
                Ok(true)
            ),
            "cross-wired runner wait: {other:?}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn live_claim_and_result_require_every_fence_and_record_one_terminal_attempt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, _, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let correlation = pin.lease.correlation();
    for other in mismatches(&correlation) {
        assert!(
            store
                .claim_tool_lease(enrollment.enrollment(), epoch, other.clone())
                .await
                .is_err(),
            "mismatched claim: {other:?}"
        );
        assert!(
            store
                .record_tool_lease_result(
                    enrollment.enrollment(),
                    epoch,
                    other.clone(),
                    success("result")
                )
                .await
                .is_err(),
            "mismatched result: {other:?}"
        );
    }
    assert!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("result")
            )
            .await
            .is_err(),
        "an offer is not a claim"
    );
    let claimed = store
        .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
        .await?;
    assert_eq!(claimed.state(), RunnerLeaseState::Claimed);
    assert_eq!(
        store
            .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
            .await?,
        claimed
    );
    let completed = store
        .record_tool_lease_result(
            enrollment.enrollment(),
            epoch,
            correlation.clone(),
            success("result"),
        )
        .await?;
    assert_eq!(completed.state(), RunnerLeaseState::Completed);
    assert_eq!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("result")
            )
            .await?,
        completed
    );
    assert!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("changed")
            )
            .await
            .is_err()
    );
    let row: (String, String) =
        sqlx::query_as("SELECT state_kind, result_text FROM tool_attempt WHERE attempt_id = $1")
            .bind(correlation.dispatch.attempt().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(row, ("terminal".to_owned(), "result".to_owned()));
    Ok(())
}

async fn resume_receipt(
    pool: &PgPool,
    enrollment: &RunnerEnrollment,
) -> Result<
    (
        signalbox_persistence::runner_protocol::RunnerEnrollmentRequestId,
        signalbox_persistence::runner_protocol::IssuedRunnerEnrollmentIdentities,
    ),
    Box<dyn Error>,
> {
    use signalbox_persistence::runner_protocol::{
        IssuedRunnerEnrollmentIdentities, RunnerEnrollmentRequestId,
    };
    let request = RunnerEnrollmentRequestId::from_uuid(Uuid::now_v7());
    let identities = IssuedRunnerEnrollmentIdentities::new(
        enrollment.enrollment(),
        enrollment.runner(),
        enrollment.authentication(),
    );
    sqlx::query("INSERT INTO runner_enrollment_request_receipt (request_id, enrollment_id, runner_id, authentication_reference_id, registration_revision) VALUES ($1, $2, $3, $4, 1)")
        .bind(request.into_uuid()).bind(enrollment.enrollment().into_uuid()).bind(enrollment.runner().into_uuid()).bind(enrollment.authentication().into_uuid())
        .execute(pool).await?;
    Ok((request, identities))
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn claimed_resume_and_retained_result_use_canonical_authority_across_epochs()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::{
        IssuedRunnerEnrollmentIdentities, RunnerLeaseResumeEvidence as Evidence,
        RunnerLeaseResumeOutcome as Outcome,
    };
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, registration, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let (request, identities) = resume_receipt(&pool, &enrollment).await?;
    let correlation = pin.lease.correlation();
    store
        .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
        .await?;
    let wrong_identity = IssuedRunnerEnrollmentIdentities::new(
        enrollment.enrollment(),
        enrollment.runner(),
        RunnerAuthenticationId::from_uuid(Uuid::now_v7()),
    );
    assert!(
        store
            .reconcile_tool_resume(
                request,
                wrong_identity,
                registration.revision(),
                &advertisement(),
                Some(Evidence::AwaitingDispatch(correlation.clone()))
            )
            .await
            .is_err()
    );
    for other in mismatches(&correlation) {
        assert!(
            store
                .reconcile_tool_resume(
                    request,
                    identities,
                    registration.revision(),
                    &advertisement(),
                    Some(Evidence::AwaitingDispatch(other))
                )
                .await
                .is_err()
        );
    }
    assert!(matches!(
        store
            .reconcile_tool_resume(
                request,
                identities,
                registration.revision(),
                &advertisement(),
                Some(Evidence::AwaitingDispatch(correlation.clone()))
            )
            .await?,
        Outcome::AwaitingDispatch
    ));
    let resumed = store.open_connection(enrollment.enrollment()).await?;
    assert!(resumed.epoch() > epoch);
    let replayed = store
        .claim_tool_lease(
            enrollment.enrollment(),
            resumed.epoch(),
            correlation.clone(),
        )
        .await?;
    assert_eq!(replayed.state(), RunnerLeaseState::Claimed);
    assert_eq!(replayed.correlation(), correlation);
    assert!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("result")
            )
            .await
            .is_err(),
        "superseded physical connection is fenced"
    );
    assert!(matches!(
        store
            .reconcile_tool_resume(
                request,
                identities,
                registration.revision(),
                &advertisement(),
                Some(Evidence::Result {
                    correlation: correlation.clone(),
                    observation: success("result")
                })
            )
            .await?,
        Outcome::Recorded
    ));
    assert_eq!(
        store
            .load_attempt_lease(correlation.dispatch.attempt())
            .await?
            .expect("completed lease")
            .state(),
        RunnerLeaseState::Completed
    );
    assert!(matches!(
        store
            .reconcile_tool_resume(
                request,
                identities,
                registration.revision(),
                &advertisement(),
                Some(Evidence::Result {
                    correlation: correlation.clone(),
                    observation: success("result")
                })
            )
            .await?,
        Outcome::Recorded
    ));
    assert!(
        store
            .reconcile_tool_resume(
                request,
                identities,
                registration.revision(),
                &advertisement(),
                Some(Evidence::Result {
                    correlation: correlation.clone(),
                    observation: success("unequal result")
                })
            )
            .await
            .is_err(),
        "an unequal duplicate is fatal, never a stale discard"
    );
    let row: (String, String) =
        sqlx::query_as("SELECT state_kind, result_text FROM tool_attempt WHERE attempt_id = $1")
            .bind(correlation.dispatch.attempt().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(row, ("terminal".to_owned(), "result".to_owned()));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn omitted_or_started_claims_require_loss_and_cannot_be_replayed_afterward()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::{
        RunnerLeaseResumeEvidence as Evidence, RunnerLeaseResumeOutcome as Outcome,
    };
    for omitted in [false, true] {
        let (_container, pool) = migrated_postgres().await?;
        let (store, enrollment, registration, pin, epoch) =
            stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
                .await?;
        let (request, identities) = resume_receipt(&pool, &enrollment).await?;
        let correlation = pin.lease.correlation();
        store
            .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
            .await?;
        let evidence = (!omitted).then(|| Evidence::ExecutionPossible(correlation.clone()));
        assert!(
            matches!(store.reconcile_tool_resume(request, identities, registration.revision(), &advertisement(), evidence).await?, Outcome::LoseConnection(connection) if connection.epoch() == epoch)
        );
        store
            .transition_connection(
                enrollment.enrollment(),
                epoch,
                RunnerConnectionTransition::TransportClosed,
            )
            .await?;
        let loss = store
            .load_current_connection_loss(enrollment.enrollment())
            .await?
            .expect("loss fence");
        store
            .propagate_connection_loss_session(loss, correlation.dispatch.session())
            .await?;
        assert_eq!(
            store
                .load_attempt_lease(correlation.dispatch.attempt())
                .await?
                .expect("lost lease")
                .state(),
            RunnerLeaseState::LostClaimed
        );
        assert!(
            store
                .load_runner_recovery_wait(correlation.dispatch.session())
                .await?
                .is_some()
        );
        assert!(matches!(
            store
                .reconcile_tool_resume(
                    request,
                    identities,
                    registration.revision(),
                    &advertisement(),
                    Some(Evidence::AwaitingDispatch(correlation.clone()))
                )
                .await?,
            Outcome::Lost
        ));
        assert!(matches!(
            store
                .reconcile_tool_resume(
                    request,
                    identities,
                    registration.revision(),
                    &advertisement(),
                    Some(Evidence::Result {
                        correlation: correlation.clone(),
                        observation: success("too late")
                    })
                )
                .await?,
            Outcome::Lost
        ));
        let resumed = store.open_connection(enrollment.enrollment()).await?;
        assert!(
            store
                .claim_tool_lease(
                    enrollment.enrollment(),
                    resumed.epoch(),
                    correlation.clone()
                )
                .await
                .is_err()
        );
        assert!(
            store
                .record_tool_lease_result(
                    enrollment.enrollment(),
                    resumed.epoch(),
                    correlation.clone(),
                    success("too late")
                )
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn shutdown_loses_offered_and_claimed_leases_but_preserves_completed_results()
-> Result<(), Box<dyn Error>> {
    for transition in [
        RunnerConnectionTransition::DaemonShutdown,
        RunnerConnectionTransition::RunnerShutdown,
    ] {
        for state in [
            RunnerLeaseState::Offered,
            RunnerLeaseState::Claimed,
            RunnerLeaseState::Completed,
        ] {
            let (_container, pool) = migrated_postgres().await?;
            let (store, enrollment, _, pin, epoch) = stored_active_pin_fixture_with_authorization(
                &pool,
                ActivePinEffectCase::EffectFree,
            )
            .await?;
            let correlation = pin.lease.correlation();
            if state != RunnerLeaseState::Offered {
                store
                    .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
                    .await?;
            }
            if state == RunnerLeaseState::Completed {
                store
                    .record_tool_lease_result(
                        enrollment.enrollment(),
                        epoch,
                        correlation.clone(),
                        success("settled"),
                    )
                    .await?;
            }
            let outcome = store
                .transition_connection(enrollment.enrollment(), epoch, transition)
                .await?;
            let RunnerConnectionTransitionOutcome::Current(snapshot) = outcome else {
                panic!("current shutdown epoch");
            };
            if state == RunnerLeaseState::Completed {
                assert_eq!(snapshot.state(), RunnerConnectionState::Shutdown);
                assert_eq!(
                    snapshot.cause(),
                    if transition == RunnerConnectionTransition::DaemonShutdown {
                        RunnerConnectionCause::DaemonShutdown
                    } else {
                        RunnerConnectionCause::RunnerShutdown
                    }
                );
            } else {
                assert_eq!(snapshot.state(), RunnerConnectionState::Lost);
                assert_eq!(snapshot.cause(), RunnerConnectionCause::TransportClosed);
                let loss = store
                    .load_current_connection_loss(enrollment.enrollment())
                    .await?
                    .expect("durable loss");
                store
                    .propagate_connection_loss_session(loss, correlation.dispatch.session())
                    .await?;
                assert_eq!(
                    store
                        .load_lease(correlation.lease, correlation.generation)
                        .await?
                        .expect("retained lease")
                        .state(),
                    if state == RunnerLeaseState::Offered {
                        RunnerLeaseState::LostUnclaimed
                    } else {
                        RunnerLeaseState::LostClaimed
                    }
                );
            }
        }
    }
    Ok(())
}
