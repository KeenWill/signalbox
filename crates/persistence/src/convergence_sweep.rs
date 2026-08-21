//! Durable state, retry, parking, and commissioned-session census for convergence sweeps.

use std::{error::Error, fmt, time::SystemTime};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_domain::{CommitSha, DurableCommandId, PullRequestNumber, RepositorySlug, SessionId};
use sqlx::{
    PgPool, Row,
    types::{Uuid, time::OffsetDateTime},
};

use crate::mapping::session_id_from_uuid;

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

impl ConvergenceSweepFailureKind {
    const fn storage(self) -> &'static str {
        match self {
            Self::FactsFetch => "facts_fetch",
            Self::CommissionRefused => "commission_refused",
            Self::TemplateDrift => "template_drift",
            Self::NoModelActivity => "no_model_activity",
            Self::StateAccess => "state_access",
        }
    }

    const fn outcome(self) -> &'static str {
        match self {
            Self::FactsFetch => "facts_fetch_failed",
            Self::CommissionRefused => "commission_refused",
            Self::TemplateDrift => "template_drift",
            Self::NoModelActivity => "no_model_activity",
            Self::StateAccess => "state_access_failed",
        }
    }

    const fn need(self) -> &'static str {
        match self {
            Self::FactsFetch => "repair_facts_fetch",
            Self::CommissionRefused => "repair_commission",
            Self::TemplateDrift => "repair_template",
            Self::NoModelActivity => "inspect_inactive_session",
            Self::StateAccess => "repair_sweep_state",
        }
    }

    fn from_storage(value: &str) -> Option<Self> {
        match value {
            "facts_fetch" => Some(Self::FactsFetch),
            "commission_refused" => Some(Self::CommissionRefused),
            "template_drift" => Some(Self::TemplateDrift),
            "no_model_activity" => Some(Self::NoModelActivity),
            "state_access" => Some(Self::StateAccess),
            _ => None,
        }
    }
}

/// Non-failure decisions retained in the append-only audit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvergenceSweepDecision {
    Converged,
    CoolingOff,
    LiveSession,
}

impl ConvergenceSweepDecision {
    const fn storage(self) -> &'static str {
        match self {
            Self::Converged => "converged",
            Self::CoolingOff => "cooling_off",
            Self::LiveSession => "live_session",
        }
    }
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
}

