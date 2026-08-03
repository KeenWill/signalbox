//! Durable repository-watch rule consumption, singleton admission, and dispatch audit.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    RepoWatchDispatchTransaction, RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome,
    RepoWatchSingletonKey,
};
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, DurableCommandId, RepoWatchActionV1, RepoWatchDispatchId,
    RepoWatchEvent, RepoWatchEventId, RepoWatchRuleId, RepoWatchRuleVersion, RepositorySlug,
    SemanticTranscriptEntryId, SessionId, TurnId,
};
use sqlx::{PgPool, Postgres, Row, Transaction, types::Uuid};

use crate::{
    commit_failure_is_ambiguous, create_session::insert_fresh_prepared, mapping::session_id_to_uuid,
};

/// Database or durable-shape failure while evaluating one repository-watch rule.
#[derive(Debug)]
pub enum RepoWatchDispatchRepositoryError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    EventStore(crate::repo_watch::RepoWatchStoreError),
    SessionCreation(crate::create_session::CreateSessionRepositoryError),
    Corruption(&'static str),
}

impl fmt::Display for RepoWatchDispatchRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(
                formatter,
                "repository-watch dispatch database failure: {error}"
            ),
            Self::CommitAmbiguous(error) => write!(
                formatter,
                "repository-watch dispatch commit outcome is ambiguous: {error}"
            ),
            Self::SessionCreation(error) => error.fmt(formatter),
            Self::EventStore(error) => error.fmt(formatter),
            Self::Corruption(reason) => {
                write!(
                    formatter,
                    "repository-watch dispatch storage is inconsistent: {reason}"
                )
            }
        }
    }
}

