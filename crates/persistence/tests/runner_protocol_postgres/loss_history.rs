//! Loss history coverage.

use super::*;

pub(crate) const REPLACEMENT_ENROLLMENT: u128 = 0x9101;
#[tokio::test]
#[ignore = "requires Docker"]
async fn revision_one_loss_authenticates_the_creation_request() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET selector_runner_id = $2,
                lost_runner_id = $2
          WHERE session_id = $1
            AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .bind(uuid(LATER_RUNNER))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("revision-one loss cannot replace the creation request");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pinned_facts_on_loss_before_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET pinned_runner_id = lost_runner_id
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("loss before pin cannot discard contradictory pinned authority");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_loss_with_another_event_kind() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'abandoned'
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("loss state cannot normalize another event vocabulary");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

/// a later current record cannot impersonate the unique
/// revision-one placement creation event after relational guards are bypassed.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_later_created_record() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'created', state_kind = 'unpinned',
                lost_runner_id = NULL, loss_source_kind = NULL
          WHERE session_id = $1 AND event_ordinal = 2",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("only the ordinal-one row may be the creation event");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

/// a pre-pin replacement cannot impersonate revision-one
/// creation after relational guards are bypassed.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_revision_one_pre_pin_replacement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_pre_pin_replacement_projection(
        &pool,
        placement.session(),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
    )
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET placement_revision = 1
          WHERE session_id = $1 AND event_kind = 'pre_pin_replaced'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("a replacement event cannot carry the initial revision");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_malformed_pre_pin_replacement_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_requires_tools,
         DISABLE TRIGGER runner_session_placement_requires_permission_overrides",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET loss_source_kind = 'connection'
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(replacement.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("replacement history cannot normalize a malformed predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_malformed_pre_pin_replacement_origin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_requires_tools,
         DISABLE TRIGGER runner_session_placement_requires_permission_overrides",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET pinned_runner_id = selector_runner_id
          WHERE session_id = $1 AND event_kind = 'pre_pin_replaced'",
    )
    .bind(replacement.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(replacement.placement.session())
        .await
        .expect_err("replacement history cannot normalize a malformed origin");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn runner_loss_requires_a_closed_source() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (_, _, _, pin) = stored_pin_fixture(&pool).await?;
    let mut malformed = pool.begin().await?;
    let rejected = append_runner_lost_without_advancing_head(
        &mut malformed,
        pin.placement.session(),
        None,
        None,
        None,
    )
    .await
    .expect_err("runner loss requires its exact closed source");

    assert_check_violation(rejected);
    malformed.rollback().await?;
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn abandonment_retains_the_complete_lost_request() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    let rejected =
        append_abandoned_projection(&pool, placement.session(), Some("/workspace/tampered"))
            .await
            .expect_err("abandonment cannot change a retained request axis");

    assert_check_violation(rejected);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_missing_pre_pin_replacement_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        registration.registration().runner(),
    )
    .await?;
    let lost_again = replacement
        .placement
        .mark_runner_lost_before_pin(successor.runner())
        .expect("the unpinned successor may lose its selected runner");
    append_runner_lost_before_pin_projection(&pool, lost_again.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'pre_pin_replaced'",
    )
    .bind(lost_again.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(lost_again.session())
        .await
        .expect_err("revision two requires its exact append-only replacement origin");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// reconstitution history is restricted to the current
/// placement head's physical event prefix.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pre_pin_replacement_proof_after_current_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        registration.registration().runner(),
    )
    .await?;
    let lost_again = replacement
        .placement
        .mark_runner_lost_before_pin(successor.runner())
        .expect("the unpinned successor may lose its selected runner");
    append_runner_lost_before_pin_projection(&pool, lost_again.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_ordinal = event_ordinal + 3
          WHERE session_id = $1 AND event_ordinal IN (2, 3)",
    )
    .bind(lost_again.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(lost_again.session())
        .await
        .expect_err("replacement proof after the current head is not history");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_loss_metadata_on_a_pinned_placement() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET lost_runner_id = pinned_runner_id
          WHERE session_id = $1 AND event_kind = 'pinned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("a pinned placement cannot carry discarded loss metadata");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_loss_for_a_runner_other_than_the_pin() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET lost_runner_id = $2
          WHERE session_id = $1 AND event_kind = 'runner_lost'",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(uuid(REPLACEMENT_RUNNER))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("runner loss must name the exact pinned runner");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

/// pre-pin abandonment reconstitution requires its exact
/// immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pre_pin_abandonment_without_loss_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'created', state_kind = 'unpinned',
                lost_runner_id = NULL
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("pre-pin abandonment requires its exact loss predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pre-pin abandonment retains the complete authenticated
/// lineage beneath its immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn pre_pin_abandonment_requires_complete_loss_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'created'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("pre-pin abandonment cannot hide a missing placement origin");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// the current placement pointer cannot rewind from
/// terminal abandonment to its authenticated replaceable loss predecessor.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_rewound_current_placement_head() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let abandoned = lost
        .abandon_lost_runner()
        .expect("the lost placement may be abandoned");
    append_abandoned_projection(&pool, abandoned.session(), None).await?;
    sqlx::query("ALTER TABLE runner_current_session_placement DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement AS current_placement
            SET event_ordinal = loss.event_ordinal
           FROM runner_session_placement_record AS loss
          WHERE current_placement.session_id = $1
            AND loss.session_id = current_placement.session_id
            AND loss.event_kind = 'runner_lost'",
    )
    .bind(abandoned.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE runner_current_session_placement ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    let corrupted = store
        .load_placement(abandoned.session())
        .await
        .expect_err("the current pointer cannot hide the terminal abandonment event");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned abandonment reconstitution requires its exact
/// immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pinned_abandonment_without_loss_predecessor() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'pinned', state_kind = 'pinned',
                lost_runner_id = NULL, loss_source_kind = NULL
          WHERE session_id = $1 AND event_kind = 'runner_lost'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned abandonment requires its exact loss predecessor");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned abandonment reconstitution authenticates the
/// retained registration against the exact loss predecessor.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pinned_abandonment_with_cross_wired_registration()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET registration_enrollment_id = $2
          WHERE session_id = $1 AND event_kind = 'runner_lost'",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(uuid(REPLACEMENT_ENROLLMENT))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned abandonment requires its loss registration snapshot");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned abandonment retains the complete authenticated
/// lineage beneath its immediately preceding loss record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn pinned_abandonment_requires_complete_loss_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'created'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned abandonment cannot hide a missing placement origin");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// pre-pin loss reconstitution requires its exact
/// immediately preceding unpinned record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pre_pin_loss_relabelled_from_abandonment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'runner_lost_before_pin',
                state_kind = 'runner_lost_before_pin'
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("pre-pin loss cannot be fabricated from terminal abandonment");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinned loss reconstitution requires its exact
/// immediately preceding pinned record.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pinned_loss_relabelled_from_abandonment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'runner_lost', state_kind = 'runner_lost'
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("pinned loss cannot be fabricated from terminal abandonment");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// active pinned reconstitution requires the exact
/// predecessor for its admitted pinned event kind.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_pinned_state_relabelled_from_abandonment() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    append_abandoned_projection(&pool, pin.placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'pinned', state_kind = 'pinned',
                lost_runner_id = NULL, loss_source_kind = NULL
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("terminal abandonment cannot be resurrected as an active pin");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// every historical pre-pin loss authenticates its own
/// immediately preceding unpinned origin.
#[tokio::test]
#[ignore = "requires Docker"]
async fn load_rejects_abandonment_relabelled_as_historical_loss() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, placement.session()).await?;
    append_abandoned_projection(&pool, placement.session(), None).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER ALL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET event_kind = 'runner_lost_before_pin',
                state_kind = 'runner_lost_before_pin'
          WHERE session_id = $1 AND event_kind = 'abandoned'",
    )
    .bind(placement.session().into_uuid())
    .execute(&pool)
    .await?;
    append_pre_pin_replacement_projection(
        &pool,
        placement.session(),
        RunnerId::from_uuid(uuid(REPLACEMENT_RUNNER)),
    )
    .await?;
    let corrupted = store
        .load_placement(placement.session())
        .await
        .expect_err("replacement history cannot resurrect an abandoned placement");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::CrossWiredReference);
    drop(pool);
    Ok(())
}

