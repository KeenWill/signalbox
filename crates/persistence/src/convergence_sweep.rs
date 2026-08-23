//! Durable state, retry, parking, and commissioned-session census for convergence sweeps.

use std::{
    error::Error,
    fmt,
    time::{Duration, SystemTime},
};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_domain::{CommitSha, DurableCommandId, PullRequestNumber, RepositorySlug, SessionId};
use sqlx::{
    PgConnection, PgPool,
    types::{Uuid, time::OffsetDateTime},
};

use crate::mapping::{
    ConvergenceSweepOutcomeStorageKind, ConvergenceSweepStateStorageKind,
    convergence_sweep_decision_outcome, convergence_sweep_failure_from_str,
    convergence_sweep_failure_outcome, convergence_sweep_failure_to_str,
    convergence_sweep_operator_need_to_str, convergence_sweep_outcome_to_str,
    convergence_sweep_state_from_str, convergence_sweep_state_to_str, session_id_from_uuid,
};

/// The exact pull-request observation used for movement and dispatch-effect checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepObservation {
    head_sha: CommitSha,
    unresolved_threads: u64,
}

impl ConvergenceSweepObservation {
    pub const fn new(head_sha: CommitSha, unresolved_threads: u64) -> Self {
        Self {
            head_sha,
            unresolved_threads,
        }
    }

    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
    }

    pub const fn unresolved_threads(&self) -> u64 {
        self.unresolved_threads
    }
}

/// Failure classes with independent durable retry lineages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvergenceSweepFailureKind {
    FactsFetch,
    CommissionRefused,
    TemplateDrift,
    NoModelActivity,
    StateAccess,
}

/// Non-failure decisions retained in the append-only audit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvergenceSweepDecision {
    Converged,
    CoolingOff,
    LiveSession,
}

/// Latest commissioned session for this pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepDispatchState {
    dispatch_id: Uuid,
    session_id: SessionId,
    dispatched_at: SystemTime,
    live: bool,
    has_model_activity: bool,
}

impl ConvergenceSweepDispatchState {
    pub const fn dispatch_id(&self) -> Uuid {
        self.dispatch_id
    }
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }
    pub const fn dispatched_at(&self) -> SystemTime {
        self.dispatched_at
    }
    pub const fn is_live(&self) -> bool {
        self.live
    }
    pub const fn has_model_activity(&self) -> bool {
        self.has_model_activity
    }
}

/// Durable state needed to decide one target's next action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepTargetState {
    parked: bool,
    retry_ready: bool,
    cool_off_elapsed: bool,
    failure_kind: Option<ConvergenceSweepFailureKind>,
    consecutive_failures: u16,
    pending_command: Option<DurableCommandId>,
    pending_observation: Option<ConvergenceSweepObservation>,
    last_observation: Option<ConvergenceSweepObservation>,
    latest_dispatch_observation: Option<ConvergenceSweepObservation>,
    pending_dispatch: Option<ConvergenceSweepDispatchState>,
    latest_dispatch: Option<ConvergenceSweepDispatchState>,
}

impl ConvergenceSweepTargetState {
    pub const fn is_parked(&self) -> bool {
        self.parked
    }
    pub const fn retry_ready(&self) -> bool {
        self.retry_ready
    }
    pub const fn cool_off_elapsed(&self) -> bool {
        self.cool_off_elapsed
    }
    pub const fn failure_kind(&self) -> Option<ConvergenceSweepFailureKind> {
        self.failure_kind
    }
    pub const fn consecutive_failures(&self) -> u16 {
        self.consecutive_failures
    }
    pub const fn pending_command(&self) -> Option<DurableCommandId> {
        self.pending_command
    }
    pub const fn pending_observation(&self) -> Option<&ConvergenceSweepObservation> {
        self.pending_observation.as_ref()
    }
    pub const fn last_observation(&self) -> Option<&ConvergenceSweepObservation> {
        self.last_observation.as_ref()
    }
    pub const fn latest_dispatch_observation(&self) -> Option<&ConvergenceSweepObservation> {
        self.latest_dispatch_observation.as_ref()
    }
    pub const fn pending_dispatch(&self) -> Option<&ConvergenceSweepDispatchState> {
        self.pending_dispatch.as_ref()
    }
    pub const fn latest_dispatch(&self) -> Option<&ConvergenceSweepDispatchState> {
        self.latest_dispatch.as_ref()
    }
}

/// Result of recording one failure against its bounded lineage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvergenceSweepFailureDisposition {
    RetryScheduled,
    Parked,
    ActivityObserved,
}

/// Delay bounds for one convergence-sweep failure lineage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepRetryPolicy {
    /// Delay used for the first retry in a failure lineage.
    pub backoff_base: Duration,
    /// Maximum delay permitted for a retry in the lineage.
    pub backoff_cap: Duration,
}

impl ConvergenceSweepRetryPolicy {
    fn stored_backoff_base_seconds(self) -> i64 {
        i64::try_from(self.backoff_base.as_secs()).unwrap_or(i64::MAX)
    }

    fn stored_backoff_cap_seconds(self) -> i64 {
        i64::try_from(self.backoff_cap.as_secs()).unwrap_or(i64::MAX)
    }
}

#[derive(Debug, sqlx::FromRow)]
struct FailureTransitionRow {
    consecutive_failures: i16,
    parking_kind: String,
}

