use super::{
    CredentialPoolRuntimeAction, CredentialPoolRuntimeCatalog, CredentialPoolRuntimeExhaustion,
    CredentialPoolRuntimeMember, CredentialPoolRuntimePolicy, CredentialPoolRuntimeTieBreak,
    MODEL_CALL_OUTBOX_ORDER_GUARD, ModelCallCorruption, ModelCallOutboxOrderGuard,
    ModelCallRepositoryError, ToolContinuationUsageLimit, ToolContinuationUsageLimitCatalog,
};
use crate::mapping::{positive_u64_from_numeric, session_id_to_uuid, turn_id_to_uuid};
use rust_decimal::Decimal;
use signalbox_application::ModelCallCredentialReference;
use signalbox_domain::{
    CorrelatedModelCallTerminalObservation, FastMode, ModelCallId, ProviderModelIdentity,
    ResolvedProviderTarget, SessionId, TurnAttemptId, TurnId,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

/// Resolves the target whose credential pool governs this call.
///
/// Fast mode can route a selectable model to an alternate serving target with a
/// different credential family and a different pool. Looking the pool up under
/// the base target would replace the correctly resolved serving credential with
/// a member of an unrelated pool and freeze that unrelated failover policy.
pub(super) fn serving_pool_target(
    families: Option<&crate::ModelCredentialFamilyCatalog>,
    selected: ResolvedProviderTarget,
    fast_mode: FastMode,
) -> ResolvedProviderTarget {
    families.map_or(selected, |families| {
        families.serving_target_for_call(selected, fast_mode)
    })
}

#[derive(Clone)]
pub(crate) struct PreparedServingEvidence<'a> {
    pub(super) effective_target: ResolvedProviderTarget,
    pub(super) credential_model_family: Option<&'a str>,
    pub(super) limit: Option<ToolContinuationUsageLimit>,
}

pub(crate) fn prepared_serving_evidence<'a>(
    families: Option<&'a crate::ModelCredentialFamilyCatalog>,
    limits: &ToolContinuationUsageLimitCatalog,
    selected_target: ResolvedProviderTarget,
    fast_mode: FastMode,
) -> PreparedServingEvidence<'a> {
    let effective_target = serving_pool_target(families, selected_target, fast_mode);
    PreparedServingEvidence {
        effective_target,
        credential_model_family: families.and_then(|families| families.family(effective_target)),
        limit: limits.get(&(selected_target, fast_mode)).cloned(),
    }
}

pub(super) fn remap_preserves_preflight_limits(
    previous: Option<ToolContinuationUsageLimit>,
    current: Option<ToolContinuationUsageLimit>,
) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => {
            let previous_input_allowance = previous
                .context_window_tokens()
                .saturating_sub(previous.max_output_tokens());
            let current_input_allowance = current
                .context_window_tokens()
                .saturating_sub(current.max_output_tokens());
            current_input_allowance >= previous_input_allowance
                && current.replays_provider_compaction() == previous.replays_provider_compaction()
        }
        _ => false,
    }
}

pub(super) fn prepared_serving_configuration_is_compatible(
    prepared_target: ResolvedProviderTarget,
    prepared_family: Option<&str>,
    prepared_limit: Option<ToolContinuationUsageLimit>,
    current: PreparedServingEvidence<'_>,
) -> bool {
    let configuration_changed = prepared_target != current.effective_target
        || prepared_family != current.credential_model_family
        || !prepared_limit_configuration_matches(prepared_limit.clone(), current.limit.clone());
    !configuration_changed
        || (matches!(
            (prepared_family, current.credential_model_family),
            (Some(prepared), Some(current)) if prepared == current
        ) && remap_preserves_preflight_limits(prepared_limit, current.limit))
}

fn prepared_limit_configuration_matches(
    prepared: Option<ToolContinuationUsageLimit>,
    current: Option<ToolContinuationUsageLimit>,
) -> bool {
    match (prepared, current) {
        (None, None) => true,
        (Some(prepared), Some(current)) => {
            prepared
                .context_window_tokens()
                .saturating_sub(prepared.max_output_tokens())
                == current
                    .context_window_tokens()
                    .saturating_sub(current.max_output_tokens())
                && prepared.replays_provider_compaction() == current.replays_provider_compaction()
        }
        _ => false,
    }
}

