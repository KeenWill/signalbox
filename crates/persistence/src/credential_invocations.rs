//! Durable per-profile invocation reservations and process identities.
use crate::model_execution::{
    ModelCallRepositoryError, credential_pool::acquire_model_call_outbox_order_guard,
};
use signalbox_domain::ModelCallId;
use sqlx::{PgConnection, PgPool, Row};
use std::num::NonZeroU32;
use uuid::Uuid;

/// Installs startup-only Codex home bounds and re-evaluates contended waits.
pub async fn replace_registrations(
    pool: &PgPool,
    registrations: &[(String, Option<NonZeroU32>)],
) -> Result<(), ModelCallRepositoryError> {
    let mut transaction = pool.begin().await?;
    acquire_model_call_outbox_order_guard(&mut transaction).await?;
    sqlx::query("UPDATE credential_invocation_capacity SET registered = false")
        .execute(&mut *transaction)
        .await?;
    let mut registrations = registrations.iter().collect::<Vec<_>>();
    registrations.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for (profile, bound) in registrations {
        sqlx::query("INSERT INTO credential_invocation_capacity (profile, max_concurrent_invocations, registered) VALUES ($1,$2::bigint::integer,true) ON CONFLICT (profile) DO UPDATE SET max_concurrent_invocations = EXCLUDED.max_concurrent_invocations, registered = true")
            .bind(profile).bind(bound.map(|bound| i64::from(bound.get()))).execute(&mut *transaction).await?;
    }
    sqlx::query("UPDATE credential_availability_wait waiting SET eligible = true WHERE waiting.consumed_by_attempt_id IS NULL AND waiting.cause = 'contended' AND NOT waiting.eligible AND EXISTS (SELECT 1 FROM credential_availability_wait_member member LEFT JOIN credential_invocation_capacity capacity USING (profile) WHERE member.wait_attempt_id = waiting.wait_attempt_id AND member.capacity_bound IS NOT NULL AND (capacity.profile IS NULL OR NOT capacity.registered OR capacity.max_concurrent_invocations IS NULL OR capacity.max_concurrent_invocations > (SELECT count(*) FROM credential_invocation_reservation reservation WHERE reservation.profile = member.profile AND reservation.released_at IS NULL)))")
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(())
}

/// Records the process group before the child receives its request.
pub async fn register_process(
    pool: &PgPool,
    call: ModelCallId,
    process_group: u32,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query("UPDATE credential_invocation_reservation SET process_group_id = $2 WHERE model_call_id = $1 AND released_at IS NULL AND process_group_id IS NULL")
        .bind(call.into_uuid()).bind(i64::from(process_group)).execute(pool).await?;
    Ok(())
}

/// Retained invocation identities awaiting proof that their process group ended.
pub async fn active_processes(
    pool: &PgPool,
) -> Result<Vec<(ModelCallId, u32)>, ModelCallRepositoryError> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as("SELECT model_call_id, process_group_id FROM credential_invocation_reservation WHERE released_at IS NULL AND process_group_id IS NOT NULL")
        .fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(call, group)| (ModelCallId::from_uuid(call), group as u32))
        .collect())
}

/// Releases capacity after the caller proves that the invocation has ended.
pub async fn release(pool: &PgPool, call: ModelCallId) -> Result<(), ModelCallRepositoryError> {
    sqlx::query("SELECT release_credential_invocation($1)")
        .bind(call.into_uuid())
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn lock_profiles(
    connection: &mut PgConnection,
    profiles: &[&str],
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query(crate::lock_inventory::CREDENTIAL_INVOCATION_CAPACITY_LOCK)
        .bind(profiles)
        .fetch_all(connection)
        .await?;
    Ok(())
}

pub(crate) struct BoundedMember {
    pub(crate) profile: String,
    pub(crate) bound: i32,
    pub(crate) reservations: Vec<Uuid>,
}

pub(crate) async fn bounded_members(
    connection: &mut PgConnection,
    profiles: &[&str],
) -> Result<Vec<BoundedMember>, ModelCallRepositoryError> {
    lock_profiles(connection, profiles).await?;
    let rows = sqlx::query("SELECT capacity.profile, capacity.max_concurrent_invocations, ARRAY(SELECT model_call_id FROM credential_invocation_reservation reservation WHERE reservation.profile = capacity.profile AND reservation.released_at IS NULL ORDER BY model_call_id) AS reservations FROM credential_invocation_capacity capacity WHERE capacity.profile = ANY($1) AND capacity.registered AND capacity.max_concurrent_invocations IS NOT NULL")
        .bind(profiles).fetch_all(connection).await?;
    let mut bounded = Vec::new();
    for row in rows {
        let bound: i32 = row.try_get("max_concurrent_invocations")?;
        let reservations: Vec<Uuid> = row.try_get("reservations")?;
        if reservations.len() >= bound as usize {
            bounded.push(BoundedMember {
                profile: row.try_get("profile")?,
                bound,
                reservations,
            });
        }
    }
    Ok(bounded)
}

pub(crate) async fn reserve(
    connection: &mut PgConnection,
    call: ModelCallId,
    profile: &str,
) -> Result<(), ModelCallRepositoryError> {
    lock_profiles(connection, &[profile]).await?;
    sqlx::query("INSERT INTO credential_invocation_reservation (model_call_id, profile) SELECT $1, profile FROM credential_invocation_capacity WHERE profile = $2 AND registered")
        .bind(call.into_uuid()).bind(profile).execute(connection).await?;
    Ok(())
}

/// Returns the retained process group for one unreleased invocation.
pub async fn process_group(
    pool: &PgPool,
    call: ModelCallId,
) -> Result<Option<u32>, ModelCallRepositoryError> {
    let group: Option<i64> = sqlx::query_scalar("SELECT process_group_id FROM credential_invocation_reservation WHERE model_call_id = $1 AND released_at IS NULL")
        .bind(call.into_uuid()).fetch_optional(pool).await?.flatten();
    Ok(group.map(|group| group as u32))
}
