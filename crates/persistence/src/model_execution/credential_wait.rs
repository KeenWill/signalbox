//! Durable credential-admission waits and their transaction-local release.
use super::credential_pool::{SelectedRuntimePoolCredential, load_durable_pool_exclusions};
use super::credential_pool_evidence::Candidate;
use super::*;
use crate::credential_pool_exhaustion::CredentialPoolExclusion;
use signalbox_domain::{
    CredentialAvailabilityWait, CredentialAvailabilityWaitCause, ModelCallExecution, TurnAttemptId,
};
use sqlx::Row;
use sqlx::types::time::OffsetDateTime;

pub(super) struct WaitSnapshot {
    pub(super) members: Vec<Vec<Candidate>>,
    pub(super) target: ResolvedProviderTarget,
}

fn member_deadline(exclusions: &[Candidate]) -> Option<i64> {
    if exclusions.is_empty() || exclusions.iter().any(|exclusion| exclusion.reset.is_none()) {
        return None;
    }
    exclusions
        .iter()
        .filter_map(|exclusion| exclusion.reset)
        .max()
}

fn selects_wait(members: &[Vec<Candidate>]) -> bool {
    members.iter().all(|exclusions| !exclusions.is_empty())
        && members.iter().any(|exclusions| {
            !exclusions.is_empty()
                && exclusions.iter().all(|exclusion| {
                    !matches!(
                        exclusion.exclusion,
                        CredentialPoolExclusion::ChainExclusion { .. }
                    )
                })
        })
}

pub(super) async fn exhaustion_snapshot(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
    policy: &CredentialPoolRuntimePolicy,
    target: ResolvedProviderTarget,
    excluded: &super::credential_pool::DurablePoolExclusions,
) -> Result<Option<WaitSnapshot>, ModelCallRepositoryError> {
    if policy.on_pool_exhausted != CredentialPoolRuntimeExhaustion::Park {
        return Ok(None);
    }
    let members = credential_pool_evidence::snapshot(
        connection,
        session,
        turn,
        policy,
        excluded.observed_at,
        &excluded.headroom,
    )
    .await?;
    Ok(selects_wait(&members).then_some(WaitSnapshot { members, target }))
}

pub(super) async fn park_initial(
    connection: &mut PgConnection,
    execution: &ModelCallExecution,
    selected: Option<&SelectedRuntimePoolCredential>,
) -> Result<Option<CredentialAvailabilityWait>, ModelCallRepositoryError> {
    let Some(selected) = selected else {
        return Ok(None);
    };
    let (Some(snapshot), Some(policy)) = (&selected.wait, &selected.policy) else {
        return Ok(None);
    };
    let ended = execution.yield_to_credential_availability().map_err(|_| {
        ModelCallRepositoryError::InvalidTransition("credential wait requires call-free admission")
    })?;
    let wait = CredentialAvailabilityWait::new(
        ended.id(),
        execution.admission_snapshot().frontier().snapshot(),
        CredentialAvailabilityWaitCause::Exhausted,
    );
    let predecessor: Option<(Uuid, bool)> = sqlx::query_as("SELECT predecessor_model_call_id, COALESCE(non_acceptance_proven, false) AS non_acceptance_proven FROM credential_pool_availability_successor WHERE successor_turn_attempt_id = $1")
        .bind(ended.id().into_uuid()).fetch_optional(&mut *connection).await?;
    let policy_id = credential_pool_records::retain_policy(connection, policy).await?;
    let deadline = snapshot
        .members
        .iter()
        .filter_map(|member| member_deadline(member))
        .min()
        .map(|millis| OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000))
        .transpose()
        .map_err(|_| ModelCallCorruption::Inconsistent("credential wait deadline"))?;
    sqlx::query("INSERT INTO credential_availability_wait (wait_attempt_id, session_id, turn_id, frontier_id, pool_policy_id, effective_target_id, cause, deadline, predecessor_model_call_id, predecessor_non_acceptance_proven) VALUES ($1,$2,$3,$4,$5,$6,'exhausted',$7,$8,$9)")
        .bind(ended.id().into_uuid()).bind(execution.session().into_uuid()).bind(execution.turn().into_uuid())
        .bind(wait.frontier().into_uuid()).bind(policy_id).bind(snapshot.target.identity().into_uuid()).bind(deadline).bind(predecessor.map(|(call, _)| call)).bind(predecessor.map(|(_, proof)| proof))
        .execute(&mut *connection).await?;
    for (ordinal, (member, exclusions)) in
        policy.members().iter().zip(&snapshot.members).enumerate()
    {
        let ordinal = i32::try_from(ordinal)
            .map_err(|_| ModelCallCorruption::Inconsistent("credential wait ordinal"))?;
        sqlx::query("INSERT INTO credential_availability_wait_member (wait_attempt_id, pool_policy_id, ordinal, profile, exclusions) VALUES ($1,$2,$3,$4,$5)")
            .bind(ended.id().into_uuid()).bind(policy_id).bind(ordinal).bind(member.credential_reference())
            .bind(serde_json::to_value(exclusions).map_err(|_| ModelCallCorruption::Inconsistent("credential wait exclusions"))?)
            .execute(&mut *connection).await?;
    }
    super::persist_disposition::persist_ended_attempt(
        connection,
        execution.session(),
        execution.turn(),
        &ended,
    )
    .await?;
    let rows = sqlx::query("UPDATE turn_lifecycle SET active_phase_kind = 'awaiting_credential_availability', current_attempt_id = NULL, active_tool_round_call_id = NULL WHERE turn_id = $1 AND session_id = $2 AND state_kind = 'active' AND active_phase_kind = 'running' AND current_attempt_id = $3")
        .bind(execution.turn().into_uuid()).bind(execution.session().into_uuid()).bind(ended.id().into_uuid())
        .execute(&mut *connection).await?.rows_affected();
    require_single(rows, "credential wait phase")?;
    Ok(Some(wait))
}

