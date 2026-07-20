//! Eligibility-time accepted-input turn activation orchestration.
//!
//! ADR-0033 owns hub-minted identity supply, while ADR-0004, ADR-0010,
//! ADR-0027, ADR-0030, and ADR-0036 keep complete eligibility derivation and
//! atomic activation behind the authoritative transaction boundary.

use std::future::Future;

use signalbox_domain::{
    AcceptedInputTurnActivationIdentities, ActivatedAcceptedInputTurn, ContextFrontierId,
    SemanticTranscriptEntryId, SessionId, TurnAttemptId,
};

/// Application effect supplying fresh identities for one activation candidate.
///
/// Production implementations return distinct UUIDv7-backed values. UUID
/// timestamps are not queue order, eligibility, or lifecycle authority.
pub trait StartEligibleTurnIdGenerator {
    /// Generates the origin semantic-entry identity.
    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId;

    /// Generates the starting context-snapshot identity.
    fn next_starting_frontier_id(&mut self) -> ContextFrontierId;

    /// Generates the initial physical-attempt identity.
    fn next_initial_attempt_id(&mut self) -> TurnAttemptId;
}

/// Production UUIDv7 generator for activation candidate identities.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7StartEligibleTurnIdGenerator;

impl StartEligibleTurnIdGenerator for UuidV7StartEligibleTurnIdGenerator {
    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
        TurnAttemptId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// The closed committed result of one session eligibility pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartEligibleTurnOutcome {
    /// Authoritative state contained no turn eligible for activation.
    NoEligibleTurn,
    /// The earliest eligible accepted-input turn was atomically activated.
    Activated(Box<ActivatedAcceptedInputTurn>),
}

/// Atomic boundary for one session eligibility pass.
///
/// Implementations load the complete authoritative scheduling projection,
/// derive the earliest queued turn without accepting a caller-selected target,
/// prepare the domain activation, and atomically commit its origin entry,
/// starting snapshot, start binding, active slot, and initial prepared
/// attempt. A false or stale wake-up returns [`StartEligibleTurnOutcome::NoEligibleTurn`].
/// The application neither preloads state nor retries this port.
pub trait StartEligibleTurnTransaction {
    /// Adapter-specific infrastructure, integrity, or identity-collision
    /// failure.
    type Error;

    /// Runs one authoritative eligibility pass with fresh identity candidates.
    fn handle(
        &mut self,
        session: SessionId,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Self::Error>> + Send;
}

/// Coordinates one session's eligibility-time activation pass.
#[derive(Clone, Debug)]
pub struct StartEligibleTurnService<Generator, Transaction> {
    ids: Generator,
    transaction: Transaction,
}

impl<Generator, Transaction> StartEligibleTurnService<Generator, Transaction> {
    /// Composes the application identity and atomic transaction ports.
    pub const fn new(ids: Generator, transaction: Transaction) -> Self {
        Self { ids, transaction }
    }

    /// Returns both ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (Generator, Transaction) {
        (self.ids, self.transaction)
    }
}

