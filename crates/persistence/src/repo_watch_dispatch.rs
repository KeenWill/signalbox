//! Durable repository-watch rule consumption, singleton admission, and dispatch audit.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    num::NonZeroU64,
    time::Duration,
};

use rust_decimal::Decimal;
use signalbox_application::{
    RepoWatchDispatchTransaction, RepoWatchRuleEvaluation, RepoWatchRuleEvaluationOutcome,
    RepoWatchSingletonKey,
};
use signalbox_domain::{
    DescendantTerminationScope, DurableCommandId, FrozenAliasDefinition, GoalUserAction,
    GoalUserCommand, ModelAlias, RepoWatchActionV1, RepoWatchDispatchId, RepoWatchEvent,
    RepoWatchEventId, RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget,
    RepoWatchRule, RepoWatchRuleId, RepoWatchRuleIdentityField, RepoWatchRuleVersion,
    RepositorySlug, SessionId,
};
use sqlx::{Acquire, PgPool, Postgres, Row, Transaction, types::Uuid};

use crate::{
    commit_failure_is_ambiguous,
    create_session::insert_fresh_prepared,
    mapping::{
        RepoWatchEvaluationOutcomeStorageKind, RepoWatchLifecycleCutoffDispositionStorageKind,
        RepoWatchSingletonScopeStorageKind, repo_watch_evaluation_outcome_from_str,
        repo_watch_evaluation_outcome_to_str, repo_watch_event_kind_from_str,
        repo_watch_lifecycle_cutoff_disposition_from_str,
        repo_watch_lifecycle_cutoff_disposition_to_str, repo_watch_singleton_scope_to_str,
        session_id_to_uuid,
    },
};

const CONFIGURATION_LOCK: &str = "repo-watch\u{1f}configuration";
// numeric-bound: ceiling - bounds accepted repository-watch work without a model call
const DISPATCH_START_LEASE_LIMIT: Duration = Duration::from_secs(5 * 60);
// numeric-bound: tunable - preserves a positive lowered lease for tests and composition
const MINIMUM_DISPATCH_START_LEASE: Duration = Duration::from_millis(1);
// numeric-bound: tunable - bounds one repository reconciliation nudge batch
const UNSTARTED_DISPATCH_NUDGE_BATCH_SIZE: i64 = 16;

struct ConfiguredRuleIdentity {
    content_digest: [u8; 32],
    field_digests: Vec<(RepoWatchRuleIdentityField, [u8; 32])>,
}

impl ConfiguredRuleIdentity {
    fn from_rule(rule: &RepoWatchRule) -> Self {
        Self {
            content_digest: *rule.content_digest().as_bytes(),
            field_digests: rule
                .identity_field_digests()
                .into_iter()
                .map(|(field, digest)| (field, *digest.as_bytes()))
                .collect(),
        }
    }

    fn encoded_field_digests(&self) -> Vec<u8> {
        self.field_digests
            .iter()
            .flat_map(|(_, digest)| digest.iter().copied())
            .collect()
    }

    fn changed_field(
        &self,
        stored: &[u8],
    ) -> Result<Option<RepoWatchRuleIdentityField>, RepoWatchDispatchRepositoryError> {
        if stored.len() != self.field_digests.len() * 32 {
            return Err(RepoWatchDispatchRepositoryError::Corruption(
                "stored rule field fingerprints have an invalid length",
            ));
        }
        Ok(self
            .field_digests
            .iter()
            .zip(stored.chunks_exact(32))
            .find_map(|((field, configured), stored)| (configured != stored).then_some(*field)))
    }
}

/// Database or durable-shape failure while evaluating one repository-watch rule.
#[derive(Debug)]
pub enum RepoWatchDispatchRepositoryError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    EventStore(crate::repo_watch::RepoWatchStoreError),
    SessionCreation(crate::create_session::CreateSessionRepositoryError),
    InitialInput(crate::submit_input::SubmitInputRepositoryError),
    GoalCommission(crate::goal::GoalRepositoryError),
    GoalCutoff(crate::goal::GoalRepositoryError),
    ReusedRuleIdentity {
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    },
    ChangedRuleIdentity {
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
        field: RepoWatchRuleIdentityField,
    },
    RegressedRuleVersion {
        rule_id: RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
        latest_version: RepoWatchRuleVersion,
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
            Self::GoalCutoff(error) => error.fmt(formatter),
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
                field,
            } => write!(
                formatter,
                "repository-watch rule {} version {} field `{}` changed without a version bump",
                rule_id.as_str(),
                rule_version.get(),
                field.configuration_path()
            ),
            Self::RegressedRuleVersion {
                rule_id,
                rule_version,
                latest_version,
            } => write!(
                formatter,
                "repository-watch rule {} version {} is below its highest recorded version {}",
                rule_id.as_str(),
                rule_version.get(),
                latest_version.get()
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
            Self::GoalCutoff(error) => Some(error),
            Self::ReusedRuleIdentity { .. }
            | Self::ChangedRuleIdentity { .. }
            | Self::RegressedRuleVersion { .. }
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
    dispatch_start_lease: Duration,
}

impl PostgresRepoWatchDispatchStore {
    pub fn new(pool: PgPool, credential_pin: crate::SessionCredentialPin) -> Self {
        Self {
            pool,
            credential_pin,
            dispatch_start_lease: DISPATCH_START_LEASE_LIMIT,
        }
    }

    /// Lowers the dispatch-start lease for a composed runtime or test.
    ///
    /// Requests above the production ceiling are clamped rather than creating
    /// a path that can raise the safety bound.
    pub fn with_dispatch_start_lease(mut self, requested: Duration) -> Self {
        self.dispatch_start_lease = requested
            .max(MINIMUM_DISPATCH_START_LEASE)
            .min(DISPATCH_START_LEASE_LIMIT);
        self
    }