pub(super) fn decode_prepared_usage_limit(
    row: &PgRow,
    target: ResolvedProviderTarget,
) -> Result<Option<ToolContinuationUsageLimit>, ModelCallRepositoryError> {
    let max_output_tokens = row.try_get::<Option<Decimal>, _>("prepared_max_output_tokens")?;
    let context_window_tokens =
        row.try_get::<Option<Decimal>, _>("prepared_context_window_tokens")?;
    let provider_compaction_replay =
        row.try_get::<Option<bool>, _>("prepared_provider_compaction_replay")?;
    match (
        max_output_tokens,
        context_window_tokens,
        provider_compaction_replay,
    ) {
        (None, None, None) => Ok(None),
        (Some(max_output_tokens), Some(context_window_tokens), Some(replays)) => {
            let max_output_tokens = positive_u64_from_numeric(max_output_tokens)
                .map_err(|_| ModelCallCorruption::Inconsistent("prepared maximum output tokens"))?;
            let context_window_tokens = positive_u64_from_numeric(context_window_tokens)
                .map_err(|_| ModelCallCorruption::Inconsistent("prepared context window tokens"))?;
            let limit = ToolContinuationUsageLimit::new(
                target,
                FastMode::Disabled,
                max_output_tokens,
                context_window_tokens,
            );
            Ok(Some(if replays {
                limit.with_provider_compaction_replay()
            } else {
                limit
            }))
        }
        _ => Err(
            ModelCallCorruption::Inconsistent("prepared serving limit evidence completeness")
                .into(),
        ),
    }
}

pub(super) struct SelectedRuntimePoolCredential {
    pub(super) wait: Option<super::credential_wait::WaitSnapshot>,
    pub(super) reference: Option<ModelCallCredentialReference>,
    pub(super) policy: Option<CredentialPoolRuntimePolicy>,
    /// Uncommitted `switch_next_turn` rows this selection would satisfy.
    ///
    /// Selection cannot consume them itself: preparation can still fail after
    /// selection succeeds, and a member that never carried a call must leave
    /// its displacement durable for the next turn.
    pub(super) pending_consumed_actions: Vec<i64>,
}

/// Marks the displacement rows a prepared call has now satisfied.
pub(super) async fn consume_pool_member_actions(
    connection: &mut PgConnection,
    turn: TurnId,
    actions: &[i64],
) -> Result<(), ModelCallRepositoryError> {
    if actions.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE credential_pool_member_action
            SET consumed_turn_id = $1
          WHERE action_id = ANY($2)
            AND consumed_turn_id IS NULL",
    )
    .bind(turn_id_to_uuid(turn))
    .bind(actions)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Reports the remaining successor delay when this call already substituted.