impl Error for RepoWatchDispatchRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::EventStore(error) => Some(error),
            Self::SessionCreation(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for RepoWatchDispatchRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// PostgreSQL implementation of atomic repository-watch dispatch admission.
#[derive(Clone, Debug)]
pub struct PostgresRepoWatchDispatchStore {
    pool: PgPool,
    credential_pin: crate::SessionCredentialPin,
}

impl PostgresRepoWatchDispatchStore {
    pub fn new(pool: PgPool, credential_pin: crate::SessionCredentialPin) -> Self {
        Self {
            pool,
            credential_pin,
        }
    }

    /// Establishes one rule after the current durable tail, before its task polls.
    pub async fn activate_rule(
        &self,
        repository: &RepositorySlug,
        rule_id: &RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, repository.as_str()).await?;
        sqlx::query(
            "INSERT INTO repo_watch_rule_activation
                (repository, rule_id, rule_version,
                 after_cursor_generation, after_event_ordinal)
             SELECT $1, $2, $3, tail.cursor_generation, tail.event_ordinal
               FROM (VALUES (true)) AS seed(present)
               LEFT JOIN LATERAL (
                    SELECT cursor_generation, event_ordinal
                      FROM repo_watch_event
                     WHERE repository = $1
                     ORDER BY cursor_generation DESC, event_ordinal DESC
                     LIMIT 1
               ) AS tail ON seed.present
             ON CONFLICT DO NOTHING",
        )
        .bind(repository.as_str())
        .bind(rule_id.as_str())
        .bind(i64::try_from(rule_version.get()).map_err(|_| {
            RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
        })?)
        .execute(&mut *transaction)
        .await?;
        commit(transaction).await
    }

    /// Loads the oldest fact not yet evaluated by this activated rule.
    pub async fn load_next_event(
        &self,
        repository: &RepositorySlug,
        rule_id: &RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    ) -> Result<Option<RepoWatchEvent>, RepoWatchDispatchRepositoryError> {
        let version = stored_rule_version(rule_version)?;
        let event_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT event.event_id
               FROM repo_watch_rule_activation AS activation
               JOIN repo_watch_event AS event
                 ON event.repository = activation.repository
                AND (
                    activation.after_cursor_generation IS NULL
                    OR (event.cursor_generation, event.event_ordinal)
                        > (activation.after_cursor_generation, activation.after_event_ordinal)
                )
              WHERE activation.repository = $1
                AND activation.rule_id = $2
                AND activation.rule_version = $3
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_rule_evaluation AS evaluation
                     WHERE evaluation.repository = activation.repository
                       AND evaluation.rule_id = activation.rule_id
                       AND evaluation.rule_version = activation.rule_version
                       AND evaluation.event_id = event.event_id
                )
              ORDER BY event.cursor_generation, event.event_ordinal
              LIMIT 1",
        )
        .bind(repository.as_str())
        .bind(rule_id.as_str())
        .bind(version)
        .fetch_optional(&self.pool)
        .await?;
        let Some(event_id) = event_id else {
            return Ok(None);
        };
        crate::repo_watch::PostgresRepoWatchStore::new(self.pool.clone())
            .load_event(repository, RepoWatchEventId::from_uuid(event_id))
            .await
            .map_err(RepoWatchDispatchRepositoryError::EventStore)?
            .ok_or(RepoWatchDispatchRepositoryError::Corruption(
                "activated repository-watch event disappeared",
            ))
            .map(Some)
    }

    /// Reserves or replays stable identities for the oldest undelivered action.
    pub async fn prepare_next_delivery(
        &self,
        repository: &RepositorySlug,
        candidates: RepoWatchDeliveryCandidates,
    ) -> Result<Option<RepoWatchPendingDelivery>, RepoWatchDispatchRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, repository.as_str()).await?;
        let row = sqlx::query(
            "SELECT action.dispatch_id, action.action_ordinal, action.event_id,
                    action.session_id, intent.submit_command_id,
                    intent.accepted_input_id, intent.turn_id,
                    intent.cancellation_entry_id, intent.cancellation_frontier_id
               FROM repo_watch_dispatch_action AS action
               JOIN repo_watch_event AS event ON event.event_id = action.event_id
               LEFT JOIN repo_watch_dispatch_delivery AS delivery
                 ON delivery.dispatch_id = action.dispatch_id
                AND delivery.action_ordinal = action.action_ordinal
               LEFT JOIN repo_watch_dispatch_delivery_intent AS intent
                 ON intent.dispatch_id = action.dispatch_id
                AND intent.action_ordinal = action.action_ordinal
              WHERE event.repository = $1
                AND delivery.dispatch_id IS NULL
              ORDER BY action.recorded_at, action.dispatch_id, action.action_ordinal
              LIMIT 1",
        )
        .bind(repository.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let dispatch_id: Uuid = row.try_get("dispatch_id")?;
        let action_ordinal: i32 = row.try_get("action_ordinal")?;
        let event_id: Uuid = row.try_get("event_id")?;
        let session_id: Uuid = row.try_get("session_id")?;
        let submit_command_id: Option<Uuid> = row.try_get("submit_command_id")?;
        let identities = match submit_command_id {
            Some(submit_command_id) => RepoWatchDeliveryCandidates {
                submit_command_id: DurableCommandId::from_uuid(submit_command_id),
                accepted_input_id: AcceptedInputId::from_uuid(row.try_get("accepted_input_id")?),
                turn_id: TurnId::from_uuid(row.try_get("turn_id")?),
                cancellation_entry_id: SemanticTranscriptEntryId::from_uuid(
                    row.try_get("cancellation_entry_id")?,
                ),
                cancellation_frontier_id: ContextFrontierId::from_uuid(
                    row.try_get("cancellation_frontier_id")?,
                ),
            },
            None => {
                sqlx::query(
                    "INSERT INTO repo_watch_dispatch_delivery_intent
                        (dispatch_id, action_ordinal, submit_command_id,
                         accepted_input_id, turn_id, cancellation_entry_id,
                         cancellation_frontier_id)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(dispatch_id)
                .bind(action_ordinal)
                .bind(candidates.submit_command_id.as_uuid())
                .bind(candidates.accepted_input_id.as_uuid())
                .bind(candidates.turn_id.as_uuid())
                .bind(candidates.cancellation_entry_id.as_uuid())
                .bind(candidates.cancellation_frontier_id.as_uuid())
                .execute(&mut *transaction)
                .await?;
                candidates
            }
        };
        commit(transaction).await?;
        Ok(Some(RepoWatchPendingDelivery {
            dispatch_id: RepoWatchDispatchId::from_uuid(dispatch_id),
            action_ordinal,
            event_id: RepoWatchEventId::from_uuid(event_id),
            session_id: SessionId::from_uuid(session_id),
            identities,
        }))
    }

    /// Completes the audit link after the existing submit-input command applies.
    pub async fn record_delivery(
        &self,
        delivery: &RepoWatchPendingDelivery,
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        sqlx::query(
            "INSERT INTO repo_watch_dispatch_delivery
                (dispatch_id, action_ordinal, submit_command_id,
                 accepted_input_id, turn_id)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (dispatch_id, action_ordinal) DO NOTHING",
        )
        .bind(delivery.dispatch_id.as_uuid())
        .bind(delivery.action_ordinal)
        .bind(delivery.identities.submit_command_id.as_uuid())
        .bind(delivery.identities.accepted_input_id.as_uuid())
        .bind(delivery.identities.turn_id.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Stable candidates reserved before a repository-watch context submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchDeliveryCandidates {
    pub submit_command_id: DurableCommandId,
    pub accepted_input_id: AcceptedInputId,
    pub turn_id: TurnId,
    pub cancellation_entry_id: SemanticTranscriptEntryId,
    pub cancellation_frontier_id: ContextFrontierId,
}

/// One dispatched session whose structured context has not yet been delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchPendingDelivery {
    dispatch_id: RepoWatchDispatchId,
    action_ordinal: i32,
    event_id: RepoWatchEventId,
    session_id: SessionId,
    identities: RepoWatchDeliveryCandidates,
}

impl RepoWatchPendingDelivery {
    pub const fn dispatch_id(&self) -> RepoWatchDispatchId {
        self.dispatch_id
    }

    pub const fn action_ordinal(&self) -> i32 {
        self.action_ordinal
    }

    pub const fn event_id(&self) -> RepoWatchEventId {
        self.event_id
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn identities(&self) -> RepoWatchDeliveryCandidates {
        self.identities
    }
}

impl RepoWatchDispatchTransaction for PostgresRepoWatchDispatchStore {
    type Error = RepoWatchDispatchRepositoryError;

    async fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error> {
        match evaluation {
            RepoWatchRuleEvaluation::NotMatched {
                event,
                rule_id,
                rule_version,
            } => {
                self.record_simple_outcome(&event, &rule_id, rule_version, "not_matched")
                    .await
            }
            RepoWatchRuleEvaluation::Matched {
                dispatch_id,
                event,
                rule_id,
                rule_version,
                singleton,
                cooldown,
                actions,
            } => {
                let mut transaction = self.pool.begin().await?;
                let singleton = StoredSingletonKey::from_domain(&singleton);
                lock_text(
                    &mut transaction,
                    &singleton.lock_key(&rule_id, rule_version),
                )
                .await?;
                if let Some(outcome) =
                    load_recorded_evaluation(&mut transaction, event.id(), &rule_id, rule_version)
                        .await?
                {
                    transaction.rollback().await?;
                    return Ok(outcome);
                }
                release_completed_batches(&mut transaction, &rule_id, rule_version, &singleton)
                    .await?;
                if singleton_is_occupied(&mut transaction, &rule_id, rule_version, &singleton)
                    .await?
                {
                    insert_evaluation(
                        &mut transaction,
                        &event,
                        &rule_id,
                        rule_version,
                        "occupied",
                        None,
                    )
                    .await?;
                    commit(transaction).await?;
                    return Ok(RepoWatchRuleEvaluationOutcome::Occupied);
                }
                if singleton_is_cooling_down(&mut transaction, &rule_id, rule_version, &singleton)
                    .await?
                {
                    insert_evaluation(
                        &mut transaction,
                        &event,
                        &rule_id,
                        rule_version,
                        "cooldown",
                        None,
                    )
                    .await?;
                    commit(transaction).await?;
                    return Ok(RepoWatchRuleEvaluationOutcome::Cooldown);
                }
                let action_count = i32::try_from(actions.len()).map_err(|_| {
                    RepoWatchDispatchRepositoryError::Corruption("action count exceeds storage")
                })?;
                let cooldown_seconds = i64::try_from(cooldown.as_secs()).map_err(|_| {
                    RepoWatchDispatchRepositoryError::Corruption("cooldown exceeds storage")
                })?;
                let batch = StoredBatchAdmission {
                    dispatch_id,
                    event: &event,
                    rule_id: &rule_id,
                    rule_version,
                    singleton: &singleton,
                    cooldown_seconds,
                    action_count,
                };
                insert_batch(&mut transaction, batch).await?;
                let mut sessions = Vec::with_capacity(actions.len());
                for (index, action) in actions.into_vec().into_iter().enumerate() {
                    let ordinal = i32::try_from(index + 1).map_err(|_| {
                        RepoWatchDispatchRepositoryError::Corruption(
                            "action ordinal exceeds storage",
                        )
                    })?;
                    let (configured_action, prepared_session) = action.into_parts();
                    let RepoWatchActionV1::DispatchSession(configured_dispatch) = configured_action;
                    let session = prepared_session.applied_result().session();
                    let command = prepared_session.command();
                    let provenance = command.template_provenance().ok_or(
                        RepoWatchDispatchRepositoryError::Corruption(
                            "dispatch session lacks template provenance",
                        ),
                    )?;
                    if provenance.name() != configured_dispatch.template() {
                        return Err(RepoWatchDispatchRepositoryError::Corruption(
                            "dispatch action and prepared template disagree",
                        ));
                    }
                    let command_id = command.command_id();
                    let template_name = provenance.name().as_str().to_owned();
                    let template_digest = provenance.content_digest().as_bytes().to_vec();
                    insert_fresh_prepared(&mut transaction, prepared_session, &self.credential_pin)
                        .await
                        .map_err(RepoWatchDispatchRepositoryError::SessionCreation)?;
                    sqlx::query(
                        "INSERT INTO repo_watch_dispatch_action
                            (dispatch_id, action_ordinal, event_id, session_id,
                             create_command_id, template_name, template_content_digest)
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(dispatch_id.as_uuid())
                    .bind(ordinal)
                    .bind(event.id().as_uuid())
                    .bind(session_id_to_uuid(session))
                    .bind(command_id.as_uuid())
                    .bind(template_name)
                    .bind(template_digest)
                    .execute(&mut *transaction)
                    .await?;
                    sessions.push(session);
                }
                insert_evaluation(
                    &mut transaction,
                    &event,
                    &rule_id,
                    rule_version,
                    "dispatched",
                    Some(dispatch_id),
                )
                .await?;
                commit(transaction).await?;
                Ok(RepoWatchRuleEvaluationOutcome::Dispatched {
                    dispatch_id,
                    sessions: sessions.into_boxed_slice(),
                })
            }
        }
    }
}

impl PostgresRepoWatchDispatchStore {
    async fn record_simple_outcome(
        &self,
        event: &RepoWatchEvent,
        rule_id: &RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
        outcome: &'static str,
    ) -> Result<RepoWatchRuleEvaluationOutcome, RepoWatchDispatchRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        if let Some(recorded) =
            load_recorded_evaluation(&mut transaction, event.id(), rule_id, rule_version).await?
        {
            transaction.rollback().await?;
            return Ok(recorded);
        }
        insert_evaluation(
            &mut transaction,
            event,
            rule_id,
            rule_version,
            outcome,
            None,
        )
        .await?;
        commit(transaction).await?;
        Ok(RepoWatchRuleEvaluationOutcome::NotMatched)
    }
}

#[derive(Clone, Debug)]
struct StoredSingletonKey {
    scope: &'static str,
    repository: Option<String>,
    pull_request: Option<Decimal>,
    stack_root: Option<String>,
}

impl StoredSingletonKey {
    fn from_domain(key: &RepoWatchSingletonKey) -> Self {
        match key {
            RepoWatchSingletonKey::PullRequest { repository, number } => Self {
                scope: "pull_request",
                repository: Some(repository.as_str().to_owned()),
                pull_request: Some(Decimal::from(number.get())),
                stack_root: None,
            },
            RepoWatchSingletonKey::Stack {
                repository,
                root_branch,
            } => Self {
                scope: "stack",
                repository: Some(repository.as_str().to_owned()),
                pull_request: None,
                stack_root: Some(root_branch.as_str().to_owned()),
            },
            RepoWatchSingletonKey::Rule => Self {
                scope: "rule",
                repository: None,
                pull_request: None,
                stack_root: None,
            },
            RepoWatchSingletonKey::Repository { repository } => Self {
                scope: "repo",
                repository: Some(repository.as_str().to_owned()),
                pull_request: None,
                stack_root: None,
            },
        }
    }

    fn lock_key(&self, rule_id: &RepoWatchRuleId, version: RepoWatchRuleVersion) -> String {
        format!(
            "repo-watch\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            rule_id.as_str(),
            version.get(),
            self.scope,
            self.repository.as_deref().unwrap_or(""),
            self.pull_request
                .map_or(String::new(), |value| value.to_string()),
            self.stack_root.as_deref().unwrap_or("")
        )
    }
}

async fn lock_text(
    transaction: &mut Transaction<'_, Postgres>,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

struct StoredBatchAdmission<'a> {
    dispatch_id: RepoWatchDispatchId,
    event: &'a RepoWatchEvent,
    rule_id: &'a RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    singleton: &'a StoredSingletonKey,
    cooldown_seconds: i64,
    action_count: i32,
}

async fn insert_batch(
    transaction: &mut Transaction<'_, Postgres>,
    batch: StoredBatchAdmission<'_>,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_batch
            (dispatch_id, event_id, rule_id, rule_version, singleton_scope,
             singleton_repository, singleton_pull_request_number,
             singleton_stack_root, cooldown_seconds, action_count)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(batch.dispatch_id.as_uuid())
    .bind(batch.event.id().as_uuid())
    .bind(batch.rule_id.as_str())
    .bind(i64::try_from(batch.rule_version.get()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
    })?)
    .bind(batch.singleton.scope)
    .bind(batch.singleton.repository.as_deref())
    .bind(batch.singleton.pull_request)
    .bind(batch.singleton.stack_root.as_deref())
    .bind(batch.cooldown_seconds)
    .bind(batch.action_count)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_evaluation(
    transaction: &mut Transaction<'_, Postgres>,
    event: &RepoWatchEvent,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    outcome: &'static str,
    dispatch: Option<RepoWatchDispatchId>,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    let affected = sqlx::query(
        "INSERT INTO repo_watch_rule_evaluation
            (repository, rule_id, rule_version, event_id,
             cursor_generation, event_ordinal, outcome_kind, dispatch_id)
         SELECT event.repository, $2, $3, event.event_id,
                event.cursor_generation, event.event_ordinal, $4, $5
           FROM repo_watch_event AS event
          WHERE event.event_id = $1
            AND event.repository = $6",
    )
    .bind(event.id().as_uuid())
    .bind(rule_id.as_str())
    .bind(i64::try_from(rule_version.get()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
    })?)
    .bind(outcome)
    .bind(dispatch.map(|value| *value.as_uuid()))
    .bind(event.repository().as_str())
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RepoWatchDispatchRepositoryError::Corruption(
            "evaluated event is absent",
        ));
    }
    Ok(())
}

