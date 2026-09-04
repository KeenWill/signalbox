//! Durable, singleton-collapsed work owed after repository-watch refusal.

use std::time::Duration;

use rust_decimal::Decimal;
use signalbox_application::RepoWatchRuleEvaluationOutcome;
use signalbox_domain::{
    RepoWatchDispatchId, RepoWatchEvent, RepoWatchEventId, RepoWatchRuleId, RepoWatchRuleVersion,
    RepositorySlug, SessionId,
};
use sqlx::{Postgres, Row, Transaction, types::Uuid};

use crate::mapping::{
    RepoWatchObligationSettlementStorageKind, repo_watch_obligation_settlement_from_str,
    repo_watch_obligation_settlement_to_str, repo_watch_singleton_scope_from_str,
    repo_watch_singleton_scope_to_str,
};
use crate::repo_watch_dispatch::{
    PostgresRepoWatchDispatchStore, RepoWatchDispatchRepositoryError, StoredSingletonKey,
    stored_rule_version,
};

pub(crate) enum DispatchObligationBlocker {
    Existing,
    RepoWatchDispatch(RepoWatchDispatchId),
    ExternalSession(SessionId),
}

impl DispatchObligationBlocker {
    fn replaces_existing(&self) -> bool {
        !matches!(self, Self::Existing)
    }

    fn stored_dispatch(&self) -> Option<Uuid> {
        match self {
            Self::Existing => None,
            Self::RepoWatchDispatch(dispatch) => Some(*dispatch.as_uuid()),
            Self::ExternalSession(_) => None,
        }
    }

    fn stored_external_session(&self) -> Option<Uuid> {
        match self {
            Self::Existing => None,
            Self::RepoWatchDispatch(_) => None,
            Self::ExternalSession(session) => Some(session.into_uuid()),
        }
    }
}

// Delay after the first failed attempt, doubling per further consecutive
// failure. Below the interval at which a watched repository is polled the delay
// would not separate attempts at all; well above it, a lineage recovering from
// one transient failure would stall behind a delay nothing else needs.
const DISPATCH_RETRY_BACKOFF_BASE: Duration = Duration::from_secs(10 * 60);
// The doubling stops here, within three doublings of the base, so the last
// attempts of an exhausting lineage are spaced by this rather than by an
// interval that keeps growing past the point of parking. The attempt budget the
// doubling runs out against is owned by the schema, in
// repo_watch_dispatch_attempt_budget().
const DISPATCH_RETRY_BACKOFF_CAP: Duration = Duration::from_secs(60 * 60);
// Doublings are clamped so the shift the load query applies cannot overflow its
// bigint, whatever attempt count storage holds.
const DISPATCH_RETRY_MAX_DOUBLINGS: i64 = 30;
// Longest operator identifier the park journal accepts, matching its column
// constraint so an over-long value is refused before it reaches the database.
const MAX_PARK_RELEASE_ACTOR_CHARS: usize = 200;

/// Delay one obligation lineage waits out between redispatches.
///
/// Both fields are ceilings: [`Self::lowered_to`] reduces one, nothing raises
/// one, so no caller can widen what production runs under. The attempt budget
/// these delays run out against is not here — the schema owns it, so that
/// parking, the readiness projection, and the dispatch loader cannot disagree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchDispatchRetryPolicy {
    backoff_base: Duration,
    backoff_cap: Duration,
}

impl RepoWatchDispatchRetryPolicy {
    pub const fn production() -> Self {
        Self {
            backoff_base: DISPATCH_RETRY_BACKOFF_BASE,
            backoff_cap: DISPATCH_RETRY_BACKOFF_CAP,
        }
    }

    /// Takes the lower of each field and the argument.
    #[must_use]
    pub fn lowered_to(self, backoff_base: Duration, backoff_cap: Duration) -> Self {
        Self {
            backoff_base: self.backoff_base.min(backoff_base),
            backoff_cap: self.backoff_cap.min(backoff_cap),
        }
    }