///
/// The successor row is written in the same transaction that terminalizes its
/// predecessor, so its presence proves the commit landed. The delay is
/// recovered from the successor attempt's own durable deadline; an elapsed
/// deadline yields zero rather than absence.
pub(super) async fn committed_availability_successor_backoff(
    connection: &mut PgConnection,
    predecessor: ModelCallId,
) -> Result<Option<Duration>, ModelCallRepositoryError> {
    let successor: Option<Uuid> = sqlx::query_scalar(
        "SELECT successor_turn_attempt_id
           FROM credential_pool_availability_successor
          WHERE predecessor_model_call_id = $1",
    )
    .bind(predecessor.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(successor) = successor else {
        return Ok(None);
    };
    let remaining =
        load_availability_successor_backoff(connection, TurnAttemptId::from_uuid(successor))
            .await?;
    Ok(Some(remaining.unwrap_or(Duration::ZERO)))
}

pub(super) async fn load_availability_successor_backoff(
    connection: &mut PgConnection,
    attempt: TurnAttemptId,
) -> Result<Option<Duration>, ModelCallRepositoryError> {
    let remaining: Option<i64> = sqlx::query_scalar(
        "SELECT GREATEST(
                    0,
                    CEIL(EXTRACT(EPOCH FROM (retry_not_before - clock_timestamp())) * 1000)
                )::bigint
           FROM credential_pool_availability_successor
          WHERE successor_turn_attempt_id = $1
            AND retry_not_before > clock_timestamp()",
    )
    .bind(attempt.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    remaining
        .map(|milliseconds| {
            u64::try_from(milliseconds)
                .map(Duration::from_millis)
                .map_err(|_| {
                    ModelCallCorruption::Inconsistent("availability successor backoff").into()
                })
        })
        .transpose()
}

/// Every member one pool currently excludes, with the rows a call would satisfy.
pub(super) struct DurablePoolExclusions {
    pub(super) observed_at: sqlx::types::time::OffsetDateTime,
    pub(super) excluded: HashSet<String>,
    pending_consumed_actions: Vec<i64>,
    pub(super) headroom: HashMap<String, Option<i64>>,
}

use super::{credential_pool_evidence, credential_pool_records};

/// Serializes action-head reads and writes for one credential profile.
///
/// A quarantine spans pools, so the profile reference alone is the lock key. Callers needing
/// several profiles take them in sorted order, so two sessions preparing calls
/// over the same pool cannot deadlock against each other.
pub(crate) async fn lock_credential_pool_action_head(
    connection: &mut PgConnection,
    credential_reference: &str,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query(crate::lock_inventory::HASHED_TRANSACTION_ADVISORY_LOCK)
        .bind(format!(
            "credential_pool_action_head:{credential_reference}"
        ))
        .execute(&mut *connection)
        .await?;
    Ok(())
}

/// Serializes model-call transactions before either credential or outbox locks.
pub(crate) async fn acquire_model_call_outbox_order_guard(
    connection: &mut PgConnection,
) -> Result<ModelCallOutboxOrderGuard, ModelCallRepositoryError> {
    sqlx::query(crate::lock_inventory::HASHED_TRANSACTION_ADVISORY_LOCK)
        .bind(MODEL_CALL_OUTBOX_ORDER_GUARD)
        .execute(&mut *connection)
        .await?;
    Ok(ModelCallOutboxOrderGuard { _private: () })
}

/// Takes every action-head lock for one pool in deterministic profile order.
pub(super) async fn lock_credential_pool_action_heads(
    connection: &mut PgConnection,
    policy: &CredentialPoolRuntimePolicy,
) -> Result<(), ModelCallRepositoryError> {
    let members = credential_pool_member_references(policy);
    let mut locked = members.iter().copied().collect::<Vec<_>>();
    locked.sort_unstable();
    for reference in locked {
        lock_credential_pool_action_head(connection, reference).await?;
    }
    Ok(())
}

fn credential_pool_member_references(policy: &CredentialPoolRuntimePolicy) -> HashSet<&str> {
    policy
        .members()
        .iter()
        .map(CredentialPoolRuntimeMember::credential_reference)
        .collect()
}

/// Reads the durable exclusions governing one pool under its members' locks.
///
/// Selection and the availability-successor test must apply exactly the same
/// predicate. Reading only same-turn chain exclusions let the observation
/// commit create a successor no member could serve, and reading action rows
/// without the profile locks let a concurrent quarantine commit between the
/// read and the dispatch it was supposed to prevent.
pub(super) async fn load_durable_pool_exclusions(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    policy: &CredentialPoolRuntimePolicy,
) -> Result<DurablePoolExclusions, ModelCallRepositoryError> {
    let members = credential_pool_member_references(policy);
    lock_credential_pool_action_heads(connection, policy).await?;
    let policy_id = credential_pool_records::retain_policy(connection, policy).await?;
    let mut excluded = sqlx::query_scalar::<_, String>(
        "SELECT credential_reference
           FROM credential_pool_chain_exclusion
          WHERE session_id = $1
            AND turn_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(turn_id_to_uuid(turn))
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let member_references = policy
        .members()
        .iter()
        .map(|member| member.credential_reference().to_owned())
        .collect::<Vec<_>>();
    let completed_references = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT call.credential_reference
           FROM model_call AS call
           JOIN model_call_credential_pool_policy AS call_policy
             ON call_policy.model_call_id = call.model_call_id
          WHERE call.session_id = $1
            AND call_policy.pool_name = $2
            AND call.state_kind = 'terminal'
            AND call.terminal_disposition_kind = 'completed'",
    )
    .bind(session_id_to_uuid(session))
    .bind(policy.name())
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    excluded.extend(crate::oauth_credential::quarantined_profiles(connection, policy).await?);
    let actions = sqlx::query_as::<_, (i64, String, String, Uuid, Uuid)>(
        "SELECT action_id, credential_reference, action_kind,
                observed_session_id, observed_turn_id
           FROM credential_pool_member_action AS action
          WHERE consumed_turn_id IS NULL
            AND (EXISTS (SELECT 1 FROM credential_exclusion_state AS exclusion
                         WHERE exclusion.action_id = action.action_id AND exclusion.active
                           AND (exclusion.pool_policy_id = $1 OR exclusion.kind = 'profile_quarantine'))
                 OR (NOT EXISTS (SELECT 1 FROM credential_exclusion_state AS exclusion
                                 WHERE exclusion.action_id = action.action_id)
                     AND (action.pool_name = $2 OR action.action_kind = 'quarantine')))",
    )
    .bind(policy_id)
    .bind(policy.name())
    .fetch_all(&mut *connection)
    .await?;
    let mut pending_consumed_actions = Vec::new();
    for (action_id, reference, action_kind, observed_session, observed_turn) in actions {
        let applies = match action_kind.as_str() {
            "quarantine" => true,
            "avoid_new_sessions" => !completed_references.contains(&reference),
            "switch_next_turn" => {
                observed_session == session_id_to_uuid(session)
                    && observed_turn != turn_id_to_uuid(turn)
            }
            _ => {
                return Err(ModelCallCorruption::Unsupported {
                    field: "credential_pool_member_action action_kind",
                    value: action_kind,
                }
                .into());
            }
        };
        // A global quarantine can name a profile this pool never ranked.
        // Selection would ignore it, but the successor backoff is derived from
        // the size of this set, so an unrelated quarantine elsewhere must not
        // push the first rotation onto a later exponential tier.
        if applies && members.contains(reference.as_str()) {
            excluded.insert(reference);
            if action_kind == "switch_next_turn" {
                pending_consumed_actions.push(action_id);
            }
        }
    }
    excluded.extend(sqlx::query_scalar::<_, String>(
        "SELECT profile FROM credential_exclusion_state WHERE active AND kind = 'profile_quarantine' AND origin <> 'pool_trigger' AND profile = ANY($1)")
        .bind(&member_references).fetch_all(&mut *connection).await?);
    let mut snapshots = Vec::new();
    for member in policy.members().iter().filter(|member| {
        policy.tie_break == CredentialPoolRuntimeTieBreak::LeastUsed
            || member
                .headroom_reserve_percent
                .or(policy.headroom_reserve_percent)
                .is_some()
    }) {
        let snapshot = crate::credential_capacity::load_credential_rate_limits(
            connection,
            member.credential_reference(),
        )
        .await?;
        snapshots.push((member, snapshot));
    }
    let (observed_at, transient_exclusions): (sqlx::types::time::OffsetDateTime, Vec<String>) =
        sqlx::query_as(
            "WITH observation AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
             SELECT observation.observed_at,
                    ARRAY(SELECT DISTINCT credential_reference
                          FROM credential_pool_transient_exclusion
                          WHERE credential_reference = ANY($1)
                            AND reset_at > observation.observed_at)
               FROM observation",
        )
        .bind(&member_references)
        .fetch_one(&mut *connection)
        .await?;
    let now = std::time::SystemTime::from(observed_at);
    excluded.extend(transient_exclusions);
    let mut headroom = HashMap::new();
    for (member, snapshot) in snapshots {
        let remaining = snapshot
            .as_ref()
            .and_then(|snapshot| capacity_headroom(snapshot, now));
        let reserve = member
            .headroom_reserve_percent
            .or(policy.headroom_reserve_percent);
        if reserve.is_some_and(|reserve| {
            remaining.is_some_and(|remaining| remaining <= i64::from(reserve))
        }) {
            excluded.insert(member.credential_reference().to_owned());
        }
        headroom.insert(member.credential_reference().to_owned(), remaining);
    }
    Ok(DurablePoolExclusions {
        observed_at,
        excluded,
        pending_consumed_actions,
        headroom,
    })
}

/// Returns the member this session most recently prepared a call on.
///
/// Selection is sticky across turns: once a displacement moves a session off
/// its preferred member, the following turn must stay on the replacement while
/// it remains admissible instead of returning to a member whose immediate
/// exclusion merely expired with the turn.
async fn load_session_sticky_pool_member(
    connection: &mut PgConnection,
    session: SessionId,
    pool_name: &str,
) -> Result<Option<String>, ModelCallRepositoryError> {
    sqlx::query_scalar::<_, String>(
        "SELECT call.credential_reference
           FROM model_call AS call
           JOIN model_call_credential_pool_policy AS call_policy
             ON call_policy.model_call_id = call.model_call_id
           JOIN turn_lifecycle AS lifecycle
             ON lifecycle.turn_id = call.turn_id
            AND lifecycle.session_id = call.session_id
          WHERE call.session_id = $1
            AND call_policy.pool_name = $2
          ORDER BY lifecycle.acceptance_position DESC,
                   EXISTS (
                       SELECT 1
                         FROM credential_pool_availability_successor AS successor
                        WHERE successor.predecessor_model_call_id = call.model_call_id
                   ) ASC
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .bind(pool_name)
    .fetch_optional(&mut *connection)
    .await
    .map_err(Into::into)
}

pub(super) async fn select_runtime_pool_credential(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    attempt: TurnAttemptId,
    serving_evidence: PreparedServingEvidence<'_>,
    default_reference: ModelCallCredentialReference,
    policies: &CredentialPoolRuntimeCatalog,
) -> Result<SelectedRuntimePoolCredential, ModelCallRepositoryError> {
    let predecessor: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT successor.predecessor_model_call_id,
                successor.cause_kind = 'quota_exhausted' OR EXISTS (
                    SELECT 1
                      FROM credential_pool_chain_exclusion AS exclusion
                     WHERE exclusion.predecessor_model_call_id =
                           successor.predecessor_model_call_id
                ) AS rotated
           FROM credential_pool_availability_successor AS successor
          WHERE successor.successor_turn_attempt_id = $1
          UNION ALL SELECT waiting.predecessor_model_call_id, EXISTS (SELECT 1 FROM model_call predecessor WHERE predecessor.model_call_id = waiting.predecessor_model_call_id AND predecessor.terminal_provider_failure_cause = 'quota_exhausted') OR EXISTS (SELECT 1 FROM credential_pool_chain_exclusion exclusion WHERE exclusion.predecessor_model_call_id = waiting.predecessor_model_call_id) FROM credential_availability_wait_release release JOIN credential_availability_wait waiting USING (wait_attempt_id) WHERE release.turn_attempt_id = $1 AND waiting.predecessor_model_call_id IS NOT NULL",
    )
    .bind(attempt.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let (policy, predecessor_reference, predecessor_rotated) = match predecessor {
        Some((predecessor, rotated)) => {
            let policy = load_call_pool_policy(connection, predecessor)
                .await?
                .ok_or(ModelCallCorruption::Missing(
                    "availability successor predecessor pool policy",
                ))?;
            let row = sqlx::query(
                "SELECT credential_reference,
                        effective_provider_model_identity_id,
                        prepared_credential_model_family,
                        prepared_max_output_tokens,
                        prepared_context_window_tokens,
                        prepared_provider_compaction_replay
                   FROM model_call
                  WHERE model_call_id = $1",
            )
            .bind(predecessor)
            .fetch_one(&mut *connection)
            .await?;
            let reference = row.try_get::<String, _>("credential_reference")?;
            let prepared_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                row.try_get("effective_provider_model_identity_id")?,
            ));
            let prepared_family =
                row.try_get::<Option<String>, _>("prepared_credential_model_family")?;
            let prepared_limit = decode_prepared_usage_limit(&row, prepared_target)?;
            if !prepared_serving_configuration_is_compatible(
                prepared_target,
                prepared_family.as_deref(),
                prepared_limit,
                serving_evidence.clone(),
            ) {
                return Err(ModelCallRepositoryError::InvalidTransition(
                    "availability successor serving configuration changed",
                ));
            }
            (Some(policy), Some(reference), rotated)
        }
        None => (
            credential_pool_records::admission_policy(
                connection,
                attempt,
                serving_evidence.effective_target,
                policies,
            )
            .await?,
            None,
            false,
        ),
    };
    let Some(policy) = policy else {
        return Ok(SelectedRuntimePoolCredential {
            wait: None,
            reference: Some(default_reference),
            policy: None,
            pending_consumed_actions: Vec::new(),
        });
    };
    let durable = load_durable_pool_exclusions(connection, session, turn, &policy).await?;
    let observed_at = durable.observed_at;
    let profiles = policy
        .members()
        .iter()
        .map(CredentialPoolRuntimeMember::credential_reference)
        .collect::<Vec<_>>();
    let mut bounded = crate::credential_invocations::bounded_members(connection, &profiles).await?;
    bounded.retain(|member| !durable.excluded.contains(&member.profile));
    let mut excluded = durable.excluded.clone();
    excluded.extend(bounded.iter().map(|member| member.profile.clone()));
    let headroom = &durable.headroom;
    let sticky_reference = match predecessor_reference {
        // An availability successor continues its predecessor's chain, so the
        // chain position rather than session stickiness governs it.
        Some(_) => None,
        None => load_session_sticky_pool_member(connection, session, policy.name()).await?,
    };
    let start = predecessor_reference
        .as_deref()
        .filter(|_| policy.tie_break == CredentialPoolRuntimeTieBreak::FirstListed)
        .and_then(|reference| {
            policy
                .members()
                .iter()
                .position(|member| member.credential_reference() == reference)
        })
        .map_or(0, |position| position.saturating_add(1));
    let predecessor_member = predecessor_reference
        .as_deref()
        .filter(|reference| !excluded.contains(*reference))
        .and_then(|reference| {
            policy
                .members()
                .iter()
                .find(|member| member.credential_reference() == reference)
        });
    let selected = predecessor_member
        .filter(|_| !predecessor_rotated)
        .or_else(|| {
            if predecessor_reference.is_some() && !predecessor_rotated {
                return None;
            }
            policy
                .members()
                .iter()
                .find(|member| {
                    sticky_reference.as_deref() == Some(member.credential_reference())
                        && !excluded.contains(member.credential_reference())
                })
                .or_else(|| {
                    policy
                        .members()
                        .iter()
                        .skip(start)
                        .chain(policy.members().iter().take(start))
                        .filter(|member| !excluded.contains(member.credential_reference()))
                        .filter(|member| {
                            !predecessor_rotated
                                || predecessor_reference.as_deref()
                                    != Some(member.credential_reference())
                        })
                        .min_by_key(|member| {
                            let remaining = headroom
                                .get(member.credential_reference())
                                .copied()
                                .flatten();
                            match policy.tie_break {
                                CredentialPoolRuntimeTieBreak::FirstListed => {
                                    (0, false, std::cmp::Reverse(None))
                                }
                                CredentialPoolRuntimeTieBreak::LeastUsed => (
                                    member.priority().get(),
                                    remaining.is_some(),
                                    std::cmp::Reverse(remaining),
                                ),
                            }
                        })
                })
        })
        .or(predecessor_member.filter(|_| predecessor_rotated))
        .map(|member| ModelCallCredentialReference::new(member.credential_reference()));
    let retry_contended = predecessor_reference
        .as_deref()
        .is_some_and(|reference| bounded.iter().any(|member| member.profile == reference));
    let wait = if selected.is_none()
        && (predecessor_reference.is_none() || predecessor_rotated || retry_contended)
    {
        super::credential_wait::admission_snapshot(
            connection,
            session,
            turn,
            &policy,
            serving_evidence.effective_target,
            &durable,
            bounded,
        )
        .await?
    } else {
        None
    };
    let releasing_wait: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM credential_availability_wait WHERE wait_attempt_id = $1 AND consumed_by_attempt_id IS NULL) OR EXISTS (SELECT 1 FROM credential_availability_wait_release release JOIN credential_availability_wait waiting USING (wait_attempt_id) WHERE release.turn_attempt_id = $1 AND waiting.predecessor_model_call_id IS NOT NULL)")
        .bind(attempt.into_uuid()).fetch_one(&mut *connection).await?;
    if selected.is_none()
        && wait.is_none()
        && !releasing_wait
        && policy
            .members()
            .iter()
            .all(|member| excluded.contains(member.credential_reference()))
    {
        credential_pool_evidence::record(
            connection,
            session,
            turn,
            attempt,
            &policy,
            observed_at,
            headroom,
        )
        .await?;
    }
    let pending_consumed_actions = if selected.is_some() {
        durable.pending_consumed_actions
    } else {
        Vec::new()
    };
    Ok(SelectedRuntimePoolCredential {
        wait,
        reference: selected,
        policy: Some(policy),
        pending_consumed_actions,
    })
}