#[derive(Debug, sqlx::FromRow)]
struct TargetStateRow {
    state_kind: String,
    failure_kind: Option<String>,
    consecutive_failures: i16,
    retry_readiness: String,
    cool_off_readiness: String,
    pending_command_id: Option<Uuid>,
    pending_head_sha: Option<String>,
    pending_unresolved_threads: Option<Decimal>,
    last_head_sha: Option<String>,
    last_unresolved_threads: Option<Decimal>,
    latest_dispatch_head_sha: Option<String>,
    latest_dispatch_unresolved_threads: Option<Decimal>,
    pending_dispatch_id: Option<Uuid>,
    pending_session_id: Option<Uuid>,
    pending_recorded_at: Option<OffsetDateTime>,
    pending_liveness_kind: Option<String>,
    pending_activity_kind: Option<String>,
    dispatch_id: Option<Uuid>,
    session_id: Option<Uuid>,
    recorded_at: Option<OffsetDateTime>,
    liveness_kind: Option<String>,
    activity_kind: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Readiness {
    Ready,
    Waiting,
}

impl Readiness {
    fn decode(value: &str) -> Result<Self, ConvergenceSweepStoreError> {
        match value {
            "ready" => Ok(Self::Ready),
            "waiting" => Ok(Self::Waiting),
            _ => Err(ConvergenceSweepStoreError::Corruption(
                "invalid target readiness kind",
            )),
        }
    }