    pub const fn backoff_base(&self) -> Duration {
        self.backoff_base
    }

    pub const fn backoff_cap(&self) -> Duration {
        self.backoff_cap
    }

    fn stored_backoff_base_seconds(&self) -> i64 {
        stored_seconds(self.backoff_base)
    }

    fn stored_backoff_cap_seconds(&self) -> i64 {
        stored_seconds(self.backoff_cap)
    }
}

impl Default for RepoWatchDispatchRetryPolicy {
    fn default() -> Self {
        Self::production()
    }
}

fn stored_seconds(value: Duration) -> i64 {
    i64::try_from(value.as_secs()).unwrap_or(i64::MAX)
}

/// Outcome of an operator's request to return a parked obligation to dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchObligationParkRelease {
    Released,
    NotParked,
    ActorRejected,
}

/// One latest-state delivery obligation retained after singleton refusal.
#[derive(Clone, Debug)]
pub struct RepoWatchDispatchObligation {
    id: Uuid,
    first_event_id: RepoWatchEventId,
    latest_event: RepoWatchEvent,
    matched_event_count: u64,
    failed_attempts: u64,
    singleton: StoredSingletonKey,
}

impl RepoWatchDispatchObligation {
    pub const fn id(&self) -> Uuid {
        self.id
    }

    pub const fn first_event_id(&self) -> RepoWatchEventId {
        self.first_event_id
    }

    pub const fn latest_event(&self) -> &RepoWatchEvent {
        &self.latest_event
    }

    pub const fn matched_event_count(&self) -> u64 {
        self.matched_event_count
    }

    /// Consecutive dispatches of this lineage that ended without meeting it.
    pub const fn failed_attempts(&self) -> u64 {
        self.failed_attempts
    }

    pub(crate) fn into_parts(self) -> (Uuid, RepoWatchEvent, StoredSingletonKey) {
        (self.id, self.latest_event, self.singleton)
    }
}

pub(crate) enum ObligationAdmission {
    Pending,
    Superseded,
    Parked,
    Settled(RepoWatchRuleEvaluationOutcome),
}

impl PostgresRepoWatchDispatchStore {
    /// Returns one parked obligation to dispatch on an operator's say-so.
    pub async fn release_parked_dispatch_obligation(
        &self,
        obligation: Uuid,
        actor: &str,
    ) -> Result<RepoWatchObligationParkRelease, RepoWatchDispatchRepositoryError> {
        let actor_chars = actor.chars().count();
        if actor_chars == 0 || actor_chars > MAX_PARK_RELEASE_ACTOR_CHARS {
            return Ok(RepoWatchObligationParkRelease::ActorRejected);
        }
        let released: bool =
            sqlx::query_scalar("SELECT repo_watch_release_parked_dispatch_obligation($1, $2)")
                .bind(obligation)
                .bind(actor)
                .fetch_one(self.pool())
                .await?;
        if released {
            Ok(RepoWatchObligationParkRelease::Released)
        } else {
            Ok(RepoWatchObligationParkRelease::NotParked)
        }
    }

