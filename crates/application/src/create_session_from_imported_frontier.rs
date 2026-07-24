//! Imported-frontier session-creation orchestration.
//!
//! Import remains pure ingestion. This use case later constructs one distinct
//! durable command selecting an addressable imported frontier and delegates
//! claim-first target resolution plus complete seed creation to one atomic
//! transaction port.

use std::future::Future;

use signalbox_domain::{
    ContextFrontierId,
    CreateSessionFromImportedFrontier as DomainCreateSessionFromImportedFrontier,
    CreateSessionFromImportedFrontierAppliedResult, DurableCommandId, ImportedConversationId,
    ImportedSessionRelationship, ImportedTranscriptFrontier, SemanticTranscriptEntryId,
    SessionConfigurationDefaults, SessionId,
};

use crate::InvalidDurableCommandId;

/// The complete admitted request for later creation from imported history.
///
/// The selected frontier owns its imported-conversation identity, so this
/// request accepts no second conversation field. Private fields keep reserved
/// command sentinels out of canonical construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateSessionFromImportedFrontierRequest {
    command_id: DurableCommandId,
    imported_frontier: ImportedTranscriptFrontier,
    relationship: ImportedSessionRelationship,
    initial_configuration_defaults: SessionConfigurationDefaults,
}

impl CreateSessionFromImportedFrontierRequest {
    /// Validates the owner-global command identity before any effect.
    pub fn try_new(
        command_id: DurableCommandId,
        imported_frontier: ImportedTranscriptFrontier,
        relationship: ImportedSessionRelationship,
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
            imported_frontier,
            relationship,
            initial_configuration_defaults,
        })
    }

    /// Returns the owner-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the selected addressable imported boundary.
    pub const fn imported_frontier(&self) -> ImportedTranscriptFrontier {
        self.imported_frontier
    }

    /// Returns the client's creation-time relationship to the imported point.
    pub const fn relationship(&self) -> ImportedSessionRelationship {
        self.relationship
    }

    /// Returns the complete initial model-selection defaults.
    pub const fn initial_configuration_defaults(&self) -> SessionConfigurationDefaults {
        self.initial_configuration_defaults
    }
}

/// Application effect supplying every identity created by imported seeding.
///
/// Fixed session and seed-frontier candidates are generated once before the
/// transaction. Semantic-entry identities are requested only through the
/// closure passed to the transaction after authoritative prefix resolution.
pub trait CreateSessionFromImportedFrontierIdGenerator {
    /// Generates one candidate session identity.
    fn next_session_id(&mut self) -> SessionId;

    /// Generates one imported-provenance semantic-entry identity.
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;

    /// Generates one candidate seed context-frontier identity.
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
}

/// Production UUIDv7 identity generator for imported session creation.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7CreateSessionFromImportedFrontierIdGenerator;

impl CreateSessionFromImportedFrontierIdGenerator
    for UuidV7CreateSessionFromImportedFrontierIdGenerator
{
    fn next_session_id(&mut self) -> SessionId {
        SessionId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// The closed application result of one imported-frontier creation handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionFromImportedFrontierOutcome {
    /// First handling or equal replay returned the recorded applied result.
    Applied(CreateSessionFromImportedFrontierAppliedResult),
    /// The selected immutable imported conversation does not exist.
    ImportedConversationNotFound {
        /// The conversation derived from the selected frontier.
        conversation: ImportedConversationId,
    },
    /// The conversation exists but does not contain the selected boundary.
    ImportedFrontierNotFound {
        /// The exact selected boundary that was not found.
        frontier: ImportedTranscriptFrontier,
    },
    /// The owner-global command identity already names another typed payload.
    ConflictingReuse {
        /// The identity whose existing meaning remains intact.
        command_id: DurableCommandId,
    },
}

/// Atomic command boundary for creation from an imported frontier.
///
/// Implementations inspect the owner-global command registry first. A claimed
/// identity resolves replay or conflict before imported-target lookup. For an
/// unseen identity, the transaction loads the complete immutable conversation,
/// resolves the selected prefix, then invokes `next_semantic_entry_id` exactly
/// once per prefix member and supplies those identities to checked domain
/// preparation. Missing targets and infrastructure failure leave the command
/// unclaimed; first handling commits every seed fact and its result atomically.
pub trait CreateSessionFromImportedFrontierTransaction {
    /// Adapter-specific infrastructure, integrity, or identity failure.
    type Error;

    /// Handles one canonical command and its application-owned candidates.
    fn handle<NextSemanticEntryId>(
        &mut self,
        command: DomainCreateSessionFromImportedFrontier,
        session: SessionId,
        seed_frontier: ContextFrontierId,
        next_semantic_entry_id: NextSemanticEntryId,
    ) -> impl Future<Output = Result<CreateSessionFromImportedFrontierOutcome, Self::Error>> + Send
    where
        NextSemanticEntryId: FnMut() -> SemanticTranscriptEntryId + Send;
}

/// Coordinates later session creation from one imported frontier.
#[derive(Debug)]
pub struct CreateSessionFromImportedFrontierService<Generator, Transaction> {
    ids: Generator,
    transaction: Transaction,
}

impl<Generator, Transaction> CreateSessionFromImportedFrontierService<Generator, Transaction> {
    /// Composes the application identity and atomic transaction ports.
    pub const fn new(ids: Generator, transaction: Transaction) -> Self {
        Self { ids, transaction }
    }

    /// Returns both ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (Generator, Transaction) {
        (self.ids, self.transaction)
    }
}

