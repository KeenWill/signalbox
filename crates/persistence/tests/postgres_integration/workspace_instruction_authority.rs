//! Durable authority used to choose daemon-local instruction roots.

use crate::*;

/// a live runner-placement head owns workspace discovery, while its
/// abandoned successor explicitly returns that authority to the daemon.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn current_runner_placement_is_detected_as_external_workspace_authority()
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

/// a placement-head change after discovery is rejected under the
/// scheduler lock before any instruction evidence is retained.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_runner_placement_rejects_the_scanned_snapshot() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x6860));
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x6861));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0x6862,
            0x6860,
            ModelSelectionRequest::Direct(selection),
        ))
        .await?;
    let turn = TurnId::from_uuid(Uuid::from_u128(0x6863));
    let mut submit = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [AcceptedInputId::from_uuid(Uuid::from_u128(0x6864))],
            [turn],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x6865)),
            session,
            UserContent::try_text("placement race".to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?;
    let repository =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        );
    let observed = repository.observe_session_runner_placement(session).await?;
    let runner = Uuid::from_u128(0x6866);
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
    let snapshot = signalbox_application::discover_workspace_instructions(Vec::new());
    let discovery = signalbox_domain::InstructionDiscoveryId::from_uuid(Uuid::from_u128(0x6867));
    let manifest_id =
        signalbox_domain::TurnInstructionManifestId::from_uuid(Uuid::from_u128(0x6868));

    let error = repository
        .record_counted_activation_for_observed_placement(
            discovery,
            signalbox_domain::TurnInstructionManifest::empty_turn_start(manifest_id, session, turn),
            &snapshot,
            &observed,
            || signalbox_domain::InstructionBundleId::from_uuid(Uuid::from_u128(0x6869)),
        )
        .await
        .expect_err("changed placement rejects the scanned snapshot");
    let discovery_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM instruction_discovery WHERE instruction_discovery_id = $1",
    )
    .bind(discovery.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        error.to_string(),
        "runner placement changed during workspace discovery"
    );
    assert_eq!(discovery_count, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// storage preserves the separately bounded root and root-relative
/// source path instead of applying the root bound to their concatenation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registered_source_path_retains_its_independent_relative_budget()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let root_path = format!("/{}\\b", "a".repeat(4092));
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

/// the registered source path prefix is measured in characters, so a
/// non-ASCII root still admits its direct descendants.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registered_source_path_accepts_a_unicode_root() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let root_path = "/workspace/资料";
    let source_path = "/workspace/资料/AGENTS.md";
    let bundle = Uuid::from_u128(0x6851);

    sqlx::query(
        "INSERT INTO registered_instruction_bundle
            (instruction_bundle_id, root_kind, root_path, source_path,
             bundle_kind, skill_name, skill_description, source_byte_length,
             source_hash_algorithm, source_hash)
         VALUES ($1, 'configured', $2, $3, 'agent_document', NULL, NULL,
                 1, 'sha256_v1', $4)",
    )
    .bind(bundle)
    .bind(root_path)
    .bind(source_path)
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

/// append-only registration evidence enforces the filename implied by
/// its closed bundle kind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registered_agent_document_requires_the_agents_filename() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let error = sqlx::query(
        "INSERT INTO registered_instruction_bundle
            (instruction_bundle_id, root_kind, root_path, source_path,
             bundle_kind, skill_name, skill_description, source_byte_length,
             source_hash_algorithm, source_hash)
         VALUES ($1, 'workspace', '/workspace', '/workspace/README.md',
                 'agent_document', NULL, NULL, 1, 'sha256_v1', $2)",
    )
    .bind(Uuid::from_u128(0x6852))
    .bind([0_u8; 32].as_slice())
    .execute(&pool)
    .await
    .expect_err("agent-document evidence requires the AGENTS.md filename");

    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("registered_instruction_bundle_source_kind_shape")
    );

    pool.close().await;
    drop(container);
    Ok(())
}
