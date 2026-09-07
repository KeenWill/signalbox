//! Checkout facts retained on the dispatch command ledger.

use crate::{RepoWatchStore, StoreError};
use signalbox_ownership_seam::{CommitSha, DurableCommandId, RepoWatchEvent, RepoWatchEventId};
use uuid::Uuid;

/// The immutable triggering event and the checkout's durable settlement.
pub struct DispatchCheckout {
    pub event: RepoWatchEvent,
    pub head: Option<CommitSha>,
    pub stop_command: Option<DurableCommandId>,
}

#[derive(sqlx::FromRow)]
struct CheckoutRow {
    event_id: Uuid,
    normalized_payload: Vec<u8>,
    checkout_head_sha: Option<String>,
    checkout_stop_command_id: Option<Uuid>,
}

impl RepoWatchStore {
    /// Reads the retained event, including its exact PR head, for one creation.
    pub async fn dispatch_checkout(
        &self,
        command: DurableCommandId,
    ) -> Result<Option<DispatchCheckout>, StoreError> {
        let row: Option<CheckoutRow> = sqlx::query_as(
            "SELECT event.event_id, event.normalized_payload, ledger.checkout_head_sha, ledger.checkout_stop_command_id
             FROM dispatch_ledger AS ledger JOIN gh_event AS event ON event.event_id = ledger.event_id
             WHERE ledger.command_id = $1 AND ledger.command_kind = 'create_session'")
            .bind(command.into_uuid()).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(DispatchCheckout {
                event: crate::event_decode::event(
                    RepoWatchEventId::from_uuid(row.event_id),
                    &row.normalized_payload,
                )
                .ok_or(StoreError::InvalidRetainedEvent)?,
                head: row
                    .checkout_head_sha
                    .map(CommitSha::try_new)
                    .transpose()
                    .map_err(|_| StoreError::InvalidRetainedCommand)?,
                stop_command: row
                    .checkout_stop_command_id
                    .map(DurableCommandId::from_uuid),
            })
        })
        .transpose()
    }

    /// Records the checkout at the derived root after Git has checked out its head.
    pub async fn record_dispatch_checkout(
        &self,
        command: DurableCommandId,
        head: &CommitSha,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE dispatch_ledger SET checkout_path = '.', checkout_head_sha = $2 WHERE command_id = $1 AND checkout_retired_reason IS NULL")
            .bind(command.into_uuid()).bind(head.as_str()).execute(&self.pool).await?;
        Ok(())
    }

    /// Retires failed provisioning and reserves a replayable core stop identity.
    pub async fn retire_dispatch_checkout(
        &self,
        command: DurableCommandId,
        step: &str,
        status: &str,
        stop: DurableCommandId,
    ) -> Result<DurableCommandId, StoreError> {
        let id: Uuid = sqlx::query_scalar("UPDATE dispatch_ledger SET checkout_retired_reason = 'checkout_provisioning_failed', checkout_failure_step = $2, checkout_failure_status = $3, checkout_stop_command_id = COALESCE(checkout_stop_command_id, $4) WHERE command_id = $1 RETURNING checkout_stop_command_id")
            .bind(command.into_uuid()).bind(step).bind(status).bind(stop.into_uuid()).fetch_one(&self.pool).await?;
        Ok(DurableCommandId::from_uuid(id))
    }
}
