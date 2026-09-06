//! Turn scheduling fixtures tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::*;

pub(super) fn current_session() -> Session {
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

pub(super) fn imported_session() -> ReconstitutedImportedSession {
    imported_session_for(1)
}

pub(super) fn imported_session_for(session_value: u128) -> ReconstitutedImportedSession {
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

pub(super) fn configuration(session: &Session) -> OriginConfiguration {
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

pub(super) fn default_origin_delivery() -> DeliveryRequest {
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
pub(super) struct OriginFixture {
    acceptance: u64,
}

pub(super) fn accepted_origin(acceptance: u64) -> OriginFixture {
    OriginFixture { acceptance }
}

impl OriginFixture {
    pub(super) fn turn(self) -> TurnId {
        turn_id(u128::from(u64::MAX - self.acceptance))
    }

    pub(super) fn accepted_input(self) -> AcceptedInputId {
        accepted_input_id(u128::from(u64::MAX / 2 - self.acceptance))
    }

    pub(super) fn position(self) -> SessionInputPosition {
        SessionInputPosition::try_from_u64(self.acceptance)
            .expect("test acceptance ordinals are positive")
    }

    pub(super) fn ordinary_order(self) -> AcceptedInputQueueOrder {
        AcceptedInputQueueOrder::ordinary(self.position())
    }

    pub(super) fn record(
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

    pub(super) fn record_with(
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

    pub(super) fn entry(
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

    pub(super) fn active_tail(self, session: &Session) -> SessionAcceptanceTailReconstitutionInput {
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

pub(super) struct OriginRecordFacts {
    pub(super) order: AcceptedInputQueueOrder,
    pub(super) delivery: DeliveryRequest,
    pub(super) state: AcceptedInputTurnSchedulingRecordState,
}

#[derive(Clone, Copy)]
pub(super) struct SemanticEntryFixture {
    seed: u128,
}

pub(super) fn semantic_entry(seed: u128) -> SemanticEntryFixture {
    SemanticEntryFixture { seed }
}

pub(super) fn user_denial(request: ToolRequestId) -> ToolApprovalResolution {
    ToolApprovalResolutionReconstitutionInput::user_fixture(
        request,
        ToolApprovalDecision::Deny { reason: None },
    )
    .reconstitute()
    .expect("the user denial fixture is valid")
}

impl SemanticEntryFixture {
    pub(super) fn id(self) -> SemanticTranscriptEntryId {
        semantic_transcript_entry_id(self.seed)
    }

    pub(super) fn reference(self, session: &Session) -> SemanticTranscriptEntryRef {
        SemanticTranscriptEntryRef::from_source(session.id(), self.id())
    }

    pub(super) fn failed_turn(
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
pub(super) struct FrontierFixture {
    seed: u128,
}

pub(super) fn frontier(seed: u128) -> FrontierFixture {
    FrontierFixture { seed }
}

impl FrontierFixture {
    pub(super) fn id(self) -> ContextFrontierId {
        context_frontier_id(self.seed)
    }

    pub(super) fn snapshot(
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
pub(super) struct ActivationFixture {
    seed: u128,
}

pub(super) fn activation(seed: u128) -> ActivationFixture {
    ActivationFixture { seed }
}

pub(super) fn matching_active_attempt() -> TurnAttemptId {
    turn_attempt_id(50)
}

impl ActivationFixture {
    pub(super) fn model_identity_entry(self) -> SemanticEntryFixture {
        semantic_entry(50 + self.seed)
    }

    pub(super) fn origin_entry(self) -> SemanticEntryFixture {
        semantic_entry(100 + self.seed)
    }

    fn starting_frontier(self) -> FrontierFixture {
        frontier(200 + self.seed)
    }

    pub(super) fn initial_attempt(self) -> TurnAttemptId {
        turn_attempt_id(300 + self.seed)
    }

    pub(super) fn identities(self) -> AcceptedInputTurnActivationIdentities {
        AcceptedInputTurnActivationIdentities::new(
            self.model_identity_entry().id(),
            self.origin_entry().id(),
            self.starting_frontier().id(),
            self.initial_attempt(),
        )
    }

    pub(super) fn identities_with_attempt(
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

    pub(super) fn identities_with_origin_entry(
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

    pub(super) fn identities_with_starting_frontier(
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
pub(super) struct ActiveReconstitutionFacts {
    pub(super) session: Session,
    pub(super) turns: Vec<AcceptedInputTurnSchedulingRecord>,
    pub(super) semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    pub(super) snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    pub(super) acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
}

impl ActiveReconstitutionFacts {
    /// The origin-entry fixture the matching baseline stores for its
    /// active turn.
    pub(super) fn matching_origin_entry() -> SemanticEntryFixture {
        semantic_entry(30)
    }

    /// The starting-snapshot fixture the matching baseline stores for
    /// its active turn.
    pub(super) fn matching_starting_frontier() -> FrontierFixture {
        frontier(40)
    }

    pub(super) fn matching(session: &Session, active: OriginFixture) -> Self {
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
    pub(super) fn replace_active_phase(
        &mut self,
        replacement: ActiveTurnSchedulingReconstitutionInput,
    ) {
        let AcceptedInputTurnSchedulingRecordState::Active { phase, .. } = &mut self.turns[0].state
        else {
            panic!("matching active facts retain an active scheduling record");
        };
        *phase = replacement;
    }

    /// Replaces only the stored starting lineage while retaining every
    /// other matching fact.
    pub(super) fn replace_starting_lineage(&mut self, replacement: AcceptedInputStartingLineage) {
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
    pub(super) fn replace_starting_frontier(&mut self, replacement: ContextFrontierId) {
        let AcceptedInputTurnSchedulingRecordState::Active {
            starting_frontier, ..
        } = &mut self.turns[0].state
        else {
            panic!("matching active facts retain an active scheduling record");
        };
        *starting_frontier = replacement;
    }

    pub(super) fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
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
pub(super) struct ConsumedSteeringReconstitutionFacts {
    session: Session,
    pub(super) turns: Vec<AcceptedInputTurnSchedulingRecord>,
    pub(super) semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    pub(super) snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    pub(super) acceptance_tail: SessionAcceptanceTailReconstitutionInput,
    pinned_targets: Vec<crate::PinnedProviderTargetReconstitutionInput>,
    pub(super) model_calls: Vec<ModelCallReconstitutionInput>,
    pub(super) consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    pub(super) steering_continuation_rounds: Vec<SteeringContinuationRoundReconstitutionInput>,
}

impl ConsumedSteeringReconstitutionFacts {
    pub(super) fn matching(
        session: &Session,
        active: OriginFixture,
        consumed: OriginFixture,
    ) -> Self {
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
    pub(super) fn matching_at_continuation(
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
    pub(super) fn matching_continuation_producing_call() -> crate::ModelCallId {
        model_call_id(90)
    }

    /// The steering-consuming continuation call the baseline stores.
    pub(super) fn matching_continuation_call() -> crate::ModelCallId {
        model_call_id(91)
    }

    /// The single proposed request the continuation baseline stores.
    pub(super) fn matching_continuation_request() -> ToolRequestId {
        tool_request_id(92)
    }

    /// The executed tool attempt the continuation baseline stores.
    pub(super) fn matching_continuation_tool_attempt() -> crate::ToolAttemptId {
        tool_attempt_id(93)
    }

    /// The ended tool attempt backing the baseline's result window.
    pub(super) fn matching_continuation_round_attempt(
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

    pub(super) fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
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
pub(super) fn ended_tool_attempt(
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

pub(super) fn ended_tool_attempt_with_end(
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

pub(super) fn active_input(
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
pub(super) fn queued_input(
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
pub(super) struct FailedTerminalReconstitutionFacts {
    session: Session,
    pub(super) turns: Vec<AcceptedInputTurnSchedulingRecord>,
    pub(super) semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    pub(super) snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    pub(super) acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
}

impl FailedTerminalReconstitutionFacts {
    /// The origin-entry fixture the matching baseline stores for its
    /// failed turn.
    pub(super) fn matching_origin_entry() -> SemanticEntryFixture {
        semantic_entry(30)
    }

    /// The failed-marker fixture the matching baseline stores for its
    /// failed turn.
    pub(super) fn matching_failure_entry() -> SemanticEntryFixture {
        semantic_entry(31)
    }

    /// The starting-snapshot fixture the matching baseline stores for
    /// its failed turn.
    pub(super) fn matching_starting_frontier() -> FrontierFixture {
        frontier(40)
    }

    /// The terminal-snapshot fixture the matching baseline stores for
    /// its failed turn.
    pub(super) fn matching_terminal_frontier() -> FrontierFixture {
        frontier(41)
    }

    pub(super) fn matching(session: &Session, failed: OriginFixture) -> Self {
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
    pub(super) fn replace_terminal_frontier(&mut self, replacement: ContextFrontierId) {
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
    pub(super) fn replace_terminal_execution(
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

    pub(super) fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
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
pub(super) struct PostAnchorOrigins {
    pub(super) active: OriginFixture,
    pub(super) queued: OriginFixture,
}

pub(super) fn active_input_with_post_anchor_origin(
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
pub(super) struct FailedPredecessorPostAnchorOrigins {
    pub(super) predecessor: OriginFixture,
    pub(super) active: OriginFixture,
    pub(super) queued: OriginFixture,
}

pub(super) fn active_input_after_failed_predecessor_with_post_anchor_origin(
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

pub(super) fn active_input_after_historical_interrupt(
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
pub(super) struct ReconstitutionFailureRow {
    pub(super) perturbed_stored_fact: &'static str,
    pub(super) failure: String,
}

/// Asserts one perturbed complete input rejects while retaining every
/// supplied fact unchanged, then returns its precise failure.
#[track_caller]
pub(super) fn assert_input_rejects_unchanged(
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
pub(super) fn assert_reconstitution_rejects_unchanged(
    facts: ActiveReconstitutionFacts,
) -> AcceptedInputSchedulingReconstitutionFailure {
    assert_input_rejects_unchanged(facts.input())
}

/// Asserts eligibility preparation rejects while retaining the complete
/// projection and supplied identities unchanged, then returns the exact
/// failure.
#[track_caller]
pub(super) fn assert_eligibility_rejects_unchanged(
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