/// pinning a pre-pin successor preserves authentication of
/// the complete append-only replacement history.
#[tokio::test]
#[ignore = "requires Docker"]
async fn pinned_pre_pin_successor_requires_complete_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    let pin = replacement
        .placement
        .pin_and_offer_lease(
            &successor,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the exact fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the pre-pin successor may pin the placement");
    store.store_pin(&pin, &registration).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(pin.placement.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(pin.placement.session())
        .await
        .expect_err("a pin cannot hide a missing pre-pin loss boundary");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// loss reconstitution authenticates the complete history
/// of the pin consumed at the loss boundary.
#[tokio::test]
#[ignore = "requires Docker"]
async fn lost_pre_pin_successor_requires_complete_history() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    insert_physical_attempt(&pool, INITIAL_PHYSICAL_ATTEMPT).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let initial_runner = RunnerId::from_uuid(uuid(RUNNER));
    let placement = SessionRunnerPlacement::new(
        SessionId::from_uuid(uuid(SESSION)),
        exact_runner_request(initial_runner),
    );
    store.store_placement(&placement, None, None).await?;
    let lost_before_pin = placement
        .mark_runner_lost_before_pin(initial_runner)
        .expect("the exact selected runner may be lost before pinning");
    append_runner_lost_before_pin_projection(&pool, lost_before_pin.session()).await?;
    let successor = replacement_enrollment();
    store.insert_enrollment(&successor).await?;
    let registration = store.register(&successor, advertisement()).await?;
    store.open_connection(successor.enrollment()).await?;
    let replacement = lost_before_pin
        .replace_lost_runner_before_pin(
            exact_runner_request(successor.runner()),
            registration.registration(),
        )
        .expect("the live distinct runner installs a successor request");
    append_pre_pin_replacement_projection(
        &pool,
        replacement.placement.session(),
        successor.runner(),
    )
    .await?;
    let pin = replacement
        .placement
        .pin_and_offer_lease(
            &successor,
            registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the exact fixture directory is valid"),
            None,
            authorized(INITIAL_PHYSICAL_ATTEMPT),
            offer_request(),
        )
        .expect("the pre-pin successor may pin the placement");
    store.store_pin(&pin, &registration).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned successor may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DISABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM runner_session_placement_record
          WHERE session_id = $1 AND event_kind = 'runner_lost_before_pin'",
    )
    .bind(lost.session().into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         ENABLE TRIGGER runner_session_placement_record_is_append_only",
    )
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(lost.session())
        .await
        .expect_err("a loss cannot hide a missing pre-pin loss boundary");

    assert_store_corruption(
        corrupted,
        RunnerProtocolCorruption::MissingCanonicalPlacement,
    );
    drop(pool);
    Ok(())
}