/// Database or durable-shape failure in convergence sweep storage.
#[derive(Debug)]
pub enum ConvergenceSweepStoreError {
    Database(sqlx::Error),
    Corruption(&'static str),
}

impl fmt::Display for ConvergenceSweepStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("convergence sweep database operation failed"),
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
            Self::Database(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for ConvergenceSweepStoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
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
                SET state_kind = 'observed', failure_kind = NULL,
                    consecutive_failures = 0, retry_not_before = NULL,
                    parked_at = NULL, operator_need = NULL
              WHERE repository = $1 AND pull_request_number = $2
                AND state_kind = 'parked'",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
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
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
        let row = sqlx::query(
            "SELECT target.state_kind, target.failure_kind,
                    target.consecutive_failures,
                    target.retry_not_before IS NULL
                      OR target.retry_not_before <= clock_timestamp() AS retry_ready,
                    target.pending_command_id, target.pending_head_sha,
                    target.pending_unresolved_threads,
                    target.last_head_sha, target.last_unresolved_threads,
                    CASE
                      WHEN latest.dispatch_id = target.last_dispatch_id
                        THEN target.last_dispatch_head_sha
                      WHEN latest.dispatch_id = pending.dispatch_id
                        THEN target.pending_head_sha
                    END AS latest_dispatch_head_sha,
                    CASE
                      WHEN latest.dispatch_id = target.last_dispatch_id
                        THEN target.last_dispatch_unresolved_threads
                      WHEN latest.dispatch_id = pending.dispatch_id
                        THEN target.pending_unresolved_threads
                    END AS latest_dispatch_unresolved_threads,
                    pending.dispatch_id AS pending_dispatch_id,
                    pending.session_id AS pending_session_id,
                    pending.recorded_at AS pending_recorded_at,
                    pending.live AS pending_live,
                    pending.has_model_activity AS pending_has_model_activity,
                    latest.dispatch_id, latest.session_id, latest.recorded_at,
                    latest.live, latest.has_model_activity
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
                     WHERE dispatch.target_kind = 'pull_request'
                       AND dispatch.repository = target.repository
                       AND dispatch.pull_request_number = target.pull_request_number
                     ORDER BY dispatch.recorded_at DESC, dispatch.dispatch_id DESC
                     LIMIT 1
               ) AS latest ON true
              WHERE target.repository = $1
                AND target.pull_request_number = $2",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
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
        transaction.commit().await?;
        Ok(DurableCommandId::from_uuid(command))
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
        sqlx::query(
            "UPDATE convergence_sweep_target
                SET state_kind = 'observed', failure_kind = NULL,
                    consecutive_failures = 0, retry_not_before = NULL,
                    parked_at = NULL, operator_need = NULL,
                    last_head_sha = $3, last_unresolved_threads = $4,
                    last_observed_at = clock_timestamp(),
                    pending_command_id = NULL, pending_head_sha = NULL,
                    pending_unresolved_threads = NULL, pending_content_digest = NULL,
                    pending_started_at = NULL,
                    last_dispatch_id = $5, last_session_id = $6,
                    last_dispatched_at = clock_timestamp(),
                    last_dispatch_head_sha = $3,
                    last_dispatch_unresolved_threads = $4
              WHERE repository = $1 AND pull_request_number = $2",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(observation.head_sha().as_str())
        .bind(Decimal::from(observation.unresolved_threads()))
        .bind(dispatch_id)
        .bind(session_id.into_uuid())
        .execute(&mut *transaction)
        .await?;
        insert_event(
            &mut transaction,
            event_id,
            repository,
            pull_request,
            "dispatched",
            None,
            Some(observation),
            Some((dispatch_id, session_id)),
            0,
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
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
                SET state_kind = 'observed', failure_kind = NULL,
                    consecutive_failures = 0, retry_not_before = NULL,
                    parked_at = NULL, operator_need = NULL,
                    last_head_sha = $3, last_unresolved_threads = $4,
                    last_observed_at = clock_timestamp()
              WHERE repository = $1 AND pull_request_number = $2",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(observation.head_sha().as_str())
        .bind(Decimal::from(observation.unresolved_threads()))
        .execute(&mut *transaction)
        .await?;
        insert_event(
            &mut transaction,
            event_id,
            repository,
            pull_request,
            decision.storage(),
            None,
            Some(observation),
            None,
            0,
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Advances one typed failure lineage, scheduling retry or parking atomically.
    pub async fn record_failure(
        &self,
        event_id: Uuid,
        repository: &RepositorySlug,
        pull_request: PullRequestNumber,
        observation: Option<&ConvergenceSweepObservation>,
        failure: ConvergenceSweepFailureKind,
        retry_delay_seconds: u64,
    ) -> Result<ConvergenceSweepFailureDisposition, ConvergenceSweepStoreError> {
        let mut transaction = self.pool.begin().await?;
        ensure_target(&mut transaction, repository, pull_request).await?;
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
        let (failures, parked): (i16, bool) = sqlx::query_as(
            "UPDATE convergence_sweep_target
                SET state_kind = CASE WHEN
                        (CASE WHEN $4 THEN $5
                            WHEN failure_kind = $3
                                THEN least(consecutive_failures + 1, $5)
                            ELSE 1::smallint END) >= $5
                        THEN 'parked' ELSE 'retry_wait' END,
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
                        ELSE clock_timestamp() + $6 * interval '1 second' END,
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
                        THEN $7 ELSE NULL END,
                    last_head_sha = coalesce($8, last_head_sha),
                    last_unresolved_threads = coalesce($9, last_unresolved_threads),
                    last_observed_at = CASE WHEN $8 IS NULL THEN last_observed_at
                        ELSE clock_timestamp() END
              WHERE repository = $1 AND pull_request_number = $2
          RETURNING consecutive_failures, state_kind = 'parked'",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(pull_request.get()))
        .bind(failure.storage())
        .bind(failure == ConvergenceSweepFailureKind::NoModelActivity)
        .bind(budget)
        .bind(i64::try_from(retry_delay_seconds).unwrap_or(i64::MAX))
        .bind(failure.need())
        .bind(head)
        .bind(threads)
        .fetch_one(&mut *transaction)
        .await?;
        insert_event(
            &mut transaction,
            event_id,
            repository,
            pull_request,
            failure.outcome(),
            Some(failure),
            observation,
            None,
            failures,
            parked.then_some(failure.need()),
        )
        .await?;
        transaction.commit().await?;
        Ok(if parked {
            ConvergenceSweepFailureDisposition::Parked
        } else {
            ConvergenceSweepFailureDisposition::RetryScheduled
        })
    }
}

async fn ensure_target(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    repository: &RepositorySlug,
    pull_request: PullRequestNumber,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO convergence_sweep_target (repository, pull_request_number)
         VALUES ($1, $2) ON CONFLICT DO NOTHING",
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
    .bind(failure.map(ConvergenceSweepFailureKind::storage))
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
    row: sqlx::postgres::PgRow,
) -> Result<ConvergenceSweepTargetState, ConvergenceSweepStoreError> {
    let state: String = row.try_get("state_kind")?;
    let failure_kind = row
        .try_get::<Option<String>, _>("failure_kind")?
        .map(|value| {
            ConvergenceSweepFailureKind::from_storage(&value).ok_or(
                ConvergenceSweepStoreError::Corruption("invalid failure kind"),
            )
        })
        .transpose()?;
    let pending_command: Option<Uuid> = row.try_get("pending_command_id")?;
    let pending_head: Option<String> = row.try_get("pending_head_sha")?;
    let pending_threads: Option<Decimal> = row.try_get("pending_unresolved_threads")?;
    let last_head: Option<String> = row.try_get("last_head_sha")?;
    let last_threads: Option<Decimal> = row.try_get("last_unresolved_threads")?;
    let dispatch_head: Option<String> = row.try_get("latest_dispatch_head_sha")?;
    let dispatch_threads: Option<Decimal> = row.try_get("latest_dispatch_unresolved_threads")?;
    let pending_observation = decode_observation(pending_head, pending_threads)?;
    let last_observation = decode_observation(last_head, last_threads)?;
    let latest_dispatch_observation = decode_observation(dispatch_head, dispatch_threads)?;
    let pending_dispatch = decode_dispatch_state(
        row.try_get("pending_dispatch_id")?,
        row.try_get("pending_session_id")?,
        row.try_get("pending_recorded_at")?,
        row.try_get("pending_live")?,
        row.try_get("pending_has_model_activity")?,
        "partial pending dispatch",
    )?;
    let latest_dispatch = decode_dispatch_state(
        row.try_get("dispatch_id")?,
        row.try_get("session_id")?,
        row.try_get("recorded_at")?,
        row.try_get("live")?,
        row.try_get("has_model_activity")?,
        "partial latest dispatch",
    )?;
    Ok(ConvergenceSweepTargetState {
        parked: state == "parked",
        retry_ready: row.try_get("retry_ready")?,
        failure_kind,
        consecutive_failures: u16::try_from(row.try_get::<i16, _>("consecutive_failures")?)
            .map_err(|_| ConvergenceSweepStoreError::Corruption("invalid failure count"))?,
        pending_command: pending_command.map(DurableCommandId::from_uuid),
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
    live: Option<bool>,
    activity: Option<bool>,
    partial: &'static str,
) -> Result<Option<ConvergenceSweepDispatchState>, ConvergenceSweepStoreError> {
    match (dispatch_id, session_id, recorded_at, live, activity) {
        (
            Some(dispatch_id),
            Some(session_id),
            Some(recorded_at),
            Some(live),
            Some(has_model_activity),
        ) => Ok(Some(ConvergenceSweepDispatchState {
            dispatch_id,
            session_id: session_id_from_uuid(session_id),
            dispatched_at: SystemTime::from(recorded_at),
            live,
            has_model_activity,
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
