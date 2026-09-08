//! PostgreSQL composition for the session-status tool's typed writer port.

use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, ReplaceSessionMetadataOutcome,
    ReplaceSessionMetadataRequest, ReplaceSessionMetadataService,
};
use signalbox_domain::{ReplaceSessionMetadataRejectedResult, ReplaceSessionMetadataResult};
use signalbox_persistence::session_metadata::{
    SessionMetadataRepository, SessionMetadataRepositoryError,
};
use signalbox_tools_basic::{SessionStatusWrite, SessionStatusWriteOutcome, SessionStatusWriter};
use sqlx::PgPool;

/// PostgreSQL-backed writer using `ReplaceSessionMetadataService`.
#[derive(Clone, Debug)]
pub struct PostgresSessionStatusWriter {
    pool: PgPool,
}

impl PostgresSessionStatusWriter {
    /// Uses the supplied pool for one command transaction per invocation.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Sanitized PostgreSQL metadata-writer failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostgresSessionStatusWriterError {
    /// Request correlation did not form a valid durable command identity.
    InvalidCommandIdentity,
    /// PostgreSQL failed outside the final ambiguous commit acknowledgement.
    Database,
    /// A freshly derived command identity collided with durable identity.
    IdentityCollision,
    /// Stored metadata command facts were inconsistent.
    Corruption,
}

impl std::fmt::Display for PostgresSessionStatusWriterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCommandIdentity => "session status command identity is invalid",
            Self::Database => "session status database operation failed",
            Self::IdentityCollision => "session status command identity collided",
            Self::Corruption => "session status durable facts are inconsistent",
        })
    }
}

impl std::error::Error for PostgresSessionStatusWriterError {}

impl ClassifyOperatorFailure for PostgresSessionStatusWriterError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::InvalidCommandIdentity => OperatorFailureClass::CallerOrHubBug,
            Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::Database => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::Corruption => OperatorFailureClass::FailClosedCorruption,
        }
    }
}

impl SessionStatusWriter for PostgresSessionStatusWriter {
    type Error = PostgresSessionStatusWriterError;

    async fn write(
        &mut self,
        update: SessionStatusWrite,
    ) -> Result<SessionStatusWriteOutcome, Self::Error> {
        let request = ReplaceSessionMetadataRequest::try_new_for_tool(
            update.command(),
            update.session(),
            update.request(),
            update.replacement().clone(),
        )
        .map_err(|_| PostgresSessionStatusWriterError::InvalidCommandIdentity)?;
        let mut service =
            ReplaceSessionMetadataService::new(SessionMetadataRepository::new(self.pool.clone()));
        match service.execute(request).await {
            Ok(ReplaceSessionMetadataOutcome::Recorded(ReplaceSessionMetadataResult::Applied(
                applied,
            ))) => Ok(SessionStatusWriteOutcome::Applied(
                applied.snapshot().clone(),
            )),
            Ok(ReplaceSessionMetadataOutcome::Recorded(
                ReplaceSessionMetadataResult::Rejected(
                    ReplaceSessionMetadataRejectedResult::SessionNotFound(_),
                ),
            )) => Ok(SessionStatusWriteOutcome::SessionNotFound),
            Ok(ReplaceSessionMetadataOutcome::ConflictingReuse { .. }) => {
                Err(PostgresSessionStatusWriterError::IdentityCollision)
            }
            Err(SessionMetadataRepositoryError::CommitAmbiguous(_)) => {
                Ok(SessionStatusWriteOutcome::Ambiguous)
            }
            Err(SessionMetadataRepositoryError::Database(_)) => {
                Err(PostgresSessionStatusWriterError::Database)
            }
            Err(
                SessionMetadataRepositoryError::DifferentCommandKind { .. }
                | SessionMetadataRepositoryError::Corruption(_),
            ) => Err(PostgresSessionStatusWriterError::Corruption),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A durable command already using the freshly attempt-derived identity is
    /// an operational identity collision rather than a caller defect.
    #[test]
    fn session_status_command_reuse_is_identity_collision() {
        assert_eq!(
            PostgresSessionStatusWriterError::IdentityCollision.operator_failure_class(),
            OperatorFailureClass::IdentityCollision
        );
    }
}
