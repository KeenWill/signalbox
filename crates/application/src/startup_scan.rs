//! Prior-process active-turn recovery before runtime scheduling.
//!
//! ADR-0010 and ADR-0044 require the inventory scan to finish before the
//! scheduler starts. ADR-0036 owns the failed marker and terminal frontier,
//! while INV-034 requires prior-process nonterminal attempts to end as Lost.

use std::{error::Error, fmt, future::Future};

use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnFailureIdentities, ContextFrontierId,
    FailedAcceptedInputTurn, SemanticTranscriptEntryId, SessionId,
};

use crate::{ClassifyOperatorFailure, OperatorFailureClass};

/// Application effect supplying fresh startup-recovery identities.
pub trait StartupScanIdGenerator {
    /// Generates one `TurnFailed` semantic-entry identity.
    fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId;

    /// Generates one terminal context-frontier identity.
    fn next_terminal_frontier_id(&mut self) -> ContextFrontierId;
}

/// Production UUIDv7 generator for startup-recovery identities.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7StartupScanIdGenerator;

impl StartupScanIdGenerator for UuidV7StartupScanIdGenerator {
    fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_terminal_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Result of one authoritative session recovery transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupScanSessionOutcome {
    /// The inventory observation was stale or the session was already healed.
    NoActiveTurn,
    /// The prior-process attempt and logical turn terminalized atomically.
    Recovered(Box<FailedAcceptedInputTurn>),
    /// Pending steering keeps the source turn active until reclassification.
    DeferredPendingSteering {
        /// The accepted input that visibly blocks terminalization.
        accepted_input: AcceptedInputId,
    },
}

/// Authoritative inventory and per-session transaction boundary.
pub trait StartupScanRepository {
    /// Adapter-specific infrastructure, integrity, or identity-collision
    /// failure.
    type Error: ClassifyOperatorFailure;

    /// Reads the finite startup inventory in deterministic order.
    fn active_sessions(
        &mut self,
    ) -> impl Future<Output = Result<Box<[SessionId]>, Self::Error>> + Send;

    /// Locks and reconstitutes one session, then commits failure atomically.
    fn recover(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> impl Future<Output = Result<StartupScanSessionOutcome, Self::Error>> + Send;
}

/// Complete result of scanning the startup inventory once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupScanOutcome {
    recovered_turn_count: usize,
    pending_steering_sessions: Box<[SessionId]>,
}

impl StartupScanOutcome {
    /// Returns the number of prior-process turns terminalized by this scan.
    pub const fn recovered_turn_count(&self) -> usize {
        self.recovered_turn_count
    }

    /// Returns every session whose pending steering visibly blocks startup.
    pub fn pending_steering_sessions(&self) -> &[SessionId] {
        &self.pending_steering_sessions
    }

    /// Returns whether startup may proceed to runtime scheduling.
    pub fn is_complete(&self) -> bool {
        self.pending_steering_sessions.is_empty()
    }
}

/// Repository failure annotated with the startup-scan aggregate scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupScanError<RepositoryError> {
    repository_error: RepositoryError,
    session: Option<SessionId>,
}

impl<RepositoryError> StartupScanError<RepositoryError> {
    const fn inventory(repository_error: RepositoryError) -> Self {
        Self {
            repository_error,
            session: None,
        }
    }

    const fn recovery(session: SessionId, repository_error: RepositoryError) -> Self {
        Self {
            repository_error,
            session: Some(session),
        }
    }

    /// Returns the session scoped by a failed recovery transaction.
    ///
    /// Inventory failures occur before one session is selected and return
    /// `None`.
    pub const fn session(&self) -> Option<SessionId> {
        self.session
    }

    /// Returns the adapter-specific failure without discarding its detail.
    pub const fn repository_error(&self) -> &RepositoryError {
        &self.repository_error
    }

    /// Consumes the scan annotation and returns the adapter-specific failure.
    pub fn into_repository_error(self) -> RepositoryError {
        self.repository_error
    }
}

impl<RepositoryError> fmt::Display for StartupScanError<RepositoryError>
where
    RepositoryError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.repository_error.fmt(formatter)
    }
}

impl<RepositoryError> Error for StartupScanError<RepositoryError>
where
    RepositoryError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.repository_error)
    }
}

impl<RepositoryError> ClassifyOperatorFailure for StartupScanError<RepositoryError>
where
    RepositoryError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.repository_error.operator_failure_class()
    }
}

/// Coordinates one finite, idempotent startup recovery scan.
#[derive(Clone, Debug)]
pub struct StartupScanService<Generator, Repository> {
    ids: Generator,
    repository: Repository,
}

impl<Generator, Repository> StartupScanService<Generator, Repository> {
    /// Composes identity generation with the authoritative repository.
    pub const fn new(ids: Generator, repository: Repository) -> Self {
        Self { ids, repository }
    }

    /// Returns both ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (Generator, Repository) {
        (self.ids, self.repository)
    }
}