pub(crate) async fn load_phase(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<CredentialAvailabilityWait, ModelCallRepositoryError> {
    sqlx::query("SELECT assert_credential_availability_wait($1)")
        .bind(turn.into_uuid())
        .execute(&mut *connection)
        .await?;
    let row = sqlx::query("SELECT wait_attempt_id, frontier_id, cause, deadline FROM credential_availability_wait WHERE session_id = $1 AND turn_id = $2 AND consumed_by_attempt_id IS NULL")
        .bind(session.into_uuid()).bind(turn.into_uuid()).fetch_one(&mut *connection).await?;
    let cause = match row.try_get::<String, _>("cause")?.as_str() {
        "exhausted" => CredentialAvailabilityWaitCause::Exhausted,
        _ => return Err(ModelCallCorruption::Inconsistent("credential wait cause").into()),
    };
    let evidence: Vec<serde_json::Value> = sqlx::query_scalar("SELECT exclusions FROM credential_availability_wait_member WHERE wait_attempt_id = $1 ORDER BY ordinal")
        .bind(row.try_get::<Uuid, _>("wait_attempt_id")?).fetch_all(&mut *connection).await?;
    let members = evidence
        .into_iter()
        .map(serde_json::from_value::<Vec<Candidate>>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ModelCallCorruption::Inconsistent("credential wait exclusion evidence"))?;
    let deadline: Option<OffsetDateTime> = row.try_get("deadline")?;
    let expected = members
        .iter()
        .filter_map(|member| member_deadline(member))
        .min();
    if deadline.map(|deadline| deadline.unix_timestamp_nanos() / 1_000_000)
        != expected.map(i128::from)
        || (cause == CredentialAvailabilityWaitCause::Exhausted && !selects_wait(&members))
    {
        return Err(
            ModelCallCorruption::Inconsistent("credential wait evidence and deadline").into(),
        );
    }
    Ok(CredentialAvailabilityWait::new(
        TurnAttemptId::from_uuid(row.try_get("wait_attempt_id")?),
        ContextFrontierId::from_uuid(row.try_get("frontier_id")?),
        cause,
    ))
}

pub(super) async fn prepare_release(
    connection: &mut PgConnection,
    repository: &PostgresModelCallRepository,
    session_id: SessionId,
    successor: TurnAttemptId,
) -> Result<Option<CredentialAvailabilityWait>, ModelCallRepositoryError> {
    let waiting = sqlx::query("SELECT wait_attempt_id, turn_id, pool_policy_id, credential_wait_is_eligible(wait_attempt_id) AS eligible FROM credential_availability_wait WHERE session_id = $1 AND consumed_by_attempt_id IS NULL")
        .bind(session_id.into_uuid()).fetch_optional(&mut *connection).await?;
    let Some(waiting) = waiting else {
        return Ok(None);
    };
    let turn = TurnId::from_uuid(waiting.try_get("turn_id")?);
    let wait = load_phase(connection, session_id, turn).await?;
    if !waiting.try_get::<bool, _>("eligible")? {
        return Ok(Some(wait));
    }
    super::credential_pool::acquire_model_call_outbox_order_guard(connection).await?;
    let policy =
        credential_pool_records::load_policy(connection, waiting.try_get("pool_policy_id")?)
            .await?;
    super::credential_pool::lock_credential_pool_action_heads(connection, &policy).await?;
    sqlx::query("SAVEPOINT credential_wait_admission")
        .execute(&mut *connection)
        .await?;
    consume_wait(connection, session_id, turn, wait, successor).await?;
    let execution = Box::pin(super::live_turn::require_live_execution(
        connection,
        session_id,
        &repository.targets,
    ))
    .await?;
    let fast = execution
        .configuration()
        .effective()
        .model_settings()
        .effective()
        .fast_mode();
    let Ok(resolved) = repository
        .targets
        .resolve(*execution.configuration().effective().model())
    else {
        sqlx::query("RELEASE SAVEPOINT credential_wait_admission")
            .execute(&mut *connection)
            .await?;
        return Ok(None);
    };
    let target = resolved.target();
    let serving = super::credential_pool::prepared_serving_evidence(
        repository.credential_families.as_ref(),
        &repository.continuation_usage_limits,
        target,
        fast,
    );
    let selected = super::credential_pool::select_runtime_pool_credential(
        connection,
        session_id,
        turn,
        successor,
        serving,
        repository.credential_reference.clone(),
        &repository.credential_pools,
    )
    .await?;
    sqlx::query("ROLLBACK TO SAVEPOINT credential_wait_admission")
        .execute(&mut *connection)
        .await?;
    sqlx::query("RELEASE SAVEPOINT credential_wait_admission")
        .execute(&mut *connection)
        .await?;
    if let Some(snapshot) = selected.wait {
        let policy = selected
            .policy
            .ok_or(ModelCallCorruption::Missing("credential wait policy"))?;
        let deadline = snapshot
            .members
            .iter()
            .filter_map(|member| member_deadline(member))
            .min()
            .map(|millis| OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000))
            .transpose()
            .map_err(|_| ModelCallCorruption::Inconsistent("credential wait deadline"))?;
        sqlx::query("UPDATE credential_availability_wait SET eligible = false, cause = 'exhausted', deadline = $2 WHERE wait_attempt_id = $1")
            .bind(wait.attempt().into_uuid()).bind(deadline).execute(&mut *connection).await?;
        for (ordinal, (member, exclusions)) in
            policy.members().iter().zip(snapshot.members).enumerate()
        {
            let ordinal = i32::try_from(ordinal)
                .map_err(|_| ModelCallCorruption::Inconsistent("credential wait ordinal"))?;
            let rows = sqlx::query("UPDATE credential_availability_wait_member SET exclusions = $4 WHERE wait_attempt_id = $1 AND ordinal = $2 AND profile = $3")
                .bind(wait.attempt().into_uuid()).bind(ordinal).bind(member.credential_reference())
                .bind(serde_json::to_value(exclusions).map_err(|_| ModelCallCorruption::Inconsistent("credential wait exclusions"))?)
                .execute(&mut *connection).await?.rows_affected();
            require_single(rows, "credential wait evidence rewrite")?;
        }
        return Ok(Some(CredentialAvailabilityWait::new(
            wait.attempt(),
            wait.frontier(),
            CredentialAvailabilityWaitCause::Exhausted,
        )));
    }
    consume_wait(connection, session_id, turn, wait, successor).await?;
    Ok(None)
}

