//! Loss propagation coverage.

use super::*;

pub(crate) fn propagation_session(ordinal: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(
        0xa200_0000_0000_0000_0000_0000_0000_0000 + ordinal,
    ))
}

pub(crate) async fn insert_uncommitted_exact_placement(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
    runner: RunnerId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_tool_count)
         VALUES ($1, 1, 1, 'created', 'identity', $2, 'runner_default',
                 'none', 'workspace_restricted', 0, 'unpinned', 0)",
    )
    .bind(session.into_uuid())
    .bind(runner.into_uuid())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_session_placement
            (session_id, event_ordinal)
         VALUES ($1, 1)",
    )
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn insert_bounded_propagation_session_fixture(
    pool: &PgPool,
    runner: RunnerId,
) -> Result<Vec<SessionId>, sqlx::Error> {
    let sessions: Vec<_> = (1..=65).map(propagation_session).collect();
    let session_uuids: Vec<_> = sessions.iter().copied().map(SessionId::into_uuid).collect();
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE session DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         SELECT session_id, 'interactive', 'none'
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         SELECT session_id, 'created', false, false, 'operator'
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         SELECT session_id, 1, 'created_unmonitored', false, 'operator'
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE session ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         SELECT session_id FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_tool_count)
         SELECT session_id, 1, 1, 'created', 'identity', $2,
                'runner_default', 'none', 'workspace_restricted', 0,
                'unpinned', 0
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .bind(runner.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_session_placement
            (session_id, event_ordinal)
         SELECT session_id, 1
           FROM unnest($1::uuid[]) AS fixture(session_id)",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(sessions)
}

pub(crate) async fn project_bounded_propagation_sessions(
    pool: &PgPool,
    sessions: &[SessionId],
) -> Result<(), sqlx::Error> {
    let session_uuids: Vec<_> = sessions.iter().copied().map(SessionId::into_uuid).collect();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision)
         SELECT placement.session_id, placement.event_ordinal + 1,
                placement.placement_revision, 'runner_lost_before_pin',
                placement.selector_kind, placement.selector_runner_id,
                placement.selector_capability_class,
                placement.directory_selection_kind,
                placement.requested_working_directory,
                placement.requested_credential_profile_name,
                placement.workspace_requirement_kind,
                placement.requested_repository_key,
                placement.requested_sandbox_profile,
                placement.permission_override_count,
                'runner_lost_before_pin', placement.selector_runner_id,
                NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL,
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                NULL
           FROM runner_session_placement_record AS placement
           JOIN runner_current_session_placement AS current_placement
             ON current_placement.session_id = placement.session_id
            AND current_placement.event_ordinal = placement.event_ordinal
          WHERE placement.session_id = ANY($1::uuid[])",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = event_ordinal + 1
          WHERE session_id = ANY($1::uuid[])",
    )
    .bind(&session_uuids)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

/// a new loss owns a pending cursor whose ordered read page is capped
/// at 64 sessions and resumes strictly after its durable session identity.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_cursor_pages_sixty_four_sessions() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
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
        .expect("the terminal connection owns its pending propagation cursor");
    let first_page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(first_page.loss(), loss);
    assert_eq!(first_page.propagated_through(), None);
    assert_eq!(first_page.sessions(), &expected_sessions[..64]);
    assert!(!first_page.is_complete());

    project_bounded_propagation_sessions(&pool, &expected_sessions[..64]).await?;
    sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[63].into_uuid())
    .execute(&pool)
    .await?;
    let second_page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(second_page.loss(), loss);
    assert_eq!(
        second_page.propagated_through(),
        Some(expected_sessions[63])
    );
    assert_eq!(second_page.sessions(), &expected_sessions[64..]);
    assert!(!second_page.is_complete());

    drop(pool);
    Ok(())
}