impl<Generator, Repository> StartupScanService<Generator, Repository>
where
    Generator: StartupScanIdGenerator,
    Repository: StartupScanRepository,
{
    /// Scans the initial inventory and retries only fresh-identity collisions.
    ///
    /// Each session transaction independently rechecks authority under lock.
    /// A crash or ambiguous infrastructure failure stops startup; a later
    /// invocation safely inventories only work still active.
    pub async fn execute(
        &mut self,
    ) -> Result<StartupScanOutcome, StartupScanError<Repository::Error>> {
        let sessions = self
            .repository
            .active_sessions()
            .await
            .map_err(StartupScanError::inventory)?;
        let mut recovered_turn_count = 0_usize;
        let mut pending_steering_sessions = Vec::new();

        for session in sessions {
            loop {
                let identities = AcceptedInputTurnFailureIdentities::new(
                    self.ids.next_failure_entry_id(),
                    self.ids.next_terminal_frontier_id(),
                );
                match self.repository.recover(session, identities).await {
                    Ok(StartupScanSessionOutcome::NoActiveTurn) => break,
                    Ok(StartupScanSessionOutcome::Recovered(_)) => {
                        recovered_turn_count += 1;
                        break;
                    }
                    Ok(StartupScanSessionOutcome::DeferredPendingSteering { .. }) => {
                        pending_steering_sessions.push(session);
                        break;
                    }
                    Err(error)
                        if error.operator_failure_class()
                            == OperatorFailureClass::IdentityCollision =>
                    {
                        continue;
                    }
                    Err(error) => return Err(StartupScanError::recovery(session, error)),
                }
            }
        }

        Ok(StartupScanOutcome {
            recovered_turn_count,
            pending_steering_sessions: pending_steering_sessions.into_boxed_slice(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{Future, ready},
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use uuid::Uuid;

    use super::*;

    fn session(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn accepted_input(value: u128) -> AcceptedInputId {
        AcceptedInputId::from_uuid(Uuid::from_u128(value))
    }

    #[derive(Debug)]
    struct FakeIds {
        next: u128,
        calls: usize,
    }

    impl StartupScanIdGenerator for FakeIds {
        fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId {
            self.calls += 1;
            self.next += 1;
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(self.next))
        }

        fn next_terminal_frontier_id(&mut self) -> ContextFrontierId {
            self.calls += 1;
            self.next += 1;
            ContextFrontierId::from_uuid(Uuid::from_u128(self.next))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Collision,
        Infrastructure,
    }

    impl ClassifyOperatorFailure for FakeError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            match self {
                Self::Collision => OperatorFailureClass::IdentityCollision,
                Self::Infrastructure => OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
            }
        }
    }

    #[derive(Debug)]
    struct FakeRepository {
        inventory: Option<Result<Box<[SessionId]>, FakeError>>,
        responses: VecDeque<Result<StartupScanSessionOutcome, FakeError>>,
        observed: Vec<SessionId>,
    }

    impl StartupScanRepository for FakeRepository {
        type Error = FakeError;

        fn active_sessions(
            &mut self,
        ) -> impl Future<Output = Result<Box<[SessionId]>, Self::Error>> + Send {
            ready(self.inventory.take().expect("one inventory response"))
        }

        fn recover(
            &mut self,
            session: SessionId,
            _identities: AcceptedInputTurnFailureIdentities,
        ) -> impl Future<Output = Result<StartupScanSessionOutcome, Self::Error>> + Send {
            self.observed.push(session);
            ready(self.responses.pop_front().expect("one recovery response"))
        }
    }

    fn run_ready<Output>(future: impl Future<Output = Output>) -> Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("fake-backed use case must be immediately ready"),
        }
    }

    /// INV-034: the finite startup inventory is handled once, collision gets
    /// fresh identities, and pending steering remains a visible blocker.
    #[test]
    fn inv034_retries_collision_and_reports_pending_steering() {
        let first = session(1);
        let second = session(2);
        let repository = FakeRepository {
            inventory: Some(Ok(vec![first, second].into_boxed_slice())),
            responses: VecDeque::from([
                Err(FakeError::Collision),
                Ok(StartupScanSessionOutcome::NoActiveTurn),
                Ok(StartupScanSessionOutcome::DeferredPendingSteering {
                    accepted_input: accepted_input(3),
                }),
            ]),
            observed: Vec::new(),
        };
        let mut service = StartupScanService::new(FakeIds { next: 10, calls: 0 }, repository);

        let outcome = run_ready(service.execute()).expect("scan succeeds");
        let (ids, repository) = service.into_parts();

        assert_eq!(ids.calls, 6);
        assert_eq!(repository.observed, vec![first, first, second]);
        assert_eq!(outcome.recovered_turn_count(), 0);
        assert_eq!(outcome.pending_steering_sessions(), &[second]);
        assert!(!outcome.is_complete());
    }

    /// ADR-0044: non-collision infrastructure failures stop startup.
    #[test]
    fn infrastructure_failure_is_not_retried() {
        let requested = session(1);
        let repository = FakeRepository {
            inventory: Some(Ok(vec![requested].into_boxed_slice())),
            responses: VecDeque::from([Err(FakeError::Infrastructure)]),
            observed: Vec::new(),
        };
        let mut service = StartupScanService::new(FakeIds { next: 10, calls: 0 }, repository);

        let error = run_ready(service.execute()).expect_err("infrastructure stops the scan");
        assert_eq!(error.session(), Some(requested));
        assert_eq!(error.repository_error(), &FakeError::Infrastructure);
        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false
            }
        );
        let (ids, repository) = service.into_parts();
        assert_eq!(ids.calls, 2);
        assert_eq!(repository.observed, vec![requested]);
    }
}
