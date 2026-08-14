//! Durable authority used to choose daemon-local instruction roots.

use crate::*;

/// INV-061: the current runner-placement pointer is positive authority to omit
/// a daemon-local workspace root from instruction discovery.
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

    assert!(!before_placement);
    assert!(after_placement);

    pool.close().await;
    drop(container);
    Ok(())
}
