//! Workspace-instruction migration boundaries for live pre-alpha turns.

use crate::*;

async fn queued_turn(
    pool: &PgPool,
    identity_base: u128,
) -> Result<(SessionId, TurnId), Box<dyn Error>> {
    let session = SessionId::from_uuid(Uuid::from_u128(identity_base + 1));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(identity_base + 2));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            identity_base + 3,
            identity_base + 1,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let turn = TurnId::from_uuid(Uuid::from_u128(identity_base + 4));
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id,
             acceptance_position, state_kind)
         VALUES ($1, $2, $3, 1, 'queued')",
    )
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(Uuid::from_u128(identity_base + 6))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO queued_input_origin
            (turn_id, accepted_input_id, session_id, acceptance_position,
             priority_kind, defaults_version,
             requested_model_kind, requested_direct_model_selection_id,
             frozen_model_kind, frozen_direct_model_selection_id,
             model_parameters, known_provider_failure_retry, model_fallback,
             dangerous_tool_auto_approval)
         VALUES
            ($1, $2, $3, 1, 'ordinary', 1,
             'direct', $4, 'direct', $4,
             'provider_defaults', 'disabled', 'disabled', 'disabled')",
    )
    .bind(turn.into_uuid())
    .bind(Uuid::from_u128(identity_base + 6))
    .bind(session.into_uuid())
    .bind(selection.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok((session, turn))
}

async fn seed_pre_migration_active_turn(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
    identity_base: u128,
) -> Result<(Uuid, Uuid), Box<dyn Error>> {
    let attempt = Uuid::from_u128(identity_base + 1);
    let frontier = Uuid::from_u128(identity_base + 2);
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(session.into_uuid())
    .bind(frontier)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET attempt_history_present = true,
                state_kind = 'active',
                start_lineage_kind = 'first_in_session',
                starting_frontier_id = $1,
                active_phase_kind = 'running',
                current_attempt_id = $2
          WHERE session_id = $3 AND turn_id = $4",
    )
    .bind(frontier)
    .bind(attempt)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO turn_attempt
            (turn_attempt_id, turn_id, session_id, state_kind)
         VALUES ($1, $2, $3, 'prepared')",
    )
    .bind(attempt)
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok((attempt, frontier))
}

async fn assert_no_instruction_evidence(
    pool: &PgPool,
    session: SessionId,
    turn: TurnId,
) -> Result<(), Box<dyn Error>> {
    let discovery_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instruction_discovery WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await?;
    let manifest_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turn_instruction_manifest WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(discovery_rows, 0);
    assert_eq!(manifest_rows, 0);
    Ok(())
}

async fn insert_candidate_inventory(
    connection: &mut PgConnection,
    discovery: Uuid,
    bundle: Uuid,
    source_bytes: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO instruction_discovery_root
            (instruction_discovery_id, root_ordinal, root_kind, root_path)
         VALUES ($1, 1, 'workspace', '/workspace')",
    )
    .bind(discovery)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO registered_instruction_bundle
            (instruction_bundle_id, root_kind, root_path, source_path,
             bundle_kind, source_byte_length, source_hash_algorithm, source_hash)
         VALUES
            ($1, 'workspace', '/workspace', '/workspace/AGENTS.md',
             'agent_document', $2, 'sha256_v1', $3)",
    )
    .bind(bundle)
    .bind(Decimal::from(source_bytes))
    .bind(vec![0_u8; 32])
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO instruction_discovery_candidate
            (instruction_discovery_id, candidate_ordinal, instruction_bundle_id)
         VALUES ($1, 1, $2)",
    )
    .bind(discovery)
    .bind(bundle)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// INV-061: migration leaves queued work without synthetic instruction
/// evidence so its eventual first call performs ordinary discovery.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_migration_leaves_queued_turn_unbound()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) =
        postgres_before_workspace_instruction_migration().await?;
    let (session, turn) = queued_turn(&pool, 0x6810).await?;

    apply_workspace_instruction_migration(&pool).await?;

    assert_no_instruction_evidence(&pool, session, turn).await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: migration leaves an active turn with no prepared model call
