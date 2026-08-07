//! Daemon adapter for atomic delegated-session await and message effects.

use std::{error::Error, fmt, future::Future, time::Duration};

use signalbox_application::{ClassifyOperatorFailure, OperatorFailureClass};
use signalbox_domain::{
    DelegatedSpawnRequest, DelegationAwaitRequest, DelegationMessageId, DelegationMessageRequest,
    DelegationWait, DelegationWaitMode, SessionId, ToolDispatchAuthority, ToolRequestId, TurnId,
};
use signalbox_persistence::session_delegation::{
    ProcessDelegationOutcome, RecordDelegationMessageOutcome, RecordDelegationWaitOutcome,
    RecordedDelegationMessage, SessionDelegationRepository, SessionDelegationRepositoryError,
};
use signalbox_tools_sessions::{
    AwaitSessionPortOutcome, AwaitSessionReceipt, DeliveredChildResult, SessionDelegationPort,
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

    async fn project_wait(
        &self,
        request: &DelegationAwaitRequest,
        outcome: RecordDelegationWaitOutcome,
    ) -> Result<AwaitSessionPortOutcome, PostgresSessionDelegationPortError> {
        let recorded = match outcome {
            RecordDelegationWaitOutcome::Recorded(recorded) => recorded,
            RecordDelegationWaitOutcome::Rejected(_) => {
                return Ok(AwaitSessionPortOutcome::Rejected);
            }
        };
        let wait = recorded.wait();
        match wait.mode() {
            DelegationWaitMode::Background => AwaitSessionReceipt::from_wait(request, wait)
                .map(AwaitSessionPortOutcome::BackgroundRegistered)
                .ok_or(PostgresSessionDelegationPortError::Contract),
            DelegationWaitMode::Foreground => Ok(
                match recover_foreground_delivery_after_commit({
                    let delivery = self.load_foreground_delivery(wait).await;
                    if let Err(error) = &delivery {
                        tracing::error!(
                            diagnostic = "delegation_foreground_delivery_reread_failed",
                            cause_code = error.operator_failure_cause_code(),
                            session_id = %request.request().session().as_uuid(),
                            turn_id = %request.request().turn().as_uuid(),
                            "foreground delegation delivery reread failed after wait commit"
                        );
                    }
                    delivery
                }) {
                    ForegroundDeliveryProjection::Delivered(delivered) => {
                        AwaitSessionPortOutcome::Delivered(delivered)
                    }
                    ForegroundDeliveryProjection::Pending => {
                        AwaitSessionPortOutcome::ForegroundPending(wait)
                    }
                },
            ),
        }
    }

    pub(crate) async fn await_process_session(
        &self,
        session: SessionId,
        turn: TurnId,
        request: ToolRequestId,
        child: SessionId,
        mode: DelegationWaitMode,
    ) -> Result<ProcessDelegationOutcome<AwaitSessionPortOutcome>, PostgresSessionDelegationPortError>
    {
        match self
            .repository
            .record_process_wait(session, turn, request, child, mode)
            .await?
        {
            ProcessDelegationOutcome::Applied((logical, recorded)) => self
                .project_wait(&logical, RecordDelegationWaitOutcome::Recorded(recorded))
                .await
                .map(ProcessDelegationOutcome::Applied),
            ProcessDelegationOutcome::InvalidRequest => {
                Ok(ProcessDelegationOutcome::InvalidRequest)
            }
            ProcessDelegationOutcome::Rejected(rejection) => {
                Ok(ProcessDelegationOutcome::Rejected(rejection))
            }
        }
    }

    pub(crate) async fn send_process_message(
        &self,
        session: SessionId,
        turn: TurnId,
        request: ToolRequestId,
        peer: SessionId,
        content: String,
    ) -> Result<ProcessDelegationOutcome<SessionMessageReceipt>, PostgresSessionDelegationPortError>
    {
        let message = DelegationMessageId::from_uuid(uuid::Uuid::now_v7());
        match self
            .repository
            .record_process_message(session, turn, request, peer, content, message)
            .await?
        {
            ProcessDelegationOutcome::Applied((logical, recorded)) => {
                let receipt = SessionMessageReceipt::from_delivery(&logical, recorded.as_ref())
                    .ok_or(PostgresSessionDelegationPortError::Contract)?;
                Ok(ProcessDelegationOutcome::Applied(receipt))
            }
            ProcessDelegationOutcome::InvalidRequest => {
                Ok(ProcessDelegationOutcome::InvalidRequest)
            }
            ProcessDelegationOutcome::Rejected(rejection) => {
                Ok(ProcessDelegationOutcome::Rejected(rejection))
            }
        }
    }

    pub(crate) async fn load_foreground_delivery(
        &self,
        wait: DelegationWait,
    ) -> Result<Option<DeliveredChildResult>, PostgresSessionDelegationPortError> {
        self.repository
            .load_foreground_delivery(wait)
            .await?
            .map(|delivery| {
                DeliveredChildResult::from_stored_outcome(wait, delivery.outcome().clone())
                    .ok_or(PostgresSessionDelegationPortError::Contract)
            })
            .transpose()
    }
}

enum ForegroundDeliveryProjection<T> {
    Delivered(T),
    Pending,
}