/// bounded propagation pages have indexes for both enrollment-fenced
/// and pre-enrollment exact-runner placement branches.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_page_has_affected_set_indexes() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let definition: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'runner_session_placement_loss_propagation_page'",
    )
    .fetch_one(&pool)
    .await?;
    let exact_definition: String = sqlx::query_scalar(
        "SELECT indexdef
           FROM pg_indexes
          WHERE schemaname = current_schema()
            AND indexname = 'runner_session_placement_exact_loss_propagation_page'",
    )
    .fetch_one(&pool)
    .await?;

    assert!(
        definition.contains("(loss_fence_enrollment_id, session_id, event_ordinal)"),
        "the loss page index must lead with enrollment and preserve session order"
    );
    assert!(
        exact_definition.contains("(selector_runner_id, session_id, event_ordinal)"),
        "the exact-selection page index must lead with runner and preserve session order"
    );
    drop(pool);
    Ok(())
}

/// a propagation cursor cannot advance past an affected session that
/// has not received the loss projection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_cursor_rejects_skipped_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
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
        .expect("the terminal connection owns its pending propagation cursor");

    let skipped = sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[63].into_uuid())
    .execute(&pool)
    .await
    .expect_err("a durable cursor cannot skip an affected session");

    assert_check_violation(skipped);
    drop(pool);
    Ok(())
}

/// a propagation cursor cannot rewind behind its durable session.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_cursor_rejects_rewind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
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
        .expect("the terminal connection owns its pending propagation cursor");
    project_bounded_propagation_sessions(&pool, &expected_sessions[..64]).await?;
    sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[63].into_uuid())
    .execute(&pool)
    .await?;

    let rewound = sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(expected_sessions[62].into_uuid())
    .execute(&pool)
    .await
    .expect_err("a durable cursor cannot rewind its session identity");

    assert_check_violation(rewound);
    drop(pool);
    Ok(())
}

/// a propagation cursor cannot complete while an affected session
/// still retains an older loss baseline.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_cursor_rejects_premature_completion() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    store.store_placement(&placement, None, None).await?;
    store.insert_enrollment(&expected_enrollment).await?;
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
        .expect("the terminal connection owns its pending propagation cursor");

    let premature_completion = sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET state_kind = 'completed'
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .execute(&pool)
    .await
    .expect_err("a durable cursor cannot complete before its final session");

    assert_check_violation(premature_completion);
    drop(pool);
    Ok(())
}

/// an exact-identity placement that observes enrollment absence
/// commits before the matching enrollment can create or complete a loss cursor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn pre_enrollment_placement_serializes_loss_cursor_creation() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let expected_session = SessionId::from_uuid(uuid(SESSION));
    let mut placement = pool.begin().await?;
    insert_uncommitted_exact_placement(
        &mut placement,
        expected_session,
        expected_enrollment.runner(),
    )
    .await?;
    let mut enrollment_insert = Box::pin(store.insert_enrollment(&expected_enrollment));

    tokio::time::timeout(LOCK_WAIT_PROBE, &mut enrollment_insert)
        .await
        .expect_err("enrollment must wait for the absent-baseline placement");
    placement.commit().await?;
    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, enrollment_insert).await??;
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
        .expect("the serialized terminal connection owns its loss cursor");
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(page.sessions(), &[expected_session]);
    assert!(!page.is_complete());
    drop(pool);
    Ok(())
}

/// exact-identity placement takes the runner-identity fence before
/// enrollment authority, so cursor completion cannot form an opposing edge.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn loss_cursor_completion_serializes_on_runner_identity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
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
        .expect("the terminal connection owns its pending propagation cursor");
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let mut enrollment_authority = pool.begin().await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_enrollment
          WHERE enrollment_id = $1
          FOR UPDATE",
    )
    .bind(expected_enrollment.enrollment().into_uuid())
    .fetch_one(&mut *enrollment_authority)
    .await?;
    let placement_store = RunnerProtocolStore::new(pool.clone(), catalog());
    let placement_insert = tokio::spawn(async move {
        tokio::time::timeout(
            LOCK_COMPLETION_TIMEOUT,
            placement_store.store_placement(&placement, None, None),
        )
        .await
    });
    let placement_blocked =
        tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, blocked_backends_reached(&pool, 1))
            .await
            .expect("placement enrollment-lock observation must remain bounded")?;
    let mut completion = Box::pin(store.complete_connection_loss_propagation(loss));

    tokio::time::timeout(LOCK_WAIT_PROBE, &mut completion)
        .await
        .expect_err("cursor completion must wait for placement's identity fence");
    enrollment_authority.commit().await?;
    placement_insert
        .await
        .expect("the placement task remains joinable")
        .expect("the placement insert must finish within its operation timeout")?;
    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, completion).await??;
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert!(
        placement_blocked,
        "placement must reach enrollment authority after taking identity"
    );
    assert_eq!(page.sessions(), &[]);
    assert!(page.is_complete());
    drop(pool);
    Ok(())
}

