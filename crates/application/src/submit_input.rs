//! Durable input-submission orchestration.
//!
//! ADR-0033 owns hub-minted identity supply, ADR-0034 owns owner-global
//! command replay, and ADR-0039 admits only the owner actor at the baseline
//! command boundary. Authoritative session loading, position allocation,
//! preparation, and recording remain inside one atomic transaction port.

use std::future::Future;

use signalbox_domain::{
    AcceptedInputId, Actor, DeliveryRequest, DurableCommandId, SessionId,
    SubmitInput as DomainSubmitInput, SubmitInputResult, TurnId, UserContent,
};

use crate::InvalidDurableCommandId;

/// The complete admitted application request for durable input submission.
///
/// Content is already a checked domain value. Private fields ensure ADR-0033's
/// nil and max command-identity sentinels cannot enter canonical command
/// construction through this boundary. The owner actor is fixed by the
/// service rather than accepted as caller input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitInputRequest {
    command_id: DurableCommandId,
    session: SessionId,
    content: UserContent,
    delivery: DeliveryRequest,
}

impl SubmitInputRequest {
    /// Validates the command identity before canonical construction.
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
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
            content,
            delivery,
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

    /// Borrows the exact checked user content.
    pub const fn content(&self) -> &UserContent {
        &self.content
    }

    /// Returns the caller's explicit delivery treatment.
    pub const fn delivery(&self) -> DeliveryRequest {
        self.delivery
    }
}

/// Application effect supplying fresh candidate identities for input handling.
///
/// Production implementations return distinct UUIDv7-backed values. A
/// candidate can remain unused when the transaction resolves replay or records
/// a rejection. UUID timestamps are not domain order or authority.
pub trait SubmitInputIdGenerator {
    /// Generates one candidate accepted-input identity.
    fn next_accepted_input_id(&mut self) -> AcceptedInputId;

    /// Generates one candidate future queued-work identity.
    fn next_turn_id(&mut self) -> TurnId;
}

/// Production UUIDv7 generator for input-handling candidate identities.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7SubmitInputIdGenerator;

impl SubmitInputIdGenerator for UuidV7SubmitInputIdGenerator {
    fn next_accepted_input_id(&mut self) -> AcceptedInputId {
        AcceptedInputId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_turn_id(&mut self) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// The closed application result of one atomic input-command handling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputOutcome {
    /// First handling or equal replay returned the recorded domain result.
    Recorded(SubmitInputResult),
    /// The owner-global command identity already names another typed payload.
    ConflictingReuse {
        /// The identity whose existing meaning remains intact.
        command_id: DurableCommandId,
    },
}

/// Atomic command-handling boundary for durable input submission.
///
/// Implementations look up the owner-global command identity before mutable
/// session validation. For an unseen command they load authoritative state,
/// allocate ordering, prepare, and atomically record the terminal result and
/// any accepted queued-work facts. Infrastructure failure claims no identity.
/// The application neither preloads state nor retries this port.
pub trait SubmitInputTransaction {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Handles one canonical command and its hub-minted identity candidates.
    fn handle(
        &mut self,
        command: DomainSubmitInput,
        accepted_input: AcceptedInputId,
        turn: TurnId,
    ) -> impl Future<Output = Result<SubmitInputOutcome, Self::Error>> + Send;
}

/// Coordinates the durable input-submission use case.
#[derive(Debug)]
pub struct SubmitInputService<Generator, Transaction> {
    ids: Generator,
    transaction: Transaction,
}

impl<Generator, Transaction> SubmitInputService<Generator, Transaction> {
    /// Composes the application identity and atomic transaction ports.
    pub const fn new(ids: Generator, transaction: Transaction) -> Self {
        Self { ids, transaction }
    }

    /// Returns both ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (Generator, Transaction) {
        (self.ids, self.transaction)
    }
}