    const fn is_ready(self) -> bool {
        match self {
            Self::Ready => true,
            Self::Waiting => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchLiveness {
    Live,
    Terminal,
}

impl DispatchLiveness {
    fn decode(value: &str) -> Result<Self, ConvergenceSweepStoreError> {
        match value {
            "live" => Ok(Self::Live),
            "terminal" => Ok(Self::Terminal),
            _ => Err(ConvergenceSweepStoreError::Corruption(
                "invalid dispatch liveness kind",
            )),
        }
    }

    const fn is_live(self) -> bool {
        match self {
            Self::Live => true,
            Self::Terminal => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelActivity {
    Present,
    Absent,
}

impl ModelActivity {
    fn decode(value: &str) -> Result<Self, ConvergenceSweepStoreError> {
        match value {
            "present" => Ok(Self::Present),
            "absent" => Ok(Self::Absent),
            _ => Err(ConvergenceSweepStoreError::Corruption(
                "invalid dispatch activity kind",
            )),
        }
    }

    const fn is_present(self) -> bool {
        match self {
            Self::Present => true,
            Self::Absent => false,
        }
    }
}

struct FailureRecord<'a> {
    observation: Option<&'a ConvergenceSweepObservation>,
    failure: ConvergenceSweepFailureKind,
    retry_policy: ConvergenceSweepRetryPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureParking {
    RetryScheduled,
    Parked,
}

impl FailureParking {
    fn decode(value: &str) -> Result<Self, ConvergenceSweepStoreError> {
        match value {
            "retry_scheduled" => Ok(Self::RetryScheduled),
            "parked" => Ok(Self::Parked),
            _ => Err(ConvergenceSweepStoreError::Corruption(
                "failure transition has an invalid parking kind",
            )),
        }
    }
}

/// Database or durable-shape failure in convergence sweep storage.
#[derive(Debug)]
pub enum ConvergenceSweepStoreError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Corruption(&'static str),
}

impl fmt::Display for ConvergenceSweepStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("convergence sweep database operation failed"),
            Self::CommitAmbiguous(_) => {
                formatter.write_str("convergence sweep commit outcome is ambiguous")
            }
            Self::Corruption(reason) => write!(
                formatter,
                "convergence sweep state is inconsistent: {reason}"
            ),
        }
    }
}

impl Error for ConvergenceSweepStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for ConvergenceSweepStoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

impl ConvergenceSweepStoreError {
    pub const fn commit_ambiguous(&self) -> bool {
        matches!(self, Self::CommitAmbiguous(_))
    }
}

/// PostgreSQL adapter for one daemon-native convergence sweep.
#[derive(Clone, Debug)]
pub struct PostgresConvergenceSweepStore {
    pool: PgPool,
}

impl PostgresConvergenceSweepStore {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reconciles durable operator-visible membership with configured targets.
    pub async fn reconcile_configured_targets(
        &self,
        configured: &[(RepositorySlug, PullRequestNumber)],
    ) -> Result<(), ConvergenceSweepStoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("UPDATE convergence_sweep_target SET enrolled = false")
            .execute(&mut *transaction)
            .await?;
        for (repository, pull_request) in configured {
            ensure_target(&mut transaction, repository, *pull_request).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Re-enrolls one configured target, making daemon restart its explicit recovery path.
    pub async fn reenroll_target(
        &self,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
    ) -> Result<(), ConvergenceSweepStoreError> {
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        sqlx::query(
            "UPDATE convergence_sweep_target
                SET state_kind = $3, failure_kind = NULL,
                    consecutive_failures = 0, retry_not_before = NULL,
                    parked_at = NULL, operator_need = NULL,
                    parked_dispatch_id = NULL, parked_session_id = NULL,
                    parked_dispatched_at = NULL
              WHERE repository = $1 AND pull_request_number = $2
                AND state_kind = $4",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(convergence_sweep_state_to_str(
            ConvergenceSweepStateStorageKind::Observed,
        ))
        .bind(convergence_sweep_state_to_str(
            ConvergenceSweepStateStorageKind::Parked,
        ))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Loads retry/park state and the latest globally commissioned session.
    pub async fn load_target(
        &self,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
    ) -> Result<Option<ConvergenceSweepTargetState>, ConvergenceSweepStoreError> {
        self.load_target_with_cool_off(repository, pull_request, std::time::Duration::ZERO)
            .await
    }

    /// Loads target state while evaluating dispatch cool-off on the database clock.
    pub async fn load_target_with_cool_off(
        &self,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        cool_off: std::time::Duration,
    ) -> Result<Option<ConvergenceSweepTargetState>, ConvergenceSweepStoreError> {
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        let row: Option<TargetStateRow> = sqlx::query_as(
            "SELECT target.state_kind, target.failure_kind,
                    target.consecutive_failures,
                    CASE WHEN target.retry_not_before IS NULL
                              OR target.retry_not_before <= clock_timestamp()
                         THEN 'ready' ELSE 'waiting'
                    END AS retry_readiness,
                    CASE WHEN coalesce(
                        latest.recorded_at + $3 * interval '1 second' <= clock_timestamp(),
                        true
                    ) THEN 'ready' ELSE 'waiting'
                    END AS cool_off_readiness,
                    target.pending_command_id, target.pending_head_sha,
                    target.pending_unresolved_threads,
                    target.last_head_sha, target.last_unresolved_threads,
                    CASE
                      WHEN latest.dispatch_id = target.last_dispatch_id
                        THEN target.last_dispatch_head_sha
                      WHEN latest.dispatch_id = pending.dispatch_id
                        THEN target.pending_head_sha
                      WHEN latest.dispatch_id = target.census_dispatch_id
                       AND latest.session_id = target.census_session_id
                        THEN target.census_dispatch_head_sha
                    END AS latest_dispatch_head_sha,
                    CASE
                      WHEN latest.dispatch_id = target.last_dispatch_id
                        THEN target.last_dispatch_unresolved_threads
                      WHEN latest.dispatch_id = pending.dispatch_id
                        THEN target.pending_unresolved_threads
                      WHEN latest.dispatch_id = target.census_dispatch_id
                       AND latest.session_id = target.census_session_id
                        THEN target.census_dispatch_unresolved_threads
                    END AS latest_dispatch_unresolved_threads,
                    pending.dispatch_id AS pending_dispatch_id,
                    pending.session_id AS pending_session_id,
                    pending.recorded_at AS pending_recorded_at,
                    CASE WHEN pending.dispatch_id IS NULL THEN NULL
                         WHEN pending.live THEN 'live' ELSE 'terminal'
                    END AS pending_liveness_kind,
                    CASE WHEN pending.dispatch_id IS NULL THEN NULL
                         WHEN pending.has_model_activity THEN 'present' ELSE 'absent'
                    END AS pending_activity_kind,
                    latest.dispatch_id, latest.session_id, latest.recorded_at,
                    CASE WHEN latest.dispatch_id IS NULL THEN NULL
                         WHEN latest.live THEN 'live' ELSE 'terminal'
                    END AS liveness_kind,
                    CASE WHEN latest.dispatch_id IS NULL THEN NULL
                         WHEN latest.has_model_activity THEN 'present' ELSE 'absent'
                    END AS activity_kind
               FROM convergence_sweep_target AS target
               LEFT JOIN LATERAL (
                    SELECT dispatch.dispatch_id, dispatch.session_id,
                           dispatch.recorded_at,
                           coalesce((
                               SELECT event.event_kind IN ('commissioned', 'resumed', 'superseded')
                                 FROM goal_event AS event
                                WHERE event.session_id = dispatch.session_id
                                ORDER BY event.event_ordinal DESC LIMIT 1
                           ), false) AS live,
                           EXISTS (
                               SELECT 1 FROM model_call AS call
                                WHERE call.session_id = dispatch.session_id
                           ) AS has_model_activity
                      FROM commissioned_dispatch AS dispatch
                     WHERE dispatch.create_command_id = target.pending_command_id
                     LIMIT 1
               ) AS pending ON true
               LEFT JOIN LATERAL (
                    SELECT source.dispatch_id, source.session_id,
                           source.recorded_at,
                           coalesce((
                               SELECT event.event_kind IN ('commissioned', 'resumed', 'superseded')
                                 FROM goal_event AS event
                                WHERE event.session_id = source.session_id
                                ORDER BY event.event_ordinal DESC LIMIT 1
                           ), false) AS live,
                           EXISTS (
                               SELECT 1 FROM model_call AS call
                                WHERE call.session_id = source.session_id
                           ) AS has_model_activity
                      FROM (
                           SELECT dispatch.dispatch_id, dispatch.session_id,
                                  dispatch.recorded_at
                             FROM commissioned_dispatch AS dispatch
                            WHERE dispatch.target_kind = 'pull_request'
                              AND dispatch.repository = target.repository
                              AND dispatch.pull_request_number = target.pull_request_number
                           UNION ALL
                           SELECT action.dispatch_id, action.session_id,
                                  batch.admitted_at AS recorded_at
                             FROM repo_watch_dispatch_action AS action
                             JOIN repo_watch_event AS event ON event.event_id = action.event_id
                             JOIN repo_watch_dispatch_batch AS batch
                               ON batch.dispatch_id = action.dispatch_id
                            WHERE event.target_kind = 'pull_request'
                              AND event.repository = target.repository
                              AND event.pull_request_number = target.pull_request_number
                      ) AS source
                     ORDER BY source.recorded_at DESC, source.dispatch_id DESC,
                              live DESC, has_model_activity DESC, source.session_id DESC
                     LIMIT 1
               ) AS latest ON true
              WHERE target.repository = $1
                AND target.pull_request_number = $2",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(i64::try_from(cool_off.as_secs()).unwrap_or(i64::MAX))
        .fetch_optional(&mut *transaction)
        .await?;
        let state = row.map(decode_target_state).transpose()?;
        transaction.commit().await?;
        Ok(state)
    }

    /// Installs or reuses the idempotency fence before commissioning.
    pub async fn begin_commission(
        &self,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        observation: &ConvergenceSweepObservation,
        content_digest: [u8; 32],
        proposed_command: DurableCommandId,
    ) -> Result<DurableCommandId, ConvergenceSweepStoreError> {
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        let command: Uuid = sqlx::query_scalar(
            "UPDATE convergence_sweep_target
                SET pending_command_id = CASE
                        WHEN pending_command_id IS NOT NULL
                         AND pending_head_sha = $4
                         AND pending_unresolved_threads = $5
                         AND pending_content_digest = $6
                        THEN pending_command_id ELSE $3 END,
                    pending_head_sha = $4,
                    pending_unresolved_threads = $5,
                    pending_content_digest = $6,
                    pending_started_at = CASE
                        WHEN pending_command_id IS NOT NULL
                         AND pending_head_sha = $4
                         AND pending_unresolved_threads = $5
                         AND pending_content_digest = $6
                        THEN pending_started_at ELSE clock_timestamp() END
              WHERE repository = $1 AND pull_request_number = $2
          RETURNING pending_command_id",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(proposed_command.as_uuid())
        .bind(observation.head_sha().as_str())
        .bind(Decimal::from(observation.unresolved_threads()))
        .bind(content_digest.to_vec())
        .fetch_one(&mut *transaction)
        .await?;
        match transaction.commit().await {
            Ok(()) => Ok(DurableCommandId::from_uuid(command)),
            Err(error) if crate::commit_failure_is_ambiguous(&error) => {
                let fence_landed = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                        SELECT 1 FROM convergence_sweep_target
                         WHERE repository = $1 AND pull_request_number = $2
                           AND pending_command_id = $3
                           AND pending_head_sha = $4
                           AND pending_unresolved_threads = $5
                           AND pending_content_digest = $6
                    )",
                )
                .bind(repository.as_str())
                .bind(Decimal::from(pull_request.get()))
                .bind(command)
                .bind(observation.head_sha().as_str())
                .bind(Decimal::from(observation.unresolved_threads()))
                .bind(content_digest.to_vec())
                .fetch_one(&self.pool)
                .await;
                resolve_ambiguous_fence_commit(error, fence_landed)
                    .map(|()| DurableCommandId::from_uuid(command))
            }
            Err(error) => Err(ConvergenceSweepStoreError::Database(error)),
        }
    }

    /// Records a committed or replayed atomic commissioned dispatch.
    pub async fn record_dispatch(
        &self,
        event_id: Uuid,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        observation: &ConvergenceSweepObservation,
        dispatch_id: Uuid,
        session_id: SessionId,
    ) -> Result<(), ConvergenceSweepStoreError> {
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        let updated = sqlx::query(
            "UPDATE convergence_sweep_target
                SET state_kind = $7, failure_kind = NULL,
                    consecutive_failures = 0, retry_not_before = NULL,
                    parked_at = NULL, operator_need = NULL,
                    parked_dispatch_id = NULL, parked_session_id = NULL,
                    parked_dispatched_at = NULL,
                    last_head_sha = $3, last_unresolved_threads = $4,
                    last_observed_at = clock_timestamp(),
                    pending_command_id = NULL, pending_head_sha = NULL,
                    pending_unresolved_threads = NULL, pending_content_digest = NULL,
                    pending_started_at = NULL,
                    last_dispatch_id = dispatch.dispatch_id,
                    last_session_id = dispatch.session_id,
                    last_dispatched_at = dispatch.recorded_at,
                    last_dispatch_head_sha = $3,
                    last_dispatch_unresolved_threads = $4
               FROM commissioned_dispatch AS dispatch
              WHERE convergence_sweep_target.repository = $1
                AND convergence_sweep_target.pull_request_number = $2
                AND dispatch.dispatch_id = $5
                AND dispatch.session_id = $6
                AND dispatch.target_kind = 'pull_request'
                AND dispatch.repository = convergence_sweep_target.repository
                AND dispatch.pull_request_number =
                    convergence_sweep_target.pull_request_number",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(observation.head_sha().as_str())
        .bind(Decimal::from(observation.unresolved_threads()))
        .bind(dispatch_id)
        .bind(session_id.into_uuid())
        .bind(convergence_sweep_state_to_str(
            ConvergenceSweepStateStorageKind::Observed,
        ))
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(ConvergenceSweepStoreError::Corruption(
                "dispatch does not belong to the convergence target",
            ));
        }
        insert_event(
            &mut transaction,
            event_id,
            repository,
            pull_request,
            convergence_sweep_outcome_to_str(ConvergenceSweepOutcomeStorageKind::Dispatched),
            None,
            Some(observation),
            Some((dispatch_id, session_id)),
            0,
            None,
        )
        .await?;
        match transaction.commit().await {
            Ok(()) => Ok(()),
            Err(error) if crate::commit_failure_is_ambiguous(&error) => {
                let event_exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                        SELECT 1 FROM convergence_sweep_event WHERE event_id = $1
                    )",
                )
                .bind(event_id)
                .fetch_one(&self.pool)
                .await;
                resolve_ambiguous_event_commit(error, event_exists)
            }
            Err(error) => Err(ConvergenceSweepStoreError::Database(error)),
        }
    }

    /// Records a non-failing census decision and resets a transient lineage.
    pub async fn record_decision(
        &self,
        event_id: Uuid,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        observation: &ConvergenceSweepObservation,
        decision: ConvergenceSweepDecision,
    ) -> Result<(), ConvergenceSweepStoreError> {
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        sqlx::query(
            "UPDATE convergence_sweep_target
                SET state_kind = $5, failure_kind = NULL,
                    consecutive_failures = 0, retry_not_before = NULL,
                    parked_at = NULL, operator_need = NULL,
                    parked_dispatch_id = NULL, parked_session_id = NULL,
                    parked_dispatched_at = NULL,
                    last_head_sha = $3, last_unresolved_threads = $4,
                    last_observed_at = clock_timestamp()
              WHERE repository = $1 AND pull_request_number = $2",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(observation.head_sha().as_str())
        .bind(Decimal::from(observation.unresolved_threads()))
        .bind(convergence_sweep_state_to_str(
            ConvergenceSweepStateStorageKind::Observed,
        ))
        .execute(&mut *transaction)
        .await?;
        insert_event(
            &mut transaction,
            event_id,
            repository,
            pull_request,
            convergence_sweep_outcome_to_str(convergence_sweep_decision_outcome(decision)),
            None,
            Some(observation),
            None,
            0,
            None,
        )
        .await?;
        match transaction.commit().await {
            Ok(()) => Ok(()),
            Err(error) if crate::commit_failure_is_ambiguous(&error) => {
                let event_exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                        SELECT 1 FROM convergence_sweep_event WHERE event_id = $1
                    )",
                )
                .bind(event_id)
                .fetch_one(&self.pool)
                .await;
                resolve_ambiguous_event_commit(error, event_exists)
            }
            Err(error) => Err(ConvergenceSweepStoreError::Database(error)),
        }
    }

    /// Records a decision while associating its observation with one selected dispatch.
    pub async fn record_dispatch_decision(
        &self,
        event_id: Uuid,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        observation: &ConvergenceSweepObservation,
        dispatch: (Uuid, SessionId),
        decision: ConvergenceSweepDecision,
    ) -> Result<(), ConvergenceSweepStoreError> {
        let (dispatch_id, session_id) = dispatch;
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        let updated = sqlx::query(
            "UPDATE convergence_sweep_target
                SET state_kind = $7, failure_kind = NULL,
                    consecutive_failures = 0, retry_not_before = NULL,
                    parked_at = NULL, operator_need = NULL,
                    parked_dispatch_id = NULL, parked_session_id = NULL,
                    parked_dispatched_at = NULL,
                    last_head_sha = $3, last_unresolved_threads = $4,
                    last_observed_at = clock_timestamp(),
                    census_dispatch_id = $5, census_session_id = $6,
                    census_dispatch_head_sha = $3,
                    census_dispatch_unresolved_threads = $4
              WHERE repository = $1 AND pull_request_number = $2
                AND (
                    EXISTS (
                        SELECT 1 FROM commissioned_dispatch AS dispatch
                         WHERE dispatch.dispatch_id = $5
                           AND dispatch.session_id = $6
                           AND dispatch.target_kind = 'pull_request'
                           AND dispatch.repository = $1
                           AND dispatch.pull_request_number = $2
                    )
                    OR EXISTS (
                        SELECT 1
                          FROM repo_watch_dispatch_action AS action
                          JOIN repo_watch_event AS event ON event.event_id = action.event_id
                         WHERE action.dispatch_id = $5
                           AND action.session_id = $6
                           AND event.target_kind = 'pull_request'
                           AND event.repository = $1
                           AND event.pull_request_number = $2
                    )
                )",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(observation.head_sha().as_str())
        .bind(Decimal::from(observation.unresolved_threads()))
        .bind(dispatch_id)
        .bind(session_id.into_uuid())
        .bind(convergence_sweep_state_to_str(
            ConvergenceSweepStateStorageKind::Observed,
        ))
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(ConvergenceSweepStoreError::Corruption(
                "dispatch baseline does not belong to the convergence target",
            ));
        }
        insert_event(
            &mut transaction,
            event_id,
            repository,
            pull_request,
            convergence_sweep_outcome_to_str(convergence_sweep_decision_outcome(decision)),
            None,
            Some(observation),
            None,
            0,
            None,
        )
        .await?;
        match transaction.commit().await {
            Ok(()) => Ok(()),
            Err(error) if crate::commit_failure_is_ambiguous(&error) => {
                let event_exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                        SELECT 1 FROM convergence_sweep_event WHERE event_id = $1
                    )",
                )
                .bind(event_id)
                .fetch_one(&self.pool)
                .await;
                resolve_ambiguous_event_commit(error, event_exists)
            }
            Err(error) => Err(ConvergenceSweepStoreError::Database(error)),
        }
    }

