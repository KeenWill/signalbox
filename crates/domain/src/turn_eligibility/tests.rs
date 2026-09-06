//! Turn scheduling eligibility tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use expect_test::expect;
use expectable::print;

use super::*;
use crate::{
    AcceptedInputDisposition, AssistantText, AttemptEnd, CreateSessionFromImportedFrontier,
    CurrentTurnAttemptState, DescendantTerminationScope, FrozenModelSelection,
    ImportedConversation, ImportedConversationFormat, ImportedRawRecordPosition,
    ImportedRawSourceRecord, ImportedRecordEntryPosition, ImportedSessionReconstitutionInput,
    ImportedSessionRelationship, ImportedSourceAttestation, ImportedSourceMetadata,
    ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
    ImportedTranscriptContent, ImportedTranscriptEntryInput, ImportedTranscriptPosition,
    ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelSelectionOverride,
    ModelSelectionRequest, NormalizedToolArguments, PerInputConfigurationChoices,
    ResolvedProviderTarget, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionCreationCause, SessionCreationProvenance, SessionPlacement, SessionPlacementVersion,
    SessionReconstitutionInput, ToolApprovalDecision, ToolApprovalResolutionReconstitutionInput,
    ToolAttemptEnd, ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
    ToolEffectClass, ToolExecutionError, ToolExecutionErrorKind, ToolName, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, ToolResultContent, ToolResultText, VersionedSessionPlacement,
    test_support::{
        accepted_input_id, command_id, context_frontier_id, delegation_message_id, direct,
        imported_conversation_id, imported_transcript_entry_id, model_call_id,
        provider_model_identity, semantic_transcript_entry_id, session_id, tool_attempt_id,
        tool_request_id, transcript_frontier, turn_attempt_id, turn_id,
    },
};

fn current_session() -> Session {
    let session = session_id(1);
    let version = SessionConfigurationDefaultsVersion::first();
    let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1)));
    SessionReconstitutionInput::new(
        session,
        session,
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        session,
        version,
        session,
        version,
        defaults,
        crate::SessionPlacementReconstitutionFacts {
            current_pointer_session: session,
            current_pointer_version: crate::SessionPlacementVersion::INITIAL,
            selected_event_session: session,
            selected_event: crate::VersionedSessionPlacement::initial(
                crate::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .expect("test session facts are fully correlated")
}

fn imported_position(value: u64) -> ImportedTranscriptPosition {
    ImportedTranscriptPosition::try_from_u64(value).expect("test position is positive")
}

fn imported_raw_position(value: u64) -> ImportedRawRecordPosition {
    ImportedRawRecordPosition::try_from_u64(value).expect("test position is positive")
}

fn imported_source_event(
    conversation: crate::ImportedConversationId,
    identity: u128,
    ordinal: u64,
    source_type: &str,
) -> (ImportedRawSourceRecord, ImportedTranscriptEntryInput) {
    let source_type = ImportedText::new(source_type.to_owned());
    let normalized = ImportedStructuredValue::Object(
        vec![ImportedStructuredObjectMember::new(
            ImportedText::new("type".to_owned()),
            ImportedStructuredValue::String(source_type.clone()),
        )]
        .into_boxed_slice(),
    );
    let raw = ImportedRawSourceRecord::from_converted(
        format!("synthetic-scheduling-record-{ordinal}").into_bytes(),
        normalized,
    );
    let source = ImportedSourceMetadata::new(
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
        ImportedSourceAttestation::NotAttested,
    );
    let entry = ImportedTranscriptEntryInput::new(
        imported_transcript_entry_id(identity),
        conversation,
        imported_position(ordinal),
        imported_raw_position(ordinal),
        ImportedRecordEntryPosition::first(),
        ImportedSourceAttestation::NotAttested,
        ImportedTranscriptContent::SourceEvent {
            source_type: ImportedSourceAttestation::Attested(source_type),
        },
        source,
    );
    (raw, entry)
}

fn imported_session() -> ReconstitutedImportedSession {
    imported_session_for(1)
}

fn imported_session_for(session_value: u128) -> ReconstitutedImportedSession {
    let conversation_id = imported_conversation_id(80);
    let (first_raw, first_entry) = imported_source_event(conversation_id, 81, 1, "summary");
    let (second_raw, second_entry) = imported_source_event(conversation_id, 82, 2, "system");
    let conversation = ImportedConversation::from_converted_records(
        conversation_id,
        ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        vec![first_raw, second_raw],
        vec![first_entry, second_entry],
    )
    .expect("synthetic imported scheduling history is checked");
    let command = CreateSessionFromImportedFrontier::new(
        command_id(83),
        conversation
            .frontiers()
            .last()
            .expect("fixture has two imported frontiers"),
        ImportedSessionRelationship::Resume,
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1))),
    );
    let mut next_entry = 84_u128;
    let prepared = command
        .clone()
        .prepare(
            &conversation,
            session_id(session_value),
            context_frontier_id(89),
            || {
                let identity = semantic_transcript_entry_id(next_entry);
                next_entry += 1;
                identity
            },
        )
        .expect("matching imported history prepares a seed");
    let command_defaults = command.initial_configuration_defaults().clone();
    let seed = prepared.imported_seed();
    let snapshot = prepared.seed_snapshot();
    ImportedSessionReconstitutionInput::new(
        prepared.session().id(),
        prepared.session().id(),
        prepared.session().provenance(),
        prepared.session().id(),
        SessionConfigurationDefaultsVersion::first(),
        prepared.session().id(),
        SessionConfigurationDefaultsVersion::first(),
        command_defaults,
        crate::SessionPlacementReconstitutionFacts {
            current_pointer_session: prepared.session().id(),
            current_pointer_version: SessionPlacementVersion::INITIAL,
            selected_event_session: prepared.session().id(),
            selected_event: VersionedSessionPlacement::initial(SessionPlacement::pathless()),
        },
        conversation,
        vec![crate::ImportedSessionSeedReconstitutionInput::new(
            seed.session(),
            seed.seed_frontier(),
        )],
        vec![ResolvedContextFrontierReconstitutionInput::new(
            snapshot.frontier().owning_session(),
            snapshot.frontier().snapshot(),
            snapshot.ordered_entries().collect(),
        )],
        prepared
            .semantic_entries()
            .iter()
            .map(|entry| {
                SemanticTranscriptEntryReconstitutionInput::new(
                    entry.identity(),
                    entry.source_session(),
                    entry.payload().clone(),
                )
            })
            .collect(),
    )
    .reconstitute()
    .expect("complete imported scheduling fixture reconstitutes")
}

fn configuration(session: &Session) -> OriginConfiguration {
    let checked = session
        .current_configuration_defaults()
        .derive_request(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        )
        .expect("the test request names the current defaults");
    OriginConfiguration::freeze(checked, |_| None)
        .expect("a direct model selection does not consult aliases")
}

#[test]
fn delegated_activation_preserves_task_origin_and_first_session_lineage() {
    let child = current_session();
    let spawning_request = tool_request_id(401);
    let child_turn = turn_id(402);
    let task = DelegationContent::try_new(String::from("inspect delegated work"))
        .expect("fixture task is valid");
    let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(403),
        child.id(),
        SemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request,
            parent_session: session_id(404),
            parent_turn: turn_id(405),
            content: task.clone(),
        },
    );
    let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
        session: child.id(),
        turn: child_turn,
        spawning_request,
        task: task.clone(),
        task_entry,
        configuration: configuration(&child),
        starting_frontier: context_frontier_id(406),
        initial_attempt: turn_attempt_id(407),
    })
    .expect("exact delegated task facts prepare activation");
    let (active, origin, snapshot) = prepared.into_parts();

    assert_eq!(active.session(), child.id());
    assert_eq!(active.turn(), child_turn);
    assert_eq!(active.spawning_request(), Some(spawning_request));
    assert_eq!(active.task(), Some(&task));
    assert_eq!(
        active.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(snapshot.entry_count(), 1);
    assert_eq!(origin.len(), 1);
    assert_eq!(
        origin.first().unwrap().reference(),
        snapshot.ordered_entries().next().unwrap()
    );
}

#[test]
fn delegated_activation_reconstitutes_consumed_steering() {
    let child = current_session();
    let child_turn = turn_id(408);
    let task = DelegationContent::try_new(String::from("inspect delegated steering"))
        .expect("fixture task is valid");
    let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(409),
        child.id(),
        SemanticTranscriptEntryPayload::DelegatedTask {
            spawning_request: tool_request_id(410),
            parent_session: session_id(411),
            parent_turn: turn_id(412),
            content: task.clone(),
        },
    );
    let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
        session: child.id(),
        turn: child_turn,
        spawning_request: tool_request_id(410),
        task,
        task_entry,
        configuration: configuration(&child),
        starting_frontier: context_frontier_id(413),
        initial_attempt: turn_attempt_id(414),
    })
    .expect("exact delegated task facts prepare activation");
    let consumed_input = accepted_input_id(415);
    let consuming_call = model_call_id(416);
    let position = SessionInputPosition::try_from_u64(2).unwrap();
    let consumed = ConsumedSteeringReconstitutionInput::new(
        child.id(),
        AcceptedInputLifecycle::new(
            consumed_input,
            AcceptedInputDisposition::ConsumedAsSteering {
                call: consuming_call,
            },
        ),
        position,
        child_turn,
    );

    let active = prepared
        .into_parts()
        .0
        .with_consumed_steering(vec![consumed])
        .expect("stored steering targets the delegated turn");

    assert_eq!(active.consumed_steering().len(), 1);
    assert_eq!(
        active.consumed_steering()[0].accepted_input(),
        consumed_input
    );
    assert_eq!(
        active.consumed_steering()[0].acceptance_position(),
        position
    );
    assert_eq!(active.consumed_steering()[0].source_turn(), child_turn);
}

#[test]
fn delegated_wake_activation_preserves_delivery_range_and_predecessor_lineage() {
    let recipient = current_session();
    let predecessor = turn_id(411);
    let predecessor_entry =
        SemanticTranscriptEntryRef::from_source(recipient.id(), semantic_transcript_entry_id(412));
    let predecessor_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        recipient.id(),
        context_frontier_id(413),
        vec![predecessor_entry],
    )
    .expect("fixture predecessor snapshot is valid");
    let first_sequence = NonZeroU64::new(1).unwrap();
    let through_sequence = NonZeroU64::new(2).unwrap();
    let first_delivery = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(414),
        recipient.id(),
        SemanticTranscriptEntryPayload::DelegationMessage {
            spawning_request: tool_request_id(415),
            message: delegation_message_id(416),
            sender: session_id(417),
            recipient: recipient.id(),
            delivery_sequence: first_sequence,
            content: DelegationContent::try_new(String::from("first wake message")).unwrap(),
        },
    );
    let through_delivery = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_transcript_entry_id(418),
        recipient.id(),
        SemanticTranscriptEntryPayload::DelegationMessage {
            spawning_request: tool_request_id(415),
            message: delegation_message_id(419),
            sender: session_id(417),
            recipient: recipient.id(),
            delivery_sequence: through_sequence,
            content: DelegationContent::try_new(String::from("second wake message")).unwrap(),
        },
    );
    let prepared =
        PreparedDelegatedTurnActivation::prepare_wake(DelegatedWakeTurnActivationInput {
            session: recipient.id(),
            turn: turn_id(420),
            first_delivery_sequence: first_sequence,
            through_delivery_sequence: through_sequence,
            deliveries: vec![first_delivery, through_delivery],
            predecessor,
            predecessor_snapshot,
            configuration: configuration(&recipient),
            starting_frontier: context_frontier_id(421),
            initial_attempt: turn_attempt_id(422),
        })
        .expect("contiguous checked deliveries prepare a wake activation");
    let (active, entries, snapshot) = prepared.into_parts();

    assert_eq!(active.spawning_request(), None);
    assert_eq!(active.task(), None);
    assert_eq!(
        active.delivery_range(),
        Some((first_sequence, through_sequence))
    );
    assert_eq!(
        active.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor
        }
    );
    assert_eq!(entries.len(), 2);
    assert_eq!(snapshot.entry_count(), 3);
    assert_eq!(
        snapshot.immediate_semantic_prefix().unwrap().snapshot(),
        context_frontier_id(413)
    );
}

fn default_origin_delivery() -> DeliveryRequest {
    DeliveryRequest::StartWhenNoActiveTurn {
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    }
}

/// One accepted turn origin whose sole identity/order knob is its
/// acceptance ordinal. Turn and accepted-input identities descend as the
/// ordinal ascends, so identity order cannot accidentally stand in for
/// durable acceptance order (`docs/agents/testing-style.md`, rule 4).
#[derive(Clone, Copy)]
struct OriginFixture {
    acceptance: u64,
}

fn accepted_origin(acceptance: u64) -> OriginFixture {
    OriginFixture { acceptance }
}

impl OriginFixture {
    fn turn(self) -> TurnId {
        turn_id(u128::from(u64::MAX - self.acceptance))
    }

    fn accepted_input(self) -> AcceptedInputId {
        accepted_input_id(u128::from(u64::MAX / 2 - self.acceptance))
    }

    fn position(self) -> SessionInputPosition {
        SessionInputPosition::try_from_u64(self.acceptance)
            .expect("test acceptance ordinals are positive")
    }

    fn ordinary_order(self) -> AcceptedInputQueueOrder {
        AcceptedInputQueueOrder::ordinary(self.position())
    }

    fn record(
        self,
        session: &Session,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> AcceptedInputTurnSchedulingRecord {
        self.record_with(
            session,
            OriginRecordFacts {
                order: self.ordinary_order(),
                delivery: default_origin_delivery(),
                state,
            },
        )
    }

    fn record_with(
        self,
        session: &Session,
        facts: OriginRecordFacts,
    ) -> AcceptedInputTurnSchedulingRecord {
        let turn = self.turn();
        AcceptedInputTurnSchedulingRecord::new(
            session.id(),
            turn,
            session.id(),
            AcceptedInputLifecycle::new(
                self.accepted_input(),
                AcceptedInputDisposition::OriginOf(turn),
            ),
            session.id(),
            turn,
            facts.order,
            facts.delivery,
            configuration(session),
            facts.state,
        )
    }

    fn entry(
        self,
        session: &Session,
        entry: SemanticEntryFixture,
    ) -> SemanticTranscriptEntryReconstitutionInput {
        SemanticTranscriptEntryReconstitutionInput::new(
            entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: self.accepted_input(),
            },
        )
    }

    fn active_tail(self, session: &Session) -> SessionAcceptanceTailReconstitutionInput {
        SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            self.accepted_input(),
            self.position(),
            vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    self.accepted_input(),
                    AcceptedInputDisposition::OriginOf(self.turn()),
                ),
                self.position(),
                default_origin_delivery(),
            )],
        )
    }
}

struct OriginRecordFacts {
    order: AcceptedInputQueueOrder,
    delivery: DeliveryRequest,
    state: AcceptedInputTurnSchedulingRecordState,
}

#[derive(Clone, Copy)]
struct SemanticEntryFixture {
    seed: u128,
}

fn semantic_entry(seed: u128) -> SemanticEntryFixture {
    SemanticEntryFixture { seed }
}

fn user_denial(request: ToolRequestId) -> ToolApprovalResolution {
    ToolApprovalResolutionReconstitutionInput::user_fixture(
        request,
        ToolApprovalDecision::Deny { reason: None },
    )
    .reconstitute()
    .expect("the user denial fixture is valid")
}

impl SemanticEntryFixture {
    fn id(self) -> SemanticTranscriptEntryId {
        semantic_transcript_entry_id(self.seed)
    }

    fn reference(self, session: &Session) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(session.id(), self.id())
    }

    fn failed_turn(
        self,
        session: &Session,
        turn: OriginFixture,
    ) -> SemanticTranscriptEntryReconstitutionInput {
        SemanticTranscriptEntryReconstitutionInput::new(
            self.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn: turn.turn() },
        )
    }
}

#[derive(Clone, Copy)]
struct FrontierFixture {
    seed: u128,
}

fn frontier(seed: u128) -> FrontierFixture {
    FrontierFixture { seed }
}

impl FrontierFixture {
    fn id(self) -> ContextFrontierId {
        context_frontier_id(self.seed)
    }

    fn snapshot(
        self,
        session: &Session,
        entries: &[SemanticEntryFixture],
    ) -> ResolvedContextFrontierReconstitutionInput {
        ResolvedContextFrontierReconstitutionInput::new(
            session.id(),
            self.id(),
            entries
                .iter()
                .map(|entry| entry.reference(session))
                .collect(),
        )
    }
}

#[derive(Clone, Copy)]
struct ActivationFixture {
    seed: u128,
}

fn activation(seed: u128) -> ActivationFixture {
    ActivationFixture { seed }
}

fn matching_active_attempt() -> TurnAttemptId {
    turn_attempt_id(50)
}

impl ActivationFixture {
    fn model_identity_entry(self) -> SemanticEntryFixture {
        semantic_entry(50 + self.seed)
    }

    fn origin_entry(self) -> SemanticEntryFixture {
        semantic_entry(100 + self.seed)
    }

    fn starting_frontier(self) -> FrontierFixture {
        frontier(200 + self.seed)
    }

    fn initial_attempt(self) -> TurnAttemptId {
        turn_attempt_id(300 + self.seed)
    }

    fn identities(self) -> AcceptedInputTurnActivationIdentities {
        AcceptedInputTurnActivationIdentities::new(
            self.model_identity_entry().id(),
            self.origin_entry().id(),
            self.starting_frontier().id(),
            self.initial_attempt(),
        )
    }

    fn identities_with_attempt(
        self,
        initial_attempt: TurnAttemptId,
    ) -> AcceptedInputTurnActivationIdentities {
        AcceptedInputTurnActivationIdentities::new(
            self.model_identity_entry().id(),
            self.origin_entry().id(),
            self.starting_frontier().id(),
            initial_attempt,
        )
    }

    fn identities_with_origin_entry(
        self,
        origin_entry: SemanticTranscriptEntryId,
    ) -> AcceptedInputTurnActivationIdentities {
        AcceptedInputTurnActivationIdentities::new(
            self.model_identity_entry().id(),
            origin_entry,
            self.starting_frontier().id(),
            self.initial_attempt(),
        )
    }

    fn identities_with_starting_frontier(
        self,
        starting_frontier: ContextFrontierId,
    ) -> AcceptedInputTurnActivationIdentities {
        AcceptedInputTurnActivationIdentities::new(
            self.model_identity_entry().id(),
            self.origin_entry().id(),
            starting_frontier,
            self.initial_attempt(),
        )
    }
}

#[derive(Clone)]
struct ActiveReconstitutionFacts {
    session: Session,
    turns: Vec<AcceptedInputTurnSchedulingRecord>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
}

impl ActiveReconstitutionFacts {
    /// The origin-entry fixture the matching baseline stores for its
    /// active turn.
    fn matching_origin_entry() -> SemanticEntryFixture {
        semantic_entry(30)
    }

    /// The starting-snapshot fixture the matching baseline stores for
    /// its active turn.
    fn matching_starting_frontier() -> FrontierFixture {
        frontier(40)
    }

    fn matching(session: &Session, active: OriginFixture) -> Self {
        let origin_entry = Self::matching_origin_entry();
        let starting_frontier = Self::matching_starting_frontier();
        Self {
            session: session.clone(),
            turns: vec![active.record(
                session,
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: starting_frontier.id(),
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        active.turn(),
                        matching_active_attempt(),
                    ),
                },
            )],
            semantic_entries: vec![active.entry(session, origin_entry)],
            snapshots: vec![starting_frontier.snapshot(session, &[origin_entry])],
            acceptance_tail: Some(active.active_tail(session)),
        }
    }

    /// Replaces only the behavior-relevant stored active phase while
    /// retaining every matching identity, lineage, frontier, origin,
    /// configuration, and acceptance-tail fact.
    fn replace_active_phase(&mut self, replacement: ActiveTurnSchedulingReconstitutionInput) {
        let AcceptedInputTurnSchedulingRecordState::Active { phase, .. } = &mut self.turns[0].state
        else {
            panic!("matching active facts retain an active scheduling record");
        };
        *phase = replacement;
    }

    /// Replaces only the stored starting lineage while retaining every
    /// other matching fact.
    fn replace_starting_lineage(&mut self, replacement: AcceptedInputStartingLineage) {
        let AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage, ..
        } = &mut self.turns[0].state
        else {
            panic!("matching active facts retain an active scheduling record");
        };
        *starting_lineage = replacement;
    }

    /// Replaces only the stored starting-snapshot identity while
    /// retaining every other matching fact.
    fn replace_starting_frontier(&mut self, replacement: ContextFrontierId) {
        let AcceptedInputTurnSchedulingRecordState::Active {
            starting_frontier, ..
        } = &mut self.turns[0].state
        else {
            panic!("matching active facts retain an active scheduling record");
        };
        *starting_frontier = replacement;
    }

    fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
        AcceptedInputSchedulingReconstitutionInput::new(
            self.session,
            self.turns,
            self.semantic_entries,
            self.snapshots,
            self.acceptance_tail,
        )
    }
}

#[derive(Clone)]
struct ConsumedSteeringReconstitutionFacts {
    session: Session,
    turns: Vec<AcceptedInputTurnSchedulingRecord>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    acceptance_tail: SessionAcceptanceTailReconstitutionInput,
    pinned_targets: Vec<crate::PinnedProviderTargetReconstitutionInput>,
    model_calls: Vec<ModelCallReconstitutionInput>,
    consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    steering_continuation_rounds: Vec<SteeringContinuationRoundReconstitutionInput>,
}