    /// Loads the oldest outstanding obligation whose singleton and cooldown are free.
    pub async fn load_next_dispatch_obligation(
        &self,
        repository: &RepositorySlug,
        rule_id: &RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
        policy: RepoWatchDispatchRetryPolicy,
    ) -> Result<Option<RepoWatchDispatchObligation>, RepoWatchDispatchRepositoryError> {
        let row = sqlx::query(
            "SELECT obligation.obligation_id, obligation.first_event_id,
                    obligation.latest_event_id, obligation.matched_event_count,
                    obligation.failed_attempts,
                    obligation.singleton_scope, obligation.singleton_repository,
                    obligation.singleton_pull_request_number,
                    obligation.singleton_stack_root_pull_request_number
               FROM repo_watch_dispatch_obligation AS obligation
              WHERE obligation.repository = $1
                AND obligation.rule_id = $2
                AND obligation.rule_version = $3
                AND obligation.settled_kind IS NULL
                AND obligation.parked_at IS NULL
                AND obligation.failed_attempts
                     < repo_watch_dispatch_attempt_budget()
                AND NOT coalesce((
                    SELECT event.event_kind IN ('commissioned', 'resumed', 'superseded')
                      FROM goal_event AS event
                     WHERE event.session_id = obligation.external_blocking_session_id
                     ORDER BY event.event_ordinal DESC
                     LIMIT 1
                ), false)
                AND (
                    obligation.last_failed_attempt_at IS NULL
                    OR extract(epoch FROM (
                            clock_timestamp() - obligation.last_failed_attempt_at
                       )) >= LEAST(
                            $4::bigint << LEAST(
                                GREATEST(obligation.failed_attempts - 1, 0),
                                $6::bigint
                            )::integer,
                            $5::bigint
                       )
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_rule_deactivation AS deactivation
                     WHERE deactivation.repository = obligation.repository
                       AND deactivation.rule_id = obligation.rule_id
                       AND deactivation.rule_version = obligation.rule_version
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_batch AS batch
                     WHERE batch.rule_id = obligation.rule_id
                       AND batch.rule_version = obligation.rule_version
                       AND batch.singleton_scope = obligation.singleton_scope
                       AND batch.singleton_repository
                            IS NOT DISTINCT FROM obligation.singleton_repository
                       AND batch.singleton_pull_request_number
                            IS NOT DISTINCT FROM obligation.singleton_pull_request_number
                       AND batch.singleton_stack_root_pull_request_number
                            IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
                       AND NOT EXISTS (
                            SELECT 1
                              FROM repo_watch_dispatch_release AS released
                             WHERE released.dispatch_id = batch.dispatch_id
                       )
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM repo_watch_dispatch_release AS released
                      JOIN repo_watch_dispatch_batch AS batch
                        ON batch.dispatch_id = released.dispatch_id
                     WHERE batch.rule_id = obligation.rule_id
                       AND batch.rule_version = obligation.rule_version
                       AND batch.singleton_scope = obligation.singleton_scope
                       AND batch.singleton_repository
                            IS NOT DISTINCT FROM obligation.singleton_repository
                       AND batch.singleton_pull_request_number
                            IS NOT DISTINCT FROM obligation.singleton_pull_request_number
                       AND batch.singleton_stack_root_pull_request_number
                            IS NOT DISTINCT FROM obligation.singleton_stack_root_pull_request_number
                       AND extract(epoch FROM (clock_timestamp() - released.released_at))
                            < batch.cooldown_seconds
                )
              ORDER BY obligation.owed_since, obligation.obligation_id
              LIMIT 1",
        )
        .bind(repository.as_str())
        .bind(rule_id.as_str())
        .bind(stored_rule_version(rule_version)?)
        .bind(policy.stored_backoff_base_seconds())
        .bind(policy.stored_backoff_cap_seconds())
        .bind(DISPATCH_RETRY_MAX_DOUBLINGS)
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let event_id = RepoWatchEventId::from_uuid(row.try_get("latest_event_id")?);
        let latest_event = crate::repo_watch::PostgresRepoWatchStore::new(self.pool().clone())
            .load_event(repository, event_id)
            .await
            .map_err(RepoWatchDispatchRepositoryError::EventStore)?
            .ok_or(RepoWatchDispatchRepositoryError::Corruption(
                "owed repository-watch event disappeared",
            ))?;
        let matched_event_count: i64 = row.try_get("matched_event_count")?;
        let failed_attempts: i64 = row.try_get("failed_attempts")?;
        Ok(Some(RepoWatchDispatchObligation {
            id: row.try_get("obligation_id")?,
            first_event_id: RepoWatchEventId::from_uuid(row.try_get("first_event_id")?),
            latest_event,
            matched_event_count: u64::try_from(matched_event_count).map_err(|_| {
                RepoWatchDispatchRepositoryError::Corruption(
                    "owed repository-watch match count is invalid",
                )
            })?,
            failed_attempts: u64::try_from(failed_attempts).map_err(|_| {
                RepoWatchDispatchRepositoryError::Corruption(
                    "owed repository-watch attempt count is invalid",
                )
            })?,
            singleton: StoredSingletonKey {
                scope: repo_watch_singleton_scope_from_str(
                    &row.try_get::<String, _>("singleton_scope")?,
                )
                .ok_or(RepoWatchDispatchRepositoryError::Corruption(
                    "repository-watch obligation singleton scope is unsupported",
                ))?,
                repository: row.try_get("singleton_repository")?,
                pull_request: row.try_get::<Option<Decimal>, _>("singleton_pull_request_number")?,
                stack_root_pull_request: row
                    .try_get::<Option<Decimal>, _>("singleton_stack_root_pull_request_number")?,
            },
        }))
    }
}

