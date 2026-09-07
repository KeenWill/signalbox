//! Durable OAuth administration claims and receipts (docs/spec/identity-and-commands.md).

mod provisioning;
mod refresh;
pub(crate) use provisioning::lock_pool_members;
pub use provisioning::{
    OauthAuthorization, OauthExchange, OauthProgress, OauthRegistration, OauthStartOutcome,
};
pub(crate) use refresh::quarantined_profiles;
pub use refresh::{OauthDispatchLease, OauthQuarantineCause, OauthStoredAuthorization};

use crate::command_registry::{self, CommandKind};
use signalbox_domain::DurableCommandId;
use sqlx::{PgConnection, PgPool, Row};

/// OAuth administration operation, separate from its wire representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OauthCredentialOperation {
    /// Initial device authorization.
    Provision,
    /// Replacement device authorization.
    Reprovision,
    /// Removal of retained authorization.
    Delete,
}

impl OauthCredentialOperation {
    const fn kind(self) -> CommandKind {
        match self {
            Self::Provision => CommandKind::ProvisionOauthCredential,
            Self::Reprovision => CommandKind::ReprovisionOauthCredential,
            Self::Delete => CommandKind::DeleteOauthCredential,
        }
    }
    const fn tables(self) -> (&'static str, &'static str) {
        match self {
            Self::Provision => (
                "provision_oauth_credential_command",
                "provision_oauth_credential_result",
            ),
            Self::Reprovision => (
                "reprovision_oauth_credential_command",
                "reprovision_oauth_credential_result",
            ),
            Self::Delete => (
                "delete_oauth_credential_command",
                "delete_oauth_credential_result",
            ),
        }
    }
}

/// Immutable relational request payload.
#[derive(Clone, Debug)]
pub struct OauthCredentialCommand {
    /// User-global command identity.
    pub command_id: DurableCommandId,
    /// Requested administration operation.
    pub operation: OauthCredentialOperation,
    /// Non-secret profile identity.
    pub profile: String,
}

impl OauthCredentialCommand {
    /// Structural replay equality excludes the command identity.
    pub fn same_payload(&self, other: &Self) -> bool {
        self.operation == other.operation && self.profile == other.profile
    }
}

/// Terminal relational receipt, separate from its wire representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OauthCredentialOutcome {
    /// Authorization was stored.
    Provisioned,
    /// Authorization already exists.
    AlreadyProvisioned,
    /// Authorization was replaced.
    Reprovisioned,
    /// Authorization was deleted.
    Deleted,
    /// No authorization remained.
    AlreadyDeleted,
    /// Re-provisioning found no authorization.
    NotProvisioned,
    /// Startup abandoned a pending exchange.
    Abandoned,
    /// A newer generation won.
    Superseded,
    /// Rejection without an authorization mutation.
    Failed(OauthCredentialFailure),
}

/// Closed OAuth administration rejection reasons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OauthCredentialFailure {
    /// The profile is undeclared.
    UnknownProfile,
    /// The profile does not use OAuth.
    NonOauthProfile,
    /// The registration changed during the exchange.
    RegistrationChanged,
    /// The device endpoint rejected the request or returned invalid details.
    DeviceEndpointRejected,
    /// The initial device request failed in transport.
    DeviceEndpointFailed,
    /// The operator denied authorization.
    AccessDenied,
    /// The authorization polling deadline expired.
    PollingExpired,
    /// The token endpoint failed.
    TokenEndpointFailed,
    /// The token response contained no identity token.
    TokenResponseWithoutIdentity,
    /// A pool co-member already holds the account identity.
    AccountIndependenceFailed,
}

impl OauthCredentialFailure {
    fn spelling(self) -> &'static str {
        match self {
            Self::UnknownProfile => "unknown_profile",
            Self::NonOauthProfile => "non_oauth_profile",
            Self::RegistrationChanged => "registration_changed",
            Self::DeviceEndpointRejected => "device_endpoint_rejected",
            Self::DeviceEndpointFailed => "device_endpoint_failed",
            Self::AccessDenied => "access_denied",
            Self::PollingExpired => "polling_expired",
            Self::TokenEndpointFailed => "token_endpoint_failed",
            Self::TokenResponseWithoutIdentity => "token_response_without_identity",
            Self::AccountIndependenceFailed => "account_independence_failed",
        }
    }
    fn from_spelling(value: &str) -> Result<Self, OauthCredentialRepositoryError> {
        match value {
            "unknown_profile" => Ok(Self::UnknownProfile),
            "non_oauth_profile" => Ok(Self::NonOauthProfile),
            "registration_changed" => Ok(Self::RegistrationChanged),
            "device_endpoint_rejected" => Ok(Self::DeviceEndpointRejected),
            "device_endpoint_failed" => Ok(Self::DeviceEndpointFailed),
            "access_denied" => Ok(Self::AccessDenied),
            "polling_expired" => Ok(Self::PollingExpired),
            "token_endpoint_failed" => Ok(Self::TokenEndpointFailed),
            "token_response_without_identity" => Ok(Self::TokenResponseWithoutIdentity),
            "account_independence_failed" => Ok(Self::AccountIndependenceFailed),
            _ => Err(OauthCredentialRepositoryError::Corruption),
        }
    }
}

