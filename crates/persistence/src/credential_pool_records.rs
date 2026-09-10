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

pub(super) async fn load_policy(
    connection: &mut PgConnection,
    policy_id: Uuid,
) -> Result<CredentialPoolRuntimePolicy, ModelCallRepositoryError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Member {
        profile: String,
        priority: NonZeroU32,
        headroom_reserve_percent: Option<u8>,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Definition {
        name: String,
        on_pool_exhausted: String,
        on_quota_exhausted: String,
        on_rate_limited: String,
        on_overloaded: String,
        on_credential_rejected: String,
        tie_break: String,
        headroom_reserve_percent: Option<u8>,
        on_headroom_low: String,
        members: Vec<Member>,
    }
    let value: serde_json::Value = sqlx::query_scalar(
        "SELECT definition FROM credential_pool_policy WHERE pool_policy_id = $1",
    )
    .bind(policy_id)
    .fetch_one(&mut *connection)
    .await?;
    let stored: Definition = serde_json::from_value(value)
        .map_err(|_| ModelCallCorruption::Inconsistent("frozen credential pool policy"))?;
    let members = stored
        .members
        .into_iter()
        .map(|member| CredentialPoolRuntimeMember {
            credential_reference: member.profile.into(),
            priority: member.priority,
            headroom_reserve_percent: member.headroom_reserve_percent,
            availability_probe: None,
        })
        .collect::<Vec<_>>();
    Ok(CredentialPoolRuntimePolicy::new(
        stored.name,
        members,
        CredentialPoolRuntimeExhaustion::parse(&stored.on_pool_exhausted)?,
        CredentialPoolRuntimeAction::parse(&stored.on_quota_exhausted)?,
        CredentialPoolRuntimeAction::parse(&stored.on_rate_limited)?,
        CredentialPoolRuntimeAction::parse(&stored.on_overloaded)?,
        CredentialPoolRuntimeAction::parse(&stored.on_credential_rejected)?,
    )
    .with_capacity_policy(
        CredentialPoolRuntimeTieBreak::parse(&stored.tie_break)?,
        stored.headroom_reserve_percent,
        CredentialPoolRuntimeAction::parse(&stored.on_headroom_low)?,
    ))
}

pub(super) async fn admission_policy(
    connection: &mut PgConnection,
    attempt: signalbox_domain::TurnAttemptId,
    target: ResolvedProviderTarget,
    catalog: &CredentialPoolRuntimeCatalog,
) -> Result<Option<CredentialPoolRuntimePolicy>, ModelCallRepositoryError> {
    let policy_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT waiting.pool_policy_id FROM credential_availability_wait_release release
         JOIN credential_availability_wait waiting USING (wait_attempt_id)
         WHERE release.turn_attempt_id = $1",
    )
    .bind(attempt.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(policy_id) = policy_id {
        return load_policy(connection, policy_id).await.map(Some);
    }
    Ok(catalog.get(&target).cloned())
}