impl<Generator, Transaction> CreateSessionFromImportedFrontierService<Generator, Transaction>
where
    Generator: CreateSessionFromImportedFrontierIdGenerator + Send,
    Transaction: CreateSessionFromImportedFrontierTransaction,
{
    /// Constructs and handles one imported-frontier creation command.
    ///
    /// Every invocation draws fresh fixed candidates, including replay after a
    /// lost acknowledgement. Semantic candidates remain transaction-controlled
    /// through the closure; the application performs no import lookup or retry.
    pub async fn execute(
        &mut self,
        request: CreateSessionFromImportedFrontierRequest,
    ) -> Result<CreateSessionFromImportedFrontierOutcome, Transaction::Error> {
        let command = DomainCreateSessionFromImportedFrontier::new(
            request.command_id,
            request.imported_frontier,
            request.relationship,
            request.initial_configuration_defaults,
        );
        let session = self.ids.next_session_id();
        let seed_frontier = self.ids.next_context_frontier_id();
        let ids = &mut self.ids;

        self.transaction
            .handle(command, session, seed_frontier, || {
                ids.next_semantic_entry_id()
            })
            .await
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
        CreateSessionFromImportedFrontierAppliedResult, DirectModelSelection, ImportedConversation,
        ImportedConversationFormat, ImportedConversationId, ImportedRawRecordPosition,
        ImportedRawSourceRecord, ImportedRecordEntryPosition, ImportedSourceAttestation,
        ImportedSourceMetadata, ImportedSpeaker, ImportedStructuredObjectMember,
        ImportedStructuredValue, ImportedText, ImportedTranscriptContent,
        ImportedTranscriptEntryId, ImportedTranscriptEntryInput, ImportedTranscriptPosition,
        ModelSelectionRequest,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        ContextFrontierId, CreateSessionFromImportedFrontierIdGenerator,
        CreateSessionFromImportedFrontierOutcome, CreateSessionFromImportedFrontierRequest,
        CreateSessionFromImportedFrontierService, CreateSessionFromImportedFrontierTransaction,
        DomainCreateSessionFromImportedFrontier, DurableCommandId, ImportedSessionRelationship,
        ImportedTranscriptFrontier, InvalidDurableCommandId, SemanticTranscriptEntryId,
        SessionConfigurationDefaults, SessionId,
        UuidV7CreateSessionFromImportedFrontierIdGenerator,
    };

    fn command_id(value: u128) -> DurableCommandId {
        DurableCommandId::from_uuid(Uuid::from_u128(value))
    }

    fn conversation_id(value: u128) -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(value))
    }

    fn imported_entry_id(value: u128) -> ImportedTranscriptEntryId {
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value))
    }

    fn session_id(value: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(value))
    }

    fn semantic_entry_id(value: u128) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(value))
    }

    fn context_frontier_id(value: u128) -> ContextFrontierId {
        ContextFrontierId::from_uuid(Uuid::from_u128(value))
    }

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(value)),
        ))
    }

    fn text(value: &str) -> ImportedText {
        ImportedText::new(String::from(value))
    }

    fn metadata(speaker: ImportedSpeaker) -> ImportedSourceMetadata {
        ImportedSourceMetadata::new(
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::Attested(speaker),
        )
    }

    fn message_record(role: &str, content: &str) -> ImportedStructuredValue {
        ImportedStructuredValue::Object(
            vec![
                ImportedStructuredObjectMember::new(
                    text("type"),
                    ImportedStructuredValue::String(text(role)),
                ),
                ImportedStructuredObjectMember::new(
                    text("message"),
                    ImportedStructuredValue::Object(
                        vec![
                            ImportedStructuredObjectMember::new(
                                text("role"),
                                ImportedStructuredValue::String(text(role)),
                            ),
                            ImportedStructuredObjectMember::new(
                                text("content"),
                                ImportedStructuredValue::String(text(content)),
                            ),
                        ]
                        .into_boxed_slice(),
                    ),
                ),
            ]
            .into_boxed_slice(),
        )
    }

    fn imported_conversation() -> ImportedConversation {
        let conversation = conversation_id(10);
        ImportedConversation::from_converted_records(
            conversation,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![
                ImportedRawSourceRecord::from_converted(
                    br#"{"type":"user","message":{"role":"user","content":"first"}}"#.to_vec(),
                    message_record("user", "first"),
                ),
                ImportedRawSourceRecord::from_converted(
                    br#"{"type":"assistant","message":{"role":"assistant","content":"second"}}"#
                        .to_vec(),
                    message_record("assistant", "second"),
                ),
            ],
            vec![
                ImportedTranscriptEntryInput::new(
                    imported_entry_id(11),
                    conversation,
                    ImportedTranscriptPosition::first(),
                    ImportedRawRecordPosition::first(),
                    ImportedRecordEntryPosition::first(),
                    ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                    ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(
                        "first",
                    ))),
                    metadata(ImportedSpeaker::User),
                ),
                ImportedTranscriptEntryInput::new(
                    imported_entry_id(12),
                    conversation,
                    ImportedTranscriptPosition::try_from_u64(2)
                        .expect("fixture position is positive"),
                    ImportedRawRecordPosition::try_from_u64(2)
                        .expect("fixture position is positive"),
                    ImportedRecordEntryPosition::first(),
                    ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                    ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(
                        "second",
                    ))),
                    metadata(ImportedSpeaker::Assistant),
                ),
            ],
        )
        .expect("fixture aggregate is complete")
    }

    fn frontier(conversation: &ImportedConversation) -> ImportedTranscriptFrontier {
        conversation
            .frontiers()
            .last()
            .expect("fixture has an imported frontier")
    }

    fn request(
        command: DurableCommandId,
        selected: ImportedTranscriptFrontier,
    ) -> CreateSessionFromImportedFrontierRequest {
        CreateSessionFromImportedFrontierRequest::try_new(
            command,
            selected,
            ImportedSessionRelationship::Resume,
            defaults(20),
        )
        .expect("fixture command identity is admitted")
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
        sessions: VecDeque<SessionId>,
        semantic_entries: VecDeque<SemanticTranscriptEntryId>,
        frontiers: VecDeque<ContextFrontierId>,
        session_calls: usize,
        semantic_entry_calls: usize,
        frontier_calls: usize,
    }

    impl FakeIds {
        fn new(
            sessions: impl IntoIterator<Item = SessionId>,
            semantic_entries: impl IntoIterator<Item = SemanticTranscriptEntryId>,
            frontiers: impl IntoIterator<Item = ContextFrontierId>,
        ) -> Self {
            Self {
                sessions: sessions.into_iter().collect(),
                semantic_entries: semantic_entries.into_iter().collect(),
                frontiers: frontiers.into_iter().collect(),
                session_calls: 0,
                semantic_entry_calls: 0,
                frontier_calls: 0,
            }
        }

        fn expecting_no_calls() -> Self {
            Self::new([], [], [])
        }
    }

    impl CreateSessionFromImportedFrontierIdGenerator for FakeIds {
        fn next_session_id(&mut self) -> SessionId {
            self.session_calls += 1;
            self.sessions
                .pop_front()
                .expect("test supplies one session candidate per invocation")
        }

        fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
            self.semantic_entry_calls += 1;
            self.semantic_entries
                .pop_front()
                .expect("test supplies each requested semantic-entry candidate")
        }

        fn next_context_frontier_id(&mut self) -> ContextFrontierId {
            self.frontier_calls += 1;
            self.frontiers
                .pop_front()
                .expect("test supplies one frontier candidate per invocation")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeTransactionError {
        Unavailable,
    }

    #[derive(Debug)]
    enum FakeResponse {
        Prepare(ImportedConversation),
        Return(CreateSessionFromImportedFrontierOutcome),
        Fail(FakeTransactionError),
    }

    #[derive(Debug)]
    struct FakeTransaction {
        responses: VecDeque<FakeResponse>,
        observed: Vec<(
            DomainCreateSessionFromImportedFrontier,
            SessionId,
            ContextFrontierId,
        )>,
        generated_semantic_entries: Vec<Vec<SemanticTranscriptEntryId>>,
    }

    impl FakeTransaction {
        fn returning(responses: impl IntoIterator<Item = FakeResponse>) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                observed: Vec::new(),
                generated_semantic_entries: Vec::new(),
            }
        }

        fn expecting_no_calls() -> Self {
            Self::returning([])
        }
    }

    impl CreateSessionFromImportedFrontierTransaction for FakeTransaction {
        type Error = FakeTransactionError;

        fn handle<NextSemanticEntryId>(
            &mut self,
            command: DomainCreateSessionFromImportedFrontier,
            session: SessionId,
            seed_frontier: ContextFrontierId,
            mut next_semantic_entry_id: NextSemanticEntryId,
        ) -> impl Future<Output = Result<CreateSessionFromImportedFrontierOutcome, Self::Error>> + Send
        where
            NextSemanticEntryId: FnMut() -> SemanticTranscriptEntryId + Send,
        {
            self.observed.push((command, session, seed_frontier));
            let response = self
                .responses
                .pop_front()
                .expect("test supplies one response per invocation");
            let outcome = match response {
                FakeResponse::Prepare(conversation) => {
                    let mut generated = Vec::new();
                    let prepared = command
                        .prepare(&conversation, session, seed_frontier, || {
                            let identity = next_semantic_entry_id();
                            generated.push(identity);
                            identity
                        })
                        .expect("fixture target and identities prepare");
                    self.generated_semantic_entries.push(generated);
                    Ok(CreateSessionFromImportedFrontierOutcome::Applied(
                        prepared.applied_result(),
                    ))
                }
                FakeResponse::Return(outcome) => {
                    self.generated_semantic_entries.push(Vec::new());
                    Ok(outcome)
                }
                FakeResponse::Fail(error) => {
                    self.generated_semantic_entries.push(Vec::new());
                    Err(error)
                }
            };
            ready(outcome)
        }
    }

    #[track_caller]
    fn assert_reserved_command_rejected(
        identity: DurableCommandId,
        selected: ImportedTranscriptFrontier,
        expected: InvalidDurableCommandId,
    ) {
        assert_eq!(
            CreateSessionFromImportedFrontierRequest::try_new(
                identity,
                selected,
                ImportedSessionRelationship::Fork,
                defaults(20),
            ),
            Err(expected)
        );
    }

    #[track_caller]
    fn assert_uuid_v7(candidate: Uuid) {
        assert_eq!(candidate.get_variant(), Variant::RFC4122);
        assert_eq!(candidate.get_version(), Some(Version::SortRand));
        assert!(!candidate.is_nil());
        assert!(!candidate.is_max());
    }

    /// S28 / INV-001 / INV-012: reserved identities fail before construction
    /// or any application effect.
    #[test]
    fn s28_inv001_inv012_request_rejects_reserved_command_identifiers() {
        let conversation = imported_conversation();
        let selected = frontier(&conversation);
        assert_reserved_command_rejected(
            DurableCommandId::from_uuid(Uuid::nil()),
            selected,
            InvalidDurableCommandId::Nil,
        );
        assert_reserved_command_rejected(
            DurableCommandId::from_uuid(Uuid::max()),
            selected,
            InvalidDurableCommandId::Max,
        );

        let service = CreateSessionFromImportedFrontierService::new(
            FakeIds::expecting_no_calls(),
            FakeTransaction::expecting_no_calls(),
        );
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.session_calls, 0);
        assert_eq!(ids.semantic_entry_calls, 0);
        assert_eq!(ids.frontier_calls, 0);
        assert!(transaction.observed.is_empty());
    }

    /// S28: the admitted request retains exactly the caller-selected frontier,
    /// relationship, and initial defaults without a second conversation field.
    #[test]
    fn s28_request_preserves_the_complete_caller_payload() {
        let conversation = imported_conversation();
        let selected = frontier(&conversation);
        let request = CreateSessionFromImportedFrontierRequest::try_new(
            command_id(1),
            selected,
            ImportedSessionRelationship::Fork,
            defaults(20),
        )
        .expect("fixture request is admitted");

        assert_eq!(request.command_id(), command_id(1));
        assert_eq!(request.imported_frontier(), selected);
        assert_eq!(request.relationship(), ImportedSessionRelationship::Fork);
        assert_eq!(request.initial_configuration_defaults(), defaults(20));
    }

    /// S28 / INV-001 / INV-002: production generation supplies fresh UUIDv7
    /// values for all three distinct domain identity kinds.
    #[test]
    fn s28_inv001_inv002_production_generator_supplies_fresh_uuid_v7_candidates() {
        let mut generator = UuidV7CreateSessionFromImportedFrontierIdGenerator;
        let first_session = generator.next_session_id();
        let first_entry = generator.next_semantic_entry_id();
        let first_frontier = generator.next_context_frontier_id();
        let second_session = generator.next_session_id();
        let second_entry = generator.next_semantic_entry_id();
        let second_frontier = generator.next_context_frontier_id();

        assert_ne!(first_session, second_session);
        assert_ne!(first_entry, second_entry);
        assert_ne!(first_frontier, second_frontier);
        assert_uuid_v7(first_session.into_uuid());
        assert_uuid_v7(first_entry.into_uuid());
        assert_uuid_v7(first_frontier.into_uuid());
        assert_uuid_v7(second_session.into_uuid());
        assert_uuid_v7(second_entry.into_uuid());
        assert_uuid_v7(second_frontier.into_uuid());
    }

    /// S28 / INV-038 / INV-039: one invocation passes fixed candidates once
    /// and semantic identities remain transaction-controlled after prefix
    /// resolution.
    #[test]
    fn s28_inv038_inv039_orchestrates_one_atomic_checked_seed_creation() {
        let conversation = imported_conversation();
        let selected = frontier(&conversation);
        let request = request(command_id(1), selected);
        let expected_session = session_id(30);
        let expected_frontier = context_frontier_id(40);
        let expected_entries = vec![semantic_entry_id(50), semantic_entry_id(51)];
        let mut service = CreateSessionFromImportedFrontierService::new(
            FakeIds::new(
                [expected_session],
                expected_entries.iter().copied(),
                [expected_frontier],
            ),
            FakeTransaction::returning([FakeResponse::Prepare(conversation)]),
        );

        let outcome = run_ready(service.execute(request)).expect("handling succeeds");
        assert!(matches!(
            outcome,
            CreateSessionFromImportedFrontierOutcome::Applied(result)
                if result.session() == expected_session
        ));

        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.session_calls, 1);
        assert_eq!(ids.frontier_calls, 1);
        assert_eq!(ids.semantic_entry_calls, 2);
        assert_eq!(transaction.observed.len(), 1);
        let (command, session, seed_frontier) = &transaction.observed[0];
        assert_eq!(command.command_id(), command_id(1));
        assert_eq!(command.imported_frontier(), selected);
        assert_eq!(command.relationship(), ImportedSessionRelationship::Resume);
        assert_eq!(command.initial_configuration_defaults(), defaults(20));
        assert_eq!(*session, expected_session);
        assert_eq!(*seed_frontier, expected_frontier);
        assert_eq!(
            transaction.generated_semantic_entries,
            vec![expected_entries]
        );
    }

    /// S28 / INV-012: equal replay returns the recorded session and does not
    /// request variable-cardinality semantic identities.
    #[test]
    fn s28_inv012_equal_replay_discards_fresh_fixed_candidates() {
        let conversation = imported_conversation();
        let selected = frontier(&conversation);
        let recorded_session = session_id(99);
        let recorded = CreateSessionFromImportedFrontierOutcome::Applied(applied_result(
            &conversation,
            selected,
            recorded_session,
        ));
        let mut service = CreateSessionFromImportedFrontierService::new(
            FakeIds::new([session_id(30)], [], [context_frontier_id(40)]),
            FakeTransaction::returning([FakeResponse::Return(recorded)]),
        );

        let outcome = run_ready(service.execute(request(command_id(1), selected)))
            .expect("equal replay succeeds");
        assert_eq!(outcome, recorded);
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.session_calls, 1);
        assert_eq!(ids.frontier_calls, 1);
        assert_eq!(ids.semantic_entry_calls, 0);
        assert_eq!(transaction.observed.len(), 1);
    }

    /// S28 / INV-012: claimed cross-kind or changed-payload reuse passes
    /// through unchanged before semantic identity generation.
    #[test]
    fn s28_inv012_conflicting_reuse_is_typed_and_not_retried() {
        let conversation = imported_conversation();
        let selected = frontier(&conversation);
        let expected = CreateSessionFromImportedFrontierOutcome::ConflictingReuse {
            command_id: command_id(1),
        };
        let mut service = CreateSessionFromImportedFrontierService::new(
            FakeIds::new([session_id(30)], [], [context_frontier_id(40)]),
            FakeTransaction::returning([FakeResponse::Return(expected)]),
        );

        assert_eq!(
            run_ready(service.execute(request(command_id(1), selected))),
            Ok(expected)
        );
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.semantic_entry_calls, 0);
        assert_eq!(transaction.observed.len(), 1);
    }

    /// S28 / INV-039: a missing imported target remains a pre-claim terminal
    /// result and requests no semantic identities.
    #[test]
    fn s28_inv039_missing_target_passes_through_without_semantic_generation() {
        let conversation = imported_conversation();
        let selected = frontier(&conversation);
        let missing_conversation =
            CreateSessionFromImportedFrontierOutcome::ImportedConversationNotFound {
                conversation: selected.conversation(),
            };
        let missing_frontier = CreateSessionFromImportedFrontierOutcome::ImportedFrontierNotFound {
            frontier: selected,
        };
        let mut service = CreateSessionFromImportedFrontierService::new(
            FakeIds::new(
                [session_id(30), session_id(31)],
                [],
                [context_frontier_id(40), context_frontier_id(41)],
            ),
            FakeTransaction::returning([
                FakeResponse::Return(missing_conversation),
                FakeResponse::Return(missing_frontier),
            ]),
        );

        assert_eq!(
            run_ready(service.execute(request(command_id(1), selected))),
            Ok(missing_conversation)
        );
        assert_eq!(
            run_ready(service.execute(request(command_id(2), selected))),
            Ok(missing_frontier)
        );
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.session_calls, 2);
        assert_eq!(ids.frontier_calls, 2);
        assert_eq!(ids.semantic_entry_calls, 0);
        assert_eq!(transaction.observed.len(), 2);
    }

    /// S28: transaction failure is returned once with no application retry.
    #[test]
    fn s28_transaction_failure_is_returned_without_retry() {
        let conversation = imported_conversation();
        let selected = frontier(&conversation);
        let mut service = CreateSessionFromImportedFrontierService::new(
            FakeIds::new([session_id(30)], [], [context_frontier_id(40)]),
            FakeTransaction::returning([FakeResponse::Fail(FakeTransactionError::Unavailable)]),
        );

        assert_eq!(
            run_ready(service.execute(request(command_id(1), selected))),
            Err(FakeTransactionError::Unavailable)
        );
        let (ids, transaction) = service.into_parts();
        assert_eq!(ids.session_calls, 1);
        assert_eq!(ids.frontier_calls, 1);
        assert_eq!(ids.semantic_entry_calls, 0);
        assert_eq!(transaction.observed.len(), 1);
    }

    fn applied_result(
        conversation: &ImportedConversation,
        selected: ImportedTranscriptFrontier,
        session: SessionId,
    ) -> CreateSessionFromImportedFrontierAppliedResult {
        DomainCreateSessionFromImportedFrontier::new(
            command_id(1),
            selected,
            ImportedSessionRelationship::Resume,
            defaults(20),
        )
        .prepare(conversation, session, context_frontier_id(90), {
            let mut next = 100_u128;
            move || {
                let identity = semantic_entry_id(next);
                next += 1;
                identity
            }
        })
        .expect("fixture target prepares")
        .applied_result()
    }
}