impl OauthCredentialOutcome {
    fn columns(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Provisioned => ("provisioned", None),
            Self::AlreadyProvisioned => ("already_provisioned", None),
            Self::Reprovisioned => ("reprovisioned", None),
            Self::Deleted => ("deleted", None),
            Self::AlreadyDeleted => ("already_deleted", None),
            Self::NotProvisioned => ("not_provisioned", None),
            Self::Abandoned => ("abandoned", None),
            Self::Superseded => ("superseded", None),
            Self::Failed(reason) => ("failed", Some(reason.spelling())),
        }
    }
    fn from_columns(
        outcome: &str,
        reason: Option<&str>,
    ) -> Result<Self, OauthCredentialRepositoryError> {
        match (outcome, reason) {
            ("provisioned", None) => Ok(Self::Provisioned),
            ("already_provisioned", None) => Ok(Self::AlreadyProvisioned),
            ("reprovisioned", None) => Ok(Self::Reprovisioned),
            ("deleted", None) => Ok(Self::Deleted),
            ("already_deleted", None) => Ok(Self::AlreadyDeleted),
            ("not_provisioned", None) => Ok(Self::NotProvisioned),
            ("abandoned", None) => Ok(Self::Abandoned),
            ("superseded", None) => Ok(Self::Superseded),
            ("failed", Some(reason)) => {
                Ok(Self::Failed(OauthCredentialFailure::from_spelling(reason)?))
            }
            _ => Err(OauthCredentialRepositoryError::Corruption),
        }
    }
}

/// The command's durable disposition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OauthCredentialHandlingOutcome {
    /// A terminal receipt committed or replayed.
    Recorded(OauthCredentialOutcome),
    /// The equal request is still pending.
    Pending,
    /// The identifier already names another payload or kind.
    ConflictingReuse,
}

/// Failure to retain or reconstruct an OAuth command.
#[derive(Debug, signalbox_derive::OperatorError)]
pub enum OauthCredentialRepositoryError {
    /// Database operation failed.
    #[error("OAuth administration database failure")]
    Database,
    /// The terminal transaction may have committed; equal replay resolves it.
    #[error("OAuth administration commit is ambiguous")]
    CommitAmbiguous,
    /// Stored records violate their closed relational shape.
    #[error("OAuth administration record is inconsistent")]
    Corruption,
    /// A caller supplied an invalid profile name.
    #[error("OAuth administration profile is invalid")]
    InvalidProfile,
}

impl From<sqlx::Error> for OauthCredentialRepositoryError {
    fn from(_error: sqlx::Error) -> Self {
        Self::Database
    }
}

/// PostgreSQL command claim and replay boundary.
#[derive(Clone, Debug)]
pub struct OauthCredentialRepository {
    pool: PgPool,
}

impl OauthCredentialRepository {
    /// Uses the owner's database pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Atomically records a no-exchange disposition, or replays the existing meaning.
    pub async fn record(
        &self,
        command: &OauthCredentialCommand,
        evaluate: impl FnOnce() -> OauthCredentialOutcome,
    ) -> Result<OauthCredentialHandlingOutcome, OauthCredentialRepositoryError> {
        let mut tx = self.pool.begin().await?;
        if let Some(existing) = claim(&mut tx, command).await? {
            return Ok(existing);
        }
        let (_, result_table) = command.operation.tables();
        let outcome = evaluate();
        let (kind, reason) = outcome.columns();
        sqlx::query(sqlx::AssertSqlSafe(
            format!("INSERT INTO {result_table} (command_id, outcome, reason) VALUES ($1, $2, $3)")
                .as_str(),
        ))
        .bind(command.command_id.into_uuid())
        .bind(kind)
        .bind(reason)
        .execute(&mut *tx)
        .await?;
        tx.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                OauthCredentialRepositoryError::CommitAmbiguous
            } else {
                OauthCredentialRepositoryError::Database
            }
        })?;
        Ok(OauthCredentialHandlingOutcome::Recorded(outcome))
    }
}

