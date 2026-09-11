//! Connection epoch coverage.

use super::*;

/// revoking an enrollment with a live physical connection advances
/// the exact loss epoch in the same transaction as terminalization.
#[tokio::test]
#[ignore = "requires Docker"]
async fn revocation_advances_live_connection_loss_epoch() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let mut expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let live_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    assert!(store.revoke_enrollment(&mut expected_enrollment).await?);
    let revoked_connection = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("revocation retains its terminal connection source");
    let revocation_loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("revocation advances the connection loss epoch");

    assert_eq!(revoked_connection.epoch(), live_connection.epoch());
    assert_eq!(revoked_connection.state(), RunnerConnectionState::Lost);
    assert_eq!(
        revoked_connection.cause(),
        RunnerConnectionCause::EnrollmentRevoked
    );
    assert_eq!(
        revocation_loss.connection_epoch(),
        revoked_connection.epoch()
    );
    assert_eq!(
        revocation_loss.connection_event_ordinal(),
        revoked_connection.event_ordinal()
    );
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn checked_runner_replacement_requires_a_live_successor_connection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let successor_registration = store.register(&successor, advertisement()).await?;
    let connection = store.open_connection(successor.enrollment()).await?;
    store
        .transition_connection(
            successor.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the replacement directory is valid"),
            None,
            pin.grant,
        )
        .expect("the caller-held registration can prepare a replacement");
    let rejected = store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            replacement.grant.as_ref(),
        )
        .await
        .expect_err("a disconnected successor cannot install replacement authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn checked_runner_replacement_rejects_a_successor_without_a_connection()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let successor_registration = store.register(&successor, advertisement()).await?;
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the replacement directory is valid"),
            None,
            pin.grant,
        )
        .expect("the caller-held registration can prepare a replacement");
    let rejected = store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            replacement.grant.as_ref(),
        )
        .await
        .expect_err("a successor without a connection cannot install replacement authority");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}

/// connection loss retains the different-runner replacement rule.
#[tokio::test]
#[ignore = "requires Docker"]
async fn connection_loss_rejects_same_runner_replacement_shape() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    let mut malformed = pool.begin().await?;
    let rejected =
        append_same_runner_replacement_projection(&mut malformed, pin.placement.session(), None)
            .await
            .expect_err("only registration loss may retain the runner identity");

    assert_check_violation(rejected);
    drop(malformed);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn transcript_snapshot_authenticates_current_runner_suspicion() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
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
    let snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(pin.placement.session())
        .await?
        .expect("the pinned fixture session has a transcript snapshot");
    let projection = snapshot
        .runner()
        .expect("the runner-placed session projects its current placement");

    assert_eq!(projection.state(), ProcessRunnerProjectionState::Pinned);
    assert_eq!(projection.runner(), Some(pin.lease.runner()));
    assert_eq!(
        projection.connection_health(),
        Some(ProcessRunnerConnectionHealth::Suspect)
    );
    drop(pool);
    Ok(())
}

/// each terminal physical connection advances one enrollment-owned
/// append-only loss epoch with its exact connection source.
#[tokio::test]
#[ignore = "requires Docker"]
async fn terminal_connections_advance_exact_loss_epochs() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let first_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            first_connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let first_terminal = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the first terminal connection remains durable");
    let first_loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the first terminal connection advances a loss epoch");
    let second_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            second_connection.epoch(),
            RunnerConnectionTransition::HeartbeatTimeout,
        )
        .await?;
    let second_terminal = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the successor terminal connection remains durable");
    let second_loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the successor terminal connection advances the loss epoch");

    assert_eq!(first_loss.loss_epoch().get(), 1);
    assert_eq!(first_loss.connection_epoch(), first_terminal.epoch());
    assert_eq!(
        first_loss.connection_event_ordinal(),
        first_terminal.event_ordinal()
    );
    assert_eq!(second_loss.loss_epoch().get(), 2);
    assert_eq!(second_loss.connection_epoch(), second_terminal.epoch());
    assert_eq!(
        second_loss.connection_event_ordinal(),
        second_terminal.event_ordinal()
    );
    drop(pool);
    Ok(())
}