impl ConsumedSteeringReconstitutionFacts {
    fn matching(session: &Session, active: OriginFixture, consumed: OriginFixture) -> Self {
        let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
        let steering_entry = semantic_entry(31);
        let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();
        let call_frontier = frontier(41);
        let call_id = model_call_id(91);
        let target = ResolvedProviderTarget::naming(provider_model_identity(51));
        let consumed_lifecycle = AcceptedInputLifecycle::new(
            consumed.accepted_input(),
            AcceptedInputDisposition::ConsumedAsSteering { call: call_id },
        );
        let mut acceptance_tail = active.active_tail(session);
        acceptance_tail.observed_last_position = consumed.position();
        acceptance_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                consumed_lifecycle.clone(),
                consumed.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        Self {
            session: session.clone(),
            turns: vec![active.record(
                session,
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: starting_frontier.id(),
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        active.turn(),
                        matching_active_attempt(),
                    ),
                },
            )],
            semantic_entries: vec![
                active.entry(session, origin_entry),
                SemanticTranscriptEntryReconstitutionInput::new(
                    steering_entry.id(),
                    session.id(),
                    InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                        accepted_input: consumed.accepted_input(),
                        source_turn: active.turn(),
                    },
                ),
            ],
            snapshots: vec![
                starting_frontier.snapshot(session, &[origin_entry]),
                call_frontier.snapshot(session, &[origin_entry, steering_entry]),
            ],
            acceptance_tail,
            pinned_targets: vec![crate::PinnedProviderTargetReconstitutionInput::new(
                active.turn(),
                target,
            )],
            model_calls: vec![ModelCallReconstitutionInput::new(
                call_id,
                active.turn(),
                matching_active_attempt(),
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Prepared,
            )],
            consumed_steering: vec![ConsumedSteeringReconstitutionInput::new(
                session.id(),
                consumed_lifecycle,
                consumed.position(),
                active.turn(),
            )],
            steering_continuation_rounds: Vec::new(),
        }
    }

    /// Matching stored facts for one steering input consumed at a
    /// tool-round continuation boundary: the completed producing call's
    /// proposal, its executed result, and the consumed steering entry fill
    /// the prepared continuation call's frontier exactly, and the round's
    /// result evidence backs that window.
    fn matching_at_continuation(
        session: &Session,
        active: OriginFixture,
        consumed: OriginFixture,
    ) -> Self {
        let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
        let steering_entry = semantic_entry(31);
        let tool_use_entry = semantic_entry(34);
        let result_entry = semantic_entry(35);
        let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();
        let call_frontier = frontier(41);
        let producing_call = Self::matching_continuation_producing_call();
        let producing_attempt = turn_attempt_id(49);
        let call_id = Self::matching_continuation_call();
        let request = Self::matching_continuation_request();
        let target = ResolvedProviderTarget::naming(provider_model_identity(51));
        let mut facts = Self::matching(session, active, consumed);
        facts.turns[0].state = AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            phase: ActiveTurnSchedulingReconstitutionInput::running(
                active.turn(),
                matching_active_attempt(),
            ),
        };
        facts.semantic_entries.extend([
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: Self::matching_continuation_tool_attempt(),
                },
            ),
        ]);
        facts.snapshots[1] = call_frontier.snapshot(
            session,
            &[origin_entry, tool_use_entry, result_entry, steering_entry],
        );
        facts.model_calls = vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                active.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                call_id,
                active.turn(),
                matching_active_attempt(),
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Prepared,
            ),
        ];
        facts.steering_continuation_rounds =
            vec![SteeringContinuationRoundReconstitutionInput::new(
                call_id,
                vec![Self::matching_continuation_round_attempt(session, active)],
                Vec::new(),
            )];
        facts
    }

    /// The completed producing call the continuation baseline stores.
    fn matching_continuation_producing_call() -> crate::ModelCallId {
        model_call_id(90)
    }

    /// The steering-consuming continuation call the baseline stores.
    fn matching_continuation_call() -> crate::ModelCallId {
        model_call_id(91)
    }

    /// The single proposed request the continuation baseline stores.
    fn matching_continuation_request() -> ToolRequestId {
        tool_request_id(92)
    }

    /// The executed tool attempt the continuation baseline stores.
    fn matching_continuation_tool_attempt() -> crate::ToolAttemptId {
        tool_attempt_id(93)
    }

    /// The ended tool attempt backing the baseline's result window.
    fn matching_continuation_round_attempt(
        session: &Session,
        active: OriginFixture,
    ) -> crate::EndedToolAttempt {
        ended_tool_attempt(
            session,
            active,
            matching_active_attempt(),
            Self::matching_continuation_tool_attempt(),
            Self::matching_continuation_request(),
        )
    }

    fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
        AcceptedInputSchedulingReconstitutionInput::new(
            self.session,
            self.turns,
            self.semantic_entries,
            self.snapshots,
            Some(self.acceptance_tail),
        )
        .with_model_call_facts(self.pinned_targets, self.model_calls)
        .with_consumed_steering_facts(self.consumed_steering)
        .with_steering_continuation_rounds(self.steering_continuation_rounds)
    }
}

/// One ended, completed tool attempt correlated to the given request for
/// continuation-round evidence.
fn ended_tool_attempt(
    session: &Session,
    turn: OriginFixture,
    issuing_attempt: TurnAttemptId,
    attempt: crate::ToolAttemptId,
    request: ToolRequestId,
) -> crate::EndedToolAttempt {
    ended_tool_attempt_with_end(
        session,
        turn,
        issuing_attempt,
        attempt,
        request,
        ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("ok")).expect("fixture tool result is valid"),
            ),
        },
    )
}

fn ended_tool_attempt_with_end(
    session: &Session,
    turn: OriginFixture,
    issuing_attempt: TurnAttemptId,
    attempt: crate::ToolAttemptId,
    request: ToolRequestId,
    end: ToolAttemptEnd,
) -> crate::EndedToolAttempt {
    let reconstituted = ToolAttemptReconstitutionInput::new(
        attempt,
        request,
        session.id(),
        turn.turn(),
        issuing_attempt,
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(end),
    )
    .reconstitute()
    .expect("fixture tool attempt is supported");
    let crate::ReconstitutedToolAttempt::Ended(ended) = reconstituted else {
        panic!("fixture tool attempt is terminal");
    };
    ended
}

fn active_input(
    session: &Session,
    active: OriginFixture,
    acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
) -> AcceptedInputSchedulingReconstitutionInput {
    ActiveReconstitutionFacts {
        acceptance_tail,
        ..ActiveReconstitutionFacts::matching(session, active)
    }
    .input()
}

/// One-record queued scheduling input: a queued turn stores no semantic
/// entries, snapshots, or acceptance tail, so those collections are
/// canonically empty here.
fn queued_input(
    session: &Session,
    queued: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record(session, AcceptedInputTurnSchedulingRecordState::Queued)],
        Vec::new(),
        Vec::new(),
        None,
    )
}

/// Matching stored facts for one first-in-session failed-terminal turn:
/// its origin entry, failed marker, starting snapshot, and terminal
/// snapshot agree with each other, so each perturbation changes exactly
/// one stored fact.
#[derive(Clone)]
struct FailedTerminalReconstitutionFacts {
    session: Session,
    turns: Vec<AcceptedInputTurnSchedulingRecord>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
}

impl FailedTerminalReconstitutionFacts {
    /// The origin-entry fixture the matching baseline stores for its
    /// failed turn.
    fn matching_origin_entry() -> SemanticEntryFixture {
        semantic_entry(30)
    }

    /// The failed-marker fixture the matching baseline stores for its
    /// failed turn.
    fn matching_failure_entry() -> SemanticEntryFixture {
        semantic_entry(31)
    }

    /// The starting-snapshot fixture the matching baseline stores for
    /// its failed turn.
    fn matching_starting_frontier() -> FrontierFixture {
        frontier(40)
    }

    /// The terminal-snapshot fixture the matching baseline stores for
    /// its failed turn.
    fn matching_terminal_frontier() -> FrontierFixture {
        frontier(41)
    }

    fn matching(session: &Session, failed: OriginFixture) -> Self {
        let origin_entry = Self::matching_origin_entry();
        let failure_entry = Self::matching_failure_entry();
        let starting_frontier = Self::matching_starting_frontier();
        let terminal_frontier = Self::matching_terminal_frontier();
        Self {
            session: session.clone(),
            turns: vec![failed.record(
                session,
                AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: starting_frontier.id(),
                    terminal_execution: None,
                    terminal_frontier: terminal_frontier.id(),
                },
            )],
            semantic_entries: vec![
                failed.entry(session, origin_entry),
                failure_entry.failed_turn(session, failed),
            ],
            snapshots: vec![
                starting_frontier.snapshot(session, &[origin_entry]),
                terminal_frontier.snapshot(session, &[origin_entry, failure_entry]),
            ],
            acceptance_tail: None,
        }
    }

    /// Replaces only the stored terminal-snapshot identity while
    /// retaining every other matching fact.
    fn replace_terminal_frontier(&mut self, replacement: ContextFrontierId) {
        let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_frontier, ..
        } = &mut self.turns[0].state
        else {
            panic!("matching failed-terminal facts retain a terminal scheduling record");
        };
        *terminal_frontier = replacement;
    }

    /// Replaces only the stored terminal execution provenance while
    /// retaining every semantic and frontier fact.
    fn replace_terminal_execution(
        &mut self,
        replacement: Option<FailedTurnExecutionReconstitutionInput>,
    ) {
        let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution, ..
        } = &mut self.turns[0].state
        else {
            panic!("matching failed-terminal facts retain a terminal scheduling record");
        };
        *terminal_execution = replacement;
    }

    fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
        AcceptedInputSchedulingReconstitutionInput::new(
            self.session,
            self.turns,
            self.semantic_entries,
            self.snapshots,
            self.acceptance_tail,
        )
    }
}

#[derive(Clone, Copy)]
struct PostAnchorOrigins {
    active: OriginFixture,
    queued: OriginFixture,
}

fn active_input_with_post_anchor_origin(
    session: &Session,
    origins: PostAnchorOrigins,
    delivery: DeliveryRequest,
) -> AcceptedInputSchedulingReconstitutionInput {
    let mut facts = ActiveReconstitutionFacts::matching(session, origins.active);
    let tail = facts
        .acceptance_tail
        .as_mut()
        .expect("matching active facts include the acceptance tail");
    tail.observed_last_position = origins.queued.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                origins.queued.accepted_input(),
                AcceptedInputDisposition::OriginOf(origins.queued.turn()),
            ),
            origins.queued.position(),
            delivery,
        ));
    facts.turns.push(origins.queued.record_with(
        session,
        OriginRecordFacts {
            order: origins.queued.ordinary_order(),
            delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    ));
    facts.input()
}

#[derive(Clone, Copy)]
struct FailedPredecessorPostAnchorOrigins {
    predecessor: OriginFixture,
    active: OriginFixture,
    queued: OriginFixture,
}

fn active_input_after_failed_predecessor_with_post_anchor_origin(
    session: &Session,
    origins: FailedPredecessorPostAnchorOrigins,
    delivery: DeliveryRequest,
) -> AcceptedInputSchedulingReconstitutionInput {
    let predecessor_origin_entry = semantic_entry(29);
    let predecessor_failure_entry = semantic_entry(30);
    let active_origin_entry = semantic_entry(31);
    let predecessor_starting_frontier = frontier(39);
    let predecessor_terminal_frontier = frontier(40);
    let active_starting_frontier = frontier(41);
    let predecessor_record = origins.predecessor.record(
        session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: predecessor_starting_frontier.id(),
            terminal_execution: None,
            terminal_frontier: predecessor_terminal_frontier.id(),
        },
    );
    let active_delivery = DeliveryRequest::AfterCurrentTurn {
        expected_active_turn: origins.predecessor.turn(),
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let active_record = origins.active.record_with(
        session,
        OriginRecordFacts {
            order: origins.active.ordinary_order(),
            delivery: active_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::After {
                    immediate_predecessor: origins.predecessor.turn(),
                },
                starting_frontier: active_starting_frontier.id(),
                phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                    origins.active.turn(),
                    turn_attempt_id(50),
                ),
            },
        },
    );
    let queued_record = origins.queued.record_with(
        session,
        OriginRecordFacts {
            order: origins.queued.ordinary_order(),
            delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let tail = SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        origins.active.accepted_input(),
        origins.queued.position(),
        vec![
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    origins.active.accepted_input(),
                    AcceptedInputDisposition::OriginOf(origins.active.turn()),
                ),
                origins.active.position(),
                active_delivery,
            ),
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    origins.queued.accepted_input(),
                    AcceptedInputDisposition::OriginOf(origins.queued.turn()),
                ),
                origins.queued.position(),
                delivery,
            ),
        ],
    );
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![predecessor_record, active_record, queued_record],
        vec![
            origins.predecessor.entry(session, predecessor_origin_entry),
            predecessor_failure_entry.failed_turn(session, origins.predecessor),
            origins.active.entry(session, active_origin_entry),
        ],
        vec![
            predecessor_starting_frontier.snapshot(session, &[predecessor_origin_entry]),
            predecessor_terminal_frontier.snapshot(
                session,
                &[predecessor_origin_entry, predecessor_failure_entry],
            ),
            active_starting_frontier.snapshot(
                session,
                &[
                    predecessor_origin_entry,
                    predecessor_failure_entry,
                    active_origin_entry,
                ],
            ),
        ],
        Some(tail),
    )
}

fn active_input_after_historical_interrupt(
    session: &Session,
    predecessor: OriginFixture,
    active: OriginFixture,
    interrupt_successor: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let predecessor_origin_entry = semantic_entry(20);
    let predecessor_failure_entry = semantic_entry(21);
    let interrupt_origin_entry = semantic_entry(22);
    let interrupt_failure_entry = semantic_entry(23);
    let active_origin_entry = semantic_entry(24);
    let predecessor_starting_frontier = frontier(30);
    let predecessor_terminal_frontier = frontier(31);
    let interrupt_starting_frontier = frontier(32);
    let interrupt_terminal_frontier = frontier(33);
    let active_starting_frontier = frontier(34);
    let interrupt_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        interrupt_successor.position(),
        predecessor.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(40),
        session.id(),
        predecessor.turn(),
        interrupt_successor.accepted_input(),
        interrupt_successor.turn(),
        interrupt_order,
    )
    .expect("the historical interrupt is exactly correlated");
    let active_delivery = DeliveryRequest::AfterCurrentTurn {
        expected_active_turn: predecessor.turn(),
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let interrupt_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: predecessor.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };

    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![
            predecessor.record(
                session,
                AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: predecessor_starting_frontier.id(),
                    terminal_execution: Some(
                        FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                            predecessor.turn(),
                            turn_attempt_id(40),
                            CancellationStopDisposition::KnownFailure,
                            interrupt,
                        ),
                    ),
                    terminal_frontier: predecessor_terminal_frontier.id(),
                },
            ),
            active.record_with(
                session,
                OriginRecordFacts {
                    order: active.ordinary_order(),
                    delivery: active_delivery,
                    state: AcceptedInputTurnSchedulingRecordState::Active {
                        starting_lineage: AcceptedInputStartingLineage::After {
                            immediate_predecessor: interrupt_successor.turn(),
                        },
                        starting_frontier: active_starting_frontier.id(),
                        phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                            active.turn(),
                            turn_attempt_id(41),
                        ),
                    },
                },
            ),
            interrupt_successor.record_with(
                session,
                OriginRecordFacts {
                    order: interrupt_order,
                    delivery: interrupt_delivery,
                    state: AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                        starting_lineage: AcceptedInputStartingLineage::After {
                            immediate_predecessor: predecessor.turn(),
                        },
                        starting_frontier: interrupt_starting_frontier.id(),
                        terminal_execution: None,
                        terminal_frontier: interrupt_terminal_frontier.id(),
                    },
                },
            ),
        ],
        vec![
            predecessor.entry(session, predecessor_origin_entry),
            predecessor_failure_entry.failed_turn(session, predecessor),
            interrupt_successor.entry(session, interrupt_origin_entry),
            interrupt_failure_entry.failed_turn(session, interrupt_successor),
            active.entry(session, active_origin_entry),
        ],
        vec![
            predecessor_starting_frontier.snapshot(session, &[predecessor_origin_entry]),
            predecessor_terminal_frontier.snapshot(
                session,
                &[predecessor_origin_entry, predecessor_failure_entry],
            ),
            interrupt_starting_frontier.snapshot(
                session,
                &[
                    predecessor_origin_entry,
                    predecessor_failure_entry,
                    interrupt_origin_entry,
                ],
            ),
            interrupt_terminal_frontier.snapshot(
                session,
                &[
                    predecessor_origin_entry,
                    predecessor_failure_entry,
                    interrupt_origin_entry,
                    interrupt_failure_entry,
                ],
            ),
            active_starting_frontier.snapshot(
                session,
                &[
                    predecessor_origin_entry,
                    predecessor_failure_entry,
                    interrupt_origin_entry,
                    interrupt_failure_entry,
                    active_origin_entry,
                ],
            ),
        ],
        Some(SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            active.accepted_input(),
            interrupt_successor.position(),
            vec![
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        active.accepted_input(),
                        AcceptedInputDisposition::OriginOf(active.turn()),
                    ),
                    active.position(),
                    active_delivery,
                ),
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        interrupt_successor.accepted_input(),
                        AcceptedInputDisposition::OriginOf(interrupt_successor.turn()),
                    ),
                    interrupt_successor.position(),
                    interrupt_delivery,
                ),
            ],
        )),
    )
}

#[derive(Debug, serde::Serialize)]
struct ReconstitutionFailureRow {
    perturbed_stored_fact: &'static str,
    failure: String,
}

/// Asserts one perturbed complete input rejects while retaining every
/// supplied fact unchanged, then returns its precise failure.
#[track_caller]
fn assert_input_rejects_unchanged(
    input: AcceptedInputSchedulingReconstitutionInput,
) -> AcceptedInputSchedulingReconstitutionFailure {
    let error = input
        .clone()
        .reconstitute()
        .expect_err("perturbed scheduling facts must fail closed");
    let failure = error.failure().clone();
    assert_eq!(error.input(), &input);
    let (returned, returned_failure) = error.into_parts();
    assert_eq!(returned, input);
    assert_eq!(returned_failure, failure);
    failure
}

/// Asserts one named perturbation rejects while retaining the complete
/// unchanged input, then returns its precise failure.
#[track_caller]
fn assert_reconstitution_rejects_unchanged(
    facts: ActiveReconstitutionFacts,
) -> AcceptedInputSchedulingReconstitutionFailure {
    assert_input_rejects_unchanged(facts.input())
}

/// Asserts eligibility preparation rejects while retaining the complete
/// projection and supplied identities unchanged, then returns the exact
/// failure.
#[track_caller]
fn assert_eligibility_rejects_unchanged(
    projection: AcceptedInputSchedulingProjection,
    identities: AcceptedInputTurnActivationIdentities,
) -> AcceptedInputEligibilityFailure {
    let error = projection
        .clone()
        .prepare_earliest_queued_activation(identities)
        .expect_err("ineligible or colliding activation facts must fail closed");
    let failure = error.failure();
    assert_eq!(error.projection(), &projection);
    assert_eq!(error.identities(), identities);
    let (returned_projection, returned_identities, returned_failure) = error.into_parts();
    assert_eq!(returned_projection, projection);
    assert_eq!(returned_identities, identities);
    assert_eq!(returned_failure, failure);
    failure
}

/// ancestry-free first eligibility fixes the
/// origin-only frontier and enters Running with one Prepared attempt in
/// the same sealed candidate.
#[test]
fn first_eligibility_prepares_one_atomic_activation_candidate() {
    let session = current_session();
    let queued = accepted_origin(1);
    let activation = activation(1);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued)],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let candidate = input
        .reconstitute()
        .expect("a complete queued projection is valid")
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the sole queued turn is eligible with no active slot");

    assert_eq!(candidate.turn().turn(), queued.turn());
    assert_eq!(
        candidate.turn().accepted_input().id(),
        queued.accepted_input()
    );
    assert_eq!(
        candidate.origin_entry().payload(),
        &InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: queued.accepted_input(),
        }
    );
    assert_eq!(
        candidate.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![activation.origin_entry().reference(&session)]
    );
    assert!(matches!(
        candidate.turn().phase(),
        ActiveTurnPhase::Running { current_attempt }
            if current_attempt.id() == activation.initial_attempt()
                && current_attempt.state() == &crate::CurrentTurnAttemptState::Prepared
    ));
}

/// an imported session's first native activation
/// appends its origin to the exact checked seed prefix without changing
/// first-in-session lineage.
#[test]
fn first_native_frontier_appends_to_imported_seed() {
    let imported = imported_session();
    let session = imported.session().clone();
    let seed_entries = imported
        .seed_snapshot()
        .ordered_entries()
        .collect::<Vec<_>>();
    let queued = accepted_origin(1);
    let activation = activation(1);

    let candidate = queued_input(&session, queued)
        .with_imported_session(imported)
        .reconstitute()
        .expect("the exact imported seed admits queued native work")
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the first native turn appends to the imported seed");

    let mut expected = seed_entries;
    expected.push(activation.origin_entry().reference(&session));
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        candidate.start().lineage(),
        AcceptedInputStartingLineage::FirstInSession
    );
}

/// restart returns a queued scheduling projection with no
/// manufactured start, and a cross-wired OriginOf fact fails closed.
#[test]
fn checked_reconstitution_preserves_queued_state_and_exact_origin() {
    let session = current_session();
    let origin = accepted_origin(1);
    let queued = origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.clone()],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect("the complete queued record is valid");
    let reconstituted = projection
        .turn(origin.turn())
        .expect("the stored queued turn remains present");
    assert_eq!(
        reconstituted.status(),
        AcceptedInputTurnSchedulingStatus::Queued
    );
    assert_eq!(reconstituted.start(), None);

    let wrong_turn = turn_id(99);
    let cross_wired = AcceptedInputTurnSchedulingRecord::new(
        queued.stored_session(),
        queued.turn(),
        queued.accepted_input_session(),
        AcceptedInputLifecycle::new(
            queued.accepted_input().id(),
            AcceptedInputDisposition::OriginOf(wrong_turn),
        ),
        queued.queue_session(),
        queued.queue_turn(),
        queued.order(),
        queued.origin_delivery(),
        queued.origin_configuration().clone(),
        queued.state().clone(),
    );
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        vec![cross_wired],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect_err("the exact OriginOf(turn) correlation is required");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
            turn: origin.turn(),
        }
    );
}

/// an admitted active restart record owns its exact
/// Prepared attempt, reconstructs Running, and makes that identity
/// unavailable to a second activation candidate.
#[test]
fn active_reconstitution_requires_and_exposes_exact_prepared_attempt() {
    let session = current_session();
    let active_origin = accepted_origin(1);
    let stored_attempt = matching_active_attempt();
    let facts = ActiveReconstitutionFacts::matching(&session, active_origin);
    let projection = facts
        .input()
        .reconstitute()
        .expect("the active turn has its exact prepared attempt");
    let active = projection
        .active_turn()
        .expect("the reconstructed turn owns the active slot");
    assert!(matches!(
        active.active_phase(),
        Some(ActiveTurnPhase::Running { current_attempt })
            if current_attempt.id() == stored_attempt
                && current_attempt.state() == &CurrentTurnAttemptState::Prepared
    ));

    let colliding_activation = activation(1);
    let collision = projection
        .clone()
        .prepare_earliest_queued_activation(
            colliding_activation.identities_with_attempt(stored_attempt),
        )
        .expect_err("a current attempt identity cannot be proposed again");
    assert_eq!(
        collision.failure(),
        AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
    );
    let occupied_activation = activation(2);
    let occupied = projection
        .prepare_earliest_queued_activation(occupied_activation.identities())
        .expect_err("an active slot blocks every queued activation");
    assert_eq!(
        occupied.failure(),
        AcceptedInputEligibilityFailure::ActiveTurnPresent {
            turn: active_origin.turn(),
        }
    );
}

/// inert prepared facts become a canonical attempt only
/// inside the validated owner projection.
#[test]
fn active_reconstitution_derives_prepared_attempt_after_validation() {
    let session = current_session();
    let active = accepted_origin(1);
    let expected_attempt = matching_active_attempt();
    let facts = ActiveReconstitutionFacts::matching(&session, active);
    let projection = facts
        .input()
        .reconstitute()
        .expect("the complete owner projection derives the prepared attempt");
    let phase = projection
        .active_turn()
        .expect("the turn owns the active slot")
        .active_phase();
    assert!(matches!(
        phase,
        Some(ActiveTurnPhase::Running { current_attempt })
            if current_attempt.id() == expected_attempt
                && current_attempt.state() == &CurrentTurnAttemptState::Prepared
    ));
}

/// inert running facts traverse the sealed
/// prepared-to-running transition only inside the validated owner
/// projection.
#[test]
fn active_reconstitution_derives_running_attempt_after_validation() {
    let session = current_session();
    let active = accepted_origin(1);
    let expected_attempt = turn_attempt_id(51);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::running(
        active.turn(),
        expected_attempt,
    ));
    let projection = facts
        .input()
        .reconstitute()
        .expect("the complete owner projection derives the running attempt");
    let execution = projection
        .active_turn_execution()
        .expect("active scheduling facts seal execution ownership");
    assert_eq!(execution.turn(), active.turn());
    assert!(matches!(
        execution.phase(),
        ActiveTurnPhase::Running { current_attempt }
            if current_attempt.id() == expected_attempt
                && current_attempt.state() == &CurrentTurnAttemptState::Running
    ));
}