    /// Advances one typed failure lineage, scheduling retry or parking atomically.
    pub async fn record_failure(
        &self,
        event_id: Uuid,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        observation: Option<&ConvergenceSweepObservation>,
        failure: ConvergenceSweepFailureKind,
        retry_policy: ConvergenceSweepRetryPolicy,
    ) -> Result<ConvergenceSweepFailureDisposition, ConvergenceSweepStoreError> {
        if failure == ConvergenceSweepFailureKind::NoModelActivity {
            return Err(ConvergenceSweepStoreError::Corruption(
                "no-model-activity failure requires an expected session",
            ));
        }
        self.record_failure_guarded(
            event_id,
            repository,
            pull_request,
            FailureRecord {
                observation,
                failure,
                retry_policy,
            },
            None,
        )
        .await
    }

    /// Parks an inactive session only after rechecking durable activity and liveness.
    pub async fn record_no_model_activity_failure(
        &self,
        event_id: Uuid,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        observation: &ConvergenceSweepObservation,
        expected_session: SessionId,
    ) -> Result<ConvergenceSweepFailureDisposition, ConvergenceSweepStoreError> {
        self.record_failure_guarded(
            event_id,
            repository,
            pull_request,
            FailureRecord {
                observation: Some(observation),
                failure: ConvergenceSweepFailureKind::NoModelActivity,
                retry_policy: ConvergenceSweepRetryPolicy {
                    backoff_base: Duration::ZERO,
                    backoff_cap: Duration::ZERO,
                },
            },
            Some(expected_session),
        )
        .await
    }

