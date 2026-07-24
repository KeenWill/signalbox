//! Durable input-submission orchestration.
//!
//! docs/spec/identity-and-commands.md owns hub-minted identity supply and
//! owner-global command replay, and admits only the owner actor at the
//! baseline command boundary. Authoritative session loading, position
//! allocation, preparation, and recording remain inside one atomic
//! transaction port.

use std::{error::Error, fmt, future::Future};

use signalbox_domain::{
    AcceptedInputId, CancelledModelCallTurnIdentities, ContextFrontierId, DeliveryRequest,
    DurableCommandId, SemanticTranscriptEntryId, SessionId, SubmitInput as DomainSubmitInput,
    SubmitInputAppliedResult, SubmitInputResult, TurnId, UserContent,
};

use crate::{
    EligibilityNudge, EligibilityNudgeOutcome, InProcessToolDispatchGate, InvalidDurableCommandId,
    OperatorFailureClass,
};

/// Why caller input cannot enter canonical `SubmitInput` construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitInputRequestError {
    /// The owner-global command identity is a reserved sentinel.
    InvalidCommandId(InvalidDurableCommandId),
    /// The accepted-input text exceeds the provisional admission bound.
    OversizedContent {
        /// The rejected text's exact UTF-8 length in bytes.
        utf8_byte_length: usize,
    },
}

impl fmt::Display for SubmitInputRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommandId(error) => error.fmt(formatter),
            Self::OversizedContent { utf8_byte_length } => write!(
                formatter,
                "accepted-input content is {utf8_byte_length} UTF-8 bytes; the provisional maximum is {}",
                SubmitInputRequest::MAX_CONTENT_UTF8_BYTES,
            ),
        }
    }
}

impl Error for SubmitInputRequestError {}