/// failure to advance the durable loss epoch rolls the terminal
/// connection event back at the same commit boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn loss_epoch_failure_rolls_back_terminal_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    sqlx::query(
        "CREATE FUNCTION reject_runner_loss_epoch_for_test()
         RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'injected runner loss epoch refusal'
                 USING ERRCODE = '23514';
         END;
         $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_runner_loss_epoch_for_test
         BEFORE INSERT ON runner_connection_loss_epoch
         FOR EACH ROW EXECUTE FUNCTION reject_runner_loss_epoch_for_test()",
    )
    .execute(&pool)
    .await?;
    let rejected = store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await
        .expect_err("terminal connection and loss epoch share one commit boundary");
    sqlx::query(
        "DROP TRIGGER reject_runner_loss_epoch_for_test
         ON runner_connection_loss_epoch",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP FUNCTION reject_runner_loss_epoch_for_test()")
        .execute(&pool)
        .await?;
    let retained = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the established connection remains current");
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?;

    assert_store_check_violation(rejected);
    assert_eq!(retained, connection);
    assert_eq!(loss, None);
    drop(pool);
    Ok(())
}

/// a loss epoch may name only its exact terminal connection source.
#[tokio::test]
#[ignore = "requires Docker"]
async fn loss_epoch_rejects_connected_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let rejected = sqlx::query(
        "INSERT INTO runner_connection_loss_epoch
            (enrollment_id, loss_epoch, connection_epoch,
             connection_event_ordinal)
         VALUES ($1, 1, $2, $3)",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(connection.epoch().get()))
    .bind(Decimal::from(connection.event_ordinal()))
    .execute(&pool)
    .await
    .expect_err("a live connection cannot mint a terminal loss fence");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a terminal connection fences the placement's later lease
/// offers even after the enrollment opens a successor physical connection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn loss_fences_placement_across_successor_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let rejected = store
        .store_lease(&lease)
        .await
        .expect_err("a terminal connection cannot authorize a later lease offer");
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let reconnect_rejected = store
        .store_lease(&lease)
        .await
        .expect_err("reconnect cannot erase the placement's observed loss fence");
    let loaded = store
        .load_lease(lease.correlation().lease, lease.correlation().generation)
        .await?;

    assert_store_check_violation(rejected);
    assert_store_check_violation(reconnect_rejected);
    assert_eq!(loaded, None);
    drop(pool);
    Ok(())
}

/// an exact runner selected before connection loss cannot
/// be pinned after reconnect without an explicit placement replacement.
#[tokio::test]
#[ignore = "requires Docker"]
async fn exact_selection_loss_rejects_post_reconnect_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let expected_unpinned_state = placement.state().clone();
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the domain pin retains the pre-loss exact selection");
    let rejected = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("the adapter rejects a pin whose exact selection predates loss");
    let loaded = store
        .load_placement(session)
        .await?
        .expect("the unpinned selection remains current");

    assert_store_check_violation(rejected);
    assert_eq!(loaded.placement().request(), pin.placement.request());
    assert_eq!(loaded.placement().revision(), pin.placement.revision());
    assert_eq!(loaded.placement().state(), &expected_unpinned_state);
    drop(pool);
    Ok(())
}

/// an exact selection created after reconnect observes the
/// prior loss epoch and may pin on the live successor connection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn post_reconnect_selection_pins_with_fresh_loss_baseline() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection owns its durable loss epoch");
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the post-reconnect exact selection can be pinned");
    store.store_pin(&pin, &registration).await?;
    let baseline: (Uuid, Decimal) = sqlx::query_as(
        "SELECT loss_fence_enrollment_id, observed_runner_loss_epoch
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;
    let loaded = store
        .load_lease(
            pin.lease.correlation().lease,
            pin.lease.correlation().generation,
        )
        .await?
        .expect("the fresh-baseline lease is durable");

    assert_eq!(baseline.0, expected_enrollment.enrollment().into_uuid());
    assert_eq!(baseline.1, Decimal::from(loss.loss_epoch().get()));
    assert_eq!(loaded, pin.lease);
    drop(pool);
    Ok(())
}

