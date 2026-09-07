//! Outbox coverage.

use super::*;

pub(crate) fn replacement_runner_directory() -> RunnerWorkingDirectory {
    RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
        .expect("the replacement fixture directory is valid")
}

pub(crate) async fn store_additional_credentialless_pin_fixture(
    pool: &PgPool,
    store: &RunnerProtocolStore,
    expected_enrollment: &RunnerEnrollment,
    registration: &StoredValidatedRunnerRegistration,
) -> Result<SessionRunnerPin, Box<dyn Error>> {
    let session = SessionId::from_uuid(uuid(SECOND_SESSION));
    insert_session_for(pool, session.into_uuid()).await?;
    insert_physical_attempt_for(pool, session, SECOND_SESSION_PHYSICAL_ATTEMPT).await?;
    let placement = SessionRunnerPlacement::new(
        session,
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: None,
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/second-session".to_owned())
                .expect("the second fixture working directory is valid"),
            None,
            authorized_for_session(session, SECOND_SESSION_PHYSICAL_ATTEMPT),
            offer_request_for(LEASE + RELATED_IDENTITY_OFFSET),
        )
        .expect("the second fixture registration pins the placement");
    store.store_pin(&pin, registration).await?;
    Ok(pin)
}

/// initial pin dispatches from its immutable placement
/// record with the complete follower-visible runner facts.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_pinned_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Pinned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Pinned,
        }
    );
    drop(pool);
    Ok(())
}

/// first-heartbeat suspicion dispatches only from its
/// exact connection event and retained pinned placement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_suspect_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (_, placement_revision) = placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let outbox_event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_event")
        .fetch_one(&pool)
        .await?;
    let runner_event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_state_transition_outbox_event")
            .fetch_one(&pool)
            .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(outbox_event_count, 1);
    assert_eq!(runner_event_count, 1);
    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Suspect,
        }
    );
    drop(pool);
    Ok(())
}

/// one connection-health transition publishes one event
/// for every session pinned to the affected enrollment.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_suspect_outbox_covers_every_pinned_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, first_pin) =
        stored_credentialless_pin_fixture(&pool).await?;
    let second_pin = store_additional_credentialless_pin_fixture(
        &pool,
        &store,
        &expected_enrollment,
        &registration,
    )
    .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let event_sessions: Vec<Uuid> = sqlx::query_scalar(
        "SELECT session_id
           FROM runner_state_transition_outbox_event
          WHERE state_kind = 'suspect'
          ORDER BY session_id",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(event_sessions.len(), 2);
    assert_eq!(event_sessions[0], first_pin.placement.session().into_uuid());
    assert_eq!(
        event_sessions[1],
        second_pin.placement.session().into_uuid()
    );
    drop(pool);
    Ok(())
}

/// a follower-event refusal rolls the exact connection
/// transition back rather than leaving durable health without its update.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_suspect_outbox_failure_rolls_back_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _) = stored_pin_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    sqlx::query(
        "CREATE FUNCTION reject_runner_health_outbox_for_test()
         RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'injected runner outbox refusal'
                 USING ERRCODE = '23514';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_runner_health_outbox_for_test
         BEFORE INSERT ON runner_state_transition_outbox_event
         FOR EACH ROW EXECUTE FUNCTION reject_runner_health_outbox_for_test()",
    )
    .execute(&pool)
    .await?;
    let rejected = store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await
        .expect_err("connection health and its follower event share one commit boundary");
    sqlx::query(
        "DROP TRIGGER reject_runner_health_outbox_for_test
         ON runner_state_transition_outbox_event",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION reject_runner_health_outbox_for_test()")
        .execute(&pool)
        .await?;
    let retained = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the established connection remains current");
    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_event")
        .fetch_one(&pool)
        .await?;

    assert_store_check_violation(rejected);
    assert_eq!(retained.state(), connection.state());
    assert_eq!(retained.event_ordinal(), connection.event_ordinal());
    assert_eq!(event_count, 0);
    drop(pool);
    Ok(())
}

