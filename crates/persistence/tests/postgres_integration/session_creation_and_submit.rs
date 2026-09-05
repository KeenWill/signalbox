//! Session creation, configuration defaults, submitted input, and first turn activation.

use crate::*;
use signalbox_application::SubmitInputRequestError;
use signalbox_domain::{AttachmentKind, BlobDigest, DeclaredMediaType, UserContentPart};

fn attachment_part(digest: BlobDigest) -> UserContentPart {
    UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::File,
        media_type: DeclaredMediaType::try_new(String::from("application/octet-stream"))
            .expect("the fixture media type is valid"),
        display_filename: None,
    }
}

fn attachment_content(digest: BlobDigest) -> UserContent {
    UserContent::try_parts(vec![attachment_part(digest)])
        .expect("the fixture attachment content is canonical")
}

/// S01: the Postgres adapters preserve
/// application command outcomes, return the complete current session
/// projection, and keep infrastructure failure nonterminal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_application_session_services_use_postgres_adapters() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x601));
    let request = CreateSessionRequest::try_new(
        command_id,
        SessionConfigurationDefaults::new(direct(0x801)),
    )?;
    let conflicting_request =
        CreateSessionRequest::try_new(command_id, SessionConfigurationDefaults::new(alias(0x802)))?;
    let winner = SessionId::from_uuid(Uuid::from_u128(0x701));
    let replay_candidate = SessionId::from_uuid(Uuid::from_u128(0x702));
    let conflicting_candidate = SessionId::from_uuid(Uuid::from_u128(0x703));
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([winner, replay_candidate, conflicting_candidate]),
        repository,
    );

    let first = service.execute(request.clone()).await?;
    let replay = service.execute(request).await?;
    assert_eq!(first, replay);
    let CreateSessionOutcome::Applied(recorded_receipt) = first else {
        panic!("first application must return the recorded applied receipt");
    };
    assert_eq!(recorded_receipt.session(), winner);
    assert_ne!(recorded_receipt.session(), replay_candidate);

    assert_eq!(
        service.execute(conflicting_request).await?,
        CreateSessionOutcome::ConflictingReuse { command_id }
    );
    let committed_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM session_scheduler)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(committed_counts, (1, 1, 1));

    let load_service = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let loaded = load_service
        .execute(winner)
        .await?
        .expect("the created session is visible through the application query");
    assert_eq!(loaded.id(), winner);
    assert_eq!(
        loaded.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        load_service
            .execute(SessionId::from_uuid(Uuid::from_u128(0x7ff)))
            .await?,
        None
    );

    let (_session_ids, repository) = service.into_parts();
    pool.close().await;
    let unavailable_request = CreateSessionRequest::try_new(
        DurableCommandId::from_uuid(Uuid::from_u128(0x602)),
        SessionConfigurationDefaults::new(direct(0x803)),
    )?;
    let mut unavailable_service = CreateSessionService::new(
        FixedSessionIds::new([SessionId::from_uuid(Uuid::from_u128(0x704))]),
        repository,
    );
    let error = unavailable_service
        .execute(unavailable_request)
        .await
        .expect_err("a closed pool cannot become a terminal command outcome");
    assert!(matches!(
        error,
        CreateSessionError::Transaction(CreateSessionRepositoryError::Database(_))
    ));

    drop(container);
    Ok(())
}

/// S01: both ordinary creation replay and current-session loading
/// reject a user-initiated row carrying a contradictory spawning request.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_creation_readers_reject_spawning_request_on_user_session() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let command = DurableCommandId::from_uuid(Uuid::from_u128(0x0016_0001));
    let session = SessionId::from_uuid(Uuid::from_u128(0x0017_0001));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x0016_0001, 0x0017_0001, direct(0x0018_0001)))
        .await?;

    sqlx::query("DROP TRIGGER session_is_append_only ON session")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_creation_cause_shape,
         DROP CONSTRAINT session_spawning_request_fk,
         DROP CONSTRAINT session_delegation_relation_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session
         SET spawning_tool_request_id = $1
         WHERE session_id = $2",
    )
    .bind(Uuid::from_u128(0x0019_0001))
    .bind(session.into_uuid())
    .execute(&pool)
    .await?;

    let creation_error = CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .load(command)
        .await
        .expect_err("ordinary creation replay must validate the spawning request column");
    assert_eq!(
        create_session_corruption(creation_error),
        CreateSessionCorruption::Inconsistent("creation cause provenance")
    );

    let session_error = SessionRepository::new(pool.clone())
        .load_session(session)
        .await
        .expect_err("current-session loading must validate the same provenance shape");
    assert_eq!(
        session_corruption(session_error),
        SessionCorruption::Inconsistent("creation cause provenance")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// template creation durably copies the original bundle and its
/// provenance; a same-command replay after a catalog edit returns that winner.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn template_creation_persists_copy_and_name_keyed_replay() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0x1601));
    let winner = SessionId::from_uuid(Uuid::from_u128(0x1701));
    let replay_candidate = SessionId::from_uuid(Uuid::from_u128(0x1702));
    let name = SessionTemplateName::try_new("reviewer".to_owned())?;
    let original_provenance = SessionTemplateProvenance::new(
        name.clone(),
        SessionTemplateContentDigest::from_bytes([0x21; 32]),
    );
    let edited_provenance = SessionTemplateProvenance::new(
        name.clone(),
        SessionTemplateContentDigest::from_bytes([0x22; 32]),
    );
    let original_defaults = SessionConfigurationDefaults::complete(
        direct(0x1801),
        signalbox_domain::DangerousToolAutoApproval::ApproveAll,
        Some(SessionSystemPrompt::try_new(
            "original reviewer prompt".to_owned(),
        )?),
    );
    let edited_defaults = SessionConfigurationDefaults::complete(
        alias(0x1802),
        signalbox_domain::DangerousToolAutoApproval::Disabled,
        Some(SessionSystemPrompt::try_new(
            "edited reviewer prompt".to_owned(),
        )?),
    );
    let original_request = CreateSessionRequest::try_new_from_template(
        command_id,
        original_provenance.clone(),
        original_defaults.clone(),
    )?;
    let replay_after_edit = CreateSessionRequest::try_new_from_template(
        command_id,
        edited_provenance,
        edited_defaults,
    )?;
    let mut service = CreateSessionService::new(
        FixedSessionIds::new([winner, replay_candidate]),
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );

    let first = service.execute(original_request).await?;
    let replay = service.execute(replay_after_edit).await?;

    assert_eq!(first, replay);
    let replayed_session = applied_session(replay);
    assert_eq!(replayed_session, winner);
    assert_ne!(replayed_session, replay_candidate);

    let stored = sqlx::query(
        "SELECT s.template_name AS session_template_name,
                s.template_content_digest AS session_template_content_digest,
                c.template_name AS command_template_name,
                c.template_content_digest AS command_template_content_digest,
                d.storage_version AS registry_storage_version,
                c.storage_version AS command_storage_version
         FROM session AS s
         JOIN create_session_command AS c
           ON c.created_session_id = s.session_id
         JOIN durable_command AS d USING (command_id)
         WHERE s.session_id = $1",
    )
    .bind(winner.into_uuid())
    .fetch_one(&pool)
    .await?;
    let session_template_name: String = stored.try_get("session_template_name")?;
    let session_template_content_digest: Vec<u8> =
        stored.try_get("session_template_content_digest")?;
    let command_template_name: String = stored.try_get("command_template_name")?;
    let command_template_content_digest: Vec<u8> =
        stored.try_get("command_template_content_digest")?;
    let registry_storage_version: i16 = stored.try_get("registry_storage_version")?;
    let command_storage_version: i16 = stored.try_get("command_storage_version")?;
    assert_eq!(session_template_name, name.as_str());
    assert_eq!(
        session_template_content_digest,
        original_provenance.content_digest().as_bytes()
    );
    assert_eq!(command_template_name, name.as_str());
    assert_eq!(
        command_template_content_digest,
        original_provenance.content_digest().as_bytes()
    );
    assert_eq!(registry_storage_version, command_storage_version);
    assert_eq!(command_storage_version, 7);

    let loaded = LoadSessionService::new(SessionRepository::new(pool.clone()))
        .execute(winner)
        .await?
        .expect("template-created session remains loadable");
    assert_eq!(loaded.template_provenance(), Some(&original_provenance));
    assert_eq!(
        loaded.current_configuration_defaults().defaults(),
        &original_defaults
    );

    sqlx::query(
        "DROP TRIGGER create_session_command_is_append_only
         ON create_session_command",
    )
    .execute(&pool)
    .await?;
    let mut disagreement = pool.begin().await?;
    sqlx::query(
        "UPDATE create_session_command
         SET template_name = NULL,
             template_content_digest = NULL
         WHERE created_session_id = $1",
    )
    .bind(winner.into_uuid())
    .execute(&mut *disagreement)
    .await?;
    let disagreement = disagreement
        .commit()
        .await
        .expect_err("session provenance must bind back to its creation command");
    let disagreement_error = disagreement
        .as_database_error()
        .expect("provenance disagreement must be a database error");
    assert_eq!(disagreement_error.code().as_deref(), Some("23503"));
    assert_eq!(
        disagreement_error.constraint(),
        Some("session_template_provenance_creation_fk")
    );

    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_initial_defaults_fk",
    )
    .execute(&pool)
    .await?;
    let promptless_schema = sqlx::query(
        "UPDATE create_session_command
         SET system_prompt = NULL
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await
    .expect_err("template provenance requires a command prompt");
    let promptless_schema_error = promptless_schema
        .as_database_error()
        .expect("promptless template rejection must be a database error");
    assert_eq!(promptless_schema_error.code().as_deref(), Some("23514"));
    assert_eq!(
        promptless_schema_error.constraint(),
        Some("create_session_command_template_prompt_required")
    );

    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_template_prompt_required",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE create_session_command
         SET system_prompt = NULL
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&pool)
    .await?;
    let promptless_reader =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
            .load(command_id)
            .await
            .expect_err("promptless template provenance must fail closed");
    assert!(matches!(
        promptless_reader,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Inconsistent(
            "template creation without system prompt"
        ))
    ));

    sqlx::query("DROP TRIGGER durable_command_is_append_only ON durable_command")
        .execute(&pool)
        .await?;
    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_template_provenance_versioned,
         DROP CONSTRAINT create_session_command_registry_fk",
    )
    .execute(&pool)
    .await?;
    let mut pre_version_four = pool.begin().await?;
    sqlx::query(
        "UPDATE durable_command
         SET storage_version = 3
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&mut *pre_version_four)
    .await?;
    sqlx::query(
        "UPDATE create_session_command
         SET storage_version = 3
         WHERE command_id = $1",
    )
    .bind(command_id.into_uuid())
    .execute(&mut *pre_version_four)
    .await?;
    pre_version_four.commit().await?;
    let pre_version_four =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
            .load(command_id)
            .await
            .expect_err("pre-version-four template provenance must fail closed");
    assert!(matches!(
        pre_version_four,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Inconsistent(
            "pre-version-four template provenance"
        ))
    ));
    let pre_version_four_session = SessionRepository::new(pool.clone())
        .load_session(winner)
        .await
        .expect_err("session loading must reject pre-version-four template provenance");
    assert!(matches!(
        pre_version_four_session,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
            "pre-version-four template provenance"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_create_session_schema_preserves_typed_facts() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let mut transaction = pool.begin().await?;

    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00', 'operator')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000001',
             'interactive', 'none')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         VALUES ('70000000-0000-7000-8000-000000000001', 'created', false, false, 'operator')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         VALUES ('70000000-0000-7000-8000-000000000001', 1, 'created_unmonitored', false, 'operator')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000001')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000001', 1, 'direct',
             '70000000-0000-7000-8000-000000000002', NULL)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000001', 1)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id, start_gate, ownership)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, 'interactive', 'none', 1,
             'direct', '70000000-0000-7000-8000-000000000002', NULL,
             'applied', '70000000-0000-7000-8000-000000000001', 'open', 'unmonitored')",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let stored: (String, String, String, String) = sqlx::query_as(
        "SELECT s.creation_cause,
                s.ancestry_kind,
                d.model_selection_kind,
                c.result_kind
         FROM session AS s
         JOIN session_defaults_version AS d USING (session_id)
         JOIN create_session_command AS c
           ON c.created_session_id = s.session_id",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            "interactive".to_owned(),
            "none".to_owned(),
            "direct".to_owned(),
            "applied".to_owned()
        )
    );

    let generated_identity_defaults: i64 = sqlx::query_scalar(
        "SELECT count(*)
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND data_type = 'uuid'
           AND is_generated = 'NEVER'
           AND column_default IS NOT NULL",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(generated_identity_defaults, 0);

    let duplicate_command_id = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000001',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:01+00', 'operator')",
    )
    .execute(&pool)
    .await
    .expect_err("the user-global command ID must be unique");
    assert_eq!(
        duplicate_command_id
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23505")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn registry_and_create_session_constraints_reject_torn_or_conflicting_records()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let mut registry_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000011',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00', 'operator')",
    )
    .execute(&mut *registry_only)
    .await?;
    let missing_typed_record = registry_only
        .commit()
        .await
        .expect_err("a registry claim without its typed record must not commit");
    assert_eq!(
        missing_typed_record
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let invalid_kind = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000012',
             'unsupported_command', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00', 'operator')",
    )
    .execute(&pool)
    .await
    .expect_err("an unadmitted command kind must be rejected");
    assert_eq!(
        invalid_kind
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let mut session_without_command = pool.begin().await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000021',
             'interactive', 'none')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         VALUES ('70000000-0000-7000-8000-000000000021', 'created', false, false, 'operator')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         VALUES ('70000000-0000-7000-8000-000000000021', 1, 'created_unmonitored', false, 'operator')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000021')",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000021', 1, 'direct',
             '70000000-0000-7000-8000-000000000022', NULL)",
    )
    .execute(&mut *session_without_command)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000021', 1)",
    )
    .execute(&mut *session_without_command)
    .await?;
    let missing_create_command = session_without_command
        .commit()
        .await
        .expect_err("a session without its CreateSession record must not commit");
    assert_eq!(
        missing_create_command
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_schema_rejects_invalid_provenance_defaults_and_mutation() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;

    let delegated_without_spawn = sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000011',
             'delegated', 'none')",
    )
    .execute(&pool)
    .await
    .expect_err("delegated provenance without a spawn must be rejected");
    assert_eq!(
        delegated_without_spawn
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .as_deref(),
        Some("23514")
    );

    let sourced_user_session = sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000012',
             'interactive', 'single_source')",
    )
    .execute(&pool)
    .await
    .expect_err("user-initiated provenance with sourced ancestry must be rejected");
    assert_eq!(
        sourced_user_session
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .as_deref(),
        Some("23514")
    );

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000013',
             'create_session', 1, TIMESTAMPTZ '2026-07-18 00:00:00+00', 'operator')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session (session_id, creation_cause, ancestry_kind)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             'interactive', 'none')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_lifecycle
            (session_id, state_kind, owned, start_gate_held, actor_kind)
         VALUES ('70000000-0000-7000-8000-000000000013', 'created', false, false, 'operator')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_ownership_event
            (session_id, event_ordinal, transition_kind, owned_after, actor_kind)
         VALUES ('70000000-0000-7000-8000-000000000013', 1, 'created_unmonitored', false, 'operator')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ('70000000-0000-7000-8000-000000000013')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 1, 'alias',
             NULL, '70000000-0000-7000-8000-000000000014')",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ('70000000-0000-7000-8000-000000000013', 1)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id, start_gate, ownership)
         VALUES
            ('10000000-0000-4000-8000-000000000013',
             'create_session', 1, 'interactive', 'none', 1,
             'alias', NULL, '70000000-0000-7000-8000-000000000014',
             'applied', '70000000-0000-7000-8000-000000000013', 'open', 'unmonitored')",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let zero_version = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 0, 'direct',
             '70000000-0000-7000-8000-000000000015', NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("zero is not a domain ordinal");
    assert_eq!(
        zero_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let invalid_selection_shape = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013', 2, 'direct',
             '70000000-0000-7000-8000-000000000016',
             '70000000-0000-7000-8000-000000000017')",
    )
    .execute(&pool)
    .await
    .expect_err("a typed selection must have exactly one matching UUID");
    assert_eq!(
        invalid_selection_shape
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let missing_current_version = sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await
    .expect_err("the current pointer must reference an existing version");
    assert_eq!(
        missing_current_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             18446744073709551615, 'direct',
             '70000000-0000-7000-8000-000000000018', NULL)",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 18446744073709551615
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await?;

    let out_of_range_version = sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES
            ('70000000-0000-7000-8000-000000000013',
             18446744073709551616, 'direct',
             '70000000-0000-7000-8000-000000000019', NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("an ordinal above u64::MAX must be rejected");
    assert_eq!(
        out_of_range_version
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let immutable_session = sqlx::query(
        "UPDATE session
         SET ancestry_kind = 'none'
         WHERE session_id = '70000000-0000-7000-8000-000000000013'",
    )
    .execute(&pool)
    .await
    .expect_err("session provenance is immutable");
    assert_eq!(
        immutable_session
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);

    Ok(())
}

/// S01: first handling commits the complete typed creation, equal
/// replay returns the recorded identity, and structural conflict changes
/// nothing. Direct and alias defaults round-trip through reconstitution.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_transaction_apply_replay_conflict_and_restart() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let first = prepared(0x101, 0x701, direct(0x801));

    assert_eq!(
        repository.handle(first.clone()).await?,
        CreateSessionHandlingOutcome::Applied(first.applied_result())
    );

    let replay_candidate = prepared(0x101, 0x702, direct(0x801));
    assert_eq!(
        repository.handle(replay_candidate).await?,
        CreateSessionHandlingOutcome::Applied(first.applied_result())
    );

    let conflicting = prepared(0x101, 0x703, alias(0x802));
    assert_eq!(
        repository.handle(conflicting).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: first.command().command_id()
        }
    );

    let separate = prepared(0x102, 0x704, direct(0x801));
    let alias_creation = prepared(0x103, 0x705, alias(0x803));
    assert_eq!(
        repository.handle(separate.clone()).await?,
        CreateSessionHandlingOutcome::Applied(separate.applied_result())
    );
    assert_eq!(
        repository.handle(alias_creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(alias_creation.applied_result())
    );
    let loaded_alias = repository
        .load(alias_creation.command().command_id())
        .await?
        .expect("the applied alias creation must load");
    assert_eq!(loaded_alias.command(), alias_creation.command());
    assert_eq!(
        loaded_alias.applied_result(),
        alias_creation.applied_result()
    );

    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM create_session_command),
            (SELECT count(*) FROM session),
            (SELECT count(*) FROM session_defaults_version),
            (SELECT count(*) FROM session_current_defaults)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (3, 3, 3, 3, 3));

    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted =
        CreateSessionRepository::new(restarted_pool.clone(), test_session_credential_pin());
    let reconstituted = restarted
        .load(first.command().command_id())
        .await?
        .expect("committed creation must survive a new pool");
    assert_eq!(reconstituted.command(), first.command());
    assert_eq!(reconstituted.session().id(), first.session().id());
    assert_eq!(reconstituted.applied_result(), first.applied_result());

    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01: the user-global primary key is the concurrency boundary.