    async fn record_failure_guarded(
        &self,
        event_id: Uuid,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        record: FailureRecord<'_>,
        expected_inactive_session: Option<SessionId>,
    ) -> Result<ConvergenceSweepFailureDisposition, ConvergenceSweepStoreError> {
        let FailureRecord {
            observation,
            failure,
            retry_policy,
        } = record;
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        if expected_inactive_session.is_some() {
            crate::commissioned_dispatch::lock_pull_request_target(
                &mut transaction,
                repository.as_str(),
                &Decimal::from(pull_request.get()),
            )
            .await?;
            let cohort_sessions: Vec<Uuid> = sqlx::query_scalar(
                "WITH target_dispatch AS (
                    SELECT dispatch.dispatch_id, dispatch.session_id,
                           dispatch.recorded_at
                      FROM commissioned_dispatch AS dispatch
                     WHERE dispatch.target_kind = 'pull_request'
                       AND dispatch.repository = $1
                       AND dispatch.pull_request_number = $2
                    UNION ALL
                    SELECT action.dispatch_id, action.session_id,
                           batch.admitted_at AS recorded_at
                      FROM repo_watch_dispatch_action AS action
                      JOIN repo_watch_event AS event
                        ON event.event_id = action.event_id
                      JOIN repo_watch_dispatch_batch AS batch
                        ON batch.dispatch_id = action.dispatch_id
                     WHERE event.target_kind = 'pull_request'
                       AND event.repository = $1
                       AND event.pull_request_number = $2
                ), latest_dispatch AS (
                    SELECT dispatch_id, recorded_at
                      FROM target_dispatch
                     ORDER BY recorded_at DESC, dispatch_id DESC
                     LIMIT 1
                )
                SELECT target.session_id
                  FROM target_dispatch AS target
                  JOIN latest_dispatch AS latest
                    ON latest.dispatch_id = target.dispatch_id
                   AND latest.recorded_at = target.recorded_at
                 ORDER BY target.session_id",
            )
            .bind(repository.as_str())
            .bind(Decimal::from(pull_request.get()))
            .fetch_all(&mut *transaction)
            .await?;
            for cohort_session in cohort_sessions {
                lock_model_activity_fence(&mut transaction, SessionId::from_uuid(cohort_session))
                    .await?;
            }
        }
        let budget: i16 = sqlx::query_scalar("SELECT convergence_sweep_retry_budget()")
            .fetch_one(&mut *transaction)
            .await?;
        let (head, threads) = observation
            .map(|value| {
                (
                    Some(value.head_sha().as_str()),
                    Some(Decimal::from(value.unresolved_threads())),
                )
            })
            .unwrap_or((None, None));
        let updated: Option<FailureTransitionRow> = sqlx::query_as(
            "WITH target_dispatch AS (
                SELECT target.dispatch_id, target.session_id, target.recorded_at,
                       coalesce((
                           SELECT event.event_kind IN ('commissioned', 'resumed', 'superseded')
                             FROM goal_event AS event
                            WHERE event.session_id = target.session_id
                            ORDER BY event.event_ordinal DESC LIMIT 1
                       ), false) AS live,
                       EXISTS (
                           SELECT 1 FROM model_call AS call
                            WHERE call.session_id = target.session_id
                       ) AS has_model_activity
                  FROM (
                       SELECT dispatch.session_id, dispatch.recorded_at, dispatch.dispatch_id
                         FROM commissioned_dispatch AS dispatch
                        WHERE dispatch.target_kind = 'pull_request'
                          AND dispatch.repository = $1
                          AND dispatch.pull_request_number = $2
                       UNION ALL
                       SELECT action.session_id, batch.admitted_at AS recorded_at,
                              action.dispatch_id
                         FROM repo_watch_dispatch_action AS action
                         JOIN repo_watch_event AS event ON event.event_id = action.event_id
                         JOIN repo_watch_dispatch_batch AS batch
                           ON batch.dispatch_id = action.dispatch_id
                        WHERE event.target_kind = 'pull_request'
                          AND event.repository = $1
                          AND event.pull_request_number = $2
                  ) AS target
             ), latest_dispatch AS (
                SELECT dispatch_id, recorded_at
                  FROM target_dispatch
                 ORDER BY recorded_at DESC, dispatch_id DESC
                 LIMIT 1
             ), expected_dispatch AS (
                SELECT candidate.dispatch_id, candidate.session_id,
                       candidate.recorded_at
                  FROM target_dispatch AS candidate
                  JOIN latest_dispatch AS latest
                    ON latest.dispatch_id = candidate.dispatch_id
                   AND latest.recorded_at = candidate.recorded_at
                 WHERE candidate.session_id = $11
                   AND NOT candidate.has_model_activity
                 ORDER BY candidate.session_id DESC
                 LIMIT 1
             )
             UPDATE convergence_sweep_target
                SET state_kind = CASE WHEN
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN $12 ELSE $13 END,
                    failure_kind = $3,
                    consecutive_failures = CASE WHEN $4 THEN $5
                        WHEN failure_kind = $3
                            THEN least(consecutive_failures + 1, $5)
                        ELSE 1::smallint END,
                    retry_not_before = CASE WHEN
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN NULL
                        ELSE clock_timestamp() + least(
                            $6::bigint * (1::bigint << greatest(
                                (CASE WHEN failure_kind = $3
                                    THEN least(consecutive_failures + 1, $5)
                                    ELSE 1::smallint END) - 1,
                                0
                            )),
                            $7::bigint
                        ) * interval '1 second' END,
                    parked_at = CASE WHEN
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN clock_timestamp() ELSE NULL END,
                    operator_need = CASE WHEN
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN $8 ELSE NULL END,
                    parked_dispatch_id = CASE WHEN $4 AND
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN (SELECT dispatch_id FROM expected_dispatch)
                        ELSE NULL END,
                    parked_session_id = CASE WHEN $4 AND
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN (SELECT session_id FROM expected_dispatch)
                        ELSE NULL END,
                    parked_dispatched_at = CASE WHEN $4 AND
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN (SELECT recorded_at FROM expected_dispatch)
                        ELSE NULL END,
                    last_head_sha = coalesce($9, last_head_sha),
                    last_unresolved_threads = coalesce($10, last_unresolved_threads),
                    last_observed_at = CASE WHEN $9 IS NULL THEN last_observed_at
                        ELSE clock_timestamp() END
              WHERE repository = $1 AND pull_request_number = $2
                AND ($11::uuid IS NULL OR (
                    EXISTS (SELECT 1 FROM expected_dispatch)
                    AND NOT EXISTS (
                        SELECT 1
                          FROM target_dispatch AS competitor
                         WHERE competitor.session_id <> $11
                           AND (
                               competitor.live
                               OR (
                                   competitor.has_model_activity
                                   AND competitor.dispatch_id = (
                                       SELECT dispatch_id FROM latest_dispatch
                                   )
                               )
                           )
                    )
                ))
          RETURNING consecutive_failures AS consecutive_failures,
                    CASE state_kind
                        WHEN $12 THEN 'parked'
                        ELSE 'retry_scheduled'
                    END AS parking_kind",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(convergence_sweep_failure_to_str(failure))
        .bind(failure == ConvergenceSweepFailureKind::NoModelActivity)
        .bind(budget)
        .bind(retry_policy.stored_backoff_base_seconds())
        .bind(retry_policy.stored_backoff_cap_seconds())
        .bind(convergence_sweep_operator_need_to_str(failure))
        .bind(head)
        .bind(threads)
        .bind(expected_inactive_session.map(SessionId::into_uuid))
        .bind(convergence_sweep_state_to_str(
            ConvergenceSweepStateStorageKind::Parked,
        ))
        .bind(convergence_sweep_state_to_str(
            ConvergenceSweepStateStorageKind::RetryWait,
        ))
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(updated) = updated else {
            transaction.commit().await?;
            return if expected_inactive_session.is_some() {
                Ok(ConvergenceSweepFailureDisposition::ActivityObserved)
            } else {
                Err(ConvergenceSweepStoreError::Corruption(
                    "target disappeared during failure recording",
                ))
            };
        };
        let parking = FailureParking::decode(&updated.parking_kind)?;
        insert_event(
            &mut transaction,
            event_id,
            repository,
            pull_request,
            convergence_sweep_outcome_to_str(convergence_sweep_failure_outcome(failure)),
            Some(failure),
            observation,
            None,
            updated.consecutive_failures,
            (parking == FailureParking::Parked)
                .then_some(convergence_sweep_operator_need_to_str(failure)),
        )
        .await?;
        let disposition = if parking == FailureParking::Parked {
            ConvergenceSweepFailureDisposition::Parked
        } else {
            ConvergenceSweepFailureDisposition::RetryScheduled
        };
        match transaction.commit().await {
            Ok(()) => Ok(disposition),
            Err(error) if crate::commit_failure_is_ambiguous(&error) => {
                let event_exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                        SELECT 1 FROM convergence_sweep_event WHERE event_id = $1
                    )",
                )
                .bind(event_id)
                .fetch_one(&self.pool)
                .await;
                resolve_ambiguous_event_commit(error, event_exists).map(|()| disposition)
            }
            Err(error) => Err(ConvergenceSweepStoreError::Database(error)),
        }
    }
}