    pub(crate) const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Loads bounded durable dispatch starts that still need priority admission.
    pub async fn load_unstarted_dispatch_sessions(
        &self,
        repository: &RepositorySlug,
    ) -> Result<Vec<SessionId>, RepoWatchDispatchRepositoryError> {
        let sessions = sqlx::query_scalar::<_, Uuid>(
            "SELECT lease.session_id
               FROM repo_watch_dispatch_start_lease AS lease
               JOIN repo_watch_dispatch_batch AS batch
                 ON batch.dispatch_id = lease.dispatch_id
               JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
              WHERE origin.repository = $1
                AND lease.expires_at > clock_timestamp()
                AND NOT EXISTS (
                    SELECT 1
                      FROM model_call AS call
                     WHERE call.session_id = lease.session_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_start_lease_expiration AS expired
                     WHERE expired.dispatch_id = lease.dispatch_id
                       AND expired.action_ordinal = lease.action_ordinal
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_release AS released
                     WHERE released.dispatch_id = lease.dispatch_id
                )
                AND EXISTS (
                    SELECT 1
                      FROM goal_event AS current_goal
                     WHERE current_goal.session_id = lease.session_id
                       AND current_goal.event_ordinal = (
                            SELECT max(candidate.event_ordinal)
                              FROM goal_event AS candidate
                             WHERE candidate.session_id = lease.session_id
                       )
                       AND current_goal.event_kind IN (
                            'commissioned', 'resumed', 'superseded'
                       )
                )
              ORDER BY lease.leased_at, lease.session_id
              LIMIT $2",
        )
        .bind(repository.as_str())
        .bind(UNSTARTED_DISPATCH_NUDGE_BATCH_SIZE)
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions
            .into_iter()
            .map(SessionId::from_uuid)
            .collect::<Vec<_>>())
    }

    /// Retires one expired evidence-free dispatch through ordinary goal authority.
    pub async fn process_next_expired_start_lease<NextCommandId>(
        &self,
        repository: &RepositorySlug,
        mut next_command_id: NextCommandId,
    ) -> Result<bool, RepoWatchDispatchRepositoryError>
    where
        NextCommandId: FnMut() -> DurableCommandId,
    {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, repository.as_str()).await?;
        // The repository lock serializes lease selection. The session lock
        // below then excludes model-call preparation before its evidence
        // recheck and the composed stop.
        let candidate = sqlx::query(
            "SELECT lease.dispatch_id, lease.action_ordinal, lease.session_id
               FROM repo_watch_dispatch_start_lease AS lease
               JOIN repo_watch_dispatch_batch AS batch
                 ON batch.dispatch_id = lease.dispatch_id
               JOIN repo_watch_event AS origin ON origin.event_id = batch.event_id
              WHERE origin.repository = $1
                AND lease.expires_at <= clock_timestamp()
                AND NOT EXISTS (
                    SELECT 1
                      FROM model_call AS call
                     WHERE call.session_id = lease.session_id
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_start_lease_expiration AS expired
                     WHERE expired.dispatch_id = lease.dispatch_id
                       AND expired.action_ordinal = lease.action_ordinal
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_release AS released
                     WHERE released.dispatch_id = lease.dispatch_id
                )
                AND EXISTS (
                    SELECT 1
                      FROM goal_event AS current_goal
                     WHERE current_goal.session_id = lease.session_id
                       AND current_goal.event_ordinal = (
                            SELECT max(candidate.event_ordinal)
                              FROM goal_event AS candidate
                             WHERE candidate.session_id = lease.session_id
                       )
                       AND current_goal.event_kind IN (
                            'commissioned', 'resumed', 'superseded'
                       )
                )
              ORDER BY lease.expires_at, lease.session_id
              LIMIT 1",
        )
        .bind(repository.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(candidate) = candidate else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let dispatch_id: Uuid = candidate.try_get("dispatch_id")?;
        let action_ordinal: i32 = candidate.try_get("action_ordinal")?;
        let session_id: Uuid = candidate.try_get("session_id")?;
        let session = SessionId::from_uuid(session_id);
        if !crate::goal::lock_session(&mut transaction, session).await? {
            transaction.rollback().await?;
            return Err(RepoWatchDispatchRepositoryError::Corruption(
                "expired dispatch start lease references a missing session",
            ));
        }
        let model_call_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM model_call WHERE session_id = $1
            )",
        )
        .bind(session_id)
        .fetch_one(&mut *transaction)
        .await?;
        if model_call_exists {
            transaction.rollback().await?;
            return Ok(true);
        }
        let current_goal_is_pursuing: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM goal_event AS current_goal
                 WHERE current_goal.session_id = $1
                   AND current_goal.event_ordinal = (
                        SELECT max(candidate.event_ordinal)
                          FROM goal_event AS candidate
                         WHERE candidate.session_id = $1
                   )
                   AND current_goal.event_kind IN (
                        'commissioned', 'resumed', 'superseded'
                   )
            )",
        )
        .bind(session_id)
        .fetch_one(&mut *transaction)
        .await?;
        if !current_goal_is_pursuing {
            transaction.rollback().await?;
            return Ok(true);
        }
        let command = GoalUserCommand::new(
            next_command_id(),
            session,
            GoalUserAction::Stop {
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        );
        let stopped =
            crate::goal::insert_repo_watch_composed_stop(&mut transaction, command.clone())
                .await
                .map_err(RepoWatchDispatchRepositoryError::GoalCutoff)?;
        if !stopped {
            transaction.rollback().await?;
            return Err(RepoWatchDispatchRepositoryError::Corruption(
                "expired dispatch start lease remained attached to a non-stoppable goal",
            ));
        }
        sqlx::query(
            "INSERT INTO repo_watch_dispatch_start_lease_expiration
                (dispatch_id, action_ordinal, session_id, goal_command_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(dispatch_id)
        .bind(action_ordinal)
        .bind(session_id)
        .bind(command.command_id().as_uuid())
        .execute(&mut *transaction)
        .await?;
        commit(transaction).await?;
        Ok(true)
    }

    /// Processes the oldest unhandled pull-request closure and withdraws every
    /// still-active generation-one goal commissioned for that pull request.
    pub async fn process_next_lifecycle_cutoff<NextCommandId>(
        &self,
        repository: &RepositorySlug,
        mut next_command_id: NextCommandId,
    ) -> Result<bool, RepoWatchDispatchRepositoryError>
    where
        NextCommandId: FnMut() -> DurableCommandId,
    {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, repository.as_str()).await?;
        let candidate = sqlx::query(
            "SELECT event.event_id, event.pull_request_number,
                    event.cursor_generation, event.event_ordinal
               FROM repo_watch_event AS event
              WHERE event.repository = $1
                AND event.event_kind IN ('pull_request_closed', 'pull_request_merged')
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_lifecycle_cutoff AS cutoff
                     WHERE cutoff.event_id = event.event_id
                )
              ORDER BY event.cursor_generation, event.event_ordinal
              LIMIT 1",
        )
        .bind(repository.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(candidate) = candidate else {
            transaction.rollback().await?;
            return Ok(false);
        };
        let event_id: Uuid = candidate.try_get("event_id")?;
        let pull_request_number: Decimal = candidate.try_get("pull_request_number")?;
        let cursor_generation: i64 = candidate.try_get("cursor_generation")?;
        let event_ordinal: i32 = candidate.try_get("event_ordinal")?;
        let next_lifecycle: Option<String> = sqlx::query_scalar(
            "SELECT event_kind
               FROM repo_watch_event
              WHERE repository = $1
                AND pull_request_number = $2
                AND event_kind IN (
                    'pull_request_opened', 'pull_request_closed', 'pull_request_merged'
                )
                AND (cursor_generation, event_ordinal) > ($3, $4)
              ORDER BY cursor_generation, event_ordinal
              LIMIT 1",
        )
        .bind(repository.as_str())
        .bind(pull_request_number)
        .bind(cursor_generation)
        .bind(event_ordinal)
        .fetch_optional(&mut *transaction)
        .await?;
        let next_lifecycle = next_lifecycle
            .map(|kind| {
                repo_watch_event_kind_from_str(&kind).ok_or(
                    RepoWatchDispatchRepositoryError::Corruption(
                        "lifecycle cutoff has an unknown following event kind",
                    ),
                )
            })
            .transpose()?;
        let disposition = if next_lifecycle == Some(RepoWatchEventKindNameV1::PullRequestOpened) {
            RepoWatchLifecycleCutoffDispositionStorageKind::Reopened
        } else {
            RepoWatchLifecycleCutoffDispositionStorageKind::Terminal
        };
        sqlx::query(
            "INSERT INTO repo_watch_lifecycle_cutoff (event_id, disposition_kind)
             VALUES ($1, $2)",
        )
        .bind(event_id)
        .bind(repo_watch_lifecycle_cutoff_disposition_to_str(disposition))
        .execute(&mut *transaction)
        .await?;
        let mut cutoff_corruption = None;
        if disposition == RepoWatchLifecycleCutoffDispositionStorageKind::Terminal {
            let sessions = sqlx::query_scalar::<_, Uuid>(
                "SELECT DISTINCT action.session_id
                   FROM repo_watch_dispatch_action AS action
                   JOIN repo_watch_event AS origin ON origin.event_id = action.event_id
                  WHERE origin.repository = $1
                    AND origin.pull_request_number = $2
                    AND origin.event_id <> $3
                    AND EXISTS (
                        SELECT 1
                          FROM goal_event AS commissioned_goal
                         WHERE commissioned_goal.session_id = action.session_id
                    )
                  ORDER BY action.session_id",
            )
            .bind(repository.as_str())
            .bind(pull_request_number)
            .bind(event_id)
            .fetch_all(&mut *transaction)
            .await?;
            for session_id in sessions {
                let session = SessionId::from_uuid(session_id);
                let command = GoalUserCommand::new(
                    next_command_id(),
                    session,
                    GoalUserAction::Stop {
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                );
                let mut savepoint = transaction.begin().await?;
                match crate::goal::insert_repo_watch_composed_stop(&mut savepoint, command.clone())
                    .await
                {
                    Ok(stopped) => {
                        if stopped {
                            sqlx::query(
                                "INSERT INTO repo_watch_lifecycle_cutoff_goal
                                    (event_id, session_id, goal_command_id)
                                 VALUES ($1, $2, $3)",
                            )
                            .bind(event_id)
                            .bind(session_id)
                            .bind(command.command_id().as_uuid())
                            .execute(&mut *savepoint)
                            .await?;
                        }
                        savepoint.commit().await?;
                    }
                    Err(crate::goal::GoalRepositoryError::Corruption(error)) => {
                        savepoint.rollback().await?;
                        cutoff_corruption
                            .get_or_insert(crate::goal::GoalRepositoryError::Corruption(error));
                    }
                    Err(error) => {
                        savepoint.rollback().await?;
                        return Err(RepoWatchDispatchRepositoryError::GoalCutoff(error));
                    }
                }
            }
            // After the stops, not before. A goal termination holds its session
            // row and then waits for its obligation row inside the requeue, so a
            // cutoff that took obligation rows first and session rows second
            // would close a lock cycle against any sibling still terminating.
            crate::repo_watch_dispatch_obligation::settle_terminal_target_obligations(
                &mut transaction,
                repository,
                pull_request_number,
                RepoWatchEventId::from_uuid(event_id),
            )
            .await?;
        }
        commit(transaction).await?;
        if let Some(error) = cutoff_corruption {
            return Err(RepoWatchDispatchRepositoryError::GoalCutoff(error));
        }
        Ok(true)
    }

    /// Fails lifecycle-cutoff processing from the first webhook disposition on,
    /// for a composed attempt test.
    ///
    /// An attempt runs cutoffs both before its drain and after it, and what is
    /// being exercised is the pass after: a fault installed up front would fail
    /// the earlier pass instead and the attempt would never reach the later
    /// one. So the fault arms itself from the drain's own terminal write. It
    /// withdraws the table rather than rejecting a record, because a cutoff
    /// that finds no candidate must fail too — the attempt under test is one
    /// whose trailing cutoff fails transiently, not one that happens to have
    /// closure work waiting. Left in place for the container's lifetime.
    #[cfg(feature = "test-support")]
    pub async fn inject_post_drain_lifecycle_cutoff_fault(
        &self,
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        sqlx::query(
            "CREATE FUNCTION withdraw_repo_watch_lifecycle_cutoff()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $$
             BEGIN
                 IF EXISTS (
                     SELECT 1
                       FROM pg_class
                      WHERE relname = 'repo_watch_lifecycle_cutoff'
                 ) THEN
                     ALTER TABLE repo_watch_lifecycle_cutoff
                        RENAME TO repo_watch_lifecycle_cutoff_withdrawn;
                 END IF;
                 RETURN NEW;
             END
             $$",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TRIGGER withdraw_repo_watch_lifecycle_cutoff
             AFTER INSERT ON repo_watch_webhook_disposition
             FOR EACH ROW
             EXECUTE FUNCTION withdraw_repo_watch_lifecycle_cutoff()",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drains durable lifecycle cutoffs before repository-specific tasks start.
    pub async fn process_pending_lifecycle_cutoffs<NextCommandId>(
        &self,
        mut next_command_id: NextCommandId,
    ) -> Result<(), RepoWatchDispatchRepositoryError>
    where
        NextCommandId: FnMut() -> DurableCommandId,
    {
        loop {
            let repository: Option<String> = sqlx::query_scalar(
                "SELECT event.repository
                   FROM repo_watch_event AS event
                  WHERE event.event_kind IN ('pull_request_closed', 'pull_request_merged')
                    AND NOT EXISTS (
                        SELECT 1
                          FROM repo_watch_lifecycle_cutoff AS cutoff
                         WHERE cutoff.event_id = event.event_id
                    )
                  ORDER BY event.repository, event.cursor_generation, event.event_ordinal
                  LIMIT 1",
            )
            .fetch_optional(&self.pool)
            .await?;
            let Some(repository) = repository else {
                return Ok(());
            };
            let repository = RepositorySlug::try_new(repository).map_err(|_| {
                RepoWatchDispatchRepositoryError::Corruption(
                    "pending lifecycle cutoff has an invalid repository",
                )
            })?;
            match self
                .process_next_lifecycle_cutoff(&repository, &mut next_command_id)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return Err(RepoWatchDispatchRepositoryError::Corruption(
                        "selected pending lifecycle cutoff disappeared",
                    ));
                }
                Err(RepoWatchDispatchRepositoryError::GoalCutoff(
                    crate::goal::GoalRepositoryError::Corruption(_),
                )) => {}
                Err(error) => return Err(error),
            }
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
        retire_unconfigured_repositories(&mut transaction, &configured).await?;
        commit(transaction).await
    }

    /// Reports whether the complete configured repository set is admissible.
    ///
    /// The transaction is always discarded, so startup can refuse an
    /// inadmissible configuration in its Configuration phase — before either
    /// local socket binds — while leaving the durable revision history
    /// untouched. A daemon whose later startup construction fails has then
    /// consumed no revision, and restoring the previous configuration is still
    /// admitted rather than refused as identity reuse.
    pub async fn validate_configured_rules(
        &self,
        repositories: &[RepositorySlug],
        configured: &[RepoWatchRule],
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, CONFIGURATION_LOCK).await?;
        let admission = admit_configured_rules(&mut transaction, repositories, configured).await;
        transaction.rollback().await?;
        admission
    }

    /// Commits the complete configured repository set, resolving ambiguity.
    ///
    /// Every watched repository is admitted together, so a refusal anywhere in
    /// the set leaves no deactivation and no activation behind. Startup calls
    /// this only after every other fallible construction has succeeded, so the
    /// revisions it consumes belong to a daemon that reaches its runtime.
    ///
    /// A lost commit response leaves the durable outcome unknown, which the
    /// caller must not guess. The resolution is a read-only reread of the
    /// active set, which commits nothing and so cannot itself become
    /// ambiguous: it answers the only question the outcome turned on. Either
    /// the admission is already applied, so the commit won and startup
    /// proceeds, or it is not, so the commit never landed, no revision was
    /// consumed, and startup fails with the previous configuration still
    /// admissible. One reread therefore settles the ambiguity instead of
    /// leaving another unknown commit outcome behind; only an unreachable
    /// database defeats it, and the next start rereads again.
    pub async fn reconcile_configured_rules(
        &self,
        repositories: &[RepositorySlug],
        configured: &[RepoWatchRule],
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        match self.commit_configured_rules(repositories, configured).await {
            Err(RepoWatchDispatchRepositoryError::CommitAmbiguous(error)) => {
                if self.admission_is_applied(repositories, configured).await? {
                    return Ok(());
                }
                Err(RepoWatchDispatchRepositoryError::CommitAmbiguous(error))
            }
            outcome => outcome,
        }
    }

    /// Whether the durable active set already equals the configured admission.
    ///
    /// Read-only by construction, so resolving an ambiguous commit never adds
    /// another commit boundary to be ambiguous about.
    async fn admission_is_applied(
        &self,
        repositories: &[RepositorySlug],
        configured: &[RepoWatchRule],
    ) -> Result<bool, RepoWatchDispatchRepositoryError> {
        let mut expected = BTreeSet::new();
        for repository in repositories {
            for rule in configured {
                expected.insert((
                    repository.as_str().to_owned(),
                    rule.id().as_str().to_owned(),
                    stored_rule_version(rule.version())?,
                ));
            }
        }
        let rows = sqlx::query(
            "SELECT activation.repository, activation.rule_id, activation.rule_version
               FROM repo_watch_rule_activation AS activation
              WHERE NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_rule_deactivation AS deactivation
                     WHERE deactivation.repository = activation.repository
                       AND deactivation.rule_id = activation.rule_id
                       AND deactivation.rule_version = activation.rule_version
              )",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut active = BTreeSet::new();
        for row in rows {
            active.insert((
                row.try_get::<String, _>("repository")?,
                row.try_get::<String, _>("rule_id")?,
                row.try_get::<i64, _>("rule_version")?,
            ));
        }
        Ok(active == expected)
    }

    /// Admits the configured repository set in one committed transaction.
    async fn commit_configured_rules(
        &self,
        repositories: &[RepositorySlug],
        configured: &[RepoWatchRule],
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, CONFIGURATION_LOCK).await?;
        if let Err(error) = admit_configured_rules(&mut transaction, repositories, configured).await
        {
            transaction.rollback().await?;
            return Err(error);
        }
        commit(transaction).await
    }

    /// Reconciles one repository's configured rule set before its task polls.
    pub async fn reconcile_rules(
        &self,
        repository: &RepositorySlug,
        configured: &[RepoWatchRule],
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        lock_text(&mut transaction, CONFIGURATION_LOCK).await?;
        if let Err(error) =
            reconcile_repository_rules(&mut transaction, repository.as_str(), configured).await
        {
            transaction.rollback().await?;
            return Err(error);
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
        self.handle_repo_watch_evaluation_with_admission(
            evaluation,
            select_definition,
            EvaluationAdmission::Fresh,
        )
        .await
    }

    /// Applies one collapsed obligation after its singleton and cooldown are free.
    pub async fn handle_repo_watch_obligation_with_alias_resolver<SelectDefinition>(
        &self,
        obligation: crate::repo_watch_dispatch_obligation::RepoWatchDispatchObligation,
        evaluation: RepoWatchRuleEvaluation,
        select_definition: SelectDefinition,
    ) -> Result<RepoWatchRuleEvaluationOutcome, RepoWatchDispatchRepositoryError>
    where
        SelectDefinition: Fn(ModelAlias) -> Option<FrozenAliasDefinition> + Copy + Send,
    {
        self.handle_repo_watch_evaluation_with_admission(
            evaluation,
            select_definition,
            EvaluationAdmission::Obligation(Box::new(obligation)),
        )
        .await
    }

    async fn handle_repo_watch_evaluation_with_admission<SelectDefinition>(
        &self,
        evaluation: RepoWatchRuleEvaluation,
        select_definition: SelectDefinition,
        admission: EvaluationAdmission,
    ) -> Result<RepoWatchRuleEvaluationOutcome, RepoWatchDispatchRepositoryError>
    where
        SelectDefinition: Fn(ModelAlias) -> Option<FrozenAliasDefinition> + Copy + Send,
    {
        match evaluation {
            RepoWatchRuleEvaluation::NotMatched {
                event,
                rule_id,
                rule_version,
            } => match admission {
                EvaluationAdmission::Fresh => {
                    self.record_simple_outcome(
                        &event,
                        &rule_id,
                        rule_version,
                        RepoWatchEvaluationOutcomeStorageKind::NotMatched,
                    )
                    .await
                }
                EvaluationAdmission::Obligation(_) => {
                    Err(RepoWatchDispatchRepositoryError::Corruption(
                        "owed repository-watch event no longer matches its activated rule",
                    ))
                }
            },
            RepoWatchRuleEvaluation::Matched {
                dispatch_id,
                event,
                rule_id,
                rule_version,
                singleton,
                cooldown,
                actions,
            } => {
                self.release_parked_obligations_for_event(&event).await?;
                let mut transaction = self.pool.begin().await?;
                let (singleton, matched_admission) = match admission {
                    EvaluationAdmission::Fresh => (
                        StoredSingletonKey::from_domain(&singleton),
                        MatchedAdmission::Fresh,
                    ),
                    EvaluationAdmission::Obligation(obligation) => {
                        let (obligation_id, owed_event, singleton) = obligation.into_parts();
                        if owed_event != event {
                            transaction.rollback().await?;
                            return Err(RepoWatchDispatchRepositoryError::Corruption(
                                "repository-watch obligation and evaluation disagree",
                            ));
                        }
                        (singleton, MatchedAdmission::Obligation { obligation_id })
                    }
                };
                lock_text(&mut transaction, event.repository().as_str()).await?;
                lock_text(
                    &mut transaction,
                    &singleton.lock_key(&rule_id, rule_version),
                )
                .await?;
                match matched_admission {
                    MatchedAdmission::Fresh => {
                        if let Some(outcome) = load_recorded_evaluation(
                            &mut transaction,
                            event.id(),
                            &rule_id,
                            rule_version,
                        )
                        .await?
                        {
                            transaction.rollback().await?;
                            return Ok(outcome);
                        }
                    }
                    MatchedAdmission::Obligation { obligation_id } => {
                        use crate::repo_watch_dispatch_obligation::ObligationAdmission;
                        match crate::repo_watch_dispatch_obligation::load_obligation_admission(
                            &mut transaction,
                            obligation_id,
                            event.id(),
                        )
                        .await?
                        {
                            ObligationAdmission::Pending => {}
                            // Parking withholds the obligation rather than
                            // settling it, so it reads as unfinished work no
                            // dispatch currently owns.
                            ObligationAdmission::Superseded | ObligationAdmission::Parked => {
                                transaction.rollback().await?;
                                return Ok(RepoWatchRuleEvaluationOutcome::Occupied);
                            }
                            ObligationAdmission::Settled(outcome) => {
                                transaction.rollback().await?;
                                return Ok(outcome);
                            }
                        }
                    }
                }
                if !rule_is_active(&mut transaction, &event, &rule_id, rule_version).await? {
                    transaction.rollback().await?;
                    return Ok(RepoWatchRuleEvaluationOutcome::Inactive);
                }
                let terminal_event = matches!(
                    event.kind(),
                    RepoWatchEventKindV1::PullRequestClosed
                        | RepoWatchEventKindV1::PullRequestMerged
                );
                let terminal_event_is_superseded = terminal_event
                    && terminal_event_has_later_terminal_cutoff(&mut transaction, &event).await?;
                if (!terminal_event || terminal_event_is_superseded)
                    && !event_target_is_open(&mut transaction, &event).await?
                {
                    match matched_admission {
                        MatchedAdmission::Fresh => {
                            insert_evaluation(
                                &mut transaction,
                                &event,
                                &rule_id,
                                rule_version,
                                RepoWatchEvaluationOutcomeStorageKind::TargetClosed,
                                None,
                            )
                            .await?;
                        }
                        MatchedAdmission::Obligation { obligation_id } => {
                            crate::repo_watch_dispatch_obligation::settle_target_closed_obligation(
                                &mut transaction,
                                obligation_id,
                                event.id(),
                            )
                            .await?;
                        }
                    }
                    commit(transaction).await?;
                    return Ok(RepoWatchRuleEvaluationOutcome::TargetClosed);
                }
                if matched_admission == MatchedAdmission::Fresh
                    && crate::repo_watch_dispatch_obligation::active_obligation_exists(
                        &mut transaction,
                        &rule_id,
                        rule_version,
                        &singleton,
                    )
                    .await?
                {
                    insert_evaluation(
                        &mut transaction,
                        &event,
                        &rule_id,
                        rule_version,
                        RepoWatchEvaluationOutcomeStorageKind::Coalesced,
                        None,
                    )
                    .await?;
                    crate::repo_watch_dispatch_obligation::record_dispatch_obligation(
                        &mut transaction,
                        dispatch_id,
                        None,
                        &event,
                        &rule_id,
                        rule_version,
                        &singleton,
                    )
                    .await?;
                    commit(transaction).await?;
                    return Ok(RepoWatchRuleEvaluationOutcome::Occupied);
                }
                if let Some(blocking_dispatch) =
                    occupying_dispatch(&mut transaction, &rule_id, rule_version, &singleton).await?
                {
                    if matched_admission == MatchedAdmission::Fresh {
                        insert_evaluation(
                            &mut transaction,
                            &event,
                            &rule_id,
                            rule_version,
                            RepoWatchEvaluationOutcomeStorageKind::Occupied,
                            None,
                        )
                        .await?;
                        crate::repo_watch_dispatch_obligation::record_dispatch_obligation(
                            &mut transaction,
                            dispatch_id,
                            Some(blocking_dispatch),
                            &event,
                            &rule_id,
                            rule_version,
                            &singleton,
                        )
                        .await?;
                        commit(transaction).await?;
                    } else {
                        transaction.rollback().await?;
                    }
                    return Ok(RepoWatchRuleEvaluationOutcome::Occupied);
                }
                if singleton_is_cooling_down(&mut transaction, &rule_id, rule_version, &singleton)
                    .await?
                {
                    if matched_admission == MatchedAdmission::Fresh {
                        insert_evaluation(
                            &mut transaction,
                            &event,
                            &rule_id,
                            rule_version,
                            RepoWatchEvaluationOutcomeStorageKind::Cooldown,
                            None,
                        )
                        .await?;
                        commit(transaction).await?;
                    } else {
                        transaction.rollback().await?;
                    }
                    return Ok(RepoWatchRuleEvaluationOutcome::Cooldown);
                }
                let action_count = i32::try_from(actions.len()).map_err(|_| {
                    RepoWatchDispatchRepositoryError::Corruption("action count exceeds storage")
                })?;
                let dispatch_start_lease_millis =
                    i64::try_from(self.dispatch_start_lease.as_millis()).map_err(|_| {
                        RepoWatchDispatchRepositoryError::Corruption(
                            "dispatch start lease exceeds storage",
                        )
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
                    if goal.session() != session {
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
                    sqlx::query(
                        "INSERT INTO repo_watch_dispatch_start_lease
                            (dispatch_id, action_ordinal, session_id, expires_at)
                         VALUES (
                            $1, $2, $3,
                            transaction_timestamp()
                                + $4 * INTERVAL '1 millisecond'
                         )",
                    )
                    .bind(dispatch_id.as_uuid())
                    .bind(ordinal)
                    .bind(session_id_to_uuid(session))
                    .bind(dispatch_start_lease_millis)
                    .execute(&mut *transaction)
                    .await?;
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
                    // The commission adopts the turn just accepted above rather
                    // than scheduling one of its own, so the session runs its
                    // template once, against the tagged context, under the
                    // generation that turn is recorded in.
                    crate::goal::insert_fresh_commissioned_goal(
                        &mut transaction,
                        goal,
                        accepted_input,
                        turn,
                    )
                    .await
                    .map_err(RepoWatchDispatchRepositoryError::GoalCommission)?;
                    sessions.push(session);
                }
                match matched_admission {
                    MatchedAdmission::Fresh => {
                        insert_evaluation(
                            &mut transaction,
                            &event,
                            &rule_id,
                            rule_version,
                            RepoWatchEvaluationOutcomeStorageKind::Dispatched,
                            Some(dispatch_id),
                        )
                        .await?;
                    }
                    MatchedAdmission::Obligation { obligation_id } => {
                        crate::repo_watch_dispatch_obligation::settle_dispatch_obligation(
                            &mut transaction,
                            obligation_id,
                            event.id(),
                            dispatch_id,
                        )
                        .await?;
                    }
                }
                commit(transaction).await?;
                Ok(RepoWatchRuleEvaluationOutcome::Dispatched {
                    dispatch_id,
                    sessions: sessions.into_boxed_slice(),
                })
            }
        }
    }
}

enum EvaluationAdmission {
    Fresh,
    Obligation(Box<crate::repo_watch_dispatch_obligation::RepoWatchDispatchObligation>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchedAdmission {
    Fresh,
    Obligation { obligation_id: Uuid },
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
    /// Releases every parked obligation this event is progress for.
    ///
    /// Both terminal evaluation paths call this, because a parked lineage is
    /// released by its pull request moving on and not by the moving event
    /// happening to match the rule that parked it.
    ///
    /// Committed on its own, before any singleton key is taken. A rule-scoped
    /// singleton spans repositories, so an evaluation holding one can own the
    /// obligation row of a lineage parked in another repository while this scan
    /// wants a row that another repository's evaluation owns the same way;
    /// inside the critical section those two waits close a cycle. Releasing
    /// first costs only idempotent repetition if the evaluation that follows
    /// fails, since a release is recorded against the event that bought it.
    async fn release_parked_obligations_for_event(
        &self,
        event: &RepoWatchEvent,
    ) -> Result<(), RepoWatchDispatchRepositoryError> {
        sqlx::query("SELECT repo_watch_release_dispatch_obligation_parks_for_event($1)")
            .bind(event.id().as_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn record_simple_outcome(
        &self,
        event: &RepoWatchEvent,
        rule_id: &RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
        outcome: RepoWatchEvaluationOutcomeStorageKind,
    ) -> Result<RepoWatchRuleEvaluationOutcome, RepoWatchDispatchRepositoryError> {
        self.release_parked_obligations_for_event(event).await?;
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
pub(crate) struct StoredSingletonKey {
    pub(crate) scope: RepoWatchSingletonScopeStorageKind,
    pub(crate) repository: Option<String>,
    pub(crate) pull_request: Option<Decimal>,
    pub(crate) stack_root_pull_request: Option<Decimal>,
}

impl StoredSingletonKey {
    fn from_domain(key: &RepoWatchSingletonKey) -> Self {
        match key {
            RepoWatchSingletonKey::PullRequest { repository, number } => Self {
                scope: RepoWatchSingletonScopeStorageKind::PullRequest,
                repository: Some(repository.as_str().to_owned()),
                pull_request: Some(Decimal::from(number.get())),
                stack_root_pull_request: None,
            },
            RepoWatchSingletonKey::Stack {
                repository,
                root_pull_request,
            } => Self {
                scope: RepoWatchSingletonScopeStorageKind::Stack,
                repository: Some(repository.as_str().to_owned()),
                pull_request: None,
                stack_root_pull_request: Some(Decimal::from(root_pull_request.get())),
            },
            RepoWatchSingletonKey::Rule => Self {
                scope: RepoWatchSingletonScopeStorageKind::Rule,
                repository: None,
                pull_request: None,
                stack_root_pull_request: None,
            },
            RepoWatchSingletonKey::Repository { repository } => Self {
                scope: RepoWatchSingletonScopeStorageKind::Repository,
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
            repo_watch_singleton_scope_to_str(self.scope),
            self.repository.as_deref().unwrap_or(""),
            self.pull_request
                .map_or(String::new(), |value| value.to_string()),
            self.stack_root_pull_request
                .map_or(String::new(), |value| value.to_string())
        )
    }
}

/// Reconciles every configured repository and retires the rest.
///
/// The caller owns the transaction, so the same admission decision serves both
/// the validating pass that discards it and the committing pass that keeps it.
async fn admit_configured_rules(
    transaction: &mut Transaction<'_, Postgres>,
    repositories: &[RepositorySlug],
    configured: &[RepoWatchRule],
) -> Result<(), RepoWatchDispatchRepositoryError> {
    let ordered = repositories
        .iter()
        .map(|repository| repository.as_str())
        .collect::<BTreeSet<_>>();
    for repository in &ordered {
        reconcile_repository_rules(transaction, repository, configured).await?;
    }
    retire_unconfigured_repositories(transaction, &ordered).await
}

/// Retires every active repository absent from the configured set.
///
/// The caller owns the transaction and its configuration lock, so retirement
/// commits with the rule admission it accompanies or with neither.
async fn retire_unconfigured_repositories(
    transaction: &mut Transaction<'_, Postgres>,
    configured: &BTreeSet<&str>,
) -> Result<(), RepoWatchDispatchRepositoryError> {
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
    .fetch_all(&mut **transaction)
    .await?;
    for repository in active_repositories {
        if configured.contains(repository.as_str()) {
            continue;
        }
        lock_text(transaction, &repository).await?;
        // Removing a repository retires its rules through this path rather
        // than through `reconcile_repository_rules`, so the same stored-shape
        // validation runs here before any deactivation is appended.
        let unfingerprinted: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM repo_watch_rule_activation AS activation
                  WHERE activation.repository = $1
                    AND NOT EXISTS (
                        SELECT 1
                          FROM repo_watch_rule_deactivation AS deactivation
                         WHERE deactivation.repository = activation.repository
                           AND deactivation.rule_id = activation.rule_id
                           AND deactivation.rule_version = activation.rule_version
                    )
                    AND NOT EXISTS (
                        SELECT 1
                          FROM repo_watch_rule_field_fingerprint AS fingerprint
                         WHERE fingerprint.repository = activation.repository
                           AND fingerprint.rule_id = activation.rule_id
                           AND fingerprint.rule_version = activation.rule_version
                    )
             )",
        )
        .bind(&repository)
        .fetch_one(&mut **transaction)
        .await?;
        if unfingerprinted {
            return Err(RepoWatchDispatchRepositoryError::Corruption(
                "active repository-watch rule activation has no field fingerprints",
            ));
        }
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
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

/// Reconciles one repository's configured rules inside an open transaction.
///
/// Every refusal returns before the caller commits, so the caller decides
/// whether the surrounding admission survives.
async fn reconcile_repository_rules(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &str,
    configured: &[RepoWatchRule],
) -> Result<(), RepoWatchDispatchRepositoryError> {
    lock_text(transaction, repository).await?;
    let configured = configured
        .iter()
        .map(|rule| {
            Ok((
                (
                    rule.id().as_str().to_owned(),
                    stored_rule_version(rule.version())?,
                ),
                ConfiguredRuleIdentity::from_rule(rule),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, RepoWatchDispatchRepositoryError>>()?;
    let configured_identities = configured.keys().cloned().collect::<BTreeSet<_>>();
    let existing = sqlx::query(
        "SELECT activation.rule_id, activation.rule_version, activation.rule_digest,
                fingerprint.rule_field_digests,
                deactivation.rule_id IS NOT NULL AS deactivated
           FROM repo_watch_rule_activation AS activation
           LEFT JOIN repo_watch_rule_deactivation AS deactivation
             USING (repository, rule_id, rule_version)
           LEFT JOIN repo_watch_rule_field_fingerprint AS fingerprint
             USING (repository, rule_id, rule_version)
          WHERE activation.repository = $1",
    )
    .bind(repository)
    .fetch_all(&mut **transaction)
    .await?;
    let mut historical = BTreeSet::new();
    let mut latest_versions: BTreeMap<String, i64> = BTreeMap::new();
    let mut active = BTreeSet::new();
    for row in existing {
        let identity: (String, i64) = (row.try_get("rule_id")?, row.try_get("rule_version")?);
        historical.insert(identity.clone());
        latest_versions
            .entry(identity.0.clone())
            .and_modify(|latest| *latest = (*latest).max(identity.1))
            .or_insert(identity.1);
        if row.try_get::<bool, _>("deactivated")? {
            continue;
        }
        let stored_digest: Vec<u8> = row.try_get("rule_digest")?;
        // An activation the configured set omits is still retired against its
        // stored shape, so this precedes the configured lookup. The column's
        // length is a database CHECK, leaving absence as the only defect this
        // read can observe.
        let Some(stored_field_digests) = row.try_get::<Option<Vec<u8>>, _>("rule_field_digests")?
        else {
            return Err(RepoWatchDispatchRepositoryError::Corruption(
                "active repository-watch rule activation has no field fingerprints",
            ));
        };
        if let Some(configured_rule) = configured.get(&identity) {
            let changed_field = configured_rule.changed_field(&stored_field_digests)?;
            if stored_digest.as_slice() != configured_rule.content_digest.as_slice() {
                let field = changed_field.ok_or(RepoWatchDispatchRepositoryError::Corruption(
                    "rule digest changed while every field fingerprint remained equal",
                ))?;
                return Err(RepoWatchDispatchRepositoryError::ChangedRuleIdentity {
                    rule_id: stored_rule_id(&identity.0)?,
                    rule_version: decoded_rule_version(identity.1)?,
                    field,
                });
            }
            if changed_field.is_some() {
                return Err(RepoWatchDispatchRepositoryError::Corruption(
                    "rule field fingerprint changed while its complete digest remained equal",
                ));
            }
        }
        active.insert(identity);
    }
    for (rule_id, rule_version) in active.difference(&configured_identities) {
        sqlx::query(
            "INSERT INTO repo_watch_rule_deactivation
                (repository, rule_id, rule_version)
             VALUES ($1, $2, $3)",
        )
        .bind(repository)
        .bind(rule_id)
        .bind(rule_version)
        .execute(&mut **transaction)
        .await?;
    }
    for (rule_id, rule_version) in configured_identities.difference(&active) {
        if historical.contains(&(rule_id.clone(), *rule_version)) {
            return Err(RepoWatchDispatchRepositoryError::ReusedRuleIdentity {
                rule_id: stored_rule_id(rule_id)?,
                rule_version: decoded_rule_version(*rule_version)?,
            });
        }
        if let Some(latest) = latest_versions
            .get(rule_id)
            .copied()
            .filter(|latest| rule_version < latest)
        {
            return Err(RepoWatchDispatchRepositoryError::RegressedRuleVersion {
                rule_id: stored_rule_id(rule_id)?,
                rule_version: decoded_rule_version(*rule_version)?,
                latest_version: decoded_rule_version(latest)?,
            });
        }
        let configured_rule = configured.get(&(rule_id.clone(), *rule_version)).ok_or(
            RepoWatchDispatchRepositoryError::Corruption("configured rule identity missing"),
        )?;
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
        .bind(repository)
        .bind(rule_id)
        .bind(rule_version)
        .bind(configured_rule.content_digest.as_slice())
        .execute(&mut **transaction)
        .await?;
        sqlx::query(
            "INSERT INTO repo_watch_rule_field_fingerprint
                (repository, rule_id, rule_version, rule_field_digests)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(repository)
        .bind(rule_id)
        .bind(rule_version)
        .bind(configured_rule.encoded_field_digests())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
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
    .bind(repo_watch_singleton_scope_to_str(batch.singleton.scope))
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
    outcome: RepoWatchEvaluationOutcomeStorageKind,
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
    .bind(repo_watch_evaluation_outcome_to_str(outcome))
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
    match repo_watch_evaluation_outcome_from_str(&outcome) {
        Some(RepoWatchEvaluationOutcomeStorageKind::NotMatched) => {
            Ok(Some(RepoWatchRuleEvaluationOutcome::NotMatched))
        }
        Some(RepoWatchEvaluationOutcomeStorageKind::TargetClosed) => {
            Ok(Some(RepoWatchRuleEvaluationOutcome::TargetClosed))
        }
        Some(
            RepoWatchEvaluationOutcomeStorageKind::Occupied
            | RepoWatchEvaluationOutcomeStorageKind::Coalesced,
        ) => Ok(Some(RepoWatchRuleEvaluationOutcome::Occupied)),
        Some(RepoWatchEvaluationOutcomeStorageKind::Cooldown) => {
            Ok(Some(RepoWatchRuleEvaluationOutcome::Cooldown))
        }
        Some(RepoWatchEvaluationOutcomeStorageKind::Dispatched) => {
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
        None => Err(RepoWatchDispatchRepositoryError::Corruption(
            "evaluation outcome is unsupported",
        )),
    }
}

async fn occupying_dispatch(
    transaction: &mut Transaction<'_, Postgres>,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    key: &StoredSingletonKey,
) -> Result<Option<RepoWatchDispatchId>, RepoWatchDispatchRepositoryError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT batch.dispatch_id
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
          ORDER BY batch.admitted_at
          LIMIT 1",
    )
    .bind(rule_id.as_str())
    .bind(i64::try_from(rule_version.get()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage")
    })?)
    .bind(repo_watch_singleton_scope_to_str(key.scope))
    .bind(key.repository.as_deref())
    .bind(key.pull_request)
    .bind(key.stack_root_pull_request)
    .fetch_optional(&mut **transaction)
    .await?
    .map(RepoWatchDispatchId::from_uuid))
}

async fn event_target_is_open(
    transaction: &mut Transaction<'_, Postgres>,
    event: &RepoWatchEvent,
) -> Result<bool, RepoWatchDispatchRepositoryError> {
    let RepoWatchEventTarget::PullRequest(context) = event.target() else {
        return Ok(true);
    };
    let lifecycle = sqlx::query_scalar::<_, String>(
        "SELECT event_kind
           FROM repo_watch_event
          WHERE repository = $1
            AND pull_request_number = $2
            AND event_kind IN (
                'pull_request_opened', 'pull_request_closed', 'pull_request_merged'
            )
          ORDER BY cursor_generation DESC, event_ordinal DESC
          LIMIT 1",
    )
    .bind(event.repository().as_str())
    .bind(Decimal::from(context.number().get()))
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(RepoWatchDispatchRepositoryError::Corruption(
        "pull-request event has no durable lifecycle",
    ))?;
    let lifecycle = repo_watch_event_kind_from_str(&lifecycle).ok_or(
        RepoWatchDispatchRepositoryError::Corruption(
            "pull-request lifecycle has an unknown event kind",
        ),
    )?;
    Ok(lifecycle == RepoWatchEventKindNameV1::PullRequestOpened)
}

async fn terminal_event_has_later_terminal_cutoff(
    transaction: &mut Transaction<'_, Postgres>,
    event: &RepoWatchEvent,
) -> Result<bool, RepoWatchDispatchRepositoryError> {
    let dispositions: Vec<String> = sqlx::query_scalar(
        "SELECT cutoff.disposition_kind
           FROM repo_watch_event AS origin
           JOIN repo_watch_event AS boundary
             ON boundary.repository = origin.repository
            AND boundary.pull_request_number = origin.pull_request_number
            AND (boundary.cursor_generation, boundary.event_ordinal) >
                (origin.cursor_generation, origin.event_ordinal)
           JOIN repo_watch_lifecycle_cutoff AS cutoff
             ON cutoff.event_id = boundary.event_id
          WHERE origin.event_id = $1
            AND origin.repository = $2",
    )
    .bind(event.id().as_uuid())
    .bind(event.repository().as_str())
    .fetch_all(&mut **transaction)
    .await?;
    for disposition in dispositions {
        let decoded = repo_watch_lifecycle_cutoff_disposition_from_str(&disposition).ok_or(
            RepoWatchDispatchRepositoryError::Corruption(
                "repository-watch lifecycle cutoff has an unknown disposition",
            ),
        )?;
        if decoded == RepoWatchLifecycleCutoffDispositionStorageKind::Terminal {
            return Ok(true);
        }
    }
    Ok(false)
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
    .bind(repo_watch_singleton_scope_to_str(key.scope))
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

pub(crate) fn stored_rule_version(
    version: RepoWatchRuleVersion,
) -> Result<i64, RepoWatchDispatchRepositoryError> {
    i64::try_from(version.get())
        .map_err(|_| RepoWatchDispatchRepositoryError::Corruption("rule version exceeds storage"))
}

fn decoded_rule_version(
    version: i64,
) -> Result<RepoWatchRuleVersion, RepoWatchDispatchRepositoryError> {
    u64::try_from(version)
        .ok()
        .and_then(NonZeroU64::new)
        .and_then(RepoWatchRuleVersion::new)
        .ok_or(RepoWatchDispatchRepositoryError::Corruption(
            "stored rule version is invalid",
        ))
}

fn stored_rule_id(rule_id: &str) -> Result<RepoWatchRuleId, RepoWatchDispatchRepositoryError> {
    RepoWatchRuleId::try_new(rule_id.to_owned()).map_err(|_| {
        RepoWatchDispatchRepositoryError::Corruption("stored rule identifier is invalid")
    })
}