/// placement pin takes the scheduler before
/// runner authority, so a loss that commits while pin waits is rechecked and
/// rejects the stale exact selection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn connection_loss_serializes_exact_selection_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let expected_unpinned_state = placement.state().clone();
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            exact_runner_directory(),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the exact selection prepares its initial pin");
    let mut scheduler = pool.begin().await?;
    sqlx::query(
        "SELECT session_id
           FROM session_scheduler
          WHERE session_id = $1
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .fetch_one(&mut *scheduler)
    .await?;
    let mut pin_store = Box::pin(store.store_pin(&pin, &registration));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut pin_store)
        .await
        .expect_err("pin waits at the scheduler before runner authority");
    tokio::time::timeout(
        LOCK_COMPLETION_TIMEOUT,
        store.transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        ),
    )
    .await
    .expect("connection loss does not wait behind the session scheduler")?;
    scheduler.commit().await?;
    let rejected = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, pin_store)
        .await
        .expect("pin resumes after the scheduler lock is released")
        .expect_err("the resumed pin observes the committed loss baseline");
    let loaded = store
        .load_placement(session)
        .await?
        .expect("the exact selection remains unpinned after loss wins");
    let lost_connection = store
        .load_connection(expected_enrollment.enrollment())
        .await?
        .expect("the terminal connection remains the durable head");

    assert_eq!(lost_connection.state(), RunnerConnectionState::Lost);
    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    assert_eq!(loaded.placement().state(), &expected_unpinned_state);
    drop(pool);
    Ok(())
}

/// clean shutdown is terminal for its exact connection
/// epoch and cannot strand a newly offered lease behind unusable authority.
#[tokio::test]
#[ignore = "requires Docker"]
async fn shutdown_connection_rejects_later_lease_offer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::DaemonShutdown,
        )
        .await?;
    let rejected = store
        .store_lease(&lease)
        .await
        .expect_err("a cleanly shut down connection cannot authorize a lease offer");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// once a terminal transition owns enrollment authority,
/// a concurrent lease offer observes the committed loss fence and is refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn connection_loss_wins_concurrent_lease_offer() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let mut loss_store = Box::pin(store.transition_connection(
        enrollment,
        connection.epoch(),
        RunnerConnectionTransition::TransportClosed,
    ));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut loss_store)
        .await
        .expect_err("the terminal transition waits on connection authority");
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("the lease offer waits behind terminal enrollment authority");
    authority.commit().await?;
    loss_store.await?;
    let rejected = lease_store
        .await
        .expect_err("the later lease offer observes the committed loss fence");

    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a lease offer that already owns enrollment authority
/// commits before a racing terminal transition installs the loss fence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn lease_offer_wins_concurrent_connection_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _, lease) = stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let mut lease_store = Box::pin(store.store_lease(&lease));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut lease_store)
        .await
        .expect_err("the lease offer waits on connection authority");
    let mut loss_store = Box::pin(store.transition_connection(
        enrollment,
        connection.epoch(),
        RunnerConnectionTransition::TransportClosed,
    ));
    tokio::time::timeout(LOCK_WAIT_PROBE, &mut loss_store)
        .await
        .expect_err("the terminal transition waits behind enrollment authority");
    authority.commit().await?;
    lease_store.await?;
    loss_store.await?;
    let loaded = store
        .load_lease(lease.correlation().lease, lease.correlation().generation)
        .await?
        .expect("the earlier lease offer remains durable");
    let loss = store
        .load_current_connection_loss(enrollment)
        .await?
        .expect("the later terminal transition advances the loss fence");

    assert_eq!(loaded, lease);
    assert_eq!(loss.connection_epoch(), connection.epoch());
    drop(pool);
    Ok(())
}

/// a claim retains the exact connection/loss baseline that
/// authorized its offer, so neither terminal loss nor a successor connection
/// can revive the stale execution capability.
#[tokio::test]
#[ignore = "requires Docker"]
async fn loss_fences_offered_lease_claim_across_reconnect() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _, lease) =
        stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact offered lease correlation prepares its claim");
    store
        .transition_connection(
            enrollment,
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let lost_rejection = store
        .store_lease(&claimed)
        .await
        .expect_err("terminal loss fences the outstanding lease claim");
    store.open_connection(enrollment).await?;
    let successor_rejection = store
        .store_lease(&claimed)
        .await
        .expect_err("a successor connection cannot revive the prior offer");

    assert_store_check_violation(lost_rejection);
    assert_store_check_violation(successor_rejection);
    drop(pool);
    Ok(())
}