/// Equal duplicates return one winner; unequal duplicates retain that winner
/// and report one typed conflict.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_concurrent_duplicates_converge_on_the_committed_winner() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());

    let equal_left = prepared(0x111, 0x711, direct(0x811));
    let equal_right = prepared(0x111, 0x712, direct(0x811));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            repository.handle(equal_left).await
        },
        async {
            barrier.wait().await;
            repository.handle(equal_right).await
        }
    );
    let (left, right) = (left?, right?);
    let (
        CreateSessionHandlingOutcome::Applied(left_result),
        CreateSessionHandlingOutcome::Applied(right_result),
    ) = (left, right)
    else {
        panic!("equal duplicates must both return the recorded applied result");
    };
    assert_eq!(left_result, right_result);

    let conflict_left = prepared(0x112, 0x713, direct(0x812));
    let conflict_right = prepared(0x112, 0x714, alias(0x813));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            repository.handle(conflict_left).await
        },
        async {
            barrier.wait().await;
            repository.handle(conflict_right).await
        }
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CreateSessionHandlingOutcome::Applied(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                CreateSessionHandlingOutcome::ConflictingReuse { .. }
            ))
            .count(),
        1
    );

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM durable_command),
            (SELECT count(*) FROM session)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: a later write failure rolls back the provisional registry
/// insert, so the same command ID remains available for a valid retry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn infrastructure_failure_leaves_the_command_unclaimed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let existing = prepared(0x121, 0x721, direct(0x821));
    repository.handle(existing).await?;

    let colliding = prepared(0x122, 0x721, direct(0x822));
    let error = repository
        .handle(colliding.clone())
        .await
        .expect_err("the session identity collision must abort first handling");
    assert!(matches!(error, CreateSessionRepositoryError::Database(_)));
    assert!(
        repository
            .load(colliding.command().command_id())
            .await?
            .is_none()
    );

    let retry = prepared(0x122, 0x722, direct(0x822));
    assert_eq!(
        repository.handle(retry.clone()).await?,
        CreateSessionHandlingOutcome::Applied(retry.applied_result())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// an observed user-global claim is never treated as unseen merely
/// because its typed record is missing or its storage version is unknown.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn incomplete_or_unknown_claims_fail_closed_as_corruption() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let cross_wired = replacement(0x135, 0x735, 1, direct(0x835));
    defaults_repository.handle(cross_wired.clone()).await?;

    sqlx::query(
        "DROP TRIGGER durable_command_requires_typed_record
         ON durable_command",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_command
         DROP CONSTRAINT durable_command_storage_version_supported",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000131',
             'create_session', 1, transaction_timestamp(), 'operator'),
            ('10000000-0000-4000-8000-000000000132',
             'create_session', 99, transaction_timestamp(), 'operator'),
            ('10000000-0000-4000-8000-000000000133',
             'replace_session_defaults', 1, transaction_timestamp(), 'operator'),
            ('10000000-0000-4000-8000-000000000134',
             'replace_session_defaults', 99, transaction_timestamp(), 'operator'),
            ('10000000-0000-4000-8000-000000000135',
             'submit_input', 3, transaction_timestamp(), 'operator'),
            ('10000000-0000-4000-8000-000000000136',
             'submit_input', 99, transaction_timestamp(), 'operator')",
    )
    .execute(&pool)
    .await?;

    let repository = CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let missing_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000131")?);
    let missing = repository
        .load(missing_id)
        .await
        .expect_err("a claimed identifier without its typed record is corruption");
    assert!(matches!(
        missing,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Missing(
            "typed_command_id"
        ))
    ));

    let unknown_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000132")?);
    let unknown = repository
        .load(unknown_id)
        .await
        .expect_err("an unknown representation version is corruption");
    assert!(matches!(
        unknown,
        CreateSessionRepositoryError::Corruption(CreateSessionCorruption::Unsupported {
            field: "registry_version",
            ..
        })
    ));

    let missing_defaults_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000133")?);
    let missing_defaults = defaults_repository
        .load(missing_defaults_id)
        .await
        .expect_err("an incomplete defaults claim is corruption");
    assert!(matches!(
        missing_defaults,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Missing("typed_command_id")
        )
    ));

    let unknown_defaults_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000134")?);
    let unknown_defaults = defaults_repository
        .load(unknown_defaults_id)
        .await
        .expect_err("an unknown defaults representation is corruption");
    assert!(matches!(
        unknown_defaults,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Unsupported {
                field: "registry_version",
                ..
            }
        )
    ));

    let missing_input_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000135")?);
    assert!(matches!(
        input_repository
            .load(missing_input_id)
            .await
            .expect_err("an incomplete input claim is corruption"),
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Missing("typed_command_id"))
    ));
    let unknown_input_id =
        DurableCommandId::from_uuid(Uuid::parse_str("10000000-0000-4000-8000-000000000136")?);
    assert!(matches!(
        input_repository
            .load(unknown_input_id)
            .await
            .expect_err("an unknown input representation is corruption"),
        SubmitInputRepositoryError::Corruption(SubmitInputCorruption::Unsupported {
            field: "registry_version",
            ..
        })
    ));

    sqlx::query(
        "ALTER TABLE replace_session_defaults_command
         DROP CONSTRAINT replace_session_defaults_command_result_session_matches,
         DISABLE TRIGGER replace_session_defaults_command_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE replace_session_defaults_command
         SET result_session_id = $2
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x135))
    .bind(Uuid::from_u128(0x736))
    .execute(&pool)
    .await?;
    let inconsistent = defaults_repository
        .load(cross_wired.command_id())
        .await
        .expect_err("cross-wired typed result facts are corruption");
    assert!(matches!(
        inconsistent,
        ReplaceSessionDefaultsRepositoryError::Corruption(
            ReplaceSessionDefaultsCorruption::Domain(_)
        )
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// the second admitted command kind retains a
/// complete typed record, while the user-global registry and append-only
/// constraints reject torn, malformed, or mutable receipts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn defaults_schema_enforces_typed_receipts() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let mut registry_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000201',
             'replace_session_defaults', 1, transaction_timestamp(), 'operator')",
    )
    .execute(&mut *registry_only)
    .await?;
    let torn = registry_only
        .commit()
        .await
        .expect_err("a defaults registry claim must have its exact typed record");
    assert_eq!(
        torn.as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let mut typed_only = pool.begin().await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000204',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000204',
             1, 'direct',
             '70000000-0000-7000-8000-000000000205', NULL,
             'rejected', 'session_not_found',
             '70000000-0000-7000-8000-000000000204',
             NULL, NULL, NULL)",
    )
    .execute(&mut *typed_only)
    .await?;
    let missing_registry = typed_only
        .commit()
        .await
        .expect_err("a typed defaults record cannot commit without its registry claim");
    assert_eq!(
        missing_registry
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let mut missing_installed = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES
            ('10000000-0000-4000-8000-000000000205',
             'replace_session_defaults', 1, transaction_timestamp(), 'operator')",
    )
    .execute(&mut *missing_installed)
    .await?;
    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000205',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000205',
             1, 'direct',
             '70000000-0000-7000-8000-000000000206', NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000205',
             2, NULL, NULL)",
    )
    .execute(&mut *missing_installed)
    .await?;
    let missing_exact_defaults = missing_installed
        .commit()
        .await
        .expect_err("an applied receipt requires its exact immutable installed defaults");
    assert_eq!(
        missing_exact_defaults
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let malformed = sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ('10000000-0000-4000-8000-000000000202',
             'replace_session_defaults', 1,
             '70000000-0000-7000-8000-000000000202',
             1, 'direct',
             '70000000-0000-7000-8000-000000000203', NULL,
             'applied', NULL,
             '70000000-0000-7000-8000-000000000202',
             NULL, NULL, NULL)",
    )
    .execute(&pool)
    .await
    .expect_err("an applied result requires its typed installed version");
    assert_eq!(
        malformed
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let absent = replacement(0x203, 0x703, 1, direct(0x803));
    assert!(matches!(
        repository.handle(absent).await?,
        ReplaceSessionDefaultsHandlingOutcome::Rejected(
            ReplaceSessionDefaultsRejectedResult::SessionNotFound(_)
        )
    ));
    let stored: (String, String, Option<String>) = sqlx::query_as(
        "SELECT result_kind, rejection_kind, result_installed_version::text
         FROM replace_session_defaults_command
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        ("rejected".to_owned(), "session_not_found".to_owned(), None)
    );

    let immutable = sqlx::query(
        "UPDATE replace_session_defaults_command
         SET result_kind = result_kind
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .execute(&pool)
    .await
    .expect_err("typed defaults receipts are append-only");
    assert_eq!(
        immutable
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    let immutable_delete = sqlx::query(
        "DELETE FROM replace_session_defaults_command
         WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x203))
    .execute(&pool)
    .await
    .expect_err("typed defaults receipts cannot be deleted");
    assert_eq!(
        immutable_delete
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: the application service through the
/// Postgres adapter records applied and stale outcomes, replays historical
/// receipts, and leaves creation history distinct from current Session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_defaults_apply_replay_stale_and_history() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let mut defaults_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let load_service = LoadSessionService::new(SessionRepository::new(pool.clone()));
    let creation = prepared(0x211, 0x711, direct(0x811));
    create_repository.handle(creation.clone()).await?;

    let first = replacement_request(0x212, 0x711, 1, alias(0x812));
    let first_outcome = defaults_service.execute(first.clone()).await?;
    let ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
        first_applied,
    )) = &first_outcome
    else {
        panic!("the first replacement must apply");
    };
    assert_eq!(
        first_applied.installed().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(2).expect("positive version")
    );
    assert_eq!(
        defaults_service.execute(first.clone()).await?,
        first_outcome
    );

    let conflict = replacement_request(0x212, 0x711, 1, direct(0x813));
    assert_eq!(
        defaults_service.execute(conflict).await?,
        ReplaceSessionDefaultsOutcome::ConflictingReuse {
            command_id: first.command_id()
        }
    );

    let stale = replacement_request(0x213, 0x711, 1, direct(0x814));
    let stale_outcome = defaults_service.execute(stale.clone()).await?;
    let ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
        ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(stale_result),
    )) = stale_outcome
    else {
        panic!("the unseen stale command must record a mismatch");
    };
    assert_eq!(
        stale_result.current(),
        SessionConfigurationDefaultsVersion::try_from_u64(2).expect("positive version")
    );

    let later = replacement_request(0x214, 0x711, 2, direct(0x815));
    assert!(matches!(
        defaults_service.execute(later).await?,
        ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(_))
    ));

    assert_eq!(
        defaults_service.execute(first).await?,
        first_outcome,
        "historical applied replay must not require the mutable pointer"
    );
    assert_eq!(
        defaults_service.execute(stale).await?,
        stale_outcome,
        "recorded stale rejection must survive later state"
    );

    let current = load_service
        .execute(creation.session().id())
        .await?
        .expect("the session remains current");
    assert_eq!(
        current.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(3).expect("positive version")
    );
    assert_eq!(
        current.current_configuration_defaults().defaults().model(),
        direct(0x815)
    );

    let receipt = create_repository
        .load(creation.command().command_id())
        .await?
        .expect("creation history remains loadable");
    assert_eq!(
        receipt.session().configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        receipt
            .session()
            .configuration_defaults()
            .defaults()
            .model(),
        direct(0x811)
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM replace_session_defaults_command),
            (SELECT count(*) FROM session_defaults_version
              WHERE session_id = $1),
            (SELECT current_version::bigint FROM session_current_defaults
              WHERE session_id = $1)",
    )
    .bind(Uuid::from_u128(0x711))
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (3, 3, 3));

    pool.close().await;
    drop(container);
    Ok(())
}