/// without synthetic instruction evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_migration_leaves_callless_active_turn_unbound()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) =
        postgres_before_workspace_instruction_migration().await?;
    let (session, turn) = queued_turn(&pool, 0x6820).await?;
    seed_pre_migration_active_turn(&pool, session, turn, 0x6830).await?;

    apply_workspace_instruction_migration(&pool).await?;

    assert_no_instruction_evidence(&pool, session, turn).await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: the one-time migration backfills the manifest correlation on a
/// terminal model call without weakening its post-migration immutability.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_migration_backfills_a_terminal_model_call()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) =
        postgres_before_workspace_instruction_migration().await?;
    let identity_base = 0x6840;
    let (session, turn) = queued_turn(&pool, identity_base).await?;
    let active = seed_pre_migration_active_turn(&pool, session, turn, 0x6850).await?;
    let provider = Uuid::from_u128(0x6854);
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET pinned_provider_model_identity_id = $1
          WHERE session_id = $2 AND turn_id = $3",
    )
    .bind(provider)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    let call = Uuid::from_u128(0x6855);
    sqlx::query(
        "INSERT INTO model_call
            (model_call_id, turn_id, session_id, turn_attempt_id,
             selection_kind, direct_model_selection_id,
             resolved_provider_model_identity_id, context_frontier_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, 'direct', $5, $6, $7, $8, 'prepared')",
    )
    .bind(call)
    .bind(turn.into_uuid())
    .bind(session.into_uuid())
    .bind(active.0)
    .bind(Uuid::from_u128(identity_base + 2))
    .bind(provider)
    .bind(active.1)
    .bind(model_credential_reference().as_str())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE model_call
            SET state_kind = 'terminal', terminal_disposition_kind = 'refused'
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended',
                end_variant = 'without_stop',
                end_disposition = 'turn_refused'
          WHERE turn_attempt_id = $1",
    )
    .bind(active.0)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = $1,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                terminal_attempt_id = $2,
                terminal_model_call_id = $3,
                terminal_disposition_kind = 'refused'
          WHERE session_id = $4 AND turn_id = $5",
    )
    .bind(active.1)
    .bind(active.0)
    .bind(call)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    apply_workspace_instruction_migration(&pool).await?;

    let manifest: Uuid = sqlx::query_scalar(
        "SELECT turn_instruction_manifest_id
           FROM model_call
          WHERE model_call_id = $1",
    )
    .bind(call)
    .fetch_one(&pool)
    .await?;
    assert_eq!(manifest, turn.into_uuid());
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: every append-only instruction-evidence table rejects statement-level
/// truncation as well as row mutation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_evidence_has_every_truncate_guard()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let guard_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
           FROM pg_trigger
          WHERE tgname IN (
                'instruction_discovery_rejects_truncate',
                'instruction_discovery_root_rejects_truncate',
                'registered_instruction_bundle_rejects_truncate',
                'instruction_discovery_candidate_rejects_truncate',
                'instruction_discovery_finding_rejects_truncate',
                'turn_instruction_manifest_rejects_truncate'
          )
            AND (tgtype & 32) = 32",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(guard_count, 6);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: inserting the discovery parent seals its complete child inventory