/// terminal loss that reaches connection authority first
/// fences a concurrently queued claim before execution capability is issued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn connection_loss_wins_concurrent_lease_claim() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _, lease) =
        stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact offered lease correlation prepares its claim");
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let loss_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let loss_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            loss_store.transition_connection(
                enrollment,
                connection.epoch(),
                RunnerConnectionTransition::TransportClosed,
            ),
        )
        .await
    });
    let loss_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1)).await;
    let claim_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let claim_task = tokio::spawn(async move {
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, claim_store.store_lease(&claimed)).await
    });
    let claim_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 2)).await;
    let authority_commit = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, authority.commit()).await;
    let loss_result = loss_task.await;
    let claim_result = claim_task.await;
    let loss_blocked = loss_observation.expect("loss lock observation must remain bounded")?;
    let claim_blocked = claim_observation.expect("claim lock observation must remain bounded")?;
    authority_commit.expect("connection-authority blocker commit must remain bounded")?;
    loss_result
        .expect("loss task must remain joinable")
        .expect("loss must finish within its task-owned timeout")?;
    let rejected = claim_result
        .expect("claim task must remain joinable")
        .expect("claim must finish within its task-owned timeout")
        .expect_err("the claim observes the loss that won authority");

    assert!(loss_blocked, "loss must reach connection authority");
    assert!(claim_blocked, "claim must queue behind terminal loss");
    assert_store_check_violation(rejected);
    drop(pool);
    Ok(())
}

/// a claim that reaches connection authority first commits
/// before a racing loss and remains the durable execution-capability boundary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn lease_claim_wins_concurrent_connection_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, registration, _, lease) =
        stored_later_lease_fixture(&pool).await?;
    let enrollment = expected_enrollment.enrollment();
    let connection = store.open_connection(enrollment).await?;
    store.store_lease(&lease).await?;
    let claimed = duplicate_lease(&lease, registration.registration())
        .claim(lease.correlation())
        .expect("the exact offered lease correlation prepares its claim");
    let expected_state = claimed.state();
    let lease_id = claimed.correlation().lease;
    let generation = claimed.correlation().generation;
    let mut authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(enrollment.into_uuid())
    .fetch_one(&mut *authority)
    .await?;
    let claim_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let claim_task = tokio::spawn(async move {
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, claim_store.store_lease(&claimed)).await
    });
    let claim_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1)).await;
    let loss_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let loss_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            loss_store.transition_connection(
                enrollment,
                connection.epoch(),
                RunnerConnectionTransition::TransportClosed,
            ),
        )
        .await
    });
    let loss_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 2)).await;
    let authority_commit = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, authority.commit()).await;
    let claim_result = claim_task.await;
    let loss_result = loss_task.await;
    let claim_blocked = claim_observation.expect("claim lock observation must remain bounded")?;
    let loss_blocked = loss_observation.expect("loss lock observation must remain bounded")?;
    authority_commit.expect("connection-authority blocker commit must remain bounded")?;
    claim_result
        .expect("claim task must remain joinable")
        .expect("claim must finish within its task-owned timeout")?;
    loss_result
        .expect("loss task must remain joinable")
        .expect("loss must finish within its task-owned timeout")?;
    let retained = store
        .load_lease(lease_id, generation)
        .await?
        .expect("the winning claim remains current after later loss");

    assert!(claim_blocked, "claim must reach connection authority");
    assert!(loss_blocked, "loss must queue behind the claim");
    assert_eq!(retained.state(), expected_state);
    drop(pool);
    Ok(())
}

/// initial pin rechecks connection health under enrollment authority
/// and cannot commit after a concurrent first-heartbeat suspicion.
#[tokio::test]
#[ignore = "requires Docker"]
async fn initial_pin_rejects_suspect_connection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let registration = store
        .register(&expected_enrollment, advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    store.store_placement(&placement, None, None).await?;
    let expected_state = placement.state().clone();
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the validated registration prepares the initial pin");
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
    let rejected = store
        .store_pin(&pin, &registration)
        .await
        .expect_err("a suspect connection cannot authorize initial pin");
    let retained = store
        .load_placement(pin.placement.session())
        .await?
        .expect("the rejected pin retains the unpinned placement");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    assert_eq!(retained.placement().state(), &expected_state);
    drop(pool);
    Ok(())
}