async fn load_recorded_evaluation(
    transaction: &mut Transaction<'_, Postgres>,
    event: signalbox_domain::RepoWatchEventId,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
) -> Result<Option<RepoWatchRuleEvaluationOutcome>, RepoWatchDispatchRepositoryError> {
    let row = sqlx::query(
        "SELECT evaluation.outcome_kind, evaluation.dispatch_id,
                action.session_id
           FROM repo_watch_rule_evaluation AS evaluation
           LEFT JOIN repo_watch_dispatch_action AS action
             ON action.dispatch_id = evaluation.dispatch_id
          WHERE evaluation.event_id = $1
            AND evaluation.rule_id = $2
            AND evaluation.rule_version = $3
          ORDER BY action.action_ordinal",
    )
    .bind(event.as_uuid())
    .bind(rule_id.as_str())
    .bind(i64::try_from(rule_version.get()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
    })?)
    .fetch_all(&mut **transaction)
    .await?;
    let Some(first) = row.first() else {
        return Ok(None);
    };
    let outcome: String = first.try_get("outcome_kind")?;
    match outcome.as_str() {
        "not_matched" => Ok(Some(RepoWatchRuleEvaluationOutcome::NotMatched)),
        "occupied" => Ok(Some(RepoWatchRuleEvaluationOutcome::Occupied)),
        "cooldown" => Ok(Some(RepoWatchRuleEvaluationOutcome::Cooldown)),
        "dispatched" => {
            let dispatch_id: Uuid = first.try_get("dispatch_id")?;
            let sessions = row
                .iter()
                .map(|row| row.try_get("session_id").map(SessionId::from_uuid))
                .collect::<Result<Vec<_>, sqlx::Error>>()?;
            if sessions.is_empty() {
                return Err(RepoWatchDispatchRepositoryError::Corruption(
                    "dispatch evaluation has no actions",
                ));
            }
            Ok(Some(RepoWatchRuleEvaluationOutcome::Replayed {
                dispatch_id: RepoWatchDispatchId::from_uuid(dispatch_id),
                sessions: sessions.into_boxed_slice(),
            }))
        }
        _ => Err(RepoWatchDispatchRepositoryError::Corruption(
            "evaluation outcome is unsupported",
        )),
    }
}