/// a running continuation retains the exact
/// independently checked tool batch correlation needed by interruption.
#[test]
fn running_tool_batch_correlation_is_reconstituted() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();
    let producing_call = model_call_id(50);
    let continuation_attempt = turn_attempt_id(60);
    let request_id = tool_request_id(70);
    let assistant_tool_entry = semantic_entry(31);
    let yielded_frontier = frontier(41);
    let request = ToolRequestReconstitutionInput::new(
        request_id,
        session.id(),
        active.turn(),
        producing_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("current_time")).expect("fixture name is canonical"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request();
    let approval = ToolApprovalResolutionReconstitutionInput::user_fixture(
        request_id,
        ToolApprovalDecision::Approve,
    )
    .reconstitute()
    .expect("user approval is implemented");
    let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        yielded_frontier.id(),
        vec![
            origin_entry.reference(&session),
            assistant_tool_entry.reference(&session),
        ],
    )
    .expect("the tool response extends the starting frontier");
    let batch = ToolBatchReconstitutionInput::new(
        session.id(),
        active.turn(),
        producing_call,
        yielded,
        vec![request],
        vec![approval],
        vec![],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: continuation_attempt,
        },
    )
    .reconstitute()
    .expect("the complete approved batch is executing");
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(
        ActiveTurnSchedulingReconstitutionInput::prepared(active.turn(), continuation_attempt)
            .with_executing_tool_batch(&batch),
    );
    facts
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            assistant_tool_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: request_id,
            },
        ));
    facts
        .snapshots
        .push(yielded_frontier.snapshot(&session, &[origin_entry, assistant_tool_entry]));
    let model_call = ModelCallReconstitutionInput::new(
        producing_call,
        active.turn(),
        turn_attempt_id(59),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        starting_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
    );

    let projection = facts
        .input()
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                active.turn(),
                model_call.target(),
            )],
            vec![model_call],
        )
        .reconstitute()
        .expect("the running batch is bound to its exact call and yielded frontier");

    assert_eq!(
        projection.active_executing_tool_batch,
        Some(ActiveExecutingToolBatchCorrelation {
            session: session.id(),
            turn: active.turn(),
            producing_call,
            yielded_frontier: yielded_frontier.id(),
            turn_attempt: Some(continuation_attempt),
        })
    );
    let active_execution = projection
        .active_turn_execution()
        .expect("the correlated continuation owns the active slot");
    let ActiveTurnPhase::Running { current_attempt } = active_execution.phase() else {
        panic!("the correlated continuation is the running phase");
    };
    assert_eq!(current_attempt.id(), continuation_attempt);
    assert_eq!(current_attempt.state(), &CurrentTurnAttemptState::Prepared);
}

/// startup recovery consumes the complete active
/// projection, ends its exact evidence-free attempt as Lost, and appends
/// one `TurnFailed` marker to the starting frontier.
#[test]
fn prepares_atomic_lost_failed_terminal_candidate() {
    let session = current_session();
    let active = accepted_origin(1);
    let failure_entry = semantic_entry(500);
    let terminal_frontier = frontier(600);
    let identities =
        AcceptedInputTurnFailureIdentities::new(failure_entry.id(), terminal_frontier.id());
    let projection = ActiveReconstitutionFacts::matching(&session, active)
        .input()
        .reconstitute()
        .expect("the complete active projection is valid");

    let candidate = projection
        .prepare_active_turn_lost_failure(identities)
        .expect("evidence-free prior-process work can end Lost");

    assert_eq!(candidate.turn().turn(), active.turn());
    assert_eq!(
        candidate.turn().ended_attempt().id(),
        matching_active_attempt()
    );
    assert_eq!(
        candidate.turn().ended_attempt().end(),
        &AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        }
    );
    assert_eq!(candidate.turn().disposition(), &TurnDisposition::Failed);
    assert_eq!(
        candidate.failure_entry().payload(),
        &InitialSemanticTranscriptEntryPayload::TurnFailed {
            turn: active.turn(),
        }
    );
    assert_eq!(
        candidate
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            ActiveReconstitutionFacts::matching_origin_entry().reference(&session),
            failure_entry.reference(&session),
        ]
    );
    assert_eq!(
        candidate.terminal_snapshot().frontier().snapshot(),
        terminal_frontier.id()
    );
}

/// the same Lost failure transition is valid for a stored
/// Running attempt, without inventing a stop cause.
#[test]
fn running_attempt_also_prepares_without_stop_lost() {
    let session = current_session();
    let active = accepted_origin(1);
    let running_attempt = turn_attempt_id(51);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::running(
        active.turn(),
        running_attempt,
    ));

    let candidate = facts
        .input()
        .reconstitute()
        .expect("the complete running projection is valid")
        .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
            semantic_entry(500).id(),
            frontier(600).id(),
        ))
        .expect("running prior-process work can end Lost");

    assert_eq!(candidate.turn().ended_attempt().id(), running_attempt);
    assert_eq!(
        candidate.turn().ended_attempt().end(),
        &AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Lost,
        }
    );
}

/// pending steering is not a stop cause; the lost
/// failure reclassifies it into a queued successor, and identities that do
/// not match the pending inventory leave the projection unchanged.
#[test]
fn lost_failure_reclassifies_pending_steering() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    let tail = facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts contain the active tail");
    tail.observed_last_position = pending.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let projection = facts
        .input()
        .reconstitute()
        .expect("the pending-steering tail is complete");
    let identities =
        AcceptedInputTurnFailureIdentities::new(semantic_entry(500).id(), frontier(600).id());

    let error = projection
        .clone()
        .prepare_active_turn_lost_failure(identities.clone())
        .expect_err("pending steering needs a successor identity");
    assert_eq!(error.projection(), &projection);
    assert_eq!(error.identities(), &identities);
    assert_eq!(
        error.failure(),
        AcceptedInputTurnFailureFailure::PendingSteeringReclassificationMismatch
    );

    let successor = turn_id(700);
    let candidate = projection
        .prepare_active_turn_lost_failure(identities.with_pending_steering_reclassifications(vec![
            PendingSteeringReclassificationIdentity::new(pending.accepted_input(), successor),
        ]))
        .expect("pending steering is reclassified rather than refused");
    let [reclassified] = candidate.reclassified_pending_steering() else {
        panic!("exactly one successor is reclassified");
    };
    assert_eq!(reclassified.turn(), successor);
    assert_eq!(reclassified.source_turn(), active.turn());
    assert_eq!(
        reclassified.accepted_input(),
        &AcceptedInputLifecycle::new(
            pending.accepted_input(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: successor,
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        )
    );
    assert_eq!(
        reclassified.order(),
        AcceptedInputQueueOrder::ordinary(pending.position())
    );
}

/// pending steering remains outside the active rendered frontier
/// until a safe-point continuation incorporates it.
#[test]
fn pending_steering_is_not_an_active_rendered_frontier_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    let tail = facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts contain the active tail");
    tail.observed_last_position = pending.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let projection = facts
        .input()
        .reconstitute()
        .expect("the pending-steering tail is complete");

    assert_eq!(
        projection.active_rendered_frontier_origins(),
        Some(vec![active.accepted_input()])
    );
}

/// startup failure preparation rejects each committed
/// identity before constructing a candidate.
#[test]
fn rejects_committed_failure_identities() {
    let session = current_session();
    let active = accepted_origin(1);
    let projection = ActiveReconstitutionFacts::matching(&session, active)
        .input()
        .reconstitute()
        .expect("the complete active projection is valid");

    let entry_collision = projection
        .clone()
        .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
            ActiveReconstitutionFacts::matching_origin_entry().id(),
            frontier(600).id(),
        ))
        .expect_err("the semantic identity is already committed");
    assert_eq!(
        entry_collision.failure(),
        AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists
    );

    let frontier_collision = projection
        .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
            semantic_entry(500).id(),
            ActiveReconstitutionFacts::matching_starting_frontier().id(),
        ))
        .expect_err("the frontier identity is already committed");
    assert_eq!(
        frontier_collision.failure(),
        AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists
    );
}

/// scheduling
/// reconstitution accepts the exact terminal shape written when an
/// interrupt closes a yielded tool round.
#[test]
fn cancelled_tool_round_reconstitutes() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                terminal_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                None,
                interrupt,
            )
            .with_terminal_tool_denials(vec![user_denial(request)]),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let projection = input
        .reconstitute()
        .expect("the writer-produced cancelled tool-round shape reconstitutes");

    assert_eq!(
        projection
            .turn(cancelled.turn())
            .expect("the cancelled turn remains present")
            .status(),
        AcceptedInputTurnSchedulingStatus::TerminalCancelled
    );
    assert_eq!(
        projection
            .earliest_queued_turn()
            .expect("the interrupt successor remains queued")
            .turn(),
        successor.turn()
    );
}

/// scheduling
/// reconstitution accepts the exact terminal shape written when a stop
/// request races a tool-using response, which names the batch's completed
/// producing call.
#[test]
fn stopped_tool_round_reconstitutes_from_named_call() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let closed_result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    // The stop-requested attempt that issued the racing call is the exact
    // attempt the cancellation ends, so one identity names both.
    let stopped_attempt = turn_attempt_id(51);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                stopped_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                Some(producing_call),
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            closed_result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    closed_result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            stopped_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let projection = input
        .reconstitute()
        .expect("the writer-produced stopped tool-round shape reconstitutes");

    assert_eq!(
        projection
            .turn(cancelled.turn())
            .expect("the cancelled turn remains present")
            .status(),
        AcceptedInputTurnSchedulingStatus::TerminalCancelled
    );
    assert_eq!(
        projection
            .earliest_queued_turn()
            .expect("the interrupt successor remains queued")
            .turn(),
        successor.turn()
    );
}

/// a cancelled terminal
/// turn naming a completed call that is not the tool round's producing
/// call fails closed.
#[test]
fn cancelled_tool_round_rejects_unrelated_named_call() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let closed_result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    // The named call is a completed call of the same turn and attempt that
    // proposed nothing in the terminal round: naming it is the behavior
    // under test.
    let unrelated_call = model_call_id(51);
    let stopped_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                stopped_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                Some(unrelated_call),
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            closed_result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    closed_result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                stopped_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                unrelated_call,
                cancelled.turn(),
                stopped_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
        ],
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// a cancelled terminal
/// tool round whose `ToolDenied` result entry names no user denial
/// resolution fails closed.
#[test]
fn cancelled_tool_round_rejects_missing_denial_resolution() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                terminal_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                None,
                interrupt,
            )
            // The denial entry's backing user resolution is deliberately
            // absent: this emptiness is the behavior under test.
            .with_terminal_tool_denials(Vec::new()),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// an approving user
/// resolution cannot back a cancelled terminal tool round's `ToolDenied`
/// result entry; the round fails closed.
#[test]
fn cancelled_tool_round_rejects_approving_resolution() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let request = tool_request_id(60);
    let approving_resolution = ToolApprovalResolutionReconstitutionInput::user_fixture(
        request,
        ToolApprovalDecision::Approve,
    )
    .reconstitute()
    .expect("the approving resolution fixture is valid");
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(70),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                terminal_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                None,
                interrupt,
            )
            .with_terminal_tool_denials(vec![approving_resolution]),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied { request },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    tool_use_entry,
                    result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            cancelled.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// scheduling
/// reconstitution accepts the exact terminal shape written when a
/// crash-lost tool round closes the turn as failed.
#[test]
fn failed_tool_round_reconstitutes() {
    let session = current_session();
    let failed = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let first_tool_use = semantic_entry(31);
    let second_tool_use = semantic_entry(32);
    let first_result = semantic_entry(33);
    let second_result = semantic_entry(34);
    let failure_entry = semantic_entry(35);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let first_request = tool_request_id(60);
    let second_request = tool_request_id(61);
    let executed_attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(70),
        first_request,
        session.id(),
        failed.turn(),
        terminal_attempt,
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("ok")).expect("fixture tool result is valid"),
            ),
        }),
    )
    .reconstitute()
    .expect("fixture tool attempt is supported");
    let crate::ReconstitutedToolAttempt::Ended(executed_attempt) = executed_attempt else {
        panic!("fixture tool attempt is terminal");
    };
    let failed_record = failed.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: Some(
                FailedTurnExecutionReconstitutionInput::attempt_only(
                    failed.turn(),
                    terminal_attempt,
                    UnstoppedAttemptDisposition::KnownFailure,
                )
                .with_terminal_tool_attempts(vec![executed_attempt]),
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let semantic_entries = vec![
        failed.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            first_tool_use.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: first_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            second_tool_use.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request: second_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            first_result.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            second_result.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: second_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            failure_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed {
                turn: failed.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![failed_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    first_tool_use,
                    second_tool_use,
                    first_result,
                    second_result,
                    failure_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            failed.turn(),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            failed.turn(),
            producing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            target,
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    );

    let mismatched_attempt = ToolAttemptReconstitutionInput::new(
        tool_attempt_id(70),
        second_request,
        session.id(),
        failed.turn(),
        terminal_attempt,
        ToolEffectClass::EffectFree,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("wrong request"))
                    .expect("fixture tool result is valid"),
            ),
        }),
    )
    .reconstitute()
    .expect("fixture tool attempt is supported");
    let crate::ReconstitutedToolAttempt::Ended(mismatched_attempt) = mismatched_attempt else {
        panic!("fixture tool attempt is terminal");
    };
    let mut mismatched_input = input.clone();
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution: Some(execution),
        ..
    } = &mut mismatched_input.turns[0].state
    else {
        panic!("fixture is a failed terminal");
    };
    execution.terminal_tool_attempts = vec![mismatched_attempt];
    assert_eq!(
        mismatched_input
            .reconstitute()
            .expect_err("a result attempt must execute its paired request")
            .failure()
            .to_owned(),
        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: failed.turn(),
        }
    );

    let projection = input
        .reconstitute()
        .expect("the writer-produced failed tool-round shape reconstitutes");

    assert_eq!(
        projection
            .turn(failed.turn())
            .expect("the failed turn remains present")
            .status(),
        AcceptedInputTurnSchedulingStatus::TerminalFailed
    );
}

/// complete scheduling reconstitution admits every
/// reference-only tool entry while retaining completed-call provenance
/// for assistant tool use from an earlier intra-turn round.
#[test]
fn scheduling_reconstitutes_tool_round_history() {
    let session = current_session();
    let active = accepted_origin(1);
    let producing_call = model_call_id(90);
    let request = tool_request_id(91);
    let attempt = tool_attempt_id(92);
    let denied_request = tool_request_id(93);
    let closed_request = tool_request_id(94);
    let tool_use = semantic_entry(31);
    let execution_result = semantic_entry(32);
    let denied = semantic_entry(33);
    let closed = semantic_entry(34);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.semantic_entries.extend([
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            execution_result.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult { attempt },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            denied.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolDenied {
                request: denied_request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            closed.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: closed_request,
            },
        ),
    ]);
    let target = ResolvedProviderTarget::naming(provider_model_identity(51));
    let projection = facts
        .input()
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                active.turn(),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                active.turn(),
                turn_attempt_id(49),
                FrozenModelSelection::Direct(direct(1)),
                target,
                ActiveReconstitutionFacts::matching_starting_frontier().id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        )
        .reconstitute()
        .expect("tool-round history and its completed producing call agree");

    assert!(matches!(
        projection
            .semantic_entry(tool_use.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::AssistantToolUse {
            producing_call: actual_call,
            request: actual_request,
        }) if *actual_call == producing_call && *actual_request == request
    ));
    assert!(matches!(
        projection
            .semantic_entry(execution_result.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
            attempt: actual,
        }) if *actual == attempt
    ));
    assert!(matches!(
        projection
            .semantic_entry(denied.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::ToolDenied { request: actual })
            if *actual == denied_request
    ));
    assert!(matches!(
        projection
            .semantic_entry(closed.reference(&session))
            .map(SemanticTranscriptEntry::payload),
        Some(InitialSemanticTranscriptEntryPayload::ToolClosed { request: actual })
            if *actual == closed_request
    ));
}

/// scheduling
/// reconstitution admits consumed steering only when its semantic subject,
/// accepted lifecycle, source turn, call frontier, and acceptance order
/// agree exactly.
#[test]
fn reconstitution_validates_steering_subjects() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed)
        .input()
        .reconstitute()
        .expect("matching consumed steering reconstructs");

    let mut nonfollowing_position =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    nonfollowing_position.consumed_steering[0].acceptance_position = active.position();
    nonfollowing_position.acceptance_tail.entries[1].position = active.position();
    nonfollowing_position.acceptance_tail.observed_last_position = active.position();
    assert_eq!(
        assert_input_rejects_unchanged(nonfollowing_position.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut skipped_reclassified =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    skipped_reclassified.turns[0].state = AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
        starting_frontier: ActiveReconstitutionFacts::matching_starting_frontier().id(),
        terminal_execution: None,
        terminal_frontier: frontier(42).id(),
    };
    let reclassified = accepted_origin(2);
    skipped_reclassified
        .turns
        .push(AcceptedInputTurnSchedulingRecord::reclassified(
            session.id(),
            reclassified.turn(),
            session.id(),
            AcceptedInputLifecycle::new(
                reclassified.accepted_input(),
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                    turn: reclassified.turn(),
                    reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                },
            ),
            session.id(),
            reclassified.turn(),
            reclassified.ordinary_order(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
            crate::SteeringBinding::new(active.turn()),
            configuration(&session),
            AcceptedInputTurnSchedulingRecordState::Queued,
        ));
    assert_eq!(
        assert_input_rejects_unchanged(skipped_reclassified.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut nonexistent = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    nonexistent.semantic_entries[1] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(31).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input: accepted_input_id(99),
            source_turn: active.turn(),
        },
    );
    assert_eq!(
        assert_input_rejects_unchanged(nonexistent.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut wrong_source =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    wrong_source.semantic_entries[1] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(31).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
            accepted_input: consumed.accepted_input(),
            source_turn: turn_id(99),
        },
    );
    assert_eq!(
        assert_input_rejects_unchanged(wrong_source.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );

    let mut missing_lifecycle =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    missing_lifecycle.consumed_steering.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_lifecycle.input()),
        AcceptedInputSchedulingReconstitutionFailure::SteeringSemanticEntryMismatch {
            entry: semantic_entry(31).id(),
        }
    );

    let mut duplicate_subject =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    duplicate_subject
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(32).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: active.turn(),
            },
        ));
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_subject.input()),
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
            entry: semantic_entry(32).id(),
        }
    );

    let mut duplicate_lifecycle =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    let duplicate = duplicate_lifecycle
        .consumed_steering
        .first()
        .cloned()
        .expect("the matching fixture contains one consumed subject");
    duplicate_lifecycle.consumed_steering.push(duplicate);
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_lifecycle.input()),
        AcceptedInputSchedulingReconstitutionFailure::DuplicateConsumedSteering {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// scheduling reconstitution admits
/// the durable shape the continuation transaction commits — a running
/// continuation attempt owning a prepared steering-consuming call whose
/// frontier is the round's exact result projection plus the consumed
/// suffix.
#[test]
fn steering_consumed_at_continuation_reconstitutes() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed)
        .input()
        .reconstitute()
        .expect("continuation-consumed steering reconstructs");
}

/// a running attempt owning a prepared
/// steering-consuming call is legal only with the round's result
/// evidence.
#[test]
fn continuation_pair_requires_round_evidence() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut missing_evidence =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    missing_evidence.steering_continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// continuation-round evidence must name a
/// steering-consuming call.
#[test]
fn round_evidence_requires_a_consuming_call() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut dangling_evidence =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    dangling_evidence.steering_continuation_rounds.push(
        SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_producing_call(),
            vec![
                ConsumedSteeringReconstitutionFacts::matching_continuation_round_attempt(
                    &session, active,
                ),
            ],
            Vec::new(),
        ),
    );
    assert_eq!(
        assert_input_rejects_unchanged(dangling_evidence.input()),
        AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
            call: ConsumedSteeringReconstitutionFacts::matching_continuation_producing_call(),
        }
    );
}

/// continuation-round evidence names each
/// consuming call at most once.
#[test]
fn round_evidence_names_each_consumer_once() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut duplicate_evidence =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    let duplicated = duplicate_evidence.steering_continuation_rounds[0].clone();
    duplicate_evidence
        .steering_continuation_rounds
        .push(duplicated);
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_evidence.input()),
        AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
            call: ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
        }
    );
}