/// A future expected epoch reaches the locked durable CAS boundary without
/// permitting its server-only placeholder replacement to apply.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn future_defaults_epoch_records_mismatch_without_applying_placeholder()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let creation = prepared(0x2111, 0x7111, direct(0x8111));
    create_repository.handle(creation.clone()).await?;
    let matching_request = replacement_request(0x2120, 0x7111, 1, alias(0x8120));
    let matching_command = ReplaceSessionDefaults::with_model_settings(
        matching_request.command_id(),
        matching_request.session(),
        matching_request.expected_current_version(),
        matching_request.replacement().clone(),
        matching_request.caller_model_settings(),
    );
    let repository = ReplaceSessionDefaultsRepository::new(pool.clone());

    assert_eq!(
        repository
            .handle_rejection_only_where_prompt_member(
                matching_command,
                PromptMemberStatement::Stated,
            )
            .await?,
        ReplaceSessionDefaultsRejectionOnlyOutcome::CurrentVersionMatched
    );
    assert!(
        repository
            .load(matching_request.command_id())
            .await?
            .is_none()
    );

    let request = replacement_request(0x2121, 0x7111, 2, alias(0x8121));
    let command = ReplaceSessionDefaults::with_model_settings(
        request.command_id(),
        request.session(),
        request.expected_current_version(),
        request.replacement().clone(),
        request.caller_model_settings(),
    );
    let outcome = repository
        .handle_rejection_only_where_prompt_member(command, PromptMemberStatement::Stated)
        .await?;
    let ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(
        ReplaceSessionDefaultsHandlingOutcome::Rejected(
            ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(rejected),
        ),
    ) = outcome
    else {
        panic!("the future epoch must record a mismatch");
    };
    let current = SessionRepository::new(pool.clone())
        .load_session(creation.session().id())
        .await?
        .expect("the created session remains present");

    assert_eq!(rejected.expected(), request.expected_current_version());
    assert_eq!(
        rejected.current(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        current.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        current.current_configuration_defaults().defaults().model(),
        creation
            .session()
            .configuration_defaults()
            .defaults()
            .model()
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S33: replacing defaults while a turn is
/// active leaves that turn bound to its accepted epoch, while the next origin
/// freezes the successor and starts behind an injected model-identity entry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s33_mid_session_model_switch_is_forward_only() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x731));
    let first_selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x831));
    let second_selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x832));
    let first_target = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x833));
    let second_target = ProviderModelIdentity::from_uuid(Uuid::from_u128(0x834));
    let first_credential = ModelCallCredentialReference::new("fixture-provider-first");
    let second_credential = ModelCallCredentialReference::new("fixture-provider-second");
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            0x231,
            0x731,
            ModelSelectionRequest::Direct(first_selection),
        ))
        .await?;

    let first_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x931));
    let first_turn = TurnId::from_uuid(Uuid::from_u128(0xa31));
    let mut first_submit = SubmitInputService::new(
        FixedSubmitInputIds::new([first_input], [first_turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let first_content = "before model replacement";
    first_submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x232)),
            session,
            UserContent::try_text(first_content.to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?;
    let false_boundary_requirement = sqlx::query(
        "INSERT INTO turn_lifecycle
            (turn_id, session_id, origin_accepted_input_id,
             acceptance_position, state_kind,
             model_identity_boundary_required)
         SELECT turn_id, session_id, origin_accepted_input_id,
                acceptance_position, state_kind, false
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(first_turn.into_uuid())
    .execute(&pool)
    .await
    .expect_err("a newly queued turn cannot claim pre-boundary compatibility");
    let false_boundary_database_error = false_boundary_requirement
        .as_database_error()
        .expect("the compatibility-shape check returns a database error");
    assert_eq!(false_boundary_database_error.code(), Some("23514".into()));
    assert_eq!(
        false_boundary_database_error.constraint(),
        Some("turn_lifecycle_model_identity_boundary_requirement_state")
    );
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xd31),
            starting_frontier: Uuid::from_u128(0xe31),
            initial_attempt: Uuid::from_u128(0xb31),
        },
    )
    .await?;

    let mut defaults_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    defaults_service
        .execute(ReplaceSessionDefaultsRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x233)),
            session,
            SessionConfigurationDefaultsVersion::first(),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(second_selection)),
            PromptMemberStatement::Stated,
        )?)
        .await?;

    let targets = ModelTargetCatalog::try_from_definitions([
        ModelTargetDefinition::new(
            first_selection,
            ResolvedProviderTarget::naming(first_target),
        ),
        ModelTargetDefinition::new(
            second_selection,
            ResolvedProviderTarget::naming(second_target),
        ),
    ])
    .expect("two exact selections form one immutable target catalog");
    let first_messages = complete_text_turn(
        &pool,
        session,
        targets.clone(),
        first_credential.clone(),
        0x10_000,
        "first reply",
    )
    .await?;
    assert_eq!(
        application_user_message(
            first_messages
                .first()
                .expect("the first call carries its user input")
        ),
        (first_input, first_content)
    );

    let second_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x932));
    let second_turn = TurnId::from_uuid(Uuid::from_u128(0xa32));
    let mut second_submit = SubmitInputService::new(
        FixedSubmitInputIds::new([second_input], [second_turn]),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let second_content = "after model replacement";
    second_submit
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x234)),
            session,
            UserContent::try_text(second_content.to_owned())
                .expect("fixture user content is admitted"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(2, ModelSelectionOverride::UseSessionDefault),
            },
        )?)
        .await?;
    let mut colliding_activation = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd33))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe33))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb33))],
        )
        .with_model_identity_entries([SemanticTranscriptEntryId::from_uuid(
            Uuid::from_u128(0xd31),
        )]),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        colliding_activation
            .execute(session)
            .await
            .expect_err("the reused model-boundary identity must fail before activation"),
        StartEligibleTurnRepositoryError::IdentityCollision(
            StartEligibleTurnIdentityCollision::ModelIdentityEntry
        )
    ));
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xd32),
            starting_frontier: Uuid::from_u128(0xe32),
            initial_attempt: Uuid::from_u128(0xb32),
        },
    )
    .await?;

    let active_snapshot = ProcessReadRepository::new(pool.clone())
        .read_transcript(session)
        .await?
        .expect("the active successor has a transcript");
    let process_tail = last_two(active_snapshot.entries());
    assert_eq!(
        process_model_identity(process_tail.penultimate),
        (second_turn, 2, second_selection)
    );
    assert_eq!(
        process_user_entry(process_tail.last),
        (second_input, second_turn, second_content)
    );

    let second_messages = complete_text_turn(
        &pool,
        session,
        targets,
        second_credential.clone(),
        0x20_000,
        "second reply",
    )
    .await?;
    let application_tail = last_two(&second_messages);
    assert_eq!(
        application_model_identity(application_tail.penultimate),
        (2, second_selection)
    );
    assert_eq!(
        application_user_message(application_tail.last),
        (second_input, second_content)
    );

    let first_pin: ModelCallPinFacts = sqlx::query_as(
        "SELECT direct_model_selection_id,
                resolved_provider_model_identity_id,
                credential_reference
           FROM model_call
          WHERE turn_id = $1",
    )
    .bind(first_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    let second_pin: ModelCallPinFacts = sqlx::query_as(
        "SELECT direct_model_selection_id,
                resolved_provider_model_identity_id,
                credential_reference
           FROM model_call
          WHERE turn_id = $1",
    )
    .bind(second_turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        first_pin,
        ModelCallPinFacts {
            direct_model_selection_id: first_selection.into_uuid(),
            resolved_provider_model_identity_id: first_target.into_uuid(),
            credential_reference: first_credential.as_str().to_owned(),
        }
    );
    assert_eq!(
        second_pin,
        ModelCallPinFacts {
            direct_model_selection_id: second_selection.into_uuid(),
            resolved_provider_model_identity_id: second_target.into_uuid(),
            credential_reference: second_credential.as_str().to_owned(),
        }
    );

    sqlx::query(
        "ALTER TABLE semantic_transcript_entry
            DROP CONSTRAINT semantic_transcript_entry_payload_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query("ALTER TABLE semantic_transcript_entry DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE semantic_transcript_entry
            SET failed_turn_id = $1
          WHERE model_identity_turn_id = $1",
    )
    .bind(second_turn.into_uuid())
    .execute(&pool)
    .await?;
    let mut corrupt_read = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd34))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe34))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb34))],
        ),
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert!(matches!(
        corrupt_read
            .execute(session)
            .await
            .expect_err("a mixed model-boundary payload must fail closed"),
        StartEligibleTurnRepositoryError::Corruption(StartEligibleTurnCorruption::Scheduling(
            SubmitInputCorruption::Inconsistent("semantic entry payload")
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compact_session_command_id_reuse_is_a_client_conflict() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = Uuid::from_u128(0xc051);
    let command = Uuid::from_u128(0xc052);
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xc053, 0xc051, direct(0xc054)))
        .await?;
    insert_pending_compact_command(
        &pool,
        command,
        session,
        Uuid::from_u128(0xc055),
        Uuid::from_u128(0xc056),
    )
    .await?;
    let input = start_input(
        0xc052,
        0xc051,
        "compact command identity reuse",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );

    assert_eq!(
        SubmitInputRepository::new(pool.clone())
            .handle(
                input.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xc057)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xc058))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// registry dispatch remains user-global across command kinds while
/// purpose-specific loads distinguish a valid other-kind claim from absence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn cross_kind_reuse_is_conflict_not_corruption_or_absence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let imported_repository =
        ImportedSessionRepository::new(pool.clone(), test_session_credential_pin());
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    let input_repository = SubmitInputRepository::new(pool.clone());
    let creation = prepared(0x221, 0x721, direct(0x821));
    create_repository.handle(creation).await?;
    assert!(matches!(
        imported_repository
            .load(DurableCommandId::from_uuid(Uuid::from_u128(0x221)))
            .await
            .expect_err("a CreateSession ID is not an unseen imported creation"),
        ImportedSessionRepositoryError::DifferentCommandKind { .. }
    ));

    let defaults_reuse = replacement(0x221, 0x721, 1, alias(0x822));
    assert_eq!(
        defaults_repository.handle(defaults_reuse.clone()).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: defaults_reuse.command_id()
        }
    );
    assert!(matches!(
        defaults_repository
            .load(defaults_reuse.command_id())
            .await
            .expect_err("a CreateSession ID is not an unseen defaults receipt"),
        ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { .. }
    ));
    let input_reuse = start_input(
        0x221,
        0x721,
        "cross-kind",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    assert_eq!(
        input_repository
            .handle(
                input_reuse.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: input_reuse.command_id(),
        }
    );
    assert!(matches!(
        input_repository
            .load(input_reuse.command_id())
            .await
            .expect_err("a CreateSession ID is not an unseen input receipt"),
        SubmitInputRepositoryError::DifferentCommandKind { .. }
    ));

    let defaults = replacement(0x222, 0x721, 1, alias(0x823));
    defaults_repository.handle(defaults.clone()).await?;
    let create_reuse = prepared(0x222, 0x722, direct(0x824));
    assert_eq!(
        create_repository.handle(create_reuse.clone()).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: create_reuse.command().command_id()
        }
    );
    assert!(matches!(
        create_repository
            .load(defaults.command_id())
            .await
            .expect_err("a defaults ID is not an unseen creation receipt"),
        CreateSessionRepositoryError::DifferentCommandKind { .. }
    ));

    let input = start_input(
        0x223,
        0x721,
        "input winner",
        2,
        ModelSelectionOverride::ReplaceWith(direct(0x825)),
    );
    input_repository
        .handle(
            input.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x923)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa23))),
        )
        .await?;
    let defaults_reuse = replacement(0x223, 0x721, 2, direct(0x826));
    assert_eq!(
        defaults_repository.handle(defaults_reuse).await?,
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );
    let create_reuse = prepared(0x223, 0x723, direct(0x827));
    assert_eq!(
        create_repository.handle(create_reuse).await?,
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: input.command_id(),
        }
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// two application-service calls expecting one version use
/// the adapter's pointer CAS as their linearization boundary. Exactly one
/// installs the successor and the loser records the winner's version as stale.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn concurrent_defaults_replacements_have_one_winner() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    create_repository
        .handle(prepared(0x231, 0x731, direct(0x831)))
        .await?;
    let mut left_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let mut right_service =
        ReplaceSessionDefaultsService::new(ReplaceSessionDefaultsRepository::new(pool.clone()));
    let left_command = replacement_request(0x232, 0x731, 1, direct(0x832));
    let right_command = replacement_request(0x233, 0x731, 1, alias(0x833));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let (left, right) = tokio::join!(
        async {
            barrier.wait().await;
            left_service.execute(left_command).await
        },
        async {
            barrier.wait().await;
            right_service.execute(right_command).await
        }
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(_))
            ))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
                    ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(_)
                ))
            ))
            .count(),
        1
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM replace_session_defaults_command
              WHERE session_id = $1),
            (SELECT count(*) FROM session_defaults_version
              WHERE session_id = $1 AND version = 2),
            (SELECT current_version::bigint FROM session_current_defaults
              WHERE session_id = $1)",
    )
    .bind(Uuid::from_u128(0x731))
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 1, 2));

    pool.close().await;
    drop(container);
    Ok(())
}