/// a fully projected loss cursor may transition once to completed.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_cursor_completes_after_final_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let expected_sessions =
        insert_bounded_propagation_session_fixture(&pool, expected_enrollment.runner()).await?;
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
        .expect("the terminal connection owns its pending propagation cursor");
    project_bounded_propagation_sessions(&pool, &expected_sessions).await?;

    sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET state_kind = 'completed'
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .execute(&pool)
    .await?;
    let completed = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(completed.loss(), loss);
    assert_eq!(completed.propagated_through(), None);
    assert!(completed.sessions().is_empty());
    assert!(completed.is_complete());
    drop(pool);
    Ok(())
}

/// the bounded loss transaction projects an exact unpinned
/// identity loss, its follower event, and its cursor advancement atomically.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_projects_exact_unpinned_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    let expected_revision = placement.revision();
    store.store_placement(&placement, None, None).await?;
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
        .expect("the exact runner loss owns its propagation cursor");
    let disposition = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let replay = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    store.complete_connection_loss_propagation(loss).await?;
    let loaded = store
        .load_placement(session)
        .await?
        .expect("the transaction installs the exact loss-before-pin record");
    let completed = store.load_connection_loss_propagation_page(loss).await?;
    let event = dispatch_next_outbox_event(&pool).await?;

    assert_eq!(
        disposition,
        RunnerConnectionLossSessionDisposition::Applied {
            state: DispatchedRunnerState::RunnerLostBeforePin,
            interrupted_tool_attempt: None,
        }
    );
    assert_eq!(replay, RunnerConnectionLossSessionDisposition::Replayed);
    assert_eq!(
        loaded.placement().state(),
        &SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
            expected_enrollment.runner(),
        ))
    );
    assert_eq!(loaded.interrupted_tool_attempt(), None);
    assert_eq!(completed.propagated_through(), Some(session));
    assert!(completed.is_complete());
    assert_eq!(event.session(), Some(session));
    assert_eq!(
        event.kind(),
        &DispatchedOutboxEventKind::RunnerStateTransition {
            runner: expected_enrollment.runner(),
            placement_revision: expected_revision,
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            working_directory: Some(exact_runner_directory()),
            state: DispatchedRunnerState::RunnerLostBeforePin,
        }
    );
    drop(pool);
    Ok(())
}

/// an exact-runner placement stored before enrollment uses
/// the same runner-identity fallback during projection that selected its page.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_projects_pre_enrollment_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
    store.insert_enrollment(&expected_enrollment).await?;
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
        .expect("the exact runner loss owns its propagation cursor");

    assert_eq!(
        store
            .propagate_connection_loss_session(loss, session)
            .await?,
        RunnerConnectionLossSessionDisposition::Applied {
            state: DispatchedRunnerState::RunnerLostBeforePin,
            interrupted_tool_attempt: None,
        }
    );
    assert_eq!(
        store
            .load_placement(session)
            .await?
            .expect("the loss projection remains readable")
            .placement()
            .state(),
        &SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
            expected_enrollment.runner(),
        ))
    );
    drop(pool);
    Ok(())
}

/// a placement change serialized after paging makes the old loss
/// subject superseded and advances the cursor without a second projection.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_advances_a_superseded_session() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
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
        .expect("the exact runner loss owns its propagation cursor");
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let disposition = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert_eq!(
        disposition,
        RunnerConnectionLossSessionDisposition::Superseded
    );
    assert_eq!(page.propagated_through(), Some(session));
    assert!(page.sessions().is_empty());
    assert!(!page.is_complete());
    drop(pool);
    Ok(())
}