/// every historical runner-replacement row retains the
/// closed pinned shape even when a later successor becomes current.
#[tokio::test]
#[ignore = "requires Docker"]
async fn historical_runner_replacement_rejects_loss_metadata() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, _, pin) = stored_pin_fixture(&pool).await?;
    let original_grant = pin
        .grant
        .as_ref()
        .expect("the credential-bearing pin has its grant");
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let successor_enrollment = replacement_enrollment();
    store.insert_enrollment(&successor_enrollment).await?;
    let successor_registration = store
        .register(&successor_enrollment, advertisement())
        .await?;
    store
        .open_connection(successor_enrollment.enrollment())
        .await?;
    let revoked = store
        .revoke_grant(
            lost.session(),
            original_grant.runner(),
            original_grant.revision(),
        )
        .await?
        .expect("the active predecessor grant revokes exactly once");
    let replacement_request = lost.request().clone();
    let replacement = lost
        .replace_lost_runner(
            replacement_request,
            successor_registration.registration(),
            RunnerWorkingDirectory::try_new("/workspace/replacement".to_owned())
                .expect("the successor directory is valid"),
            None,
            Some(revoked),
        )
        .expect("the live successor replaces the lost runner");
    store
        .store_runner_replacement_projection_for_test(
            &replacement.placement,
            &successor_registration,
            replacement.grant.as_ref(),
        )
        .await?;
    let historical_replacement_revision = replacement.placement.revision();
    let replacement_grant = replacement
        .grant
        .as_ref()
        .expect("the successor carries the advanced grant");
    let profile_replacement = duplicate_placement(
        &replacement.placement,
        Some(successor_registration.registration()),
    )
    .replace_credential_profile(
        duplicate_grant(replacement_grant, successor_registration.registration()),
        successor_registration.registration(),
        replacement_profile(),
        [tool("inspect")],
    )
    .expect("the successor may replace its credential profile");
    store
        .store_placement(
            &profile_replacement.placement,
            Some(&successor_registration),
            Some(&profile_replacement.grant.grant),
        )
        .await?;
    let later_loss = profile_replacement
        .placement
        .mark_runner_lost()
        .expect("the profile-replaced successor may be marked lost");
    append_runner_lost_projection(&pool, later_loss.session()).await?;
    sqlx::query("ALTER TABLE runner_session_placement_record DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE runner_session_placement_record
         DROP CONSTRAINT runner_session_placement_state_shape,
         ADD CONSTRAINT runner_session_placement_state_shape CHECK (TRUE)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE runner_session_placement_record
            SET lost_runner_id = pinned_runner_id,
                loss_source_kind = 'registration'
          WHERE session_id = $1
            AND event_kind = 'runner_replaced'
            AND placement_revision = $2",
    )
    .bind(later_loss.session().into_uuid())
    .bind(Decimal::from(historical_replacement_revision.get()))
    .execute(&pool)
    .await?;
    let corrupted = store
        .load_placement(later_loss.session())
        .await
        .expect_err("a current successor cannot hide loss metadata on historical pinned state");

    assert_store_corruption(corrupted, RunnerProtocolCorruption::InvalidEncoding);
    drop(pool);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn generic_store_rejects_abandonment_without_scheduler_authority()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, _, registration, pin) = stored_pin_fixture(&pool).await?;
    let lost = pin
        .placement
        .mark_runner_lost()
        .expect("the pinned runner may be marked lost");
    append_runner_lost_projection(&pool, lost.session()).await?;
    let abandoned = lost
        .abandon_lost_runner()
        .expect("the lost placement can prepare terminal abandonment");
    let rejected = store
        .store_placement(&abandoned, Some(&registration), pin.grant.as_ref())
        .await
        .expect_err("the generic writer cannot invent an empty active-turn proof");

    assert_store_domain_error(rejected, RunnerDomainError::InvalidState);
    drop(pool);
    Ok(())
}