/// Serializes first model-call creation with inactivity parking for one session.
pub(crate) async fn lock_model_activity_fence(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "convergence_model_activity:{}",
            session.into_uuid()
        ))
        .execute(connection)
        .await?;
    Ok(())
}

async fn ensure_target(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    repository: &RepositorySlug,
    pull_request: PullRequestNumber,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO convergence_sweep_target (repository, pull_request_number)
         VALUES ($1, $2)
         ON CONFLICT (repository, pull_request_number)
         DO UPDATE SET enrolled = true",
    )
    .bind(repository.as_str())
    .bind(Decimal::from(pull_request.get()))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the append-only event has one labeled field per durable fact"
)]
async fn insert_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_id: Uuid,
    repository: &RepositorySlug,
    pull_request: PullRequestNumber,
    outcome: &str,
    failure: Option<ConvergenceSweepFailureKind>,
    observation: Option<&ConvergenceSweepObservation>,
    dispatch: Option<(Uuid, SessionId)>,
    failures: i16,
    need: Option<&str>,
) -> Result<(), sqlx::Error> {
    let (head, threads) = observation
        .map(|value| {
            (
                Some(value.head_sha().as_str()),
                Some(Decimal::from(value.unresolved_threads())),
            )
        })
        .unwrap_or((None, None));
    let (dispatch_id, session_id) = dispatch
        .map(|(dispatch, session)| (Some(dispatch), Some(session.into_uuid())))
        .unwrap_or((None, None));
    sqlx::query(
        "INSERT INTO convergence_sweep_event
            (event_id, repository, pull_request_number, outcome_kind,
             failure_kind, head_sha, unresolved_threads, dispatch_id, session_id,
             consecutive_failures, retry_not_before, operator_need)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
            CASE WHEN $10 > 0 AND $11 IS NULL
                 THEN (SELECT retry_not_before FROM convergence_sweep_target
                        WHERE repository = $2 AND pull_request_number = $3)
                 ELSE NULL END, $11)",
    )
    .bind(event_id)
    .bind(repository.as_str())
    .bind(Decimal::from(pull_request.get()))
    .bind(outcome)
    .bind(failure.map(convergence_sweep_failure_to_str))
    .bind(head)
    .bind(threads)
    .bind(dispatch_id)
    .bind(session_id)
    .bind(failures)
    .bind(need)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn decode_target_state(
    row: TargetStateRow,
) -> Result<ConvergenceSweepTargetState, ConvergenceSweepStoreError> {
    let state = convergence_sweep_state_from_str(&row.state_kind).ok_or(
        ConvergenceSweepStoreError::Corruption("invalid sweep state kind"),
    )?;
    let failure_kind = row
        .failure_kind
        .map(|value| {
            convergence_sweep_failure_from_str(&value).ok_or(
                ConvergenceSweepStoreError::Corruption("invalid failure kind"),
            )
        })
        .transpose()?;
    let pending_observation =
        decode_observation(row.pending_head_sha, row.pending_unresolved_threads)?;
    let last_observation = decode_observation(row.last_head_sha, row.last_unresolved_threads)?;
    let latest_dispatch_observation = decode_observation(
        row.latest_dispatch_head_sha,
        row.latest_dispatch_unresolved_threads,
    )?;
    let pending_dispatch = decode_dispatch_state(
        row.pending_dispatch_id,
        row.pending_session_id,
        row.pending_recorded_at,
        row.pending_liveness_kind,
        row.pending_activity_kind,
        "partial pending dispatch",
    )?;
    let latest_dispatch = decode_dispatch_state(
        row.dispatch_id,
        row.session_id,
        row.recorded_at,
        row.liveness_kind,
        row.activity_kind,
        "partial latest dispatch",
    )?;
    Ok(ConvergenceSweepTargetState {
        parked: state == ConvergenceSweepStateStorageKind::Parked,
        retry_ready: Readiness::decode(&row.retry_readiness)?.is_ready(),
        cool_off_elapsed: Readiness::decode(&row.cool_off_readiness)?.is_ready(),
        failure_kind,
        consecutive_failures: u16::try_from(row.consecutive_failures)
            .map_err(|_| ConvergenceSweepStoreError::Corruption("invalid failure count"))?,
        pending_command: row.pending_command_id.map(DurableCommandId::from_uuid),
        pending_observation,
        last_observation,
        latest_dispatch_observation,
        pending_dispatch,
        latest_dispatch,
    })
}

