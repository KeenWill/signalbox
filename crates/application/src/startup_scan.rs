//! Prior-process active-turn recovery before runtime scheduling.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md requires the inventory scan to
//! finish before the scheduler starts. docs/spec/sessions-and-transcript.md
//! owns the failed marker and terminal frontier. Prior-process nonterminal
//! attempts end as Lost.

use std::future::Future;

use signalbox_domain::{
    AcceptedInputId, AcceptedInputTurnFailureIdentities, ContextFrontierId,
    FailedAcceptedInputTurn, ModelCallDisposition, ModelCallId, ModelCallTerminalOutcome,
    SemanticTranscriptEntryId, SessionId, ToolAttemptCrashOutcome, TurnId,
};

use crate::{ClassifyOperatorFailure, OperatorFailureClass};

/// Application effect supplying fresh startup-recovery identities.
pub trait StartupScanIdGenerator {
    /// Generates one `TurnFailed` semantic-entry identity.
    fn next_failure_entry_id(&mut self) -> SemanticTranscriptEntryId;

    /// Generates one terminal context-frontier identity.
    fn next_terminal_frontier_id(&mut self) -> ContextFrontierId;

    /// Generates one proposal-order `ToolClosed` semantic-entry identity.
    fn next_tool_closure_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.next_failure_entry_id()
    }

    /// Generates the yielded-plus-tool-closures frontier identity.
    fn next_tool_closure_frontier_id(&mut self) -> ContextFrontierId {
        self.next_terminal_frontier_id()
    }

    /// Generates one successor turn for a pending steering input reclassified
    /// by call-aware restart recovery.
    fn next_reclassified_turn_id(&mut self, accepted_input: AcceptedInputId) -> TurnId;
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

    fn next_reclassified_turn_id(&mut self, _accepted_input: AcceptedInputId) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Result of one authoritative session recovery transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupScanSessionOutcome {
    /// The inventory observation was stale or the session was already healed.
    NoActiveTurn,
    /// The abandoned attempt and logical turn terminalized atomically.
    Recovered(Box<FailedAcceptedInputTurn>),
    /// A durable model call received its call-aware restart classification.
    RecoveredModelCall(Box<ModelCallTerminalOutcome>),
    /// A dedicated compaction call and its command received exact restart
    /// classification without producing a summary.
    RecoveredContextCompaction {
        /// Recovered dedicated physical call.
        call: ModelCallId,
        /// `KnownFailed` for Prepared or `Ambiguous` for InFlight.
        disposition: ModelCallDisposition,
    },
    /// A live tool attempt received crash/effect-aware restart classification.
    RecoveredToolAttempt(Box<ToolAttemptCrashOutcome>),
    /// A crash left only ended tool attempts; ordinary tool orchestration can
    /// resume the durable batch without recovery mutation.
    ResumableToolBatch {
        /// Active turn whose result/next-attempt boundary is ready.
        turn: TurnId,
    },
    /// A durable unsent model call remains prepared for ordinary scheduling.
    ResumablePreparedModelCall {
        /// Active turn whose exact prepared call remains retryable.
        turn: TurnId,
    },
    /// A prior process already ended this turn's tenure and parked it on an
    /// exact ambiguity set. The scan has nothing left to classify; bounded
    /// runtime reconciliation or an operator decision can release the slot.
    AwaitingRecoveryDecision {
        /// The active turn holding the slot until reconciliation.
        turn: TurnId,
    },
}

/// Authoritative inventory and per-session transaction boundary.
pub trait StartupScanRepository {
    /// Adapter-specific infrastructure, integrity, or identity-collision
    /// failure.
    type Error: ClassifyOperatorFailure;

    /// Reads the finite startup inventory in deterministic order.
    fn sessions(&mut self) -> impl Future<Output = Result<Box<[SessionId]>, Self::Error>> + Send;

    /// Records a durable operator item for one failed reconstitution.
    fn record_corrupt_session(
        &mut self,
        session: SessionId,
        error: &Self::Error,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Locks and reconstitutes one session, then commits failure atomically.
    fn recover<Generator>(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnFailureIdentities,
        ids: &mut Generator,
    ) -> impl Future<Output = Result<StartupScanSessionOutcome, Self::Error>> + Send
    where
        Generator: StartupScanIdGenerator + Send;
}

/// Complete result of scanning the startup inventory once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupScanOutcome {
    recovered_turn_count: usize,
    skipped_corrupt_sessions: Box<[SessionId]>,
    awaiting_recovery_decision_sessions: Box<[SessionId]>,
}