async fn release_completed_batches(
    transaction: &mut Transaction<'_, Postgres>,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    key: &StoredSingletonKey,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_release (dispatch_id)
         SELECT batch.dispatch_id
           FROM repo_watch_dispatch_batch AS batch
          WHERE batch.rule_id = $1
            AND batch.rule_version = $2
            AND batch.singleton_scope = $3
            AND batch.singleton_repository IS NOT DISTINCT FROM $4
            AND batch.singleton_pull_request_number IS NOT DISTINCT FROM $5
            AND batch.singleton_stack_root IS NOT DISTINCT FROM $6
            AND NOT EXISTS (
                SELECT 1 FROM repo_watch_dispatch_release AS released
                 WHERE released.dispatch_id = batch.dispatch_id
            )
            AND batch.action_count = (
                SELECT count(*)
                  FROM repo_watch_dispatch_action AS action
                  JOIN repo_watch_dispatch_delivery AS delivery
                    ON delivery.dispatch_id = action.dispatch_id
                   AND delivery.action_ordinal = action.action_ordinal
                  JOIN turn_lifecycle AS turn
                    ON turn.turn_id = delivery.turn_id
                   AND turn.state_kind = 'terminal'
                 WHERE action.dispatch_id = batch.dispatch_id
                   AND NOT EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS live_turn
                         WHERE live_turn.session_id = action.session_id
                           AND live_turn.state_kind <> 'terminal'
                   )
            )
         ON CONFLICT DO NOTHING",
    )
    .bind(rule_id.as_str())
    .bind(i64::try_from(rule_version.get()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
    })?)
    .bind(key.scope)
    .bind(key.repository.as_deref())
    .bind(key.pull_request)
    .bind(key.stack_root.as_deref())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn singleton_is_occupied(
    transaction: &mut Transaction<'_, Postgres>,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    key: &StoredSingletonKey,
) -> Result<bool, RepoWatchDispatchRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_batch AS batch
             WHERE batch.rule_id = $1
               AND batch.rule_version = $2
               AND batch.singleton_scope = $3
               AND batch.singleton_repository IS NOT DISTINCT FROM $4
               AND batch.singleton_pull_request_number IS NOT DISTINCT FROM $5
               AND batch.singleton_stack_root IS NOT DISTINCT FROM $6
               AND NOT EXISTS (
                    SELECT 1 FROM repo_watch_dispatch_release AS released
                     WHERE released.dispatch_id = batch.dispatch_id
               )
        )",
    )
    .bind(rule_id.as_str())
    .bind(i64::try_from(rule_version.get()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
    })?)
    .bind(key.scope)
    .bind(key.repository.as_deref())
    .bind(key.pull_request)
    .bind(key.stack_root.as_deref())
    .fetch_one(&mut **transaction)
    .await?)
}