async fn consume_wait(
    connection: &mut PgConnection,
    session_id: SessionId,
    turn: TurnId,
    wait: CredentialAvailabilityWait,
    successor: TurnAttemptId,
) -> Result<(), ModelCallRepositoryError> {
    sqlx::query("INSERT INTO turn_attempt (turn_attempt_id, turn_id, session_id, continued_from_attempt_id, state_kind) VALUES ($1,$2,$3,$4,'prepared')")
        .bind(successor.into_uuid()).bind(turn.into_uuid()).bind(session_id.into_uuid()).bind(wait.attempt().into_uuid())
        .execute(&mut *connection).await?;
    sqlx::query("INSERT INTO credential_availability_wait_release (turn_attempt_id, wait_attempt_id) VALUES ($1,$2)")
        .bind(successor.into_uuid()).bind(wait.attempt().into_uuid()).execute(&mut *connection).await?;
    sqlx::query("UPDATE credential_availability_wait SET consumed_by_attempt_id = $2 WHERE wait_attempt_id = $1")
        .bind(wait.attempt().into_uuid()).bind(successor.into_uuid()).execute(&mut *connection).await?;
    sqlx::query("UPDATE turn_lifecycle SET active_phase_kind = 'running', current_attempt_id = $2 WHERE turn_id = $1")
        .bind(turn.into_uuid()).bind(successor.into_uuid()).execute(&mut *connection).await?;
    Ok(())
}

