//! Checkout facts retained on the dispatch command ledger.

use crate::{RepoWatchStore, StoreError};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_ownership_seam::{
    CommitSha, DurableCommandId, RepoWatchDispatchId, RepoWatchEvent, RepoWatchEventId, SessionId,
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
    pub dispatch: RepoWatchDispatchId,
    pub event: RepoWatchEvent,
    pub head: Option<CommitSha>,
    pub removed: bool,
    pub stop_command: Option<DurableCommandId>,
    pub retired_reason: Option<CheckoutRetirementReason>,
    pub location: Option<CheckoutLocation>,
}

/// Provisioning location, independent of lifecycle settlement and current configuration.
pub struct CheckoutLocation {
    pub session: SessionId,
    pub workspace_root: Vec<u8>,
}

/// Filesystem identity retained before Git writes into the checkout directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckoutDirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(sqlx::FromRow)]
struct CheckoutRemovalRow {
    dispatch_ref: Uuid,
    command_id: Uuid,
    checkout_created: bool,
    checkout_session_id: Uuid,
    checkout_workspace_root: Vec<u8>,
    checkout_retired_reason: Option<String>,
    checkout_device: Option<Decimal>,
    checkout_inode: Option<Decimal>,
}

#[derive(sqlx::FromRow)]
struct CheckoutRow {
    dispatch_ref: Uuid,
    event_id: Uuid,
    normalized_payload: Vec<u8>,
    checkout_head_sha: Option<String>,
    checkout_removed: bool,
    checkout_stop_command_id: Option<Uuid>,
    checkout_retired_reason: Option<String>,
    checkout_workspace_root: Option<Vec<u8>>,
    checkout_session_id: Option<Uuid>,
}

/// A checkout whose filesystem removal has not yet settled.
pub struct CheckoutRemovalCandidate {
    pub dispatch: RepoWatchDispatchId,
    pub command: DurableCommandId,
    pub created: bool,
    pub location: CheckoutLocation,
    pub retired_reason: Option<String>,
    pub identity: Option<CheckoutDirectoryIdentity>,
}

impl RepoWatchStore {
    /// Lists retained locations, including interrupted filesystem provisioning.
    pub async fn checkout_removal_candidates(
        &self,
    ) -> Result<Vec<CheckoutRemovalCandidate>, StoreError> {
        let rows: Vec<CheckoutRemovalRow> = sqlx::query_as(
            "SELECT dispatch_ref, command_id, checkout_created, checkout_session_id, checkout_workspace_root, checkout_retired_reason, checkout_device, checkout_inode FROM dispatch_ledger
             WHERE checkout_session_id IS NOT NULL AND NOT checkout_removed",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(CheckoutRemovalCandidate {
                    dispatch: RepoWatchDispatchId::from_uuid(row.dispatch_ref),
                    command: DurableCommandId::from_uuid(row.command_id),
                    created: row.checkout_created,
                    location: CheckoutLocation {
                        session: SessionId::from_uuid(row.checkout_session_id),
                        workspace_root: row.checkout_workspace_root,
                    },
                    retired_reason: row.checkout_retired_reason,
                    identity: match (row.checkout_device, row.checkout_inode) {
                        (Some(device), Some(inode)) => Some(decode_identity(device, inode)?),
                        (None, None) => None,
                        _ => return Err(StoreError::InvalidRetainedCommand),
                    },
                })
            })
            .collect()
    }

    /// Settles cleanup after removal or when the dispatch does not own the directory.
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
            "SELECT ledger.dispatch_ref, event.event_id, event.normalized_payload, ledger.checkout_head_sha, ledger.checkout_removed, ledger.checkout_stop_command_id, ledger.checkout_retired_reason, ledger.checkout_workspace_root, ledger.checkout_session_id
             FROM dispatch_ledger AS ledger JOIN gh_event AS event ON event.event_id = ledger.event_id
             WHERE ledger.command_id = $1 AND ledger.command_kind = 'create_session'")
            .bind(command.into_uuid()).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(DispatchCheckout {
                dispatch: RepoWatchDispatchId::from_uuid(row.dispatch_ref),
                removed: row.checkout_removed,
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

    /// Records ownership only for a created directory, preserving it across retries.
    pub async fn retain_checkout_identity(
        &self,
        command: DurableCommandId,
        identity: CheckoutDirectoryIdentity,
        created: bool,
    ) -> Result<Option<CheckoutDirectoryIdentity>, StoreError> {
        let (device, inode): (Option<Decimal>, Option<Decimal>) = sqlx::query_as(
            "UPDATE dispatch_ledger SET checkout_created = checkout_created OR $4,
                checkout_device = CASE WHEN $4 THEN COALESCE(checkout_device, $2) ELSE checkout_device END,
                checkout_inode = CASE WHEN $4 THEN COALESCE(checkout_inode, $3) ELSE checkout_inode END
             WHERE command_id = $1 RETURNING checkout_device, checkout_inode",
        )
        .bind(command.into_uuid())
        .bind(Decimal::from(identity.device))
        .bind(Decimal::from(identity.inode))
        .bind(created)
        .fetch_one(&self.pool)
        .await?;
        match (device, inode) {
            (Some(device), Some(inode)) => Ok(Some(decode_identity(device, inode)?)),
            (None, None) => Ok(None),
            _ => Err(StoreError::InvalidRetainedCommand),
        }
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

fn decode_identity(
    device: Decimal,
    inode: Decimal,
) -> Result<CheckoutDirectoryIdentity, StoreError> {
    Ok(CheckoutDirectoryIdentity {
        device: device.to_u64().ok_or(StoreError::InvalidRetainedCommand)?,
        inode: inode.to_u64().ok_or(StoreError::InvalidRetainedCommand)?,
    })
}