/// exhausted versions are recorded rejections, while an
/// infrastructure failure after provisional claim rolls back both the claim
/// and the attempted pointer change.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn exhaustion_and_precommit_failure_are_distinct() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let defaults_repository = ReplaceSessionDefaultsRepository::new(pool.clone());
    create_repository
        .handle(prepared(0x241, 0x741, direct(0x841)))
        .await?;
    create_repository
        .handle(prepared(0x242, 0x742, direct(0x842)))
        .await?;

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 18446744073709551615, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x741))
    .bind(Uuid::from_u128(0x843))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 18446744073709551615
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x741))
    .execute(&pool)
    .await?;
    let exhausted = replacement(0x243, 0x741, u64::MAX, alias(0x844));
    let exhausted_outcome = defaults_repository.handle(exhausted.clone()).await?;
    assert!(matches!(
        exhausted_outcome,
        ReplaceSessionDefaultsHandlingOutcome::Rejected(
            ReplaceSessionDefaultsRejectedResult::VersionExhausted(_)
        )
    ));
    assert_eq!(
        defaults_repository.handle(exhausted).await?,
        exhausted_outcome
    );

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', $2, NULL)",
    )
    .bind(Uuid::from_u128(0x742))
    .bind(Uuid::from_u128(0x845))
    .execute(&pool)
    .await?;
    let fails_after_claim = replacement_request(0x244, 0x742, 1, alias(0x846));
    let mut failing_service = ReplaceSessionDefaultsService::new(defaults_repository.clone());
    assert!(matches!(
        failing_service
            .execute(fails_after_claim.clone())
            .await
            .expect_err("the colliding immutable successor aborts the transaction"),
        ReplaceSessionDefaultsRepositoryError::Database { .. }
    ));
    assert!(
        defaults_repository
            .load(fails_after_claim.command_id())
            .await?
            .is_none(),
        "the failed transaction must not claim the command ID"
    );
    let pointer: i64 = sqlx::query_scalar(
        "SELECT current_version::bigint
         FROM session_current_defaults
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x742))
    .fetch_one(&pool)
    .await?;
    assert_eq!(pointer, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: load-by-session identity returns the
/// complete version selected by the current pointer, while creation receipt
/// replay remains pinned to the immutable creation-time version.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_current_session_load_and_receipt_replay_remain_distinct() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let session_repository = SessionRepository::new(pool.clone());
    let direct_creation = prepared(0x501, 0x901, direct(0x801));
    let alias_creation = prepared(0x502, 0x902, alias(0x802));

    assert!(
        session_repository
            .load_session(SessionId::from_uuid(Uuid::from_u128(0x999)))
            .await?
            .is_none(),
        "only an absent session row is a not-found result"
    );
    assert_eq!(
        create_repository.handle(direct_creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(direct_creation.applied_result())
    );
    assert_eq!(
        create_repository.handle(alias_creation.clone()).await?,
        CreateSessionHandlingOutcome::Applied(alias_creation.applied_result())
    );

    let loaded_direct = session_repository
        .load_session(direct_creation.session().id())
        .await?
        .expect("the committed direct session must load");
    assert_eq!(loaded_direct.id(), direct_creation.session().id());
    assert_eq!(
        loaded_direct.creation_provenance(),
        direct_creation.session().provenance()
    );
    assert_eq!(
        loaded_direct.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        loaded_direct
            .current_configuration_defaults()
            .defaults()
            .model(),
        direct(0x801)
    );

    let loaded_alias = session_repository
        .load_session(alias_creation.session().id())
        .await?
        .expect("the committed alias session must load");
    assert_eq!(
        loaded_alias
            .current_configuration_defaults()
            .defaults()
            .model(),
        alias(0x802)
    );

    let direct_session_id = Uuid::from_u128(0x901);
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'alias', NULL, $2)",
    )
    .bind(direct_session_id)
    .bind(Uuid::from_u128(0x803))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(direct_session_id)
    .execute(&pool)
    .await?;

    let current = session_repository
        .load_session(direct_creation.session().id())
        .await
        .expect("the advanced current session load must succeed")
        .expect("the session row remains present");
    assert_eq!(
        current.current_configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::try_from_u64(2)
            .expect("two is a positive defaults version")
    );
    assert_eq!(
        current.current_configuration_defaults().defaults().model(),
        alias(0x803)
    );

    let receipt = create_repository
        .load(direct_creation.command().command_id())
        .await?
        .expect("creation receipt remains loadable after current defaults advance");
    assert_eq!(receipt.command(), direct_creation.command());
    assert_eq!(
        receipt.session().configuration_defaults().version(),
        SessionConfigurationDefaultsVersion::first()
    );
    assert_eq!(
        receipt
            .session()
            .configuration_defaults()
            .defaults()
            .model(),
        direct(0x801)
    );

    let replay_candidate = prepared(0x501, 0x903, direct(0x801));
    assert_eq!(
        create_repository.handle(replay_candidate).await?,
        CreateSessionHandlingOutcome::Applied(direct_creation.applied_result())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// once the session row exists, absent,
/// malformed, unknown, undecodable, or non-unique current projection facts fail
/// closed as typed corruption rather than becoming `None` or nearby valid
/// defaults.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn current_session_corruption_fails_closed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let create_repository =
        CreateSessionRepository::new(pool.clone(), test_session_credential_pin());
    let session_repository = SessionRepository::new(pool.clone());
    let missing_pointer = prepared(0x511, 0x911, direct(0x811));
    let invalid_pointer = prepared(0x512, 0x912, direct(0x812));
    let missing_selected = prepared(0x513, 0x913, direct(0x813));
    let malformed_selected = prepared(0x514, 0x914, direct(0x814));
    let unknown_provenance = prepared(0x515, 0x915, direct(0x815));
    let duplicate_projection = prepared(0x516, 0x916, direct(0x816));
    create_repository.handle(missing_pointer.clone()).await?;
    create_repository.handle(invalid_pointer.clone()).await?;
    create_repository.handle(missing_selected.clone()).await?;
    create_repository.handle(malformed_selected.clone()).await?;
    create_repository.handle(unknown_provenance.clone()).await?;
    create_repository
        .handle(duplicate_projection.clone())
        .await?;

    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_current_defaults_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM session_current_defaults
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x911))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_version_fk,
         DROP CONSTRAINT session_current_defaults_version_positive_u64",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 0
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x912))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x913))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_defaults_version
         DROP CONSTRAINT session_defaults_version_model_selection_shape",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, 2, 'direct', NULL, NULL)",
    )
    .bind(Uuid::from_u128(0x914))
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session_current_defaults
         SET current_version = 2
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x914))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE create_session_command
         DROP CONSTRAINT create_session_command_provenance_fk",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session
         DROP CONSTRAINT session_creation_cause_closed,
         DROP CONSTRAINT session_creation_cause_shape,
         DISABLE TRIGGER session_is_append_only",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE session
         SET creation_cause = 'unknown'
         WHERE session_id = $1",
    )
    .bind(Uuid::from_u128(0x915))
    .execute(&pool)
    .await?;

    sqlx::query(
        "ALTER TABLE session_current_defaults
         DROP CONSTRAINT session_current_defaults_pkey",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(Uuid::from_u128(0x916))
    .execute(&pool)
    .await?;

    let missing = session_repository
        .load_session(missing_pointer.session().id())
        .await
        .expect_err("a missing pointer is corruption");
    assert!(matches!(
        missing,
        SessionRepositoryError::Corruption(SessionCorruption::Missing(
            "current_defaults_session_id"
        ))
    ));

    let invalid = session_repository
        .load_session(invalid_pointer.session().id())
        .await
        .expect_err("a non-positive pointer version is corruption");
    assert!(matches!(
        invalid,
        SessionRepositoryError::Corruption(SessionCorruption::InvalidOrdinal {
            field: "current_version",
            ..
        })
    ));

    let missing_selected_row = session_repository
        .load_session(missing_selected.session().id())
        .await
        .expect_err("a missing selected defaults row is corruption");
    assert!(matches!(
        missing_selected_row,
        SessionRepositoryError::Corruption(SessionCorruption::Missing(
            "selected_defaults_session_id"
        ))
    ));

    let malformed = session_repository
        .load_session(malformed_selected.session().id())
        .await
        .expect_err("a malformed selected defaults record is corruption");
    assert!(matches!(
        malformed,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent("model selection"))
    ));

    let unknown = session_repository
        .load_session(unknown_provenance.session().id())
        .await
        .expect_err("an unknown creation cause is corruption");
    assert!(matches!(
        unknown,
        SessionRepositoryError::Corruption(SessionCorruption::Unsupported {
            field: "creation cause",
            ..
        })
    ));

    let duplicate = session_repository
        .load_session(duplicate_projection.session().id())
        .await
        .expect_err("more than one current projection row is corruption");
    assert!(matches!(
        duplicate,
        SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
            "current session projection cardinality"
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// the third command family is a
/// normalized closed schema whose deferred reverse and effect constraints
/// reject a claim without its typed terminal record.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_schema_is_closed_and_normalized() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name
           FROM information_schema.tables
          WHERE table_schema = 'public'
            AND table_name IN (
                'submit_input_command',
                'accepted_input',
                'queued_input_origin'
            )
          ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        tables,
        vec![
            "accepted_input".to_owned(),
            "queued_input_origin".to_owned(),
            "submit_input_command".to_owned(),
        ]
    );

    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname
           FROM pg_constraint
          WHERE conname IN (
                'submit_input_command_applied_effect_fk',
                'submit_input_command_last_position_fk',
                'submit_input_command_current_defaults_fk',
                'submit_input_command_selected_defaults_fk',
                'submit_input_command_actor_shape',
                'submit_input_command_delivery_shape',
                'submit_input_command_result_shape',
                'accepted_input_queued_origin_fk',
                'queued_input_origin_accepted_input_fk'
          )
          ORDER BY conname",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(constraints.len(), 9);

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'submit_input', 3, transaction_timestamp(), 'operator')",
    )
    .bind(Uuid::from_u128(0x3ff))
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("a registry claim without its typed SubmitInput record must not commit");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    let command = start_input(
        0x3fe,
        0x7fe,
        "immutable",
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    SubmitInputRepository::new(pool.clone())
        .handle(
            command,
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9fe)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xafe))),
        )
        .await?;
    let error = sqlx::query(
        "UPDATE submit_input_command_content_part
            SET text_value = 'mutated'
          WHERE command_id = $1",
    )
    .bind(Uuid::from_u128(0x3fe))
    .execute(&pool)
    .await
    .expect_err("typed SubmitInput records are append-only");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23514".into())
    );

    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x3fc, 0x7fc, direct(0x8fc)))
        .await?;
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x3fd,
                0x7fc,
                "complete source",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x9fd)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xafd))),
        )
        .await?;

    let source_command_id = Uuid::from_u128(0x3fd);
    let malformed_no_active_turn = insert_malformed_submit_rejection(
        &pool,
        Uuid::from_u128(0x3fa),
        source_command_id,
        "no_active_turn",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect_err("no_active_turn");
    assert_eq!(
        malformed_no_active_turn
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    let malformed_defaults_mismatch = insert_malformed_submit_rejection(
        &pool,
        Uuid::from_u128(0x3f9),
        source_command_id,
        "session_defaults_version_mismatch",
        None,
        None,
        Some(Decimal::ONE),
        None,
        None,
        None,
    )
    .await
    .expect_err("session_defaults_version_mismatch");
    assert_eq!(
        malformed_defaults_mismatch
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    let malformed_unknown_alias = insert_malformed_submit_rejection(
        &pool,
        Uuid::from_u128(0x3f8),
        source_command_id,
        "unknown_model_alias",
        None,
        None,
        None,
        Some(Uuid::from_u128(0x8f8)),
        None,
        None,
    )
    .await
    .expect_err("unknown_model_alias");
    assert_eq!(
        malformed_unknown_alias
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );
    let malformed_exhaustion = insert_malformed_submit_rejection(
        &pool,
        Uuid::from_u128(0x3f7),
        source_command_id,
        "acceptance_position_exhausted",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect_err("acceptance_position_exhausted");
    assert_eq!(
        malformed_exhaustion
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    let error = insert_malformed_submit_rejection(
        &pool,
        Uuid::from_u128(0x3f6),
        source_command_id,
        "acceptance_position_exhausted",
        None,
        None,
        None,
        None,
        None,
        Some(Decimal::from(u64::MAX)),
    )
    .await
    .expect_err("exhaustion must reference the session's actual maximum-position input");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'submit_input', 3, transaction_timestamp(), 'operator')",
    )
    .bind(Uuid::from_u128(0x3fb))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             $2, $3,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position
           FROM submit_input_command
          WHERE command_id = $4",
    )
    .bind(Uuid::from_u128(0x3fb))
    .bind(Uuid::from_u128(0x9fb))
    .bind(Uuid::from_u128(0xafb))
    .bind(Uuid::from_u128(0x3fd))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value, blob_digest,
             attachment_kind, declared_media_type, display_filename)
         SELECT $1, position, part_kind, text_value, blob_digest,
                attachment_kind, declared_media_type, display_filename
           FROM submit_input_command_content_part
          WHERE command_id = $2",
    )
    .bind(Uuid::from_u128(0x3fb))
    .bind(Uuid::from_u128(0x3fd))
    .execute(&mut *transaction)
    .await?;
    let error = transaction
        .commit()
        .await
        .expect_err("an applied typed receipt without its exact effects must not commit");
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some("23503".into())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The deployment's configured accepted-input content bound is enforced at
/// application admission: oversized text fails before the typed command
/// exists, so it never reaches SQL and claims no durable identifier.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn content_size_bound_rejects_oversized_text_at_application() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    const CONFIGURED_MAX_UTF8_BYTES: usize = 11;
    let oversized = UserContent::try_text("a".repeat(CONFIGURED_MAX_UTF8_BYTES + 1))
        .expect("domain text is intentionally unbounded");
    let error = SubmitInputRequest::try_new_with_content_limit(
        DurableCommandId::from_uuid(Uuid::from_u128(0x320)),
        SessionId::from_uuid(Uuid::from_u128(0x720)),
        oversized,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
        Some(CONFIGURED_MAX_UTF8_BYTES),
    )
    .expect_err("text over the provisional bound fails application admission");
    assert_eq!(
        error,
        SubmitInputRequestError::OversizedContent {
            utf8_byte_length: CONFIGURED_MAX_UTF8_BYTES + 1,
            max_utf8_bytes: CONFIGURED_MAX_UTF8_BYTES,
        }
    );
    let claimed: i64 = sqlx::query_scalar("SELECT count(*) FROM durable_command")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        claimed, 0,
        "content rejected before typed-command construction claims no durable identifier"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

async fn persist_at_bound_content(pool: &PgPool) -> Result<usize, Box<dyn Error>> {
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x321, 0x721, direct(0x821)))
        .await?;
    let at_bound = "a".repeat(1_048_576);
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x322,
                0x721,
                &at_bound,
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0x921)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xa21))),
        )
        .await?;
    Ok(at_bound.len())
}

