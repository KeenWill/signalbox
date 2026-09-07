//! Command replay and pending-successor authority under PostgreSQL transactions.

use super::*;
use signalbox_domain::{
    AbandonLostRunner, AbandonLostRunnerResult, PromotePendingRunner, PromotePendingRunnerResult,
    RunnerEnrollmentRequestId, RunnerEnrollmentState, RunnerRecoveryRejection,
};
use signalbox_persistence::runner_protocol::{
    IssuedRunnerEnrollmentIdentities, PristineRunnerEnrollmentRequest, RunnerRecoveryOutcome,
};

pub(super) fn enrollment_request() -> PristineRunnerEnrollmentRequest {
    PristineRunnerEnrollmentRequest::new(
        RunnerEnrollmentRequestId::from_uuid(Uuid::now_v7()),
        IssuedRunnerEnrollmentIdentities::new(
            RunnerEnrollmentId::from_uuid(Uuid::now_v7()),
            RunnerId::from_uuid(Uuid::now_v7()),
            RunnerAuthenticationId::from_uuid(Uuid::now_v7()),
        ),
        [class()],
        advertisement(),
    )
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_promotion_replays_after_candidate_disconnects() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let predecessor_connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            predecessor_connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let pending_request = enrollment_request();
    let pending_request_id = pending_request.request();
    let candidate = store
        .enroll_pristine(pending_request.clone())
        .await?
        .into_receipt();
    let candidate_connection = store
        .open_connection(candidate.identities().enrollment())
        .await?;
    assert_eq!(
        candidate.enrollment().state(),
        RunnerEnrollmentState::Pending
    );
    let replay = store.enroll_pristine(pending_request).await?.into_receipt();
    assert_eq!(candidate.identities(), replay.identities());
    let command = PromotePendingRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        enrollment_request: pending_request_id,
    };

    let result = store.promote_pending_runner(command.clone()).await?;

    let active_receipt = store
        .promoted_runner_receipt(candidate.identities().enrollment())
        .await?
        .expect("the promoted candidate has a deliverable active receipt");
    assert_eq!(active_receipt.request(), pending_request_id);
    assert_eq!(active_receipt.identities(), candidate.identities());
    assert_eq!(
        active_receipt.enrollment().state(),
        RunnerEnrollmentState::Active
    );

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(PromotePendingRunnerResult::Promoted {
            runner: candidate.identities().runner()
        })
    );
    assert_eq!(
        store
            .load_enrollment(predecessor.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Revoked
    );
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Active
    );
    store
        .transition_connection(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    assert_eq!(store.promote_pending_runner(command).await?, result);
    let placements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_session_placement_record")
            .fetch_one(&pool)
            .await?;
    assert_eq!(placements, 0);
    let mut promoted = store
        .load_enrollment(candidate.identities().enrollment())
        .await?
        .unwrap();
    assert!(store.revoke_enrollment(&mut promoted).await?);
    assert_eq!(promoted.state(), RunnerEnrollmentState::Revoked);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pending_enrollment_cannot_change_its_advertisement() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool, catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let changed = RunnerAdvertisement::new([], [], [], [], [], []);

    let result = store
        .resume_registration(
            candidate.request(),
            candidate.identities(),
            candidate.registration().revision(),
            changed,
        )
        .await;

    assert!(matches!(result, Err(RunnerProtocolStoreError::EnrollmentRequest(
        signalbox_persistence::runner_protocol::RunnerEnrollmentRequestFailure::ReplayAdvertisementMismatch { .. }
    ))));
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Pending
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandonment_replays_the_original_rejection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let command = AbandonLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: SessionId::from_uuid(Uuid::now_v7()),
    };
    let result = store.abandon_lost_runner(command.clone()).await?;
    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(AbandonLostRunnerResult::Rejected(
            RunnerRecoveryRejection::SessionNotFound
        ))
    );
    insert_session_for(&pool, command.session.into_uuid()).await?;

    assert_eq!(store.abandon_lost_runner(command.clone()).await?, result);
    let conflicting = AbandonLostRunner {
        session: SessionId::from_uuid(Uuid::now_v7()),
        ..command.clone()
    };
    assert_eq!(
        store.abandon_lost_runner(conflicting).await?,
        RunnerRecoveryOutcome::ConflictingReuse
    );
    let other_kind = PromotePendingRunner {
        command_id: command.command_id,
        enrollment_request: RunnerEnrollmentRequestId::from_uuid(Uuid::now_v7()),
    };
    assert_eq!(
        store.promote_pending_runner(other_kind).await?,
        RunnerRecoveryOutcome::ConflictingReuse
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pre_pin_replacement_promotes_and_replays_without_provisioning()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        exact_runner_request(predecessor.identities().runner()),
    );
    store.store_placement(&placement, None, None).await?;
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };

    let result = store.replace_lost_runner(command.clone()).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Replaced {
            runner: candidate.identities().runner(),
            placement_revision: placement.revision().checked_next().unwrap(),
        })
    );
    assert_eq!(store.replace_lost_runner(command).await?, result);
    let stored = store.load_placement(session).await?.unwrap();
    assert_eq!(
        stored.placement().state(),
        &SessionRunnerPlacementState::Unpinned
    );
    assert_eq!(
        stored.placement().request().selector,
        RunnerSelector::Identity(candidate.identities().runner())
    );
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Active
    );
    let authorizations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_replacement_provisioning_authorization")
            .fetch_one(&pool)
            .await?;
    assert_eq!(authorizations, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandonment_terminalizes_the_lost_pre_pin_placement() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        exact_runner_request(RunnerId::from_uuid(uuid(RUNNER))),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let command = AbandonLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
    };

    let result = store.abandon_lost_runner(command.clone()).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(AbandonLostRunnerResult::Abandoned)
    );
    assert_eq!(store.abandon_lost_runner(command).await?, result);
    assert!(matches!(
        store
            .load_placement(session)
            .await?
            .unwrap()
            .placement()
            .state(),
        SessionRunnerPlacementState::RunnerAbandoned(_)
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pinned_installation_commits_one_reference_boundary_and_replays()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, predecessor, _, pin) = stored_pin_fixture(&pool).await?;
    let connection = store
        .load_connection(predecessor.enrollment())
        .await?
        .unwrap();
    store
        .transition_connection(
            predecessor.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: pin.placement.session(),
        revision: None,
    };
    let result = store.replace_lost_runner(command.clone()).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Replaced {
            runner: candidate.identities().runner(),
            placement_revision: pin.placement.revision().checked_next().unwrap()
        })
    );
    assert_eq!(store.replace_lost_runner(command.clone()).await?, result);
    assert_eq!(
        store.resume_runner_replacement(command.command_id).await?,
        result
    );
    let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM semantic_transcript_entry WHERE payload_kind = 'runner_placement_changed'").fetch_one(&pool).await?;
    assert_eq!(entries, 1);
    let member_count: Decimal = sqlx::query_scalar("SELECT frontier.member_count FROM runner_session_placement_frontier AS head JOIN runner_placement_boundary AS boundary USING (session_id, placement_revision) JOIN context_frontier AS frontier ON frontier.owning_session_id = boundary.session_id AND frontier.context_frontier_id = boundary.context_frontier_id WHERE head.session_id = $1").bind(command.session.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(member_count, Decimal::ONE);
    assert!(
        matches!(store.load_placement(command.session).await?.unwrap().placement().state(), SessionRunnerPlacementState::Pinned(pinned) if pinned.runner == candidate.identities().runner())
    );
    Ok(())
}