/// cursor completion rechecks the affected placement set and cannot
/// hide a session that has not crossed the atomic propagation boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_rejects_premature_completion() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
    let connection = store
        .open_connection(expected_enrollment.enrollment())
        .await?;
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(expected_enrollment.runner()),
    );
    let session = placement.session();
    store.store_placement(&placement, None, None).await?;
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
        .expect("the exact runner loss owns its propagation cursor");
    let rejected = store
        .complete_connection_loss_propagation(loss)
        .await
        .expect_err("completion cannot skip the affected placement");
    let page = store.load_connection_loss_propagation_page(loss).await?;

    assert_store_check_violation(rejected);
    assert_eq!(page.propagated_through(), None);
    assert_eq!(page.sessions(), &[session]);
    assert!(!page.is_complete());
    drop(pool);
    Ok(())
}

/// an offered lease becomes exact
/// no-execution loss while its physical attempt and yielded turn wait remain
/// correlated to the same placement boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_retires_offered_lease() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let session = pin.placement.session();
    let attempt = pin.lease.attempt();
    let lease_id = pin.lease.correlation().lease;
    let lease_generation = pin.lease.generation();
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the offered lease loss owns its exact cursor");
    let disposition = store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(lease_id, lease_generation)
        .await?
        .expect("the offered lease is durably classified as lost");
    let wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the issuing turn yields to runner recovery");
    let attempt_state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(attempt.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(
        disposition,
        RunnerConnectionLossSessionDisposition::Applied {
            state: DispatchedRunnerState::RunnerLost,
            interrupted_tool_attempt: Some(attempt),
        }
    );
    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostUnclaimed
    );
    assert_eq!(
        loaded_loss
            .no_execution_proof()
            .map(|proof| proof.correlation()),
        Some(&loaded_loss.lost().correlation())
    );
    assert_eq!(wait.interrupted_tool_attempt(), Some(attempt));
    assert_eq!(attempt_state, "in_flight");
    drop(pool);
    Ok(())
}

/// refusing the follower event rolls
/// placement, lease, turn wait, and propagation-cursor mutation back together.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_rolls_back_as_one_boundary() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let session = pin.placement.session();
    let expected_placement_state = pin.placement.state().clone();
    let lease_id = pin.lease.correlation().lease;
    let lease_generation = pin.lease.generation();
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the rollback fixture owns its exact loss cursor");
    sqlx::raw_sql(
        "CREATE FUNCTION reject_runner_loss_outbox_for_test()
         RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'synthetic runner outbox refusal'
                 USING ERRCODE = '23514';
         END;
         $$;
         CREATE TRIGGER reject_runner_loss_outbox_for_test
         BEFORE INSERT ON runner_state_transition_outbox_event
         FOR EACH ROW EXECUTE FUNCTION reject_runner_loss_outbox_for_test();",
    )
    .execute(&pool)
    .await?;
    let rejected = store
        .propagate_connection_loss_session(loss, session)
        .await
        .expect_err("the injected follower-event refusal aborts propagation");
    let loaded_placement = store
        .load_placement(session)
        .await?
        .expect("the original pinned placement remains current");
    let loaded_lease = store
        .load_lease(lease_id, lease_generation)
        .await?
        .expect("the original offered lease remains current");
    let page = store.load_connection_loss_propagation_page(loss).await?;
    let wait = store.load_runner_recovery_wait(session).await?;

    assert_store_check_violation(rejected);
    assert_eq!(
        loaded_placement.placement().state(),
        &expected_placement_state
    );
    assert_eq!(
        loaded_lease.state(),
        signalbox_domain::RunnerLeaseState::Offered
    );
    assert_eq!(page.propagated_through(), None);
    assert_eq!(page.sessions(), &[session]);
    assert_eq!(wait, None);
    drop(pool);
    Ok(())
}

/// claimed pure work remains retryable and
/// in-flight while the turn yields to the exact runner-recovery wait.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_retains_claimed_pure_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let session = pin.placement.session();
    let correlation = pin.lease.correlation();
    let attempt = correlation.dispatch.attempt();
    let claimed = pin
        .lease
        .claim(correlation.clone())
        .expect("the offered pure lease accepts its exact claim");
    store.store_lease(&claimed).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the claimed pure lease loss owns its exact cursor");
    store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(correlation.lease, correlation.generation)
        .await?
        .expect("the claimed pure lease is durably lost");
    let attempt_state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(attempt.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert!(loaded_loss.retry().is_some());
    assert_eq!(loaded_loss.crash_attempt(), None);
    assert_eq!(attempt_state, "in_flight");
    drop(pool);
    Ok(())
}