/// the consumed steering entries must be
/// the exact trailing suffix after the round's result window.
#[test]
fn consumed_steering_is_the_continuation_trailing_suffix() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut interposed_steering =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    interposed_steering.snapshots[1] = frontier(41).snapshot(
        &session,
        &[
            ActiveReconstitutionFacts::matching_origin_entry(),
            semantic_entry(34),
            semantic_entry(31),
            semantic_entry(35),
        ],
    );
    assert_eq!(
        assert_input_rejects_unchanged(interposed_steering.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// each result entry in the
/// continuation window must correlate to its proposal-ordered request.
#[test]
fn continuation_results_correlate_to_proposal_order() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut miscorrelated_result =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    miscorrelated_result.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt(
                &session,
                active,
                matching_active_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                tool_request_id(96),
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(miscorrelated_result.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// the round's tools were issued by
/// the same continuation attempt that owns the consuming call; evidence
/// issued by a foreign attempt fails closed.
#[test]
fn continuation_results_bind_to_the_consuming_attempt() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut foreign_issuing_attempt =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    foreign_issuing_attempt.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt(
                &session,
                active,
                turn_attempt_id(49),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(foreign_issuing_attempt.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// a continuation window forbids
/// turn-end closures, which exist only in terminal materialization.
#[test]
fn continuation_window_forbids_turn_end_closures() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut closed_request =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    closed_request.semantic_entries[3] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(35).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ToolClosed {
            request: ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
        },
    );
    closed_request.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            Vec::new(),
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(closed_request.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// an ambiguous attempt end is a
/// turn-level failure and never reaches a continuation window.
#[test]
fn continuation_window_rejects_an_ambiguous_attempt_end() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut ambiguous_attempt =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    ambiguous_attempt.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt_with_end(
                &session,
                active,
                matching_active_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
                ToolAttemptEnd::Ambiguous,
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(ambiguous_attempt.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// a crash-lost attempt end is a
/// turn-level failure and never reaches a continuation window.
#[test]
fn continuation_window_rejects_a_crash_lost_attempt_end() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut crash_lost_attempt =
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed);
    crash_lost_attempt.steering_continuation_rounds =
        vec![SteeringContinuationRoundReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            vec![ended_tool_attempt_with_end(
                &session,
                active,
                matching_active_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
                ToolAttemptEnd::KnownFailed {
                    error: ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
                },
            )],
            Vec::new(),
        )];
    assert_eq!(
        assert_input_rejects_unchanged(crash_lost_attempt.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// only a tool proposal keeps a completed
/// consumer's turn going, so a text-only completed consumer inside an
/// active turn cannot claim the historical-consumer correlation.
#[test]
fn text_only_completed_consumer_fails_closed() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let mut text_only_consumer =
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    text_only_consumer.model_calls[0] = ModelCallReconstitutionInput::new(
        ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
        active.turn(),
        matching_active_attempt(),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        frontier(41).id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
    );
    text_only_consumer
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(36).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
                value: AssistantText::try_new(String::from("text-only response"))
                    .expect("fixture assistant text is valid"),
            },
        ));
    assert_eq!(
        assert_input_rejects_unchanged(text_only_consumer.input()),
        AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// a steering-consuming call that
/// completed by proposing a tool round stays reconstitutable while the
/// round is parked awaiting approval — the consumer is correlated through
/// its assistant history and exact frontier window, not the current
/// phase's attempt.
#[test]
fn parked_tool_round_retains_consumed_steering() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let steering_entry = semantic_entry(31);
    let tool_use_entry = semantic_entry(34);
    let call_frontier = frontier(41);
    let yielded_frontier = frontier(42);
    let consuming_call = ConsumedSteeringReconstitutionFacts::matching_continuation_call();
    let request_id = ConsumedSteeringReconstitutionFacts::matching_continuation_request();
    let request = ToolRequestReconstitutionInput::new(
        request_id,
        session.id(),
        active.turn(),
        consuming_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("current_time")).expect("fixture name is canonical"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request();
    let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        yielded_frontier.id(),
        vec![
            origin_entry.reference(&session),
            steering_entry.reference(&session),
            tool_use_entry.reference(&session),
        ],
    )
    .expect("the tool response extends the steering-bearing call frontier");
    let batch = ToolBatchReconstitutionInput::new(
        session.id(),
        active.turn(),
        consuming_call,
        yielded,
        vec![request],
        vec![],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: request_id,
        },
    )
    .reconstitute()
    .expect("the undecided batch is awaiting approval");
    let mut facts = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    let AcceptedInputTurnSchedulingRecordState::Active { phase, .. } = &mut facts.turns[0].state
    else {
        panic!("matching consumed-steering facts retain an active scheduling record");
    };
    *phase = ActiveTurnSchedulingReconstitutionInput::awaiting_approval(active.turn(), &batch)
        .expect("the approval wait names the parked batch");
    facts
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: consuming_call,
                request: request_id,
            },
        ));
    facts
        .snapshots
        .push(yielded_frontier.snapshot(&session, &[origin_entry, steering_entry, tool_use_entry]));
    facts.model_calls = vec![ModelCallReconstitutionInput::new(
        consuming_call,
        active.turn(),
        matching_active_attempt(),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        call_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
    )];

    facts
        .input()
        .reconstitute()
        .expect("a parked tool round retains its consumed steering");
}

/// Matching stored facts for one failed terminal turn naming its
/// round-two continuation call: the call's whole frontier is the
/// completed round's result projection and the terminal frontier extends
/// it by exactly the failure marker.
fn failed_continuation_call_input(
    session: &Session,
    failed: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let failure_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        failed,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let failed_record = failed.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: Some(
                FailedTurnExecutionReconstitutionInput::with_call(
                    failed.turn(),
                    terminal_attempt,
                    UnstoppedAttemptDisposition::KnownFailure,
                    continuation_call,
                )
                .with_terminal_tool_attempts(vec![executed_attempt]),
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let semantic_entries = vec![
        failed.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            failure_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed {
                turn: failed.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![failed_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            terminal_frontier.snapshot(
                &session,
                &[origin_entry, tool_use_entry, result_entry, failure_entry],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            failed.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                failed.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                failed.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::KnownFailed),
            ),
        ],
    )
}

/// a failed terminal turn naming its round-two
/// continuation call reconstitutes when that call's whole frontier is the
/// completed round's result projection the terminal marker extends.
#[test]
fn failed_continuation_call_reconstitutes() {
    let session = current_session();
    let failed = accepted_origin(1);
    failed_continuation_call_input(&session, failed)
        .reconstitute()
        .expect("the failed continuation-call terminal shape reconstructs");
}

/// a failed terminal turn naming a
/// continuation call is accepted only with its round's result evidence.
#[test]
fn failed_continuation_call_requires_round_evidence() {
    let session = current_session();
    let failed = accepted_origin(1);
    let mut missing_evidence = failed_continuation_call_input(&session, failed);
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution: Some(execution),
        ..
    } = &mut missing_evidence.turns[0].state
    else {
        panic!("fixture is a failed terminal");
    };
    execution.terminal_tool_attempts = Vec::new();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}

/// a named continuation call's round
/// completed, so its window forbids turn-end closures.
#[test]
fn failed_continuation_call_window_forbids_turn_end_closures() {
    let session = current_session();
    let failed = accepted_origin(1);
    let mut closed_request = failed_continuation_call_input(&session, failed);
    closed_request.semantic_entries[2] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(32).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ToolClosed {
            request: tool_request_id(60),
        },
    );
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution: Some(execution),
        ..
    } = &mut closed_request.turns[0].state
    else {
        panic!("fixture is a failed terminal");
    };
    execution.terminal_tool_attempts = Vec::new();
    assert_eq!(
        assert_input_rejects_unchanged(closed_request),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}

/// Matching stored facts for one cancelled terminal turn naming its
/// unsent round-two continuation call: the call's whole frontier is the
/// completed round's result projection and the terminal frontier extends
/// it by exactly the cancellation marker.
fn cancelled_continuation_call_input(
    session: &Session,
    cancelled: OriginFixture,
    successor: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let compaction_entry = semantic_entry(34);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let cancellation_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        cancelled,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(71),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the terminal interrupt is exactly correlated");
    let cancelled_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                terminal_attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                Some(continuation_call),
                interrupt,
            )
            .with_terminal_tool_attempts(vec![executed_attempt]),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: cancelled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        cancelled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            compaction_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call,
                block: crate::ProviderCompactionBlock::try_new(String::from(
                    r#"{"type":"compaction","content":"retained summary"}"#,
                ))
                .expect("fixture provider compaction is valid"),
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cancelled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(
                &session,
                &[origin_entry, compaction_entry, tool_use_entry, result_entry],
            ),
            terminal_frontier.snapshot(
                &session,
                &[
                    origin_entry,
                    compaction_entry,
                    tool_use_entry,
                    result_entry,
                    cancellation_entry,
                ],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                cancelled.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled),
            ),
        ],
    )
}

/// a cancelled terminal turn naming
/// its unsent round-two continuation call reconstitutes when provider
/// compaction precedes the tool proposal and that call's whole frontier is
/// the completed round's result projection the cancellation marker
/// extends.
#[test]
fn cancelled_continuation_call_reconstitutes() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    cancelled_continuation_call_input(&session, cancelled, successor)
        .reconstitute()
        .expect("the cancelled continuation-call terminal shape reconstructs");
}

/// a cancelled terminal turn naming
/// a continuation call is accepted only with its round's result evidence.
#[test]
fn cancelled_continuation_call_requires_round_evidence() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let mut missing_evidence = cancelled_continuation_call_input(&session, cancelled, successor);
    let AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
        terminal_execution, ..
    } = &mut missing_evidence.turns[0].state
    else {
        panic!("fixture is a cancelled terminal");
    };
    terminal_execution.terminal_tool_attempts = Vec::new();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// Matching stored facts for one refused terminal turn naming its
/// round-two continuation call: the call's whole frontier is the
/// completed round's result projection and the equal-content terminal
/// frontier extends it by no entry.
fn refused_continuation_call_input(
    session: &Session,
    refused: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        refused,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let refused_record = refused.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt: terminal_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call: continuation_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let semantic_entries = vec![
        refused.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![refused_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            refused.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                refused.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                refused.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
            ),
        ],
    )
    .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
        continuation_call,
        vec![executed_attempt],
        Vec::new(),
    )])
}

/// a refused terminal turn naming its round-two
/// continuation call reconstitutes when that call's whole frontier is the
/// completed round's result projection the equal-content terminal
/// frontier repeats.
#[test]
fn refused_continuation_call_reconstitutes() {
    let session = current_session();
    let refused = accepted_origin(1);
    refused_continuation_call_input(&session, refused)
        .reconstitute()
        .expect("the refused continuation-call terminal shape reconstructs");
}

/// a refused terminal turn naming a continuation
/// call is accepted only with its round's result evidence.
#[test]
fn refused_continuation_call_requires_round_evidence() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut missing_evidence = refused_continuation_call_input(&session, refused);
    missing_evidence.continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: refused.turn(),
        }
    );
}

/// a named refused continuation call's round
/// completed, so its window forbids turn-end closures.
#[test]
fn refused_continuation_call_window_forbids_turn_end_closures() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut closed_request = refused_continuation_call_input(&session, refused);
    closed_request.semantic_entries[2] = SemanticTranscriptEntryReconstitutionInput::new(
        semantic_entry(32).id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ToolClosed {
            request: tool_request_id(60),
        },
    );
    closed_request.continuation_rounds = vec![ContinuationRoundReconstitutionInput::new(
        model_call_id(53),
        Vec::new(),
        Vec::new(),
    )];
    assert_eq!(
        assert_input_rejects_unchanged(closed_request),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: refused.turn(),
        }
    );
}

/// gate-named continuation-round evidence names each
/// call at most once.
#[test]
fn continuation_round_evidence_names_each_call_once() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut duplicate_evidence = refused_continuation_call_input(&session, refused);
    let duplicated = duplicate_evidence.continuation_rounds[0].clone();
    duplicate_evidence.continuation_rounds.push(duplicated);
    assert_eq!(
        assert_input_rejects_unchanged(duplicate_evidence),
        AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
            call: model_call_id(53),
        }
    );
}

/// gate-named continuation-round evidence must name
/// a call a terminal or recovery gate proves against it.
#[test]
fn continuation_round_evidence_requires_a_naming_gate() {
    let session = current_session();
    let refused = accepted_origin(1);
    let mut dangling_evidence = refused_continuation_call_input(&session, refused);
    dangling_evidence
        .continuation_rounds
        .push(ContinuationRoundReconstitutionInput::new(
            model_call_id(50),
            Vec::new(),
            Vec::new(),
        ));
    assert_eq!(
        assert_input_rejects_unchanged(dangling_evidence),
        AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
            call: model_call_id(50),
        }
    );
}

/// Matching stored facts for one reconciliation-required terminal turn
/// naming its interrupted round-two continuation call: the ambiguous
/// call's whole frontier is the completed round's result projection and
/// the equal-content terminal frontier extends it by no entry.
fn reconciliation_required_continuation_call_input(
    session: &Session,
    reconciled: OriginFixture,
    successor: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let terminal_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        reconciled,
        terminal_attempt,
        tool_attempt_id(70),
        request,
    );
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        reconciled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(71),
        session.id(),
        reconciled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the reconciling interrupt is exactly correlated");
    let reconciled_record = reconciled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            reconciling_attempt: terminal_attempt,
            reconciling_attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::Lost,
                interrupt,
            ),
            ambiguous_call: continuation_call,
            authority: AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: reconciled.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let semantic_entries = vec![
        reconciled.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![reconciled_record, successor_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            reconciled.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                reconciled.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                reconciled.turn(),
                terminal_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
            ),
        ],
    )
    .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
        continuation_call,
        vec![executed_attempt],
        Vec::new(),
    )])
}

/// a reconciliation-required terminal turn
/// naming its interrupted round-two continuation call reconstitutes when
/// that call's whole frontier is the completed round's result projection
/// the equal-content terminal frontier repeats.
#[test]
fn reconciliation_required_continuation_call_reconstitutes() {
    let session = current_session();
    let reconciled = accepted_origin(1);
    let successor = accepted_origin(2);
    reconciliation_required_continuation_call_input(&session, reconciled, successor)
        .reconstitute()
        .expect("the reconciliation-required continuation-call terminal shape reconstructs");
}

/// a reconciliation-required terminal turn
/// naming a continuation call is accepted only with its round's result
/// evidence.
#[test]
fn reconciliation_required_continuation_call_requires_round_evidence() {
    let session = current_session();
    let reconciled = accepted_origin(1);
    let successor = accepted_origin(2);
    let mut missing_evidence =
        reconciliation_required_continuation_call_input(&session, reconciled, successor);
    missing_evidence.continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: reconciled.turn(),
        }
    );
}

/// Matching stored facts for one active turn parked on the ambiguous
/// round-two continuation call of a completed tool round: the call's
/// whole frontier is the completed round's result projection and the
/// recovery wait extends it by no entry.
fn recovery_wait_continuation_call_input(
    session: &Session,
    active: OriginFixture,
) -> AcceptedInputSchedulingReconstitutionInput {
    let session = session.clone();
    let origin_entry = semantic_entry(30);
    let tool_use_entry = semantic_entry(31);
    let result_entry = semantic_entry(32);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let producing_call = model_call_id(50);
    let producing_attempt = turn_attempt_id(51);
    let recovery_attempt = turn_attempt_id(52);
    let continuation_call = model_call_id(53);
    let request = tool_request_id(60);
    let executed_attempt = ended_tool_attempt(
        &session,
        active,
        recovery_attempt,
        tool_attempt_id(70),
        request,
    );
    let active_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            phase:
                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                    active.turn(),
                    recovery_attempt,
                    continuation_call,
                ),
        },
    );
    let semantic_entries = vec![
        active.entry(&session, origin_entry),
        SemanticTranscriptEntryReconstitutionInput::new(
            tool_use_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call,
                request,
            },
        ),
        SemanticTranscriptEntryReconstitutionInput::new(
            result_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(70),
            },
        ),
    ];
    let target = ResolvedProviderTarget::naming(provider_model_identity(80));
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![active_record],
        semantic_entries,
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
        ],
        Some(active.active_tail(&session)),
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            active.turn(),
            target,
        )],
        vec![
            ModelCallReconstitutionInput::new(
                producing_call,
                active.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                continuation_call,
                active.turn(),
                recovery_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
            ),
        ],
    )
    .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
        continuation_call,
        vec![executed_attempt],
        Vec::new(),
    )])
}

/// an active turn parked on the ambiguous
/// round-two continuation call of a completed tool round reconstitutes
/// the exact recovery wait when that call's whole frontier is the
/// completed round's result projection.
#[test]
fn recovery_wait_continuation_call_reconstitutes() {
    let session = current_session();
    let active = accepted_origin(1);
    let projection = recovery_wait_continuation_call_input(&session, active)
        .reconstitute()
        .expect("the parked continuation-call recovery wait reconstructs");
    let waiting = projection
        .active_turn()
        .expect("the recovery wait retains the progressing slot");

    assert!(matches!(
        waiting.active_phase(),
        Some(ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations,
            ..
        }) if ambiguous_operations
            .contains(crate::IssuedOperationRef::ModelCall(model_call_id(53)))
    ));
}

/// a recovery wait naming a continuation call is
/// accepted only with its round's result evidence.
#[test]
fn recovery_wait_continuation_call_requires_round_evidence() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut missing_evidence = recovery_wait_continuation_call_input(&session, active);
    missing_evidence.continuation_rounds.clear();
    assert_eq!(
        assert_input_rejects_unchanged(missing_evidence),
        AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
            turn: active.turn(),
        }
    );
}

/// an active scheduling projection
/// requires the exact session-scoped interval anchored at its origin; a
/// missing, cross-session, or cross-wired interval fails closed.
#[test]
fn active_reconstitution_requires_exact_session_acceptance_tail_identity() {
    let session = current_session();
    let active = accepted_origin(1);

    let missing = assert_reconstitution_rejects_unchanged(ActiveReconstitutionFacts {
        acceptance_tail: None,
        ..ActiveReconstitutionFacts::matching(&session, active)
    });
    assert_eq!(
        missing,
        AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
            turn: active.turn(),
        }
    );

    let other_session = session_id(2);
    let mut wrong_session_facts = ActiveReconstitutionFacts::matching(&session, active);
    wrong_session_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail")
        .session = other_session;
    let wrong_session = assert_reconstitution_rejects_unchanged(wrong_session_facts);
    assert_eq!(
        wrong_session,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
            expected: session.id(),
            actual: other_session,
        }
    );

    let other_anchor = accepted_input_id(99);
    let mut wrong_anchor_facts = ActiveReconstitutionFacts::matching(&session, active);
    wrong_anchor_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail")
        .anchor = other_anchor;
    let wrong_anchor = assert_reconstitution_rejects_unchanged(wrong_anchor_facts);
    assert_eq!(
        wrong_anchor,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
            turn: active.turn(),
            expected: active.accepted_input(),
            actual: other_anchor,
        }
    );

    expect![[r#"
            ┌──────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact    │ failure                                                                                                                                                                                                             │
            ├──────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ active tail omitted      │ MissingActiveAcceptanceTail { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }                                                                                                                                  │
            │ tail session cross-wired │ AcceptanceTailSessionMismatch { expected: SessionId(00000000-0000-0000-0000-000000000001), actual: SessionId(00000000-0000-0000-0000-000000000002) }                                                                │
            │ tail anchor cross-wired  │ AcceptanceTailAnchorMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe), expected: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe), actual: AcceptedInputId(00000000-0000-0000-0000-000000000063) } │
            └──────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "active tail omitted",
                failure: format!("{missing:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail session cross-wired",
                failure: format!("{wrong_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail anchor cross-wired",
                failure: format!("{wrong_anchor:?}"),
            },
        ]));
}

/// every position from the active origin through
/// the observed session tail is present exactly once and every
/// pending-steering disposition remains bound to that active turn.
#[test]
fn active_reconstitution_rejects_gapped_or_misbound_acceptance_tail() {
    let session = current_session();
    let active = accepted_origin(1);
    let second = accepted_origin(2);
    let third = accepted_origin(3);

    let mut gapped_facts = ActiveReconstitutionFacts::matching(&session, active);
    let gapped_tail = gapped_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    gapped_tail.observed_last_position = third.position();
    gapped_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                second.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            third.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let gapped = assert_reconstitution_rejects_unchanged(gapped_facts);
    assert_eq!(
        gapped,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
            accepted_input: second.accepted_input(),
            expected: second.position(),
            actual: third.position(),
        }
    );

    let other_turn = turn_id(99);
    let mut misbound_facts = ActiveReconstitutionFacts::matching(&session, active);
    let misbound_tail = misbound_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    misbound_tail.observed_last_position = second.position();
    misbound_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                second.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(other_turn),
                },
            ),
            second.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: other_turn,
            },
        ));
    let misbound = assert_reconstitution_rejects_unchanged(misbound_facts);
    assert_eq!(
        misbound,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: second.accepted_input(),
        }
    );

    let after_active_delivery = DeliveryRequest::AfterCurrentTurn {
        expected_active_turn: active.turn(),
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let mut cross_wired_facts = ActiveReconstitutionFacts::matching(&session, active);
    let cross_wired_tail = cross_wired_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    cross_wired_tail.observed_last_position = third.position();
    cross_wired_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                second.accepted_input(),
                AcceptedInputDisposition::OriginOf(second.turn()),
            ),
            second.position(),
            after_active_delivery,
        ));
    cross_wired_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                third.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            third.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    cross_wired_facts.turns.push(second.record_with(
        &session,
        OriginRecordFacts {
            order: AcceptedInputQueueOrder::ordinary(third.position()),
            delivery: after_active_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    ));
    let cross_wired = assert_reconstitution_rejects_unchanged(cross_wired_facts);
    assert_eq!(
        cross_wired,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: second.accepted_input(),
        }
    );

    expect![[r#"
            ┌────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact              │ failure                                                                                                                                                                      │
            ├────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ interior position omitted          │ AcceptanceTailPositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd), expected: SessionInputPosition(2), actual: SessionInputPosition(3) } │
            │ pending steering owner cross-wired │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            │ origin position cross-wired        │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            └────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "interior position omitted",
                failure: format!("{gapped:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "pending steering owner cross-wired",
                failure: format!("{misbound:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "origin position cross-wired",
                failure: format!("{cross_wired:?}"),
            },
        ]));
}

/// a newly active queued origin retains later acceptance
/// positions already consumed by its terminal predecessor, while only its
/// own consumed steering reaches the active execution aggregate.
#[test]
fn active_tail_retains_predecessor_consumed_steering() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let predecessor_consumed = accepted_origin(3);
    let active_consumed = accepted_origin(4);
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: frontier(70).id(),
            terminal_execution: None,
            terminal_frontier: frontier(71).id(),
        },
    );
    let active_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor.turn(),
            },
            starting_frontier: frontier(72).id(),
            phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                active.turn(),
                turn_attempt_id(73),
            ),
        },
    );
    let records = BTreeMap::from([
        (predecessor.turn(), &predecessor_record),
        (active.turn(), &active_record),
    ]);
    let accepted_input_turns = BTreeMap::from([
        (predecessor.accepted_input(), predecessor.turn()),
        (active.accepted_input(), active.turn()),
    ]);
    let execution_position_by_turn = BTreeMap::from([(predecessor.turn(), 0), (active.turn(), 1)]);
    let tail_input = SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        active.accepted_input(),
        active_consumed.position(),
        vec![
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    active.accepted_input(),
                    AcceptedInputDisposition::OriginOf(active.turn()),
                ),
                active.position(),
                default_origin_delivery(),
            ),
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    predecessor_consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: model_call_id(74),
                    },
                ),
                predecessor_consumed.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: predecessor.turn(),
                },
            ),
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    active_consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: model_call_id(75),
                    },
                ),
                active_consumed.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ),
        ],
    );
    let tail = reconstitute_active_acceptance_tail(
        session.id(),
        Some(active.turn()),
        Some(&tail_input),
        ActiveAcceptanceTailReconstitutionEvidence {
            records_by_turn: &records,
            accepted_input_turns: &accepted_input_turns,
            consumed_inputs: &BTreeMap::from([
                (predecessor_consumed.accepted_input(), model_call_id(74)),
                (active_consumed.accepted_input(), model_call_id(75)),
            ]),
            preceding_non_accepted_terminals: &BTreeSet::new(),
            execution_position_by_turn: &execution_position_by_turn,
        },
    )
    .expect("the terminal predecessor's consumed steering remains valid history")
    .expect("an active turn retains its complete acceptance tail");
    let (pending, consumed) = active_execution_steering_inputs(active.turn(), &tail);

    assert!(pending.is_empty());
    assert_eq!(consumed.len(), 1);
    assert_eq!(
        consumed[0].accepted_input(),
        active_consumed.accepted_input()
    );
    assert_eq!(consumed[0].source_turn(), active.turn());

    let mut cross_wired_tail = tail_input.clone();
    cross_wired_tail.entries[1] = SessionAcceptanceTailEntryReconstitutionInput::new(
        session.id(),
        AcceptedInputLifecycle::new(
            predecessor_consumed.accepted_input(),
            AcceptedInputDisposition::ConsumedAsSteering {
                call: model_call_id(76),
            },
        ),
        predecessor_consumed.position(),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: predecessor.turn(),
        },
    );
    let failure = reconstitute_active_acceptance_tail(
        session.id(),
        Some(active.turn()),
        Some(&cross_wired_tail),
        ActiveAcceptanceTailReconstitutionEvidence {
            records_by_turn: &records,
            accepted_input_turns: &accepted_input_turns,
            consumed_inputs: &BTreeMap::from([
                (predecessor_consumed.accepted_input(), model_call_id(74)),
                (active_consumed.accepted_input(), model_call_id(75)),
            ]),
            preceding_non_accepted_terminals: &BTreeSet::new(),
            execution_position_by_turn: &execution_position_by_turn,
        },
    )
    .expect_err("cross-wired historical steering must fail closed");

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: predecessor_consumed.accepted_input(),
        }
    );
}