/// The persistence adapter and both mirrored rows admit the domain's exact
/// one-mebibyte text bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn content_size_bound_commits_at_exact_maximum() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let expected_length = i32::try_from(persist_at_bound_content(&pool).await?)?;
    let stored_lengths: Vec<i32> = sqlx::query_scalar(
        "SELECT octet_length(text_value)
           FROM submit_input_command_content_part
         UNION ALL
         SELECT octet_length(text_value)
           FROM accepted_input_content_part",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        stored_lengths,
        vec![expected_length, expected_length],
        "the schema must admit the domain's exact maximum"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The submit-command satellite refuses content one byte above the domain
/// maximum even when SQL bypasses domain construction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn submit_command_schema_rejects_content_above_maximum() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let above_bound = "a".repeat(persist_at_bound_content(&pool).await? + 1);

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, 'submit_input', 3, transaction_timestamp(), 'operator')",
    )
    .bind(Uuid::from_u128(0x323))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position)
         SELECT
             $1, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position
           FROM submit_input_command
          WHERE command_id = $2",
    )
    .bind(Uuid::from_u128(0x323))
    .bind(Uuid::from_u128(0x322))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO submit_input_command_content_part
            (command_id, position, part_kind, text_value)
         VALUES ($1, 0, 'text', $2)",
    )
    .bind(Uuid::from_u128(0x323))
    .bind(above_bound)
    .execute(&mut *transaction)
    .await?;
    let command_error =
        sqlx::query("SET CONSTRAINTS submit_input_command_content_parts_are_valid IMMEDIATE")
            .execute(&mut *transaction)
            .await
            .expect_err("the schema refuses command content one byte over the bound");
    let database_error = command_error
        .as_database_error()
        .expect("a check violation is a database error");
    assert_eq!(database_error.code(), Some("23514".into()));
    assert_eq!(
        database_error.constraint(),
        Some("submit_input_command_content_parts_valid")
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

/// The accepted-input satellite refuses content one byte above the domain
/// maximum even when SQL bypasses domain construction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn accepted_input_schema_rejects_content_above_maximum() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let above_bound = "a".repeat(persist_at_bound_content(&pool).await? + 1);

    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO accepted_input
            (accepted_input_id, accepting_command_id, session_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             acceptance_position, disposition_kind, origin_turn_id)
         SELECT
             $1, NULL, session_id,
             delivery_kind,
             expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             $2, disposition_kind, $3
           FROM accepted_input
          WHERE accepted_input_id = $4",
    )
    .bind(Uuid::from_u128(0x922))
    .bind(Decimal::TWO)
    .bind(Uuid::from_u128(0xa22))
    .bind(Uuid::from_u128(0x921))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO accepted_input_content_part
            (accepted_input_id, position, part_kind, text_value)
         VALUES ($1, 0, 'text', $2)",
    )
    .bind(Uuid::from_u128(0x922))
    .bind(above_bound)
    .execute(&mut *transaction)
    .await?;
    let accepted_error =
        sqlx::query("SET CONSTRAINTS accepted_input_content_parts_are_valid IMMEDIATE")
            .execute(&mut *transaction)
            .await
            .expect_err("the schema refuses accepted content one byte over the bound");
    let database_error = accepted_error
        .as_database_error()
        .expect("a check violation is a database error");
    assert_eq!(database_error.code(), Some("23514".into()));
    assert_eq!(
        database_error.constraint(),
        Some("accepted_input_content_parts_valid")
    );
    transaction.rollback().await?;

    pool.close().await;
    drop(container);
    Ok(())
}

struct MultipartReplayFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: SubmitInputRepository,
    command: SubmitInput,
    first: SubmitInputHandlingOutcome,
}

const MULTIPART_SESSION_COMMAND_ID: u128 = 0x324;
const MULTIPART_SESSION_ID: u128 = 0x724;
const MULTIPART_MODEL_SELECTION_ID: u128 = 0x824;
const MULTIPART_BLOB_NAMESPACE_ID: u128 = 0xb24;
const MULTIPART_SUBMIT_COMMAND_ID: u128 = 0x325;
const MULTIPART_ACCEPTED_INPUT_ID: u128 = 0x925;
const MULTIPART_TURN_ID: u128 = 0xa25;
const MULTIPART_REPLAY_ACCEPTED_INPUT_ID: u128 = 0x926;
const MULTIPART_REPLAY_TURN_ID: u128 = 0xa26;
const MULTIPART_REORDERED_ACCEPTED_INPUT_ID: u128 = 0x927;
const MULTIPART_REORDERED_TURN_ID: u128 = 0xa27;
const MULTIPART_METADATA_ACCEPTED_INPUT_ID: u128 = 0x928;
const MULTIPART_METADATA_TURN_ID: u128 = 0xa28;
const MULTIPART_ATTACHMENT_PAYLOAD: &[u8] = b"multipart attachment";
const MULTIPART_ATTACHMENT_MAXIMUM_BYTES: u64 = 1_024;
const MULTIPART_BLOB_STORE_NAME: &str = "multipart_test";
const MULTIPART_BLOB_OBJECT_KEY: &str = "multipart/object";

fn multipart_input_choices() -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::UseSessionDefault,
    )
}

#[derive(sqlx::FromRow)]
struct MultipartProjectionFacts {
    command_projection: Value,
    accepted_projection: Value,
}

impl MultipartReplayFixture {
    async fn finish(self) {
        self.pool.close().await;
        drop(self.container);
    }
}

async fn multipart_replay_fixture(
    command: SubmitInput,
    attachment_payload: &[u8],
) -> Result<MultipartReplayFixture, Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            MULTIPART_SESSION_COMMAND_ID,
            MULTIPART_SESSION_ID,
            direct(MULTIPART_MODEL_SELECTION_ID),
        ))
        .await?;

    let digest = BlobDigest::digest(attachment_payload);
    let mut catalog = pool.begin().await?;
    sqlx::query(
        "INSERT INTO blob_store_binding (store_name, namespace_id)
         VALUES ($1, $2)",
    )
    .bind(MULTIPART_BLOB_STORE_NAME)
    .bind(Uuid::from_u128(MULTIPART_BLOB_NAMESPACE_ID))
    .execute(&mut *catalog)
    .await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, $2)")
        .bind(digest.as_bytes().as_slice())
        .bind(Decimal::from(attachment_payload.len()))
        .execute(&mut *catalog)
        .await?;
    sqlx::query(
        "INSERT INTO blob_replica (digest, store_name, object_key)
         VALUES ($1, $2, $3)",
    )
    .bind(digest.as_bytes().as_slice())
    .bind(MULTIPART_BLOB_STORE_NAME)
    .bind(MULTIPART_BLOB_OBJECT_KEY)
    .execute(&mut *catalog)
    .await?;
    catalog.commit().await?;

    let repository = SubmitInputRepository::new(pool.clone())
        .with_attachment_maximum_bytes(MULTIPART_ATTACHMENT_MAXIMUM_BYTES);
    let first = repository
        .handle(
            command.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(MULTIPART_ACCEPTED_INPUT_ID)),
            Some(TurnId::from_uuid(Uuid::from_u128(MULTIPART_TURN_ID))),
        )
        .await?;
    match &first {
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_),
        )) => {}
        SubmitInputHandlingOutcome::Recorded(
            SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
            | SubmitInputResult::Rejected(_),
        )
        | SubmitInputHandlingOutcome::ConflictingReuse { .. } => panic!(
            "the multipart fixture submission must record a turn-origin acceptance, not {first:?}"
        ),
    }

    Ok(MultipartReplayFixture {
        container,
        pool,
        repository,
        command,
        first,
    })
}

/// equal multipart replay returns the original durable acceptance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn equal_multipart_submit_replay_returns_the_original_receipt() -> Result<(), Box<dyn Error>>
{
    let digest = BlobDigest::digest(MULTIPART_ATTACHMENT_PAYLOAD);
    let before = UserContentPart::try_text(String::from("before"))
        .expect("the fixture leading text is valid");
    let attachment = UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: Some(
            AttachmentDisplayFilename::try_new(String::from("notes.pdf"))
                .expect("the fixture display filename is valid"),
        ),
    };
    let after = UserContentPart::try_text(String::from("after"))
        .expect("the fixture trailing text is valid");
    let content = UserContent::try_parts(vec![before, attachment, after])
        .expect("the fixture parts are canonical");
    let command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(MULTIPART_SUBMIT_COMMAND_ID)),
        SessionId::from_uuid(Uuid::from_u128(MULTIPART_SESSION_ID)),
        content,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: multipart_input_choices(),
        },
    );
    let fixture = multipart_replay_fixture(command, MULTIPART_ATTACHMENT_PAYLOAD).await?;
    assert_eq!(
        fixture
            .repository
            .handle(
                fixture.command.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(MULTIPART_REPLAY_ACCEPTED_INPUT_ID)),
                Some(TurnId::from_uuid(Uuid::from_u128(MULTIPART_REPLAY_TURN_ID))),
            )
            .await?,
        fixture.first
    );
    fixture.finish().await;
    Ok(())
}

/// loading a durable multipart command reconstructs its exact value.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn durable_multipart_command_reconstructs_the_original_value() -> Result<(), Box<dyn Error>> {
    let digest = BlobDigest::digest(MULTIPART_ATTACHMENT_PAYLOAD);
    let before = UserContentPart::try_text(String::from("before"))
        .expect("the fixture leading text is valid");
    let attachment = UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: Some(
            AttachmentDisplayFilename::try_new(String::from("notes.pdf"))
                .expect("the fixture display filename is valid"),
        ),
    };
    let after = UserContentPart::try_text(String::from("after"))
        .expect("the fixture trailing text is valid");
    let content = UserContent::try_parts(vec![before, attachment, after])
        .expect("the fixture parts are canonical");
    let command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(MULTIPART_SUBMIT_COMMAND_ID)),
        SessionId::from_uuid(Uuid::from_u128(MULTIPART_SESSION_ID)),
        content,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: multipart_input_choices(),
        },
    );
    let fixture = multipart_replay_fixture(command, MULTIPART_ATTACHMENT_PAYLOAD).await?;
    assert_eq!(
        fixture
            .repository
            .load(fixture.command.command_id())
            .await?
            .expect("the multipart command is complete")
            .command(),
        &fixture.command
    );
    fixture.finish().await;
    Ok(())
}

/// multipart part order participates in durable replay equality.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reordered_multipart_submit_is_conflicting_reuse() -> Result<(), Box<dyn Error>> {
    let digest = BlobDigest::digest(MULTIPART_ATTACHMENT_PAYLOAD);
    let before = UserContentPart::try_text(String::from("before"))
        .expect("the fixture leading text is valid");
    let attachment = UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: Some(
            AttachmentDisplayFilename::try_new(String::from("notes.pdf"))
                .expect("the fixture display filename is valid"),
        ),
    };
    let after = UserContentPart::try_text(String::from("after"))
        .expect("the fixture trailing text is valid");
    let content = UserContent::try_parts(vec![before.clone(), attachment.clone(), after.clone()])
        .expect("the fixture parts are canonical");
    let command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(MULTIPART_SUBMIT_COMMAND_ID)),
        SessionId::from_uuid(Uuid::from_u128(MULTIPART_SESSION_ID)),
        content,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: multipart_input_choices(),
        },
    );
    let fixture = multipart_replay_fixture(command, MULTIPART_ATTACHMENT_PAYLOAD).await?;
    let reordered = SubmitInput::new(
        fixture.command.command_id(),
        fixture.command.session(),
        UserContent::try_parts(vec![after, attachment, before])
            .expect("the reordered fixture parts are canonical"),
        fixture.command.delivery(),
    );
    assert_eq!(
        fixture
            .repository
            .handle(
                reordered,
                AcceptedInputId::from_uuid(Uuid::from_u128(MULTIPART_REORDERED_ACCEPTED_INPUT_ID)),
                Some(TurnId::from_uuid(Uuid::from_u128(
                    MULTIPART_REORDERED_TURN_ID
                ))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: fixture.command.command_id(),
        }
    );
    fixture.finish().await;
    Ok(())
}

/// attachment display metadata participates in durable replay
/// equality.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_attachment_metadata_is_conflicting_reuse() -> Result<(), Box<dyn Error>> {
    let digest = BlobDigest::digest(MULTIPART_ATTACHMENT_PAYLOAD);
    let before = UserContentPart::try_text(String::from("before"))
        .expect("the fixture leading text is valid");
    let attachment = UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: Some(
            AttachmentDisplayFilename::try_new(String::from("notes.pdf"))
                .expect("the fixture display filename is valid"),
        ),
    };
    let after = UserContentPart::try_text(String::from("after"))
        .expect("the fixture trailing text is valid");
    let content = UserContent::try_parts(vec![before.clone(), attachment, after.clone()])
        .expect("the fixture parts are canonical");
    let command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(MULTIPART_SUBMIT_COMMAND_ID)),
        SessionId::from_uuid(Uuid::from_u128(MULTIPART_SESSION_ID)),
        content,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: multipart_input_choices(),
        },
    );
    let fixture = multipart_replay_fixture(command, MULTIPART_ATTACHMENT_PAYLOAD).await?;
    let changed_attachment = UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: Some(
            AttachmentDisplayFilename::try_new(String::from("changed.pdf"))
                .expect("the changed fixture filename is valid"),
        ),
    };
    let changed_metadata = SubmitInput::new(
        fixture.command.command_id(),
        fixture.command.session(),
        UserContent::try_parts(vec![before, changed_attachment, after])
            .expect("the changed-metadata fixture parts are canonical"),
        fixture.command.delivery(),
    );
    assert_eq!(
        fixture
            .repository
            .handle(
                changed_metadata,
                AcceptedInputId::from_uuid(Uuid::from_u128(MULTIPART_METADATA_ACCEPTED_INPUT_ID)),
                Some(TurnId::from_uuid(Uuid::from_u128(
                    MULTIPART_METADATA_TURN_ID
                ))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: fixture.command.command_id(),
        }
    );
    fixture.finish().await;
    Ok(())
}

/// the command and accepted-input satellites mirror the exact ordered
/// multipart projection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn multipart_command_and_accepted_satellites_are_identical() -> Result<(), Box<dyn Error>> {
    let digest = BlobDigest::digest(MULTIPART_ATTACHMENT_PAYLOAD);
    let before = UserContentPart::try_text(String::from("before"))
        .expect("the fixture leading text is valid");
    let attachment = UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: Some(
            AttachmentDisplayFilename::try_new(String::from("notes.pdf"))
                .expect("the fixture display filename is valid"),
        ),
    };
    let after = UserContentPart::try_text(String::from("after"))
        .expect("the fixture trailing text is valid");
    let content = UserContent::try_parts(vec![before, attachment, after])
        .expect("the fixture parts are canonical");
    let command = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(MULTIPART_SUBMIT_COMMAND_ID)),
        SessionId::from_uuid(Uuid::from_u128(MULTIPART_SESSION_ID)),
        content,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: multipart_input_choices(),
        },
    );
    let fixture = multipart_replay_fixture(command, MULTIPART_ATTACHMENT_PAYLOAD).await?;
    let mirrored: MultipartProjectionFacts = sqlx::query_as(
        "SELECT
            (SELECT jsonb_agg(
                jsonb_build_object(
                    'position', position,
                    'part_kind', part_kind,
                    'text_value', text_value,
                    'blob_digest', encode(blob_digest, 'hex'),
                    'attachment_kind', attachment_kind,
                    'declared_media_type', declared_media_type,
                    'display_filename', display_filename)
                ORDER BY position)
               FROM submit_input_command_content_part
              WHERE command_id = $1) AS command_projection,
            (SELECT jsonb_agg(
                jsonb_build_object(
                    'position', position,
                    'part_kind', part_kind,
                    'text_value', text_value,
                    'blob_digest', encode(blob_digest, 'hex'),
                    'attachment_kind', attachment_kind,
                    'declared_media_type', declared_media_type,
                    'display_filename', display_filename)
                ORDER BY position)
               FROM accepted_input_content_part AS part
               JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = part.accepted_input_id
              WHERE accepted.accepting_command_id = $1) AS accepted_projection",
    )
    .bind(fixture.command.command_id().as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(mirrored.command_projection, mirrored.accepted_projection);

    fixture.finish().await;
    Ok(())
}

/// Catalogues one blob identity with a verified replica in its own store
/// binding, which is the only committed shape an admission check can observe as
/// available.
async fn catalog_verified_blob(
    pool: &PgPool,
    digest: BlobDigest,
    byte_length: u64,
    store_name: &str,
    namespace_id: Uuid,
    object_key: &str,
) -> Result<(), Box<dyn Error>> {
    let mut catalog = pool.begin().await?;
    sqlx::query("INSERT INTO blob_store_binding (store_name, namespace_id) VALUES ($1, $2)")
        .bind(store_name)
        .bind(namespace_id)
        .execute(&mut *catalog)
        .await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, $2)")
        .bind(digest.as_bytes().as_slice())
        .bind(Decimal::from(byte_length))
        .execute(&mut *catalog)
        .await?;
    sqlx::query("INSERT INTO blob_replica (digest, store_name, object_key) VALUES ($1, $2, $3)")
        .bind(digest.as_bytes().as_slice())
        .bind(store_name)
        .bind(object_key)
        .execute(&mut *catalog)
        .await?;
    catalog.commit().await?;
    Ok(())
}