/// against later roots, candidates, and findings.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_discovery_parent_seals_its_inventory()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x6870).await?;
    let discovery = Uuid::from_u128(0x6877);
    let bundle = Uuid::from_u128(0x6878);
    sqlx::query(
        "INSERT INTO registered_instruction_bundle
            (instruction_bundle_id, root_kind, root_path, source_path,
             bundle_kind, skill_name, skill_description, source_byte_length,
             source_hash_algorithm, source_hash)
         VALUES ($1, 'workspace', '/workspace', '/workspace/AGENTS.md',
                 'agent_document', NULL, NULL, 1, 'sha256_v1', $2)",
    )
    .bind(bundle)
    .bind([0_u8; 32].as_slice())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 0, 0, 0, 0, true)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;

    let root_error = sqlx::query(
        "INSERT INTO instruction_discovery_root
            (instruction_discovery_id, root_ordinal, root_kind, root_path)
         VALUES ($1, 1, 'workspace', '/workspace')",
    )
    .bind(discovery)
    .execute(&pool)
    .await
    .expect_err("a sealed discovery rejects a later root");
    let candidate_error = sqlx::query(
        "INSERT INTO instruction_discovery_candidate
            (instruction_discovery_id, candidate_ordinal, instruction_bundle_id)
         VALUES ($1, 1, $2)",
    )
    .bind(discovery)
    .bind(bundle)
    .execute(&pool)
    .await
    .expect_err("a sealed discovery rejects a later candidate");
    let finding_error = sqlx::query(
        "INSERT INTO instruction_discovery_finding
            (instruction_discovery_id, finding_ordinal, source_path, finding_kind)
         VALUES ($1, 1, '/workspace/AGENTS.md', 'entry_unreadable')",
    )
    .bind(discovery)
    .execute(&pool)
    .await
    .expect_err("a sealed discovery rejects a later finding");

    assert_eq!(
        root_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("instruction_discovery_membership_sealed")
    );
    assert_eq!(
        candidate_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("instruction_discovery_membership_sealed")
    );
    assert_eq!(
        finding_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("instruction_discovery_membership_sealed")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: finding evidence stores only canonical absolute source paths.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_finding_rejects_a_noncanonical_path()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO instruction_discovery_finding
            (instruction_discovery_id, finding_ordinal, source_path, finding_kind)
         VALUES ($1, 1, '/workspace/../secret', 'entry_unreadable')",
    )
    .bind(Uuid::from_u128(0x6880))
    .execute(&pool)
    .await
    .expect_err("a noncanonical finding source path is rejected");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.code()),
        Some("23514".into())
    );
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("instruction_discovery_finding_path_bounded")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: sealing authenticates both the stored finding count and contiguous
/// finding ordinals against the exact child inventory.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_discovery_seal_rejects_a_finding_ordinal_gap()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x6890).await?;
    let discovery = Uuid::from_u128(0x6897);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO instruction_discovery_finding
            (instruction_discovery_id, finding_ordinal, source_path, finding_kind)
         VALUES ($1, 2, '/workspace/AGENTS.md', 'entry_unreadable')",
    )
    .bind(discovery)
    .execute(&mut *transaction)
    .await?;
    let error = sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 1, 1, 0, 0, true)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("a finding ordinal gap prevents the discovery seal");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("instruction_discovery_finding_inventory_exact")
    );
    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: a complete discovery cannot retain resource-limit evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_complete_discovery_rejects_a_limit_finding()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x68b0).await?;
    let discovery = Uuid::from_u128(0x68b7);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO instruction_discovery_finding
            (instruction_discovery_id, finding_ordinal, source_path, finding_kind)
         VALUES ($1, 1, '/workspace', 'limit_elapsed_time')",
    )
    .bind(discovery)
    .execute(&mut *transaction)
    .await?;
    let error = sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 0, 1, 0, 1, true)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("limit evidence prevents a complete discovery seal");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("instruction_discovery_completeness_exact")
    );
    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: an incomplete discovery must end in exactly one terminal resource
/// limit finding.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_incomplete_discovery_requires_a_terminal_limit()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x68c0).await?;
    let discovery = Uuid::from_u128(0x68c7);
    let error = sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 0, 0, 0, 1, false)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await
    .expect_err("an incomplete discovery without a terminal limit cannot seal");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("instruction_discovery_completeness_exact")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: an append-only manifest cannot bind an incomplete diagnostic scan.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_manifest_requires_a_complete_discovery()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x68d0).await?;
    let discovery = Uuid::from_u128(0x68d7);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO instruction_discovery_finding
            (instruction_discovery_id, finding_ordinal, source_path, finding_kind)
         VALUES ($1, 1, '/workspace', 'limit_elapsed_time')",
    )
    .bind(discovery)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 0, 1, 0, 1, false)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    let error = sqlx::query(
        "INSERT INTO turn_instruction_manifest
            (turn_instruction_manifest_id, session_id, turn_id,
             instruction_discovery_id, boundary_kind,
             eligibility_hash_algorithm, eligibility_hash,
             admitted_set_hash_algorithm, admitted_set_hash,
             manifest_hash_algorithm, manifest_hash)
         VALUES ($1, $2, $3, $4, 'turn_start',
                 'sha256_v1', $5, 'sha256_v1', $5, 'sha256_v1', $5)",
    )
    .bind(Uuid::from_u128(0x68d8))
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(discovery)
    .bind(vec![0_u8; 32])
    .execute(&pool)
    .await
    .expect_err("an incomplete discovery cannot acquire a manifest");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("turn_instruction_manifest_discovery_complete")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: an append-only empty manifest accepts only the canonical hashes