/// later-accepted interrupt work executes before the
/// ordinary origin it displaced, so steering consumed by that interrupt
/// remains historical rather than becoming active execution input.
#[test]
fn active_tail_rejects_unproven_historical_consumed_steering() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let interrupt_successor = accepted_origin(3);
    let interrupt_consumed = accepted_origin(4);
    let mut input =
        active_input_after_historical_interrupt(&session, predecessor, active, interrupt_successor);
    let tail = input
        .active_acceptance_tail
        .as_mut()
        .expect("the historical-interrupt helper supplies an active tail");
    tail.observed_last_position = interrupt_consumed.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                interrupt_consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: model_call_id(76),
                },
            ),
            interrupt_consumed.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: interrupt_successor.turn(),
            },
        ));
    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: interrupt_consumed.accepted_input(),
        }
    );
}

/// a scheduler-gap start remains
/// a valid ordinary origin after an earlier queued turn becomes active.
#[test]
fn active_reconstitution_preserves_post_anchor_scheduler_gap_start() {
    let session = current_session();
    let origins = FailedPredecessorPostAnchorOrigins {
        predecessor: accepted_origin(1),
        active: accepted_origin(2),
        queued: accepted_origin(3),
    };
    active_input_after_failed_predecessor_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect("the later origin was accepted during a valid scheduler gap");
}

/// an ordinary queued origin
/// retains the historical active target named at acceptance.
#[test]
fn active_reconstitution_preserves_post_anchor_historical_target() {
    let session = current_session();
    let origins = FailedPredecessorPostAnchorOrigins {
        predecessor: accepted_origin(1),
        active: accepted_origin(2),
        queued: accepted_origin(3),
    };
    active_input_after_failed_predecessor_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: origins.predecessor.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect("the later origin retains its exact previously active target");
}

/// after-current delivery must
/// name an earlier nonqueued target in the complete turn inventory.
#[test]
fn active_reconstitution_rejects_missing_historical_delivery_target() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let missing_target_turn = turn_id(99);
    let missing_target = active_input_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: missing_target_turn,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect_err("after-current delivery requires its historical target record");
    assert_eq!(
        missing_target.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: origins.queued.turn(),
        }
    );
}

/// an interrupt delivery must
/// agree with the origin record's durable interrupt-priority relation.
#[test]
fn active_reconstitution_rejects_delivery_priority_mismatch() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let wrong_priority = active_input_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::Interrupt {
            expected_active_turn: origins.active.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    )
    .reconstitute()
    .expect_err("interrupt delivery cannot carry ordinary queue priority");
    assert_eq!(
        wrong_priority.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: origins.queued.turn(),
        }
    );
}

/// origin delivery and queue facts
/// are validated even when no active turn requires an acceptance tail.
#[test]
fn queued_reconstitution_rejects_delivery_order_mismatch() {
    let session = current_session();
    let queued = accepted_origin(1);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record_with(
            &session,
            OriginRecordFacts {
                order: queued.ordinary_order(),
                delivery: DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(99),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        )],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let error = input
        .reconstitute()
        .expect_err("steering-only delivery cannot reconstruct queued turn work");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: queued.turn(),
        }
    );
}

/// a configured origin's
/// accepted defaults version must equal its frozen provenance version.
#[test]
fn queued_origin_rejects_defaults_version_mismatch() {
    let session = current_session();
    let queued = accepted_origin(1);
    let mismatched_version = SessionConfigurationDefaultsVersion::try_from_u64(2)
        .expect("the mismatched test version is positive");
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record_with(
            &session,
            OriginRecordFacts {
                order: queued.ordinary_order(),
                delivery: DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        mismatched_version,
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        )],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let error = input
        .reconstitute()
        .expect_err("accepted delivery and frozen provenance versions must agree");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: queued.turn(),
        }
    );
}

/// an explicit accepted
/// model request must equal the request retained by frozen provenance.
#[test]
fn queued_origin_rejects_explicit_request_mismatch() {
    let session = current_session();
    let queued = accepted_origin(1);
    let requested = ModelSelectionRequest::Direct(direct(99));
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let no_active_acceptance_tail = None;
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![queued.record_with(
            &session,
            OriginRecordFacts {
                order: queued.ordinary_order(),
                delivery: DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::ReplaceWith(requested),
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        )],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    );

    let error = input
        .reconstitute()
        .expect_err("explicit delivery request and frozen provenance must agree");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: queued.turn(),
        }
    );
}

/// the tail repeats the exact
/// immutable versioned delivery stored for its origin rather than
/// supplying an independently plausible configuration choice.
#[test]
fn active_reconstitution_rejects_origin_delivery_configuration_mismatch() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the active tail")
        .entries[0]
        .delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::try_from_u64(2)
                .expect("the mismatched test version is positive"),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };

    let error = assert_reconstitution_rejects_unchanged(facts);
    assert_eq!(
        error,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: active.accepted_input(),
        }
    );
}

/// an accepted interrupt
/// against the current owner prevents evidence-free phase reconstruction.
#[test]
fn active_reconstitution_rejects_interrupt_evidence_for_evidence_free_phase() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let delivery = DeliveryRequest::Interrupt {
        expected_active_turn: origins.active.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let mut input = active_input_with_post_anchor_origin(&session, origins, delivery);
    input.turns[1] = origins.queued.record_with(
        &session,
        OriginRecordFacts {
            order: AcceptedInputQueueOrder::interrupt_immediately_after(
                origins.queued.position(),
                origins.active.turn(),
            ),
            delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );

    let error = input
        .reconstitute()
        .expect_err("applied interrupt evidence requires a proof-bearing phase projection");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
            turn: origins.active.turn(),
            accepted_input: origins.queued.accepted_input(),
        }
    );
}

/// a historical interrupt in the active
/// acceptance tail retains the target terminal's exact stop proof.
#[test]
fn historical_interrupt_requires_target_stop_proof() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let interrupt_successor = accepted_origin(3);
    let matching =
        active_input_after_historical_interrupt(&session, predecessor, active, interrupt_successor);
    matching
        .clone()
        .reconstitute()
        .expect("the exact historical interrupt proof remains admissible");

    let mut missing_proof = matching;
    let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
        terminal_execution, ..
    } = &mut missing_proof.turns[0].state
    else {
        panic!("the historical target fixture is terminal failed");
    };
    *terminal_execution = None;
    assert_eq!(
        assert_input_rejects_unchanged(missing_proof),
        AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
            turn: active.turn(),
            accepted_input: interrupt_successor.accepted_input(),
        }
    );
}

/// one accepted input cannot
/// be both pending steering and a turn origin in the scheduling inventory.
#[test]
fn active_reconstitution_rejects_pending_identity_that_is_also_an_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let mut tail = active.active_tail(&session);
    tail.observed_last_position = pending.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));

    active_input(&session, active, Some(tail.clone()))
        .reconstitute()
        .expect("pending steering remains distinct from every origin");

    let mut aliased = active_input(&session, active, Some(tail));
    aliased
        .turns
        .push(pending.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let aliased = aliased
        .reconstitute()
        .expect_err("pending steering cannot reuse a turn-origin identity");
    assert_eq!(
        aliased.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: pending.accepted_input(),
        }
    );
}

/// a prepared call consumes the complete
/// pending prefix; durable history cannot claim that it skipped an earlier
/// pending input and consumed a later one.
#[test]
fn active_tail_rejects_consumed_after_pending() {
    let session = current_session();
    let active = accepted_origin(1);
    let pending = accepted_origin(2);
    let consumed = accepted_origin(3);
    let mut tail = active.active_tail(&session);
    tail.observed_last_position = consumed.position();
    tail.entries.extend([
        SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            pending.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ),
        SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: model_call_id(91),
                },
            ),
            consumed.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ),
    ]);

    let error = active_input(&session, active, Some(tail))
        .reconstitute()
        .expect_err("a later consumed receipt cannot skip earlier pending steering");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: consumed.accepted_input(),
        }
    );
}

/// a pending tail entry cannot
/// replace a different origin that owns the same acceptance position.
#[test]
fn active_reconstitution_rejects_pending_position_owned_by_an_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin = accepted_origin(2);
    let pending = accepted_origin(3);
    let mut tail = active.active_tail(&session);
    tail.observed_last_position = origin.position();
    tail.entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            origin.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let mut input = active_input(&session, active, Some(tail));
    input
        .turns
        .push(origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));

    let error = input
        .reconstitute()
        .expect_err("the complete tail cannot replace an origin at the same position");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
            accepted_input: pending.accepted_input(),
        }
    );
}

/// the last represented position must equal
/// the authoritative session tail observed by the same read.
#[test]
fn active_reconstitution_rejects_incomplete_claimed_acceptance_tail() {
    let session = current_session();
    let active = accepted_origin(1);
    let next = accepted_origin(2);
    let mut incomplete = active.active_tail(&session);
    incomplete.observed_last_position = next.position();
    let incomplete = active_input(&session, active, Some(incomplete))
        .reconstitute()
        .expect_err("the represented interval must reach the claimed session tail");
    assert_eq!(
        incomplete.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
            expected: next.position(),
            actual: Some(active.position()),
        }
    );
}

/// the claimed session observation
/// cannot end before a later origin supplied by the same scheduling read.
#[test]
fn active_tail_reaches_every_known_origin() {
    let session = current_session();
    let origins = PostAnchorOrigins {
        active: accepted_origin(1),
        queued: accepted_origin(2),
    };
    let mut input = active_input_with_post_anchor_origin(
        &session,
        origins,
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    let tail = input
        .active_acceptance_tail
        .as_mut()
        .expect("the helper supplies an active tail");
    tail.observed_last_position = origins.active.position();
    tail.entries.truncate(1);

    let error = input
        .reconstitute()
        .expect_err("a known later origin disproves the claimed tail observation");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
            expected: origins.active.position(),
            actual: Some(origins.queued.position()),
        }
    );
}

/// a current attempt owned by another turn cannot
/// reconstruct an active aggregate.
#[test]
fn active_reconstitution_rejects_cross_wired_attempt_owner() {
    let session = current_session();
    let active = accepted_origin(1);
    let other_turn = turn_id(99);
    let attempt = matching_active_attempt();
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::prepared(
        other_turn, attempt,
    ));
    let error = assert_reconstitution_rejects_unchanged(facts);
    assert_eq!(
        error,
        AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
            turn: active.turn(),
            attempt,
        }
    );
}

/// eligibility derives the target from complete durable
/// order and cannot be directed to skip earlier queued work.
#[test]
fn eligibility_consumes_the_earliest_queued_origin() {
    let session = current_session();
    let later = accepted_origin(2);
    let earlier = accepted_origin(1);
    let later_record = later.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let earlier_record = earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let no_semantic_entries = Vec::new();
    let no_snapshots = Vec::new();
    let activation = activation(1);
    let no_active_acceptance_tail = None;
    let candidate = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        vec![later_record, earlier_record],
        no_semantic_entries,
        no_snapshots,
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect("the complete queue order is valid")
    .prepare_earliest_queued_activation(activation.identities())
    .expect("no active slot blocks the earliest queued work");

    assert_eq!(candidate.turn().turn(), earlier.turn());
    assert_eq!(
        candidate.turn().accepted_input().id(),
        earlier.accepted_input()
    );
}

/// the earliest queued successor starts only
/// after the exact immediately preceding failed turn and retains its
/// complete origin-then-failure terminal prefix before appending its own
/// origin.
#[test]
fn successor_uses_exact_failed_predecessor_terminal_frontier() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let predecessor_origin_entry = semantic_entry(30);
    let predecessor_failure_entry = semantic_entry(31);
    let predecessor_starting_frontier = frontier(40);
    let predecessor_terminal_frontier = frontier(41);
    let activation = activation(1);
    let no_active_acceptance_tail = None;
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: predecessor_starting_frontier.id(),
            terminal_execution: None,
            terminal_frontier: predecessor_terminal_frontier.id(),
        },
    );
    let successor_record =
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![successor_record, predecessor_record],
        vec![
            predecessor_failure_entry.failed_turn(&session, predecessor),
            predecessor.entry(&session, predecessor_origin_entry),
        ],
        vec![
            predecessor_terminal_frontier.snapshot(
                &session,
                &[predecessor_origin_entry, predecessor_failure_entry],
            ),
            predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
        ],
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect("the failed predecessor has a complete validated frontier");

    let candidate = projection
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the successor is the earliest queued turn with no active slot");

    assert_eq!(candidate.turn().turn(), successor.turn());
    assert_eq!(
        candidate.start().lineage(),
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor.turn(),
        }
    );
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            predecessor_origin_entry.reference(&session),
            predecessor_failure_entry.reference(&session),
            activation.origin_entry().reference(&session),
        ]
    );
}

/// an actual frozen direct-model transition
/// inserts exactly one typed identity boundary between the predecessor
/// terminal frontier and the successor origin.
#[test]
fn model_transition_extends_frontier_before_origin() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let successor_selection = direct(2);
    let predecessor_origin_entry = semantic_entry(30);
    let predecessor_failure_entry = semantic_entry(31);
    let predecessor_starting_frontier = frontier(40);
    let predecessor_terminal_frontier = frontier(41);
    let activation = activation(1);
    let successor_choices = PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(successor_selection)),
    );
    let successor_configuration = OriginConfiguration::freeze(
        session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                    successor_selection,
                )),
            )
            .expect("the override is derived from current defaults"),
        |_| None,
    )
    .expect("the direct selection needs no alias resolution");
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: predecessor_starting_frontier.id(),
            terminal_execution: None,
            terminal_frontier: predecessor_terminal_frontier.id(),
        },
    );
    let successor_record = AcceptedInputTurnSchedulingRecord::new(
        session.id(),
        successor.turn(),
        session.id(),
        AcceptedInputLifecycle::new(
            successor.accepted_input(),
            AcceptedInputDisposition::OriginOf(successor.turn()),
        ),
        session.id(),
        successor.turn(),
        successor.ordinary_order(),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: successor_choices,
        },
        successor_configuration,
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![successor_record, predecessor_record],
        vec![
            predecessor_failure_entry.failed_turn(&session, predecessor),
            predecessor.entry(&session, predecessor_origin_entry),
        ],
        vec![
            predecessor_terminal_frontier.snapshot(
                &session,
                &[predecessor_origin_entry, predecessor_failure_entry],
            ),
            predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
        ],
        None,
    )
    .reconstitute()
    .expect("the predecessor and changed successor are fully correlated");

    let candidate = projection
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the changed successor is eligible");

    assert_eq!(candidate.starting_entries().len(), 2);
    assert_eq!(
        candidate.starting_entries()[0].identity(),
        activation.model_identity_entry().id()
    );
    assert_eq!(
        candidate.starting_entries()[0].payload(),
        &SemanticTranscriptEntryPayload::ModelIdentityChanged {
            turn: successor.turn(),
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selected: successor_selection,
        }
    );
    assert_eq!(
        candidate
            .starting_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            predecessor_origin_entry.reference(&session),
            predecessor_failure_entry.reference(&session),
            activation.model_identity_entry().reference(&session),
            activation.origin_entry().reference(&session),
        ]
    );
}

/// a durable legacy marker admits only a start whose
/// frontier was committed before model-identity boundaries existed.
#[test]
fn legacy_start_grandfathers_its_historical_frontier() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let queued = accepted_origin(3);
    let predecessor_selection = direct(2);
    let predecessor_choices = PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(predecessor_selection)),
    );
    let predecessor_configuration = OriginConfiguration::freeze(
        session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                    predecessor_selection,
                )),
            )
            .expect("the legacy override is derived from current defaults"),
        |_| None,
    )
    .expect("the direct selection needs no alias resolution");
    let mut input = active_input_after_failed_predecessor_with_post_anchor_origin(
        &session,
        FailedPredecessorPostAnchorOrigins {
            predecessor,
            active,
            queued,
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: active.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        },
    );
    input.turns[0].origin_delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: predecessor_choices,
    };
    input.turns[0].origin_configuration = predecessor_configuration.clone();
    input.turns[0].configuration_provenance =
        TurnConfigurationProvenance::ExplicitOrigin(predecessor_configuration);

    let strict = input
        .clone()
        .reconstitute()
        .expect_err("a post-migration start cannot omit its changed-model boundary");
    assert_eq!(
        strict.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active.turn(),
        }
    );

    input.turns[1] = input.turns[1]
        .clone()
        .without_legacy_model_identity_boundary();
    input
        .reconstitute()
        .expect("the durable legacy bit retains the historical marker-free frontier");
}

/// terminally reclassified
/// steering becomes ordinary queued work at its original position and
/// inherits the source turn's canonical configuration.
#[test]
fn reclassified_steering_becomes_eligible_work() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let predecessor_origin_entry = semantic_entry(30);
    let predecessor_failure_entry = semantic_entry(31);
    let predecessor_starting_frontier = frontier(40);
    let predecessor_terminal_frontier = frontier(41);
    let activation = activation(1);
    let source_configuration = configuration(&session);
    let binding = crate::SteeringBinding::new(predecessor.turn());
    let predecessor_record = predecessor.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: predecessor_starting_frontier.id(),
            terminal_execution: None,
            terminal_frontier: predecessor_terminal_frontier.id(),
        },
    );
    let successor_record = AcceptedInputTurnSchedulingRecord::reclassified(
        session.id(),
        successor.turn(),
        session.id(),
        AcceptedInputLifecycle::new(
            successor.accepted_input(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: successor.turn(),
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        ),
        session.id(),
        successor.turn(),
        successor.ordinary_order(),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: predecessor.turn(),
        },
        binding,
        source_configuration.clone(),
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let mismatched_delivery_record = AcceptedInputTurnSchedulingRecord::reclassified(
        session.id(),
        successor.turn(),
        session.id(),
        AcceptedInputLifecycle::new(
            successor.accepted_input(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: successor.turn(),
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        ),
        session.id(),
        successor.turn(),
        successor.ordinary_order(),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(99),
        },
        binding,
        source_configuration.clone(),
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let semantic_entries = vec![
        predecessor_failure_entry.failed_turn(&session, predecessor),
        predecessor.entry(&session, predecessor_origin_entry),
    ];
    let snapshots = vec![
        predecessor_terminal_frontier.snapshot(
            &session,
            &[predecessor_origin_entry, predecessor_failure_entry],
        ),
        predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
    ];
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![mismatched_delivery_record, predecessor_record.clone()],
        semantic_entries.clone(),
        snapshots.clone(),
        None,
    )
    .reconstitute()
    .expect_err("stored reclassified delivery must agree with its exact source binding");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
            turn: successor.turn(),
        }
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![successor_record, predecessor_record],
        semantic_entries,
        snapshots,
        None,
    )
    .reconstitute()
    .expect("reclassified steering is correlated to its terminal source");

    let candidate = projection
        .prepare_earliest_queued_activation(activation.identities())
        .expect("the reclassified successor is eligible after its source");

    assert_eq!(candidate.turn().turn(), successor.turn());
    assert_eq!(candidate.turn().order(), successor.ordinary_order());
    assert_eq!(candidate.turn().configuration(), &source_configuration);
    assert_eq!(
        candidate.turn().configuration_provenance(),
        &TurnConfigurationProvenance::InheritedForReclassifiedSteering(binding)
    );
}

#[track_caller]
fn assert_failed_terminal_call_provenance_is_complete(
    session: &Session,
    failed: OriginFixture,
    attempt: TurnAttemptId,
    call_disposition: ModelCallDisposition,
) {
    let origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();
    let failure_entry = FailedTerminalReconstitutionFacts::matching_failure_entry();
    let steering_entry = semantic_entry(32);
    let consumed = accepted_origin(2);
    let call_frontier = frontier(42);
    let terminal_frontier = FailedTerminalReconstitutionFacts::matching_terminal_frontier();
    let call_id = model_call_id(50);
    let mut facts = FailedTerminalReconstitutionFacts::matching(session, failed);
    facts
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            steering_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: failed.turn(),
            },
        ));
    facts
        .snapshots
        .retain(|snapshot| snapshot.snapshot() != terminal_frontier.id());
    facts.snapshots.extend([
        call_frontier.snapshot(session, &[origin_entry, steering_entry]),
        terminal_frontier.snapshot(session, &[origin_entry, steering_entry, failure_entry]),
    ]);
    facts.replace_terminal_execution(Some(FailedTurnExecutionReconstitutionInput::with_call(
        failed.turn(),
        attempt,
        UnstoppedAttemptDisposition::KnownFailure,
        call_id,
    )));
    let call = ModelCallReconstitutionInput::new(
        call_id,
        failed.turn(),
        attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        call_frontier.id(),
        ModelCallReconstitutionState::Terminal(call_disposition),
    );
    let projection = facts
        .input()
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                failed.turn(),
                call.target(),
            )],
            vec![call],
        )
        .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering { call: call_id },
            ),
            consumed.position(),
            failed.turn(),
        )])
        .reconstitute()
        .expect("failed terminal call provenance is fully correlated");
    assert_eq!(
        projection.attempt_owners.get(&attempt),
        Some(&failed.turn())
    );
    assert!(
        projection
            .semantic_entries
            .contains_key(&origin_entry.reference(session))
    );
}

/// failed-terminal reconstitution
/// preserves all three accepted execution shapes and any steering already
/// committed in an ended call's source frontier.
#[test]
fn failed_terminal_execution_provenance_is_complete() {
    let session = current_session();
    let failed = accepted_origin(1);
    let attempt = turn_attempt_id(60);

    let direct_failure = FailedTerminalReconstitutionFacts::matching(&session, failed)
        .input()
        .reconstitute()
        .expect("a direct static failure has no execution provenance");
    assert!(direct_failure.attempt_owners.is_empty());

    let mut attempt_only_facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    attempt_only_facts.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only(
            failed.turn(),
            attempt,
            UnstoppedAttemptDisposition::Lost,
        ),
    ));
    let attempt_only = attempt_only_facts
        .input()
        .reconstitute()
        .expect("startup loss retains its exact ended attempt");
    assert_eq!(
        attempt_only.attempt_owners.get(&attempt),
        Some(&failed.turn())
    );

    assert_failed_terminal_call_provenance_is_complete(
        &session,
        failed,
        attempt,
        ModelCallDisposition::KnownFailed,
    );
    assert_failed_terminal_call_provenance_is_complete(
        &session,
        failed,
        attempt,
        ModelCallDisposition::Cancelled,
    );
}

/// a proof-bearing known-failure attempt
/// can only correlate a physically known-failed call. Confirmed physical
/// cancellation remains the cancelled terminal outcome.
#[test]
fn stopped_failure_rejects_cancelled_call() {
    let session = current_session();
    let failed = accepted_origin(1);
    let successor = accepted_origin(2);
    let attempt = turn_attempt_id(60);
    let call_id = model_call_id(50);
    let starting_frontier = FailedTerminalReconstitutionFacts::matching_starting_frontier();
    let successor_order =
        AcceptedInputQueueOrder::interrupt_immediately_after(successor.position(), failed.turn());
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        failed.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
            failed.turn(),
            attempt,
            CancellationStopDisposition::KnownFailure,
            interrupt,
            call_id,
        ),
    ));
    facts.turns.push(successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: failed.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    ));
    let input_for = |disposition| {
        let call = ModelCallReconstitutionInput::new(
            call_id,
            failed.turn(),
            attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(disposition),
        );
        facts.clone().input().with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                failed.turn(),
                call.target(),
            )],
            vec![call],
        )
    };

    input_for(ModelCallDisposition::KnownFailed)
        .reconstitute()
        .expect("stopped known failure retains its known-failed call");
    assert_eq!(
        assert_input_rejects_unchanged(input_for(ModelCallDisposition::Cancelled)),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}

