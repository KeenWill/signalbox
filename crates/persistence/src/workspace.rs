//! Atomic workspace operator writes and replay for `docs/spec/identity-and-commands.md`.

use crate::command_registry::{self, CommandKind};
use signalbox_application::workspace::WorkspaceIdentityGenerator;
use signalbox_domain::{
    ConfiguredGitRemoteRecord, GitRemoteMintId, GitRemoteName, GitRemoteUrl, GitRemoteWithdrawalId,
    WorkspaceCommand, WorkspaceCommandResult, WorkspaceId, WorkspaceOperation, WorkspaceOrigin,
    WorkspaceRecord, WorkspaceRootPath,
};
use sqlx::{PgConnection, PgPool, Row};

/// A recorded fact or a different meaning already bound to the identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceOutcome {
    /// The original immutable result, also returned on equal replay.
    Applied(WorkspaceCommandResult),
    /// The command identifier is already bound to another payload or kind.
    ConflictingReuse,
}

/// Workspace storage failure.
#[derive(Debug, signalbox_derive::OperatorError)]
pub enum WorkspaceError {
    /// A database operation failed.
    #[error("workspace database failure: {field_0}")]
    Database(#[source] sqlx::Error),
    /// Commit failed without establishing whether the fact was stored.
    #[error("workspace commit outcome is ambiguous: {field_0}")]
    CommitAmbiguous(#[source] sqlx::Error),
    /// The request violates a workspace or remote state constraint.
    #[error("workspace request rejected")]
    Rejected,
    /// Stored values cannot reconstruct the recorded request.
    #[error("invalid workspace record: {field_0}")]
    Corruption(&'static str),
}

impl From<sqlx::Error> for WorkspaceError {
    fn from(error: sqlx::Error) -> Self {
        match error
            .as_database_error()
            .and_then(|error| error.constraint())
        {
            Some(
                "workspace_root_path_key"
                | "configured_git_remote_mint_workspace_fk"
                | "configured_git_remote_live_pk"
                | "configured_git_remote_withdrawal_mint_fk"
                | "configured_git_remote_withdrawal_mint_key",
            ) => Self::Rejected,
            _ => Self::Database(error),
        }
    }
}

/// Transactional workspace operator store.
#[derive(Clone, Debug)]
pub struct WorkspaceRepository {
    pool: PgPool,
}

impl WorkspaceRepository {
    /// Uses the hub's existing pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Recovers an original registration request without resolving its path again.
    pub async fn registration_replay(
        &self,
        command_id: signalbox_domain::DurableCommandId,
        requested_root: &str,
    ) -> Result<Option<WorkspaceOutcome>, WorkspaceError> {
        let mut connection = self.pool.acquire().await?;
        let Some(row) = sqlx::query(
            "SELECT root_path, registration_request_root FROM workspace WHERE command_id = $1",
        )
        .bind(command_id.into_uuid())
        .fetch_optional(&mut *connection)
        .await?
        else {
            return Ok(None);
        };
        let original: Option<String> = row.try_get("registration_request_root")?;
        let original = original.ok_or(WorkspaceError::Corruption("registration request root"))?;
        if original != requested_root {
            return Ok(None);
        }
        let root = WorkspaceRootPath::try_new(row.try_get("root_path")?)
            .map_err(|_| WorkspaceError::Corruption("workspace root"))?;
        let command = WorkspaceCommand::new(command_id, WorkspaceOperation::Register { root });
        replay(&mut connection, &command)
            .await?
            .map(Some)
            .ok_or(WorkspaceError::Corruption("registration registry claim"))
    }

    /// Retains the original request spelling with a newly resolved registration.
    pub async fn register(
        &self,
        command_id: signalbox_domain::DurableCommandId,
        requested_root: &str,
        root: WorkspaceRootPath,
        ids: &mut impl WorkspaceIdentityGenerator,
    ) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.handle_with_registration_request(
            WorkspaceCommand::new(command_id, WorkspaceOperation::Register { root }),
            Some(requested_root),
            ids,
        )
        .await
    }

    /// Records one operator command, or returns its exact recorded result.
    pub async fn handle(
        &self,
        command: WorkspaceCommand,
        ids: &mut impl WorkspaceIdentityGenerator,
    ) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.handle_with_registration_request(command, None, ids)
            .await
    }

    async fn handle_with_registration_request(
        &self,
        command: WorkspaceCommand,
        registration_request_root: Option<&str>,
        ids: &mut impl WorkspaceIdentityGenerator,
    ) -> Result<WorkspaceOutcome, WorkspaceError> {
        let mut tx = self.pool.begin().await?;
        if let Some(outcome) = replay(&mut tx, &command).await? {
            return Ok(outcome);
        }
        let kind = kind(command.operation());
        let claimed = sqlx::query("INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at, issuer_kind) VALUES ($1, $2, 1, transaction_timestamp(), 'operator') ON CONFLICT DO NOTHING")
            .bind(command.command_id().into_uuid()).bind(crate::mapping::durable_command_kind_to_str(kind))
            .execute(&mut *tx).await?.rows_affected() == 1;
        if !claimed {
            return replay(&mut tx, &command)
                .await?
                .ok_or(WorkspaceError::Corruption("winning command missing"));
        }
        let result = match command.operation() {
            WorkspaceOperation::Register { root } => {
                let record = WorkspaceRecord::new(
                    ids.workspace(),
                    root.clone(),
                    WorkspaceOrigin::OperatorRegistered,
                );
                sqlx::query("INSERT INTO workspace (workspace_id, root_path, origin, command_id, command_kind, storage_version, registration_request_root) VALUES ($1, $2, 'operator_registered', $3, 'register_workspace', 1, $4)")
                    .bind(record.id().into_uuid()).bind(record.root().as_str()).bind(command.command_id().into_uuid()).bind(registration_request_root.unwrap_or(root.as_str())).execute(&mut *tx).await?;
                WorkspaceCommandResult::Registered(record.id())
            }
            WorkspaceOperation::MintRemote {
                workspace,
                name,
                url,
            } => {
                let record = ConfiguredGitRemoteRecord::new(
                    ids.remote_mint(),
                    *workspace,
                    name.clone(),
                    url.clone(),
                );
                sqlx::query("INSERT INTO configured_git_remote_mint (mint_id, command_id, command_kind, storage_version, workspace_id, remote_name, remote_url) VALUES ($1, $2, 'mint_git_remote', 1, $3, $4, $5)")
                    .bind(record.mint().into_uuid()).bind(command.command_id().into_uuid()).bind(record.workspace().into_uuid()).bind(record.name().as_str()).bind(record.url().as_str()).execute(&mut *tx).await?;
                WorkspaceCommandResult::Minted(record.mint())
            }
            WorkspaceOperation::WithdrawRemote { mint } => {
                let withdrawal = ids.remote_withdrawal();
                sqlx::query("INSERT INTO configured_git_remote_withdrawal (withdrawal_id, mint_id, command_id, command_kind, storage_version) VALUES ($1, $2, $3, 'withdraw_git_remote', 1)")
                    .bind(withdrawal.into_uuid()).bind(mint.into_uuid()).bind(command.command_id().into_uuid()).execute(&mut *tx).await?;
                WorkspaceCommandResult::Withdrawn(withdrawal)
            }
        };
        tx.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                WorkspaceError::CommitAmbiguous(error)
            } else {
                WorkspaceError::from(error)
            }
        })?;
        Ok(WorkspaceOutcome::Applied(result))
    }
}

