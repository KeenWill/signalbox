//! Captures exclusion evidence under the pool's selection locks.
use super::*;
use crate::credential_pool_exhaustion::{
    CredentialPoolExclusion as Exclusion, CredentialPoolMemberEvidence as Member,
};
use sqlx::types::time::OffsetDateTime;

struct Candidate {
    exclusion: Exclusion,
    rank: u8,
    action: Option<i64>,
    reset: Option<i64>,
}

pub(super) async fn record(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    policy: &CredentialPoolRuntimePolicy,
    observed_at: OffsetDateTime,
    headroom: &HashMap<String, Option<i64>>,
) -> Result<(), ModelCallRepositoryError> {
    let policy_id = credential_pool_records::retain_policy(connection, policy).await?;
    let ceiling: i64 =
        sqlx::query_scalar("SELECT COALESCE(max(record_generation), 0) FROM credential_exclusion")
            .fetch_one(&mut *connection)
            .await?;
    let completed: HashSet<String> = sqlx::query_scalar("SELECT DISTINCT call.credential_reference FROM model_call call JOIN model_call_credential_pool_policy policy USING (model_call_id) WHERE call.session_id = $1 AND policy.pool_name = $2 AND call.state_kind = 'terminal' AND call.terminal_disposition_kind = 'completed'")
        .bind(session.into_uuid()).bind(policy.name()).fetch_all(&mut *connection).await?.into_iter().collect();
    let actions = sqlx::query("SELECT a.action_id, a.credential_reference, a.action_kind, a.observed_session_id, a.observed_turn_id, x.record_generation FROM credential_pool_member_action a LEFT JOIN credential_exclusion_state x ON x.action_id = a.action_id WHERE a.consumed_turn_id IS NULL AND ((x.active AND (x.pool_policy_id = $1 OR x.kind = 'profile_quarantine')) OR (x.record_generation IS NULL AND (a.pool_name = $2 OR a.action_kind = 'quarantine'))) ORDER BY x.record_generation DESC NULLS LAST, a.action_id DESC")
        .bind(policy_id).bind(policy.name()).fetch_all(&mut *connection).await?;
    let quarantines = sqlx::query("SELECT profile, record_generation FROM credential_exclusion_state WHERE active AND kind = 'profile_quarantine' AND origin <> 'pool_trigger' ORDER BY record_generation DESC").fetch_all(&mut *connection).await?;
    let chains = sqlx::query("SELECT credential_reference, predecessor_model_call_id FROM credential_pool_chain_exclusion WHERE session_id = $1 AND turn_id = $2 ORDER BY predecessor_model_call_id")
        .bind(session.into_uuid()).bind(turn.into_uuid()).fetch_all(&mut *connection).await?;
    let transient = sqlx::query("SELECT credential_reference, observation_model_call_id, reset_at FROM credential_pool_transient_exclusion WHERE reset_at > $1 ORDER BY reset_at DESC, observation_model_call_id")
        .bind(observed_at).fetch_all(&mut *connection).await?;
    for (ordinal, member) in policy.members().iter().enumerate() {
        let profile = member.credential_reference();
        let mut candidates = Vec::new();
        for row in &quarantines {
            if row.try_get::<String, _>("profile")? == profile {
                candidates.push(Candidate {
                    exclusion: Exclusion::ProfileQuarantine {
                        record_generation: Some(generation(row.try_get("record_generation")?)?),
                    },
                    rank: 0,
                    action: None,
                    reset: None,
                });
            }
        }
        for row in &actions {
            if row.try_get::<String, _>("credential_reference")? != profile {
                continue;
            }
            let record_generation = row
                .try_get::<Option<i64>, _>("record_generation")?
                .map(generation)
                .transpose()?;
            let (exclusion, rank) = match row.try_get::<String, _>("action_kind")?.as_str() {
                "quarantine" => (Exclusion::ProfileQuarantine { record_generation }, 0),
                "avoid_new_sessions" if !completed.contains(profile) => {
                    (Exclusion::MembershipExclusion { record_generation }, 1)
                }
                "switch_next_turn"
                    if row.try_get::<Uuid, _>("observed_session_id")? == session.into_uuid()
                        && row.try_get::<Uuid, _>("observed_turn_id")? != turn.into_uuid() =>
                {
                    (Exclusion::SessionDisplacement { record_generation }, 2)
                }
                _ => continue,
            };
            candidates.push(Candidate {
                exclusion,
                rank,
                action: Some(row.try_get("action_id")?),
                reset: None,
            });
        }
        for row in &chains {
            if row.try_get::<String, _>("credential_reference")? == profile {
                candidates.push(Candidate {
                    exclusion: Exclusion::ChainExclusion {
                        predecessor_model_call_id: row.try_get("predecessor_model_call_id")?,
                    },
                    rank: 3,
                    action: None,
                    reset: None,
                });
            }
        }
        for row in &transient {
            if row.try_get::<String, _>("credential_reference")? == profile {
                let reset: OffsetDateTime = row.try_get("reset_at")?;
                candidates.push(Candidate {
                    exclusion: Exclusion::TransientExclusion {
                        observation_model_call_id: row.try_get("observation_model_call_id")?,
                    },
                    rank: 4,
                    action: None,
                    reset: Some(unix_ms(reset)?),
                });
            }
        }
        if let (Some(observed), Some(reserve)) = (
            headroom.get(profile).copied().flatten(),
            member
                .headroom_reserve_percent
                .or(policy.headroom_reserve_percent),
        ) && observed <= i64::from(reserve)
        {
            let snapshot =
                crate::credential_capacity::load_credential_rate_limits(connection, profile)
                    .await?
                    .ok_or(ModelCallCorruption::Missing("headroom exhaustion snapshot"))?;
            let reset = snapshot
                .windows()
                .iter()
                .filter_map(|window| *window.resets_at())
                .min()
                .ok_or(ModelCallCorruption::Missing("headroom exhaustion reset"))?;
            candidates.push(Candidate {
                exclusion: Exclusion::HeadroomReserve {
                    observed_headroom_percent: observed,
                    reserve_percent: reserve,
                },
                rank: 5,
                action: None,
                reset: Some(unix_ms(OffsetDateTime::from(reset))?),
            });
        }
        let reset = if candidates.iter().all(|candidate| candidate.reset.is_some()) {
            candidates
                .iter()
                .filter_map(|candidate| candidate.reset)
                .max()
        } else {
            None
        };
        let selected = candidates
            .into_iter()
            .min_by_key(|candidate| candidate.rank)
            .ok_or(ModelCallCorruption::Missing(
                "exhausted pool member evidence",
            ))?;
        let evidence = serde_json::to_value(Member {
            profile: profile.to_owned(),
            reset_at_unix_ms: reset,
            exclusion: selected.exclusion,
        })
        .map_err(|_| ModelCallCorruption::Inconsistent("pool evidence encoding"))?;
        sqlx::query("INSERT INTO credential_pool_exhaustion_member (terminal_attempt_id, pool_policy_id, ordinal, profile, evidence, observed_at, generation_ceiling, action_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(attempt.into_uuid()).bind(policy_id).bind(i32::try_from(ordinal).map_err(|_| ModelCallCorruption::Inconsistent("pool member ordinal"))?).bind(profile).bind(evidence).bind(observed_at).bind(ceiling).bind(selected.action).execute(&mut *connection).await?;
    }
    Ok(())
}

fn generation(value: i64) -> Result<u64, ModelCallRepositoryError> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(ModelCallCorruption::Inconsistent("pool evidence generation").into())
}
fn unix_ms(value: OffsetDateTime) -> Result<i64, ModelCallRepositoryError> {
    i64::try_from(value.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| ModelCallCorruption::Inconsistent("pool evidence reset").into())
}