/// claimed idempotent work retains
/// retry authority without erasing the fact that execution may have occurred.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_retains_idempotent_attempt() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(
            &pool,
            ActivePinEffectCase::IdempotentExternalEffect,
        )
        .await?;
    let session = pin.placement.session();
    let correlation = pin.lease.correlation();
    let attempt = correlation.dispatch.attempt();
    let claimed = pin
        .lease
        .claim(correlation.clone())
        .expect("the idempotent lease accepts its exact claim");
    store.store_lease(&claimed).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the idempotent lease loss owns its exact cursor");
    store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(correlation.lease, correlation.generation)
        .await?
        .expect("the idempotent lease is durably lost");
    let attempt_state: String =
        sqlx::query_scalar("SELECT state_kind FROM tool_attempt WHERE attempt_id = $1")
            .bind(attempt.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert!(loaded_loss.retry().is_some());
    assert_eq!(loaded_loss.crash_attempt(), None);
    assert_eq!(attempt_state, "in_flight");
    drop(pool);
    Ok(())
}

/// claimed side-effecting work keeps
/// execution ambiguity instead of being rewritten as known failure.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_transaction_preserves_side_effect_ambiguity() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, expected_enrollment, _, pin, connection_epoch) =
        stored_active_pin_fixture_with_authorization(
            &pool,
            ActivePinEffectCase::SideEffectingExternalEffect,
        )
        .await?;
    let session = pin.placement.session();
    let correlation = pin.lease.correlation();
    let attempt = correlation.dispatch.attempt();
    let claimed = pin
        .lease
        .claim(correlation.clone())
        .expect("the side-effecting lease accepts its exact claim");
    store.store_lease(&claimed).await?;
    store
        .transition_connection(
            expected_enrollment.enrollment(),
            connection_epoch,
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let loss = store
        .load_current_connection_loss(expected_enrollment.enrollment())
        .await?
        .expect("the side-effecting loss owns its exact cursor");
    store
        .propagate_connection_loss_session(loss, session)
        .await?;
    let loaded_loss = store
        .load_lease_loss(correlation.lease, correlation.generation)
        .await?
        .expect("the side-effecting lease is durably lost");
    let (attempt_state, disposition): (String, Option<String>) = sqlx::query_as(
        "SELECT state_kind, terminal_disposition_kind
           FROM tool_attempt WHERE attempt_id = $1",
    )
    .bind(attempt.into_uuid())
    .fetch_one(&pool)
    .await?;
    let wait = store
        .load_runner_recovery_wait(session)
        .await?
        .expect("the ambiguous attempt remains named by runner recovery");

    assert_eq!(
        loaded_loss.lost().state(),
        signalbox_domain::RunnerLeaseState::LostClaimed
    );
    assert_eq!(loaded_loss.retry(), None);
    assert_eq!(loaded_loss.crash_attempt(), Some(attempt));
    assert_eq!(attempt_state, "terminal");
    assert_eq!(disposition.as_deref(), Some("ambiguous"));
    assert_eq!(wait.interrupted_tool_attempt(), Some(attempt));
    drop(pool);
    Ok(())
}

/// a runner-loss propagation cursor is durable evidence and cannot be
/// deleted independently of its exact loss epoch.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_propagation_cursor_rejects_delete() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
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
        .expect("the terminal connection owns its durable cursor");
    let deleted = sqlx::query(
        "DELETE FROM runner_connection_loss_propagation
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .execute(&pool)
    .await
    .expect_err("a durable runner-loss cursor cannot be deleted");

    assert_check_violation(deleted);
    drop(pool);
    Ok(())
}

/// bulk truncation cannot bypass runner-loss cursor durability.
#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_propagation_cursor_rejects_truncate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let expected_enrollment = enrollment();
    store.insert_enrollment(&expected_enrollment).await?;
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
    let truncated = sqlx::query("TRUNCATE runner_connection_loss_propagation")
        .execute(&pool)
        .await
        .expect_err("durable runner-loss cursors cannot be truncated");

    assert_check_violation(truncated);
    drop(pool);
    Ok(())
}