async fn singleton_is_cooling_down(
    transaction: &mut Transaction<'_, Postgres>,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    key: &StoredSingletonKey,
) -> Result<bool, RepoWatchDispatchRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_release AS released
              JOIN repo_watch_dispatch_batch AS batch
                ON batch.dispatch_id = released.dispatch_id
             WHERE batch.rule_id = $1
               AND batch.rule_version = $2
               AND batch.singleton_scope = $3
               AND batch.singleton_repository IS NOT DISTINCT FROM $4
               AND batch.singleton_pull_request_number IS NOT DISTINCT FROM $5
               AND batch.singleton_stack_root IS NOT DISTINCT FROM $6
               AND extract(epoch FROM (transaction_timestamp() - released.released_at))
                    < batch.cooldown_seconds
        )",
    )
    .bind(rule_id.as_str())
    .bind(i64::try_from(rule_version.get()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
    })?)
    .bind(key.scope)
    .bind(key.repository.as_deref())
    .bind(key.pull_request)
    .bind(key.stack_root.as_deref())
    .fetch_one(&mut **transaction)
    .await?)
}

async fn commit(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    transaction.commit().await.map_err(|error| {
        if commit_failure_is_ambiguous(&error) {
            RepoWatchDispatchRepositoryError::CommitAmbiguous(error)
        } else {
            RepoWatchDispatchRepositoryError::Database(error)
        }
    })
}

fn stored_rule_version(
    version: RepoWatchRuleVersion,
) -> Result<i64, RepoWatchDispatchRepositoryError> {
    i64::try_from(version.get())
        .map_err(|_| RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage"))
}