impl<Generator, Transaction> StartEligibleTurnService<Generator, Transaction>
where
    Generator: StartEligibleTurnIdGenerator,
    Transaction: StartEligibleTurnTransaction,
{
    /// Runs one authoritative eligibility pass for `session`.
    ///
    /// The application supplies fresh candidates once and calls the atomic
    /// port once. It does not select a turn, preload scheduling state, retry,
    /// sweep, or perform runtime dispatch.
    pub async fn execute(
        &mut self,
        session: SessionId,
    ) -> Result<StartEligibleTurnOutcome, Transaction::Error> {
        let identities = AcceptedInputTurnActivationIdentities::new(
            self.ids.next_origin_entry_id(),
            self.ids.next_starting_frontier_id(),
            self.ids.next_initial_attempt_id(),
        );

        self.transaction.handle(session, identities).await
    }

    pub(crate) fn execute_with_cloned_transaction(
        &mut self,
        session: SessionId,
    ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Transaction::Error>> + Send + 'static
    where
        Transaction: Clone + Send + 'static,
        Transaction::Error: Send + 'static,
    {
        let identities = AcceptedInputTurnActivationIdentities::new(
            self.ids.next_origin_entry_id(),
            self.ids.next_starting_frontier_id(),
            self.ids.next_initial_attempt_id(),
        );
        let mut transaction = self.transaction.clone();
        async move { transaction.handle(session, identities).await }
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
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputTurnSchedulingRecord,
        AcceptedInputTurnSchedulingRecordState, DeliveryRequest, DirectModelSelection,
        ModelSelectionOverride, ModelSelectionRequest, OriginConfiguration,
        PerInputConfigurationChoices, Session, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionReconstitutionInput, TranscriptAncestry, TurnId,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        AcceptedInputTurnActivationIdentities, ContextFrontierId, SemanticTranscriptEntryId,
        SessionId, StartEligibleTurnIdGenerator, StartEligibleTurnOutcome,
        StartEligibleTurnService, StartEligibleTurnTransaction, TurnAttemptId,
        UuidV7StartEligibleTurnIdGenerator,
    };

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn turn_id(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }

    fn accepted_input_id(value: u128) -> signalbox_domain::AcceptedInputId {
        signalbox_domain::AcceptedInputId::from_uuid(Uuid::from_u128(value))
    }

    fn origin_entry_id(value: u128) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
    }

    fn frontier_id(value: u128) -> ContextFrontierId {
        ContextFrontierId::from_uuid(Uuid::from_u128(value))
    }

    fn attempt_id(value: u128) -> TurnAttemptId {
        TurnAttemptId::from_uuid(Uuid::from_u128(value))
    }

    fn current_session() -> Session {
        let session = session_id(1);
        let version = SessionConfigurationDefaultsVersion::first();
        SessionReconstitutionInput::new(
            session,
            session,
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            session,
            version,
            session,
            version,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::from_u128(2)),
            )),
        )
        .reconstitute()
        .expect("test session facts are fully correlated")
    }

    fn configuration(session: &Session) -> OriginConfiguration {
        let checked = session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            )
            .expect("the test request names current defaults");
        OriginConfiguration::freeze(checked, |_| None)
            .expect("the direct model selection does not consult aliases")
    }

    fn activated_turn() -> signalbox_domain::ActivatedAcceptedInputTurn {
        let session = current_session();
        let turn = turn_id(3);
        let record = AcceptedInputTurnSchedulingRecord::new(
            session.id(),
            turn,
            session.id(),
            AcceptedInputLifecycle::new(
                accepted_input_id(4),
                AcceptedInputDisposition::OriginOf(turn),
            ),
            session.id(),
            turn,
            AcceptedInputQueueOrder::ordinary(signalbox_domain::SessionInputPosition::first()),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            configuration(&session),
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let no_semantic_entries = Vec::new();
        let no_context_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let candidate = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![record],
            no_semantic_entries,
            no_context_snapshots,
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect("the test queued projection is complete")
        .prepare_earliest_queued_activation(AcceptedInputTurnActivationIdentities::new(
            origin_entry_id(5),
            frontier_id(6),
            attempt_id(7),
        ))
        .expect("the sole queued turn is eligible");

        candidate.into_parts().0
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
    struct FakeIds {
        origin_entries: VecDeque<SemanticTranscriptEntryId>,
        frontiers: VecDeque<ContextFrontierId>,
        attempts: VecDeque<TurnAttemptId>,
        origin_entry_calls: usize,
        frontier_calls: usize,
        attempt_calls: usize,
    }

    impl FakeIds {
        fn new(
            origin_entries: impl IntoIterator<Item = SemanticTranscriptEntryId>,
            frontiers: impl IntoIterator<Item = ContextFrontierId>,
            attempts: impl IntoIterator<Item = TurnAttemptId>,
        ) -> Self {
            Self {
                origin_entries: origin_entries.into_iter().collect(),
                frontiers: frontiers.into_iter().collect(),
                attempts: attempts.into_iter().collect(),
                origin_entry_calls: 0,
                frontier_calls: 0,
                attempt_calls: 0,
            }
        }

        /// Supplies one canonical candidate of each kind for tests that care
        /// only that one complete identity set is consumed.
        fn supplying_one_candidate_set() -> Self {
            Self::new([origin_entry_id(2)], [frontier_id(3)], [attempt_id(4)])
        }
    }

    impl StartEligibleTurnIdGenerator for FakeIds {
        fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
            self.origin_entry_calls += 1;
            self.origin_entries
                .pop_front()
                .expect("test must supply one origin-entry candidate per invocation")
        }

        fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
            self.frontier_calls += 1;
            self.frontiers
                .pop_front()
                .expect("test must supply one frontier candidate per invocation")
        }

        fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
            self.attempt_calls += 1;
            self.attempts
                .pop_front()
                .expect("test must supply one attempt candidate per invocation")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeTransactionError {
        Unavailable,
    }

    #[derive(Debug)]
    struct FakeTransaction {
        responses: VecDeque<Result<StartEligibleTurnOutcome, FakeTransactionError>>,
        observed: Vec<(SessionId, AcceptedInputTurnActivationIdentities)>,
    }

    impl FakeTransaction {
        fn returning(
            responses: impl IntoIterator<Item = Result<StartEligibleTurnOutcome, FakeTransactionError>>,
        ) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                observed: Vec::new(),
            }
        }
    }

    impl StartEligibleTurnTransaction for FakeTransaction {
        type Error = FakeTransactionError;

        fn handle(
            &mut self,
            session: SessionId,
            identities: AcceptedInputTurnActivationIdentities,
        ) -> impl Future<Output = Result<StartEligibleTurnOutcome, Self::Error>> + Send {
            self.observed.push((session, identities));
            ready(
                self.responses
                    .pop_front()
                    .expect("test must supply one response per invocation"),
            )
        }
    }

    #[track_caller]
    fn assert_uuid_v7_candidate(value: Uuid) {
        assert_eq!(value.get_variant(), Variant::RFC4122);
        assert_eq!(value.get_version(), Some(Version::SortRand));
        assert!(!value.is_nil());
        assert!(!value.is_max());
    }

    /// INV-001 / INV-002: each production activation identity is a fresh
    /// UUIDv7 value of its distinct domain kind without using UUID order as
    /// authority.
    #[test]
    fn inv001_inv002_production_generator_supplies_fresh_uuid_v7_candidates() {
        let mut generator = UuidV7StartEligibleTurnIdGenerator;
        let first_origin = generator.next_origin_entry_id();
        let first_frontier = generator.next_starting_frontier_id();
        let first_attempt = generator.next_initial_attempt_id();
        let second_origin = generator.next_origin_entry_id();
        let second_frontier = generator.next_starting_frontier_id();
        let second_attempt = generator.next_initial_attempt_id();

        let first_origin_uuid = first_origin.into_uuid();
        let first_frontier_uuid = first_frontier.into_uuid();
        let first_attempt_uuid = first_attempt.into_uuid();
        let second_origin_uuid = second_origin.into_uuid();
        let second_frontier_uuid = second_frontier.into_uuid();
        let second_attempt_uuid = second_attempt.into_uuid();

        assert_ne!(first_origin, second_origin);
        assert_ne!(first_frontier, second_frontier);
        assert_ne!(first_attempt, second_attempt);
        assert_ne!(first_origin_uuid, first_frontier_uuid);
        assert_ne!(first_origin_uuid, first_attempt_uuid);
        assert_ne!(first_origin_uuid, second_frontier_uuid);
        assert_ne!(first_origin_uuid, second_attempt_uuid);
        assert_ne!(first_frontier_uuid, first_attempt_uuid);
        assert_ne!(first_frontier_uuid, second_origin_uuid);
        assert_ne!(first_frontier_uuid, second_attempt_uuid);
        assert_ne!(first_attempt_uuid, second_origin_uuid);
        assert_ne!(first_attempt_uuid, second_frontier_uuid);
        assert_ne!(second_origin_uuid, second_frontier_uuid);
        assert_ne!(second_origin_uuid, second_attempt_uuid);
        assert_ne!(second_frontier_uuid, second_attempt_uuid);
        assert_uuid_v7_candidate(first_origin_uuid);
        assert_uuid_v7_candidate(first_frontier_uuid);
        assert_uuid_v7_candidate(first_attempt_uuid);
        assert_uuid_v7_candidate(second_origin_uuid);
        assert_uuid_v7_candidate(second_frontier_uuid);
        assert_uuid_v7_candidate(second_attempt_uuid);
    }

    /// S01 / INV-002 / INV-009: orchestration forwards one exact session and
    /// identity set to the atomic port without selecting a target turn.
    #[test]
    fn s01_inv002_inv009_forwards_one_exact_session_and_identity_set() {
        let session = session_id(1);
        let identities = AcceptedInputTurnActivationIdentities::new(
            origin_entry_id(2),
            frontier_id(3),
            attempt_id(4),
        );
        let expected = StartEligibleTurnOutcome::NoEligibleTurn;
        let mut service = StartEligibleTurnService::new(
            FakeIds::new(
                [identities.origin_entry()],
                [identities.starting_frontier()],
                [identities.initial_attempt()],
            ),
            FakeTransaction::returning([Ok(expected.clone())]),
        );

        assert_eq!(
            run_ready(service.execute(session)).expect("fake transaction succeeds"),
            expected
        );
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.origin_entry_calls, 1);
        assert_eq!(ids.frontier_calls, 1);
        assert_eq!(ids.attempt_calls, 1);
        assert_eq!(transaction.observed, vec![(session, identities)]);
    }

    /// S01 / INV-009: the committed activated-turn view returned by the
    /// transaction passes through without application reconstruction.
    #[test]
    fn s01_inv009_activated_outcome_passes_through_unchanged() {
        let activated = activated_turn();
        let expected = StartEligibleTurnOutcome::Activated(Box::new(activated));
        let mut service = StartEligibleTurnService::new(
            FakeIds::supplying_one_candidate_set(),
            FakeTransaction::returning([Ok(expected.clone())]),
        );

        assert_eq!(
            run_ready(service.execute(session_id(1))).expect("fake transaction succeeds"),
            expected
        );
        assert_eq!(service.into_parts().1.observed.len(), 1);
    }

    /// S03 / INV-009: a transaction failure remains nonterminal after one
    /// call; orchestration does not retry or fabricate an eligibility result.
    #[test]
    fn s03_inv009_transaction_failure_is_returned_without_retry() {
        let mut service = StartEligibleTurnService::new(
            FakeIds::supplying_one_candidate_set(),
            FakeTransaction::returning([Err(FakeTransactionError::Unavailable)]),
        );

        assert_eq!(
            run_ready(service.execute(session_id(1))),
            Err(FakeTransactionError::Unavailable)
        );
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.origin_entry_calls, 1);
        assert_eq!(ids.frontier_calls, 1);
        assert_eq!(ids.attempt_calls, 1);
        assert_eq!(transaction.observed.len(), 1);
    }
}
