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
