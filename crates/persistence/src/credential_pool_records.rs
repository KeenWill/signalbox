//! Immutable identities for complete credential-pool policy snapshots.
use super::*;

pub(super) fn definition(policy: &CredentialPoolRuntimePolicy) -> serde_json::Value {
    serde_json::json!({
        "name": policy.name(), "on_pool_exhausted": policy.on_pool_exhausted.as_str(),
        "on_quota_exhausted": policy.quota_exhausted.as_str(), "on_rate_limited": policy.rate_limited.as_str(),
        "on_overloaded": policy.overloaded.as_str(), "on_credential_rejected": policy.credential_rejected.as_str(),
        "tie_break": policy.tie_break.as_str(), "headroom_reserve_percent": policy.headroom_reserve_percent,
        "on_headroom_low": policy.headroom_low.as_str(),
        "members": policy.members().iter().map(|member| serde_json::json!({
            "profile": member.credential_reference(), "priority": member.priority().get(),
            "headroom_reserve_percent": member.headroom_reserve_percent,
        })).collect::<Vec<_>>(),
    })
}

pub(super) async fn retain_policy(
    connection: &mut PgConnection,
    policy: &CredentialPoolRuntimePolicy,
) -> Result<Uuid, ModelCallRepositoryError> {
    Ok(
        sqlx::query_scalar("SELECT retain_credential_pool_policy($1, $2::jsonb)")
            .bind(Uuid::now_v7())
            .bind(definition(policy).to_string())
            .fetch_one(connection)
            .await?,
    )
}