impl<Generator, Transaction> SubmitInputService<Generator, Transaction>
where
    Generator: SubmitInputIdGenerator,
    Transaction: SubmitInputTransaction,
{
    /// Constructs and handles one owner-attributed input command.
    ///
    /// Each invocation creates fresh candidates, including retransmission
    /// after a lost acknowledgement. The atomic port remains authoritative:
    /// recorded replay returns the original result rather than these
    /// invocation-local candidates.
    pub async fn execute(
        &mut self,
        request: SubmitInputRequest,
    ) -> Result<SubmitInputOutcome, Transaction::Error> {
        let command = DomainSubmitInput::new(
            request.command_id,
            request.session,
            Actor::Owner,
            request.content,
            request.delivery,
        );
        let accepted_input = self.ids.next_accepted_input_id();
        let turn = self.ids.next_turn_id();

        self.transaction.handle(command, accepted_input, turn).await
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
        DirectModelSelection, ModelAlias, ModelSelectionOverride, ModelSelectionRequest,
        PerInputConfigurationChoices, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SubmitInputRejectedResult,
        TranscriptAncestry,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        AcceptedInputId, Actor, DeliveryRequest, DomainSubmitInput, DurableCommandId,
        InvalidDurableCommandId, SessionId, SubmitInputIdGenerator, SubmitInputOutcome,
        SubmitInputRequest, SubmitInputResult, SubmitInputService, SubmitInputTransaction, TurnId,
        UserContent, UuidV7SubmitInputIdGenerator,
    };

    fn command_id(value: u128) -> DurableCommandId {
        DurableCommandId::from_uuid(Uuid::from_u128(value))
    }

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn accepted_input_id(value: u128) -> AcceptedInputId {
        AcceptedInputId::from_uuid(Uuid::from_u128(value))
    }

    fn turn_id(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }

    fn version(value: u64) -> SessionConfigurationDefaultsVersion {
        SessionConfigurationDefaultsVersion::try_from_u64(value)
            .expect("test versions are positive")
    }

    fn direct(value: u128) -> ModelSelectionRequest {
        ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(value)))
    }

    fn content(value: &str) -> UserContent {
        UserContent::try_text(value.to_owned()).expect("test content is valid")
    }

    fn choices(expected: u64) -> PerInputConfigurationChoices {
        PerInputConfigurationChoices::new(
            version(expected),
            ModelSelectionOverride::UseSessionDefault,
        )
    }

    fn delivery(expected: u64) -> DeliveryRequest {
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(expected),
        }
    }

    fn request(command: u128, text: &str) -> SubmitInputRequest {
        SubmitInputRequest::try_new(
            command_id(command),
            session_id(2),
            content(text),
            delivery(1),
        )
        .expect("ordinary command identity is admitted")
    }

    fn current_session() -> signalbox_domain::Session {
        SessionReconstitutionInput::new(
            session_id(2),
            session_id(2),
            SessionCreationProvenance::new(
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            session_id(2),
            version(1),
            session_id(2),
            version(1),
            SessionConfigurationDefaults::new(direct(3)),
        )
        .reconstitute()
        .expect("test facts form one complete current session")
    }

    fn command_for(request: &SubmitInputRequest) -> DomainSubmitInput {
        DomainSubmitInput::new(
            request.command_id(),
            request.session(),
            Actor::Owner,
            request.content().clone(),
            request.delivery(),
        )
    }

    fn applied_result(
        request: &SubmitInputRequest,
        accepted_input: AcceptedInputId,
        turn: TurnId,
    ) -> SubmitInputResult {
        let (_, result) = command_for(request)
            .prepare_when_no_active_turn(&current_session(), accepted_input, turn, None, |_| None)
            .expect("the command target matches the current session")
            .into_parts();
        result
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
        accepted_inputs: VecDeque<AcceptedInputId>,
        turns: VecDeque<TurnId>,
        accepted_input_calls: usize,
        turn_calls: usize,
    }

    impl FakeIds {
        fn new(
            accepted_inputs: impl IntoIterator<Item = AcceptedInputId>,
            turns: impl IntoIterator<Item = TurnId>,
        ) -> Self {
            Self {
                accepted_inputs: accepted_inputs.into_iter().collect(),
                turns: turns.into_iter().collect(),
                accepted_input_calls: 0,
                turn_calls: 0,
            }
        }
    }

    impl SubmitInputIdGenerator for FakeIds {
        fn next_accepted_input_id(&mut self) -> AcceptedInputId {
            self.accepted_input_calls += 1;
            self.accepted_inputs
                .pop_front()
                .expect("test must supply one accepted-input candidate per invocation")
        }

        fn next_turn_id(&mut self) -> TurnId {
            self.turn_calls += 1;
            self.turns
                .pop_front()
                .expect("test must supply one turn candidate per invocation")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeTransactionError {
        Unavailable,
    }

    #[derive(Debug)]
    struct FakeTransaction {
        responses: VecDeque<Result<SubmitInputOutcome, FakeTransactionError>>,
        observed: Vec<(DomainSubmitInput, AcceptedInputId, TurnId)>,
    }

    impl FakeTransaction {
        fn returning(
            responses: impl IntoIterator<Item = Result<SubmitInputOutcome, FakeTransactionError>>,
        ) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                observed: Vec::new(),
            }
        }
    }

    impl SubmitInputTransaction for FakeTransaction {
        type Error = FakeTransactionError;

        fn handle(
            &mut self,
            command: DomainSubmitInput,
            accepted_input: AcceptedInputId,
            turn: TurnId,
        ) -> impl Future<Output = Result<SubmitInputOutcome, Self::Error>> + Send {
            self.observed.push((command, accepted_input, turn));
            ready(
                self.responses
                    .pop_front()
                    .expect("test must supply one response per invocation"),
            )
        }
    }

    /// S01 / INV-001 / INV-012: reserved command identities fail before
    /// canonical command construction or any application effect.
    #[test]
    fn s01_inv001_inv012_request_rejects_reserved_command_identifiers() {
        for (raw, expected) in [
            (Uuid::nil(), InvalidDurableCommandId::Nil),
            (Uuid::max(), InvalidDurableCommandId::Max),
        ] {
            assert_eq!(
                SubmitInputRequest::try_new(
                    DurableCommandId::from_uuid(raw),
                    session_id(2),
                    content("hello"),
                    delivery(1),
                ),
                Err(expected)
            );
        }

        let service = SubmitInputService::new(FakeIds::new([], []), FakeTransaction::returning([]));
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 0);
        assert_eq!(ids.turn_calls, 0);
        assert!(transaction.observed.is_empty());
    }

    /// S01 / INV-001 / INV-002: production candidates are fresh UUIDv7
    /// values of their distinct domain kinds without using UUID order.
    #[test]
    fn s01_inv001_inv002_production_generator_supplies_fresh_uuid_v7_candidates() {
        let mut generator = UuidV7SubmitInputIdGenerator;
        let first_input = generator.next_accepted_input_id();
        let first_turn = generator.next_turn_id();
        let second_input = generator.next_accepted_input_id();
        let second_turn = generator.next_turn_id();

        assert_ne!(first_input, second_input);
        assert_ne!(first_turn, second_turn);
        for value in [
            first_input.into_uuid(),
            first_turn.into_uuid(),
            second_input.into_uuid(),
            second_turn.into_uuid(),
        ] {
            assert_eq!(value.get_variant(), Variant::RFC4122);
            assert_eq!(value.get_version(), Some(Version::SortRand));
            assert!(!value.is_nil());
            assert!(!value.is_max());
        }
    }

    /// S01 / INV-002 / INV-007 / INV-008 / INV-012 / INV-028: orchestration
    /// fixes owner attribution and forwards one exact command and candidate
    /// pair to the atomic port.
    #[test]
    fn s01_inv002_inv007_inv008_inv012_inv028_orchestrates_one_owner_command_and_candidate_pair() {
        let request = request(1, "hello");
        let accepted_input = accepted_input_id(4);
        let turn = turn_id(5);
        let recorded = applied_result(&request, accepted_input, turn);
        let expected = SubmitInputOutcome::Recorded(recorded);
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input], [turn]),
            FakeTransaction::returning([Ok(expected.clone())]),
        );

        let actual =
            run_ready(service.execute(request.clone())).expect("fake transaction succeeds");

        assert_eq!(actual, expected);
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 1);
        assert_eq!(ids.turn_calls, 1);
        assert_eq!(transaction.observed.len(), 1);
        let (command, observed_input, observed_turn) = &transaction.observed[0];
        assert_eq!(command.command_id(), request.command_id());
        assert_eq!(command.session(), request.session());
        assert_eq!(command.actor(), Actor::Owner);
        assert_eq!(command.content(), request.content());
        assert_eq!(command.delivery(), request.delivery());
        assert_eq!(*observed_input, accepted_input);
        assert_eq!(*observed_turn, turn);
    }

    /// S01 / INV-012: every closed recorded result shape passes through
    /// unchanged without application preparation or translation.
    #[test]
    fn s01_inv012_recorded_applied_and_rejected_results_pass_through() {
        let request = request(1, "hello");
        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        let results = [
            applied_result(&request, accepted_input_id(4), turn_id(5)),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound {
                session: session_id(2),
            }),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
                session: session_id(2),
                expected_active_turn: turn_id(6),
            }),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                    session: session_id(2),
                    expected: version(1),
                    current: version(2),
                },
            ),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
                session: session_id(2),
                alias: ModelAlias::from_uuid(Uuid::from_u128(7)),
            }),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(2),
                last: maximum,
            }),
        ];

        for result in results {
            let expected = SubmitInputOutcome::Recorded(result);
            let mut service = SubmitInputService::new(
                FakeIds::new([accepted_input_id(8)], [turn_id(9)]),
                FakeTransaction::returning([Ok(expected.clone())]),
            );

            assert_eq!(
                run_ready(service.execute(request.clone()))
                    .expect("recorded terminal result is returned"),
                expected
            );
            assert_eq!(service.into_parts().1.observed.len(), 1);
        }
    }

    /// S01 / INV-012: equal replay returns original durable identities rather
    /// than either retransmission's fresh candidates.
    #[test]
    fn s01_inv012_equal_replay_returns_the_recorded_result() {
        let request = request(1, "hello");
        let winner_input = accepted_input_id(4);
        let winner_turn = turn_id(5);
        let recorded = applied_result(&request, winner_input, winner_turn);
        let expected = SubmitInputOutcome::Recorded(recorded);
        let mut service = SubmitInputService::new(
            FakeIds::new(
                [accepted_input_id(6), accepted_input_id(7)],
                [turn_id(8), turn_id(9)],
            ),
            FakeTransaction::returning([Ok(expected.clone()), Ok(expected.clone())]),
        );

        let first = run_ready(service.execute(request.clone())).expect("first invocation succeeds");
        let replay = run_ready(service.execute(request)).expect("equal replay succeeds");

        assert_eq!(first, expected);
        assert_eq!(replay, expected);
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 2);
        assert_eq!(ids.turn_calls, 2);
        assert_eq!(transaction.observed.len(), 2);
        let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(applied)) = replay else {
            panic!("recorded replay remains applied");
        };
        assert_eq!(applied.accepted_input(), winner_input);
        assert_eq!(applied.turn(), winner_turn);
    }

    /// S01 / INV-012: owner-global conflicting reuse is returned unchanged.
    #[test]
    fn s01_inv012_conflicting_reuse_is_returned_unchanged() {
        let request = request(1, "hello");
        let expected = SubmitInputOutcome::ConflictingReuse {
            command_id: request.command_id(),
        };
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input_id(4)], [turn_id(5)]),
            FakeTransaction::returning([Ok(expected.clone())]),
        );

        let actual = run_ready(service.execute(request)).expect("conflict is terminal");

        assert_eq!(actual, expected);
        assert_eq!(service.into_parts().1.observed.len(), 1);
    }

    /// S01 / INV-012: a transaction failure remains nonterminal after exactly
    /// one call; application orchestration does not retry or fabricate a
    /// recorded result.
    #[test]
    fn s01_inv012_transaction_failure_is_returned_without_retry() {
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input_id(4)], [turn_id(5)]),
            FakeTransaction::returning([Err(FakeTransactionError::Unavailable)]),
        );

        let error = run_ready(service.execute(request(1, "hello")))
            .expect_err("infrastructure failure remains nonterminal");

        assert_eq!(error, FakeTransactionError::Unavailable);
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 1);
        assert_eq!(ids.turn_calls, 1);
        assert_eq!(transaction.observed.len(), 1);
    }
}
