//! Owner-initiated, no-ancestry session-creation orchestration.
//!
//! ADR-0033 owns identity supply, ADR-0034 owns durable-command replay, and
//! ADR-0038 keeps the recorded creation receipt distinct from a separately
//! loaded current [`signalbox_domain::Session`] snapshot.

use std::{error::Error, fmt, future::Future};

use signalbox_domain::{
    CreateSession as DomainCreateSession, CreateSessionAppliedResult,
    CreateSessionPreparationFailure, DurableCommandId, PreparedCreateSession,
    SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
    TranscriptAncestry,
};

/// Why a caller-supplied command identity cannot enter canonical construction.
///
/// ADR-0033 reserves nil and max UUIDs as invalid sentinel-like command
/// identities. Rejection occurs while constructing [`CreateSessionRequest`],
/// before session identity generation, domain command construction, or
/// durable-command lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidDurableCommandId {
    /// The supplied UUID contains all zero bits.
    Nil,
    /// The supplied UUID contains all one bits.
    Max,
}

impl fmt::Display for InvalidDurableCommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nil => formatter.write_str("nil durable command identity is reserved"),
            Self::Max => formatter.write_str("max durable command identity is reserved"),
        }
    }
}

impl Error for InvalidDurableCommandId {}

/// The complete admitted application request for owner-initiated creation.
///
/// The request deliberately has no cause or ancestry input: this slice fixes
/// them to `OwnerInitiated` and `None`. Its private fields ensure sentinel
/// command identities cannot reach the use case through this boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateSessionRequest {
    command_id: DurableCommandId,
    initial_configuration_defaults: SessionConfigurationDefaults,
}

impl CreateSessionRequest {
    /// Validates the caller-supplied identity before canonical construction.
    pub fn try_new(
        command_id: DurableCommandId,
        initial_configuration_defaults: SessionConfigurationDefaults,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command_id.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command_id.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }

        Ok(Self {
            command_id,
            initial_configuration_defaults,
        })
    }

    /// Returns the caller-supplied owner-global command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the complete initial model-selection defaults.
    pub const fn initial_configuration_defaults(&self) -> SessionConfigurationDefaults {
        self.initial_configuration_defaults
    }
}

/// Application effect that supplies a fresh hub-minted session identity.
///
/// Production implementations supply a distinct UUIDv7-backed value for each
/// invocation. The UUID timestamp is not domain order or authority.
pub trait SessionIdGenerator {
    /// Generates one candidate identity for the creation transition.
    fn next_session_id(&mut self) -> SessionId;
}

/// Production UUIDv7 session-identity generator.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7SessionIdGenerator;

impl SessionIdGenerator for UuidV7SessionIdGenerator {
    fn next_session_id(&mut self) -> SessionId {
        SessionId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// The terminal application result of handling one creation command.
///
/// `Applied` always carries the recorded typed receipt returned by the atomic
/// port. On equal replay it may therefore name a different session from the
/// fresh candidate generated for this invocation. A current `Session` is
/// deliberately not a result variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionOutcome {
    /// First application or equal replay returned the recorded receipt.
    Applied(CreateSessionAppliedResult),
    /// The command identity is already claimed by a different typed payload.
    ConflictingReuse {
        /// The owner-global identity whose existing meaning remains intact.
        command_id: DurableCommandId,
    },
}

/// Atomic persistence boundary for one prepared creation.
///
/// Implementations resolve first handling, equal replay, or conflicting reuse
/// and atomically commit a first handling before returning `Applied`.
/// Infrastructure failure claims no identifier. The application calls this
/// port exactly once and does not pre-load, retry, or reconstruct a receipt.
pub trait CreateSessionTransaction {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Handles one sealed candidate through the owner-global command boundary.
    fn handle(
        &mut self,
        prepared: PreparedCreateSession,
    ) -> impl Future<Output = Result<CreateSessionOutcome, Self::Error>> + Send;
}

/// A nonterminal orchestration failure.
#[derive(Debug, Eq, PartialEq)]
pub enum CreateSessionError<TransactionError> {
    /// The fixed baseline unexpectedly failed domain preparation.
    Preparation(CreateSessionPreparationFailure),
    /// The atomic transaction port could not complete.
    Transaction(TransactionError),
}

impl<TransactionError> fmt::Display for CreateSessionError<TransactionError>
where
    TransactionError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preparation(failure) => {
                write!(formatter, "CreateSession preparation failed: {failure:?}")
            }
            Self::Transaction(error) => {
                write!(formatter, "CreateSession transaction failed: {error}")
            }
        }
    }
}

