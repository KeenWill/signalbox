//! Session-defaults replacement orchestration.
//!
//! ADR-0027 admits the canonical compare-and-set command, ADR-0034 owns
//! owner-global replay, and the domain replacement boundary owns typed
//! applied-or-rejected results. This application slice constructs the
//! canonical command and delegates exactly once to an atomic transaction
//! port. Authoritative session loading and preparation remain inside that
//! transaction.

use std::future::Future;

use signalbox_domain::{
    DurableCommandId, ReplaceSessionDefaults as DomainReplaceSessionDefaults,
    ReplaceSessionDefaultsResult, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId,
};

use crate::InvalidDurableCommandId;

/// The complete validated application request for replacing session defaults.
///
/// Private fields ensure ADR-0033's nil and max command-identity sentinels
/// cannot enter canonical command construction through this boundary. The
/// request contains exactly the canonical command's caller-supplied fields and
/// no loaded session, current version, installed version, or persistence
/// representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplaceSessionDefaultsRequest {
    command_id: DurableCommandId,
    session: SessionId,
    expected_current_version: SessionConfigurationDefaultsVersion,
    replacement: SessionConfigurationDefaults,
}

impl ReplaceSessionDefaultsRequest {
    /// Validates the command identity before canonical construction.
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command_id.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command_id.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }

        Ok(Self {
            command_id,
            session,
            expected_current_version,
            replacement,
        })
    }

    /// Returns the owner-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the target session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the version the caller expects to be current.
    pub const fn expected_current_version(&self) -> SessionConfigurationDefaultsVersion {
        self.expected_current_version
    }

    /// Returns the complete replacement defaults.
    pub const fn replacement(&self) -> SessionConfigurationDefaults {
        self.replacement
    }
}

/// The closed application result of one atomic replacement handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceSessionDefaultsOutcome {
    /// First handling or equal replay returned the recorded domain result.
    ///
    /// The nested value remains exactly the domain's applied-or-rejected
    /// receipt; application orchestration does not recompute or reshape it.
    Recorded(ReplaceSessionDefaultsResult),
    /// The owner-global command identity already names another typed payload.
    ConflictingReuse {
        /// The identity whose existing meaning remains intact.
        command_id: DurableCommandId,
    },
}

/// Atomic command-handling boundary for session-defaults replacement.
///
/// Implementations look up owner-global command identity before current-state
/// validation. For unseen commands they load and prepare against authoritative
/// session state, compare-and-set the expected version, and atomically record
/// either the applied effects or typed rejection before returning `Recorded`.
/// Infrastructure failure claims no identifier. The application neither
/// preloads a [`signalbox_domain::Session`] nor retries this port.
pub trait ReplaceSessionDefaultsTransaction {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Handles one canonical command through the atomic persistence boundary.
    fn handle(
        &mut self,
        command: DomainReplaceSessionDefaults,
    ) -> impl Future<Output = Result<ReplaceSessionDefaultsOutcome, Self::Error>> + Send;
}

/// Coordinates the session-defaults replacement use case.
#[derive(Debug)]
pub struct ReplaceSessionDefaultsService<Transaction> {
    transaction: Transaction,
}

impl<Transaction> ReplaceSessionDefaultsService<Transaction> {
    /// Composes the use case with its atomic transaction port.
    pub const fn new(transaction: Transaction) -> Self {
        Self { transaction }
    }

    /// Returns the transaction port, primarily for explicit ownership handoff.
    pub fn into_transaction(self) -> Transaction {
        self.transaction
    }
}

