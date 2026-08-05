//! Daemon adapter for atomic delegated-session await and message effects.

use std::{error::Error, fmt};

use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    DelegatedSpawnRequest, DelegationAwaitRequest, DelegationMessageId, DelegationMessageRequest,
    DelegationWaitMode, ToolDispatchAuthority,
};
use signalbox_persistence::session_delegation::{
    DelegationOperationRejection, RecordDelegationMessageOutcome, RecordDelegationWaitOutcome,
    SessionDelegationRepository, SessionDelegationRepositoryError,
};
use signalbox_tools_sessions::{
    AwaitSessionPortOutcome, AwaitSessionReceipt, SessionDelegationPort,
    SessionDelegationPortOutcome, SessionMessageReceipt, SpawnSessionReceipt,
};
use sqlx::PgPool;

/// PostgreSQL-backed nonblocking delegation tool boundary.
#[derive(Clone, Debug)]
pub struct PostgresSessionDelegationPort {
    repository: SessionDelegationRepository,
}

#[derive(Clone, Debug)]
pub(crate) enum DaemonSessionDelegationPort {
    Postgres(PostgresSessionDelegationPort),
    Unavailable,
}

impl DaemonSessionDelegationPort {
    pub(crate) fn postgres(pool: PgPool) -> Self {
        Self::Postgres(PostgresSessionDelegationPort::new(pool))
    }

    pub(crate) const fn unavailable() -> Self {
        Self::Unavailable
    }
}

impl PostgresSessionDelegationPort {
    /// Shares the daemon pool with atomic delegation tool transactions.
    pub fn new(pool: PgPool) -> Self {
        Self {
            repository: SessionDelegationRepository::new(pool),
        }
    }
}

/// Sanitized failure before a trustworthy delegation receipt exists.
#[derive(Debug)]
pub enum PostgresSessionDelegationPortError {
    Repository(SessionDelegationRepositoryError),
    Contract,
}

impl fmt::Display for PostgresSessionDelegationPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Repository(_) => "session-delegation persistence failed",
            Self::Contract => "session-delegation receipt correlation failed",
        })
    }
}

impl Error for PostgresSessionDelegationPortError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Repository(error) => Some(error),
            Self::Contract => None,
        }
    }
}

impl ClassifyOperatorFailure for PostgresSessionDelegationPortError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Repository(SessionDelegationRepositoryError::Database(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                }
            }
            Self::Repository(SessionDelegationRepositoryError::CommitAmbiguous(_)) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
            Self::Repository(SessionDelegationRepositoryError::ToolLoop(error)) => {
                error.operator_failure_class()
            }
            Self::Repository(SessionDelegationRepositoryError::Corruption(_)) => {
                OperatorFailureClass::FailClosedCorruption
            }
            Self::Repository(SessionDelegationRepositoryError::InvalidTransition(_))
            | Self::Contract => OperatorFailureClass::CallerOrHubBug,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Repository(_) => "session_delegation_persistence",
            Self::Contract => "session_delegation_receipt_correlation",
        }
    }
}

impl From<SessionDelegationRepositoryError> for PostgresSessionDelegationPortError {
    fn from(error: SessionDelegationRepositoryError) -> Self {
        Self::Repository(error)
    }
}

impl SessionDelegationPort for PostgresSessionDelegationPort {
    type Error = PostgresSessionDelegationPortError;

    async fn spawn_session(
        &mut self,
        _request: DelegatedSpawnRequest,
        _dispatch: ToolDispatchAuthority,
    ) -> Result<SessionDelegationPortOutcome<SpawnSessionReceipt>, Self::Error> {
        // Delegated creation remains fail-closed until the placement-owned
        // creation transaction supplies the child's decided placement proof.
        Ok(SessionDelegationPortOutcome::Rejected)
    }

    async fn await_session(
        &mut self,
        request: DelegationAwaitRequest,
        dispatch: ToolDispatchAuthority,
    ) -> Result<AwaitSessionPortOutcome, Self::Error> {
        let retained = request.clone();
        match self.repository.record_wait(request, &dispatch).await? {
            RecordDelegationWaitOutcome::Recorded(recorded) => {
                let wait = recorded.wait();
                match wait.mode() {
                    DelegationWaitMode::Background => {
                        AwaitSessionReceipt::from_wait(&retained, wait)
                            .map(AwaitSessionPortOutcome::BackgroundRegistered)
                            .ok_or(PostgresSessionDelegationPortError::Contract)
                    }
                    DelegationWaitMode::Foreground => {
                        Ok(AwaitSessionPortOutcome::ForegroundPending(wait))
                    }
                }
            }
            RecordDelegationWaitOutcome::Rejected(_) => Ok(AwaitSessionPortOutcome::Rejected),
        }
    }

    async fn send_session_message(
        &mut self,
        request: DelegationMessageRequest,
        dispatch: ToolDispatchAuthority,
    ) -> Result<SessionDelegationPortOutcome<SessionMessageReceipt>, Self::Error> {
        loop {
            let message = DelegationMessageId::from_uuid(uuid::Uuid::now_v7());
            match self
                .repository
                .record_message(request.clone(), message, &dispatch)
                .await?
            {
                RecordDelegationMessageOutcome::Recorded(recorded) => {
                    let receipt = SessionMessageReceipt::from_relation_event(
                        &request,
                        recorded.relation(),
                        recorded.event(),
                        recorded.delivery_sequence(),
                    )
                    .ok_or(PostgresSessionDelegationPortError::Contract)?;
                    return Ok(SessionDelegationPortOutcome::Applied(receipt));
                }
                RecordDelegationMessageOutcome::Rejected(
                    DelegationOperationRejection::MessageIdentityCollision,
                ) => {}
                RecordDelegationMessageOutcome::Rejected(_) => {
                    return Ok(SessionDelegationPortOutcome::Rejected);
                }
            }
        }
    }
}

impl SessionDelegationPort for DaemonSessionDelegationPort {
    type Error = PostgresSessionDelegationPortError;

    async fn spawn_session(
        &mut self,
        request: DelegatedSpawnRequest,
        dispatch: ToolDispatchAuthority,
    ) -> Result<SessionDelegationPortOutcome<SpawnSessionReceipt>, Self::Error> {
        match self {
            Self::Postgres(port) => port.spawn_session(request, dispatch).await,
            Self::Unavailable => Ok(SessionDelegationPortOutcome::Rejected),
        }
    }

    async fn await_session(
        &mut self,
        request: DelegationAwaitRequest,
        dispatch: ToolDispatchAuthority,
    ) -> Result<AwaitSessionPortOutcome, Self::Error> {
        match self {
            Self::Postgres(port) => port.await_session(request, dispatch).await,
            Self::Unavailable => Ok(AwaitSessionPortOutcome::Rejected),
        }
    }

    async fn send_session_message(
        &mut self,
        request: DelegationMessageRequest,
        dispatch: ToolDispatchAuthority,
    ) -> Result<SessionDelegationPortOutcome<SessionMessageReceipt>, Self::Error> {
        match self {
            Self::Postgres(port) => port.send_session_message(request, dispatch).await,
            Self::Unavailable => Ok(SessionDelegationPortOutcome::Rejected),
        }
    }
}
