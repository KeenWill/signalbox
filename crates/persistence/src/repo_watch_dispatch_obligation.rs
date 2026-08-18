//! Durable, singleton-collapsed work owed after repository-watch refusal.

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

/// One latest-state delivery obligation retained after singleton refusal.
#[derive(Clone, Debug)]
pub struct RepoWatchDispatchObligation {
    id: Uuid,
    first_event_id: RepoWatchEventId,
    latest_event: RepoWatchEvent,
    matched_event_count: u64,
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

    pub(crate) fn into_parts(self) -> (Uuid, RepoWatchEvent, StoredSingletonKey) {
        (self.id, self.latest_event, self.singleton)
    }
}

pub(crate) enum ObligationAdmission {
    Pending,
    Superseded,
    Settled(RepoWatchRuleEvaluationOutcome),
}

impl PostgresRepoWatchDispatchStore {
    /// Loads the oldest outstanding obligation whose singleton and cooldown are free.
    pub async fn load_next_dispatch_obligation(
        &self,
        repository: &RepositorySlug,
        rule_id: &RepoWatchRuleId,
        rule_version: RepoWatchRuleVersion,
    ) -> Result<Option<RepoWatchDispatchObligation>, RepoWatchDispatchRepositoryError> {
        let row = sqlx::query(
            "SELECT obligation.obligation_id, obligation.first_event_id,
                    obligation.latest_event_id, obligation.matched_event_count,
                    obligation.singleton_scope, obligation.singleton_repository,
                    obligation.singleton_pull_request_number,
                    obligation.singleton_stack_root_pull_request_number
               FROM repo_watch_dispatch_obligation AS obligation
              WHERE obligation.repository = $1
                AND obligation.rule_id = $2
                AND obligation.rule_version = $3
                AND obligation.settled_kind IS NULL
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
        Ok(Some(RepoWatchDispatchObligation {
            id: row.try_get("obligation_id")?,
            first_event_id: RepoWatchEventId::from_uuid(row.try_get("first_event_id")?),
            latest_event,
            matched_event_count: u64::try_from(matched_event_count).map_err(|_| {
                RepoWatchDispatchRepositoryError::Corruption(
                    "owed repository-watch match count is invalid",
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
    blocking_dispatch: Option<RepoWatchDispatchId>,
    event: &RepoWatchEvent,
    rule_id: &RepoWatchRuleId,
    rule_version: RepoWatchRuleVersion,
    singleton: &StoredSingletonKey,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    let updated = sqlx::query(
        "UPDATE repo_watch_dispatch_obligation
            SET repository = $1,
                latest_event_id = $2,
                matched_event_count = matched_event_count + 1,
                blocking_dispatch_id = COALESCE($3, blocking_dispatch_id),
                latest_match_at = clock_timestamp()
          WHERE rule_id = $4
            AND rule_version = $5
            AND singleton_scope = $6
            AND singleton_repository IS NOT DISTINCT FROM $7
            AND singleton_pull_request_number IS NOT DISTINCT FROM $8
            AND singleton_stack_root_pull_request_number IS NOT DISTINCT FROM $9
            AND settled_kind IS NULL",
    )
    .bind(event.repository().as_str())
    .bind(event.id().as_uuid())
    .bind(blocking_dispatch.map(|value| *value.as_uuid()))
    .bind(rule_id.as_str())
    .bind(stored_rule_version(rule_version)?)
    .bind(repo_watch_singleton_scope_to_str(singleton.scope))
    .bind(singleton.repository.as_deref())
    .bind(singleton.pull_request)
    .bind(singleton.stack_root_pull_request)
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if updated == 1 {
        return Ok(());
    }
    let blocking_dispatch =
        blocking_dispatch.ok_or(RepoWatchDispatchRepositoryError::Corruption(
            "new repository-watch obligation lacks its blocking dispatch",
        ))?;
    sqlx::query(
        "INSERT INTO repo_watch_dispatch_obligation
            (obligation_id, repository, rule_id, rule_version,
             singleton_scope, singleton_repository, singleton_pull_request_number,
             singleton_stack_root_pull_request_number, first_repository,
             first_event_id, latest_event_id, matched_event_count, blocking_dispatch_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $2, $9, $9, 1, $10)",
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
    .bind(blocking_dispatch.as_uuid())
    .execute(&mut **transaction)
    .await?;
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
    let settled_kind: Option<String> = row.try_get("settled_kind")?;
    let Some(settled_kind) = settled_kind else {
        return Ok(ObligationAdmission::Pending);
    };
    match repo_watch_obligation_settlement_from_str(&settled_kind) {
        Some(RepoWatchObligationSettlementStorageKind::Deactivated) => Ok(
            ObligationAdmission::Settled(RepoWatchRuleEvaluationOutcome::Inactive),
        ),
        Some(RepoWatchObligationSettlementStorageKind::TargetClosed) => Ok(
            ObligationAdmission::Settled(RepoWatchRuleEvaluationOutcome::TargetClosed),
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

pub(crate) async fn settle_terminal_target_obligations(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    pull_request: Decimal,
    cutoff_event: RepoWatchEventId,
) -> Result<(), RepoWatchDispatchRepositoryError> {
    sqlx::query(
        "UPDATE repo_watch_dispatch_obligation AS obligation
            SET settled_kind = $3,
                settled_at = clock_timestamp()
           FROM repo_watch_event AS event
          WHERE obligation.latest_event_id = event.event_id
            AND obligation.settled_kind IS NULL
            AND event.repository = $1
            AND event.pull_request_number = $2
            AND event.event_id <> $4",
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