struct UnknownAttachmentFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: SubmitInputRepository,
    command: SubmitInput,
    expected_result: SubmitInputResult,
    digest: BlobDigest,
    changed_digest: BlobDigest,
}

impl UnknownAttachmentFixture {
    async fn finish(self) {
        self.pool.close().await;
        drop(self.container);
    }
}

async fn unknown_attachment_fixture() -> Result<UnknownAttachmentFixture, Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let digest = BlobDigest::digest(b"unknown attachment");
    let changed_digest = BlobDigest::digest(b"changed attachment");
    let session = SessionId::from_uuid(Uuid::from_u128(0xb310));
    let command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xb311));
    let command = SubmitInput::new(
        command_id,
        session,
        attachment_content(digest),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let repository = SubmitInputRepository::new(pool.clone()).with_attachment_maximum_bytes(1024);
    let expected_result =
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentBlobNotFound { digest });
    Ok(UnknownAttachmentFixture {
        container,
        pool,
        repository,
        command,
        expected_result,
        digest,
        changed_digest,
    })
}

/// an attachment with no catalogued blob identity is
/// rejected only after the durable command identity is claimed, and the
/// unavailable digest is the recorded evidence. A committed catalogued
/// identity always carries a verified replica, because the deferred
/// `blob_requires_replica` constraint trigger rejects any commit without one
/// and the catalog tables are append-only, so an absent `blob` row is the only
/// unavailability an admission check can observe.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unknown_attachment_is_a_post_claim_rejection() -> Result<(), Box<dyn Error>> {
    let fixture = unknown_attachment_fixture().await?;
    assert_eq!(
        fixture
            .repository
            .handle(
                fixture.command.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb312)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb313))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(fixture.expected_result.clone())
    );
    let durable: (i16, String, Vec<u8>) = sqlx::query_as(
        "SELECT storage_version, rejection_kind, result_attachment_digest
           FROM submit_input_command WHERE command_id = $1",
    )
    .bind(fixture.command.command_id().as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        durable,
        (
            3,
            String::from("attachment_blob_not_found"),
            fixture.digest.as_bytes().to_vec()
        )
    );
    fixture.finish().await;
    Ok(())
}

/// an unknown-attachment rejection replays exactly. The digest is
/// catalogued with a verified replica between the two calls, so revalidation
/// would now admit the command and only durable replay returns the rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unknown_attachment_rejection_replays_exactly() -> Result<(), Box<dyn Error>> {
    let fixture = unknown_attachment_fixture().await?;
    let first = fixture
        .repository
        .handle(
            fixture.command.clone(),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb312)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb313))),
        )
        .await?;
    assert_eq!(
        first,
        SubmitInputHandlingOutcome::Recorded(fixture.expected_result.clone())
    );
    catalog_verified_blob(
        &fixture.pool,
        fixture.digest,
        16,
        "replayed_test",
        Uuid::from_u128(0xb31a),
        "replayed",
    )
    .await?;
    assert_eq!(
        fixture
            .repository
            .handle(
                fixture.command.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb314)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb315))),
            )
            .await?,
        first
    );
    fixture.finish().await;
    Ok(())
}

/// the durable unknown-attachment rejection reconstitutes exactly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn unknown_attachment_rejection_reconstitutes_exactly() -> Result<(), Box<dyn Error>> {
    let fixture = unknown_attachment_fixture().await?;
    assert_eq!(
        fixture
            .repository
            .handle(
                fixture.command.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb312)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb313))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(fixture.expected_result.clone())
    );
    assert_eq!(
        fixture
            .repository
            .load(fixture.command.command_id())
            .await?
            .expect("the rejected command remains complete")
            .result(),
        &fixture.expected_result
    );
    fixture.finish().await;
    Ok(())
}

/// correcting an unknown attachment to a catalogued one under the
/// claimed identity is conflicting reuse, not a second admission.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn changed_unknown_attachment_is_conflicting_reuse() -> Result<(), Box<dyn Error>> {
    let fixture = unknown_attachment_fixture().await?;
    assert_eq!(
        fixture
            .repository
            .handle(
                fixture.command.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb312)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb313))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(fixture.expected_result.clone())
    );
    catalog_verified_blob(
        &fixture.pool,
        fixture.changed_digest,
        16,
        "changed_test",
        Uuid::from_u128(0xb318),
        "changed",
    )
    .await?;
    let changed = SubmitInput::new(
        fixture.command.command_id(),
        fixture.command.session(),
        attachment_content(fixture.changed_digest),
        fixture.command.delivery(),
    );
    assert_eq!(
        fixture
            .repository
            .handle(
                changed,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb316)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb317))),
            )
            .await?,
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: fixture.command.command_id(),
        }
    );
    fixture.finish().await;
    Ok(())
}

struct AttachmentBudgetFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: SubmitInputRepository,
    first_part: UserContentPart,
    /// A second reference to `first_part`'s digest carrying different
    /// attachment metadata, so a repeated digest cannot be mistaken for a
    /// repeated part value.
    repeated_first_part: UserContentPart,
    second_part: UserContentPart,
    /// A third catalogued digest whose length completes `first_part`'s to
    /// exactly `maximum`, so admission at the bound is observable.
    completing_part: UserContentPart,
    session: SessionId,
    delivery: DeliveryRequest,
    first_length: u64,
    maximum: u64,
}

impl AttachmentBudgetFixture {
    async fn finish(self) {
        self.pool.close().await;
        drop(self.container);
    }
}

async fn attachment_budget_fixture() -> Result<AttachmentBudgetFixture, Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_digest = BlobDigest::digest(b"first attachment");
    let second_digest = BlobDigest::digest(b"second attachment");
    let completing_digest = BlobDigest::digest(b"completing attachment");
    // Each catalogued length is admissible on its own and only their sum
    // exceeds the maximum, so admission has to aggregate rather than compare
    // lengths one at a time. Doubling the first length also exceeds the
    // maximum, so counting one digest twice cannot pass either. The completing
    // length brings the first to exactly the maximum, which the spec's "must
    // not exceed" admits, so a `>=` comparison is observable.
    let first_length = 16_u64;
    let second_length = 12_u64;
    let maximum = 20_u64;
    let completing_length = maximum - first_length;
    let mut catalog = pool.begin().await?;
    sqlx::query(
        "INSERT INTO blob_store_binding (store_name, namespace_id)
         VALUES ('attachment_test', $1)",
    )
    .bind(Uuid::from_u128(0xb320))
    .execute(&mut *catalog)
    .await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, $2), ($3, $4), ($5, $6)")
        .bind(first_digest.as_bytes().as_slice())
        .bind(Decimal::from(first_length))
        .bind(second_digest.as_bytes().as_slice())
        .bind(Decimal::from(second_length))
        .bind(completing_digest.as_bytes().as_slice())
        .bind(Decimal::from(completing_length))
        .execute(&mut *catalog)
        .await?;
    sqlx::query(
        "INSERT INTO blob_replica (digest, store_name, object_key)
         VALUES ($1, 'attachment_test', 'first'),
                ($2, 'attachment_test', 'second'),
                ($3, 'attachment_test', 'completing')",
    )
    .bind(first_digest.as_bytes().as_slice())
    .bind(second_digest.as_bytes().as_slice())
    .bind(completing_digest.as_bytes().as_slice())
    .execute(&mut *catalog)
    .await?;
    catalog.commit().await?;

    let session = SessionId::from_uuid(Uuid::from_u128(0xb321));
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
    };
    let repository =
        SubmitInputRepository::new(pool.clone()).with_attachment_maximum_bytes(maximum);
    Ok(AttachmentBudgetFixture {
        container,
        pool,
        repository,
        first_part: attachment_part(first_digest),
        repeated_first_part: UserContentPart::Attachment {
            digest: first_digest,
            kind: AttachmentKind::Document,
            media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
                .expect("the fixture media type is valid"),
            display_filename: Some(
                AttachmentDisplayFilename::try_new(String::from("second-reference.pdf"))
                    .expect("the fixture display filename is valid"),
            ),
        },
        second_part: attachment_part(second_digest),
        completing_part: attachment_part(completing_digest),
        session,
        delivery,
        first_length,
        maximum,
    })
}

fn distinct_attachment_command(
    fixture: &AttachmentBudgetFixture,
    command_id: DurableCommandId,
) -> SubmitInput {
    SubmitInput::new(
        command_id,
        fixture.session,
        UserContent::try_parts(vec![
            fixture.first_part.clone(),
            fixture.second_part.clone(),
        ])
        .expect("the distinct fixture content is canonical"),
        fixture.delivery,
    )
}

fn repeated_attachment_command(
    fixture: &AttachmentBudgetFixture,
    command_id: DurableCommandId,
) -> SubmitInput {
    SubmitInput::new(
        command_id,
        fixture.session,
        UserContent::try_parts(vec![
            fixture.first_part.clone(),
            fixture.repeated_first_part.clone(),
        ])
        .expect("the repeated fixture content is canonical"),
        fixture.delivery,
    )
}

/// the digest is the accounting key, so two metadata-distinct parts
/// naming one catalogued digest consume its length only once and reach session
/// lookup rather than the byte-budget rejection.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attachment_admission_counts_a_repeated_digest_once() -> Result<(), Box<dyn Error>> {
    let fixture = attachment_budget_fixture().await?;
    let repeated = repeated_attachment_command(
        &fixture,
        DurableCommandId::from_uuid(Uuid::from_u128(0xb322)),
    );
    assert_eq!(
        fixture
            .repository
            .handle(
                repeated,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb323)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb324))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionNotFound {
                session: fixture.session,
            }
        ))
    );
    fixture.finish().await;
    Ok(())
}

/// a repeated digest is charged once rather than not at all, so the
/// same two metadata-distinct parts are rejected under a maximum below their
/// one catalogued length.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attachment_admission_charges_a_repeated_digest_at_least_once() -> Result<(), Box<dyn Error>>
{
    let fixture = attachment_budget_fixture().await?;
    let narrow_maximum = fixture.first_length - 1;
    let narrow = SubmitInputRepository::new(fixture.pool.clone())
        .with_attachment_maximum_bytes(narrow_maximum);
    let repeated = repeated_attachment_command(
        &fixture,
        DurableCommandId::from_uuid(Uuid::from_u128(0xb32a)),
    );
    assert_eq!(
        narrow
            .handle(
                repeated,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb32b)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb32c))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: narrow_maximum,
            }
        ))
    );
    fixture.finish().await;
    Ok(())
}

/// the bound is "must not exceed", so distinct catalogued digests
/// summing to exactly the maximum are admitted and reach session lookup.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attachment_bytes_equal_to_the_maximum_are_admitted() -> Result<(), Box<dyn Error>> {
    let fixture = attachment_budget_fixture().await?;
    let exact = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb32d)),
        fixture.session,
        UserContent::try_parts(vec![
            fixture.first_part.clone(),
            fixture.completing_part.clone(),
        ])
        .expect("the completing fixture content is canonical"),
        fixture.delivery,
    );
    assert_eq!(
        fixture
            .repository
            .handle(
                exact,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb32e)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb32f))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionNotFound {
                session: fixture.session,
            }
        ))
    );
    fixture.finish().await;
    Ok(())
}

/// distinct catalogued digests, each admissible alone, are rejected
/// once their summed lengths pass the deployment maximum, and that maximum is
/// the durable evidence.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn distinct_attachment_bytes_above_the_maximum_are_rejected() -> Result<(), Box<dyn Error>> {
    let fixture = attachment_budget_fixture().await?;
    let distinct_command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xb325));
    let distinct = distinct_attachment_command(&fixture, distinct_command_id);
    assert_eq!(
        fixture
            .repository
            .handle(
                distinct,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb326)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb327))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: fixture.maximum,
            }
        ))
    );
    let durable_maximum: Decimal = sqlx::query_scalar(
        "SELECT result_attachment_maximum_bytes
           FROM submit_input_command WHERE command_id = $1",
    )
    .bind(distinct_command_id.as_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(durable_maximum, Decimal::from(fixture.maximum));
    fixture.finish().await;
    Ok(())
}

/// the attachment-byte-bound rejection replays exactly. The replay
/// runs under a maximum that now admits the same aggregate, so revalidation
/// would return acceptance and only durable replay returns the first maximum.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn attachment_byte_bound_rejection_replays_exactly() -> Result<(), Box<dyn Error>> {
    let fixture = attachment_budget_fixture().await?;
    let distinct = distinct_attachment_command(
        &fixture,
        DurableCommandId::from_uuid(Uuid::from_u128(0xb325)),
    );
    let expected = SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
            maximum_bytes: fixture.maximum,
        },
    ));
    assert_eq!(
        fixture
            .repository
            .handle(
                distinct.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb326)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb327))),
            )
            .await?,
        expected
    );
    let widened = SubmitInputRepository::new(fixture.pool.clone())
        .with_attachment_maximum_bytes(fixture.maximum * 4);
    assert_eq!(
        widened
            .handle(
                distinct,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb328)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb329))),
            )
            .await?,
        expected
    );
    fixture.finish().await;
    Ok(())
}

struct QueuedFrontierFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: SubmitInputRepository,
    session: SessionId,
    first_digest: BlobDigest,
    second_digest: BlobDigest,
    maximum: u64,
}

impl QueuedFrontierFixture {
    async fn finish(self) {
        self.pool.close().await;
        drop(self.container);
    }
}

async fn queued_frontier_fixture() -> Result<QueuedFrontierFixture, Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first_digest = BlobDigest::digest(b"first prospective attachment");
    let second_digest = BlobDigest::digest(b"second prospective attachment");
    let maximum = 10_u64;
    let mut catalog = pool.begin().await?;
    sqlx::query(
        "INSERT INTO blob_store_binding (store_name, namespace_id)
         VALUES ('prospective_queue', $1)",
    )
    .bind(Uuid::from_u128(0xb330))
    .execute(&mut *catalog)
    .await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, 7), ($2, 7)")
        .bind(first_digest.as_bytes().as_slice())
        .bind(second_digest.as_bytes().as_slice())
        .execute(&mut *catalog)
        .await?;
    sqlx::query(
        "INSERT INTO blob_replica (digest, store_name, object_key)
         VALUES ($1, 'prospective_queue', 'first'),
                ($2, 'prospective_queue', 'second')",
    )
    .bind(first_digest.as_bytes().as_slice())
    .bind(second_digest.as_bytes().as_slice())
    .execute(&mut *catalog)
    .await?;
    catalog.commit().await?;

    let session = SessionId::from_uuid(Uuid::from_u128(0xb331));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xb332, 0xb331, direct(0xb333)))
        .await?;
    let repository =
        SubmitInputRepository::new(pool.clone()).with_attachment_maximum_bytes(maximum);
    Ok(QueuedFrontierFixture {
        container,
        pool,
        repository,
        session,
        first_digest,
        second_digest,
        maximum,
    })
}