fn decode_dispatch_state(
    dispatch_id: Option<Uuid>,
    session_id: Option<Uuid>,
    recorded_at: Option<OffsetDateTime>,
    liveness: Option<String>,
    activity: Option<String>,
    partial: &'static str,
) -> Result<Option<ConvergenceSweepDispatchState>, ConvergenceSweepStoreError> {
    match (dispatch_id, session_id, recorded_at, liveness, activity) {
        (
            Some(dispatch_id),
            Some(session_id),
            Some(recorded_at),
            Some(liveness),
            Some(activity),
        ) => Ok(Some(ConvergenceSweepDispatchState {
            dispatch_id,
            session_id: session_id_from_uuid(session_id),
            dispatched_at: SystemTime::from(recorded_at),
            live: DispatchLiveness::decode(&liveness)?.is_live(),
            has_model_activity: ModelActivity::decode(&activity)?.is_present(),
        })),
        (None, None, None, None, None) => Ok(None),
        _ => Err(ConvergenceSweepStoreError::Corruption(partial)),
    }
}

fn decode_observation(
    head: Option<String>,
    threads: Option<Decimal>,
) -> Result<Option<ConvergenceSweepObservation>, ConvergenceSweepStoreError> {
    match (head, threads.and_then(|value| value.to_u64())) {
        (Some(head), Some(threads)) => Ok(Some(ConvergenceSweepObservation::new(
            CommitSha::try_new(head)
                .map_err(|_| ConvergenceSweepStoreError::Corruption("invalid head SHA"))?,
            threads,
        ))),
        (None, None) => Ok(None),
        _ => Err(ConvergenceSweepStoreError::Corruption(
            "partial observation",
        )),
    }
}

