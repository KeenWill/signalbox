//! Checkout facts retained on the dispatch command ledger.

use crate::{RepoWatchStore, StoreError};
use signalbox_ownership_seam::{
    CommitSha, DurableCommandId, RepoWatchEvent, RepoWatchEventId, SessionId,
};
use uuid::Uuid;

/// Closed terminal reasons for a checkout dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckoutRetirementReason {
    ProvisioningFailed,
    RepositoryUnconfigured,
}

impl CheckoutRetirementReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProvisioningFailed => "checkout_provisioning_failed",
            Self::RepositoryUnconfigured => "repository_unconfigured",
        }
    }
}

/// The immutable triggering event and the checkout's durable settlement.
pub struct DispatchCheckout {
    pub event: RepoWatchEvent,
    pub head: Option<CommitSha>,
    pub stop_command: Option<DurableCommandId>,
    pub retired_reason: Option<CheckoutRetirementReason>,
    pub location: Option<CheckoutLocation>,
}

/// Provisioning location, independent of lifecycle settlement and current configuration.
pub struct CheckoutLocation {
    pub session: SessionId,
    pub workspace_root: Vec<u8>,
}

#[derive(sqlx::FromRow)]
struct CheckoutRow {
    event_id: Uuid,
    normalized_payload: Vec<u8>,
    checkout_head_sha: Option<String>,
    checkout_stop_command_id: Option<Uuid>,
    checkout_retired_reason: Option<String>,
    checkout_workspace_root: Option<Vec<u8>>,
    checkout_session_id: Option<Uuid>,
}

/// A checkout whose filesystem removal has not yet settled.
pub struct CheckoutRemovalCandidate {
    pub command: DurableCommandId,
    pub location: CheckoutLocation,
    pub retired_reason: Option<String>,
}

impl RepoWatchStore {
    /// Lists provisioned or failed checkouts, including interrupted command follow-ups.
    pub async fn checkout_removal_candidates(
        &self,
    ) -> Result<Vec<CheckoutRemovalCandidate>, StoreError> {
        let rows: Vec<(Uuid, Uuid, Vec<u8>, Option<String>)> = sqlx::query_as(
            "SELECT command_id, checkout_session_id, checkout_workspace_root, checkout_retired_reason FROM dispatch_ledger
             WHERE checkout_session_id IS NOT NULL AND NOT checkout_removed
               AND (checkout_path IS NOT NULL OR checkout_retired_reason IS NOT NULL)",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(command, session, workspace_root, retired_reason)| CheckoutRemovalCandidate {
                    command: DurableCommandId::from_uuid(command),
                    location: CheckoutLocation {
                        session: SessionId::from_uuid(session),
                        workspace_root,
                    },
                    retired_reason,
                },
            )
            .collect())
    }

    /// Marks removal only after the derived checkout is absent.
    pub async fn settle_checkout_removal(
        &self,
        command: DurableCommandId,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE dispatch_ledger SET checkout_removed = true WHERE command_id = $1")
            .bind(command.into_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Reads the retained event, including its exact PR head, for one creation.
    pub async fn dispatch_checkout(
        &self,
        command: DurableCommandId,
    ) -> Result<Option<DispatchCheckout>, StoreError> {
        let row: Option<CheckoutRow> = sqlx::query_as(
            "SELECT event.event_id, event.normalized_payload, ledger.checkout_head_sha, ledger.checkout_stop_command_id, ledger.checkout_retired_reason, ledger.checkout_workspace_root, ledger.checkout_session_id
             FROM dispatch_ledger AS ledger JOIN gh_event AS event ON event.event_id = ledger.event_id
             WHERE ledger.command_id = $1 AND ledger.command_kind = 'create_session'")
            .bind(command.into_uuid()).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(DispatchCheckout {
                location: match (row.checkout_session_id, row.checkout_workspace_root) {
                    (Some(session), Some(workspace_root)) => Some(CheckoutLocation {
                        session: SessionId::from_uuid(session),
                        workspace_root,
                    }),
                    (None, None) => None,
                    _ => return Err(StoreError::InvalidRetainedCommand),
                },
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
                retired_reason: match row.checkout_retired_reason.as_deref() {
                    None => None,
                    Some("checkout_provisioning_failed") => {
                        Some(CheckoutRetirementReason::ProvisioningFailed)
                    }
                    Some("repository_unconfigured") => {
                        Some(CheckoutRetirementReason::RepositoryUnconfigured)
                    }
                    Some(_) => return Err(StoreError::InvalidRetainedCommand),
                },
            })
        })
        .transpose()
    }

    /// Pins cleanup identity before filesystem work, retaining it across retries.
    pub async fn retain_checkout_location(
        &self,
        command: DurableCommandId,
        session: SessionId,
        workspace_root: &[u8],
    ) -> Result<CheckoutLocation, StoreError> {
        let (session, workspace_root): (Uuid, Vec<u8>) = sqlx::query_as(
            "UPDATE dispatch_ledger
             SET checkout_session_id = COALESCE(checkout_session_id, $2),
                 checkout_workspace_root = COALESCE(checkout_workspace_root, $3)
             WHERE command_id = $1 RETURNING checkout_session_id, checkout_workspace_root",
        )
        .bind(command.into_uuid())
        .bind(session.into_uuid())
        .bind(workspace_root)
        .fetch_one(&self.pool)
        .await?;
        Ok(CheckoutLocation {
            session: SessionId::from_uuid(session),
            workspace_root,
        })
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

    /// Retains a terminal checkout disposition and a replayable core stop identity.
    pub async fn retire_dispatch_checkout(
        &self,
        command: DurableCommandId,
        reason: CheckoutRetirementReason,
        step: &str,
        status: &str,
        stop: DurableCommandId,
    ) -> Result<DurableCommandId, StoreError> {
        let id: Uuid = sqlx::query_scalar("UPDATE dispatch_ledger SET checkout_retired_reason = $5, checkout_failure_step = $2, checkout_failure_status = $3, checkout_stop_command_id = COALESCE(checkout_stop_command_id, $4) WHERE command_id = $1 RETURNING checkout_stop_command_id")
            .bind(command.into_uuid()).bind(step).bind(status).bind(stop.into_uuid()).bind(reason.as_str()).fetch_one(&self.pool).await?;
        Ok(DurableCommandId::from_uuid(id))
    }
}