/// a newly queued input is rejected when the complete prospective
/// rendered frontier, rather than either input alone, exceeds the attachment
/// verification bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_input_checks_the_complete_prospective_attachment_frontier()
-> Result<(), Box<dyn Error>> {
    let fixture = queued_frontier_fixture().await?;
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
    };
    let first = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb334)),
        fixture.session,
        attachment_content(fixture.first_digest),
        delivery,
    );
    assert!(
        matches!(
            fixture
                .repository
                .handle(
                    first,
                    AcceptedInputId::from_uuid(Uuid::from_u128(0xb335)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xb336))),
                )
                .await?,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the first seven-byte attachment must remain within the ten-byte bound"
    );
    let second = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb337)),
        fixture.session,
        attachment_content(fixture.second_digest),
        delivery,
    );

    assert_eq!(
        fixture
            .repository
            .handle(
                second,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb338)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb339))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: fixture.maximum,
            },
        ))
    );
    fixture.finish().await;
    Ok(())
}

/// a prospective queued-frontier rejection replays exactly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_frontier_rejection_replays_exactly() -> Result<(), Box<dyn Error>> {
    let fixture = queued_frontier_fixture().await?;
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
    };
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb334)),
                fixture.session,
                attachment_content(fixture.first_digest),
                delivery,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb335)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb336))),
        )
        .await?;
    let rejected = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb337)),
        fixture.session,
        attachment_content(fixture.second_digest),
        delivery,
    );
    let expected = SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
            maximum_bytes: fixture.maximum,
        },
    ));
    assert_eq!(
        fixture
            .repository
            .handle(
                rejected.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb338)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb339))),
            )
            .await?,
        expected
    );
    assert_eq!(
        fixture
            .repository
            .handle(
                rejected,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb33a)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb33b))),
            )
            .await?,
        expected
    );
    fixture.finish().await;
    Ok(())
}

/// the frontier sum is over distinct digests, so one digest referenced
/// by both the rendered origin and a newly queued input is charged once and the
/// queued input is admitted, even though doubling that length would exceed the
/// bound.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn prospective_frontier_charges_a_shared_digest_once() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let shared_digest = BlobDigest::digest(b"shared prospective attachment");
    let shared_length = 7_u64;
    let maximum = 10_u64;
    catalog_verified_blob(
        &pool,
        shared_digest,
        shared_length,
        "prospective_shared",
        Uuid::from_u128(0xb350),
        "shared",
    )
    .await?;

    let session = SessionId::from_uuid(Uuid::from_u128(0xb351));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xb352, 0xb351, direct(0xb353)))
        .await?;
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
    };
    let repository =
        SubmitInputRepository::new(pool.clone()).with_attachment_maximum_bytes(maximum);
    assert!(matches!(
        repository
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(0xb354)),
                    session,
                    attachment_content(shared_digest),
                    delivery,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb355)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb356))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));
    // The same digest carried by different attachment metadata, so a shared
    // digest cannot be mistaken for a repeated input value.
    let restated = UserContent::try_parts(vec![UserContentPart::Attachment {
        digest: shared_digest,
        kind: AttachmentKind::Document,
        media_type: DeclaredMediaType::try_new(String::from("application/pdf"))
            .expect("the fixture media type is valid"),
        display_filename: Some(
            AttachmentDisplayFilename::try_new(String::from("restated.pdf"))
                .expect("the fixture display filename is valid"),
        ),
    }])
    .expect("the restated fixture content is canonical");
    assert!(2 * shared_length > maximum);
    assert!(matches!(
        repository
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(0xb357)),
                    session,
                    restated,
                    delivery,
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb358)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb359))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
            SubmitInputAppliedResult::TurnOrigin(_)
        ))
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

/// retained attachment evidence never outlives the rejection that
/// authorizes it. Dropping the rejection kind while the maximum stands leaves
/// both named-rejection comparisons null, so the shape is asserted with
/// `IS TRUE` and rejects the row rather than admitting an unreadable one. The
/// append-only guard is suspended inside a transaction this test rolls back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retained_attachment_maximum_requires_its_rejection_kind() -> Result<(), Box<dyn Error>> {
    let fixture = attachment_budget_fixture().await?;
    let rejected_command_id = DurableCommandId::from_uuid(Uuid::from_u128(0xb325));
    assert_eq!(
        fixture
            .repository
            .handle(
                distinct_attachment_command(&fixture, rejected_command_id),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb326)),
                Some(TurnId::from_uuid(Uuid::from_u128(0xb327))),
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: fixture.maximum,
            }
        ))
    );
    let mut orphaned_maximum = fixture.pool.begin().await?;
    sqlx::query("ALTER TABLE submit_input_command DISABLE TRIGGER USER")
        .execute(&mut *orphaned_maximum)
        .await?;
    let orphaned_maximum_error = sqlx::query(
        "UPDATE submit_input_command
            SET rejection_kind = NULL
          WHERE command_id = $1",
    )
    .bind(rejected_command_id.as_uuid())
    .execute(&mut *orphaned_maximum)
    .await
    .expect_err("a retained attachment maximum cannot outlive its rejection");
    assert_eq!(
        orphaned_maximum_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("submit_input_command_attachment_result_evidence_shape")
    );
    orphaned_maximum.rollback().await?;
    fixture.finish().await;
    Ok(())
}

/// a rejected queued frontier rolls back every provisional accepted
/// input and queue-origin effect.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn queued_frontier_rejection_rolls_back_provisional_effects() -> Result<(), Box<dyn Error>> {
    let fixture = queued_frontier_fixture().await?;
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
    };
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb334)),
                fixture.session,
                attachment_content(fixture.first_digest),
                delivery,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb335)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb336))),
        )
        .await?;
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb337)),
                fixture.session,
                attachment_content(fixture.second_digest),
                delivery,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb338)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb339))),
        )
        .await?;
    #[derive(sqlx::FromRow)]
    struct QueuedFrontierEffectCounts {
        accepted_inputs: i64,
        queued_origins: i64,
    }
    let effects = sqlx::query_as::<_, QueuedFrontierEffectCounts>(
        "SELECT (SELECT count(*) FROM accepted_input WHERE session_id = $1)
                    AS accepted_inputs,
                (SELECT count(*) FROM queued_input_origin WHERE session_id = $1)
                    AS queued_origins",
    )
    .bind(fixture.session.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(effects.accepted_inputs, 1);
    assert_eq!(effects.queued_origins, 1);

    fixture.finish().await;
    Ok(())
}

struct SteeringFrontierFixture {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    repository: SubmitInputRepository,
    session: SessionId,
    active_turn: TurnId,
    queued_digest: BlobDigest,
    later_queued_digest: BlobDigest,
    steering_digest: BlobDigest,
    maximum: u64,
}

impl SteeringFrontierFixture {
    async fn finish(self) {
        self.pool.close().await;
        drop(self.container);
    }
}

async fn steering_frontier_fixture() -> Result<SteeringFrontierFixture, Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let queued_digest = BlobDigest::digest(b"queued prospective attachment");
    let later_queued_digest = BlobDigest::digest(b"later queued prospective attachment");
    let steering_digest = BlobDigest::digest(b"steering prospective attachment");
    // Each catalogued attachment is seven bytes. Before steering the queue
    // totals fourteen; steering adds a third to the rendered base, so the
    // earlier successor reaches fourteen and only the later one reaches
    // twenty-one.
    let maximum = 20_u64;
    let mut catalog = pool.begin().await?;
    sqlx::query(
        "INSERT INTO blob_store_binding (store_name, namespace_id)
         VALUES ('prospective_steering', $1)",
    )
    .bind(Uuid::from_u128(0xb340))
    .execute(&mut *catalog)
    .await?;
    sqlx::query("INSERT INTO blob (digest, byte_length) VALUES ($1, 7), ($2, 7), ($3, 7)")
        .bind(queued_digest.as_bytes().as_slice())
        .bind(later_queued_digest.as_bytes().as_slice())
        .bind(steering_digest.as_bytes().as_slice())
        .execute(&mut *catalog)
        .await?;
    sqlx::query(
        "INSERT INTO blob_replica (digest, store_name, object_key)
         VALUES ($1, 'prospective_steering', 'queued'),
                ($2, 'prospective_steering', 'later_queued'),
                ($3, 'prospective_steering', 'steering')",
    )
    .bind(queued_digest.as_bytes().as_slice())
    .bind(later_queued_digest.as_bytes().as_slice())
    .bind(steering_digest.as_bytes().as_slice())
    .execute(&mut *catalog)
    .await?;
    catalog.commit().await?;

    let session = SessionId::from_uuid(Uuid::from_u128(0xb341));
    let active_turn = TurnId::from_uuid(Uuid::from_u128(0xb342));
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0xb343, 0xb341, direct(0xb344)))
        .await?;
    let repository =
        SubmitInputRepository::new(pool.clone()).with_attachment_maximum_bytes(maximum);
    repository
        .handle(
            start_input(
                0xb345,
                0xb341,
                "active text origin",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb346)),
            Some(active_turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(0xb347),
            starting_frontier: Uuid::from_u128(0xb348),
            initial_attempt: Uuid::from_u128(0xb349),
        },
    )
    .await?;
    Ok(SteeringFrontierFixture {
        container,
        pool,
        repository,
        session,
        active_turn,
        queued_digest,
        later_queued_digest,
        steering_digest,
        maximum,
    })
}

/// pending steering is rejected when it would make a queued
/// successor's eventual rendered frontier exceed the attachment bound. Two
/// successors are queued in canonical order: after the steering transition the
/// earlier one's prospective frontier still fits and only the later one
/// exceeds the bound, so every affected queued frontier has to be recomputed
/// rather than just the first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_steering_rechecks_affected_queued_attachment_frontiers()
-> Result<(), Box<dyn Error>> {
    let fixture = steering_frontier_fixture().await?;
    let queued = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb34a)),
        fixture.session,
        attachment_content(fixture.queued_digest),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: fixture.active_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    assert!(
        matches!(
            fixture
                .repository
                .handle(
                    queued,
                    AcceptedInputId::from_uuid(Uuid::from_u128(0xb34b)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xb34c))),
                )
                .await?,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the queued seven-byte attachment must remain within the twenty-byte bound"
    );
    let later_queued = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb370)),
        fixture.session,
        attachment_content(fixture.later_queued_digest),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: fixture.active_turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    assert!(
        matches!(
            fixture
                .repository
                .handle(
                    later_queued,
                    AcceptedInputId::from_uuid(Uuid::from_u128(0xb371)),
                    Some(TurnId::from_uuid(Uuid::from_u128(0xb372))),
                )
                .await?,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the queued pair must total fourteen bytes, within the twenty-byte bound"
    );
    let steering = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb34d)),
        fixture.session,
        attachment_content(fixture.steering_digest),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: fixture.active_turn,
        },
    );

    assert_eq!(
        fixture
            .repository
            .handle(
                steering,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb34e)),
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: fixture.maximum,
            },
        ))
    );
    fixture.finish().await;
    Ok(())
}

/// a prospective steering-frontier rejection replays exactly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn steering_frontier_rejection_replays_exactly() -> Result<(), Box<dyn Error>> {
    let fixture = steering_frontier_fixture().await?;
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb34a)),
                fixture.session,
                attachment_content(fixture.queued_digest),
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: fixture.active_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb34b)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb34c))),
        )
        .await?;
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb370)),
                fixture.session,
                attachment_content(fixture.later_queued_digest),
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: fixture.active_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb371)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb372))),
        )
        .await?;
    let steering = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(0xb34d)),
        fixture.session,
        attachment_content(fixture.steering_digest),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: fixture.active_turn,
        },
    );
    let expected = SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
        SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
            maximum_bytes: fixture.maximum,
        },
    ));
    assert_eq!(
        fixture
            .repository
            .handle(
                steering.clone(),
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb34e)),
                None,
            )
            .await?,
        expected
    );
    assert_eq!(
        fixture
            .repository
            .handle(
                steering,
                AcceptedInputId::from_uuid(Uuid::from_u128(0xb34f)),
                None,
            )
            .await?,
        expected
    );
    fixture.finish().await;
    Ok(())
}

/// a rejected steering frontier rolls back the provisional pending
/// steering while preserving the active and queued origins.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn steering_frontier_rejection_rolls_back_provisional_effects() -> Result<(), Box<dyn Error>>
{
    let fixture = steering_frontier_fixture().await?;
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb34a)),
                fixture.session,
                attachment_content(fixture.queued_digest),
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: fixture.active_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb34b)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb34c))),
        )
        .await?;
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb370)),
                fixture.session,
                attachment_content(fixture.later_queued_digest),
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: fixture.active_turn,
                    configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb371)),
            Some(TurnId::from_uuid(Uuid::from_u128(0xb372))),
        )
        .await?;
    fixture
        .repository
        .handle(
            SubmitInput::new(
                DurableCommandId::from_uuid(Uuid::from_u128(0xb34d)),
                fixture.session,
                attachment_content(fixture.steering_digest),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: fixture.active_turn,
                },
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(0xb34e)),
            None,
        )
        .await?;
    #[derive(sqlx::FromRow)]
    struct SteeringFrontierEffectCounts {
        accepted_inputs: i64,
        queued_successor_origins: i64,
        pending_steering: i64,
    }
    let effects = sqlx::query_as::<_, SteeringFrontierEffectCounts>(
        "SELECT (SELECT count(*) FROM accepted_input WHERE session_id = $1)
                    AS accepted_inputs,
                (SELECT count(*) FROM queued_input_origin WHERE session_id = $1
                    AND turn_id <> $2) AS queued_successor_origins,
                (SELECT count(*) FROM accepted_input WHERE session_id = $1
                    AND disposition_kind = 'pending_steering') AS pending_steering",
    )
    .bind(fixture.session.into_uuid())
    .bind(fixture.active_turn.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(effects.accepted_inputs, 3);
    assert_eq!(effects.queued_successor_origins, 2);
    assert_eq!(effects.pending_steering, 0);

    fixture.finish().await;
    Ok(())
}

/// Base identity for the accepted-input executing-batch scenario. Arbitrary:
/// it only has to leave `checkpoint_tool_batch_with_approval`'s derived
/// identities distinct from every other fixture in this file.
const TOOL_BATCH_FIXTURE_SEED: u128 = 0xb360;
/// Base identity for the delegated executing-batch scenario. Arbitrary, and
/// far enough above [`TOOL_BATCH_FIXTURE_SEED`] that the two fixtures' derived
/// identities cannot meet.
const DELEGATED_BATCH_FIXTURE_SEED: u128 = 0xb380;
/// Seed offsets for identities these scenarios introduce. Each is arbitrary
/// and only has to be distinct from the fixture's own identities.
const STORE_NAMESPACE: u128 = 0x200;
const SECOND_STORE_NAMESPACE: u128 = 0x201;
const QUEUED_COMMAND: u128 = 0x202;
const QUEUED_ACCEPTED_INPUT: u128 = 0x203;
const QUEUED_TURN_CANDIDATE: u128 = 0x204;
const STEERING_COMMAND: u128 = 0x205;
const STEERING_ACCEPTED_INPUT: u128 = 0x206;
const LATER_STEERING_COMMAND: u128 = 0x207;
const LATER_STEERING_ACCEPTED_INPUT: u128 = 0x208;
const TOOL_REQUEST: u128 = 0x209;
const TOOL_CALL_ENTRY: u128 = 0x20a;
const YIELDED_FRONTIER: u128 = 0x20b;
const CONTINUATION_ATTEMPT: u128 = 0x20c;
/// Each catalogued attachment in these scenarios. Load-bearing: one fits under
/// [`TOOL_BATCH_ATTACHMENT_MAXIMUM`] and two do not.
const RETAINED_ATTACHMENT_LENGTH: u64 = 7;
const TOOL_BATCH_ATTACHMENT_MAXIMUM: u64 = 10;

