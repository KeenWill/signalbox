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