/// The complete admitted application request for durable input submission.
///
/// Content is already a checked domain value. Private fields ensure the nil
/// and max command-identity sentinels reserved by
/// docs/spec/identity-and-commands.md cannot enter canonical command
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
    /// The provisional inclusive admission maximum: one mebibyte of UTF-8
    /// text.
    pub const MAX_CONTENT_UTF8_BYTES: usize = 1_048_576;

    /// Validates admission policy before canonical command construction.
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
    ) -> Result<Self, SubmitInputRequestError> {
        if command_id.as_uuid().is_nil() {
            return Err(SubmitInputRequestError::InvalidCommandId(
                InvalidDurableCommandId::Nil,
            ));
        }
        if command_id.as_uuid().is_max() {
            return Err(SubmitInputRequestError::InvalidCommandId(
                InvalidDurableCommandId::Max,
            ));
        }
        let utf8_byte_length = content.text().as_str().len();
        if utf8_byte_length > Self::MAX_CONTENT_UTF8_BYTES {
            return Err(SubmitInputRequestError::OversizedContent { utf8_byte_length });
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
/// a rejection. A turn candidate is requested only for a delivery mode that
/// creates a turn when applied; `NextSafePoint` initially creates none. UUID
/// timestamps are not domain order or authority.
pub trait SubmitInputIdGenerator {
    /// Generates one candidate accepted-input identity.
    fn next_accepted_input_id(&mut self) -> AcceptedInputId;

    /// Generates one candidate future queued-work identity.
    fn next_turn_id(&mut self) -> TurnId;

    /// Generates one candidate cancellation-marker identity.
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;

    /// Generates one candidate terminal-frontier identity.
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
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

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
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
/// any accepted queued-work facts. A failure proven to precede commit claims no
/// identity, but an adapter error may be commit-ambiguous. Callers retain the
/// command identity and exact payload until a terminal response and resubmit
/// that same command for recovery. The application neither preloads state nor
/// retries this port automatically.
pub trait SubmitInputTransaction {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Handles one canonical command and its hub-minted identity candidates.
    fn handle<NextTurn, NextToolCancellation>(
        &mut self,
        command: DomainSubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
        next_tool_cancellation: NextToolCancellation,
    ) -> impl Future<Output = Result<SubmitInputOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        NextToolCancellation: FnMut(
                &[signalbox_domain::ToolRequestId],
            ) -> (Vec<SemanticTranscriptEntryId>, ContextFrontierId)
            + Send;
}

/// Coordinates the durable input-submission use case.
#[derive(Debug)]
pub struct SubmitInputService<Generator, Transaction, Nudge> {
    ids: Generator,
    transaction: Transaction,
    nudge: Nudge,
    tool_dispatch_gate: Option<InProcessToolDispatchGate>,
}

impl<Generator, Transaction, Nudge> SubmitInputService<Generator, Transaction, Nudge> {
    /// Composes the application identity, transaction, and nudge ports.
    pub const fn new(ids: Generator, transaction: Transaction, nudge: Nudge) -> Self {
        Self {
            ids,
            transaction,
            nudge,
            tool_dispatch_gate: None,
        }
    }

    /// Shares tool dispatch/interrupt ordering with tool execution.
    pub fn with_tool_dispatch_gate(mut self, gate: InProcessToolDispatchGate) -> Self {
        self.tool_dispatch_gate = Some(gate);
        self
    }

    /// Returns all three ports, primarily for explicit ownership handoff.
    pub fn into_parts(
        self,
    ) -> (
        Generator,
        Transaction,
        Nudge,
        Option<InProcessToolDispatchGate>,
    ) {
        (
            self.ids,
            self.transaction,
            self.nudge,
            self.tool_dispatch_gate,
        )
    }
}

impl<Generator, Transaction, Nudge> SubmitInputService<Generator, Transaction, Nudge>
where
    Generator: SubmitInputIdGenerator + Send,
    Transaction: SubmitInputTransaction,
    Nudge: EligibilityNudge,
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
        let session = request.session;
        let interrupt_turn = match request.delivery {
            DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            } => Some(expected_active_turn),
            DeliveryRequest::StartWhenNoActiveTurn { .. }
            | DeliveryRequest::AfterCurrentTurn { .. }
            | DeliveryRequest::NextSafePoint { .. } => None,
        };
        let _tool_dispatch_permit = match (&self.tool_dispatch_gate, interrupt_turn) {
            (Some(gate), Some(turn)) => Some(gate.acquire(turn).await),
            (Some(_) | None, None) | (None, Some(_)) => None,
        };
        let turn = match request.delivery {
            DeliveryRequest::NextSafePoint { .. } => None,
            DeliveryRequest::StartWhenNoActiveTurn { .. }
            | DeliveryRequest::Interrupt { .. }
            | DeliveryRequest::AfterCurrentTurn { .. } => Some(self.ids.next_turn_id()),
        };
        let command = DomainSubmitInput::new(
            request.command_id,
            request.session,
            request.content,
            request.delivery,
        );
        let accepted_input = self.ids.next_accepted_input_id();
        let cancellation_identities = CancelledModelCallTurnIdentities::new(
            self.ids.next_semantic_entry_id(),
            self.ids.next_context_frontier_id(),
        );
        let ids = std::sync::Mutex::new(&mut self.ids);

        let outcome = self
            .transaction
            .handle(
                command,
                accepted_input,
                turn,
                cancellation_identities,
                |_| {
                    ids.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .next_turn_id()
                },
                |requests| {
                    let mut ids = ids
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    (
                        requests
                            .iter()
                            .map(|_| ids.next_semantic_entry_id())
                            .collect(),
                        ids.next_context_frontier_id(),
                    )
                },
            )
            .await;
        if matches!(
            &outcome,
            Ok(SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
                SubmitInputAppliedResult::TurnOrigin(_)
            )))
        ) {
            let nudge_outcome = self.nudge.nudge(session);
            if nudge_outcome != EligibilityNudgeOutcome::Enqueued {
                tracing::warn!(
                    failure_class = ?OperatorFailureClass::Infrastructure {
                        commit_ambiguous: false,
                    },
                    ?nudge_outcome,
                    "eligibility nudge was lost after command handling; \
                     the reconciliation sweep will recover it"
                );
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::VecDeque,
        future::{Future, ready},
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use signalbox_domain::{
        Actor, DirectModelSelection, ModelAlias, ModelSelectionOverride, ModelSelectionRequest,
        PerInputConfigurationChoices, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SubmitInputRejectedResult,
        TranscriptAncestry,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        AcceptedInputId, CancelledModelCallTurnIdentities, DeliveryRequest, DomainSubmitInput,
        DurableCommandId, EligibilityNudge, EligibilityNudgeOutcome, InProcessToolDispatchGate,
        InvalidDurableCommandId, SessionId, SubmitInputAppliedResult, SubmitInputIdGenerator,
        SubmitInputOutcome, SubmitInputRequest, SubmitInputRequestError, SubmitInputResult,
        SubmitInputService, SubmitInputTransaction, TurnId, UserContent,
        UuidV7SubmitInputIdGenerator,
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

    /// A validated request whose one knob is the command-identity seed
    /// (`docs/agents/testing-style.md`, rule 4); it targets the canonical session
    /// with canonical "hello" content and the version-one
    /// start-when-no-active-turn delivery.
    fn request(command: u128) -> SubmitInputRequest {
        SubmitInputRequest::try_new(
            command_id(command),
            session_id(2),
            content("hello"),
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
            .prepare_when_no_active_turn(
                &current_session(),
                accepted_input,
                Some(turn),
                None,
                |_| None,
            )
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

    /// Deliberately supplies no turn candidates to a [`FakeIds`] script: a
    /// delivery mode that must not mint a turn panics if it tries.
    const NO_TURN_CANDIDATES: [TurnId; 0] = [];

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

        /// Deliberately scripted with no candidates: any mint call panics.
        fn expecting_no_calls() -> Self {
            Self::new([], [])
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

        fn next_semantic_entry_id(&mut self) -> signalbox_domain::SemanticTranscriptEntryId {
            signalbox_domain::SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                0x1000 + self.accepted_input_calls as u128,
            ))
        }

        fn next_context_frontier_id(&mut self) -> signalbox_domain::ContextFrontierId {
            signalbox_domain::ContextFrontierId::from_uuid(Uuid::from_u128(
                0x2000 + self.accepted_input_calls as u128,
            ))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeTransactionError {
        Unavailable,
    }

    #[derive(Debug)]
    struct FakeTransaction {
        responses: VecDeque<Result<SubmitInputOutcome, FakeTransactionError>>,
        observed: Vec<(DomainSubmitInput, AcceptedInputId, Option<TurnId>)>,
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

        /// Deliberately scripted with no responses: any handling call panics.
        fn expecting_no_calls() -> Self {
            Self::returning([])
        }
    }

    impl SubmitInputTransaction for FakeTransaction {
        type Error = FakeTransactionError;

        fn handle<NextTurn, NextToolCancellation>(
            &mut self,
            command: DomainSubmitInput,
            accepted_input: AcceptedInputId,
            turn: Option<TurnId>,
            _cancellation_identities: CancelledModelCallTurnIdentities,
            _next_reclassified_turn: NextTurn,
            _next_tool_cancellation: NextToolCancellation,
        ) -> impl Future<Output = Result<SubmitInputOutcome, Self::Error>> + Send
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
            NextToolCancellation: FnMut(
                    &[signalbox_domain::ToolRequestId],
                ) -> (
                    Vec<signalbox_domain::SemanticTranscriptEntryId>,
                    signalbox_domain::ContextFrontierId,
                ) + Send,
        {
            self.observed.push((command, accepted_input, turn));
            ready(
                self.responses
                    .pop_front()
                    .expect("test must supply one response per invocation"),
            )
        }
    }

    #[derive(Debug, Default)]
    struct FakeNudge {
        observed: RefCell<Vec<SessionId>>,
    }

    impl EligibilityNudge for FakeNudge {
        fn nudge(&self, session: SessionId) -> EligibilityNudgeOutcome {
            self.observed.borrow_mut().push(session);
            EligibilityNudgeOutcome::Enqueued
        }
    }

    /// S01 / INV-001 / INV-012: reserved command identities fail before
    /// canonical command construction or any application effect.
    #[test]
    fn s01_inv001_inv012_request_rejects_reserved_command_identifiers() {
        assert_eq!(
            SubmitInputRequest::try_new(
                DurableCommandId::from_uuid(Uuid::nil()),
                session_id(2),
                content("hello"),
                delivery(1),
            ),
            Err(SubmitInputRequestError::InvalidCommandId(
                InvalidDurableCommandId::Nil
            ))
        );
        assert_eq!(
            SubmitInputRequest::try_new(
                DurableCommandId::from_uuid(Uuid::max()),
                session_id(2),
                content("hello"),
                delivery(1),
            ),
            Err(SubmitInputRequestError::InvalidCommandId(
                InvalidDurableCommandId::Max
            ))
        );

        let service = SubmitInputService::new(
            FakeIds::expecting_no_calls(),
            FakeTransaction::expecting_no_calls(),
            FakeNudge::default(),
        );
        let (ids, transaction, nudge, _) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 0);
        assert_eq!(ids.turn_calls, 0);
        assert!(transaction.observed.is_empty());
        assert!(nudge.observed.into_inner().is_empty());
    }

    /// Decision log 2026-07-20: exact-bound text remains admissible before
    /// canonical command construction, including a multi-byte terminal scalar.
    #[test]
    fn accepted_input_content_at_the_utf8_byte_bound_is_admitted() {
        let mut exact = "a".repeat(SubmitInputRequest::MAX_CONTENT_UTF8_BYTES - 2);
        exact.push('\u{e9}');

        let request =
            SubmitInputRequest::try_new(command_id(1), session_id(2), content(&exact), delivery(1))
                .expect("text ending exactly at the UTF-8 byte bound is admitted");

        assert_eq!(request.content().text().as_str().len(), 1_048_576);
    }

    /// INV-011 / INV-037: immediate interrupt handling waits on the same
    /// turn-keyed gate held across tool authorization, execution, and result
    /// commit.
    #[tokio::test]
    async fn inv011_inv037_interrupt_waits_for_tool_dispatch_gate() {
        let expected_turn = turn_id(9);
        let request = SubmitInputRequest::try_new(
            command_id(10),
            session_id(2),
            content("stop"),
            DeliveryRequest::Interrupt {
                expected_active_turn: expected_turn,
                configuration: choices(1),
            },
        )
        .expect("fixture interrupt is valid");
        let expected = SubmitInputOutcome::ConflictingReuse {
            command_id: request.command_id(),
        };
        let gate = InProcessToolDispatchGate::default();
        let permit = gate.acquire(expected_turn).await;
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input_id(11)], [turn_id(12)]),
            FakeTransaction::returning([Ok(expected.clone())]),
            FakeNudge::default(),
        )
        .with_tool_dispatch_gate(gate);
        let mut handling = Box::pin(service.execute(request));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut handling)
                .await
                .is_err(),
            "interrupt handling must remain behind the active tool dispatch"
        );
        drop(permit);
        assert_eq!(handling.await.expect("fake transaction succeeds"), expected);
    }

    /// Decision log 2026-07-20: oversized text is rejected at the application
    /// admission boundary without retaining it in the error.
    #[test]
    fn oversized_accepted_input_content_is_rejected_before_command_construction() {
        let oversized = "a".repeat(SubmitInputRequest::MAX_CONTENT_UTF8_BYTES + 1);

        assert_eq!(
            SubmitInputRequest::try_new(
                command_id(1),
                session_id(2),
                content(&oversized),
                delivery(1),
            ),
            Err(SubmitInputRequestError::OversizedContent {
                utf8_byte_length: 1_048_577,
            })
        );
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
        assert_uuid_v7_candidate_shape(first_input.into_uuid());
        assert_uuid_v7_candidate_shape(first_turn.into_uuid());
        assert_uuid_v7_candidate_shape(second_input.into_uuid());
        assert_uuid_v7_candidate_shape(second_turn.into_uuid());
    }

    /// Asserts the production candidate's UUIDv7 shape, reporting a failure
    /// at the generated candidate's call site.
    #[track_caller]
    fn assert_uuid_v7_candidate_shape(candidate: Uuid) {
        assert_eq!(candidate.get_variant(), Variant::RFC4122);
        assert_eq!(candidate.get_version(), Some(Version::SortRand));
        assert!(!candidate.is_nil());
        assert!(!candidate.is_max());
    }

    /// S01 / INV-002 / INV-007 / INV-008 / INV-012 / INV-028: orchestration
    /// fixes owner attribution and forwards one exact command and candidate
    /// pair to the atomic port.
    #[test]
    fn s01_inv002_inv007_inv008_inv012_inv028_orchestrates_one_owner_command_and_candidate_pair() {
        let request = request(1);
        let accepted_input = accepted_input_id(4);
        let turn = turn_id(5);
        let recorded = applied_result(&request, accepted_input, turn);
        let expected = SubmitInputOutcome::Recorded(recorded);
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input], [turn]),
            FakeTransaction::returning([Ok(expected.clone())]),
            FakeNudge::default(),
        );

        let actual =
            run_ready(service.execute(request.clone())).expect("fake transaction succeeds");

        assert_eq!(actual, expected);
        let (ids, transaction, nudge, _) = service.into_parts();
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
        assert_eq!(*observed_turn, Some(turn));
        assert_eq!(nudge.observed.into_inner(), vec![request.session()]);
    }

    /// S08 / INV-002 / INV-028: safe-point steering supplies no turn
    /// candidate because successful acceptance initially creates no turn.
    #[test]
    fn s08_inv002_inv028_next_safe_point_mints_no_turn() {
        let requested_session = session_id(2);
        let request = SubmitInputRequest::try_new(
            command_id(1),
            requested_session,
            content("steer"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(3),
            },
        )
        .expect("ordinary command identity is admitted");
        let expected = SubmitInputOutcome::Recorded(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::NoActiveTurn {
                session: requested_session,
                expected_active_turn: turn_id(3),
            },
        ));
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input_id(4)], NO_TURN_CANDIDATES),
            FakeTransaction::returning([Ok(expected.clone())]),
            FakeNudge::default(),
        );

        assert_eq!(
            run_ready(service.execute(request)).expect("fake transaction succeeds"),
            expected
        );
        let (ids, transaction, nudge, _) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 1);
        assert_eq!(ids.turn_calls, 0);
        assert_eq!(transaction.observed[0].2, None);
        assert!(nudge.observed.into_inner().is_empty());
    }

    /// Asserts that one recorded terminal result shape passes through the
    /// canonical request's execution unchanged after exactly one transaction
    /// call, reporting a failure at the shape's call site.
    #[track_caller]
    fn assert_recorded_result_passes_through(result: SubmitInputResult) {
        let eligibility_affecting = matches!(
            &result,
            SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(_))
        );
        let expected = SubmitInputOutcome::Recorded(result);
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input_id(8)], [turn_id(9)]),
            FakeTransaction::returning([Ok(expected.clone())]),
            FakeNudge::default(),
        );

        assert_eq!(
            run_ready(service.execute(request(1))).expect("recorded terminal result is returned"),
            expected
        );
        let (_, transaction, nudge, _) = service.into_parts();
        assert_eq!(transaction.observed.len(), 1);
        assert_eq!(
            nudge.observed.into_inner(),
            if eligibility_affecting {
                vec![session_id(2)]
            } else {
                Vec::new()
            }
        );
    }

    /// S01 / INV-012: a recorded applied result passes through unchanged
    /// without application preparation or translation.
    #[test]
    fn s01_inv012_recorded_applied_result_passes_through() {
        assert_recorded_result_passes_through(applied_result(
            &request(1),
            accepted_input_id(4),
            turn_id(5),
        ));
    }

    /// S01 / INV-012: every closed rejected result shape passes through
    /// unchanged without application preparation or translation.
    #[test]
    fn s01_inv012_recorded_rejected_results_pass_through() {
        assert_recorded_result_passes_through(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionNotFound {
                session: session_id(2),
            },
        ));
        assert_recorded_result_passes_through(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::NoActiveTurn {
                session: session_id(2),
                expected_active_turn: turn_id(6),
            },
        ));
        assert_recorded_result_passes_through(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session: session_id(2),
                expected: version(1),
                current: version(2),
            },
        ));
        assert_recorded_result_passes_through(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::UnknownModelAlias {
                session: session_id(2),
                alias: ModelAlias::from_uuid(Uuid::from_u128(7)),
            },
        ));
        assert_recorded_result_passes_through(SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(2),
                last: SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum"),
            },
        ));
    }

    /// S01 / INV-012: equal replay returns original durable identities rather
    /// than either retransmission's fresh candidates.
    #[test]
    fn s01_inv012_equal_replay_returns_the_recorded_result() {
        let request = request(1);
        let session = request.session();
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
            FakeNudge::default(),
        );

        let first = run_ready(service.execute(request.clone())).expect("first invocation succeeds");
        let replay = run_ready(service.execute(request)).expect("equal replay succeeds");

        assert_eq!(first, expected);
        assert_eq!(replay, expected);
        let (ids, transaction, nudge, _) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 2);
        assert_eq!(ids.turn_calls, 2);
        assert_eq!(transaction.observed.len(), 2);
        assert_eq!(nudge.observed.into_inner(), vec![session, session]);
        let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
            signalbox_domain::SubmitInputAppliedResult::TurnOrigin(applied),
        )) = replay
        else {
            panic!("recorded replay remains applied");
        };
        assert_eq!(applied.accepted_input(), winner_input);
        assert_eq!(applied.turn(), winner_turn);
    }

    /// S01 / INV-012: owner-global conflicting reuse is returned unchanged.
    #[test]
    fn s01_inv012_conflicting_reuse_is_returned_unchanged() {
        let request = request(1);
        let expected = SubmitInputOutcome::ConflictingReuse {
            command_id: request.command_id(),
        };
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input_id(4)], [turn_id(5)]),
            FakeTransaction::returning([Ok(expected.clone())]),
            FakeNudge::default(),
        );

        let actual = run_ready(service.execute(request)).expect("conflict is terminal");

        assert_eq!(actual, expected);
        let (_, transaction, nudge, _) = service.into_parts();
        assert_eq!(transaction.observed.len(), 1);
        assert!(nudge.observed.into_inner().is_empty());
    }

    /// S01 / INV-012: a transaction failure remains nonterminal after exactly
    /// one call; application orchestration does not retry or fabricate a
    /// recorded result.
    #[test]
    fn s01_inv012_transaction_failure_is_returned_without_retry() {
        let mut service = SubmitInputService::new(
            FakeIds::new([accepted_input_id(4)], [turn_id(5)]),
            FakeTransaction::returning([Err(FakeTransactionError::Unavailable)]),
            FakeNudge::default(),
        );

        let error = run_ready(service.execute(request(1)))
            .expect_err("infrastructure failure remains nonterminal");

        assert_eq!(error, FakeTransactionError::Unavailable);
        let (ids, transaction, nudge, _) = service.into_parts();
        assert_eq!(ids.accepted_input_calls, 1);
        assert_eq!(ids.turn_calls, 1);
        assert_eq!(transaction.observed.len(), 1);
        assert!(
            nudge.observed.into_inner().is_empty(),
            "a failed transaction has no committed eligibility change to nudge"
        );
    }
}