/// failed-terminal attempt provenance fails closed
/// when either ownership or the allowed terminal end is contradicted.
#[test]
fn failed_terminal_attempt_provenance_fails_closed() {
    let session = current_session();
    let failed = accepted_origin(1);
    let attempt = turn_attempt_id(60);

    let mut wrong_owner = FailedTerminalReconstitutionFacts::matching(&session, failed);
    wrong_owner.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only(
            turn_id(99),
            attempt,
            UnstoppedAttemptDisposition::KnownFailure,
        ),
    ));
    assert_eq!(
        assert_input_rejects_unchanged(wrong_owner.input()),
        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptOwnershipMismatch {
            turn: failed.turn(),
            attempt,
        }
    );

    let mut wrong_end = FailedTerminalReconstitutionFacts::matching(&session, failed);
    wrong_end.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only(
            failed.turn(),
            attempt,
            UnstoppedAttemptDisposition::TurnCompleted,
        ),
    ));
    assert_eq!(
        assert_input_rejects_unchanged(wrong_end.input()),
        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
            turn: failed.turn(),
            attempt,
        }
    );

    let successor = accepted_origin(2);
    let successor_order =
        AcceptedInputQueueOrder::interrupt_immediately_after(successor.position(), failed.turn());
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        failed.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let mut lost_after_cancellation = FailedTerminalReconstitutionFacts::matching(&session, failed);
    lost_after_cancellation.replace_terminal_execution(Some(
        FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
            failed.turn(),
            attempt,
            CancellationStopDisposition::Lost,
            interrupt,
        ),
    ));
    lost_after_cancellation.turns.push(successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn: failed.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    ));
    assert_eq!(
        assert_input_rejects_unchanged(lost_after_cancellation.input()),
        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
            turn: failed.turn(),
            attempt,
        }
    );
}

/// a failed terminal call must match the ended
/// attempt and the turn's selection, target, starting frontier, and
/// KnownFailed-or-Cancelled physical disposition.
#[test]
fn failed_terminal_call_provenance_fails_closed() {
    let session = current_session();
    let failed = accepted_origin(1);
    let attempt = turn_attempt_id(60);
    let call_id = model_call_id(50);
    let starting_frontier = FailedTerminalReconstitutionFacts::matching_starting_frontier();
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts.replace_terminal_execution(Some(FailedTurnExecutionReconstitutionInput::with_call(
        failed.turn(),
        attempt,
        UnstoppedAttemptDisposition::KnownFailure,
        call_id,
    )));
    let mismatched_call = ModelCallReconstitutionInput::new(
        call_id,
        failed.turn(),
        turn_attempt_id(61),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        starting_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::KnownFailed),
    );
    let input = facts.input().with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            failed.turn(),
            mismatched_call.target(),
        )],
        vec![mismatched_call],
    );
    assert_eq!(
        assert_input_rejects_unchanged(input),
        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: failed.turn(),
        }
    );
}

/// a live or startup-recovered completed response validates the
/// producing call's steering-extended source, stop provenance, and final
/// marker before the exact terminal frontier becomes the successor's
/// starting prefix.
#[test]
fn completed_frontier_becomes_successor_prefix() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let consumed = accepted_origin(2);
    let successor = accepted_origin(3);
    let origin_entry = semantic_entry(30);
    let steering_entry = semantic_entry(31);
    let assistant_entry = semantic_entry(32);
    let completion_entry = semantic_entry(33);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let completing_call = model_call_id(50);
    let activation = activation(2);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        predecessor.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        predecessor.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let interrupt_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: predecessor.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let assert_case = |completing_attempt_end, queued_record| {
        let terminal_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                completing_attempt: turn_attempt_id(60),
                completing_attempt_end,
                completing_call,
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let steering = SemanticTranscriptEntryReconstitutionInput::new(
            steering_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: predecessor.turn(),
            },
        );
        let assistant = SemanticTranscriptEntryReconstitutionInput::new(
            assistant_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: completing_call,
                value: AssistantText::try_new(String::from("reply"))
                    .expect("test assistant text is nonempty"),
            },
        );
        let completion = SemanticTranscriptEntryReconstitutionInput::new(
            completion_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCompleted {
                turn: predecessor.turn(),
            },
        );
        let call = ModelCallReconstitutionInput::new(
            completing_call,
            predecessor.turn(),
            turn_attempt_id(60),
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            call_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued_record, terminal_record],
            vec![
                assistant,
                completion,
                steering,
                predecessor.entry(&session, origin_entry),
            ],
            vec![
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        steering_entry,
                        assistant_entry,
                        completion_entry,
                    ],
                ),
                call_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                starting_frontier.snapshot(&session, &[origin_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )],
            vec![call],
        )
        .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: completing_call,
                },
            ),
            consumed.position(),
            predecessor.turn(),
        )])
        .reconstitute()
        .expect("the completed predecessor is fully correlated");

        let collision = projection
            .clone()
            .prepare_earliest_queued_activation(
                activation.identities_with_attempt(turn_attempt_id(60)),
            )
            .expect_err("a terminal attempt identity cannot be minted again");
        assert_eq!(
            collision.failure(),
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
        );

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the completed predecessor releases the progressing slot");

        assert_eq!(candidate.turn().turn(), successor.turn());
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                origin_entry.reference(&session),
                steering_entry.reference(&session),
                assistant_entry.reference(&session),
                completion_entry.reference(&session),
                activation.origin_entry().reference(&session),
            ]
        );
    };

    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(
            UnstoppedAttemptDisposition::TurnCompleted,
        ),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Lost),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::TurnCompleted,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::Lost,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
}

/// one physical attempt identity cannot
/// back terminal outcomes for two different turns.
#[test]
fn terminal_turns_reject_shared_attempt_identity() {
    let session = current_session();
    let completed = accepted_origin(1);
    let refused = accepted_origin(2);
    let completed_origin = semantic_entry(30);
    let assistant = semantic_entry(31);
    let completion = semantic_entry(32);
    let refused_origin = semantic_entry(33);
    let completed_start = frontier(40);
    let completed_terminal = frontier(41);
    let refused_start = frontier(42);
    let refused_terminal = frontier(43);
    let shared_attempt = turn_attempt_id(60);
    let completed_call = model_call_id(50);
    let refused_call = model_call_id(51);
    let completed_start_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        completed_start.id(),
        vec![completed_origin.reference(&session)],
    )
    .expect("the completed call frontier has unique membership");
    let refused_start_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        refused_start.id(),
        vec![
            completed_origin.reference(&session),
            assistant.reference(&session),
            completion.reference(&session),
            refused_origin.reference(&session),
        ],
    )
    .expect("the refused call frontier has unique membership");
    let completed_record = completed.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: completed_start.id(),
            completing_attempt: shared_attempt,
            completing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnCompleted,
            ),
            completing_call: completed_call,
            terminal_frontier: completed_terminal.id(),
        },
    );
    let refused_record = refused.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::After {
                immediate_predecessor: completed.turn(),
            },
            starting_frontier: refused_start.id(),
            refusing_attempt: shared_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call: refused_call,
            terminal_frontier: refused_terminal.id(),
        },
    );
    let assistant_entry = SemanticTranscriptEntryReconstitutionInput::new(
        assistant.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::AssistantText {
            producing_call: completed_call,
            value: AssistantText::try_new("reply".to_owned())
                .expect("test assistant text is nonempty"),
        },
    );
    let completion_entry = SemanticTranscriptEntryReconstitutionInput::new(
        completion.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCompleted {
            turn: completed.turn(),
        },
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![refused_record, completed_record],
        vec![
            completed.entry(&session, completed_origin),
            assistant_entry,
            completion_entry,
            refused.entry(&session, refused_origin),
        ],
        vec![
            completed_start.snapshot(&session, &[completed_origin]),
            completed_terminal.snapshot(&session, &[completed_origin, assistant, completion]),
            refused_start.snapshot(
                &session,
                &[completed_origin, assistant, completion, refused_origin],
            ),
            refused_terminal.snapshot(
                &session,
                &[completed_origin, assistant, completion, refused_origin],
            ),
        ],
        None,
    )
    .with_model_call_facts(
        vec![
            crate::PinnedProviderTargetReconstitutionInput::new(
                completed.turn(),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
            ),
            crate::PinnedProviderTargetReconstitutionInput::new(
                refused.turn(),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
            ),
        ],
        vec![
            ModelCallReconstitutionInput::new(
                completed_call,
                completed.turn(),
                shared_attempt,
                FrozenModelSelection::Direct(direct(1)),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
                completed_start_snapshot.frontier().snapshot(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            ),
            ModelCallReconstitutionInput::new(
                refused_call,
                refused.turn(),
                shared_attempt,
                FrozenModelSelection::Direct(direct(1)),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
                refused_start_snapshot.frontier().snapshot(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
            ),
        ],
    );

    let error = input
        .reconstitute()
        .expect_err("one attempt cannot terminalize two turns");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
            attempt: shared_attempt,
        }
    );
}

/// a live or startup-recovered refusal validates the producing
/// call's steering-extended source and stop provenance, releases the slot,
/// and preserves its equal-content terminal frontier as the successor's
/// exact prefix.
#[test]
fn refused_frontier_becomes_successor_prefix() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let consumed = accepted_origin(2);
    let successor = accepted_origin(3);
    let origin_entry = semantic_entry(30);
    let steering_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let call_frontier = frontier(41);
    let terminal_frontier = frontier(42);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let activation = activation(2);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        predecessor.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        predecessor.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let interrupt_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: predecessor.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let assert_case = |refusing_attempt_end, queued_record| {
        let terminal_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                refusing_attempt,
                refusing_attempt_end,
                refusing_call,
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let steering = SemanticTranscriptEntryReconstitutionInput::new(
            steering_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: predecessor.turn(),
            },
        );
        let call = ModelCallReconstitutionInput::new(
            refusing_call,
            predecessor.turn(),
            refusing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            call_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued_record, terminal_record],
            vec![predecessor.entry(&session, origin_entry), steering],
            vec![
                terminal_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                call_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                starting_frontier.snapshot(&session, &[origin_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )],
            vec![call],
        )
        .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: refusing_call,
                },
            ),
            consumed.position(),
            predecessor.turn(),
        )])
        .reconstitute()
        .expect("the refused predecessor is fully correlated");

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the refused predecessor releases the progressing slot");

        assert_eq!(candidate.turn().turn(), successor.turn());
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                origin_entry.reference(&session),
                steering_entry.reference(&session),
                activation.origin_entry().reference(&session),
            ]
        );
    };

    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(
            UnstoppedAttemptDisposition::TurnRefused,
        ),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Lost),
        successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::TurnRefused,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
    assert_case(
        TerminalAttemptEndReconstitutionInput::after_cancellation(
            CancellationStopDisposition::Lost,
            interrupt,
        ),
        successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: interrupt_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ),
    );
}

/// assistant text cannot name a refused call because only
/// completed physical calls can produce semantic assistant content.
#[test]
fn refused_call_rejects_assistant_content() {
    let session = current_session();
    let origin = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let assistant_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let terminal_record = origin.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let assistant = SemanticTranscriptEntryReconstitutionInput::new(
        assistant_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::AssistantText {
            producing_call: refusing_call,
            value: AssistantText::try_new(String::from("not a refusal"))
                .expect("test assistant text is nonempty"),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        refusing_call,
        origin.turn(),
        refusing_attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record],
        vec![assistant, origin.entry(&session, origin_entry)],
        vec![
            terminal_frontier.snapshot(&session, &[origin_entry]),
            starting_frontier.snapshot(&session, &[origin_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    );

    let error = input
        .reconstitute()
        .expect_err("refused calls cannot produce assistant content");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMismatch {
            entry: assistant_entry.id(),
            call: refusing_call,
        }
    );
}

#[test]
fn refused_compaction_suffix_reconstitutes_exact_terminal_frontier() {
    let session = current_session();
    let origin = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let compaction_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let terminal_record = origin.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            refusing_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let compaction = SemanticTranscriptEntryReconstitutionInput::new(
        compaction_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ProviderCompaction {
            producing_call: refusing_call,
            block: crate::ProviderCompactionBlock::try_new(String::from(
                r#"{"type":"compaction","content":"retained refusal summary"}"#,
            ))
            .expect("the fixture compaction block is valid"),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        refusing_call,
        origin.turn(),
        refusing_attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
    );
    AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record],
        vec![origin.entry(&session, origin_entry), compaction],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, compaction_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    )
    .reconstitute()
    .expect("a refusal reload accepts its exact provider-compaction suffix");
}

/// a terminal refusal must be backed by the
/// stored ended-attempt refusal disposition, not only a matching identity.
#[test]
fn refused_turn_rejects_attempt_disposition_mismatch() {
    let session = current_session();
    let origin = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let refusing_call = model_call_id(50);
    let refusing_attempt = turn_attempt_id(60);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let terminal_record = origin.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            refusing_attempt,
            refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::KnownFailure,
            ),
            refusing_call,
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        refusing_call,
        origin.turn(),
        refusing_attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record],
        vec![origin.entry(&session, origin_entry)],
        vec![
            terminal_frontier.snapshot(&session, &[origin_entry]),
            starting_frontier.snapshot(&session, &[origin_entry]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    );

    let error = input
        .reconstitute()
        .expect_err("a refusal cannot be inferred from attempt identity alone");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: origin.turn(),
        }
    );
}

/// a terminal-cancelled projection
/// validates the stored attempt end rather than inferring it from the
/// separately supplied interrupt result.
#[test]
fn cancelled_turn_rejects_attempt_end_mismatch() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let cancellation_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let attempt = turn_attempt_id(50);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let terminal_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                attempt,
                TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::Ambiguous,
                ),
                None,
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: cancelled.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: successor_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let cancellation_entry = SemanticTranscriptEntryReconstitutionInput::new(
        cancellation_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCancelled {
            turn: cancelled.turn(),
        },
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record, successor_record],
        vec![cancelled.entry(&session, origin_entry), cancellation_entry],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry, semantic_entry(31)]),
        ],
        None,
    );

    let error = input
        .reconstitute()
        .expect_err("cancelled turn authority cannot substitute for its attempt end");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
            turn: cancelled.turn(),
            attempt,
        }
    );
}

/// a cancelled call frontier must
/// preserve the starting frontier rather than substituting unrelated
/// semantic history before the cancellation marker.
#[test]
fn cancelled_turn_rejects_unrelated_call_frontier() {
    let session = current_session();
    let cancelled = accepted_origin(1);
    let successor = accepted_origin(2);
    let origin_entry = semantic_entry(30);
    let cancellation_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let unrelated_call_frontier = frontier(42);
    let call = model_call_id(49);
    let attempt = turn_attempt_id(50);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        cancelled.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(60),
        session.id(),
        cancelled.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let terminal_record = cancelled.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                cancelled.turn(),
                attempt,
                TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Cancelled,
                    interrupt,
                ),
                Some(call),
                interrupt,
            ),
            terminal_frontier: terminal_frontier.id(),
        },
    );
    let successor_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: cancelled.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: successor_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let cancellation_entry = SemanticTranscriptEntryReconstitutionInput::new(
        cancellation_entry.id(),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCancelled {
            turn: cancelled.turn(),
        },
    );
    let stored_call = ModelCallReconstitutionInput::new(
        call,
        cancelled.turn(),
        attempt,
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        unrelated_call_frontier.id(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![terminal_record, successor_record],
        vec![cancelled.entry(&session, origin_entry), cancellation_entry],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            unrelated_call_frontier.snapshot(&session, &[]),
            terminal_frontier.snapshot(&session, &[semantic_entry(31)]),
        ],
        None,
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            cancelled.turn(),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
        )],
        vec![stored_call],
    );

    let error = input
        .reconstitute()
        .expect_err("a cancelled call cannot replace its turn's starting history");
    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
            turn: cancelled.turn(),
        }
    );
}

/// complete ambiguous-call facts reconstruct the
/// exact recovery wait and preserve the active progressing slot.
#[test]
fn ambiguous_call_reconstructs_recovery_wait() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let starting_frontier = frontier(40);
    let ambiguous_call = model_call_id(50);
    let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the call frontier has unique membership");
    let active_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            phase: ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                active.turn(),
                turn_attempt_id(60),
                ambiguous_call,
            ),
        },
    );
    let call = ModelCallReconstitutionInput::new(
        ambiguous_call,
        active.turn(),
        turn_attempt_id(60),
        FrozenModelSelection::Direct(direct(1)),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        resolved_starting.frontier().snapshot(),
        ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![active_record],
        vec![active.entry(&session, origin_entry)],
        vec![starting_frontier.snapshot(&session, &[origin_entry])],
        Some(active.active_tail(&session)),
    )
    .with_model_call_facts(
        vec![crate::PinnedProviderTargetReconstitutionInput::new(
            call.turn(),
            call.target(),
        )],
        vec![call],
    )
    .reconstitute()
    .expect("the ambiguous call and wait are fully correlated");
    let waiting = projection
        .active_turn()
        .expect("the recovery wait retains the progressing slot");

    assert!(matches!(
        waiting.active_phase(),
        Some(ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations,
            ..
        }) if ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(ambiguous_call))
    ));
}

/// an opaque wait from
/// a completely validated ambiguous tool batch reconstructs the exact
/// typed recovery subject and preserves it through interruption.
#[test]
fn interrupting_tool_recovery_preserves_exact_ambiguity() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let starting_frontier = frontier(40);
    let producing_call = model_call_id(50);
    let issuing_attempt = turn_attempt_id(60);
    let request = ToolRequestReconstitutionInput::new(
        tool_request_id(70),
        session.id(),
        active.turn(),
        producing_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("external_tool")).expect("fixture name is canonical"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request();
    let approval = ToolApprovalResolutionReconstitutionInput::user_fixture(
        request.id(),
        ToolApprovalDecision::Approve,
    )
    .reconstitute()
    .expect("user approval is implemented");
    let expected_tool_attempt = tool_attempt_id(80);
    let tool_attempt = ToolAttemptReconstitutionInput::new(
        expected_tool_attempt,
        request.id(),
        session.id(),
        active.turn(),
        issuing_attempt,
        ToolEffectClass::ExternalEffect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
    )
    .reconstitute()
    .expect("the first tool dispatch generation is supported");
    let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    )
    .expect("the yielded snapshot is valid");
    let expected_request = request.id();
    let batch = ToolBatchReconstitutionInput::new(
        session.id(),
        active.turn(),
        producing_call,
        yielded,
        vec![request],
        vec![approval],
        vec![tool_attempt],
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
            attempt: expected_tool_attempt,
        },
    )
    .reconstitute()
    .expect("the complete tool batch is exactly ambiguous");
    let wait = batch
        .awaiting_recovery()
        .expect("the validated batch exposes opaque wait evidence");
    let cross_wired_record = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            phase: ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery(
                active.turn(),
                turn_attempt_id(61),
                wait,
            ),
        },
    );
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![cross_wired_record],
        vec![active.entry(&session, origin_entry)],
        vec![starting_frontier.snapshot(&session, &[origin_entry])],
        Some(active.active_tail(&session)),
    )
    .reconstitute()
    .expect_err("the wait cannot be attached to another turn attempt");
    let AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch { turn, .. } =
        error.failure()
    else {
        panic!("the cross-wired wait fails as an active-phase mismatch");
    };
    assert_eq!(*turn, active.turn());
    let successor = accepted_origin(2);
    let successor_order =
        AcceptedInputQueueOrder::interrupt_immediately_after(successor.position(), active.turn());
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(90),
        session.id(),
        active.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let terminal_tool_reconciliation = active.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: starting_frontier.id(),
            reconciling_attempt: issuing_attempt,
            reconciling_attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::Lost,
                interrupt,
            ),
            tool_batch: batch.clone(),
            authority: AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
            terminal_frontier: starting_frontier.id(),
        },
    );
    assert!(
        scheduling_record_is_terminal(&terminal_tool_reconciliation),
        "tool reconciliation is terminal historical proof"
    );
    let active_record = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                phase:
                    ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_cancellation_restart(
                    active.turn(),
                    issuing_attempt,
                    wait,
                    interrupt,
                ),
            },
        );
    let successor_delivery = DeliveryRequest::Interrupt {
        expected_active_turn: active.turn(),
        descendant_scope: DescendantTerminationScope::ParentAlone,
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let successor_record = successor.record_with(
        &session,
        OriginRecordFacts {
            order: successor_order,
            delivery: successor_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Queued,
        },
    );
    let acceptance_tail = SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        active.accepted_input(),
        successor.position(),
        vec![
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    active.accepted_input(),
                    AcceptedInputDisposition::OriginOf(active.turn()),
                ),
                active.position(),
                default_origin_delivery(),
            ),
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    successor.accepted_input(),
                    AcceptedInputDisposition::OriginOf(successor.turn()),
                ),
                successor.position(),
                successor_delivery,
            ),
        ],
    );
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![active_record, successor_record],
        vec![active.entry(&session, origin_entry)],
        vec![starting_frontier.snapshot(&session, &[origin_entry])],
        Some(acceptance_tail),
    )
    .reconstitute()
    .expect("the opaque tool wait and ended turn attempt are correlated");
    let waiting = projection
        .active_turn()
        .expect("the recovery wait retains the progressing slot");

    let Some(ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        ..
    }) = waiting.active_phase()
    else {
        panic!("the opaque tool wait remains an active recovery decision");
    };
    assert!(
        ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(
            expected_tool_attempt
        ))
    );

    let retained_request = batch
        .requests()
        .first()
        .expect("the one-request batch retains its request");
    assert_eq!(retained_request.id(), expected_request);
    let Some(crate::ReconstitutedToolAttempt::Ended(ended_tool)) =
        batch.attempt(retained_request.id())
    else {
        panic!("the batch retains its ended ambiguous attempt");
    };
    assert_eq!(ended_tool.attempt(), expected_tool_attempt);
    let ended_tool = ended_tool.clone();
    let result_entry = semantic_entry(31);
    let result_projection = batch
        .prepare_reconciliation_projection(vec![result_entry.id()], frontier(41).id())
        .expect("the terminal batch closes its logical request");
    let reconciled = projection
        .apply_interrupt_to_tool_recovery(
            wait,
            ended_tool,
            result_projection,
            interrupt,
            crate::AmbiguousModelCallTurnIdentities::new(frontier(41).id()),
        )
        .expect("the interrupt retains exact tool ambiguity");
    assert_eq!(reconciled.tool_attempt().attempt(), expected_tool_attempt);
    assert_eq!(
        reconciled.attempt().end(),
        &AttemptEnd::AfterCancellation {
            cause: interrupt.proof(),
            disposition: CancellationStopDisposition::Lost,
        }
    );
    assert_eq!(
        reconciled
            .terminal_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>(),
        vec![
            origin_entry.reference(&session),
            result_entry.reference(&session)
        ]
    );
    assert_eq!(
        reconciled.tool_result_entries()[0].payload(),
        &crate::SemanticTranscriptEntryPayload::ToolClosed {
            request: expected_request,
        }
    );
    let crate::TurnDisposition::ReconciliationRequired { marker } = reconciled.disposition() else {
        panic!("the interrupted ambiguity requires reconciliation");
    };
    assert!(
        marker
            .ambiguous_operations()
            .contains(crate::IssuedOperationRef::ToolAttempt(
                expected_tool_attempt
            ))
    );
}

/// a later interrupt supplies terminal authority
/// without being rewritten into an already ambiguous attempt end.
#[test]
fn tool_reconciliation_retains_without_stop_attempt_end() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let successor = accepted_origin(2);
    let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
        successor.position(),
        predecessor.turn(),
    );
    let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
        command_id(90),
        session.id(),
        predecessor.turn(),
        successor.accepted_input(),
        successor.turn(),
        successor_order,
    )
    .expect("the fixture interrupt is exactly correlated");
    let attempt_end =
        TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Ambiguous);

    assert!(tool_reconciliation_attempt_end_matches(
        &attempt_end,
        Some(interrupt),
    ));
}