pub(super) async fn park_failed(
    connection: &mut PgConnection,
    execution: &ModelCallExecution,
    policy: &CredentialPoolRuntimePolicy,
    observation: &signalbox_domain::CorrelatedModelCallTerminalObservation,
    successor_attempt: TurnAttemptId,
    cause: ProviderModelCallFailureCause,
    targets: &ModelTargetCatalog,
) -> Result<Option<CredentialAvailabilityWait>, ModelCallRepositoryError> {
    if policy.on_pool_exhausted != CredentialPoolRuntimeExhaustion::Park {
        return Ok(None);
    }
    let effective_target: Uuid = sqlx::query_scalar(
        "SELECT effective_provider_model_identity_id FROM model_call WHERE model_call_id = $1",
    )
    .bind(observation.call().into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    let target = ResolvedProviderTarget::naming(
        signalbox_domain::ProviderModelIdentity::from_uuid(effective_target),
    );
    let excluded =
        load_durable_pool_exclusions(connection, execution.session(), execution.turn(), policy)
            .await?;
    let Some(mut snapshot) = exhaustion_snapshot(
        connection,
        execution.session(),
        execution.turn(),
        policy,
        target,
        &excluded,
    )
    .await?
    else {
        return Ok(None);
    };
    let successor = execution
        .clone()
        .apply_availability_successor(observation.clone(), successor_attempt)
        .map_err(|_| {
            ModelCallRepositoryError::InvalidTransition("credential wait successor authority")
        })?;
    let backoff = super::persist_tool_round::availability_retry_backoff(
        cause,
        observation.retry_after(),
        1,
        observation.call(),
    );
    super::persist_tool_round::persist_availability_successor(
        connection,
        &successor,
        observation.usage(),
        cause,
        backoff,
    )
    .await?;
    let refreshed =
        load_durable_pool_exclusions(connection, execution.session(), execution.turn(), policy)
            .await?;
    snapshot.members = credential_pool_evidence::snapshot(
        connection,
        execution.session(),
        execution.turn(),
        policy,
        refreshed.observed_at,
        &refreshed.headroom,
    )
    .await?;
    let fresh = Box::pin(super::live_turn::require_live_execution(
        connection,
        execution.session(),
        targets,
    ))
    .await?;
    let selected = SelectedRuntimePoolCredential {
        reference: None,
        policy: Some(policy.clone()),
        pending_consumed_actions: Vec::new(),
        wait: Some(snapshot),
    };
    park_initial(connection, &fresh, Some(&selected)).await
}

pub(super) async fn fail_released_chain(
    connection: &mut PgConnection,
    execution: &ModelCallExecution,
    selected: Option<&SelectedRuntimePoolCredential>,
    identities: signalbox_domain::FailedModelCallTurnIdentities,
) -> Result<Option<signalbox_domain::FailedModelCallTurn>, ModelCallRepositoryError> {
    let Some(selected) =
        selected.filter(|selected| selected.reference.is_none() && selected.wait.is_none())
    else {
        return Ok(None);
    };
    let predecessor: Option<Uuid> = sqlx::query_scalar("SELECT waiting.predecessor_model_call_id FROM credential_availability_wait_release release JOIN credential_availability_wait waiting USING (wait_attempt_id) WHERE release.turn_attempt_id = $1 AND waiting.predecessor_model_call_id IS NOT NULL")
        .bind(execution.current_attempt().id().into_uuid()).fetch_optional(&mut *connection).await?;
    let Some(predecessor) = predecessor else {
        return Ok(None);
    };
    let policy = selected
        .policy
        .as_ref()
        .ok_or(ModelCallCorruption::Missing(
            "credential wait release policy",
        ))?;
    let exhausted = execution
        .clone()
        .fail_credential_pool_exhausted(policy.name().to_owned(), identities)
        .map_err(|_| {
            ModelCallRepositoryError::InvalidTransition("credential wait terminal release")
        })?;
    let failed = exhausted.failed().clone();
    sqlx::query("INSERT INTO credential_availability_wait_failure (turn_attempt_id, predecessor_model_call_id) VALUES ($1,$2)")
        .bind(execution.current_attempt().id().into_uuid()).bind(predecessor).execute(&mut *connection).await?;
    super::persist_terminal::persist_failed_with_delegated_child_result(
        connection,
        &failed,
        TurnTerminalCause::ModelCallFailed,
        ProviderReportedTokenUsage::unreported(),
        None,
        None,
    )
    .await?;
    Ok(Some(failed))
}

pub(crate) async fn release_for_stop(
    connection: &mut PgConnection,
    interrupt: signalbox_domain::AppliedInterruptCommandResult,
) -> Result<(), ModelCallRepositoryError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM credential_availability_wait WHERE session_id = $1 AND turn_id = $2 AND consumed_by_attempt_id IS NULL)")
        .bind(interrupt.session().into_uuid()).bind(interrupt.proof().predecessor().into_uuid()).fetch_one(&mut *connection).await?;
    if !exists {
        return Ok(());
    }
    let wait = load_phase(
        connection,
        interrupt.session(),
        interrupt.proof().predecessor(),
    )
    .await?;
    consume_wait(
        connection,
        interrupt.session(),
        interrupt.proof().predecessor(),
        wait,
        TurnAttemptId::from_uuid(interrupt.proof().command().into_uuid()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transient(reset: i64) -> Candidate {
        Candidate {
            exclusion: CredentialPoolExclusion::TransientExclusion {
                observation_model_call_id: Uuid::from_u128(1),
            },
            rank: 4,
            action: None,
            reset: Some(reset),
        }
    }

    #[test]
    fn credential_pool_wait_deadline_waits_for_the_whole_member() {
        // Synthetic millisecond deadlines distinguish per-exclusion and per-member minima.
        let members = [vec![transient(30), transient(90)], vec![transient(60)]];
        assert_eq!(
            members
                .iter()
                .filter_map(|member| member_deadline(member))
                .min(),
            Some(60)
        );
        assert_eq!(member_deadline(&members[0]), Some(90));
    }

    #[test]
    fn credential_pool_wait_quarantine_prevents_a_timer_even_with_an_expiring_exclusion() {
        let members = vec![vec![
            transient(30),
            Candidate {
                exclusion: CredentialPoolExclusion::ProfileQuarantine {
                    record_generation: Some(1),
                },
                rank: 0,
                action: None,
                reset: None,
            },
        ]];
        assert_eq!(member_deadline(&members[0]), None);
        assert!(
            selects_wait(&members),
            "an operator can clear the quarantine"
        );
    }

    #[test]
    fn credential_pool_wait_chain_exclusions_never_qualify_for_park() {
        let members = vec![vec![
            transient(30),
            Candidate {
                exclusion: CredentialPoolExclusion::ChainExclusion {
                    predecessor_model_call_id: Uuid::from_u128(2),
                },
                rank: 3,
                action: None,
                reset: None,
            },
        ]];
        assert!(!selects_wait(&members));
        assert_eq!(member_deadline(&members[0]), None);
    }

    #[test]
    fn credential_pool_wait_requires_exhaustion_of_every_member() {
        assert!(!selects_wait(&[vec![transient(30)], vec![]]));
        assert!(!selects_wait(&[]));
    }
}
