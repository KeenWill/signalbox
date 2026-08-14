//! Durable authority used to choose daemon-local instruction roots.

use crate::*;

/// INV-061: a live runner-placement head owns workspace discovery, while its
/// abandoned successor explicitly returns that authority to the daemon.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_current_runner_placement_is_detected_as_external_workspace_authority()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x6840));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x6841));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0x6842,
            0x6840,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let repository =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        );
    let before_placement = repository.session_has_runner_placement(session).await?;
    let runner = Uuid::from_u128(0x6843);
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, state_kind, pinned_tool_count,
             requested_sandbox_profile, permission_override_count)
         VALUES ($1, 1, 1, 'created', 'identity', $2, 'runner_default',
                 'none', 'unpinned', 0, 'ambient', 0)",
    )
    .bind(session.into_uuid())
    .bind(runner)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_session_placement (session_id, event_ordinal)
         VALUES ($1, 1)",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let after_placement = repository.session_has_runner_placement(session).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, state_kind, lost_runner_id,
             pinned_tool_count, requested_sandbox_profile,
             permission_override_count)
         VALUES ($1, 2, 1, 'runner_lost_before_pin', 'identity', $2,
                 'runner_default', 'none', 'runner_lost_before_pin', $2,
                 0, 'ambient', 0)",
    )
    .bind(session.into_uuid())
    .bind(runner)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = 2
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    let after_loss = repository.session_has_runner_placement(session).await?;
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, directory_selection_kind,
             workspace_requirement_kind, state_kind, lost_runner_id,
             pinned_tool_count, requested_sandbox_profile,
             permission_override_count)
         VALUES ($1, 3, 1, 'abandoned', 'identity', $2, 'runner_default',
                 'none', 'runner_abandoned', $2, 0, 'ambient', 0)",
    )
    .bind(session.into_uuid())
    .bind(runner)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_session_placement
            SET event_ordinal = 3
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    let after_abandonment = repository.session_has_runner_placement(session).await?;

    assert!(!before_placement);
    assert!(after_placement);
    assert!(after_loss);
    assert!(!after_abandonment);

    pool.close().await;
    drop(container);
    Ok(())
}

/// INV-061: storage preserves the separately bounded root and root-relative
/// source path instead of applying the root bound to their concatenation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn inv061_registered_source_path_retains_its_independent_relative_budget()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let root_path = format!("/{}", "a".repeat(4095));
    let source_path = format!("{root_path}/AGENTS.md");
    let bundle = Uuid::from_u128(0x6850);

    sqlx::query(
        "INSERT INTO registered_instruction_bundle
            (instruction_bundle_id, root_kind, root_path, source_path,
             bundle_kind, skill_name, skill_description, source_byte_length,
             source_hash_algorithm, source_hash)
         VALUES ($1, 'configured', $2, $3, 'agent_document', NULL, NULL,
                 1, 'sha256_v1', $4)",
    )
    .bind(bundle)
    .bind(&root_path)
    .bind(&source_path)
    .bind([0_u8; 32].as_slice())
    .execute(&pool)
    .await?;
    let persisted: String = sqlx::query_scalar(
        "SELECT source_path
           FROM registered_instruction_bundle
          WHERE instruction_bundle_id = $1",
    )
    .bind(bundle)
    .fetch_one(&pool)
    .await?;

    assert_eq!(persisted, source_path);

    pool.close().await;
    drop(container);
    Ok(())
}