/// a predecessor snapshot that omits its required failed
/// marker is not a terminal frontier and cannot authorize a successor.
#[test]
fn incomplete_failed_terminal_frontier_fails_closed() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let failure_entry = semantic_entry(31);
    let starting_frontier = frontier(40);
    let terminal_frontier = frontier(41);
    let no_active_acceptance_tail = None;
    let error = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        vec![predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: None,
                terminal_frontier: terminal_frontier.id(),
            },
        )],
        vec![
            predecessor.entry(&session, origin_entry),
            failure_entry.failed_turn(&session, predecessor),
        ],
        vec![
            starting_frontier.snapshot(&session, &[origin_entry]),
            terminal_frontier.snapshot(&session, &[origin_entry]),
        ],
        no_active_acceptance_tail,
    )
    .reconstitute()
    .expect_err("the failed marker must follow the exact starting prefix");

    assert_eq!(
        error.failure(),
        &AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
            turn: predecessor.turn(),
        }
    );
}

/// imported ancestry is admitted only together
/// with its exact complete independently checked seed projection.
#[test]
fn imported_scheduling_requires_exact_seed_projection() {
    let imported = imported_session();
    let session = imported.session().clone();
    let queued = accepted_origin(1);

    let missing = assert_input_rejects_unchanged(queued_input(&session, queued));
    assert_eq!(
        missing,
        AcceptedInputSchedulingReconstitutionFailure::MissingImportedSession
    );

    let mismatched = assert_input_rejects_unchanged(
        queued_input(&session, queued).with_imported_session(imported_session_for(2)),
    );
    assert_eq!(
        mismatched,
        AcceptedInputSchedulingReconstitutionFailure::ImportedSessionMismatch
    );

    let unexpected = assert_input_rejects_unchanged(
        queued_input(&current_session(), queued).with_imported_session(imported),
    );
    assert_eq!(
        unexpected,
        AcceptedInputSchedulingReconstitutionFailure::UnexpectedImportedSession
    );
}

/// this closed slice still cannot resolve a first frontier
/// from native session ancestry, so an otherwise-valid queued projection
/// for a native ancestral session fails closed.
#[test]
fn reconstitution_rejects_ancestral_session() {
    let ancestral = session_id(1);
    let version = SessionConfigurationDefaultsVersion::first();
    let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1)));
    let session = SessionReconstitutionInput::new(
        ancestral,
        ancestral,
        SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::SingleSource {
                source_session: session_id(9),
                source_frontier: transcript_frontier(9),
            },
        ),
        ancestral,
        version,
        ancestral,
        version,
        defaults,
        crate::SessionPlacementReconstitutionFacts {
            current_pointer_session: ancestral,
            current_pointer_version: crate::SessionPlacementVersion::INITIAL,
            selected_event_session: ancestral,
            selected_event: crate::VersionedSessionPlacement::initial(
                crate::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .expect("ancestral session facts are fully correlated");
    let queued = accepted_origin(1);

    let failure = assert_input_rejects_unchanged(queued_input(&session, queued));

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::UnsupportedSessionAncestry
    );
}

/// every stored session and turn correlation on one
/// scheduling record must repeat the owning identities exactly; each
/// cross-wired stored identity fails closed with its own failure.
#[test]
fn reconstitution_rejects_cross_wired_record_identities() {
    let session = current_session();
    let queued = accepted_origin(1);
    let other_session = session_id(2);
    let other_turn = turn_id(99);

    let mut turn_session_facts = queued_input(&session, queued);
    turn_session_facts.turns[0].stored_session = other_session;
    let turn_session = assert_input_rejects_unchanged(turn_session_facts);
    assert_eq!(
        turn_session,
        AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch {
            turn: queued.turn(),
        }
    );

    let mut accepted_input_session_facts = queued_input(&session, queued);
    accepted_input_session_facts.turns[0].accepted_input_session = other_session;
    let accepted_input_session = assert_input_rejects_unchanged(accepted_input_session_facts);
    assert_eq!(
        accepted_input_session,
        AcceptedInputSchedulingReconstitutionFailure::AcceptedInputSessionMismatch {
            turn: queued.turn(),
        }
    );

    let mut queue_session_facts = queued_input(&session, queued);
    queue_session_facts.turns[0].queue_session = other_session;
    let queue_session = assert_input_rejects_unchanged(queue_session_facts);
    assert_eq!(
        queue_session,
        AcceptedInputSchedulingReconstitutionFailure::QueueSessionMismatch {
            turn: queued.turn(),
        }
    );

    let mut queue_turn_facts = queued_input(&session, queued);
    queue_turn_facts.turns[0].queue_turn = other_turn;
    let queue_turn = assert_input_rejects_unchanged(queue_turn_facts);
    assert_eq!(
        queue_turn,
        AcceptedInputSchedulingReconstitutionFailure::QueueTurnMismatch {
            turn: queued.turn(),
        }
    );

    expect![[r#"
            ┌───────────────────────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact                     │ failure                                                                             │
            ├───────────────────────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────┤
            │ turn record session cross-wired           │ TurnSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }          │
            │ accepted-input record session cross-wired │ AcceptedInputSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            │ queue record session cross-wired          │ QueueSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }         │
            │ queue record turn cross-wired             │ QueueTurnMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }            │
            └───────────────────────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "turn record session cross-wired",
                failure: format!("{turn_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "accepted-input record session cross-wired",
                failure: format!("{accepted_input_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "queue record session cross-wired",
                failure: format!("{queue_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "queue record turn cross-wired",
                failure: format!("{queue_turn:?}"),
            },
        ]));
}

/// two turn records cannot both claim one
/// accepted input as their typed durable origin.
#[test]
fn reconstitution_rejects_shared_accepted_input_identity() {
    let session = current_session();
    let first = accepted_origin(1);
    let second = accepted_origin(2);
    let mut input = queued_input(&session, first);
    input
        .turns
        .push(second.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    input.turns[1].accepted_input = AcceptedInputLifecycle::new(
        first.accepted_input(),
        AcceptedInputDisposition::OriginOf(second.turn()),
    );

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptedInput {
            accepted_input: first.accepted_input(),
        }
    );
}

/// a delegation-origin turn fact cannot also be represented
/// by an accepted-input lifecycle record.
#[test]
fn reconstitution_rejects_delegated_accepted_turn_fact() {
    let session = current_session();
    let queued = accepted_origin(1);
    let input = queued_input(&session, queued).with_delegated_turn_facts(vec![
        DelegatedTurnSchedulingFact::new(
            queued.turn(),
            SessionConfigurationDefaultsVersion::first(),
            direct(1),
            DelegatedTurnSchedulingState::Active,
        ),
    ]);

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch {
            turn: queued.turn(),
        }
    );
}

/// complete delegation-origin turn facts cannot duplicate
/// the same stored turn identity.
#[test]
fn reconstitution_rejects_duplicate_delegated_turn_fact() {
    let session = current_session();
    let queued = accepted_origin(1);
    let delegated = turn_id(99);
    let fact = DelegatedTurnSchedulingFact::new(
        delegated,
        SessionConfigurationDefaultsVersion::first(),
        direct(1),
        DelegatedTurnSchedulingState::Active,
    );
    let input = queued_input(&session, queued).with_delegated_turn_facts(vec![fact, fact]);

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch { turn: delegated }
    );
}

/// a delegated model-identity entry must match
/// the exact configuration frozen by its stored turn origin.
#[test]
fn delegated_model_identity_requires_stored_configuration() {
    let session = current_session();
    let queued = accepted_origin(1);
    let delegated = turn_id(99);
    let identity_entry = semantic_entry(99);
    let mut input = queued_input(&session, queued).with_delegated_turn_facts(vec![
        DelegatedTurnSchedulingFact::new(
            delegated,
            SessionConfigurationDefaultsVersion::first(),
            direct(1),
            DelegatedTurnSchedulingState::Active,
        ),
    ]);
    input
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            identity_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                turn: delegated,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                selected: direct(2),
            },
        ));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
            entry: identity_entry.id(),
        }
    );
}

/// a delegated terminal semantic entry must match the
/// independently stored delegated lifecycle state.
#[test]
fn delegated_terminal_entry_requires_stored_lifecycle() {
    let session = current_session();
    let queued = accepted_origin(1);
    let delegated = turn_id(99);
    let failure_entry = semantic_entry(99);
    let mut input = queued_input(&session, queued).with_delegated_turn_facts(vec![
        DelegatedTurnSchedulingFact::new(
            delegated,
            SessionConfigurationDefaultsVersion::first(),
            direct(1),
            DelegatedTurnSchedulingState::Active,
        ),
    ]);
    input
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            failure_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn: delegated },
        ));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
            entry: failure_entry.id(),
        }
    );
}

/// immutable queue facts that cannot form one
/// durable total order fail closed with the exact derivation error.
#[test]
fn reconstitution_rejects_underivable_queue_order() {
    let session = current_session();
    let first = accepted_origin(1);
    let second = accepted_origin(2);
    let mut input = queued_input(&session, first);
    input
        .turns
        .push(second.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    input.turns[1].order = AcceptedInputQueueOrder::ordinary(first.position());

    let failure = assert_input_rejects_unchanged(input);

    // Turn identities descend as acceptance ordinals ascend, so the
    // second fixture holds the lower canonical turn identity.
    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::InvalidQueueOrder {
            error: AcceptedInputQueueOrderError::DuplicateAcceptancePosition {
                position: first.position(),
                first_turn: second.turn(),
                second_turn: first.turn(),
            },
        }
    );
}

/// a stored semantic entry must name the scheduling
/// session as its source session.
#[test]
fn reconstitution_rejects_cross_session_semantic_entry() {
    let session = current_session();
    let active = accepted_origin(1);
    let other_session = session_id(2);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.semantic_entries[0] = SemanticTranscriptEntryReconstitutionInput::new(
        origin_entry.id(),
        other_session,
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: active.accepted_input(),
        },
    );

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySourceSessionMismatch {
            entry: origin_entry.id(),
        }
    );
}

/// the same source-qualified semantic entry cannot appear
/// twice in the complete entry collection.
#[test]
fn reconstitution_rejects_duplicate_semantic_entry() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts
        .semantic_entries
        .push(active.entry(&session, origin_entry));

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntry {
            entry: origin_entry.reference(&session),
        }
    );
}

/// a failed marker naming a turn absent from the complete
/// scheduling inventory fails closed.
#[test]
fn reconstitution_rejects_semantic_entry_without_subject() {
    let session = current_session();
    let queued = accepted_origin(1);
    let unknown_turn = turn_id(99);
    let stray_entry = semantic_entry(31);
    let mut input = queued_input(&session, queued);
    input
        .semantic_entries
        .push(SemanticTranscriptEntryReconstitutionInput::new(
            stray_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn: unknown_turn },
        ));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
            entry: stray_entry.id(),
        }
    );
}

/// an origin entry for a turn whose stored lifecycle is
/// still queued contradicts that turn's state and fails closed.
#[test]
fn reconstitution_rejects_origin_entry_for_queued_turn() {
    let session = current_session();
    let queued = accepted_origin(1);
    let origin_entry = semantic_entry(30);
    let mut input = queued_input(&session, queued);
    input
        .semantic_entries
        .push(queued.entry(&session, origin_entry));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
            entry: origin_entry.id(),
        }
    );
}

/// one started turn owns exactly one origin entry; a
/// second origin entry naming the same accepted input fails closed.
#[test]
fn reconstitution_rejects_second_origin_entry_for_one_turn() {
    let session = current_session();
    let active = accepted_origin(1);
    let second_origin_entry = semantic_entry(31);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts
        .semantic_entries
        .push(active.entry(&session, second_origin_entry));

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
            entry: second_origin_entry.id(),
        }
    );
}

/// a started turn requires its exact origin entry; an
/// absent origin fails closed instead of deriving a start without one.
#[test]
fn reconstitution_rejects_started_turn_without_origin_entry() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    // The starting snapshot must stop referencing the removed entry, or
    // the snapshot-reference check would mask the origin-entry check.
    facts.semantic_entries.clear();
    facts.snapshots =
        vec![ActiveReconstitutionFacts::matching_starting_frontier().snapshot(&session, &[])];

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry {
            turn: active.turn(),
        }
    );
}

/// a failed turn requires its exact failed
/// marker; an absent marker fails closed instead of accepting the
/// stored terminal frontier on faith.
#[test]
fn reconstitution_rejects_failed_turn_without_failure_marker() {
    let session = current_session();
    let failed = accepted_origin(1);
    let origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    // The terminal snapshot must stop referencing the removed marker, or
    // the snapshot-reference check would mask the failed-marker check.
    facts.semantic_entries = vec![failed.entry(&session, origin_entry)];
    facts.snapshots = vec![
        FailedTerminalReconstitutionFacts::matching_starting_frontier()
            .snapshot(&session, &[origin_entry]),
        FailedTerminalReconstitutionFacts::matching_terminal_frontier()
            .snapshot(&session, &[origin_entry]),
    ];

    let failure = assert_input_rejects_unchanged(facts.input());

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::MissingFailureEntry {
            turn: failed.turn(),
        }
    );
}

/// a supplied acceptance tail requires an active turn; a
/// tail alongside a queued-only projection fails closed.
#[test]
fn reconstitution_rejects_tail_without_active_turn() {
    let session = current_session();
    let queued = accepted_origin(1);
    let mut input = queued_input(&session, queued);
    input.active_acceptance_tail = Some(queued.active_tail(&session));

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::UnexpectedActiveAcceptanceTail
    );
}

/// every tail entry belongs to the
/// scheduling session and appears exactly once; a cross-session entry or
/// a repeated accepted-input identity fails closed.
#[test]
fn active_reconstitution_rejects_cross_session_or_repeated_tail_entries() {
    let session = current_session();
    let active = accepted_origin(1);
    let second = accepted_origin(2);

    let other_session = session_id(2);
    let mut cross_session_facts = ActiveReconstitutionFacts::matching(&session, active);
    cross_session_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail")
        .entries[0]
        .session = other_session;
    let cross_session = assert_reconstitution_rejects_unchanged(cross_session_facts);
    assert_eq!(
        cross_session,
        AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailEntrySessionMismatch {
            accepted_input: active.accepted_input(),
        }
    );

    let mut repeated_facts = ActiveReconstitutionFacts::matching(&session, active);
    let repeated_tail = repeated_facts
        .acceptance_tail
        .as_mut()
        .expect("matching facts include the acceptance tail");
    repeated_tail.observed_last_position = second.position();
    repeated_tail
        .entries
        .push(SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                active.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: crate::SteeringBinding::new(active.turn()),
                },
            ),
            second.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: active.turn(),
            },
        ));
    let repeated = assert_reconstitution_rejects_unchanged(repeated_facts);
    assert_eq!(
        repeated,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptanceTailEntry {
            accepted_input: active.accepted_input(),
        }
    );

    expect![[r#"
            ┌────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact          │ failure                                                                                                      │
            ├────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ tail entry session cross-wired │ AcceptanceTailEntrySessionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe) } │
            │ tail entry identity repeated   │ DuplicateAcceptanceTailEntry { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe) }       │
            └────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail entry session cross-wired",
                failure: format!("{cross_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail entry identity repeated",
                failure: format!("{repeated:?}"),
            },
        ]));
}

/// every stored snapshot is owned by the
/// scheduling session, unique, duplicate-free, and backed by supplied
/// entries; each malformed snapshot collection fails closed.
#[test]
fn reconstitution_rejects_malformed_snapshot_collection() {
    let session = current_session();
    let active = accepted_origin(1);
    let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
    let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();

    let other_session = session_id(2);
    let mut cross_session_facts = ActiveReconstitutionFacts::matching(&session, active);
    cross_session_facts.snapshots[0] = ResolvedContextFrontierReconstitutionInput::new(
        other_session,
        starting_frontier.id(),
        vec![origin_entry.reference(&session)],
    );
    let cross_session = assert_reconstitution_rejects_unchanged(cross_session_facts);
    assert_eq!(
        cross_session,
        AcceptedInputSchedulingReconstitutionFailure::SnapshotOwningSessionMismatch {
            snapshot: starting_frontier.id(),
        }
    );

    let mut duplicate_facts = ActiveReconstitutionFacts::matching(&session, active);
    duplicate_facts
        .snapshots
        .push(starting_frontier.snapshot(&session, &[origin_entry]));
    let duplicate = assert_reconstitution_rejects_unchanged(duplicate_facts);
    assert_eq!(
        duplicate,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateSnapshot {
            snapshot: starting_frontier.id(),
        }
    );

    let mut membership_facts = ActiveReconstitutionFacts::matching(&session, active);
    membership_facts.snapshots[0] =
        starting_frontier.snapshot(&session, &[origin_entry, origin_entry]);
    let membership = assert_reconstitution_rejects_unchanged(membership_facts);
    assert_eq!(
        membership,
        AcceptedInputSchedulingReconstitutionFailure::InvalidSnapshotMembership {
            snapshot: starting_frontier.id(),
        }
    );

    let absent_entry = semantic_entry(99);
    let mut unbacked_facts = ActiveReconstitutionFacts::matching(&session, active);
    unbacked_facts.snapshots[0] = starting_frontier.snapshot(&session, &[absent_entry]);
    let unbacked = assert_reconstitution_rejects_unchanged(unbacked_facts);
    assert_eq!(
        unbacked,
        AcceptedInputSchedulingReconstitutionFailure::SnapshotEntryMissing {
            snapshot: starting_frontier.id(),
            entry: absent_entry.reference(&session),
        }
    );

    expect![[r#"
            ┌────────────────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact              │ failure                                                                                                                                                                                                                                                                   │
            ├────────────────────────────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ snapshot owner cross-wired         │ SnapshotOwningSessionMismatch { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                       │
            │ snapshot identity repeated         │ DuplicateSnapshot { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                                   │
            │ snapshot membership entry repeated │ InvalidSnapshotMembership { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                           │
            │ snapshot entry unsupplied          │ SnapshotEntryMissing { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028), entry: SemanticTranscriptEntryRef { source_session: SessionId(00000000-0000-0000-0000-000000000001), entry: SemanticTranscriptEntryId(00000000-0000-0000-0000-000000000063) } } │
            └────────────────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot owner cross-wired",
                failure: format!("{cross_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot identity repeated",
                failure: format!("{duplicate:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot membership entry repeated",
                failure: format!("{membership:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot entry unsupplied",
                failure: format!("{unbacked:?}"),
            },
        ]));
}

/// a stored start or failed terminal must
/// name a snapshot present in the complete supplied set; an absent
/// snapshot fails closed. Together with the frontier-exactness
/// rejections, this validated precondition backs eligibility's
/// failed-terminal-prefix expectation when preparing a successor.
#[test]
fn reconstitution_rejects_absent_starting_or_terminal_snapshot() {
    let session = current_session();
    let absent_frontier = frontier(99);

    let active = accepted_origin(1);
    let mut starting_facts = ActiveReconstitutionFacts::matching(&session, active);
    starting_facts.replace_starting_frontier(absent_frontier.id());
    let starting = assert_reconstitution_rejects_unchanged(starting_facts);
    assert_eq!(
        starting,
        AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing {
            turn: active.turn(),
        }
    );

    let failed = accepted_origin(1);
    let mut terminal_facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    terminal_facts.replace_terminal_frontier(absent_frontier.id());
    let terminal = assert_input_rejects_unchanged(terminal_facts.input());
    assert_eq!(
        terminal,
        AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing {
            turn: failed.turn(),
        }
    );

    expect![[r#"
            ┌─────────────────────────────────┬────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact           │ failure                                                                        │
            ├─────────────────────────────────┼────────────────────────────────────────────────────────────────────────────────┤
            │ stored starting snapshot absent │ StartingSnapshotMissing { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            │ stored terminal snapshot absent │ TerminalSnapshotMissing { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            └─────────────────────────────────┴────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored starting snapshot absent",
                failure: format!("{starting:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored terminal snapshot absent",
                failure: format!("{terminal:?}"),
            },
        ]));
}

/// a supplied snapshot that no stored lifecycle
/// fact references cannot ride along; the complete collection fails
/// closed. This is the read-side rejection recorded for orphan committed
/// snapshot headers.
#[test]
fn reconstitution_rejects_unreferenced_snapshot() {
    let session = current_session();
    let active = accepted_origin(1);
    let stray_frontier = frontier(90);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.snapshots.push(stray_frontier.snapshot(
        &session,
        &[ActiveReconstitutionFacts::matching_origin_entry()],
    ));

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot {
            snapshot: stray_frontier.id(),
        }
    );
}

/// durable total order admits only a failed-terminal
/// prefix, at most one active slot, and a queued suffix; every
/// out-of-order stored lifecycle fails closed on the first offending
/// turn.
#[test]
fn reconstitution_rejects_out_of_order_lifecycle_states() {
    let session = current_session();
    let earlier = accepted_origin(1);
    let later = accepted_origin(2);

    let mut active_after_queued_facts = ActiveReconstitutionFacts::matching(&session, later);
    active_after_queued_facts
        .turns
        .push(earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let active_after_queued = assert_reconstitution_rejects_unchanged(active_after_queued_facts);
    assert_eq!(
        active_after_queued,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    let mut terminal_after_queued_facts =
        FailedTerminalReconstitutionFacts::matching(&session, later);
    terminal_after_queued_facts
        .turns
        .push(earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let terminal_after_queued = assert_input_rejects_unchanged(terminal_after_queued_facts.input());
    assert_eq!(
        terminal_after_queued,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    // The ordering check rejects a second active slot before any
    // duplicate current-attempt bookkeeping, which is why the stored
    // attempt identity may repeat here: DuplicateCurrentAttempt is
    // unreachable behind this rejection.
    let mut second_active_facts = ActiveReconstitutionFacts::matching(&session, earlier);
    second_active_facts.turns.push(later.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::Active {
            starting_lineage: AcceptedInputStartingLineage::FirstInSession,
            starting_frontier: ActiveReconstitutionFacts::matching_starting_frontier().id(),
            phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                later.turn(),
                matching_active_attempt(),
            ),
        },
    ));
    let second_active = assert_reconstitution_rejects_unchanged(second_active_facts);
    assert_eq!(
        second_active,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    // The ordering check rejects the record before consulting its
    // frontier facts, so the claimed snapshots need not be supplied.
    let mut terminal_after_active_facts = ActiveReconstitutionFacts::matching(&session, earlier);
    terminal_after_active_facts.turns.push(later.record(
        &session,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            starting_lineage: AcceptedInputStartingLineage::After {
                immediate_predecessor: earlier.turn(),
            },
            starting_frontier: frontier(98).id(),
            terminal_execution: None,
            terminal_frontier: frontier(99).id(),
        },
    ));
    let terminal_after_active =
        assert_reconstitution_rejects_unchanged(terminal_after_active_facts);
    assert_eq!(
        terminal_after_active,
        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn: later.turn() }
    );

    expect![[r#"
            ┌───────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact                 │ failure                                                                      │
            ├───────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────┤
            │ active slot after queued work         │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ failed terminal after queued work     │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ second active slot                    │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ failed terminal after the active slot │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            └───────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&print(&[
            ReconstitutionFailureRow {
                perturbed_stored_fact: "active slot after queued work",
                failure: format!("{active_after_queued:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "failed terminal after queued work",
                failure: format!("{terminal_after_queued:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "second active slot",
                failure: format!("{second_active:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "failed terminal after the active slot",
                failure: format!("{terminal_after_active:?}"),
            },
        ]));
}

/// the stored starting lineage must equal the
/// lineage derived from durable total order; a first-in-session active
/// turn cannot claim a predecessor.
#[test]
fn reconstitution_rejects_stored_lineage_disagreeing_with_order() {
    let session = current_session();
    let active = accepted_origin(1);
    let claimed_lineage = AcceptedInputStartingLineage::After {
        immediate_predecessor: turn_id(99),
    };
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.replace_starting_lineage(claimed_lineage);

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingLineageMismatch {
            turn: active.turn(),
            expected: AcceptedInputStartingLineage::FirstInSession,
            actual: claimed_lineage,
        }
    );
}

/// attachment origins hidden by completed context
/// compaction do not contribute to the rendered frontier bound.
#[test]
fn rendered_frontier_origins_exclude_compacted_input() {
    let session = current_session();
    let hidden_input = accepted_input_id(1);
    let hidden_origin = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(1),
        session.id(),
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: hidden_input,
        },
    );
    let terminal = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(2),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCompleted { turn: turn_id(1) },
    );
    let summary = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(3),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ContextSummary {
            producing_call: model_call_id(4),
            summarized: crate::ContextCompactionRange::inclusive(
                hidden_origin.reference(),
                terminal.reference(),
            ),
            value: AssistantText::try_new(String::from("summary"))
                .expect("fixture summary is nonempty"),
        },
    );
    let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(5),
        vec![
            hidden_origin.reference(),
            terminal.reference(),
            summary.reference(),
        ],
    )
    .expect("the complete frontier retains compacted entries");
    let semantic_entries = BTreeMap::from([
        (hidden_origin.reference(), hidden_origin),
        (terminal.reference(), terminal),
        (summary.reference(), summary),
    ]);

    assert_eq!(
        AcceptedInputSchedulingProjection::rendered_frontier_origins(
            Some(&snapshot),
            &semantic_entries,
        ),
        Some(Vec::new())
    );
}

/// the stored starting snapshot must be exactly
/// the predecessor prefix plus the turn's origin entry; a snapshot
/// omitting the origin fails closed.
#[test]
fn reconstitution_rejects_starting_snapshot_omitting_origin() {
    let session = current_session();
    let active = accepted_origin(1);
    let mut facts = ActiveReconstitutionFacts::matching(&session, active);
    facts.snapshots =
        vec![ActiveReconstitutionFacts::matching_starting_frontier().snapshot(&session, &[])];

    let failure = assert_reconstitution_rejects_unchanged(facts);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active.turn(),
        }
    );
}