pub(crate) async fn active_obligation_exists(
    transaction: &mut Transaction<'_, Postgres>,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    singleton: &StoredSingletonKey,
) -> Result<bool, RepoWatchDispatchRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM repo_watch_dispatch_obligation AS obligation
             WHERE obligation.rule_id = $1
               AND obligation.rule_version = $2
               AND obligation.singleton_scope = $3
               AND obligation.singleton_repository IS NOT DISTINCT FROM $4
               AND obligation.singleton_pull_request_number IS NOT DISTINCT FROM $5
               AND obligation.singleton_stack_root_pull_request_number IS NOT DISTINCT FROM $6
               AND obligation.settled_kind IS NULL
        )",
    )
    .bind(rule_id.as_str())
    .bind(stored_rule_version(rule_version)?)
    .bind(repo_watch_singleton_scope_to_str(singleton.scope))
    .bind(singleton.repository.as_deref())
    .bind(singleton.pull_request)
    .bind(singleton.stack_root_pull_request)
    .fetch_one(&mut **transaction)
    .await?)
}

pub(crate) async fn record_dispatch_obligation(
    transaction: &mut Transaction<'_, Postgres>,
    obligation: RepoWatchDispatchId,
    blocker: DispatchObligationBlocker,
    event: &RepoWatchEvent,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    singleton: &StoredSingletonKey,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    let active = sqlx::query(
        "SELECT obligation.obligation_id
           FROM repo_watch_dispatch_obligation AS obligation
          WHERE obligation.rule_id = $1
            AND obligation.rule_version = $2
            AND obligation.singleton_scope = $3
            AND obligation.singleton_repository IS NOT DISTINCT FROM $4
            AND obligation.singleton_pull_request_number IS NOT DISTINCT FROM $5
            AND obligation.singleton_stack_root_pull_request_number
                 IS NOT DISTINCT FROM $6
            AND obligation.settled_kind IS NULL",
    )
    .bind(rule_id.as_str())
    .bind(stored_rule_version(rule_version)?)
    .bind(repo_watch_singleton_scope_to_str(singleton.scope))
    .bind(singleton.repository.as_deref())
    .bind(singleton.pull_request)
    .bind(singleton.stack_root_pull_request)
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(active) = active {
        let obligation_id: Uuid = active.try_get("obligation_id")?;
        if blocker.replaces_existing() {
            replace_dispatch_obligation_blocker(transaction, obligation_id, blocker).await?;
        }
        let updated = sqlx::query(
            "UPDATE repo_watch_dispatch_obligation
                SET repository = $1,
                    latest_event_id = $2,
                    matched_event_count = matched_event_count + 1,
                    latest_match_at = clock_timestamp()
              WHERE obligation_id = $3
                AND settled_kind IS NULL",
        )
        .bind(event.repository().as_str())
        .bind(event.id().as_uuid())
        .bind(obligation_id)
        .execute(&mut **transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(RepoWatchDispatchRepositoryError::Corruption(
                "repository-watch obligation disappeared while recording its match",
            ));
        }
        return Ok(());
    }
    if matches!(blocker, DispatchObligationBlocker::Existing) {
        return Err(RepoWatchDispatchRepositoryError::Corruption(
            "new repository-watch obligation lacks its blocker",
        ));
    }
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_obligation
            (obligation_id, repository, rule_id, rule_version,
             singleton_scope, singleton_repository, singleton_pull_request_number,
             singleton_stack_root_pull_request_number, first_repository,
             first_event_id, latest_event_id, matched_event_count, blocking_dispatch_id,
             external_blocking_session_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $2, $9, $9, 1, $10, $11)",
    )
    .bind(obligation.as_uuid())
    .bind(event.repository().as_str())
    .bind(rule_id.as_str())
    .bind(stored_rule_version(rule_version)?)
    .bind(repo_watch_singleton_scope_to_str(singleton.scope))
    .bind(singleton.repository.as_deref())
    .bind(singleton.pull_request)
    .bind(singleton.stack_root_pull_request)
    .bind(event.id().as_uuid())
    .bind(blocker.stored_dispatch())
    .bind(blocker.stored_external_session())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn replace_dispatch_obligation_blocker(
    transaction: &mut Transaction<'_, Postgres>,
    obligation: Uuid,
    blocker: DispatchObligationBlocker,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    if matches!(blocker, DispatchObligationBlocker::Existing) {
        return Err(RepoWatchDispatchRepositoryError::Corruption(
            "repository-watch obligation blocker replacement lacks a blocker",
        ));
    }
    sqlx::query(crate::lock_inventory::REPO_WATCH_DISPATCH_OBLIGATION_IDENTITY)
        .bind(obligation)
        .execute(&mut **transaction)
        .await?;
    // A deferred module-park projection touches both the old and replacement
    // subjects. Acquire that complete lifecycle set first, in canonical order,
    // so a concurrent lifecycle transition cannot meet this write from the
    // opposite side of the obligation row.
    sqlx::query(crate::lock_inventory::REPO_WATCH_OBLIGATION_BLOCKER_SUBJECTS)
        .bind(obligation)
        .bind(blocker.stored_external_session())
        .bind(blocker.stored_dispatch())
        .execute(&mut **transaction)
        .await?;
    let updated = sqlx::query(
        "UPDATE repo_watch_dispatch_obligation
            SET blocking_dispatch_id = $2,
                external_blocking_session_id = $3
          WHERE obligation_id = $1
            AND settled_kind IS NULL",
    )
    .bind(obligation)
    .bind(blocker.stored_dispatch())
    .bind(blocker.stored_external_session())
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(RepoWatchDispatchRepositoryError::Corruption(
            "repository-watch obligation disappeared during blocker replacement",
        ));
    }
    Ok(())
}