impl<TransactionError> Error for CreateSessionError<TransactionError> where
    TransactionError: Error + 'static
{
}

/// Coordinates the admitted owner-initiated session-creation use case.
#[derive(Debug)]
pub struct CreateSessionService<Generator, Transaction> {
    session_ids: Generator,
    transaction: Transaction,
}

impl<Generator, Transaction> CreateSessionService<Generator, Transaction> {
    /// Composes the application identity and atomic transaction ports.
    pub const fn new(session_ids: Generator, transaction: Transaction) -> Self {
        Self {
            session_ids,
            transaction,
        }
    }

    /// Returns the ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (Generator, Transaction) {
        (self.session_ids, self.transaction)
    }
}

impl<Generator, Transaction> CreateSessionService<Generator, Transaction>
where
    Generator: SessionIdGenerator,
    Transaction: CreateSessionTransaction,
{
    /// Handles one owner-initiated, no-ancestry creation request.
    ///
    /// Each invocation generates a fresh candidate, including a retransmission
    /// after a lost acknowledgement. The atomic port remains authoritative:
    /// equal replay returns its original receipt instead of the new candidate.
    pub async fn execute(
        &mut self,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionOutcome, CreateSessionError<Transaction::Error>> {
        let candidate_session = self.session_ids.next_session_id();
        let command = DomainCreateSession::new(
            request.command_id,
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            request.initial_configuration_defaults,
        );
        let prepared = command
            .prepare(candidate_session)
            .map_err(|error| CreateSessionError::Preparation(error.failure()))?;

        self.transaction
            .handle(prepared)
            .await
            .map_err(CreateSessionError::Transaction)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        future::{Future, ready},
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use signalbox_domain::{
        CreateSession as DomainCreateSession, DirectModelSelection, ModelSelectionRequest,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        CreateSessionError, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
        CreateSessionTransaction, InvalidDurableCommandId, SessionConfigurationDefaults,
        SessionCreationCause, SessionId, SessionIdGenerator, TranscriptAncestry,
        UuidV7SessionIdGenerator,
    };

    fn command_id(value: u128) -> signalbox_domain::DurableCommandId {
        signalbox_domain::DurableCommandId::from_uuid(Uuid::from_u128(value))
    }

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(value)),
        ))
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

    #[derive(Debug)]
    struct FakeSessionIds {
        remaining: VecDeque<SessionId>,
        calls: usize,
    }

    impl FakeSessionIds {
        fn new(values: impl IntoIterator<Item = SessionId>) -> Self {
            Self {
                remaining: values.into_iter().collect(),
                calls: 0,
            }
        }
    }

    impl SessionIdGenerator for FakeSessionIds {
        fn next_session_id(&mut self) -> SessionId {
            self.calls += 1;
            self.remaining
                .pop_front()
                .expect("test must supply one identity per invocation")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeTransactionError {
        Unavailable,
    }

    impl std::fmt::Display for FakeTransactionError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("unavailable")
        }
    }

    impl std::error::Error for FakeTransactionError {}

    #[derive(Clone, Copy, Debug)]
    struct RecordedCreation {
        command: DomainCreateSession,
        result: signalbox_domain::CreateSessionAppliedResult,
    }

    #[derive(Debug, Default)]
    struct FakeTransaction {
        records: HashMap<signalbox_domain::DurableCommandId, RecordedCreation>,
        observed_candidates: Vec<SessionId>,
        calls: usize,
        fail_next: bool,
    }

    impl CreateSessionTransaction for FakeTransaction {
        type Error = FakeTransactionError;

        fn handle(
            &mut self,
            prepared: signalbox_domain::PreparedCreateSession,
        ) -> impl Future<Output = Result<CreateSessionOutcome, Self::Error>> + Send {
            self.calls += 1;
            self.observed_candidates.push(prepared.session().id());

            if std::mem::take(&mut self.fail_next) {
                return ready(Err(FakeTransactionError::Unavailable));
            }

            let command_id = prepared.command().command_id();
            let outcome = match self.records.get(&command_id) {
                Some(recorded) if recorded.command == *prepared.command() => {
                    CreateSessionOutcome::Applied(recorded.result)
                }
                Some(_) => CreateSessionOutcome::ConflictingReuse { command_id },
                None => {
                    let result = prepared.applied_result();
                    self.records.insert(
                        command_id,
                        RecordedCreation {
                            command: *prepared.command(),
                            result,
                        },
                    );
                    CreateSessionOutcome::Applied(result)
                }
            };
            ready(Ok(outcome))
        }
    }

    /// S01 / INV-001 / INV-012: sentinel command identities fail before
    /// canonical command construction and claim nothing.
    #[test]
    fn s01_inv001_inv012_request_rejects_reserved_command_identifiers() {
        assert_eq!(
            CreateSessionRequest::try_new(
                signalbox_domain::DurableCommandId::from_uuid(Uuid::nil()),
                defaults(1),
            ),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            CreateSessionRequest::try_new(
                signalbox_domain::DurableCommandId::from_uuid(Uuid::max()),
                defaults(1),
            ),
            Err(InvalidDurableCommandId::Max)
        );

        let non_v4 = command_id(1);
        assert_ne!(non_v4.as_uuid().get_version(), Some(Version::Random));
        assert!(
            CreateSessionRequest::try_new(non_v4, defaults(2)).is_ok(),
            "non-sentinel command UUID versions are accepted"
        );
    }

    /// S01 / INV-001 / INV-002: production session identities are fresh
    /// RFC-9562 UUIDv7 values, while their timestamp has no domain role.
    #[test]
    fn s01_inv001_inv002_production_generator_supplies_fresh_uuid_v7_sessions() {
        let mut generator = UuidV7SessionIdGenerator;
        let first = generator.next_session_id();
        let second = generator.next_session_id();

        assert_ne!(first, second);
        for session in [first, second] {
            assert_eq!(session.as_uuid().get_variant(), Variant::RFC4122);
            assert_eq!(session.as_uuid().get_version(), Some(Version::SortRand));
            assert!(!session.as_uuid().is_nil());
            assert!(!session.as_uuid().is_max());
        }
    }

    /// S01 / INV-003 / INV-008 / INV-012: orchestration fixes the admitted
    /// provenance, establishes defaults version one, and calls the atomic port
    /// exactly once with the sealed candidate.
    #[test]
    fn s01_inv003_inv008_inv012_orchestrates_one_atomic_creation() {
        let request = CreateSessionRequest::try_new(command_id(1), defaults(2))
            .expect("ordinary command identity is admitted");
        let candidate = session_id(3);
        let mut service =
            CreateSessionService::new(FakeSessionIds::new([candidate]), FakeTransaction::default());

        let outcome = run_ready(service.execute(request))
            .expect("fake transaction applies the first handling");
        let recorded_result = service
            .transaction
            .records
            .get(&request.command_id())
            .expect("one record is committed")
            .result;
        assert_eq!(outcome, CreateSessionOutcome::Applied(recorded_result));

        let (generator, transaction) = service.into_parts();
        assert_eq!(generator.calls, 1);
        assert_eq!(transaction.calls, 1);
        assert_eq!(transaction.observed_candidates, [candidate]);
        let recorded = transaction
            .records
            .get(&request.command_id())
            .expect("one record is committed");
        assert_eq!(recorded.result.session(), candidate);
        assert_eq!(
            recorded.command.provenance().cause(),
            SessionCreationCause::OwnerInitiated
        );
        assert_eq!(
            recorded.command.provenance().ancestry(),
            TranscriptAncestry::None
        );
        assert_eq!(
            recorded
                .command
                .establish_initial_defaults()
                .version()
                .as_u64(),
            1
        );
        assert_eq!(
            recorded.command.initial_configuration_defaults(),
            defaults(2)
        );
    }

    /// S01 / INV-012: equal replay returns the recorded receipt unchanged
    /// rather than the freshly generated candidate or a loaded Session.
    #[test]
    fn s01_inv012_equal_replay_returns_original_receipt() {
        let request = CreateSessionRequest::try_new(command_id(1), defaults(2))
            .expect("ordinary command identity is admitted");
        let winner = session_id(3);
        let replay_candidate = session_id(4);
        let mut service = CreateSessionService::new(
            FakeSessionIds::new([winner, replay_candidate]),
            FakeTransaction::default(),
        );

        let first = run_ready(service.execute(request)).expect("first invocation applies creation");
        let replay = run_ready(service.execute(request)).expect("equal replay succeeds");

        assert_eq!(first, replay);
        assert_eq!(
            replay,
            CreateSessionOutcome::Applied(
                service.transaction.records[&request.command_id()].result
            )
        );
        let CreateSessionOutcome::Applied(recorded_receipt) = replay else {
            panic!("equal replay must return the recorded applied receipt");
        };
        assert_eq!(recorded_receipt.session(), winner);
        assert_ne!(recorded_receipt.session(), replay_candidate);
        let (_, transaction) = service.into_parts();
        assert_eq!(transaction.calls, 2);
        assert_eq!(transaction.observed_candidates, [winner, replay_candidate]);
    }

    /// S01 / INV-012: reusing one command ID for different canonical defaults
    /// returns a typed conflict and never substitutes the second candidate.
    #[test]
    fn s01_inv012_conflicting_reuse_is_typed() {
        let command = command_id(1);
        let first = CreateSessionRequest::try_new(command, defaults(2))
            .expect("ordinary command identity is admitted");
        let conflicting = CreateSessionRequest::try_new(command, defaults(3))
            .expect("ordinary command identity is admitted");
        let mut service = CreateSessionService::new(
            FakeSessionIds::new([session_id(4), session_id(5)]),
            FakeTransaction::default(),
        );

        let _ = run_ready(service.execute(first)).expect("first invocation applies creation");
        let conflict =
            run_ready(service.execute(conflicting)).expect("typed conflict is a terminal outcome");

        assert_eq!(
            conflict,
            CreateSessionOutcome::ConflictingReuse {
                command_id: command
            }
        );
        let (_, transaction) = service.into_parts();
        assert_eq!(transaction.calls, 2);
        assert_eq!(transaction.records.len(), 1);
    }

    /// S01 / INV-012: application orchestration neither retries transaction
    /// failure nor fabricates a terminal command result.
    #[test]
    fn s01_inv012_transaction_failure_is_returned_without_retry() {
        let request = CreateSessionRequest::try_new(command_id(1), defaults(2))
            .expect("ordinary command identity is admitted");
        let transaction = FakeTransaction {
            fail_next: true,
            ..FakeTransaction::default()
        };
        let mut service =
            CreateSessionService::new(FakeSessionIds::new([session_id(3)]), transaction);

        let error = run_ready(service.execute(request))
            .expect_err("infrastructure failure cannot become a receipt");

        assert_eq!(
            error,
            CreateSessionError::Transaction(FakeTransactionError::Unavailable)
        );
        let (generator, transaction) = service.into_parts();
        assert_eq!(generator.calls, 1);
        assert_eq!(transaction.calls, 1);
        assert!(transaction.records.is_empty());
    }
}