/// after a completed compaction, the exact compacted
/// result followed by the next turn's origin is a valid starting
/// frontier even though the predecessor frontier remains complete.
#[test]
fn reconstitution_accepts_exact_compaction_result_then_origin() {
    let session = current_session();
    let predecessor_turn = turn_id(1);
    let active_turn = turn_id(2);
    let predecessor_entry = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(1),
        session.id(),
        InitialSemanticTranscriptEntryPayload::TurnCompleted {
            turn: predecessor_turn,
        },
    );
    let origin_entry = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(3),
        session.id(),
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: accepted_input_id(2),
        },
    );
    let range = crate::ContextCompactionRange::inclusive(
        predecessor_entry.reference(),
        predecessor_entry.reference(),
    );
    let compaction_call = model_call_id(4);
    let summary_entry = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(5),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ContextSummary {
            producing_call: compaction_call,
            summarized: range,
            value: AssistantText::try_new(String::from("summary"))
                .expect("fixture summary is nonempty"),
        },
    );
    let predecessor_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(6),
        vec![predecessor_entry.reference()],
    )
    .expect("the predecessor fixture is a unique complete frontier");
    let compacted_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(7),
        vec![predecessor_entry.reference(), summary_entry.reference()],
    )
    .expect("the compaction result appends the summary");
    let starting_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(8),
        vec![
            predecessor_entry.reference(),
            summary_entry.reference(),
            origin_entry.reference(),
        ],
    )
    .expect("the next start appends its origin to the compaction result");
    let call = crate::ContextCompactionModelCallReconstitutionInput::new(
        compaction_call,
        session.id(),
        direct(9),
        ResolvedProviderTarget::naming(provider_model_identity(10)),
        predecessor_snapshot.frontier().snapshot(),
        crate::ContextCompactionModelCallState::Terminal(crate::ModelCallDisposition::Completed),
        crate::ContextCompactionTokenUsage::unreported(),
    )
    .reconstitute(&predecessor_snapshot)
    .expect("the dedicated call exactly names the predecessor frontier");
    let compaction = crate::ContextCompactionReconstitutionInput::new(
        crate::ContextCompactionId::from_uuid(uuid::Uuid::from_u128(11)),
        session.id(),
        None,
        predecessor_snapshot.frontier().snapshot(),
        compacted_snapshot.frontier().snapshot(),
        compaction_call,
        range,
        summary_entry.identity(),
    )
    .reconstitute(
        &predecessor_snapshot,
        &compacted_snapshot,
        std::slice::from_ref(&predecessor_entry),
        &[predecessor_entry.clone(), summary_entry.clone()],
        &summary_entry,
        &call,
    )
    .expect("the exact compaction facts reconstruct");
    let mut compactions = BTreeMap::from([(compaction.id(), compaction)]);
    let mut snapshots = BTreeMap::from([
        (
            predecessor_snapshot.frontier().snapshot(),
            predecessor_snapshot.clone(),
        ),
        (
            compacted_snapshot.frontier().snapshot(),
            compacted_snapshot.clone(),
        ),
        (
            starting_snapshot.frontier().snapshot(),
            starting_snapshot.clone(),
        ),
    ]);
    let origins = BTreeMap::from([(active_turn, origin_entry.reference())]);
    let mut referenced_snapshots = BTreeSet::new();
    let compaction_chain = compactions.values().collect::<Vec<_>>();

    let start = validate_start(
        1,
        active_turn,
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor_turn,
        },
        starting_snapshot.frontier().snapshot(),
        None,
        Some(&(predecessor_turn, predecessor_snapshot.clone())),
        &origins,
        None,
        &compaction_chain,
        &snapshots,
        &mut referenced_snapshots,
    )
    .expect("the validated compacted frontier remains an exact start");

    assert_eq!(start.frontier(), starting_snapshot.frontier());

    let successor_range = crate::ContextCompactionRange::inclusive(
        summary_entry.reference(),
        origin_entry.reference(),
    );
    let successor_call = model_call_id(13);
    let successor_summary = SemanticTranscriptEntry::from_validated_parts(
        semantic_transcript_entry_id(14),
        session.id(),
        InitialSemanticTranscriptEntryPayload::ContextSummary {
            producing_call: successor_call,
            summarized: successor_range,
            value: AssistantText::try_new(String::from("newer summary"))
                .expect("fixture summary is nonempty"),
        },
    );
    let successor_result = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(15),
        vec![
            predecessor_entry.reference(),
            summary_entry.reference(),
            origin_entry.reference(),
            successor_summary.reference(),
        ],
    )
    .expect("the successor compaction appends its summary");
    let successor_call_record = crate::ContextCompactionModelCallReconstitutionInput::new(
        successor_call,
        session.id(),
        direct(9),
        ResolvedProviderTarget::naming(provider_model_identity(10)),
        starting_snapshot.frontier().snapshot(),
        crate::ContextCompactionModelCallState::Terminal(crate::ModelCallDisposition::Completed),
        crate::ContextCompactionTokenUsage::unreported(),
    )
    .reconstitute(&starting_snapshot)
    .expect("the successor call names its exact source");
    let predecessor_compaction = compactions
        .values()
        .next()
        .expect("the first compaction is present");
    let successor_compaction = crate::ContextCompactionReconstitutionInput::new(
        crate::ContextCompactionId::from_uuid(uuid::Uuid::from_u128(16)),
        session.id(),
        Some(predecessor_compaction.id()),
        starting_snapshot.frontier().snapshot(),
        successor_result.frontier().snapshot(),
        successor_call,
        successor_range,
        successor_summary.identity(),
    )
    .reconstitute(
        &starting_snapshot,
        &successor_result,
        &[
            predecessor_entry.clone(),
            summary_entry.clone(),
            origin_entry.clone(),
        ],
        &[
            predecessor_entry.clone(),
            summary_entry.clone(),
            origin_entry.clone(),
            successor_summary.clone(),
        ],
        &successor_summary,
        &successor_call_record,
    )
    .expect("the successor compaction reconstructs");
    compactions.insert(successor_compaction.id(), successor_compaction);
    snapshots.insert(successor_result.frontier().snapshot(), successor_result);
    let compaction_chain = [
        compactions
            .values()
            .find(|compaction| compaction.predecessor().is_none())
            .expect("the root compaction is present"),
        compactions
            .values()
            .find(|compaction| compaction.predecessor().is_some())
            .expect("the successor compaction is present"),
    ];
    let mut historical_references = BTreeSet::new();

    let historical_start = validate_start(
        1,
        active_turn,
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor_turn,
        },
        starting_snapshot.frontier().snapshot(),
        None,
        Some(&(predecessor_turn, predecessor_snapshot.clone())),
        &origins,
        None,
        &compaction_chain,
        &snapshots,
        &mut historical_references,
    )
    .expect("the intervening historical start retains the earlier summary");

    assert_eq!(historical_start.frontier(), starting_snapshot.frontier());

    let stale_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
        session.id(),
        context_frontier_id(12),
        vec![predecessor_entry.reference(), origin_entry.reference()],
    )
    .expect("the stale fixture omits only the required summary append");
    snapshots.insert(stale_snapshot.frontier().snapshot(), stale_snapshot.clone());
    let mut stale_references = BTreeSet::new();

    let stale_failure = validate_start(
        1,
        active_turn,
        AcceptedInputStartingLineage::After {
            immediate_predecessor: predecessor_turn,
        },
        stale_snapshot.frontier().snapshot(),
        None,
        Some(&(predecessor_turn, predecessor_snapshot)),
        &origins,
        None,
        &compaction_chain,
        &snapshots,
        &mut stale_references,
    )
    .expect_err("a post-compaction start cannot omit the summary result");

    assert_eq!(
        stale_failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active_turn,
        }
    );
}

/// each start owns a distinct snapshot; a
/// successor start naming its predecessor's already-referenced starting
/// snapshot fails closed. With the content-exactness rejection, this
/// backs eligibility's expectation that fresh snapshot identities
/// preserve the validated prefix.
#[test]
fn reconstitution_rejects_starting_frontier_reused_from_predecessor() {
    let session = current_session();
    let predecessor = accepted_origin(1);
    let active = accepted_origin(2);
    let active_origin_entry = semantic_entry(32);
    let active_delivery = DeliveryRequest::AfterCurrentTurn {
        expected_active_turn: predecessor.turn(),
        configuration: PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, predecessor);
    facts.turns.push(active.record_with(
        &session,
        OriginRecordFacts {
            order: active.ordinary_order(),
            delivery: active_delivery,
            state: AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::After {
                    immediate_predecessor: predecessor.turn(),
                },
                // The perturbation: the successor claims its
                // predecessor's starting snapshot instead of a distinct
                // successor prefix snapshot.
                starting_frontier:
                    FailedTerminalReconstitutionFacts::matching_starting_frontier().id(),
                phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                    active.turn(),
                    matching_active_attempt(),
                ),
            },
        },
    ));
    facts
        .semantic_entries
        .push(active.entry(&session, active_origin_entry));
    facts.acceptance_tail = Some(SessionAcceptanceTailReconstitutionInput::new(
        session.id(),
        active.accepted_input(),
        active.position(),
        vec![SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                active.accepted_input(),
                AcceptedInputDisposition::OriginOf(active.turn()),
            ),
            active.position(),
            active_delivery,
        )],
    ));

    let failure = assert_input_rejects_unchanged(facts.input());

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
            turn: active.turn(),
        }
    );
}

/// an all-terminal projection holds no queued work;
/// eligibility rejects instead of manufacturing a candidate.
#[test]
fn eligibility_rejects_projection_without_queued_work() {
    let session = current_session();
    let failed = accepted_origin(1);
    let activation = activation(1);
    let projection = FailedTerminalReconstitutionFacts::matching(&session, failed)
        .input()
        .reconstitute()
        .expect("the complete failed-terminal record is valid");

    let failure = assert_eligibility_rejects_unchanged(projection, activation.identities());

    assert_eq!(failure, AcceptedInputEligibilityFailure::NoQueuedTurn);
}

/// a proposed origin-entry identity colliding with
/// a committed semantic entry fails closed before any candidate is
/// prepared.
#[test]
fn eligibility_rejects_committed_origin_entry_identity() {
    let session = current_session();
    let failed = accepted_origin(1);
    let queued = accepted_origin(2);
    let activation = activation(1);
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts
        .turns
        .push(queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let projection = facts
        .input()
        .reconstitute()
        .expect("a failed-terminal prefix with one queued successor is valid");
    let committed_origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();

    let failure = assert_eligibility_rejects_unchanged(
        projection,
        activation.identities_with_origin_entry(committed_origin_entry.id()),
    );

    assert_eq!(
        failure,
        AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists
    );
}

/// a proposed starting-snapshot identity
/// colliding with a committed session-scoped snapshot fails closed
/// before any candidate is prepared.
#[test]
fn eligibility_rejects_committed_starting_frontier_identity() {
    let session = current_session();
    let failed = accepted_origin(1);
    let queued = accepted_origin(2);
    let activation = activation(1);
    let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
    facts
        .turns
        .push(queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
    let projection = facts
        .input()
        .reconstitute()
        .expect("a failed-terminal prefix with one queued successor is valid");
    let committed_frontier = FailedTerminalReconstitutionFacts::matching_terminal_frontier();

    let failure = assert_eligibility_rejects_unchanged(
        projection,
        activation.identities_with_starting_frontier(committed_frontier.id()),
    );

    assert_eq!(
        failure,
        AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists
    );
}

/// a prepared standalone compaction call survives complete
/// reconstitution and prevents queued-turn activation until recovery.
#[test]
fn prepared_compaction_call_blocks_activation_after_reconstitution() {
    let session = current_session();
    let source = ResolvedContextFrontierReconstitutionInput::new(
        session.id(),
        context_frontier_id(701),
        Vec::new(),
    );
    let call = model_call_id(702);
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        Vec::new(),
        Vec::new(),
        vec![source],
        None,
    )
    .with_context_compaction_facts(
        vec![crate::ContextCompactionModelCallReconstitutionInput::new(
            call,
            session.id(),
            direct(703),
            ResolvedProviderTarget::naming(provider_model_identity(704)),
            context_frontier_id(701),
            crate::ContextCompactionModelCallState::Prepared,
            crate::ContextCompactionTokenUsage::unreported(),
        )],
        Vec::new(),
    );
    let projection = input
        .reconstitute()
        .expect("prepared compaction evidence remains recoverable");
    let error = projection
        .prepare_earliest_queued_activation(activation(705).identities())
        .expect_err("unfinished compaction owns the execution slot");

    assert_eq!(
        error.failure(),
        AcceptedInputEligibilityFailure::ContextCompactionInProgress { call }
    );
}

/// an authorized standalone compaction call remains
/// recoverable and owns the execution slot after restart reconstitution.
#[test]
fn in_flight_compaction_call_blocks_activation_after_reconstitution() {
    let session = current_session();
    let source = ResolvedContextFrontierReconstitutionInput::new(
        session.id(),
        context_frontier_id(706),
        Vec::new(),
    );
    let call = model_call_id(707);
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session.clone(),
        Vec::new(),
        Vec::new(),
        vec![source],
        None,
    )
    .with_context_compaction_facts(
        vec![crate::ContextCompactionModelCallReconstitutionInput::new(
            call,
            session.id(),
            direct(708),
            ResolvedProviderTarget::naming(provider_model_identity(709)),
            context_frontier_id(706),
            crate::ContextCompactionModelCallState::InFlight,
            crate::ContextCompactionTokenUsage::unreported(),
        )],
        Vec::new(),
    );
    let projection = input
        .reconstitute()
        .expect("in-flight compaction evidence remains recoverable");
    let error = projection
        .prepare_earliest_queued_activation(activation(710).identities())
        .expect_err("authorized compaction owns the execution slot");

    assert_eq!(
        error.failure(),
        AcceptedInputEligibilityFailure::ContextCompactionInProgress { call }
    );
}

/// a terminal non-completed dedicated call is retained as
/// historical recovery evidence without requiring a compaction result.
#[test]
fn known_failed_compaction_call_is_legal_standalone_evidence() {
    let session = current_session();
    let source = ResolvedContextFrontierReconstitutionInput::new(
        session.id(),
        context_frontier_id(711),
        Vec::new(),
    );
    let call = crate::ContextCompactionModelCallReconstitutionInput::new(
        model_call_id(712),
        session.id(),
        direct(713),
        ResolvedProviderTarget::naming(provider_model_identity(714)),
        context_frontier_id(711),
        crate::ContextCompactionModelCallState::Terminal(ModelCallDisposition::KnownFailed),
        crate::ContextCompactionTokenUsage::unreported(),
    );
    let input = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        Vec::new(),
        Vec::new(),
        vec![source],
        None,
    )
    .with_context_compaction_facts(vec![call], Vec::new());

    let projection = input
        .reconstitute()
        .expect("known-failed compaction evidence is complete without a summary");
    let error = projection
        .prepare_earliest_queued_activation(activation(715).identities())
        .expect_err("the fixture contains no queued turn");

    assert_eq!(
        error.failure(),
        AcceptedInputEligibilityFailure::NoQueuedTurn
    );
}

/// ordinary and compaction call maps cannot claim the
/// same identity even when both purpose-specific records are valid alone.
#[test]
fn reconstitution_rejects_cross_kind_model_call_identity() {
    let session = current_session();
    let active = accepted_origin(1);
    let consumed = accepted_origin(2);
    let facts = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
    let call = facts.model_calls[0].id();
    let source = facts.model_calls[0].frontier();
    let collision = crate::ContextCompactionModelCallReconstitutionInput::new(
        call,
        session.id(),
        direct(1),
        ResolvedProviderTarget::naming(provider_model_identity(51)),
        source,
        crate::ContextCompactionModelCallState::Prepared,
        crate::ContextCompactionTokenUsage::unreported(),
    );
    let input = facts
        .input()
        .with_context_compaction_facts(vec![collision], Vec::new());

    let failure = assert_input_rejects_unchanged(input);

    assert_eq!(
        failure,
        AcceptedInputSchedulingReconstitutionFailure::DuplicateModelCallIdentityAcrossKinds {
            call,
        }
    );
}

/// checked relational runner-loss facts reconstitute the
/// exact closed active phase without a live turn attempt.
#[test]
fn runner_recovery_phase_reconstitutes_exact_loss_subject() {
    let owning_turn = turn_id(801);
    let runner = crate::RunnerId::from_uuid(uuid::Uuid::from_u128(802));
    let revision = crate::RunnerGeneration::try_from_u64(3)
        .expect("the fixture placement revision is positive");
    let interrupted_tool_attempt = Some(tool_attempt_id(803));
    let input = ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
        owning_turn,
        runner,
        revision,
        interrupted_tool_attempt,
        None,
    );

    assert_eq!(
        input.canonical_evidence_free_phase(),
        Some(ActiveTurnPhase::AwaitingRunnerRecovery {
            runner,
            placement_revision: revision,
            optional_tool_attempt: interrupted_tool_attempt,
        })
    );
}

/// an interrupt successor authenticated against an external
/// terminal predecessor remains ahead of older ordinary queued work.
#[test]
fn external_interrupt_chain_is_the_first_accepted_order_root() {
    let older_ordinary = turn_id(811);
    let external_successor = turn_id(812);
    let interrupt_descendant = turn_id(813);
    let later_ordinary = turn_id(814);
    let ordinary_roots = BTreeSet::from([older_ordinary, later_ordinary]);
    let queued_turns = BTreeSet::from([
        older_ordinary,
        external_successor,
        interrupt_descendant,
        later_ordinary,
    ]);

    let promoted = super::promote_external_interrupt_chains(
        vec![
            older_ordinary,
            external_successor,
            interrupt_descendant,
            later_ordinary,
        ],
        BTreeSet::from([external_successor]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(
        promoted,
        vec![
            external_successor,
            interrupt_descendant,
            older_ordinary,
            later_ordinary,
        ]
    );
}

/// later external chains retain their historical placement once
/// the oldest crossing chain is promoted ahead of queued work.
#[test]
fn multiple_external_interrupt_chains_are_retained_in_order() {
    let older_ordinary = turn_id(821);
    let first_external_successor = turn_id(822);
    let first_descendant = turn_id(823);
    let second_external_successor = turn_id(824);
    let second_descendant = turn_id(825);
    let later_ordinary = turn_id(826);
    let ordinary_roots = BTreeSet::from([older_ordinary, later_ordinary]);
    let queued_turns = BTreeSet::from([
        older_ordinary,
        first_external_successor,
        first_descendant,
        second_external_successor,
        second_descendant,
        later_ordinary,
    ]);

    let promoted = super::promote_external_interrupt_chains(
        vec![
            older_ordinary,
            first_external_successor,
            first_descendant,
            second_external_successor,
            second_descendant,
            later_ordinary,
        ],
        BTreeSet::from([first_external_successor, second_external_successor]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(
        promoted,
        vec![
            first_external_successor,
            first_descendant,
            older_ordinary,
            second_external_successor,
            second_descendant,
            later_ordinary,
        ]
    );
}

/// an external interrupt chain does not cross a completed
/// accepted-input terminal prefix.
#[test]
fn external_interrupt_chain_retains_terminal_prefix() {
    let terminal = turn_id(831);
    let external_successor = turn_id(832);
    let ordinary_roots = BTreeSet::from([terminal]);

    let promoted = super::promote_external_interrupt_chains(
        vec![terminal, external_successor],
        BTreeSet::from([external_successor]),
        &ordinary_roots,
        &BTreeSet::from([external_successor]),
    );

    assert_eq!(promoted, vec![terminal, external_successor]);
}

/// an external interrupt chain crosses queued ordinary work but
/// retains the completed accepted-input terminal prefix.
#[test]
fn external_interrupt_chain_precedes_only_queued_prefix() {
    let terminal = turn_id(841);
    let older_queued = turn_id(842);
    let external_successor = turn_id(843);
    let ordinary_roots = BTreeSet::from([terminal, older_queued]);
    let queued_turns = BTreeSet::from([older_queued, external_successor]);

    let promoted = super::promote_external_interrupt_chains(
        vec![terminal, older_queued, external_successor],
        BTreeSet::from([external_successor]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(promoted, vec![terminal, external_successor, older_queued]);
}

/// a historical external terminal does not hide the later
/// external interrupt chain that actually crosses queued ordinary work.
#[test]
fn later_external_interrupt_chain_crosses_queued_work() {
    let historical_external = turn_id(851);
    let older_queued = turn_id(852);
    let crossing_external = turn_id(853);
    let ordinary_roots = BTreeSet::from([older_queued]);
    let queued_turns = BTreeSet::from([older_queued, crossing_external]);

    let promoted = super::promote_external_interrupt_chains(
        vec![older_queued, historical_external, crossing_external],
        BTreeSet::from([historical_external, crossing_external]),
        &ordinary_roots,
        &queued_turns,
    );

    assert_eq!(
        promoted,
        vec![historical_external, crossing_external, older_queued]
    );
}
