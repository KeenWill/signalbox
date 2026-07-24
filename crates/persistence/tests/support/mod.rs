use sqlx::PgPool;

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
