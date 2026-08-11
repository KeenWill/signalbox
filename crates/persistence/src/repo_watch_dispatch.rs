//! Durable repository-watch rule consumption, singleton admission, and dispatch audit.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use rust_decimal::Decimal;
use signalbox_application::{
    RepoWatchDispatchTransaction, RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome,
    RepoWatchSingletonKey,
};
use signalbox_domain::{
    FrozenAliasDefinition, ModelAlias, RepoWatchActionV1, RepoWatchDispatchId, RepoWatchEvent,
    RepoWatchEventId, RepoWatchRule, RepoWatchRuleId, RepoWatchRuleVersion, RepositorySlug,
    SessionId,
};
use sqlx::{PgPool, Postgres, Row, Transaction, types::Uuid};

use crate::{
    commit_failure_is_ambiguous, create_session::insert_fresh_prepared, mapping::session_id_to_uuid,
};

const CONFIGURATION_LOCK: &str = "repo-watch\u{1f}configuration";

/// Database or durable-shape failure while evaluating one repository-watch rule.
#[derive(Debug)]
pub enum RepoWatchDispatchRepositoryError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    EventStore(crate::repo_watch::RepoWatchStoreError),
    SessionCreation(crate::create_session::CreateSessionRepositoryError),
    InitialInput(crate::submit_input::SubmitInputRepositoryError),
    GoalCommission(crate::goal::GoalRepositoryError),
    ReusedRuleIdentity {
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    },
    ChangedRuleIdentity {
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    },
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
            Self::InitialInput(error) => error.fmt(formatter),
            Self::GoalCommission(error) => error.fmt(formatter),
            Self::EventStore(error) => error.fmt(formatter),
            Self::ReusedRuleIdentity {
                rule_id,
                rule_version,
            } => write!(
                formatter,
                "repository-watch rule {} version {} was retired and cannot be reused",
                rule_id.as_str(),
                rule_version.get()
            ),
            Self::ChangedRuleIdentity {
                rule_id,
                rule_version,
            } => write!(
                formatter,
                "repository-watch rule {} version {} changed without a new identity",
                rule_id.as_str(),
                rule_version.get()
            ),
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
            Self::InitialInput(error) => Some(error),
            Self::GoalCommission(error) => Some(error),
            Self::ReusedRuleIdentity { .. }
            | Self::ChangedRuleIdentity { .. }
            | Self::Corruption(_) => None,
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

    /// Deactivates rules belonging to repositories absent from configuration.
    pub async fn deactivate_unconfigured_repositories(
        &self,
        configured: &[RepositorySlug],
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        let configured = configured
            .iter()
            .map(|repository| repository.as_str())
            .collect::<BTreeSet<_>>();
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, CONFIGURATION_LOCK).await?;
        let active_repositories: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT activation.repository
               FROM repo_watch_rule_activation AS activation
              WHERE NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_rule_deactivation AS deactivation
                     WHERE deactivation.repository = activation.repository
                       AND deactivation.rule_id = activation.rule_id
                       AND deactivation.rule_version = activation.rule_version
              )
              ORDER BY activation.repository",
        )
        .fetch_all(&mut *transaction)
        .await?;
        for repository in active_repositories {
            if configured.contains(repository.as_str()) {
                continue;
            }
            lock_text(&mut transaction, &repository).await?;
            sqlx::query(
                "INSERT INTO repo_watch_rule_deactivation
                    (repository, rule_id, rule_version)
                 SELECT activation.repository, activation.rule_id, activation.rule_version
                   FROM repo_watch_rule_activation AS activation
                  WHERE activation.repository = $1
                    AND NOT EXISTS (
                        SELECT 1
                          FROM repo_watch_rule_deactivation AS deactivation
                         WHERE deactivation.repository = activation.repository
                           AND deactivation.rule_id = activation.rule_id
                           AND deactivation.rule_version = activation.rule_version
                    )
                 ON CONFLICT DO NOTHING",
            )
            .bind(repository)
            .execute(&mut *transaction)
            .await?;
        }
        commit(transaction).await
    }

    /// Reconciles the configured rule set before its repository task polls.
    pub async fn reconcile_rules(
        &self,
        repository: &RepositorySlug,
        configured: &[RepoWatchRule],
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, CONFIGURATION_LOCK).await?;
        lock_text(&mut transaction, repository.as_str()).await?;
        let configured = configured
            .iter()
            .map(|rule| {
                Ok((
                    (
                        rule.id().as_str().to_owned(),
                        stored_rule_version(rule.version())?,
                    ),
                    *rule.content_digest().as_bytes(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, RepoWatchDispatchRepositoryError>>()?;
        let configured_identities = configured.keys().cloned().collect::<BTreeSet<_>>();
        let existing = sqlx::query(
            "SELECT activation.rule_id, activation.rule_version, activation.rule_digest,
                    deactivation.rule_id IS NOT NULL AS deactivated
               FROM repo_watch_rule_activation AS activation
               LEFT JOIN repo_watch_rule_deactivation AS deactivation
                 USING (repository, rule_id, rule_version)
              WHERE activation.repository = $1",
        )
        .bind(repository.as_str())
        .fetch_all(&mut *transaction)
        .await?;
        let mut historical = BTreeSet::new();
        let mut active = BTreeSet::new();
        for row in existing {
            let identity = (row.try_get("rule_id")?, row.try_get("rule_version")?);
            historical.insert(identity.clone());
            if !row.try_get::<bool, _>("deactivated")? {
                let stored_digest: Vec<u8> = row.try_get("rule_digest")?;
                if configured
                    .get(&identity)
                    .is_some_and(|digest| stored_digest.as_slice() != digest)
                {
                    transaction.rollback().await?;
                    return Err(RepoWatchDispatchRepositoryError::ChangedRuleIdentity {
                        rule_id: RepoWatchRuleId::try_new(identity.0).map_err(|_| {
                            RepoWatchDispatchRepositoryError::Corruption("stored rule identifier")
                        })?,
                        rule_version: RepoWatchRuleVersion::V1,
                    });
                }
                active.insert(identity);
            }
        }
        for (rule_id, rule_version) in active.difference(&configured_identities) {
            sqlx::query(
                "INSERT INTO repo_watch_rule_deactivation
                    (repository, rule_id, rule_version)
                 VALUES ($1, $2, $3)",
            )
            .bind(repository.as_str())
            .bind(rule_id)
            .bind(rule_version)
            .execute(&mut *transaction)
            .await?;
        }
        for (rule_id, rule_version) in configured_identities.difference(&active) {
            if historical.contains(&(rule_id.clone(), *rule_version)) {
                transaction.rollback().await?;
                return Err(RepoWatchDispatchRepositoryError::ReusedRuleIdentity {
                    rule_id: RepoWatchRuleId::try_new(rule_id.clone()).map_err(|_| {
                        RepoWatchDispatchRepositoryError::Corruption("stored rule identifier")
                    })?,
                    rule_version: RepoWatchRuleVersion::V1,
                });
            }
            sqlx::query(
                "INSERT INTO repo_watch_rule_activation
                (repository, rule_id, rule_version, rule_digest,
                 after_cursor_generation, after_event_ordinal)
             SELECT $1, $2, $3, $4, tail.cursor_generation, tail.event_ordinal
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
            .bind(rule_id)
            .bind(rule_version)
            .bind(configured.get(&(rule_id.clone(), *rule_version)).ok_or(
                RepoWatchDispatchRepositoryError::Corruption("configured rule digest missing"),
            )?)
            .execute(&mut *transaction)
            .await?;
        }
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
                      FROM repo_watch_rule_deactivation AS deactivation
                     WHERE deactivation.repository = activation.repository
                       AND deactivation.rule_id = activation.rule_id
                       AND deactivation.rule_version = activation.rule_version
                )
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
}

impl PostgresRepoWatchDispatchStore {
    /// Applies one evaluation while resolving any session-default model alias.
    pub async fn handle_repo_watch_evaluation_with_alias_resolver<SelectDefinition>(
        &self,
        evaluation: RepoWatchRuleEvaluation,
        select_definition: SelectDefinition,
    ) -> Result<RepoWatchRuleEvaluationOutcome, RepoWatchDispatchRepositoryError>
    where
        SelectDefinition: Fn(ModelAlias) -> Option<FrozenAliasDefinition> + Copy + Send,
    {
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
                lock_text(&mut transaction, event.repository().as_str()).await?;
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
                if !rule_is_active(&mut transaction, &event, &rule_id, rule_version).await? {
                    transaction.rollback().await?;
                    return Ok(RepoWatchRuleEvaluationOutcome::Inactive);
                }
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
                    let (
                        configured_action,
                        prepared_session,
                        initial_input,
                        accepted_input,
                        turn,
                        cancellation_entry,
                        cancellation_frontier,
                        goal,
                    ) = action.into_parts();
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
                    if initial_input.session() != session {
                        return Err(RepoWatchDispatchRepositoryError::Corruption(
                            "dispatch initial input targets another session",
                        ));
                    }
                    if goal.command().session() != session {
                        return Err(RepoWatchDispatchRepositoryError::Corruption(
                            "dispatch goal targets another session",
                        ));
                    }
                    let command_id = command.command_id();
                    let submit_command_id = initial_input.command_id();
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
                    // The goal is commissioned before the work input is accepted, so
                    // the authority a dispatched session runs under is durably
                    // earlier than the turn doing the work. A consumer deciding
                    // what that turn may do can then check the ordering rather
                    // than trust that one code path happened to build it that way.
                    let (goal_command, goal_accepted_input, goal_turn) =
                        (goal.command().clone(), goal.accepted_input(), goal.turn());
                    crate::goal::insert_fresh_commissioned_goal(
                        &mut transaction,
                        goal_command,
                        crate::goal_turn::GoalTurnCandidates::new(goal_accepted_input, goal_turn),
                        select_definition,
                    )
                    .await
                    .map_err(RepoWatchDispatchRepositoryError::GoalCommission)?;
                    crate::submit_input::insert_fresh_initial_input(
                        &mut transaction,
                        initial_input,
                        accepted_input,
                        turn,
                        cancellation_entry,
                        cancellation_frontier,
                        select_definition,
                    )
                    .await
                    .map_err(RepoWatchDispatchRepositoryError::InitialInput)?;
                    sqlx::query(
                        "INSERT INTO repo_watch_dispatch_delivery_intent
                            (dispatch_id, action_ordinal, submit_command_id,
                             accepted_input_id, turn_id, cancellation_entry_id,
                             cancellation_frontier_id)
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(dispatch_id.as_uuid())
                    .bind(ordinal)
                    .bind(submit_command_id.as_uuid())
                    .bind(accepted_input.as_uuid())
                    .bind(turn.as_uuid())
                    .bind(cancellation_entry.as_uuid())
                    .bind(cancellation_frontier.as_uuid())
                    .execute(&mut *transaction)
                    .await?;
                    sqlx::query(
                        "INSERT INTO repo_watch_dispatch_delivery
                            (dispatch_id, action_ordinal, submit_command_id,
                             accepted_input_id, turn_id)
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(dispatch_id.as_uuid())
                    .bind(ordinal)
                    .bind(submit_command_id.as_uuid())
                    .bind(accepted_input.as_uuid())
                    .bind(turn.as_uuid())
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

impl RepoWatchDispatchTransaction for PostgresRepoWatchDispatchStore {
    type Error = RepoWatchDispatchRepositoryError;

    async fn handle_repo_watch_evaluation(
        &mut self,
        evaluation: RepoWatchRuleEvaluation,
    ) -> Result<RepoWatchRuleEvaluationOutcome, Self::Error> {
        self.handle_repo_watch_evaluation_with_alias_resolver(evaluation, |_| None)
            .await
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
        lock_text(&mut transaction, event.repository().as_str()).await?;
        if let Some(recorded) =
            load_recorded_evaluation(&mut transaction, event.id(), rule_id, rule_version).await?
        {
            transaction.rollback().await?;
            return Ok(recorded);
        }
        if !rule_is_active(&mut transaction, event, rule_id, rule_version).await? {
            transaction.rollback().await?;
            return Ok(RepoWatchRuleEvaluationOutcome::Inactive);
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
    stack_root_pull_request: Option<Decimal>,
}

impl StoredSingletonKey {
    fn from_domain(key: &RepoWatchSingletonKey) -> Self {
        match key {
            RepoWatchSingletonKey::PullRequest { repository, number } => Self {
                scope: "pull_request",
                repository: Some(repository.as_str().to_owned()),
                pull_request: Some(Decimal::from(number.get())),
                stack_root_pull_request: None,
            },
            RepoWatchSingletonKey::Stack {
                repository,
                root_pull_request,
            } => Self {
                scope: "stack",
                repository: Some(repository.as_str().to_owned()),
                pull_request: None,
                stack_root_pull_request: Some(Decimal::from(root_pull_request.get())),
            },
            RepoWatchSingletonKey::Rule => Self {
                scope: "rule",
                repository: None,
                pull_request: None,
                stack_root_pull_request: None,
            },
            RepoWatchSingletonKey::Repository { repository } => Self {
                scope: "repo",
                repository: Some(repository.as_str().to_owned()),
                pull_request: None,
                stack_root_pull_request: None,
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
            self.stack_root_pull_request
                .map_or(String::new(), |value| value.to_string())
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
             singleton_stack_root_pull_request_number, cooldown_seconds, action_count)
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
    .bind(batch.singleton.stack_root_pull_request)
    .bind(batch.cooldown_seconds)
    .bind(batch.action_count)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn rule_is_active(
    transaction: &mut Transaction<'_, Postgres>,
    event: &RepoWatchEvent,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
) -> Result<bool, RepoWatchDispatchRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM repo_watch_rule_activation AS activation
             WHERE activation.repository = $1
               AND activation.rule_id = $2
               AND activation.rule_version = $3
               AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_rule_deactivation AS deactivation
                     WHERE deactivation.repository = activation.repository
                       AND deactivation.rule_id = activation.rule_id
                       AND deactivation.rule_version = activation.rule_version
               )
        )",
    )
    .bind(event.repository().as_str())
    .bind(rule_id.as_str())
    .bind(stored_rule_version(rule_version)?)
    .fetch_one(&mut **transaction)
    .await?)
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
               AND batch.singleton_stack_root_pull_request_number IS NOT DISTINCT FROM $6
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
    .bind(key.stack_root_pull_request)
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
               AND batch.singleton_stack_root_pull_request_number IS NOT DISTINCT FROM $6
               AND extract(epoch FROM (clock_timestamp() - released.released_at))
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
    .bind(key.stack_root_pull_request)
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