#[cfg(test)]
impl<T> ForegroundDeliveryProjection<T> {
    const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

fn recover_foreground_delivery_after_commit<T, E>(
    delivered: Result<Option<T>, E>,
) -> ForegroundDeliveryProjection<T> {
    match delivered {
        Ok(Some(delivered)) => ForegroundDeliveryProjection::Delivered(delivered),
        Ok(None) | Err(_) => ForegroundDeliveryProjection::Pending,
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
    type MessageDelivery = RecordedDelegationMessage;

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
        let first = self
            .repository
            .record_wait(request.clone(), &dispatch)
            .await;
        let outcome = reconcile_commit_ambiguous(first, || {
            self.repository.record_wait(request.clone(), &dispatch)
        })
        .await?;
        self.project_wait(&request, outcome).await
    }

    async fn send_session_message(
        &mut self,
        request: DelegationMessageRequest,
        dispatch: ToolDispatchAuthority,
    ) -> Result<SessionDelegationPortOutcome<Self::MessageDelivery>, Self::Error> {
        let message = DelegationMessageId::from_uuid(uuid::Uuid::now_v7());
        let first = self
            .repository
            .record_message(request.clone(), message, &dispatch)
            .await;
        match reconcile_commit_ambiguous(first, || {
            self.repository
                .record_message(request.clone(), message, &dispatch)
        })
        .await?
        {
            RecordDelegationMessageOutcome::Recorded(recorded) => {
                Ok(SessionDelegationPortOutcome::Applied(*recorded))
            }
            RecordDelegationMessageOutcome::Rejected(_) => {
                Ok(SessionDelegationPortOutcome::Rejected)
            }
            RecordDelegationMessageOutcome::DurablyRejected(_) => {
                Ok(SessionDelegationPortOutcome::DurablyRejected)
            }
        }
    }
}

async fn reconcile_commit_ambiguous<T, Retry, RetryFuture>(
    first: Result<T, SessionDelegationRepositoryError>,
    mut retry: Retry,
) -> Result<T, SessionDelegationRepositoryError>
where
    Retry: FnMut() -> RetryFuture,
    RetryFuture: Future<Output = Result<T, SessionDelegationRepositoryError>>,
{
    match first {
        Err(SessionDelegationRepositoryError::CommitAmbiguous(_)) => {}
        decided => return decided,
    }
    let mut retry_delay = Duration::from_millis(10);
    loop {
        tokio::time::sleep(retry_delay).await;
        match retry().await {
            Err(
                SessionDelegationRepositoryError::CommitAmbiguous(_)
                | SessionDelegationRepositoryError::Database(_),
            ) => {
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(1));
            }
            decided => return decided,
        }
    }
}

impl SessionDelegationPort for DaemonSessionDelegationPort {
    type Error = PostgresSessionDelegationPortError;
    type MessageDelivery = RecordedDelegationMessage;

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
    ) -> Result<SessionDelegationPortOutcome<Self::MessageDelivery>, Self::Error> {
        match self {
            Self::Postgres(port) => port.send_session_message(request, dispatch).await,
            Self::Unavailable => Ok(SessionDelegationPortOutcome::Rejected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn commit_ambiguous_delegation_effect_retries_its_exact_replay() {
        let mut retries = [
            Err(SessionDelegationRepositoryError::CommitAmbiguous(
                sqlx::Error::PoolClosed,
            )),
            Err(SessionDelegationRepositoryError::Database(
                sqlx::Error::PoolClosed,
            )),
            Ok(7),
        ]
        .into_iter();
        let reconciled = reconcile_commit_ambiguous::<u8, _, _>(
            Err(SessionDelegationRepositoryError::CommitAmbiguous(
                sqlx::Error::PoolClosed,
            )),
            || {
                std::future::ready(
                    retries
                        .next()
                        .expect("the fixture supplies each ambiguous replay"),
                )
            },
        )
        .await
        .expect("the immutable replay resolves the ambiguous commit");

        assert_eq!(reconciled, 7);
    }

    #[tokio::test]
    async fn decided_delegation_effect_does_not_run_reconciliation() {
        let decided = reconcile_commit_ambiguous::<u8, _, _>(Ok(11), || async {
            panic!("a decided commit must not be replayed")
        })
        .await
        .expect("the decided result is preserved");

        assert_eq!(decided, 11);
    }

    #[tokio::test]
    async fn reconciliation_backoff_remains_cancellation_safe() {
        let retries = Arc::new(AtomicUsize::new(0));
        let observed_retries = Arc::clone(&retries);
        let outcome = tokio::time::timeout(
            Duration::from_millis(25),
            reconcile_commit_ambiguous::<u8, _, _>(
                Err(SessionDelegationRepositoryError::CommitAmbiguous(
                    sqlx::Error::PoolClosed,
                )),
                move || {
                    observed_retries.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Err(SessionDelegationRepositoryError::Database(
                        sqlx::Error::PoolClosed,
                    )))
                },
            ),
        )
        .await;

        assert!(outcome.is_err());
        assert_eq!(retries.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn committed_foreground_wait_survives_follow_up_read_failure() {
        assert!(recover_foreground_delivery_after_commit::<u8, ()>(Err(())).is_pending());
    }
}