impl<Transaction> ReplaceSessionDefaultsService<Transaction>
where
    Transaction: ReplaceSessionDefaultsTransaction,
{
    /// Constructs and handles one canonical replacement command.
    ///
    /// The command is constructed once and the atomic port is called exactly
    /// once. Recorded applied or rejected replay, conflicting reuse, and
    /// transaction failure are returned unchanged.
    pub async fn execute(
        &mut self,
        request: ReplaceSessionDefaultsRequest,
    ) -> Result<ReplaceSessionDefaultsOutcome, Transaction::Error> {
        let command = DomainReplaceSessionDefaults::new(
            request.command_id,
            request.session,
            request.expected_current_version,
            request.replacement,
        );
        self.transaction.handle(command).await
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

    use signalbox_domain::{
        DirectModelSelection, ModelSelectionRequest, ReplaceSessionDefaultsRejectedResult,
        ReplaceSessionDefaultsResult, Session, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionReconstitutionInput, TranscriptAncestry,
    };
    use uuid::Uuid;

    use super::{
        DomainReplaceSessionDefaults, DurableCommandId, InvalidDurableCommandId,
        ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest,
        ReplaceSessionDefaultsService, ReplaceSessionDefaultsTransaction, SessionId,
    };

    fn command_id(value: u128) -> DurableCommandId {
        DurableCommandId::from_uuid(Uuid::from_u128(value))
    }

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn version(value: u64) -> SessionConfigurationDefaultsVersion {
        SessionConfigurationDefaultsVersion::try_from_u64(value)
            .expect("test versions are positive")
    }

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(value)),
        ))
    }

    fn current_session(id: SessionId, current: u64) -> Session {
        SessionReconstitutionInput::new(
            id,
            id,
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            id,
            version(current),
            id,
            version(current),
            defaults(1),
        )
        .reconstitute()
        .expect("test facts form one complete current session")
    }

    fn result_for(
        command: DomainReplaceSessionDefaults,
        current: u64,
    ) -> ReplaceSessionDefaultsResult {
        command
            .prepare_against(&current_session(command.session(), current))
            .expect("test command and session identities match")
            .result()
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeTransactionError {
        Unavailable,
    }

    #[derive(Debug)]
    struct FakeTransaction {
        responses: VecDeque<Result<ReplaceSessionDefaultsOutcome, FakeTransactionError>>,
        observed: Vec<DomainReplaceSessionDefaults>,
    }

    impl FakeTransaction {
        fn returning(
            responses: impl IntoIterator<
                Item = Result<ReplaceSessionDefaultsOutcome, FakeTransactionError>,
            >,
        ) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                observed: Vec::new(),
            }
        }
    }

    impl ReplaceSessionDefaultsTransaction for FakeTransaction {
        type Error = FakeTransactionError;

        fn handle(
            &mut self,
            command: DomainReplaceSessionDefaults,
        ) -> impl Future<Output = Result<ReplaceSessionDefaultsOutcome, Self::Error>> + Send
        {
            self.observed.push(command);
            ready(
                self.responses
                    .pop_front()
                    .expect("test must supply one response per invocation"),
            )
        }
    }

    /// S01 / INV-001 / INV-012: reserved command identities fail before
    /// canonical command construction or any transaction call.
    #[test]
    fn s01_inv001_inv012_sentinel_rejection_calls_no_transaction() {
        let target = session_id(1);
        for (raw, expected) in [
            (Uuid::nil(), InvalidDurableCommandId::Nil),
            (Uuid::max(), InvalidDurableCommandId::Max),
        ] {
            assert_eq!(
                ReplaceSessionDefaultsRequest::try_new(
                    DurableCommandId::from_uuid(raw),
                    target,
                    version(1),
                    defaults(2),
                ),
                Err(expected)
            );
        }

        let service = ReplaceSessionDefaultsService::new(FakeTransaction::returning([]));
        assert!(service.into_transaction().observed.is_empty());
    }

    /// S01 / INV-002 / INV-008 / INV-012: orchestration forwards exactly the
    /// four-field canonical command and calls the atomic port once.
    #[test]
    fn s01_inv002_inv008_inv012_forwards_exact_command_once() {
        let request = ReplaceSessionDefaultsRequest::try_new(
            command_id(1),
            session_id(2),
            version(3),
            defaults(4),
        )
        .expect("ordinary command identity is admitted");
        let recorded = result_for(
            DomainReplaceSessionDefaults::new(
                request.command_id(),
                request.session(),
                request.expected_current_version(),
                request.replacement(),
            ),
            3,
        );
        let expected_outcome = ReplaceSessionDefaultsOutcome::Recorded(recorded);
        let mut service =
            ReplaceSessionDefaultsService::new(FakeTransaction::returning([Ok(expected_outcome)]));

        let outcome =
            run_ready(service.execute(request)).expect("fake transaction returns its result");

        assert_eq!(outcome, expected_outcome);
        let transaction = service.into_transaction();
        assert_eq!(transaction.observed.len(), 1);
        let observed = transaction.observed[0];
        assert_eq!(observed.command_id(), request.command_id());
        assert_eq!(observed.session(), request.session());
        assert_eq!(
            observed.expected_current_version(),
            request.expected_current_version()
        );
        assert_eq!(observed.replacement(), request.replacement());
    }

    /// S01 / INV-008 / INV-012: recorded applied and authoritative-rejected
    /// replay results pass through without recomputation or reshaping.
    #[test]
    fn s01_inv008_inv012_recorded_applied_and_rejected_results_pass_through() {
        let target = session_id(1);
        let applied_command =
            DomainReplaceSessionDefaults::new(command_id(1), target, version(1), defaults(2));
        let rejected_command =
            DomainReplaceSessionDefaults::new(command_id(2), target, version(1), defaults(3));
        let applied = result_for(applied_command, 1);
        let rejected = result_for(rejected_command, 2);
        assert!(matches!(applied, ReplaceSessionDefaultsResult::Applied(_)));
        assert!(matches!(
            rejected,
            ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(_)
            )
        ));

        for (command, recorded) in [(applied_command, applied), (rejected_command, rejected)] {
            let request = ReplaceSessionDefaultsRequest::try_new(
                command.command_id(),
                command.session(),
                command.expected_current_version(),
                command.replacement(),
            )
            .expect("ordinary command identity is admitted");
            let expected = ReplaceSessionDefaultsOutcome::Recorded(recorded);
            let mut service =
                ReplaceSessionDefaultsService::new(FakeTransaction::returning([Ok(expected)]));

            let actual =
                run_ready(service.execute(request)).expect("recorded replay result is returned");

            assert_eq!(actual, expected);
            assert_eq!(service.into_transaction().observed, [command]);
        }
    }

    /// S01 / INV-012: conflicting owner-global reuse is returned unchanged and
    /// does not acquire a replacement meaning in application code.
    #[test]
    fn s01_inv012_conflicting_reuse_is_returned_unchanged() {
        let request = ReplaceSessionDefaultsRequest::try_new(
            command_id(1),
            session_id(2),
            version(1),
            defaults(3),
        )
        .expect("ordinary command identity is admitted");
        let expected = ReplaceSessionDefaultsOutcome::ConflictingReuse {
            command_id: request.command_id(),
        };
        let mut service =
            ReplaceSessionDefaultsService::new(FakeTransaction::returning([Ok(expected)]));

        let actual = run_ready(service.execute(request)).expect("conflict is a terminal outcome");

        assert_eq!(actual, expected);
        assert_eq!(service.into_transaction().observed.len(), 1);
    }

    /// S01 / INV-012: a transaction failure is returned after one call; the
    /// application does not retry or reinterpret it as a terminal result.
    #[test]
    fn s01_inv012_transaction_failure_is_returned_without_retry() {
        let request = ReplaceSessionDefaultsRequest::try_new(
            command_id(1),
            session_id(2),
            version(1),
            defaults(3),
        )
        .expect("ordinary command identity is admitted");
        let mut service = ReplaceSessionDefaultsService::new(FakeTransaction::returning([Err(
            FakeTransactionError::Unavailable,
        )]));

        let error = run_ready(service.execute(request))
            .expect_err("transaction failure remains nonterminal");

        assert_eq!(error, FakeTransactionError::Unavailable);
        assert_eq!(service.into_transaction().observed.len(), 1);
    }
}