pub(super) async fn persist_call_pool_policy(
    connection: &mut PgConnection,
    call: ModelCallId,
    policy: &CredentialPoolRuntimePolicy,
) -> Result<(), ModelCallRepositoryError> {
    crate::oauth_credential::lock_pool_members(connection, policy).await?;
    sqlx::query(
        "INSERT INTO model_call_credential_pool_policy
            (model_call_id, pool_name, on_pool_exhausted,
             on_quota_exhausted, on_rate_limited, on_overloaded,
             on_credential_rejected, tie_break, headroom_reserve_percent, on_headroom_low)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(call.into_uuid())
    .bind(policy.name())
    .bind(policy.on_pool_exhausted.as_str())
    .bind(policy.quota_exhausted.as_str())
    .bind(policy.rate_limited.as_str())
    .bind(policy.overloaded.as_str())
    .bind(policy.credential_rejected.as_str())
    .bind(policy.tie_break.as_str())
    .bind(policy.headroom_reserve_percent.map(i16::from))
    .bind(policy.headroom_low.as_str())
    .execute(&mut *connection)
    .await?;
    for (ordinal, member) in policy.members().iter().enumerate() {
        let ordinal = i32::try_from(ordinal).map_err(|_| {
            ModelCallRepositoryError::InvalidTransition("credential pool ordinal overflow")
        })?;
        sqlx::query(
            "INSERT INTO model_call_credential_pool_member
                (model_call_id, member_ordinal, credential_reference, priority, headroom_reserve_percent)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(call.into_uuid())
        .bind(ordinal)
        .bind(member.credential_reference())
        .bind(i64::from(member.priority().get()))
        .bind(member.headroom_reserve_percent.map(i16::from))
        .execute(&mut *connection)
        .await?;
    }
    let policy_id = credential_pool_records::retain_policy(connection, policy).await?;
    sqlx::query(
        "UPDATE model_call_credential_pool_policy SET pool_policy_id = $2 WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .bind(policy_id)
    .execute(connection)
    .await?;
    Ok(())
}

/// Records one durable exclusion under the profile's action-head lock.
pub(super) async fn persist_credential_pool_member_action(
    connection: &mut PgConnection,
    policy: &CredentialPoolRuntimePolicy,
    action: CredentialPoolRuntimeAction,
    credential_reference: String,
    observation: &CorrelatedModelCallTerminalObservation,
    cause: &str,
) -> Result<(), ModelCallRepositoryError> {
    if action == CredentialPoolRuntimeAction::Stay
        || action == CredentialPoolRuntimeAction::SwitchNow
    {
        return Err(ModelCallRepositoryError::InvalidTransition(
            "non-durable pool action reached durable action persistence",
        ));
    }
    lock_credential_pool_action_head(connection, &credential_reference).await?;
    sqlx::query(
        "INSERT INTO credential_pool_member_action
            (pool_name, credential_reference, action_kind,
             observed_session_id, observed_turn_id,
             observation_model_call_id, cause_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(policy.name())
    .bind(credential_reference)
    .bind(action.as_str())
    .bind(session_id_to_uuid(observation.correlation().session()))
    .bind(turn_id_to_uuid(observation.correlation().turn()))
    .bind(observation.call().into_uuid())
    .bind(cause)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Loads the policy frozen onto one call, or `None` when it carried no pool.
///
/// Deployments without credential pools prepare calls with no policy row at
/// all, so absence is an ordinary shape rather than durable corruption.
pub(super) async fn load_call_pool_policy(
    connection: &mut PgConnection,
    call: Uuid,
) -> Result<Option<CredentialPoolRuntimePolicy>, ModelCallRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT pool_name, on_pool_exhausted,
                on_quota_exhausted, on_rate_limited, on_overloaded,
                on_credential_rejected, tie_break, headroom_reserve_percent, on_headroom_low
           FROM model_call_credential_pool_policy
          WHERE model_call_id = $1",
    )
    .bind(call)
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    let members = sqlx::query_as::<_, (String, i64, Option<i16>)>(
        "SELECT credential_reference, priority, headroom_reserve_percent
           FROM model_call_credential_pool_member
          WHERE model_call_id = $1
          ORDER BY member_ordinal",
    )
    .bind(call)
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(|(reference, priority, reserve)| {
        let priority = u32::try_from(priority)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or(ModelCallCorruption::Inconsistent(
                "credential pool member priority",
            ))?;
        Ok(CredentialPoolRuntimeMember::new(reference, priority)
            .with_headroom_reserve(decode_headroom_reserve(reserve)?))
    })
    .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
    Ok(Some(
        CredentialPoolRuntimePolicy::new(
            row.try_get::<String, _>("pool_name")?,
            Arc::<[CredentialPoolRuntimeMember]>::from(members),
            CredentialPoolRuntimeExhaustion::parse(row.try_get("on_pool_exhausted")?)?,
            CredentialPoolRuntimeAction::parse(row.try_get("on_quota_exhausted")?)?,
            CredentialPoolRuntimeAction::parse(row.try_get("on_rate_limited")?)?,
            CredentialPoolRuntimeAction::parse(row.try_get("on_overloaded")?)?,
            CredentialPoolRuntimeAction::parse(row.try_get("on_credential_rejected")?)?,
        )
        .with_capacity_policy(
            CredentialPoolRuntimeTieBreak::parse(row.try_get("tie_break")?)?,
            decode_headroom_reserve(row.try_get("headroom_reserve_percent")?)?,
            CredentialPoolRuntimeAction::parse(row.try_get("on_headroom_low")?)?,
        ),
    ))
}

fn decode_headroom_reserve(value: Option<i16>) -> Result<Option<u8>, ModelCallRepositoryError> {
    value
        .map(|value| {
            u8::try_from(value)
                .ok()
                .filter(|value| *value <= 99)
                .ok_or_else(|| {
                    ModelCallCorruption::Inconsistent("credential pool headroom reserve").into()
                })
        })
        .transpose()
}

fn capacity_headroom(
    snapshot: &signalbox_domain::ProviderRateLimitSnapshot,
    now: std::time::SystemTime,
) -> Option<i64> {
    let mut remaining = None;
    for window in snapshot.windows() {
        if window.resets_at().is_none_or(|reset| reset <= now) {
            return None;
        }
        remaining = Some(remaining.map_or(*window.remaining_percent(), |prior: i64| {
            prior.min(*window.remaining_percent())
        }));
    }
    remaining
}

pub(super) async fn retain_call_capacity_policy_observation(
    connection: &mut PgConnection,
    observation: &CorrelatedModelCallTerminalObservation,
    snapshot: &signalbox_domain::ProviderRateLimitSnapshot,
) -> Result<(), ModelCallRepositoryError> {
    let policy = load_call_pool_policy(connection, observation.call().into_uuid()).await?;
    acquire_model_call_outbox_order_guard(connection).await?;
    if let Some(policy) = &policy {
        lock_credential_pool_action_heads(connection, policy).await?;
        crate::credential_invocations::lock_profiles(
            connection,
            &policy
                .members()
                .iter()
                .map(CredentialPoolRuntimeMember::credential_reference)
                .collect::<Vec<_>>(),
        )
        .await?;
    }
    let retained = crate::credential_capacity::retain_call_rate_limits(
        connection,
        observation.call(),
        snapshot,
    )
    .await?;
    let Some(policy) = policy.filter(|policy| {
        retained
            && observation.provider_failure_cause().is_none()
            && policy.headroom_low != CredentialPoolRuntimeAction::Stay
    }) else {
        return Ok(());
    };
    let reference: String =
        sqlx::query_scalar("SELECT credential_reference FROM model_call WHERE model_call_id = $1")
            .bind(observation.call().into_uuid())
            .fetch_one(&mut *connection)
            .await?;
    let reserve = policy
        .members()
        .iter()
        .find(|member| member.credential_reference() == reference)
        .and_then(|member| member.headroom_reserve_percent)
        .or(policy.headroom_reserve_percent)
        .unwrap_or(0);
    if capacity_headroom(snapshot, std::time::SystemTime::now())
        .is_some_and(|remaining| remaining <= i64::from(reserve))
    {
        persist_credential_pool_member_action(
            connection,
            &policy,
            policy.headroom_low,
            reference,
            observation,
            "headroom_low",
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod capacity_policy_tests {
    use super::capacity_headroom;
    use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
    use std::time::{Duration, SystemTime};

    #[test]
    fn headroom_uses_the_more_constrained_window() {
        let now = SystemTime::UNIX_EPOCH;
        let reset = Some(now + Duration::from_secs(1));
        let snapshot = ProviderRateLimitSnapshot::new(
            now,
            vec![
                ProviderRateLimitWindow::new(73, None, reset),
                ProviderRateLimitWindow::new(21, None, reset),
            ],
        );
        assert_eq!(capacity_headroom(&snapshot, now), Some(21));
    }

    #[test]
    fn an_unknown_window_makes_member_capacity_unknown() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        for reset in [None, Some(now), Some(SystemTime::UNIX_EPOCH)] {
            let snapshot = ProviderRateLimitSnapshot::new(
                now,
                vec![
                    ProviderRateLimitWindow::new(73, None, Some(now + Duration::from_secs(1))),
                    ProviderRateLimitWindow::new(21, None, reset),
                ],
            );
            assert_eq!(capacity_headroom(&snapshot, now), None, "reset {reset:?}");
        }
        assert_eq!(
            capacity_headroom(&ProviderRateLimitSnapshot::new(now, Vec::new()), now),
            None
        );
    }
}