/// dispatch rejects a non-connection state that retains
/// connection provenance after post-admission corruption.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_outbox_dispatch_rejects_pinned_connection_provenance() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _) = stored_pin_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    sqlx::query(
        "ALTER TABLE runner_state_transition_outbox_event
            DROP CONSTRAINT runner_state_transition_outbox_source_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_state_transition_outbox_event
            SET state_kind = 'pinned'
          WHERE event_sequence = 1",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
        .await
        .expect_err("a pinned event cannot retain connection provenance");

    assert!(matches!(
        rejected,
        OutboxDispatchError::Corruption(OutboxCorruption::InvalidRunnerEvent)
    ));
    drop(pool);
    Ok(())
}

/// heartbeat recovery dispatches from the exact recovered
/// connection event rather than the mutable current connection head.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_connected_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (_, placement_revision) = placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let suspect = dispatch_next_outbox_event(&pool).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatRecovered,
        )
        .await?;
    let event = dispatch_next_outbox_event_at(&pool, 2).await?;

    assert_eq!(suspect.sequence(), 1);
    assert_eq!(event.sequence(), 2);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Connected,
        }
    );
    drop(pool);
    Ok(())
}

/// connection-state publication must name the enrollment's
/// latest durable connection event at insertion time.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_outbox_rejects_superseded_connection_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let source = connection_outbox_source(
        &pool,
        placement_event_ordinal,
        expected_enrollment.enrollment(),
        "heartbeat_missed",
    )
    .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatRecovered,
        )
        .await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Suspect,
            source,
        ),
    )
    .await
    .expect_err("a superseded connection event cannot publish current suspicion");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// connection-state publication is bound to the session's
/// current placement rather than a historical placement for the same runner.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_outbox_rejects_historical_connection_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let source = connection_outbox_source(
        &pool,
        placement_event_ordinal,
        expected_enrollment.enrollment(),
        "heartbeat_missed",
    )
    .await?;
    append_runner_registration_loss_projection(&pool, session).await?;
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(&mut replacement, session, None).await?;
    replacement.commit().await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Suspect,
            source,
        ),
    )
    .await
    .expect_err("a historical placement cannot publish current connection state");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// exact-identity loss before pin dispatches the retained
/// requested sandbox and user-selected directory.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_lost_before_pin_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let expected_directory = exact_runner_directory();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request_with_directory(runner, expected_directory.clone()),
    );
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, placement.session(), "runner_lost_before_pin").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            placement.session(),
            runner,
            placement_revision,
            placement.request().sandbox,
            Some(expected_directory.clone()),
            DispatchedRunnerState::RunnerLostBeforePin,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(placement.session()));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner,
            placement_revision,
            sandbox: placement.request().sandbox,
            working_directory: Some(expected_directory),
            state: DispatchedRunnerState::RunnerLostBeforePin,
        }
    );
    drop(pool);
    Ok(())
}

/// pinned loss dispatches against the historical loss
/// record even after the placement head can later advance.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_lost_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_lost_projection(&pool, session).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_lost").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::RunnerLost,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    append_abandoned_projection(&pool, session, None).await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::RunnerLost,
        }
    );
    drop(pool);
    Ok(())
}

/// pre-pin user replacement dispatches the successor
/// identity and successor placement revision without fabricating pinned facts.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_pre_pin_replaced_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let successor = RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER));
    let expected_directory = exact_runner_directory();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request_with_directory(runner, expected_directory.clone()),
    );
    RunnerProtocolStore::new(pool.clone(), catalog())
        .store_placement(&placement, None, None)
        .await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_pre_pin_replacement_projection(&pool, placement.session(), successor).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, placement.session(), "pre_pin_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            placement.session(),
            successor,
            placement_revision,
            placement.request().sandbox,
            Some(expected_directory.clone()),
            DispatchedRunnerState::Replaced,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(placement.session()));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: successor,
            placement_revision,
            sandbox: placement.request().sandbox,
            working_directory: Some(expected_directory),
            state: DispatchedRunnerState::Replaced,
        }
    );
    drop(pool);
    Ok(())
}