async fn claim(
    connection: &mut PgConnection,
    command: &OauthCredentialCommand,
) -> Result<Option<OauthCredentialHandlingOutcome>, OauthCredentialRepositoryError> {
    if command.profile.is_empty()
        || command.profile.len() > 256
        || command.profile.trim() != command.profile
        || command.profile.contains('\0')
    {
        return Err(OauthCredentialRepositoryError::InvalidProfile);
    }
    if let Some(kind) = inspect(connection, command.command_id).await? {
        return existing(connection, command, kind).await.map(Some);
    }
    let issuer = command_registry::issuer_columns(signalbox_domain::CommandPrincipal::Operator);
    let claimed = sqlx::query(
            "INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at, issuer_kind, issuer_module)
             VALUES ($1, $2, 1, transaction_timestamp(), $3, $4) ON CONFLICT DO NOTHING",
        ).bind(command.command_id.into_uuid())
            .bind(crate::mapping::durable_command_kind_to_str(command.operation.kind()))
            .bind(issuer.0).bind(issuer.1)
            .execute(&mut *connection).await?.rows_affected() == 1;
    if !claimed {
        let kind = inspect(connection, command.command_id)
            .await?
            .ok_or(OauthCredentialRepositoryError::Corruption)?;
        return existing(connection, command, kind).await.map(Some);
    }
    // Table identifiers come exclusively from the closed operation; values are bound.
    let (request_table, _) = command.operation.tables();
    sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO {request_table} (command_id, command_kind, storage_version, profile) VALUES ($1, $2, 1, $3)").as_str()))
            .bind(command.command_id.into_uuid())
            .bind(crate::mapping::durable_command_kind_to_str(command.operation.kind()))
            .bind(&command.profile).execute(&mut *connection).await?;
    Ok(None)
}

async fn inspect(
    connection: &mut PgConnection,
    id: DurableCommandId,
) -> Result<Option<CommandKind>, OauthCredentialRepositoryError> {
    command_registry::inspect(connection, id)
        .await
        .map_err(|error| match error {
            command_registry::RegistryInspectionError::Database(_) => {
                OauthCredentialRepositoryError::Database
            }
            command_registry::RegistryInspectionError::Corruption(_) => {
                OauthCredentialRepositoryError::Corruption
            }
        })
}

async fn existing(
    connection: &mut PgConnection,
    command: &OauthCredentialCommand,
    kind: CommandKind,
) -> Result<OauthCredentialHandlingOutcome, OauthCredentialRepositoryError> {
    if kind != command.operation.kind() {
        return Ok(OauthCredentialHandlingOutcome::ConflictingReuse);
    }
    // Table identifiers come exclusively from the closed operation; values are bound.
    let (request_table, result_table) = command.operation.tables();
    let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT request.profile, result.outcome, result.reason FROM {request_table} AS request LEFT JOIN {result_table} AS result USING (command_id) WHERE request.command_id = $1").as_str()))
        .bind(command.command_id.into_uuid()).fetch_one(connection).await?;
    let retained = OauthCredentialCommand {
        command_id: command.command_id,
        operation: command.operation,
        profile: row.try_get("profile")?,
    };
    if !command.same_payload(&retained) {
        return Ok(OauthCredentialHandlingOutcome::ConflictingReuse);
    }
    let outcome: Option<String> = row.try_get("outcome")?;
    let reason: Option<String> = row.try_get("reason")?;
    outcome.map_or(Ok(OauthCredentialHandlingOutcome::Pending), |outcome| {
        Ok(OauthCredentialHandlingOutcome::Recorded(
            OauthCredentialOutcome::from_columns(&outcome, reason.as_deref())?,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_payload_equality_excludes_identity_but_retains_operation_and_profile() {
        let command = OauthCredentialCommand {
            command_id: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            operation: OauthCredentialOperation::Provision,
            profile: "subscription".into(),
        };
        let another_identity = OauthCredentialCommand {
            command_id: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            ..command.clone()
        };
        assert!(command.same_payload(&another_identity));
        assert!(!command.same_payload(&OauthCredentialCommand {
            operation: OauthCredentialOperation::Reprovision,
            ..another_identity.clone()
        }));
        assert!(!command.same_payload(&OauthCredentialCommand {
            profile: "different".into(),
            ..another_identity
        }));
    }
}