/// a turn executing a tool batch keeps the `running` phase while the
/// call that produced the batch is already terminal, so prospective
/// attachment accounting reads the batch's yielded frontier instead of
/// rejecting the submission it cannot reconstitute.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn executing_tool_batch_admits_a_bounded_attachment_queue() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        TOOL_BATCH_FIXTURE_SEED,
        &[("current_time", "{}")],
        InitialToolApproval::PolicyAuto,
    )
    .await?;
    let digest = BlobDigest::digest(b"tool batch prospective attachment");
    catalog_verified_blob(
        &pool,
        digest,
        RETAINED_ATTACHMENT_LENGTH,
        "prospective_tool_batch",
        Uuid::from_u128(TOOL_BATCH_FIXTURE_SEED + STORE_NAMESPACE),
        "queued",
    )
    .await?;
    let queued = SubmitInput::new(
        DurableCommandId::from_uuid(Uuid::from_u128(TOOL_BATCH_FIXTURE_SEED + QUEUED_COMMAND)),
        fixture.session,
        attachment_content(digest),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: fixture.turn,
            configuration: input_choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );

    assert!(
        matches!(
            SubmitInputRepository::new(pool.clone())
                .with_attachment_maximum_bytes(TOOL_BATCH_ATTACHMENT_MAXIMUM)
                .handle(
                    queued,
                    AcceptedInputId::from_uuid(Uuid::from_u128(
                        TOOL_BATCH_FIXTURE_SEED + QUEUED_ACCEPTED_INPUT,
                    )),
                    Some(TurnId::from_uuid(Uuid::from_u128(
                        TOOL_BATCH_FIXTURE_SEED + QUEUED_TURN_CANDIDATE,
                    ))),
                )
                .await?,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            ))
        ),
        "the queued seven-byte attachment must remain within the ten-byte bound"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// a delegated turn executing a tool batch keeps the `running` phase
/// with no cancellation-requested call, and a delegation origin owns no
/// accepted-input turn in the scheduling projection. The batch's yielded
/// frontier and the steering pending against that turn are still retained
/// context, so their attachments are charged against the bound rather than
/// being replaced by the earliest queued base.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegated_executing_tool_batch_charges_its_retained_attachment()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture =
        authorize_delegated_model_call_fixture(&pool, DELEGATED_BATCH_FIXTURE_SEED).await?;
    let turn = fixture.authorized.turn();
    let request =
        ToolRequestId::from_uuid(Uuid::from_u128(DELEGATED_BATCH_FIXTURE_SEED + TOOL_REQUEST));
    let response =
        ToolUsingAssistantResponse::try_from_parts(vec![AssistantResponsePart::ToolCall(
            ToolCallProposal::new(
                ToolName::try_new(String::from("current_time")).expect("valid fixture tool name"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("bounded fixture arguments"),
            ),
        )])
        .expect("the proposal forms a tool-using response");
    let outcome = fixture
        .repository
        .apply_terminal_observation(
            fixture.child,
            fixture
                .authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::CompletedWithTools {
                    response,
                }),
            ModelCallTerminalIdentities::ToolRound(ToolRoundModelCallIdentities::new(
                vec![ToolResponsePartIdentity::tool_call(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                        DELEGATED_BATCH_FIXTURE_SEED + TOOL_CALL_ENTRY,
                    )),
                    request,
                    InitialToolApproval::PolicyAuto,
                )],
                ContextFrontierId::from_uuid(Uuid::from_u128(
                    DELEGATED_BATCH_FIXTURE_SEED + YIELDED_FRONTIER,
                )),
                Some(TurnAttemptId::from_uuid(Uuid::from_u128(
                    DELEGATED_BATCH_FIXTURE_SEED + CONTINUATION_ATTEMPT,
                ))),
            )),
            |_| panic!("the delegated fixture has no pending steering to reclassify"),
        )
        .await?;
    assert!(
        matches!(
            &outcome,
            ModelCallTerminalOutcome::ToolRound(round)
                if matches!(round.next_phase(), ActiveTurnPhase::Running { .. })
        ),
        "the delegated fixture reaches a tool round whose automatically approved batch executes under the running phase"
    );

    let retained_digest = BlobDigest::digest(b"delegated retained attachment");
    let later_steering_digest = BlobDigest::digest(b"delegated later steering attachment");
    catalog_verified_blob(
        &pool,
        retained_digest,
        RETAINED_ATTACHMENT_LENGTH,
        "delegated_batch_retained",
        Uuid::from_u128(DELEGATED_BATCH_FIXTURE_SEED + STORE_NAMESPACE),
        "retained",
    )
    .await?;
    catalog_verified_blob(
        &pool,
        later_steering_digest,
        RETAINED_ATTACHMENT_LENGTH,
        "delegated_batch_later_steering",
        Uuid::from_u128(DELEGATED_BATCH_FIXTURE_SEED + SECOND_STORE_NAMESPACE),
        "later_steering",
    )
    .await?;
    let repository = SubmitInputRepository::new(pool.clone())
        .with_attachment_maximum_bytes(TOOL_BATCH_ATTACHMENT_MAXIMUM);
    assert!(
        matches!(
            repository
                .handle(
                    SubmitInput::new(
                        DurableCommandId::from_uuid(Uuid::from_u128(
                            DELEGATED_BATCH_FIXTURE_SEED + STEERING_COMMAND,
                        )),
                        fixture.child,
                        attachment_content(retained_digest),
                        DeliveryRequest::NextSafePoint {
                            expected_active_turn: turn,
                        },
                    ),
                    AcceptedInputId::from_uuid(Uuid::from_u128(
                        DELEGATED_BATCH_FIXTURE_SEED + STEERING_ACCEPTED_INPUT,
                    )),
                    None,
                )
                .await?,
            SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::PendingSteering(_)
            ))
        ),
        "the retained seven-byte attachment must remain within the ten-byte bound"
    );
    assert_eq!(
        repository
            .handle(
                SubmitInput::new(
                    DurableCommandId::from_uuid(Uuid::from_u128(
                        DELEGATED_BATCH_FIXTURE_SEED + LATER_STEERING_COMMAND,
                    )),
                    fixture.child,
                    attachment_content(later_steering_digest),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: turn,
                    },
                ),
                AcceptedInputId::from_uuid(Uuid::from_u128(
                    DELEGATED_BATCH_FIXTURE_SEED + LATER_STEERING_ACCEPTED_INPUT,
                )),
                None,
            )
            .await?,
        SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: TOOL_BATCH_ATTACHMENT_MAXIMUM,
            }
        ))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// S01: first acceptance
/// commits the complete exact receipt and immutable queued origin; equal
/// replay and a restarted adapter return that receipt without consulting new
/// candidates.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_submit_apply_replay_conflict_and_restart() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x301, 0x701, direct(0x801)))
        .await?;
    let exact = " \tline one\r\ncafe\u{301}\n ";
    let command = start_input(
        0x302,
        0x701,
        exact,
        1,
        ModelSelectionOverride::UseSessionDefault,
    );
    let accepted = AcceptedInputId::from_uuid(Uuid::from_u128(0x901));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xa01));
    let request = SubmitInputRequest::try_new(
        command.command_id(),
        command.session(),
        command.content().clone(),
        command.delivery(),
    )?;
    let mut service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [
                accepted,
                AcceptedInputId::from_uuid(Uuid::from_u128(0x902)),
                AcceptedInputId::from_uuid(Uuid::from_u128(0x903)),
            ],
            [
                turn,
                TurnId::from_uuid(Uuid::from_u128(0xa02)),
                TurnId::from_uuid(Uuid::from_u128(0xa03)),
            ],
        ),
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );

    let first = service.execute(request.clone()).await?;
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(applied),
    )) = first.clone()
    else {
        panic!("no-active-turn start must apply");
    };
    assert_eq!(applied.accepted_input(), accepted);
    assert_eq!(applied.turn(), turn);
    assert_eq!(applied.acceptance_position().as_u64(), 1);
    assert_eq!(service.execute(request.clone()).await?, first);

    let conflicting = SubmitInputRequest::try_new(
        command.command_id(),
        command.session(),
        UserContent::try_text("different".to_owned())
            .expect("conflicting test content is admitted"),
        command.delivery(),
    )?;
    assert_eq!(
        service.execute(conflicting).await?,
        SubmitInputOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    );

    let stored: (String, String, String, i64, String) = sqlx::query_as(
        "SELECT command_part.text_value, accepted_part.text_value,
                queued.priority_kind,
                queued.acceptance_position::bigint, turn.state_kind
           FROM submit_input_command AS typed
           JOIN submit_input_command_content_part AS command_part
             ON command_part.command_id = typed.command_id
            AND command_part.position = 0
           JOIN accepted_input AS accepted
             ON accepted.accepting_command_id = typed.command_id
           JOIN accepted_input_content_part AS accepted_part
             ON accepted_part.accepted_input_id = accepted.accepted_input_id
            AND accepted_part.position = 0
           JOIN queued_input_origin AS queued
             ON queued.accepted_input_id = accepted.accepted_input_id
           JOIN turn_lifecycle AS turn
             ON turn.turn_id = queued.turn_id
          WHERE typed.command_id = $1",
    )
    .bind(Uuid::from_u128(0x302))
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            exact.to_owned(),
            exact.to_owned(),
            "ordinary".into(),
            1,
            "queued".into()
        )
    );

    drop(service);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let restarted = SubmitInputRepository::new(restarted_pool.clone());
    let mut restarted_service = SubmitInputService::new(
        FixedSubmitInputIds::new(
            [AcceptedInputId::from_uuid(Uuid::from_u128(0x904))],
            [TurnId::from_uuid(Uuid::from_u128(0xa04))],
        ),
        restarted.clone(),
        AcceptingEligibilityNudge,
        signalbox_application::InProcessToolDispatchGate::default(),
    );
    let loaded = restarted
        .load(command.command_id())
        .await?
        .expect("the committed receipt survives adapter restart");
    assert_eq!(loaded.command(), &command);
    assert_eq!(restarted_service.execute(request).await?, first);
    let effect_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM submit_input_command WHERE command_id = $1),
            (SELECT count(*) FROM accepted_input WHERE accepting_command_id = $1),
            (SELECT count(*)
               FROM queued_input_origin AS queued
               JOIN accepted_input AS accepted
                 ON accepted.accepted_input_id = queued.accepted_input_id
              WHERE accepted.accepting_command_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle AS turn
               JOIN accepted_input AS accepted
                 ON accepted.origin_turn_id = turn.turn_id
              WHERE accepted.accepting_command_id = $1)",
    )
    .bind(Uuid::from_u128(0x302))
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(effect_counts, (1, 1, 1, 1));

    drop(restarted_service);
    restarted_pool.close().await;
    drop(container);
    Ok(())
}

/// S01 / S03: the real application service
/// commits one complete activation, and a fresh repository and pool observe
/// the same occupied slot after restart without activating it again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s01_s03_start_eligible_turn_survives_restart() -> Result<(), Box<dyn Error>> {
    let (container, pool, database_url) = migrated_postgres().await?;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(0x381, 0x781, direct(0x881)))
        .await?;
    let session = SessionId::from_uuid(Uuid::from_u128(0x781));
    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x981));
    let turn = TurnId::from_uuid(Uuid::from_u128(0xa81));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                0x382,
                0x781,
                "restart-boundary activation",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            accepted_input,
            Some(turn),
        )
        .await?;

    let origin_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd81));
    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0xe81));
    let initial_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0xb81));
    let mut service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new([origin_entry], [starting_frontier], [initial_attempt]),
        StartEligibleTurnRepository::new(pool.clone()),
    );

    let StartEligibleTurnOutcome::Activated(activated) = service.execute(session).await? else {
        panic!("the sole queued turn must activate");
    };
    assert_eq!(activated.session(), session);
    assert_eq!(activated.turn(), turn);
    assert_eq!(
        activated.accepted_input().expect("accepted origin").id(),
        accepted_input
    );
    assert_eq!(
        activated.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(activated.start().frontier().snapshot(), starting_frontier);
    let ActiveTurnPhase::Running { current_attempt } = activated.phase() else {
        panic!("initial activation must return the running phase");
    };
    assert_eq!(current_attempt.id(), initial_attempt);
    assert_eq!(current_attempt.state(), &CurrentTurnAttemptState::Prepared);

    let stored: (String, String, String, Uuid, i64, i64, i64) = sqlx::query_as(
        "SELECT
            turn.state_kind,
            turn.active_phase_kind,
            attempt.state_kind,
            turn.current_attempt_id,
            frontier.member_count::bigint,
            (SELECT count(*)
               FROM turn_lifecycle AS active
              WHERE active.session_id = turn.session_id
                AND active.state_kind = 'active'),
            (SELECT count(*)
               FROM session_scheduler AS scheduler
              WHERE scheduler.session_id = turn.session_id)
         FROM turn_lifecycle AS turn
         JOIN turn_attempt AS attempt
           ON attempt.turn_attempt_id = turn.current_attempt_id
         JOIN context_frontier AS frontier
           ON frontier.owning_session_id = turn.session_id
          AND frontier.context_frontier_id = turn.starting_frontier_id
        WHERE turn.turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored,
        (
            "active".into(),
            "running".into(),
            "prepared".into(),
            initial_attempt.into_uuid(),
            1,
            1,
            1,
        )
    );

    drop(service);
    pool.close().await;
    let restarted_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    let mut restarted_service = StartEligibleTurnService::new(
        FixedStartEligibleTurnIds::new(
            [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0xd82))],
            [ContextFrontierId::from_uuid(Uuid::from_u128(0xe82))],
            [TurnAttemptId::from_uuid(Uuid::from_u128(0xb82))],
        ),
        StartEligibleTurnRepository::new(restarted_pool.clone()),
    );
    assert_eq!(
        restarted_service.execute(session).await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );
    let persisted: (String, Uuid, i64, i64, i64) = sqlx::query_as(
        "SELECT
            state_kind,
            current_attempt_id,
            (SELECT count(*) FROM semantic_transcript_entry),
            (SELECT count(*) FROM context_frontier),
            (SELECT count(*) FROM turn_attempt)
         FROM turn_lifecycle
        WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .fetch_one(&restarted_pool)
    .await?;
    assert_eq!(
        persisted,
        ("active".into(), initial_attempt.into_uuid(), 1, 1, 1,)
    );

    drop(restarted_service);
    restarted_pool.close().await;
    drop(container);
    Ok(())
}