/// pin and heartbeat publication serialize on enrollment
/// authority, so neither can observe a split connection/placement state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn initial_pin_serializes_with_heartbeat_suspicion() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let setup_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    setup_store.insert_enrollment(&expected_enrollment).await?;
    let registration = setup_store
        .register(&expected_enrollment, advertisement())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        SessionRunnerPlacementRequest {
            selector: RunnerSelector::CapabilityClass(class()),
            working_directory: WorkingDirectorySelection::RunnerDefault,
            credential_profile: Some(profile()),
            workspace: WorkspaceRequirement::None,
            sandbox: RunnerSandboxProfile::Ambient,
            permission_overrides: no_permission_overrides(),
        },
    );
    setup_store.store_placement(&placement, None, None).await?;
    let pin = placement
        .pin_and_offer_lease(
            &expected_enrollment,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture working directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the validated registration prepares the initial pin");
    let expected_session = pin.placement.session();
    let expected_state = pin.placement.state().clone();
    let connection = setup_store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let mut blocker = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_enrollment
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .fetch_one(&mut *blocker)
    .await?;
    let pin_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let pin_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            pin_store.store_pin(&pin, &registration),
        )
        .await
    });
    let pin_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1)).await;
    let heartbeat_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let enrollment_id = expected_enrollment.enrollment();
    let heartbeat_task = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            heartbeat_store.transition_connection(
                enrollment_id,
                connection.epoch(),
                RunnerConnectionTransition::HeartbeatMissed,
            ),
        )
        .await
    });
    let heartbeat_observation =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 2)).await;
    let blocker_commit = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocker.commit()).await;
    let pin_result = pin_task.await;
    let heartbeat_result = heartbeat_task.await;
    let pin_blocked = pin_observation.expect("pin lock observation must remain bounded")?;
    let heartbeat_blocked =
        heartbeat_observation.expect("heartbeat lock observation must remain bounded")?;
    blocker_commit.expect("enrollment blocker commit must remain bounded")?;
    pin_result
        .expect("pin task must remain joinable")
        .expect("pin must finish within its task-owned timeout")?;
    heartbeat_result
        .expect("heartbeat task must remain joinable")
        .expect("heartbeat must finish within its task-owned timeout")?;
    let retained = setup_store
        .load_placement(expected_session)
        .await?
        .expect("the serialized pin remains current");
    let runner_event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_state_transition_outbox_event")
            .fetch_one(&pool)
            .await?;

    assert!(pin_blocked, "pin must reach enrollment authority");
    assert!(heartbeat_blocked, "heartbeat must queue behind the pin");
    assert_eq!(retained.placement().state(), &expected_state);
    assert_eq!(runner_event_count, 1);
    drop(pool);
    Ok(())
}

/// a new connected epoch that supersedes durable suspicion
/// publishes recovery from the new epoch for every affected pinned session.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_reconnect_after_suspicion_publishes_connected() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin) = stored_pin_fixture(&pool).await?;
    let session = pin.placement.session();
    let (_, placement_revision) = placement_outbox_facts(&pool, session, "pinned").await?;
    let first_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            first_connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    let suspect = dispatch_next_outbox_event(&pool).await?;
    let replacement_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let connected = dispatch_next_outbox_event_at(&pool, 2).await?;

    assert_eq!(suspect.sequence(), 1);
    assert_eq!(connected.sequence(), 2);
    assert_eq!(connected.session(), Some(session));
    assert_eq!(replacement_connection.event_ordinal(), 1);
    assert_eq!(
        connected.kind(),
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

/// dispatch rechecks the immutable predecessor chain so
/// post-admission corruption cannot turn an established epoch into recovery.
#[tokio::test]
#[ignore = "requires Docker"]
async fn reconnect_dispatch_quarantines_corrupted_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, _) = stored_pin_fixture(&pool).await?;
    let first_connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            first_connection.epoch(),
            RunnerConnectionTransition::HeartbeatMissed,
        )
        .await?;
    dispatch_next_outbox_event(&pool).await?;
    store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    sqlx::query("ALTER TABLE runner_connection_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_connection_event
            SET state_kind = 'connected',
                cause_kind = 'heartbeat_recovered'
          WHERE enrollment_id = $1
            AND connection_epoch = $2
            AND event_ordinal = 2",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .bind(Decimal::from(first_connection.epoch().get()))
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_connection_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    assert_next_outbox_event_quarantined(&pool, OutboxRowCorruption::InvalidRunnerEvent).await?;
    drop(pool);
    Ok(())
}
