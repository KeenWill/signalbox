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

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn lease_refusal_settles_authority_with_immutable_verbatim_evidence()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::{
        RunnerLeaseFailureKind, RunnerLeaseResumeEvidence, RunnerLeaseResumeOutcome,
    };
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, registration, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let correlation = pin.lease.correlation();
    let detail = serde_json::json!({"code":"workspace_open_failed","message":"cannot open /private/work","payload":{"path":"/private/work","nested":[{"credential":"/private/key"}],"attempts":1}});
    let refused_without_evidence = pin
        .lease
        .refuse(correlation.clone())
        .expect("offered authority");
    assert!(
        store.store_lease(&refused_without_evidence).await.is_err(),
        "SQL forbids refusal without evidence and attempt settlement"
    );
    for other in mismatches(&correlation) {
        assert!(
            store
                .record_tool_lease_failure(
                    enrollment.enrollment(),
                    epoch,
                    other,
                    RunnerLeaseFailureKind::LeaseAdmissionRefused,
                    &detail
                )
                .await
                .is_err()
        );
    }
    let refused = store
        .record_tool_lease_failure(
            enrollment.enrollment(),
            epoch,
            correlation.clone(),
            RunnerLeaseFailureKind::LeaseAdmissionRefused,
            &detail,
        )
        .await
        .expect("offered refusal commits");
    assert_eq!(refused.state(), RunnerLeaseState::Refused);
    let terminal: (String, String, String) = sqlx::query_as("SELECT state_kind,terminal_disposition_kind,error_kind FROM tool_attempt WHERE attempt_id = $1")
        .bind(correlation.dispatch.attempt().into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        terminal,
        (
            "terminal".to_owned(),
            "known_failed".to_owned(),
            "execution_failed".to_owned()
        )
    );
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM runner_lease_failure WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, detail);
    assert!(
        store
            .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .record_tool_lease_failure(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                RunnerLeaseFailureKind::LeaseAdmissionRefused,
                &detail
            )
            .await
            .expect("exact refusal replays")
            .state(),
        RunnerLeaseState::Refused
    );
    let unequal = serde_json::json!({"code":"workspace_open_failed","message":"different detail","payload":{}});
    assert!(
        store
            .record_tool_lease_failure(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                RunnerLeaseFailureKind::LeaseAdmissionRefused,
                &unequal
            )
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE runner_lease_failure SET detail = $2 WHERE lease_id = $1")
            .bind(correlation.lease.into_uuid())
            .bind(&unequal)
            .execute(&pool)
            .await
            .is_err()
    );
    use signalbox_persistence::runner_protocol::status::{RunnerStatusFailure, read_runner_status};
    let mut after = None;
    let mut projected = Vec::new();
    loop {
        let page = read_runner_status(&pool, 1, after).await?;
        assert!(page.runners.len() + page.failures.len() + page.leaks.len() <= 1);
        projected.extend(page.failures);
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(
        projected,
        [RunnerStatusFailure::LeaseOffer {
            correlation: correlation.clone(),
            category: RunnerLeaseFailureKind::LeaseAdmissionRefused,
            detail: detail.clone()
        }]
    );
    let (request, identities) = resume_receipt(&pool, &enrollment).await?;
    assert!(matches!(
        store
            .reconcile_tool_resume(
                request,
                identities,
                registration.revision(),
                &advertisement(),
                Some(RunnerLeaseResumeEvidence::Refusal {
                    correlation,
                    category: RunnerLeaseFailureKind::LeaseAdmissionRefused,
                    detail,
                })
            )
            .await
            .expect("refusal resume reconciles"),
        RunnerLeaseResumeOutcome::Recorded
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn lease_refusal_cannot_settle_claimed_execution() -> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::RunnerLeaseFailureKind;
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, _, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let correlation = pin.lease.correlation();
    store
        .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
        .await?;
    let detail =
        serde_json::json!({"code":"admission_refused","message":"cannot admit","payload":{}});
    assert!(
        store
            .record_tool_lease_failure(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                RunnerLeaseFailureKind::LeaseAdmissionRefused,
                &detail
            )
            .await
            .is_err()
    );
    assert_eq!(
        store
            .load_lease(correlation.lease, correlation.generation)
            .await?
            .expect("retained lease")
            .state(),
        RunnerLeaseState::Claimed
    );
    assert!(sqlx::query("INSERT INTO runner_lease_event (lease_id,generation,event_ordinal,state_kind) VALUES ($1,$2,3,'refused')")
        .bind(correlation.lease.into_uuid()).bind(Decimal::from(correlation.generation.get())).execute(&pool).await.is_err(), "the third refusal event requires lost-unclaimed authority, never a claim");
    let mut invalid = pool.begin().await?;
    sqlx::query("INSERT INTO runner_lease_failure (lease_id,generation,category,detail) VALUES ($1,$2,'lease_admission_refused',$3)")
        .bind(correlation.lease.into_uuid()).bind(Decimal::from(correlation.generation.get())).bind(&detail).execute(&mut *invalid).await?;
    assert!(
        invalid.commit().await.is_err(),
        "SQL does not permit refusal evidence for a claimed lease"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM runner_lease_failure")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn resume_commits_unacknowledged_lease_refusal_before_reopening_connection()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::{
        RunnerLeaseFailureKind, RunnerLeaseResumeEvidence, RunnerLeaseResumeOutcome,
    };
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, registration, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let correlation = pin.lease.correlation();
    let detail =
        serde_json::json!({"code":"admission_unavailable","message":"cannot admit","payload":{}});
    let (request, identities) = resume_receipt(&pool, &enrollment).await?;
    assert!(matches!(
        store
            .reconcile_tool_resume(
                request,
                identities,
                registration.revision(),
                &advertisement(),
                Some(RunnerLeaseResumeEvidence::Refusal {
                    correlation: correlation.clone(),
                    category: RunnerLeaseFailureKind::LeaseAdmissionRefused,
                    detail: detail.clone(),
                })
            )
            .await?,
        RunnerLeaseResumeOutcome::Recorded
    ));
    assert_eq!(
        store
            .load_connection(enrollment.enrollment())
            .await?
            .expect("connection")
            .epoch(),
        epoch
    );
    assert_eq!(
        store
            .load_lease(correlation.lease, correlation.generation)
            .await?
            .expect("retained refusal")
            .state(),
        RunnerLeaseState::Refused
    );
    let retained: serde_json::Value =
        sqlx::query_scalar("SELECT detail FROM runner_lease_failure WHERE lease_id = $1")
            .bind(correlation.lease.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(retained, detail);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn resume_retains_refusal_after_transport_loss_and_placement_propagation()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::runner_protocol::{
        RunnerLeaseFailureKind, RunnerLeaseResumeEvidence, RunnerLeaseResumeOutcome,
    };
    for effect in [
        ActivePinEffectCase::EffectFree,
        ActivePinEffectCase::IdempotentExternalEffect,
        ActivePinEffectCase::SideEffectingExternalEffect,
    ] {
        let (_container, pool) = migrated_postgres().await?;
        let (store, enrollment, registration, pin, epoch) =
            stored_active_pin_fixture_with_authorization(&pool, effect).await?;
        let correlation = pin.lease.correlation();
        let detail = serde_json::json!({"code":"admission_unavailable","message":"cannot admit","payload":{}});
        store
            .transition_connection(
                enrollment.enrollment(),
                epoch,
                RunnerConnectionTransition::TransportClosed,
            )
            .await?;
        let (request, identities) = resume_receipt(&pool, &enrollment).await?;
        let evidence = RunnerLeaseResumeEvidence::Refusal {
            correlation: correlation.clone(),
            category: RunnerLeaseFailureKind::LeaseAdmissionRefused,
            detail: detail.clone(),
        };
        assert!(
            matches!(
                store
                    .reconcile_tool_resume(
                        request,
                        identities,
                        registration.revision(),
                        &advertisement(),
                        Some(evidence.clone())
                    )
                    .await?,
                RunnerLeaseResumeOutcome::LoseConnection(_)
            ),
            "pending loss is propagated before refusing the offer"
        );
        let loss = store
            .load_current_connection_loss(enrollment.enrollment())
            .await?
            .expect("durable transport loss");
        store
            .propagate_connection_loss_session(loss, correlation.dispatch.session())
            .await?;
        assert_eq!(
            store
                .load_lease(correlation.lease, correlation.generation)
                .await?
                .expect("lost offer")
                .state(),
            RunnerLeaseState::LostUnclaimed
        );
        for other in mismatches(&correlation) {
            assert!(
                store
                    .reconcile_tool_resume(
                        request,
                        identities,
                        registration.revision(),
                        &advertisement(),
                        Some(RunnerLeaseResumeEvidence::Refusal {
                            correlation: other,
                            category: RunnerLeaseFailureKind::LeaseAdmissionRefused,
                            detail: detail.clone(),
                        })
                    )
                    .await
                    .is_err(),
                "loss does not weaken correlation authentication"
            );
        }
        assert!(matches!(
            store
                .reconcile_tool_resume(
                    request,
                    identities,
                    registration.revision(),
                    &advertisement(),
                    Some(RunnerLeaseResumeEvidence::Refusal {
                        correlation: correlation.clone(),
                        category: RunnerLeaseFailureKind::LeaseAdmissionRefused,
                        detail: detail.clone(),
                    })
                )
                .await?,
            RunnerLeaseResumeOutcome::Recorded
        ));
        assert_eq!(
            store
                .load_connection(enrollment.enrollment())
                .await?
                .expect("connection")
                .epoch(),
            epoch
        );
        assert_eq!(
            store
                .load_lease(correlation.lease, correlation.generation)
                .await?
                .expect("retained refusal")
                .state(),
            RunnerLeaseState::Refused
        );
        let retained: serde_json::Value =
            sqlx::query_scalar("SELECT detail FROM runner_lease_failure WHERE lease_id = $1")
                .bind(correlation.lease.into_uuid())
                .fetch_one(&pool)
                .await?;
        assert_eq!(retained, detail);
        let terminal: (String, String, String) = sqlx::query_as("SELECT state_kind,terminal_disposition_kind,error_kind FROM tool_attempt WHERE attempt_id = $1")
        .bind(correlation.dispatch.attempt().into_uuid()).fetch_one(&pool).await?;
        assert_eq!(
            terminal,
            (
                "terminal".to_owned(),
                "known_failed".to_owned(),
                "execution_failed".to_owned()
            )
        );

        let states: Vec<String> = sqlx::query_scalar(
            "SELECT state_kind FROM runner_lease_event WHERE lease_id = $1 ORDER BY event_ordinal",
        )
        .bind(correlation.lease.into_uuid())
        .fetch_all(&pool)
        .await?;
        assert_eq!(states, ["offered", "lost_unclaimed", "refused"]);
        let proofs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_lease_no_execution_proof WHERE lease_id = $1",
        )
        .bind(correlation.lease.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            proofs, 1,
            "refusal preserves the durable no-execution evidence"
        );
        let connection = store.open_connection(enrollment.enrollment()).await?;
        assert!(connection.epoch() > epoch);
        assert!(matches!(
            store
                .reconcile_tool_resume(
                    request,
                    identities,
                    registration.revision(),
                    &advertisement(),
                    Some(evidence.clone())
                )
                .await?,
            RunnerLeaseResumeOutcome::Recorded
        ));
        let mut unequal = evidence;
        let RunnerLeaseResumeEvidence::Refusal { detail, .. } = &mut unequal else {
            panic!("refusal fixture")
        };
        detail["message"] = serde_json::json!("different refusal");
        assert!(
            store
                .reconcile_tool_resume(
                    request,
                    identities,
                    registration.revision(),
                    &advertisement(),
                    Some(unequal)
                )
                .await
                .is_err()
        );
        assert!(
            store
                .claim_tool_lease(enrollment.enrollment(), connection.epoch(), correlation)
                .await
                .is_err()
        );
    }
    Ok(())
}