fn kind(operation: &WorkspaceOperation) -> CommandKind {
    match operation {
        WorkspaceOperation::Register { .. } => CommandKind::RegisterWorkspace,
        WorkspaceOperation::MintRemote { .. } => CommandKind::MintGitRemote,
        WorkspaceOperation::WithdrawRemote { .. } => CommandKind::WithdrawGitRemote,
    }
}

async fn replay(
    connection: &mut PgConnection,
    command: &WorkspaceCommand,
) -> Result<Option<WorkspaceOutcome>, WorkspaceError> {
    let Some(stored_kind) = command_registry::inspect(connection, command.command_id())
        .await
        .map_err(|error| match error {
            command_registry::RegistryInspectionError::Database(error) => {
                WorkspaceError::Database(error)
            }
            command_registry::RegistryInspectionError::Corruption(error) => {
                WorkspaceError::Corruption(match error {
                    command_registry::RegistryCorruption::UnsupportedKind(_) => "registry kind",
                    command_registry::RegistryCorruption::UnsupportedVersion(_) => {
                        "registry version"
                    }
                    command_registry::RegistryCorruption::MissingTypedRecord(_) => {
                        "registry typed record"
                    }
                    command_registry::RegistryCorruption::ConflictingTypedRecords => {
                        "registry record conflict"
                    }
                })
            }
        })?
    else {
        return Ok(None);
    };
    if stored_kind != kind(command.operation()) {
        return Ok(Some(WorkspaceOutcome::ConflictingReuse));
    }
    let id = command.command_id().into_uuid();
    let (operation, result) = match command.operation() {
        WorkspaceOperation::Register { .. } => {
            let row =
                sqlx::query("SELECT workspace_id, root_path FROM workspace WHERE command_id = $1")
                    .bind(id)
                    .fetch_one(&mut *connection)
                    .await?;
            let root = WorkspaceRootPath::try_new(row.try_get::<String, _>("root_path")?)
                .map_err(|_| WorkspaceError::Corruption("workspace root"))?;
            (
                WorkspaceOperation::Register { root },
                WorkspaceCommandResult::Registered(WorkspaceId::from_uuid(
                    row.try_get("workspace_id")?,
                )),
            )
        }
        WorkspaceOperation::MintRemote { .. } => {
            let row = sqlx::query("SELECT mint_id, workspace_id, remote_name, remote_url FROM configured_git_remote_mint WHERE command_id = $1").bind(id).fetch_one(&mut *connection).await?;
            let name = GitRemoteName::try_new(row.try_get::<String, _>("remote_name")?)
                .map_err(|_| WorkspaceError::Corruption("remote name"))?;
            let url = GitRemoteUrl::try_new(row.try_get::<String, _>("remote_url")?)
                .map_err(|_| WorkspaceError::Corruption("remote URL"))?;
            (
                WorkspaceOperation::MintRemote {
                    workspace: WorkspaceId::from_uuid(row.try_get("workspace_id")?),
                    name,
                    url,
                },
                WorkspaceCommandResult::Minted(GitRemoteMintId::from_uuid(row.try_get("mint_id")?)),
            )
        }
        WorkspaceOperation::WithdrawRemote { .. } => {
            let row = sqlx::query("SELECT withdrawal_id, mint_id FROM configured_git_remote_withdrawal WHERE command_id = $1").bind(id).fetch_one(&mut *connection).await?;
            (
                WorkspaceOperation::WithdrawRemote {
                    mint: GitRemoteMintId::from_uuid(row.try_get("mint_id")?),
                },
                WorkspaceCommandResult::Withdrawn(GitRemoteWithdrawalId::from_uuid(
                    row.try_get("withdrawal_id")?,
                )),
            )
        }
    };
    Ok(Some(
        if *command == WorkspaceCommand::new(command.command_id(), operation) {
            WorkspaceOutcome::Applied(result)
        } else {
            WorkspaceOutcome::ConflictingReuse
        },
    ))
}