fn resolve_ambiguous_event_commit(
    commit_error: sqlx::Error,
    event_exists: Result<bool, sqlx::Error>,
) -> Result<(), ConvergenceSweepStoreError> {
    match event_exists {
        Ok(true) => Ok(()),
        Ok(false) => Err(ConvergenceSweepStoreError::Database(commit_error)),
        Err(_) => Err(ConvergenceSweepStoreError::CommitAmbiguous(commit_error)),
    }
}

fn resolve_ambiguous_fence_commit(
    commit_error: sqlx::Error,
    fence_landed: Result<bool, sqlx::Error>,
) -> Result<(), ConvergenceSweepStoreError> {
    match fence_landed {
        Ok(true) => Ok(()),
        Ok(false) => Err(ConvergenceSweepStoreError::Database(commit_error)),
        Err(_) => Err(ConvergenceSweepStoreError::CommitAmbiguous(commit_error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConvergenceSweepStoreError, resolve_ambiguous_event_commit, resolve_ambiguous_fence_commit,
    };

    #[test]
    fn ambiguous_decision_commit_is_resolved_by_event_identity() {
        assert!(resolve_ambiguous_event_commit(sqlx::Error::PoolClosed, Ok(true)).is_ok());
        assert!(matches!(
            resolve_ambiguous_event_commit(sqlx::Error::PoolClosed, Ok(false)),
            Err(ConvergenceSweepStoreError::Database(_))
        ));
        assert!(matches!(
            resolve_ambiguous_event_commit(sqlx::Error::PoolClosed, Err(sqlx::Error::PoolClosed),),
            Err(ConvergenceSweepStoreError::CommitAmbiguous(_))
        ));
    }

    #[test]
    fn ambiguous_commission_fence_commit_is_resolved_by_exact_intent() {
        assert!(resolve_ambiguous_fence_commit(sqlx::Error::PoolClosed, Ok(true)).is_ok());
        assert!(matches!(
            resolve_ambiguous_fence_commit(sqlx::Error::PoolClosed, Ok(false)),
            Err(ConvergenceSweepStoreError::Database(_))
        ));
        assert!(matches!(
            resolve_ambiguous_fence_commit(sqlx::Error::PoolClosed, Err(sqlx::Error::PoolClosed),),
            Err(ConvergenceSweepStoreError::CommitAmbiguous(_))
        ));
    }
}
