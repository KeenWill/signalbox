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
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                identity_base + 5,
                identity_base + 1,
                "pre-migration unstarted turn",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(identity_base + 6)),
            Some(turn),
        )
        .await?;
    Ok((session, turn))
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
    let activation = StartEligibleTurnRepository::new(pool.clone());
    let preview = activation
        .preview(
            session,
            AcceptedInputTurnActivationIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x6830)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x6831)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0x6832)),
                TurnAttemptId::from_uuid(Uuid::from_u128(0x6833)),
            ),
        )
        .await?
        .expect("the queued fixture has one activation preview");
    let activated = activation.commit_preview(preview).await?;
    let CommitActivationPreviewOutcome::Activated(activated) = activated else {
        panic!("the callless fixture turn activates");
    };
    assert_eq!(activated.turn(), turn);

    apply_workspace_instruction_migration(&pool).await?;

    assert_no_instruction_evidence(&pool, session, turn).await?;
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