pub(crate) async fn load_obligation_admission(
    transaction: &mut Transaction<'_, Postgres>,
    obligation: Uuid,
    event: RepoWatchEventId,
) -> Result<ObligationAdmission, RepoWatchDispatchRepositoryError> {
    let row = sqlx::query(crate::lock_inventory::REPO_WATCH_DISPATCH_OBLIGATION)
        .bind(obligation)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RepoWatchDispatchRepositoryError::Corruption(
            "repository-watch obligation disappeared",
        ))?;
    let latest_event: Uuid = row.try_get("latest_event_id")?;
    if latest_event != *event.as_uuid() {
        return Ok(ObligationAdmission::Superseded);
    }
    // Settlement before parking. A settled obligation keeps its parking stamp
    // as the record of why it stopped being dispatched, so reading the stamp
    // first would answer "not now" for work that is already finished and would
    // lose the outcome a settled obligation carries, including the sessions a
    // dispatched one replays.
    let settled_kind: Option<String> = row.try_get("settled_kind")?;
    let Some(settled_kind) = settled_kind else {
        // The handle admission holds was read before this transaction opened,
        // so the parked state is rechecked here rather than trusted from the
        // load.
        if row.try_get::<bool, _>("parked")? {
            return Ok(ObligationAdmission::Parked);
        }
        return Ok(ObligationAdmission::Pending);
    };
    match repo_watch_obligation_settlement_from_str(&settled_kind) {
        Some(RepoWatchObligationSettlementStorageKind::Deactivated) => Ok(
            ObligationAdmission::Settled(RepoWatchRuleEvaluationOutcome::Inactive),
        ),
        Some(RepoWatchObligationSettlementStorageKind::TargetClosed) => Ok(
            ObligationAdmission::Settled(RepoWatchRuleEvaluationOutcome::TargetClosed),
        ),
        Some(RepoWatchObligationSettlementStorageKind::TargetConverged) => Ok(
            ObligationAdmission::Settled(RepoWatchRuleEvaluationOutcome::TargetConverged),
        ),
        Some(RepoWatchObligationSettlementStorageKind::Dispatched) => {
            let dispatch_id: Uuid = row.try_get("settled_dispatch_id")?;
            let sessions = sqlx::query_scalar(
                "SELECT session_id
                   FROM repo_watch_dispatch_action
                  WHERE dispatch_id = $1
                  ORDER BY action_ordinal",
            )
            .bind(dispatch_id)
            .fetch_all(&mut **transaction)
            .await?
            .into_iter()
            .map(SessionId::from_uuid)
            .collect::<Vec<_>>();
            if sessions.is_empty() {
                return Err(RepoWatchDispatchRepositoryError::Corruption(
                    "settled repository-watch obligation has no dispatch actions",
                ));
            }
            Ok(ObligationAdmission::Settled(
                RepoWatchRuleEvaluationOutcome::Replayed {
                    dispatch_id: RepoWatchDispatchId::from_uuid(dispatch_id),
                    sessions: sessions.into_boxed_slice(),
                },
            ))
        }
        None => Err(RepoWatchDispatchRepositoryError::Corruption(
            "repository-watch obligation settlement is unsupported",
        )),
    }
}