impl StartupScanOutcome {
    /// Returns sessions excluded with durable reconstitution-failure evidence.
    pub fn skipped_corrupt_sessions(&self) -> &[SessionId] {
        &self.skipped_corrupt_sessions
    }

    /// Returns the number of abandoned turns terminalized or newly parked
    /// on model-call recovery by this scan.
    pub const fn recovered_turn_count(&self) -> usize {
        self.recovered_turn_count
    }

    /// Returns every session whose active turn holds the slot awaiting
    /// bounded runtime reconciliation.
    ///
    /// The scan cannot resolve these turns: their physical tenure has ended
    /// and the exact ambiguity set is durable, whether this scan classified
    /// the issued call or found the wait already parked. They do not block
    /// startup; newly created model-call waits also count as recovered. Only
    /// model-call waits with an automatic and eventual operator surface are
    /// reported.
    pub fn awaiting_recovery_decision_sessions(&self) -> &[SessionId] {
        &self.awaiting_recovery_decision_sessions
    }
}

#[derive(signalbox_derive::OperatorError)]
#[error(transparent)]
#[operator(delegate = repository_error, code = delegate)]
/// Repository failure annotated with the startup-scan aggregate scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupScanError<RepositoryError> {
    #[source]
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
    Generator: StartupScanIdGenerator + Send,
    Repository: StartupScanRepository,
{
    /// Scans the initial inventory and retries only fresh-identity collisions.
    ///
    /// Each session transaction independently rechecks authority under lock.
    /// Infrastructure failure stops startup. Corrupt sessions are parked individually;
    /// every other session continues through reconstitution.
    pub async fn execute(
        &mut self,
    ) -> Result<StartupScanOutcome, StartupScanError<Repository::Error>> {
        let sessions = self
            .repository
            .sessions()
            .await
            .map_err(StartupScanError::inventory)?;
        let mut recovered_turn_count = 0_usize;
        let mut skipped_corrupt_sessions = Vec::new();
        let mut awaiting_recovery_decision_sessions = Vec::new();

        for session in sessions {
            loop {
                let identities = AcceptedInputTurnFailureIdentities::new(
                    self.ids.next_failure_entry_id(),
                    self.ids.next_terminal_frontier_id(),
                );
                match self
                    .repository
                    .recover(session, identities, &mut self.ids)
                    .await
                {
                    Ok(StartupScanSessionOutcome::NoActiveTurn) => break,
                    Ok(StartupScanSessionOutcome::Recovered(_)) => {
                        recovered_turn_count += 1;
                        break;
                    }
                    Ok(StartupScanSessionOutcome::RecoveredModelCall(outcome)) => {
                        recovered_turn_count += 1;
                        if matches!(*outcome, ModelCallTerminalOutcome::AwaitingRecovery(_)) {
                            awaiting_recovery_decision_sessions.push(session);
                        }
                        break;
                    }
                    Ok(StartupScanSessionOutcome::RecoveredContextCompaction { .. }) => {
                        break;
                    }
                    Ok(StartupScanSessionOutcome::RecoveredToolAttempt(outcome)) => {
                        if matches!(*outcome, ToolAttemptCrashOutcome::KnownFailed(_)) {
                            recovered_turn_count += 1;
                        }
                        break;
                    }
                    Ok(
                        StartupScanSessionOutcome::ResumableToolBatch { .. }
                        | StartupScanSessionOutcome::ResumablePreparedModelCall { .. },
                    ) => break,
                    Ok(StartupScanSessionOutcome::AwaitingRecoveryDecision { .. }) => {
                        awaiting_recovery_decision_sessions.push(session);
                        break;
                    }
                    Err(error)
                        if error.operator_failure_class()
                            == OperatorFailureClass::IdentityCollision =>
                    {
                        continue;
                    }
                    Err(error)
                        if error.operator_failure_class()
                            == OperatorFailureClass::FailClosedCorruption =>
                    {
                        self.repository
                            .record_corrupt_session(session, &error)
                            .await
                            .map_err(|error| StartupScanError::recovery(session, error))?;
                        skipped_corrupt_sessions.push(session);
                        break;
                    }
                    Err(error) => return Err(StartupScanError::recovery(session, error)),
                }
            }
        }

        Ok(StartupScanOutcome {
            recovered_turn_count,
            skipped_corrupt_sessions: skipped_corrupt_sessions.into_boxed_slice(),
            awaiting_recovery_decision_sessions: awaiting_recovery_decision_sessions
                .into_boxed_slice(),
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

        fn next_reclassified_turn_id(&mut self, _accepted_input: AcceptedInputId) -> TurnId {
            self.calls += 1;
            self.next += 1;
            TurnId::from_uuid(Uuid::from_u128(self.next))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Collision,
        Infrastructure,
        Corruption,
    }

    impl ClassifyOperatorFailure for FakeError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            match self {
                Self::Corruption => OperatorFailureClass::FailClosedCorruption,
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
        parked: Vec<SessionId>,
    }

    impl StartupScanRepository for FakeRepository {
        type Error = FakeError;

        fn sessions(
            &mut self,
        ) -> impl Future<Output = Result<Box<[SessionId]>, Self::Error>> + Send {
            ready(self.inventory.take().expect("one inventory response"))
        }

        async fn record_corrupt_session(
            &mut self,
            session: SessionId,
            _error: &Self::Error,
        ) -> Result<(), Self::Error> {
            self.parked.push(session);
            Ok(())
        }

        fn recover<Generator>(
            &mut self,
            session: SessionId,
            _identities: AcceptedInputTurnFailureIdentities,
            _ids: &mut Generator,
        ) -> impl Future<Output = Result<StartupScanSessionOutcome, Self::Error>> + Send
        where
            Generator: StartupScanIdGenerator + Send,
        {
            self.observed.push(session);
            ready(self.responses.pop_front().expect("one recovery response"))
        }
    }

    #[test]
    fn startup_parks_corruption_and_reconstitutes_remaining_sessions() {
        let corrupt = session(1);
        let healthy = session(2);
        let repository = FakeRepository {
            inventory: Some(Ok(vec![corrupt, healthy].into_boxed_slice())),
            responses: VecDeque::from([
                Err(FakeError::Corruption),
                Ok(StartupScanSessionOutcome::NoActiveTurn),
            ]),
            observed: Vec::new(),
            parked: Vec::new(),
        };
        let mut scan = StartupScanService::new(FakeIds { next: 10, calls: 0 }, repository);
        let outcome = run_ready(scan.execute()).unwrap();
        assert_eq!(outcome.skipped_corrupt_sessions(), &[corrupt]);
        let (_, repository) = scan.into_parts();
        assert_eq!(repository.parked, [corrupt]);
        assert_eq!(repository.observed, [corrupt, healthy]);
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

    /// the finite startup inventory is handled once and an
    /// identity-collision retry receives fresh identities.
    #[test]
    fn retries_collision_and_scans_finite_inventory() {
        let first = session(1);
        let second = session(2);
        let repository = FakeRepository {
            inventory: Some(Ok(vec![first, second].into_boxed_slice())),
            responses: VecDeque::from([
                Err(FakeError::Collision),
                Ok(StartupScanSessionOutcome::NoActiveTurn),
                Ok(StartupScanSessionOutcome::NoActiveTurn),
            ]),
            observed: Vec::new(),
            parked: Vec::new(),
        };
        let mut service = StartupScanService::new(FakeIds { next: 10, calls: 0 }, repository);

        let outcome = run_ready(service.execute()).expect("scan succeeds");
        let (ids, repository) = service.into_parts();

        assert_eq!(ids.calls, 6);
        assert_eq!(repository.observed, vec![first, first, second]);
        assert_eq!(outcome.recovered_turn_count(), 0);
    }

    /// a turn parked for bounded reconciliation is neither counted as
    /// recovered nor hidden — it is reported and startup proceeds.
    #[test]
    fn reports_awaiting_recovery_decision_without_blocking_startup() {
        let parked = session(1);
        let healthy = session(2);
        let repository = FakeRepository {
            inventory: Some(Ok(vec![parked, healthy].into_boxed_slice())),
            responses: VecDeque::from([
                Ok(StartupScanSessionOutcome::AwaitingRecoveryDecision {
                    turn: TurnId::from_uuid(Uuid::from_u128(7)),
                }),
                Ok(StartupScanSessionOutcome::NoActiveTurn),
            ]),
            observed: Vec::new(),
            parked: Vec::new(),
        };
        let mut service = StartupScanService::new(FakeIds { next: 10, calls: 0 }, repository);

        let outcome = run_ready(service.execute()).expect("scan succeeds");

        assert_eq!(outcome.recovered_turn_count(), 0);
        assert_eq!(outcome.awaiting_recovery_decision_sessions(), &[parked]);
    }

    /// docs/spec/turn-lifecycle-and-scheduling.md: non-collision
    /// infrastructure failures stop startup.
    #[test]
    fn infrastructure_failure_is_not_retried() {
        let requested = session(1);
        let repository = FakeRepository {
            inventory: Some(Ok(vec![requested].into_boxed_slice())),
            responses: VecDeque::from([Err(FakeError::Infrastructure)]),
            observed: Vec::new(),
            parked: Vec::new(),
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