/// checked pinned replacement dispatches from the exact
/// successor placement record without requiring a directory relocation.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_pinned_replaced_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(&mut replacement, session, None).await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Replaced,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Replaced,
        }
    );
    drop(pool);
    Ok(())
}

/// checked same-runner recovery with a new user-selected
/// directory dispatches the relocation state and exact requested directory.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_working_directory_changed_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let replacement_directory = replacement_runner_directory();
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(
        &mut replacement,
        session,
        Some(&replacement_directory),
    )
    .await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            Some(replacement_directory.clone()),
            DispatchedRunnerState::WorkingDirectoryChanged,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: Some(replacement_directory),
            state: DispatchedRunnerState::WorkingDirectoryChanged,
        }
    );
    drop(pool);
    Ok(())
}

/// a same-runner directory relocation has exactly one
/// follower state and cannot also masquerade as an ordinary replacement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_directory_relocation_rejects_replaced_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let replacement_directory = replacement_runner_directory();
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(
        &mut replacement,
        session,
        Some(&replacement_directory),
    )
    .await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            Some(replacement_directory),
            DispatchedRunnerState::Replaced,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await
    .expect_err("a directory relocation cannot publish an ordinary replacement state");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// dispatch repeats relocation exclusivity checks so
/// post-admission state corruption cannot publish an ordinary replacement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_outbox_dispatch_rejects_corrupted_relocation_state() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_credentialless_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_registration_loss_projection(&pool, session).await?;
    let replacement_directory = replacement_runner_directory();
    let mut replacement = pool.begin().await?;
    append_same_runner_replacement_projection(
        &mut replacement,
        session,
        Some(&replacement_directory),
    )
    .await?;
    replacement.commit().await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "runner_replaced").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            Some(replacement_directory),
            DispatchedRunnerState::WorkingDirectoryChanged,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_state_transition_outbox_event
            SET state_kind = 'replaced'
          WHERE event_sequence = 1",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
        .await
        .expect_err("a corrupted relocation state cannot be offered");

    assert!(matches!(
        rejected,
        OutboxDispatchError::Corruption(OutboxCorruption::InvalidRunnerEvent)
    ));
    drop(pool);
    Ok(())
}

/// abandonment dispatches from its exact terminal
/// placement record while retaining the lost runner identity.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_abandoned_outbox_round_trips() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    append_runner_lost_projection(&pool, session).await?;
    append_abandoned_projection(&pool, session, None).await?;
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "abandoned").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Abandoned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(event.sequence(), 1);
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: pin.lease.runner(),
            placement_revision,
            sandbox: pin.placement.request().sandbox,
            working_directory: None,
            state: DispatchedRunnerState::Abandoned,
        }
    );
    drop(pool);
    Ok(())
}

/// dispatch revalidates the immutable placement source and
/// rejects a runner event whose stored runner was corrupted after admission.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_outbox_dispatch_rejects_cross_wired_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            pin.lease.runner(),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Pinned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_state_transition_outbox_event
            SET runner_id = $1
          WHERE event_sequence = 1",
    )
    .bind(uuid(FOREIGN_RUNNER))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_state_transition_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let rejected = OutboxDispatcher::new(pool.clone())
        .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
        .await
        .expect_err("a cross-wired runner event cannot be offered");

    assert!(matches!(
        rejected,
        OutboxDispatchError::Corruption(OutboxCorruption::InvalidRunnerEvent)
    ));
    drop(pool);
    Ok(())
}

/// the relational outbox guard refuses a transition whose
/// runner identity disagrees with its immutable placement source.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_outbox_insert_rejects_cross_wired_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (placement_event_ordinal, placement_revision) =
        placement_outbox_facts(&pool, session, "pinned").await?;
    let rejected = append_runner_state_transition_for_test(
        &pool,
        RunnerStateTransitionOutboxTestEvent::new(
            session,
            RunnerId::from_uuid(uuid(FOREIGN_RUNNER)),
            placement_revision,
            pin.placement.request().sandbox,
            None,
            DispatchedRunnerState::Pinned,
            RunnerStateTransitionOutboxTestSource::placement(placement_event_ordinal),
        ),
    )
    .await
    .expect_err("a cross-wired runner event cannot commit");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}
