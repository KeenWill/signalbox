#![allow(
    dead_code,
    reason = "standalone integration-test crates select different shared fixture helpers"
)]

use sqlx::PgPool;

pub(crate) async fn record_empty_instruction_manifest(
    pool: &PgPool,
    session: signalbox_domain::SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let turn = signalbox_domain::TurnId::from_uuid(
        sqlx::query_scalar::<_, sqlx::types::Uuid>(
            "SELECT turn_id FROM turn_lifecycle WHERE session_id = $1 AND state_kind = 'active'",
        )
        .bind(session.into_uuid())
        .fetch_one(pool)
        .await?,
    );
    let snapshot = signalbox_application::discover_workspace_instructions(Vec::new());
    let manifest = signalbox_domain::TurnInstructionManifest::empty_turn_start(
        signalbox_domain::TurnInstructionManifestId::from_uuid(turn.into_uuid()),
        session,
        turn,
    );
    let outcome =
        signalbox_persistence::workspace_instructions::WorkspaceInstructionRepository::new(
            pool.clone(),
        )
        .record_turn_start(
            signalbox_domain::InstructionDiscoveryId::from_uuid(turn.into_uuid()),
            manifest,
            &snapshot,
            || {
                signalbox_domain::InstructionBundleId::from_uuid(sqlx::types::Uuid::from_u128(
                    0x1e77,
                ))
            },
        )
        .await?;
    assert!(!matches!(
        outcome,
        signalbox_persistence::workspace_instructions::RecordTurnInstructionSnapshotOutcome::TurnUnavailable
    ));
    Ok(())
}

/// Polls until exactly `expected` backends are lock-blocked behind another
/// backend, returning whether that count appeared within the polling budget.
pub(crate) async fn blocked_backends_reached(
    pool: &PgPool,
    expected: i64,
) -> Result<bool, sqlx::Error> {
    for _ in 0..400 {
        let observed: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM pg_stat_activity
              WHERE cardinality(pg_blocking_pids(pid)) > 0",
        )
        .fetch_one(pool)
        .await?;
        if observed == expected {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Ok(false)
}