pub(crate) async fn settle_target_closed_obligation(
    transaction: &mut Transaction<'_, Postgres>,
    obligation: Uuid,
    event: RepoWatchEventId,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    let affected = sqlx::query(
        "UPDATE repo_watch_dispatch_obligation
            SET settled_kind = $3,
                settled_at = clock_timestamp()
          WHERE obligation_id = $1
            AND latest_event_id = $2
            AND settled_kind IS NULL",
    )
    .bind(obligation)
    .bind(event.as_uuid())
    .bind(repo_watch_obligation_settlement_to_str(
        RepoWatchObligationSettlementStorageKind::TargetClosed,
    ))
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RepoWatchDispatchRepositoryError::Corruption(
            "closed-target obligation settlement lost its active row",
        ));
    }
    Ok(())
}

pub(crate) async fn settle_target_converged_obligation(
    transaction: &mut Transaction<'_, Postgres>,
    obligation: Uuid,
    event: RepoWatchEventId,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    let affected = sqlx::query(
        "UPDATE repo_watch_dispatch_obligation
            SET settled_kind = $3,
                settled_at = clock_timestamp()
          WHERE obligation_id = $1
            AND latest_event_id = $2
            AND settled_kind IS NULL",
    )
    .bind(obligation)
    .bind(event.as_uuid())
    .bind(repo_watch_obligation_settlement_to_str(
        RepoWatchObligationSettlementStorageKind::TargetConverged,
    ))
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RepoWatchDispatchRepositoryError::Corruption(
            "converged-target obligation settlement lost its active row",
        ));
    }
    Ok(())
}