/// derived from its exact session, turn, and boundary.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_manifest_requires_canonical_hashes()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x68f0).await?;
    let discovery = Uuid::from_u128(0x68f7);
    sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 0, 0, 0, 0, true)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&pool)
    .await?;
    let error = sqlx::query(
        "INSERT INTO turn_instruction_manifest
            (turn_instruction_manifest_id, session_id, turn_id,
             instruction_discovery_id, boundary_kind,
             eligibility_hash_algorithm, eligibility_hash,
             admitted_set_hash_algorithm, admitted_set_hash,
             manifest_hash_algorithm, manifest_hash)
         VALUES ($1, $2, $3, $4, 'turn_start',
                 'sha256_v1', $5, 'sha256_v1', $5, 'sha256_v1', $5)",
    )
    .bind(Uuid::from_u128(0x68f8))
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .bind(discovery)
    .bind(vec![0_u8; 32])
    .execute(&pool)
    .await
    .expect_err("an empty manifest rejects noncanonical hashes");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("turn_instruction_manifest_hash_shape")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: candidate inventory cannot exceed the scan's classified entries.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_candidates_require_classified_entry_usage()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x68e0).await?;
    let discovery = Uuid::from_u128(0x68e7);
    let mut transaction = pool.begin().await?;
    insert_candidate_inventory(&mut transaction, discovery, Uuid::from_u128(0x68e8), 1).await?;
    let error = sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 0, 0, 1, 0, true)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("one candidate cannot be sealed with zero classified entries");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("instruction_discovery_candidate_usage_within_consumed")
    );
    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: candidate inventory cannot exceed charged candidate-source bytes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_candidates_require_source_byte_usage()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x68f0).await?;
    let discovery = Uuid::from_u128(0x68f7);
    let mut transaction = pool.begin().await?;
    insert_candidate_inventory(&mut transaction, discovery, Uuid::from_u128(0x68f8), 1).await?;
    let error = sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 1, 0, 0, 0, true)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("one source byte cannot be sealed with zero charged bytes");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("instruction_discovery_candidate_usage_within_consumed")
    );
    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: sealing rejects a candidate whose registered authorizing root is
/// absent from the discovery's own ordered root inventory.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_workspace_instruction_discovery_seal_requires_the_candidate_root()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, turn) = queued_turn(&pool, 0x68a0).await?;
    let discovery = Uuid::from_u128(0x68a7);
    let bundle = Uuid::from_u128(0x68a8);
    sqlx::query(
        "INSERT INTO registered_instruction_bundle
            (instruction_bundle_id, root_kind, root_path, source_path,
             bundle_kind, skill_name, skill_description, source_byte_length,
             source_hash_algorithm, source_hash)
         VALUES ($1, 'workspace', '/other', '/other/AGENTS.md',
                 'agent_document', NULL, NULL, 1, 'sha256_v1', $2)",
    )
    .bind(bundle)
    .bind([0_u8; 32].as_slice())
    .execute(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO instruction_discovery_root
            (instruction_discovery_id, root_ordinal, root_kind, root_path)
         VALUES ($1, 1, 'workspace', '/workspace')",
    )
    .bind(discovery)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO instruction_discovery_candidate
            (instruction_discovery_id, candidate_ordinal, instruction_bundle_id)
         VALUES ($1, 1, $2)",
    )
    .bind(discovery)
    .bind(bundle)
    .execute(&mut *transaction)
    .await?;
    let error = sqlx::query(
        "INSERT INTO instruction_discovery
            (instruction_discovery_id, session_id, turn_id, limit_set_version,
             classified_entry_count, finding_count,
             candidate_source_byte_count, elapsed_millis, scan_complete)
         VALUES ($1, $2, $3, 1, 1, 0, 1, 0, true)",
    )
    .bind(discovery)
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .execute(&mut *transaction)
    .await
    .expect_err("a candidate authorized by an uninventoried root prevents the seal");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("instruction_discovery_candidate_root_in_inventory")
    );
    transaction.rollback().await?;
    pool.close().await;
    drop(container);
    Ok(())
}
