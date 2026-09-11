//! Admission of unfinished pull requests already observed when a revision activates.
use crate::{DispatchAdmission, RepoWatchStore, StoreError};
use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_session_ownership::{
    OffsetDateTime, RepoWatchEventId, RepoWatchObservation, RepoWatchRule, RepositorySlug,
};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

async fn observation(
    tx: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
) -> Result<Option<RepoWatchObservation>, StoreError> {
    let payload: Option<Value> =
        sqlx::query_scalar("SELECT comparison_baseline FROM repository_state WHERE repository=$1")
            .bind(repository.as_str())
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
    payload
        .as_ref()
        .map(|payload| {
            crate::observation_decode::observation(payload).ok_or(StoreError::InvalidRetainedEvent)
        })
        .transpose()
}

pub(crate) async fn seed(
    tx: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    rule: &RepoWatchRule,
) -> Result<(), StoreError> {
    let Some(observed) = observation(tx, repository).await? else {
        return Ok(());
    };
    let rows: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT DISTINCT ON (pull_request_number) event_id, normalized_payload
         FROM gh_readable_event WHERE repository=$1 AND pull_request_number IS NOT NULL
         ORDER BY pull_request_number, repository_event_ordinal DESC",
    )
    .bind(repository.as_str())
    .fetch_all(&mut **tx)
    .await?;
    for (id, payload) in rows {
        let previous = crate::event_decode::event(RepoWatchEventId::from_uuid(id), &payload)
            .ok_or(StoreError::InvalidRetainedEvent)?;
        if crate::retry::reevaluation_event(
            rule,
            &previous,
            observed.state().pull_requests(),
            crate::retry::MatchSource::Activation,
        )
        .is_some()
        {
            sqlx::query("INSERT INTO rule_activation_candidate (repository,rule_id,rule_revision,event_id) VALUES ($1,$2,$3,$4)")
                .bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get())).bind(id)
                .execute(&mut **tx).await?;
        }
    }
    Ok(())
}

async fn remove(
    tx: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    rule: &RepoWatchRule,
    id: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM rule_activation_candidate WHERE repository=$1 AND rule_id=$2 AND rule_revision=$3 AND event_id=$4")
        .bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get())).bind(id)
        .execute(&mut **tx).await?;
    Ok(())
}

impl RepoWatchStore {
    pub(crate) async fn activate_next<Ids, Factory, Codec>(
        &self,
        repository: &RepositorySlug,
        rule: &RepoWatchRule,
        ids: &mut Ids,
        factory: &mut Factory,
        codec: &mut Codec,
        now: OffsetDateTime,
    ) -> Result<bool, crate::dispatch::EvaluationError<Factory::Error>>
    where
        Ids: crate::DispatchReferenceGenerator,
        Factory: crate::CreateSessionCommandFactory,
        Codec: crate::SessionCommandCodec,
    {
        use crate::dispatch::EvaluationError;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(StoreError::from)
            .map_err(EvaluationError::Store)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(crate::CONFIGURATION_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::from)
            .map_err(EvaluationError::Store)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1,0))")
            .bind(repository.as_str())
            .execute(&mut *tx)
            .await
            .map_err(StoreError::from)
            .map_err(EvaluationError::Store)?;
        let rows: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT c.event_id,e.normalized_payload FROM rule_activation_candidate c
             JOIN rule r ON r.repository=c.repository AND r.rule_id=c.rule_id AND r.active_revision=c.rule_revision
             JOIN gh_readable_event e USING(event_id)
             WHERE c.repository=$1 AND c.rule_id=$2 AND c.rule_revision=$3
               AND NOT EXISTS (SELECT 1 FROM rule_evaluation_cursor WHERE repository=$1 AND rule_id=$2 AND rule_revision=$3 AND effect_id IS NOT NULL)
             ORDER BY e.repository_event_ordinal",
        ).bind(repository.as_str()).bind(rule.id().as_str()).bind(Decimal::from(rule.version().get()))
            .fetch_all(&mut *tx).await.map_err(StoreError::from).map_err(EvaluationError::Store)?;
        if rows.is_empty() {
            return Ok(false);
        }
        let Some(observed) = observation(&mut tx, repository)
            .await
            .map_err(EvaluationError::Store)?
        else {
            return Ok(false);
        };
        for (id, payload) in rows {
            let previous = crate::event_decode::event(RepoWatchEventId::from_uuid(id), &payload)
                .ok_or(EvaluationError::Store(StoreError::InvalidRetainedEvent))?;
            let Some(event) = crate::retry::reevaluation_event(
                rule,
                &previous,
                observed.state().pull_requests(),
                crate::retry::MatchSource::Activation,
            ) else {
                remove(&mut tx, repository, rule, id)
                    .await
                    .map_err(EvaluationError::Store)?;
                continue;
            };
            let key = crate::dispatch::singleton_key(rule.singleton_per(), &event, Some(&observed))
                .ok_or(EvaluationError::Store(StoreError::InvalidDispatchBatch))?;
            let activation = serde_json::to_vec(&crate::normalized_event_payload(&event))
                .map_err(|_| EvaluationError::Store(StoreError::InvalidRetainedEvent))?;
            let batch = crate::plan_rule_commands(rule, &event, ids, factory)
                .map_err(EvaluationError::Plan)?;
            let result = self
                .record_commands_transaction(
                    &mut tx,
                    &batch,
                    now,
                    codec,
                    Some((&key, rule.cooldown())),
                    Some(crate::Reevaluation::Activation(&activation)),
                )
                .await
                .map_err(EvaluationError::Store)?;
            match result {
                DispatchAdmission::Inserted | DispatchAdmission::Replayed { .. } => {
                    remove(&mut tx, repository, rule, id)
                        .await
                        .map_err(EvaluationError::Store)?;
                    tx.commit()
                        .await
                        .map_err(StoreError::from)
                        .map_err(EvaluationError::Store)?;
                    return Ok(true);
                }
                DispatchAdmission::ConflictingReuse => {
                    return Err(EvaluationError::ConflictingDispatch);
                }
                DispatchAdmission::Suppressed | DispatchAdmission::InactiveRule => {}
            }
        }
        tx.commit()
            .await
            .map_err(StoreError::from)
            .map_err(EvaluationError::Store)?;
        Ok(false)
    }
}