pub(crate) async fn settle_terminal_target_obligations(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    pull_request: Decimal,
    cutoff_event: RepoWatchEventId,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    // Taken in obligation order before the settlement writes, because the
    // progress-release scan takes the same rows in the same order and runs
    // outside the repository key this holds. An unordered multi-row update
    // could meet that scan from the other end.
    sqlx::query(crate::lock_inventory::REPO_WATCH_TERMINAL_TARGET_OBLIGATIONS)
        .bind(repository.as_str())
        .bind(pull_request)
        .bind(cutoff_event.as_uuid())
        .fetch_all(&mut **transaction)
        .await?;
    sqlx::query(
        "UPDATE repo_watch_dispatch_obligation AS obligation
            SET settled_kind = $3,
                settled_at = clock_timestamp()
          WHERE obligation.settled_kind IS NULL
            -- An obligation stalled on this cutoff owes the close automation
            -- and an operator release is what lets it run, so it survives the
            -- cutoff whatever else has matched since. Checked outside the
            -- disjunction below: a later match on the same pull request
            -- advances the latest-event projection, and the arm reading that
            -- projection would otherwise settle it anyway.
            AND obligation.parked_state_event_id IS DISTINCT FROM $4
            AND (
                EXISTS (
                    SELECT 1
                      FROM repo_watch_event AS event
                     WHERE event.event_id = obligation.latest_event_id
                       AND event.repository = $1
                       AND event.pull_request_number = $2
                       AND event.event_id <> $4
                )
                -- A parked lineage keeps the target it stalled on while its
                -- latest-event projection follows whatever matched since, which
                -- under a collapsed singleton can be another pull request, so
                -- the close of the stalled target has to reach it here.
                OR EXISTS (
                    SELECT 1
                      FROM repo_watch_event AS parked_state
                     WHERE parked_state.event_id = obligation.parked_state_event_id
                       AND parked_state.repository = $1
                       AND parked_state.pull_request_number = $2
                )
            )",
    )
    .bind(repository.as_str())
    .bind(pull_request)
    .bind(repo_watch_obligation_settlement_to_str(
        RepoWatchObligationSettlementStorageKind::TargetClosed,
    ))
    .bind(cutoff_event.as_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn settle_dispatch_obligation(
    transaction: &mut Transaction<'_, Postgres>,
    obligation: Uuid,
    event: RepoWatchEventId,
    dispatch: RepoWatchDispatchId,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    let affected = sqlx::query(
        "UPDATE repo_watch_dispatch_obligation
            SET settled_kind = $4,
                settled_dispatch_id = $3,
                settled_at = clock_timestamp()
          WHERE obligation_id = $1
            AND latest_event_id = $2
            AND settled_kind IS NULL",
    )
    .bind(obligation)
    .bind(event.as_uuid())
    .bind(dispatch.as_uuid())
    .bind(repo_watch_obligation_settlement_to_str(
        RepoWatchObligationSettlementStorageKind::Dispatched,
    ))
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RepoWatchDispatchRepositoryError::Corruption(
            "repository-watch obligation settlement lost its active row",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::RepoWatchDispatchRetryPolicy;

    #[test]
    fn a_lower_argument_replaces_every_production_bound() {
        let lowered = RepoWatchDispatchRetryPolicy::production()
            .lowered_to(Duration::from_secs(1), Duration::from_secs(4));

        assert_eq!(lowered.backoff_base(), Duration::from_secs(1));
        assert_eq!(lowered.backoff_cap(), Duration::from_secs(4));
    }

    #[test]
    fn a_higher_argument_leaves_every_production_bound_in_place() {
        let production = RepoWatchDispatchRetryPolicy::production();

        let raised = production.lowered_to(
            Duration::from_secs(u64::from(u32::MAX)),
            Duration::from_secs(u64::from(u32::MAX)),
        );

        assert_eq!(raised, production);
    }

    /// A cap the doubling never reaches is not a cap. Three doublings is well
    /// inside the attempt budget the schema holds, so the last attempts of an
    /// exhausting lineage are spaced by the cap rather than by a delay that
    /// keeps growing.
    #[test]
    fn the_backoff_cap_binds_within_three_doublings_of_the_base() {
        let production = RepoWatchDispatchRetryPolicy::production();

        assert!(production.backoff_cap() > production.backoff_base());
        assert!(production.backoff_cap() < production.backoff_base() * 8);
    }
}
