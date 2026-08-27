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

/// Counts the backends currently waiting behind another backend's lock.
async fn blocked_backend_count(connection: &mut sqlx::PgConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_stat_activity
          WHERE cardinality(pg_blocking_pids(pid)) > 0",
    )
    .fetch_one(connection)
    .await
}

/// Polls until exactly `expected` backends are lock-blocked behind another
/// backend, returning whether that count appeared within the polling budget.
///
/// This takes a pooled connection per sample, which is right for a wait the
/// test itself ends: the blocked backend stays blocked until the observer
/// releases it, so a sample delayed by a cold connection merely arrives later
/// and still sees the wait. Callers observing a wait the *database* ends on its
/// own budget must use [`blocked_backends_reached_on`] instead, because for
/// them that connection can outlast the window it was meant to sample.
pub(crate) async fn blocked_backends_reached(
    pool: &PgPool,
    expected: i64,
) -> Result<bool, sqlx::Error> {
    for _ in 0..400 {
        let mut connection = pool.acquire().await?;
        if blocked_backend_count(&mut connection).await? == expected {
            return Ok(true);
        }
        drop(connection);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Ok(false)
}

/// [`blocked_backends_reached`] over a connection the caller already holds.
///
/// Establishing a PostgreSQL connection is not free, and on a loaded host it is
/// not fast. A caller whose observed wait self-terminates — a statement under
/// `lock_timeout`, or an acquisition under its own budget — must therefore have
/// its observing connection in hand *before* opening the window, or it can
/// spend the entire window connecting and then correctly report that it never
/// saw a wait that did happen.
pub(crate) async fn blocked_backends_reached_on(
    connection: &mut sqlx::PgConnection,
    expected: i64,
) -> Result<bool, sqlx::Error> {
    for _ in 0..400 {
        if blocked_backend_count(connection).await? == expected {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Ok(false)
}
