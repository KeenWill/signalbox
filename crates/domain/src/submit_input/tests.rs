//! Submit-input preparation and reconstitution tests for `docs/spec/turn-lifecycle-and-scheduling.md`.

use std::collections::{BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use super::{
    NonAcceptedTurnPredecessorReconstitutionInput, ReconstitutedSubmitInput, SubmitInput,
    SubmitInputAppliedPendingSteeringReconstitutionInput, SubmitInputAppliedResult,
    SubmitInputAppliedTurnOriginReconstitutionInput, SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputPreparationFailure, SubmitInputReclassifiedTurnOriginConstructionInput,
    SubmitInputReconstitutionFailure, SubmitInputReconstitutionInput,
    SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
    SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
    SubmitInputRejectedActiveTurnPresentReconstitutionInput,
    SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
    SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
    SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
    SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
    SubmitInputRejectedNoActiveTurnReconstitutionInput, SubmitInputRejectedResult,
    SubmitInputRejectedSessionNotFoundReconstitutionInput,
    SubmitInputRejectedUnknownModelAliasReconstitutionInput, SubmitInputResult,
    SubmitInputTerminalSourceConstructionInput, SubmitInputTerminalSourceReconstitutionInput,
    SubmitInputTurnOriginReconstitutionInput, command::freeze_origin_configuration,
    validation::StoredOriginConfigurationReconstitutionFacts,
    validation::reconstruct_origin_configuration,
};
use crate::applied_interrupt::test_applied_interrupt_proof;
use crate::test_support::{
    accepted_input_id, alias, command_id, direct, model_call_id, provider_target_evidence_id,
    session_id, turn_id,
};
use crate::test_support::{
    context_frontier_id, provider_model_identity, semantic_transcript_entry_id, tool_request_id,
    turn_attempt_id,
};
use crate::turn_attempt::test_fatal_mismatch_stop_causes;
use crate::turn_lifecycle::{
    test_applied_stop_for_reconciliation_proof, test_reconciliation_marker,
};
use crate::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueuePriority, AcceptedInputSchedulingProjection,
    AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
    AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState, ActiveTurnPhase,
    ActiveTurnSchedulingReconstitutionInput, Actor, AttachmentKind, BlobDigest,
    DangerousToolAutoApproval, DeclaredMediaType, DeliveryRequest, DescendantTerminationScope,
    FastModeOverlay, FastModeSupport, FrozenAliasDefinition, FrozenModelSelection,
    InitialSemanticTranscriptEntryPayload, IssuedOperationRef, ModelCallDisposition,
    ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelCapabilities,
    ModelCapabilityCatalog, ModelCapabilityDefinition, ModelSelectionOverride,
    ModelSelectionRequest, ModelSettingsOverlay, ModelSettingsPrecedence,
    NonEmptyIssuedOperationRefs, NormalizedToolArguments, OriginConfiguration,
    OriginModelSettingsError, PerInputConfigurationChoices,
    PinnedProviderTargetReconstitutionInput, ReasoningLevel, ReconciliationReason,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    ResolvedProviderTarget, SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef,
    Session, SessionAcceptanceTailEntryReconstitutionInput,
    SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionInputPosition, SessionReconstitutionInput, SettingOverlay, SteeringBinding,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolName, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, TranscriptAncestry, TurnDisposition, UserContent,
    UserContentPart,
};

fn version(value: u64) -> SessionConfigurationDefaultsVersion {
    SessionConfigurationDefaultsVersion::try_from_u64(value).expect("positive test version")
}

fn choices(expected: u64, model: ModelSelectionOverride) -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(version(expected), model)
}

fn defaults(selection: ModelSelectionRequest) -> SessionConfigurationDefaults {
    SessionConfigurationDefaults::new(selection)
}

fn session(id: u128, current: u64, selection: ModelSelectionRequest) -> Session {
    SessionReconstitutionInput::new(
        session_id(id),
        session_id(id),
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        session_id(id),
        version(current),
        session_id(id),
        version(current),
        defaults(selection),
        crate::SessionPlacementReconstitutionFacts {
            current_pointer_session: session_id(id),
            current_pointer_version: crate::SessionPlacementVersion::INITIAL,
            selected_event_session: session_id(id),
            selected_event: crate::VersionedSessionPlacement::initial(
                crate::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .expect("test session projection is complete")
}

fn content(value: &str) -> UserContent {
    UserContent::try_text(value.to_owned()).expect("test content is valid")
}

fn start_command(command: u128, text: &str, expected: u64) -> SubmitInput {
    SubmitInput::new(
        command_id(command),
        session_id(1),
        content(text),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(expected, ModelSelectionOverride::UseSessionDefault),
        },
    )
}

fn attachment_command(command: u128, digest: BlobDigest) -> SubmitInput {
    SubmitInput::new(
        command_id(command),
        session_id(1),
        UserContent::try_parts(vec![UserContentPart::Attachment {
            digest,
            kind: AttachmentKind::File,
            media_type: DeclaredMediaType::try_new("application/octet-stream".to_owned())
                .expect("test media type is valid"),
            display_filename: None,
        }])
        .expect("test attachment content is valid"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    )
}

fn start_command_with_settings(
    command: u128,
    text: &str,
    expected: u64,
    settings: ModelSettingsOverlay,
) -> SubmitInput {
    SubmitInput::new(
        command_id(command),
        session_id(1),
        content(text),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::with_model_settings(
                version(expected),
                ModelSelectionOverride::UseSessionDefault,
                settings,
            ),
        },
    )
}

fn after_command(command: u128, expected_active_turn: crate::TurnId) -> SubmitInput {
    SubmitInput::new(
        command_id(command),
        session_id(1),
        content("hello"),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    )
}

fn safe_point_command(command: u128, expected_active_turn: crate::TurnId) -> SubmitInput {
    SubmitInput::new(
        command_id(command),
        session_id(1),
        content("hello"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn,
        },
    )
}

fn interrupt_command(command: u128, expected_active_turn: crate::TurnId) -> SubmitInput {
    SubmitInput::new(
        command_id(command),
        session_id(1),
        content("hello"),
        DeliveryRequest::Interrupt {
            expected_active_turn,
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    )
}

fn origin_configuration(current: &Session) -> OriginConfiguration {
    let current_version = current.current_configuration_defaults().version();
    let checked = current
        .current_configuration_defaults()
        .derive_request(current_version, ModelSelectionOverride::UseSessionDefault)
        .expect("the test defaults version is current");
    OriginConfiguration::freeze(checked, |_| None)
        .expect("direct test selection does not require an alias")
}

fn active_turn(current: &Session) -> AcceptedInputSchedulingProjection {
    active_turn_at_position(current, SessionInputPosition::first())
}

fn active_turn_at_position(
    current: &Session,
    position: SessionInputPosition,
) -> AcceptedInputSchedulingProjection {
    active_turn_at_position_in_phase(
        current,
        position,
        ActiveTurnSchedulingReconstitutionInput::prepared(turn_id(7), turn_attempt_id(0x51)),
    )
}

fn active_turn_at_position_in_phase(
    current: &Session,
    position: SessionInputPosition,
    phase: ActiveTurnSchedulingReconstitutionInput,
) -> AcceptedInputSchedulingProjection {
    let origin_entry = semantic_transcript_entry_id(0x31);
    let accepted_input = AcceptedInputLifecycle::new(
        accepted_input_id(0x21),
        AcceptedInputDisposition::OriginOf(turn_id(7)),
    );
    AcceptedInputSchedulingReconstitutionInput::new(
        current.clone(),
        vec![AcceptedInputTurnSchedulingRecord::new(
            current.id(),
            turn_id(7),
            current.id(),
            accepted_input.clone(),
            current.id(),
            turn_id(7),
            AcceptedInputQueueOrder::ordinary(position),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(
                    current.current_configuration_defaults().version().as_u64(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            origin_configuration(current),
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: context_frontier_id(0x41),
                phase,
            },
        )],
        vec![SemanticTranscriptEntryReconstitutionInput::new(
            origin_entry,
            current.id(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id(0x21),
            },
        )],
        vec![ResolvedContextFrontierReconstitutionInput::new(
            current.id(),
            context_frontier_id(0x41),
            vec![SemanticTranscriptEntryRef::from_source(
                current.id(),
                origin_entry,
            )],
        )],
        Some(SessionAcceptanceTailReconstitutionInput::new(
            current.id(),
            accepted_input.id(),
            position,
            vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                current.id(),
                accepted_input,
                position,
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: choices(
                        current.current_configuration_defaults().version().as_u64(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            )],
        )),
    )
    .reconstitute()
    .expect("test active scheduling facts are complete")
}

fn runner_recovery_turn(current: &Session) -> AcceptedInputSchedulingProjection {
    active_turn_at_position_in_phase(
        current,
        SessionInputPosition::first(),
        ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
            turn_id(7),
            crate::RunnerId::from_uuid(uuid::Uuid::from_u128(0x81)),
            crate::RunnerGeneration::try_from_u64(2)
                .expect("the fixture placement revision is positive"),
            None,
            None,
        ),
    )
}

fn queued_turn(current: &Session) -> AcceptedInputSchedulingProjection {
    AcceptedInputSchedulingReconstitutionInput::new(
        current.clone(),
        vec![AcceptedInputTurnSchedulingRecord::new(
            current.id(),
            turn_id(7),
            current.id(),
            AcceptedInputLifecycle::new(
                accepted_input_id(0x21),
                AcceptedInputDisposition::OriginOf(turn_id(7)),
            ),
            current.id(),
            turn_id(7),
            AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(
                    current.current_configuration_defaults().version().as_u64(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            origin_configuration(current),
            AcceptedInputTurnSchedulingRecordState::Queued,
        )],
        vec![],
        vec![],
        None,
    )
    .reconstitute()
    .expect("test queued scheduling facts are complete")
}

fn terminal_source_turn_with_disposition(
    disposition: TurnDisposition,
) -> SubmitInputTerminalSourceReconstitutionInput {
    SubmitInputTerminalSourceReconstitutionInput::new(SubmitInputTerminalSourceConstructionInput {
        origin: source_turn_origin(),
        turn: turn_id(7),
        disposition,
    })
}

fn hash(value: &SubmitInput) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// One complete applied projection whose every fact matches the command.
fn applied_input() -> SubmitInputReconstitutionInput {
    let command = start_command(1, "hello", 1);
    SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(3),
            result_turn: turn_id(4),
            predecessor_origin: None,
            non_accepted_predecessor: None,
            accepted_command: command_id(1),
            accepted_input: accepted_input_id(3),
            accepted_session: session_id(1),
            accepted_content: content("hello"),
            accepted_delivery: command.delivery(),
            accepted_position: SessionInputPosition::first(),
            accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(4)),
            queue_session: session_id(1),
            queue_turn: turn_id(4),
            queue_order: crate::AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
            stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
            stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        },
    )
}

fn applied_facts(
    input: &mut SubmitInputReconstitutionInput,
) -> &mut super::reconstitution::SubmitInputTurnOriginAppliedReconstitutionFacts {
    let super::reconstitution::SubmitInputReconstitutionFacts::AppliedTurnOrigin(facts) =
        &mut input.facts
    else {
        panic!("the base reconstitution input is applied");
    };
    facts
}

fn terminal_source_facts(
    input: &mut SubmitInputTurnOriginReconstitutionInput,
) -> &mut super::reconstitution_input::SubmitInputTerminalFacts {
    let Some(source_terminal) = &mut turn_origin_facts(input).source_terminal else {
        panic!("the origin must come from reclassified steering");
    };
    source_terminal
}

fn turn_origin_facts(
    input: &mut SubmitInputTurnOriginReconstitutionInput,
) -> &mut super::reconstitution_input::SubmitInputTurnOriginReconstitutionFacts {
    input.chain.last_mut().expect("an origin chain is nonempty")
}

fn replace_source_origin(
    input: &mut SubmitInputTurnOriginReconstitutionInput,
    mut source: SubmitInputTurnOriginReconstitutionInput,
) {
    let current = input.chain.pop().expect("a reclassified origin has a head");
    source.chain.push(current);
    input.chain = source.chain;
}

fn append_unchecked_reclassified_origin(
    mut source: SubmitInputTurnOriginReconstitutionInput,
    position_value: u64,
    command_value: u128,
    accepted_input_value: u128,
) -> SubmitInputTurnOriginReconstitutionInput {
    let position =
        SessionInputPosition::try_from_u64(position_value).expect("the test position is positive");
    let source_turn = turn_id(u128::from(position_value) + 5);
    let turn = turn_id(u128::from(position_value) + 6);
    let command = SubmitInput::new(
        command_id(command_value),
        session_id(1),
        content("chained steering"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: source_turn,
        },
    );
    let accepted_input = accepted_input_id(accepted_input_value);
    source.chain.push(
        super::reconstitution_input::SubmitInputTurnOriginReconstitutionFacts {
            provenance: super::reconstitution_input::TurnOriginProvenance::Submit(Box::new(
                ReconstitutedSubmitInput {
                    command,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(
                        super::SubmitInputPendingSteeringAppliedResult {
                            accepted_input,
                            session: session_id(1),
                            acceptance_position: position,
                            binding: SteeringBinding::new(source_turn),
                        },
                    )),
                },
            )),
            lifecycle: AcceptedInputLifecycle::new(
                accepted_input,
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                    turn,
                    reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                },
            ),
            queue_accepted_input: accepted_input,
            queue_session: session_id(1),
            queue_turn: turn,
            queue_order: AcceptedInputQueueOrder::ordinary(position),
            source_terminal: Some(super::reconstitution_input::SubmitInputTerminalFacts {
                turn: source_turn,
                disposition: TurnDisposition::Completed,
            }),
        },
    );
    source
}

fn source_turn_origin() -> SubmitInputTurnOriginReconstitutionInput {
    source_turn_origin_with_identities(0x70, 0x71)
}

fn source_turn_origin_with_identities(
    source_command: u128,
    source_accepted_input: u128,
) -> SubmitInputTurnOriginReconstitutionInput {
    source_turn_origin_with_position(
        source_command,
        source_accepted_input,
        SessionInputPosition::first(),
    )
}

fn source_turn_origin_with_position(
    source_command: u128,
    source_accepted_input: u128,
    position: SessionInputPosition,
) -> SubmitInputTurnOriginReconstitutionInput {
    let command = start_command(source_command, "source", 1);
    let receipt = SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(source_accepted_input),
            result_turn: turn_id(7),
            predecessor_origin: None,
            non_accepted_predecessor: None,
            accepted_command: command_id(source_command),
            accepted_input: accepted_input_id(source_accepted_input),
            accepted_session: session_id(1),
            accepted_content: content("source"),
            accepted_delivery: command.delivery(),
            accepted_position: position,
            accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(7)),
            queue_session: session_id(1),
            queue_turn: turn_id(7),
            queue_order: AcceptedInputQueueOrder::ordinary(position),
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
            stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
            stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        },
    )
    .reconstitute()
    .expect("the source turn origin facts are complete");
    explicit_turn_origin_input(receipt)
}

fn explicit_turn_origin_input(
    receipt: ReconstitutedSubmitInput,
) -> SubmitInputTurnOriginReconstitutionInput {
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        panic!("the receipt must be an explicit turn origin");
    };
    let accepted_input = origin.accepted_input();
    let session = origin.session();
    let turn = origin.turn();
    let queue_order = origin.queue_order();
    SubmitInputTurnOriginReconstitutionInput::new(SubmitInputDirectTurnOriginConstructionInput {
        receipt,
        lifecycle: AcceptedInputLifecycle::new(
            accepted_input,
            AcceptedInputDisposition::OriginOf(turn),
        ),
        queue_accepted_input: accepted_input,
        queue_session: session,
        queue_turn: turn,
        queue_order,
    })
}

fn reclassified_turn_origin() -> SubmitInputTurnOriginReconstitutionInput {
    reclassified_turn_origin_with_disposition(TurnDisposition::Failed)
}

fn reclassified_turn_origin_with_disposition(
    disposition: TurnDisposition,
) -> SubmitInputTurnOriginReconstitutionInput {
    let position = SessionInputPosition::first()
        .checked_next()
        .expect("the pending input follows its source");
    let command = SubmitInput::new(
        command_id(0x72),
        session_id(1),
        content("reclassified steering"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(7),
        },
    );
    let receipt = SubmitInputReconstitutionInput::applied_pending_steering(
        SubmitInputAppliedPendingSteeringReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(0x73),
            result_source_turn: turn_id(7),
            source_turn_origin: source_turn_origin(),
            accepted_command: command.command_id(),
            accepted_input: accepted_input_id(0x73),
            accepted_session: session_id(1),
            accepted_content: content("reclassified steering"),
            accepted_delivery: command.delivery(),
            accepted_position: position,
        },
    )
    .reconstitute()
    .expect("the pending-steering receipt is canonical");
    let lifecycle = AcceptedInputLifecycle::new(
        accepted_input_id(0x73),
        AcceptedInputDisposition::PendingSteering {
            binding: SteeringBinding::new(turn_id(7)),
        },
    )
    .reclassify_as_turn_origin(
        turn_id(8),
        crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
    )
    .expect("pending steering can become visible origin work");
    SubmitInputTurnOriginReconstitutionInput::reclassified(
        SubmitInputReclassifiedTurnOriginConstructionInput {
            receipt,
            lifecycle,
            queue_accepted_input: accepted_input_id(0x73),
            queue_session: session_id(1),
            queue_turn: turn_id(8),
            queue_order: AcceptedInputQueueOrder::ordinary(position),
            source_terminal: terminal_source_turn_with_disposition(disposition),
        },
    )
}

fn after_applied_input() -> SubmitInputReconstitutionInput {
    let command = after_command(1, turn_id(7));
    let position = SessionInputPosition::first()
        .checked_next()
        .expect("after-current acceptance follows its predecessor");
    SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(3),
            result_turn: turn_id(8),
            predecessor_origin: Some(source_turn_origin()),
            non_accepted_predecessor: None,
            accepted_command: command_id(1),
            accepted_input: accepted_input_id(3),
            accepted_session: session_id(1),
            accepted_content: content("hello"),
            accepted_delivery: command.delivery(),
            accepted_position: position,
            accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(8)),
            queue_session: session_id(1),
            queue_turn: turn_id(8),
            queue_order: AcceptedInputQueueOrder::ordinary(position),
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
            stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
            stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        },
    )
}

fn interrupt_applied_input_with_non_accepted_predecessor(
    predecessor_session: crate::SessionId,
    predecessor_turn: crate::TurnId,
) -> SubmitInputReconstitutionInput {
    let mut input = after_applied_input();
    let command = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn_id(7),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    input.command = command.clone();
    let facts = applied_facts(&mut input);
    facts.predecessor_origin = None;
    facts.non_accepted_predecessor = Some(NonAcceptedTurnPredecessorReconstitutionInput {
        session: predecessor_session,
        turn: predecessor_turn,
    });
    facts.accepted_delivery = command.delivery();
    facts.queue_order =
        AcceptedInputQueueOrder::interrupt_immediately_after(facts.accepted_position, turn_id(7));
    input
}

fn after_applied_input_with_chained_predecessor(
    command_value: u128,
    accepted_input_value: u128,
    result_turn: crate::TurnId,
) -> SubmitInputReconstitutionInput {
    let command = after_command(command_value, turn_id(8));
    let position = SessionInputPosition::try_from_u64(3)
        .expect("after-current acceptance follows the complete predecessor chain");
    SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(accepted_input_value),
            result_turn,
            predecessor_origin: Some(append_unchecked_reclassified_origin(
                source_turn_origin(),
                2,
                0x102,
                0x202,
            )),
            non_accepted_predecessor: None,
            accepted_command: command_id(command_value),
            accepted_input: accepted_input_id(accepted_input_value),
            accepted_session: session_id(1),
            accepted_content: content("hello"),
            accepted_delivery: command.delivery(),
            accepted_position: position,
            accepted_disposition: AcceptedInputDisposition::OriginOf(result_turn),
            queue_session: session_id(1),
            queue_turn: result_turn,
            queue_order: AcceptedInputQueueOrder::ordinary(position),
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
            stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
            stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        },
    )
}

fn pending_steering_input() -> SubmitInputReconstitutionInput {
    let command = safe_point_command(1, turn_id(7));
    SubmitInputReconstitutionInput::applied_pending_steering(
        SubmitInputAppliedPendingSteeringReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(3),
            result_source_turn: turn_id(7),
            source_turn_origin: source_turn_origin(),
            accepted_command: command_id(1),
            accepted_input: accepted_input_id(3),
            accepted_session: session_id(1),
            accepted_content: content("hello"),
            accepted_delivery: command.delivery(),
            accepted_position: SessionInputPosition::first()
                .checked_next()
                .expect("pending steering follows its source origin"),
        },
    )
}

fn pending_steering_input_with_chained_source(
    command_value: u128,
    accepted_input_value: u128,
) -> SubmitInputReconstitutionInput {
    let command = safe_point_command(command_value, turn_id(8));
    SubmitInputReconstitutionInput::applied_pending_steering(
        SubmitInputAppliedPendingSteeringReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(accepted_input_value),
            result_source_turn: turn_id(8),
            source_turn_origin: append_unchecked_reclassified_origin(
                source_turn_origin(),
                2,
                0x102,
                0x202,
            ),
            accepted_command: command_id(command_value),
            accepted_input: accepted_input_id(accepted_input_value),
            accepted_session: session_id(1),
            accepted_content: content("hello"),
            accepted_delivery: command.delivery(),
            accepted_position: SessionInputPosition::try_from_u64(3)
                .expect("pending steering follows the complete source chain"),
        },
    )
}

fn pending_facts(
    input: &mut SubmitInputReconstitutionInput,
) -> &mut super::reconstitution::SubmitInputPendingSteeringAppliedReconstitutionFacts {
    let super::reconstitution::SubmitInputReconstitutionFacts::AppliedPendingSteering(facts) =
        &mut input.facts
    else {
        panic!("the base reconstitution input is pending steering");
    };
    facts
}

#[track_caller]
fn assert_reconstitutes_rejection(
    input: SubmitInputReconstitutionInput,
    expected: SubmitInputRejectedResult,
) {
    let reconstructed = input
        .reconstitute()
        .expect("complete rejection facts reconstruct");
    assert_eq!(
        reconstructed.result(),
        &SubmitInputResult::Rejected(expected),
        "replay must return the exact immutable rejection"
    );
}

#[track_caller]
fn assert_rejection_reconstitution_fails(
    input: SubmitInputReconstitutionInput,
    expected: SubmitInputReconstitutionFailure,
) {
    assert_eq!(
        input
            .reconstitute()
            .expect_err("cross-wired rejection facts must fail closed")
            .failure(),
        expected
    );
}

/// S01: comparison excludes only command identity and
/// includes the fixed user actor, session, exact content, delivery
/// discriminator, and every delivery field.
#[test]
fn s01_comparison_payload_is_structural() {
    let baseline = start_command(1, "hello", 1);
    let parent_alone_interrupt = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn_id(9),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let equal_interrupt_replay = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn_id(9),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );
    let conflicting_interrupt_replay = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn_id(9),
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
        },
    );

    assert_eq!(baseline, start_command(2, "hello", 1));
    assert_eq!(hash(&baseline), hash(&start_command(2, "hello", 1)));
    assert_ne!(baseline, start_command(1, "hello ", 1));
    assert_ne!(baseline, start_command(1, "hello", 2));
    assert_ne!(
        baseline,
        SubmitInput::new(
            command_id(1),
            session_id(2),
            content("hello"),
            baseline.delivery(),
        )
    );
    assert_eq!(baseline.actor(), Actor::User);
    assert_ne!(
        baseline,
        SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(9),
            },
        )
    );
    assert_eq!(parent_alone_interrupt, equal_interrupt_replay);
    assert_ne!(parent_alone_interrupt, conflicting_interrupt_replay);
}

/// S01: start preparation creates exact
/// queued-origin disposition, ordinary position, and frozen provenance.
#[test]
fn s01_start_prepares_complete_queued_work() {
    let command = start_command(1, "hello", 1);
    let prepared = command
        .clone()
        .prepare_when_no_active_turn(
            &session(1, 1, ModelSelectionRequest::Direct(direct(2))),
            accepted_input_id(3),
            Some(turn_id(4)),
            None,
            |_| None,
        )
        .expect("session matches");

    assert_eq!(prepared.command(), &command);
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        prepared.result()
    else {
        panic!("matching start request applies");
    };
    assert_eq!(applied.accepted_input(), accepted_input_id(3));
    assert_eq!(applied.session(), session_id(1));
    assert_eq!(applied.turn(), turn_id(4));
    assert_eq!(
        applied.disposition(),
        AcceptedInputDisposition::OriginOf(turn_id(4))
    );
    assert_eq!(applied.acceptance_position(), SessionInputPosition::first());
    assert_eq!(
        applied.origin_configuration().session_defaults_version(),
        version(1)
    );
    assert_eq!(
        applied.origin_configuration().requested().model(),
        ModelSelectionRequest::Direct(direct(2))
    );
    assert_eq!(
        applied.origin_configuration().effective().model(),
        &FrozenModelSelection::Direct(direct(2))
    );
}

/// S37: per-call settings participate in authoritative
/// origin derivation and remain explicit in the frozen request.
#[test]
fn s37_per_call_settings_are_frozen_for_the_origin() {
    let selection = direct(2);
    let per_call = ModelSettingsOverlay::new(
        SettingOverlay::Value(ReasoningLevel::High),
        FastModeOverlay::Inherit,
        SettingOverlay::Inherit,
    );
    let command = start_command_with_settings(1, "settings input", 1, per_call);
    let catalog = ModelCapabilityCatalog::try_from_definitions([ModelCapabilityDefinition::new(
        selection,
        ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        ),
    )])
    .expect("the fixture catalog has one direct selection");

    let prepared = command
        .prepare_when_no_active_turn_with_model_settings(
            &session(1, 1, ModelSelectionRequest::Direct(selection)),
            accepted_input_id(3),
            Some(turn_id(4)),
            None,
            |_| None,
            &catalog,
        )
        .expect("the explicit level is supported");

    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        prepared.result()
    else {
        panic!("the supported request applies");
    };
    assert_eq!(
        applied
            .origin_configuration()
            .effective()
            .model_settings()
            .effective()
            .reasoning_level(),
        Some(ReasoningLevel::High)
    );
    assert_eq!(
        applied
            .origin_configuration()
            .requested()
            .per_call_model_settings(),
        per_call
    );
    let event = applied
        .model_settings_event()
        .expect("the frozen settings match the selected direct model");
    assert_eq!(event.per_call_override(), per_call);
    assert_eq!(
        event.settings(),
        applied.origin_configuration().effective().model_settings()
    );
}

/// S37: the legacy preparation path fails closed when a caller
/// supplies settings that require a capability record.
#[test]
fn s37_legacy_preparation_rejects_unvalidated_per_call_settings() {
    let selection = direct(2);
    let per_call = ModelSettingsOverlay::new(
        SettingOverlay::Value(ReasoningLevel::High),
        FastModeOverlay::Inherit,
        SettingOverlay::Inherit,
    );
    let command = start_command_with_settings(1, "settings input", 1, per_call);

    let error = command
        .prepare_when_no_active_turn(
            &session(1, 1, ModelSelectionRequest::Direct(selection)),
            accepted_input_id(3),
            Some(turn_id(4)),
            None,
            |_| None,
        )
        .expect_err("the legacy path has no capability record");

    assert_eq!(
        error.failure(),
        SubmitInputPreparationFailure::ModelSettingsResolution(
            OriginModelSettingsError::MissingCapabilities { selection }
        )
    );
}

/// S37: catalog-free preparation cannot carry settings
/// validated for an alias's prior direct target across a retarget.
#[test]
fn s37_legacy_preparation_rejects_alias_retarget_settings() {
    let prior_selection = direct(2);
    let installed_selection = direct(3);
    let stored = ModelCapabilities::new(
        BTreeSet::from([ReasoningLevel::High]),
        FastModeSupport::Unsupported,
        BTreeSet::new(),
    )
    .validate_precedence(
        prior_selection,
        ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        ),
    )
    .expect("the prior model supports the stored level");
    let defaults = SessionConfigurationDefaults::complete_with_model_settings(
        ModelSelectionRequest::Alias(alias(1)),
        DangerousToolAutoApproval::Disabled,
        None,
        stored,
    )
    .expect("an alias retains its prior validation identity");
    let versioned = crate::VersionedSessionConfigurationDefaults::establish(defaults);
    let checked = versioned
        .derive_request_with_model_settings(
            versioned.version(),
            ModelSelectionOverride::UseSessionDefault,
            ModelSettingsOverlay::inherit_all(),
        )
        .expect("the fixture names the current defaults epoch");

    let error = freeze_origin_configuration(
        checked,
        |requested| {
            assert_eq!(requested, alias(1));
            Some(FrozenAliasDefinition::selecting(installed_selection))
        },
        None,
    )
    .expect_err("alias retargeting requires the new target capability record");

    assert_eq!(
        error,
        OriginModelSettingsError::MissingCapabilities {
            selection: installed_selection,
        }
    );
}

/// a legacy origin row cannot omit settings evidence while the
/// caller contributes an explicit per-call setting.
#[test]
fn legacy_reconstitution_rejects_explicit_per_call_settings() {
    let selection = direct(2);
    let per_call = ModelSettingsOverlay::new(
        SettingOverlay::Value(ReasoningLevel::High),
        FastModeOverlay::Inherit,
        SettingOverlay::Inherit,
    );
    let command = start_command_with_settings(1, "settings input", 1, per_call);
    let facts = StoredOriginConfigurationReconstitutionFacts {
        defaults_session: session_id(1),
        defaults_version: version(1),
        defaults: defaults(ModelSelectionRequest::Direct(selection)),
        stored_requested_model: ModelSelectionRequest::Direct(selection),
        stored_frozen_model: FrozenModelSelection::Direct(selection),
        stored_model_settings: None,
        stored_model_settings_adjustments: Vec::new(),
    };

    let result = reconstruct_origin_configuration(&command, facts);

    assert_eq!(
        result.expect_err("explicit settings require stored evidence"),
        SubmitInputReconstitutionFailure::FrozenModelMismatch
    );
}

/// a legacy origin row cannot carry defaults settings
/// validated for an alias's prior direct selection across a retarget.
#[test]
fn legacy_reconstitution_rejects_alias_retarget_settings() {
    let requested_alias = alias(1);
    let prior_selection = direct(2);
    let installed_selection = direct(3);
    let stored_settings = ModelCapabilities::new(
        BTreeSet::from([ReasoningLevel::High]),
        FastModeSupport::Unsupported,
        BTreeSet::new(),
    )
    .validate_precedence(
        prior_selection,
        ModelSettingsPrecedence::new(
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::new(
                SettingOverlay::Value(ReasoningLevel::High),
                FastModeOverlay::Inherit,
                SettingOverlay::Inherit,
            ),
            ModelSettingsOverlay::inherit_all(),
            ModelSettingsOverlay::inherit_all(),
        ),
    )
    .expect("the prior selection supports the stored defaults");
    let stored_defaults = SessionConfigurationDefaults::complete_with_model_settings(
        ModelSelectionRequest::Alias(requested_alias),
        DangerousToolAutoApproval::Disabled,
        None,
        stored_settings,
    )
    .expect("alias defaults can retain prior validation evidence");
    let command = start_command(1, "settings input", 1);
    let facts = StoredOriginConfigurationReconstitutionFacts {
        defaults_session: session_id(1),
        defaults_version: version(1),
        defaults: stored_defaults,
        stored_requested_model: ModelSelectionRequest::Alias(requested_alias),
        stored_frozen_model: FrozenModelSelection::FrozenAlias {
            alias: requested_alias,
            definition: FrozenAliasDefinition::selecting(installed_selection),
        },
        stored_model_settings: None,
        stored_model_settings_adjustments: Vec::new(),
    };

    let result = reconstruct_origin_configuration(&command, facts);

    assert_eq!(
        result.expect_err("retargeted defaults require stored settings evidence"),
        SubmitInputReconstitutionFailure::FrozenModelMismatch
    );
}

/// S01: explicit alias requests freeze the supplied immutable
/// definition, while a missing definition is a typed recorded rejection.
#[test]
fn s01_alias_definition_is_frozen_or_rejected() {
    let command = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(
                1,
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
            ),
        },
    );
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(3)));
    let frozen = command
        .clone()
        .prepare_when_no_active_turn(
            &current,
            accepted_input_id(4),
            Some(turn_id(5)),
            None,
            |requested| {
                assert_eq!(requested, alias(2));
                Some(FrozenAliasDefinition::selecting(direct(6)))
            },
        )
        .expect("session matches");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) = frozen.result()
    else {
        panic!("selectable alias applies");
    };
    assert_eq!(
        applied.origin_configuration().effective().model(),
        &FrozenModelSelection::FrozenAlias {
            alias: alias(2),
            definition: FrozenAliasDefinition::selecting(direct(6)),
        }
    );

    assert!(matches!(
        command
            .prepare_when_no_active_turn(
                &current,
                accepted_input_id(4),
                Some(turn_id(5)),
                None,
                |_| None,
            )
            .expect("session matches")
            .result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
            session,
            alias: rejected_alias,
        }) if *session == session_id(1) && *rejected_alias == alias(2)
    ));
}

/// Prepares one active-work command against the canonical vacant-slot
/// session and asserts the exact recorded rejection; the command and
/// every expected field stay at the call site.
#[track_caller]
fn assert_vacant_slot_records_rejection(
    command: SubmitInput,
    turn_candidate: Option<crate::TurnId>,
    expected: SubmitInputRejectedResult,
) {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let prepared = command
        .prepare_when_no_active_turn(&current, accepted_input_id(3), turn_candidate, None, |_| {
            panic!("active-work rejection does not resolve configuration")
        })
        .expect("session matches");
    assert_eq!(prepared.result(), &SubmitInputResult::Rejected(expected));
}

/// S01: active-work variants record the exact
/// expected turn in a no-active-turn rejection.
#[test]
fn s01_active_modes_reject_when_no_turn_is_active() {
    assert_vacant_slot_records_rejection(
        interrupt_command(1, turn_id(7)),
        Some(turn_id(4)),
        SubmitInputRejectedResult::NoActiveTurn {
            session: session_id(1),
            expected_active_turn: turn_id(7),
        },
    );
    assert_vacant_slot_records_rejection(
        safe_point_command(1, turn_id(7)),
        None,
        SubmitInputRejectedResult::NoActiveTurn {
            session: session_id(1),
            expected_active_turn: turn_id(7),
        },
    );
    assert_vacant_slot_records_rejection(
        after_command(1, turn_id(7)),
        Some(turn_id(4)),
        SubmitInputRejectedResult::NoActiveTurn {
            session: session_id(1),
            expected_active_turn: turn_id(7),
        },
    );

    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let mismatch = safe_point_command(1, turn_id(7))
        .prepare_when_no_active_turn(
            &current,
            accepted_input_id(3),
            Some(turn_id(4)),
            None,
            |_| None,
        )
        .expect_err("safe-point steering initially creates no turn");
    assert_eq!(
        mismatch.failure(),
        SubmitInputPreparationFailure::TurnCandidateMismatch
    );
}

/// S09: matching after-current input
/// creates ordinary queued origin work with the next acceptance position
/// and exact frozen configuration.
#[test]
fn s09_matching_after_current_prepares_ordinary_turn_origin() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);
    let command = after_command(1, active_turn);
    let prepared = command
        .clone()
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("matching after-current input is available");

    let SubmitInputResult::Applied(applied) = prepared.result() else {
        panic!("matching after-current input applies");
    };
    let origin = applied
        .turn_origin()
        .expect("after-current input creates origin work");
    assert_eq!(origin.accepted_input(), accepted_input);
    assert_eq!(origin.turn(), turn_candidate);
    assert_eq!(
        origin.disposition(),
        AcceptedInputDisposition::OriginOf(turn_candidate)
    );
    assert_eq!(origin.acceptance_position().as_u64(), 2);
    assert_eq!(
        origin.queue_order(),
        AcceptedInputQueueOrder::ordinary(origin.acceptance_position())
    );
    assert_eq!(
        origin.origin_configuration().effective().model(),
        &FrozenModelSelection::Direct(direct(2))
    );
}

/// S08: matching safe-point input creates
/// pending steering bound to the exact active turn and carries no
/// turn-origin fields.
#[test]
fn s08_matching_next_safe_point_prepares_pending_steering() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let prepared = safe_point_command(1, active_turn)
        .prepare_with_active_turn(&active, accepted_input, None, |_| {
            panic!("safe-point acceptance has no configuration")
        })
        .expect("matching safe-point input is available");

    let SubmitInputResult::Applied(applied) = prepared.result() else {
        panic!("matching safe-point input applies");
    };
    assert_eq!(applied.accepted_input(), accepted_input);
    assert_eq!(applied.acceptance_position().as_u64(), 2);
    assert_eq!(
        applied.disposition(),
        AcceptedInputDisposition::PendingSteering {
            binding: SteeringBinding::new(active_turn),
        }
    );
    assert!(applied.turn_origin().is_none());
    let steering = applied
        .pending_steering()
        .expect("safe-point acceptance creates pending steering");
    assert_eq!(steering.binding().source_turn(), active_turn);
}

/// S01: a vacant-slot start submitted while the slot
/// is occupied records the exact authoritative active turn.
#[test]
fn s01_occupied_slot_start_records_active_turn_presence() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);
    let start = start_command(1, "hello", 1)
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("active presence is an authoritative rejection");
    assert!(matches!(
        start.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn: recorded_active_turn,
        }) if *session == current.id() && *recorded_active_turn == active_turn
    ));
}

#[test]
fn delegated_active_turn_blocks_vacant_slot_start() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let delegated_turn = turn_id(7);
    let prepared = start_command(1, "hello", 1)
        .prepare_with_delegated_active_turn(
            &current,
            delegated_turn,
            Some(SessionInputPosition::first()),
            None,
            false,
            accepted_input_id(3),
            Some(turn_id(8)),
            |_| None,
        )
        .expect("delegated slot ownership is authoritative");

    let SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
        active_turn,
        ..
    }) = prepared.result()
    else {
        panic!("the delegated turn must retain the active slot");
    };
    assert_eq!(*active_turn, delegated_turn);
}

#[test]
fn delegated_active_turn_accepts_safe_point_steering() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let delegated_turn = turn_id(7);
    let prepared = safe_point_command(2, delegated_turn)
        .prepare_with_delegated_active_turn(
            &current,
            delegated_turn,
            Some(SessionInputPosition::first()),
            None,
            false,
            accepted_input_id(3),
            None,
            |_| None,
        )
        .expect("safe-point input binds to the delegated active turn");
    let steering = prepared.result();
    let steering = applied_result(steering)
        .pending_steering()
        .expect("the delegated turn receives pending steering");

    assert_eq!(steering.binding().source_turn(), delegated_turn);
    assert_eq!(
        steering.acceptance_position(),
        SessionInputPosition::first()
            .checked_next()
            .expect("the second position exists")
    );
}

#[test]
fn delegated_active_turn_accepts_correlated_interrupt_successor() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let delegated_turn = turn_id(7);
    let successor = turn_id(8);
    let prepared = interrupt_command(3, delegated_turn)
        .prepare_with_delegated_active_turn(
            &current,
            delegated_turn,
            Some(SessionInputPosition::first()),
            None,
            false,
            accepted_input_id(3),
            Some(successor),
            |_| None,
        )
        .expect("the interrupt correlates to the delegated active turn");
    let origin = prepared.result();
    let origin = applied_result(origin)
        .turn_origin()
        .expect("the interrupt creates an immediate successor");

    assert_eq!(origin.turn(), successor);
    assert_eq!(
        origin.queue_order().priority(),
        AcceptedInputQueuePriority::InterruptImmediatelyAfter {
            predecessor: delegated_turn,
        }
    );
    assert_eq!(
        origin
            .applied_interrupt()
            .expect("the interrupt carries proof")
            .proof()
            .predecessor(),
        delegated_turn
    );
}

/// S37: delegation-origin slot ownership cannot bypass the
/// capability evidence required by an explicit per-call setting.
#[test]
fn s37_delegated_successor_rejects_unvalidated_per_call_settings() {
    let selection = direct(2);
    let current = session(1, 1, ModelSelectionRequest::Direct(selection));
    let delegated_turn = turn_id(7);
    let per_call = ModelSettingsOverlay::new(
        SettingOverlay::Value(ReasoningLevel::High),
        FastModeOverlay::Inherit,
        SettingOverlay::Inherit,
    );
    let command = SubmitInput::new(
        command_id(4),
        current.id(),
        content("settings successor"),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: delegated_turn,
            configuration: PerInputConfigurationChoices::with_model_settings(
                version(1),
                ModelSelectionOverride::UseSessionDefault,
                per_call,
            ),
        },
    );

    let error = command
        .prepare_with_delegated_active_turn(
            &current,
            delegated_turn,
            Some(SessionInputPosition::first()),
            None,
            false,
            accepted_input_id(4),
            Some(turn_id(8)),
            |_| None,
        )
        .expect_err("the legacy delegated path has no capability record");

    assert_eq!(
        error.failure(),
        SubmitInputPreparationFailure::ModelSettingsResolution(
            OriginModelSettingsError::MissingCapabilities { selection }
        )
    );
}

/// S07 / S08 / S09: every active-work delivery mode
/// records its stale target against the exact authoritative active turn.
#[test]
fn s07_s08_s09_occupied_slot_active_work_records_stale_target() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let actual_active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let stale_target = turn_id(9);
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);

    let stale_after = after_command(2, stale_target)
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("a stale after-current target is an authoritative rejection");
    assert!(matches!(
        stale_after.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
            expected_active_turn,
            actual_active_turn: recorded_active_turn,
            ..
        }) if *expected_active_turn == stale_target
            && *recorded_active_turn == actual_active_turn
    ));

    let stale_safe_point = safe_point_command(3, stale_target)
        .prepare_with_active_turn(&active, accepted_input, None, |_| None)
        .expect("a stale safe-point target is an authoritative rejection");
    assert!(matches!(
        stale_safe_point.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
            expected_active_turn,
            actual_active_turn: recorded_active_turn,
            ..
        }) if *expected_active_turn == stale_target
            && *recorded_active_turn == actual_active_turn
    ));

    let stale_interrupt = interrupt_command(4, stale_target)
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("a stale interrupt target is an authoritative rejection");
    assert!(matches!(
        stale_interrupt.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
            expected_active_turn,
            actual_active_turn: recorded_active_turn,
            ..
        }) if *expected_active_turn == stale_target
            && *recorded_active_turn == actual_active_turn
    ));
}

/// S07: matching interrupt preparation
/// creates the exact immediate successor and sole cancellation proof.
#[test]
fn s07_occupied_slot_matching_interrupt_applies() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);
    let interrupt = interrupt_command(6, active_turn);
    let prepared = interrupt
        .clone()
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("matching interrupt creates one correlated result");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        prepared.result()
    else {
        panic!("matching interrupt applies as successor origin");
    };
    let authority = applied
        .applied_interrupt()
        .expect("interrupt origin carries cancellation authority");
    assert_eq!(authority.proof().command(), interrupt.command_id());
    assert_eq!(authority.proof().predecessor(), active_turn);
    assert_eq!(authority.successor(), turn_candidate);
    assert_eq!(
        applied.queue_order().priority(),
        AcceptedInputQueuePriority::InterruptImmediatelyAfter {
            predecessor: active_turn
        }
    );
}

/// S07: runner recovery does not invent a new
/// non-consuming rejection that would foreclose stop-before-abandonment.
#[test]
fn s07_runner_recovery_preserves_interrupt_authority() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = runner_recovery_turn(&current);
    let interrupted_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let successor = turn_id(8);
    let prepared = interrupt_command(6, interrupted_turn)
        .prepare_with_active_turn(&active, accepted_input_id(3), Some(successor), |_| None)
        .expect("runner recovery preserves the existing interrupt algebra");
    let origin = applied_result(prepared.result())
        .turn_origin()
        .expect("the interrupt creates an immediate successor");

    assert_eq!(origin.turn(), successor);
    assert_eq!(
        origin
            .applied_interrupt()
            .expect("the interrupt carries cancellation authority")
            .proof()
            .predecessor(),
        interrupted_turn
    );
}

/// The canonical active-slot projection parked on one confirm request:
/// fixture turn 7 completed its producing call, the yielded tool round
/// proposes one request, and no decision has resolved the approval wait.
fn approval_wait_turn(current: &Session) -> AcceptedInputSchedulingProjection {
    let origin_entry = semantic_transcript_entry_id(0x31);
    let tool_use_entry = semantic_transcript_entry_id(0x32);
    let producing_call = model_call_id(0x61);
    let undecided_request = tool_request_id(0x71);
    let starting_frontier = context_frontier_id(0x41);
    let yielded_frontier = context_frontier_id(0x42);
    let request = ToolRequestReconstitutionInput::new(
        undecided_request,
        current.id(),
        turn_id(7),
        producing_call,
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("confirmed")).expect("fixture name is canonical"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are canonical"),
    )
    .into_request();
    let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
        current.id(),
        yielded_frontier,
        vec![
            SemanticTranscriptEntryRef::from_source(current.id(), origin_entry),
            SemanticTranscriptEntryRef::from_source(current.id(), tool_use_entry),
        ],
    )
    .expect("the tool response extends the starting frontier");
    let batch = ToolBatchReconstitutionInput::new(
        current.id(),
        turn_id(7),
        producing_call,
        yielded,
        vec![request],
        vec![],
        vec![],
        ToolBatchPhaseReconstitutionInput::AwaitingApproval {
            request: undecided_request,
        },
    )
    .reconstitute()
    .expect("the undecided single-request batch awaits its approval");
    let target = ResolvedProviderTarget::naming(provider_model_identity(0x62));
    let accepted_input = AcceptedInputLifecycle::new(
        accepted_input_id(0x21),
        AcceptedInputDisposition::OriginOf(turn_id(7)),
    );
    AcceptedInputSchedulingReconstitutionInput::new(
        current.clone(),
        vec![AcceptedInputTurnSchedulingRecord::new(
            current.id(),
            turn_id(7),
            current.id(),
            accepted_input.clone(),
            current.id(),
            turn_id(7),
            AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(
                    current.current_configuration_defaults().version().as_u64(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
            origin_configuration(current),
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier,
                phase: ActiveTurnSchedulingReconstitutionInput::awaiting_approval(
                    turn_id(7),
                    &batch,
                )
                .expect("the approval wait names the fixture turn"),
            },
        )],
        vec![
            SemanticTranscriptEntryReconstitutionInput::new(
                origin_entry,
                current.id(),
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: accepted_input_id(0x21),
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry,
                current.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request: undecided_request,
                },
            ),
        ],
        vec![
            ResolvedContextFrontierReconstitutionInput::new(
                current.id(),
                starting_frontier,
                vec![SemanticTranscriptEntryRef::from_source(
                    current.id(),
                    origin_entry,
                )],
            ),
            ResolvedContextFrontierReconstitutionInput::new(
                current.id(),
                yielded_frontier,
                vec![
                    SemanticTranscriptEntryRef::from_source(current.id(), origin_entry),
                    SemanticTranscriptEntryRef::from_source(current.id(), tool_use_entry),
                ],
            ),
        ],
        Some(SessionAcceptanceTailReconstitutionInput::new(
            current.id(),
            accepted_input.id(),
            SessionInputPosition::first(),
            vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                current.id(),
                accepted_input,
                SessionInputPosition::first(),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: choices(
                        current.current_configuration_defaults().version().as_u64(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
            )],
        )),
    )
    .with_model_call_facts(
        vec![PinnedProviderTargetReconstitutionInput::new(
            turn_id(7),
            target,
        )],
        vec![ModelCallReconstitutionInput::new(
            producing_call,
            turn_id(7),
            turn_attempt_id(0x52),
            FrozenModelSelection::Direct(direct(2)),
            target,
            starting_frontier,
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )],
    )
    .reconstitute()
    .expect("the parked approval-wait scheduling facts are complete")
}

/// S07 / S10: an interrupt against a parked approval
/// wait records the typed rejection instead of accepting a successor; the
/// wait remains parked until its canonical decision command resolves the
/// approval obligation.
#[test]
fn s07_s10_interrupt_against_parked_approval_wait_is_rejected() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = approval_wait_turn(&current);
    let parked_turn = active
        .active_turn()
        .expect("the fixture has one active turn");
    assert!(
        matches!(
            parked_turn.active_phase(),
            Some(ActiveTurnPhase::AwaitingApproval { .. })
        ),
        "the fixture slot must be parked on its approval wait"
    );
    let actual_active_turn = parked_turn.turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);

    let rejected = interrupt_command(6, actual_active_turn)
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("a parked approval wait is an authoritative rejection");
    assert!(
        matches!(
            rejected.result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                    session,
                    active_turn,
                },
            ) if *session == current.id() && *active_turn == actual_active_turn
        ),
        "the interrupt must not bypass the decision command: {:?}",
        rejected.result()
    );
}

/// S07 / S10: the recorded parked-approval interrupt
/// rejection reconstructs exactly.
#[test]
fn s07_s10_parked_approval_interrupt_rejection_reconstitutes_exactly() {
    let session = session_id(1);
    let active_turn = turn_id(7);

    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
            SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                command: interrupt_command(1, active_turn),
                stored_actor: Actor::User,
                result_session: session,
                result_active_turn: active_turn,
                active_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
            session,
            active_turn,
        },
    );
}

/// S07 / S10: parked-approval interrupt rejection
/// replay fails closed when the command's delivery or expected active turn
/// is cross-wired against the recorded rejection.
#[test]
fn s07_s10_parked_approval_interrupt_rejection_evidence_is_exact() {
    let session = session_id(1);
    let active_turn = turn_id(7);
    let other_turn = turn_id(9);

    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
            SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                command: interrupt_command(1, other_turn),
                stored_actor: Actor::User,
                result_session: session,
                result_active_turn: active_turn,
                active_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputReconstitutionFailure::StoppingRejectionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
            SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                command: safe_point_command(1, active_turn),
                stored_actor: Actor::User,
                result_session: session,
                result_active_turn: active_turn,
                active_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputReconstitutionFailure::StoppingRejectionMismatch,
    );
}

/// S09: after-current preparation records
/// the exact stale session-defaults version.
#[test]
fn s09_occupied_slot_after_current_records_stale_defaults_version() {
    let stale_session = session(1, 2, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&stale_session);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);
    let stale = after_command(1, active_turn)
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| {
            panic!("stale defaults cannot reach alias resolution")
        })
        .expect("a stale defaults version is an authoritative rejection");
    assert!(matches!(
        stale.result(),
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                expected,
                current,
                ..
            }
        ) if *expected == version(1) && *current == version(2)
    ));
}

/// S09: after-current preparation records the exact
/// unresolved model alias.
#[test]
fn s09_occupied_slot_after_current_records_unknown_alias() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);
    let unknown_alias = alias(9);
    let alias_command = SubmitInput::new(
        command_id(2),
        session_id(1),
        content("hello"),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: active_turn,
            configuration: choices(
                1,
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(unknown_alias)),
            ),
        },
    );
    let rejected = alias_command
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("an unresolved alias is an authoritative rejection");
    assert!(matches!(
        rejected.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
            alias: unknown,
            ..
        }) if *unknown == unknown_alias
    ));
}

/// S08 / S09: both occupied-slot acceptance paths
/// record exhaustion of the validated session acceptance tail.
#[test]
fn s08_s09_occupied_slot_acceptance_records_position_exhaustion() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
    let active = active_turn_at_position(&current, maximum);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);

    let after = after_command(3, active_turn)
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect("after-current position exhaustion is authoritative");
    assert!(matches!(
        after.result(),
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
        ) if *last == maximum
    ));

    let safe_point = safe_point_command(4, active_turn)
        .prepare_with_active_turn(&active, accepted_input, None, |_| None)
        .expect("safe-point position exhaustion is authoritative");
    assert!(matches!(
        safe_point.result(),
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
        ) if *last == maximum
    ));
}

/// S09: occupied-slot preparation rejects a scheduling
/// projection from another session without claiming the command.
#[test]
fn s09_occupied_slot_preparation_rejects_cross_session_projection() {
    let wrong_session = session(2, 1, ModelSelectionRequest::Direct(direct(2)));
    let wrong_projection = active_turn(&wrong_session);
    let projected_active_turn = wrong_projection
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);
    let command = after_command(1, projected_active_turn);
    let wrong_active_session = command
        .clone()
        .prepare_with_active_turn(
            &wrong_projection,
            accepted_input,
            Some(turn_candidate),
            |_| None,
        )
        .expect_err("a cross-session active projection is nonterminal");
    assert_eq!(
        wrong_active_session.failure(),
        SubmitInputPreparationFailure::SessionMismatch {
            provided_session: wrong_session.id(),
        }
    );
    assert_eq!(wrong_active_session.command(), &command);
}

/// S09: a queued projection cannot stand in for the
/// authoritative active turn.
#[test]
fn s09_occupied_slot_preparation_requires_active_projection() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let queued = queued_turn(&current);
    let projected_turn = queued
        .turns()
        .next()
        .expect("the fixture has one queued turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);
    let command = after_command(1, projected_turn);
    let not_active = command
        .clone()
        .prepare_with_active_turn(&queued, accepted_input, Some(turn_candidate), |_| None)
        .expect_err("a queued projection cannot stand in for the active turn");
    assert_eq!(
        not_active.failure(),
        SubmitInputPreparationFailure::ActiveTurnProjectionMissing
    );
    assert_eq!(not_active.command(), &command);
}

/// S08 / S09: each occupied-slot delivery mode requires the
/// exact candidate shape it can apply.
#[test]
fn s08_s09_occupied_slot_preparation_rejects_mismatched_turn_candidate_shape() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let active_turn = active
        .active_turn()
        .expect("the fixture has one active turn")
        .turn();
    let accepted_input = accepted_input_id(3);
    let turn_candidate = turn_id(8);

    let missing_turn = after_command(1, active_turn)
        .prepare_with_active_turn(&active, accepted_input, None, |_| None)
        .expect_err("after-current input requires a minted turn candidate");
    assert_eq!(
        missing_turn.failure(),
        SubmitInputPreparationFailure::TurnCandidateMismatch
    );

    let reused_active_turn = after_command(2, active_turn)
        .prepare_with_active_turn(&active, accepted_input, Some(active_turn), |_| None)
        .expect_err("after-current work cannot reuse its active predecessor");
    assert_eq!(
        reused_active_turn.failure(),
        SubmitInputPreparationFailure::TurnCandidateMismatch
    );

    let extra_turn = safe_point_command(3, active_turn)
        .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
        .expect_err("safe-point input cannot receive a turn candidate");
    assert_eq!(
        extra_turn.failure(),
        SubmitInputPreparationFailure::TurnCandidateMismatch
    );
}

/// S08 / S09: no occupied-slot acceptance path can
/// reuse the active turn's canonical origin identity.
#[test]
fn s08_s09_occupied_slot_preparation_rejects_active_origin_identity_reuse() {
    let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
    let active = active_turn(&current);
    let active_turn = active
        .active_turn()
        .expect("the test projection has one active turn")
        .turn();
    let active_origin = active
        .turn(active_turn)
        .expect("the fixture retains its active turn")
        .accepted_input()
        .id();
    let turn_candidate = turn_id(8);

    let after = after_command(2, active_turn)
        .prepare_with_active_turn(&active, active_origin, Some(turn_candidate), |_| None)
        .expect_err("after-current acceptance cannot reuse the active origin");
    assert_eq!(
        after.failure(),
        SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
            active_turn,
            accepted_input: active_origin,
        }
    );

    let safe_point = safe_point_command(3, active_turn)
        .prepare_with_active_turn(&active, active_origin, None, |_| None)
        .expect_err("safe-point acceptance cannot reuse the active origin");
    assert_eq!(
        safe_point.failure(),
        SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
            active_turn,
            accepted_input: active_origin,
        }
    );
}

/// S01: missing sessions, stale defaults, unknown
/// aliases, and exhausted positions remain distinct terminal results.
#[test]
fn s01_authoritative_rejections_are_typed() {
    let command = start_command(1, "hello", 1);
    assert!(matches!(
        command.clone().prepare_session_not_found().result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { .. })
    ));
    assert!(matches!(
        command
            .clone()
            .prepare_when_no_active_turn(
                &session(1, 2, ModelSelectionRequest::Direct(direct(2))),
                accepted_input_id(3),
                Some(turn_id(4)),
                None,
                |_| None,
            )
            .expect("session matches")
            .result(),
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch { .. }
        )
    ));
    let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
    assert!(matches!(
        command
            .prepare_when_no_active_turn(
                &session(1, 1, ModelSelectionRequest::Direct(direct(2))),
                accepted_input_id(3),
                Some(turn_id(4)),
                Some(maximum),
                |_| None,
            )
            .expect("session matches")
            .result(),
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
        ) if *last == maximum
    ));
}

#[test]
fn attachment_authority_rejections_reconstitute_exact_evidence() {
    let digest = BlobDigest::from_bytes([0x5a; 32]);
    let command = attachment_command(0x51, digest);

    let missing = SubmitInputReconstitutionInput::rejected_attachment_blob_not_found(
        SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_digest: digest,
        },
    )
    .reconstitute()
    .expect("matching missing-blob evidence reconstructs");
    assert!(matches!(
        missing.result(),
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::AttachmentBlobNotFound { digest: stored }
        ) if *stored == digest
    ));

    let budget = SubmitInputReconstitutionInput::rejected_attachment_byte_budget_exceeded(
        SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_maximum_bytes: 4096,
        },
    )
    .reconstitute()
    .expect("matching byte-budget evidence reconstructs");
    assert!(matches!(
        budget.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
            maximum_bytes: 4096
        })
    ));

    let text_budget = SubmitInputReconstitutionInput::rejected_attachment_byte_budget_exceeded(
        SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
            command: start_command(0x52, "text-only frontier input", 1),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_maximum_bytes: 4096,
        },
    )
    .reconstitute()
    .expect("frontier-driven byte-budget evidence reconstructs for text input");
    assert!(matches!(
        text_budget.result(),
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
            maximum_bytes: 4096
        })
    ));

    let mismatch = SubmitInputReconstitutionInput::rejected_attachment_blob_not_found(
        SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
            command,
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_digest: BlobDigest::from_bytes([0x6b; 32]),
        },
    )
    .reconstitute()
    .expect_err("a digest absent from the command fails closed");
    assert_eq!(
        mismatch.failure(),
        SubmitInputReconstitutionFailure::AttachmentDigestMismatch
    );
}

/// complete applied facts reconstruct the canonical
/// result, while a cross-wired content fact fails closed.
#[test]
fn applied_reconstitution_checks_complete_correlations() {
    let reconstructed = applied_input()
        .reconstitute()
        .expect("complete matching facts reconstruct");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        reconstructed.result()
    else {
        panic!("applied facts reconstruct an applied result");
    };
    assert_eq!(applied.turn(), turn_id(4));

    let mut wrong = applied_input();
    applied_facts(&mut wrong).accepted_content = content("different");
    assert_eq!(
        wrong
            .reconstitute()
            .expect_err("cross-wired content fails closed")
            .failure(),
        SubmitInputReconstitutionFailure::AcceptedContentMismatch
    );
}

/// S08 / S09: both occupied applied
/// shapes reconstruct only from exact treatment and source correlations.
#[test]
fn occupied_applied_shapes_reconstitute_exactly() {
    let after = after_applied_input()
        .reconstitute()
        .expect("complete after-current origin facts reconstruct");
    let SubmitInputResult::Applied(after) = after.result() else {
        panic!("after-current facts remain applied");
    };
    let after = after
        .turn_origin()
        .expect("after-current facts create turn-origin work");
    assert_eq!(after.turn(), turn_id(8));
    assert_eq!(
        after.origin_configuration().effective().model(),
        &FrozenModelSelection::Direct(direct(2))
    );

    let pending = pending_steering_input()
        .reconstitute()
        .expect("complete pending-steering facts reconstruct");
    let SubmitInputResult::Applied(pending) = pending.result() else {
        panic!("safe-point facts remain applied");
    };
    assert_eq!(
        pending.disposition(),
        AcceptedInputDisposition::PendingSteering {
            binding: SteeringBinding::new(turn_id(7)),
        }
    );
    assert!(pending.turn_origin().is_none());
}

/// Asserts the advanced lifecycle has left pending steering behind while
/// replay of the canonical receipt still reconstructs pending steering.
#[track_caller]
fn assert_replay_survives_lifecycle_progress(advanced: &AcceptedInputLifecycle) {
    assert!(
        !matches!(
            advanced.disposition(),
            AcceptedInputDisposition::PendingSteering { .. }
        ),
        "the lifecycle under test must have progressed past pending steering"
    );
    let replayed = pending_steering_input()
        .reconstitute()
        .expect("mutable lifecycle progress cannot rewrite the receipt");
    assert!(matches!(
        replayed.result(),
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
    ));
}

/// S08: replay reconstructs the immutable original
/// pending-steering receipt independently of its mutable lifecycle.
#[test]
fn pending_steering_replay_survives_lifecycle_progress() {
    let initial = AcceptedInputLifecycle::new(
        accepted_input_id(3),
        AcceptedInputDisposition::PendingSteering {
            binding: SteeringBinding::new(turn_id(7)),
        },
    );

    let consumed = initial
        .clone()
        .consume_as_steering(crate::test_support::model_call_id(0x81))
        .expect("pending steering can be consumed");
    assert_replay_survives_lifecycle_progress(&consumed);

    let reclassified = initial
        .reclassify_as_turn_origin(
            turn_id(8),
            crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
        )
        .expect("pending steering can be reclassified");
    assert_replay_survives_lifecycle_progress(&reclassified);
}

fn applied_result(result: &SubmitInputResult) -> &SubmitInputAppliedResult {
    match result {
        SubmitInputResult::Applied(applied) => applied,
        SubmitInputResult::Rejected(rejected) => {
            panic!("expected an applied result, got {rejected:?}")
        }
    }
}

/// S08 / S09: a canonical turn origin can come from
/// either an original turn-producing receipt or a later visible
/// reclassification of immutable pending steering.
#[test]
fn s08_s09_reclassified_turn_origins_support_replay() {
    let predecessor_position = SessionInputPosition::first()
        .checked_next()
        .expect("the reclassified origin follows its source");
    let accepted_position = predecessor_position
        .checked_next()
        .expect("later input follows the reclassified origin");

    let after_command = after_command(0x80, turn_id(8));
    let after = SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command: after_command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(0x81),
            result_turn: turn_id(9),
            predecessor_origin: Some(reclassified_turn_origin()),
            non_accepted_predecessor: None,
            accepted_command: after_command.command_id(),
            accepted_input: accepted_input_id(0x81),
            accepted_session: session_id(1),
            accepted_content: content("hello"),
            accepted_delivery: after_command.delivery(),
            accepted_position,
            accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(9)),
            queue_session: session_id(1),
            queue_turn: turn_id(9),
            queue_order: AcceptedInputQueueOrder::ordinary(accepted_position),
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
            stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
            stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        },
    )
    .reconstitute()
    .expect("after-current replay accepts a reclassified predecessor");
    assert_eq!(
        applied_result(after.result())
            .turn_origin()
            .expect("the after-current result creates a turn")
            .turn(),
        turn_id(9)
    );

    let steering_command = SubmitInput::new(
        command_id(0x82),
        session_id(1),
        content("later steering"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(8),
        },
    );
    let steering = SubmitInputReconstitutionInput::applied_pending_steering(
        SubmitInputAppliedPendingSteeringReconstitutionInput {
            command: steering_command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(0x83),
            result_source_turn: turn_id(8),
            source_turn_origin: reclassified_turn_origin(),
            accepted_command: steering_command.command_id(),
            accepted_input: accepted_input_id(0x83),
            accepted_session: session_id(1),
            accepted_content: content("later steering"),
            accepted_delivery: steering_command.delivery(),
            accepted_position,
        },
    )
    .reconstitute()
    .expect("pending-steering replay accepts a reclassified source");
    assert!(
        applied_result(steering.result())
            .pending_steering()
            .is_some()
    );

    let rejection = SubmitInputReconstitutionInput::rejected_active_turn_present(
        SubmitInputRejectedActiveTurnPresentReconstitutionInput {
            command: start_command(0x84, "rejected start", 1),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_active_turn: turn_id(8),
            active_turn_origin: reclassified_turn_origin(),
        },
    )
    .reconstitute()
    .expect("rejection replay accepts a reclassified active origin");
    assert_eq!(
        rejection.result(),
        &SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
            session: session_id(1),
            active_turn: turn_id(8),
        })
    );
}

/// S08: model rendering recovers the final accepted
/// input's exact user content from a fully checked reclassification chain.
#[test]
fn s08_reclassified_origin_preserves_renderable_user_content() {
    let origin = reclassified_turn_origin();
    let content = crate::ModelCallOriginContent::from_reconstituted_turn_origin(&origin)
        .expect("the canonical reclassified origin has exact accepted content");

    assert_eq!(content.accepted_input(), accepted_input_id(0x73));
    assert_eq!(
        content
            .content()
            .single_text()
            .expect("the fixture has exactly one text part")
            .as_str(),
        "reclassified steering"
    );
}

/// Replays a rejection whose reclassified origin's source turn ended with
/// the given terminal disposition and asserts replay authenticates it.
#[track_caller]
fn assert_terminal_source_authenticates_reclassification(disposition: TurnDisposition) {
    SubmitInputReconstitutionInput::rejected_active_turn_present(
        SubmitInputRejectedActiveTurnPresentReconstitutionInput {
            command: start_command(0x84, "rejected start", 1),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_active_turn: turn_id(8),
            active_turn_origin: reclassified_turn_origin_with_disposition(disposition),
        },
    )
    .reconstitute()
    .expect("every terminal source disposition authenticates reclassification");
}

/// S08: reclassification replay admits every
/// terminal disposition and recursively validates a source turn that was
/// itself created by steering reclassification.
#[test]
fn s08_reclassification_accepts_all_terminal_sources_and_chains() {
    assert_terminal_source_authenticates_reclassification(TurnDisposition::Completed);
    assert_terminal_source_authenticates_reclassification(TurnDisposition::Refused);
    assert_terminal_source_authenticates_reclassification(TurnDisposition::Failed);
    assert_terminal_source_authenticates_reclassification(TurnDisposition::Cancelled {
        cause: test_applied_interrupt_proof(command_id(0x90), turn_id(7)),
    });
    assert_terminal_source_authenticates_reclassification(
        TurnDisposition::ReconciliationRequired {
            marker: test_reconciliation_marker(
                NonEmptyIssuedOperationRefs::try_from_operations([IssuedOperationRef::ModelCall(
                    model_call_id(0x91),
                )])
                .expect("the test ambiguity set is nonempty"),
                ReconciliationReason::InterruptRequiresReconciliation {
                    interrupt: test_applied_interrupt_proof(command_id(0x92), turn_id(7)),
                },
            ),
        },
    );

    let source_origin = reclassified_turn_origin_with_disposition(TurnDisposition::Completed);
    let position = SessionInputPosition::first()
        .checked_next()
        .and_then(SessionInputPosition::checked_next)
        .expect("the chained steering follows its reclassified source");
    let command = SubmitInput::new(
        command_id(0x74),
        session_id(1),
        content("second reclassified steering"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(8),
        },
    );
    let receipt = SubmitInputReconstitutionInput::applied_pending_steering(
        SubmitInputAppliedPendingSteeringReconstitutionInput {
            command: command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_accepted_input: accepted_input_id(0x75),
            result_source_turn: turn_id(8),
            source_turn_origin: source_origin.clone(),
            accepted_command: command.command_id(),
            accepted_input: accepted_input_id(0x75),
            accepted_session: session_id(1),
            accepted_content: content("second reclassified steering"),
            accepted_delivery: command.delivery(),
            accepted_position: position,
        },
    )
    .reconstitute()
    .expect("the second pending-steering receipt has a canonical reclassified source");
    let lifecycle = AcceptedInputLifecycle::new(
        accepted_input_id(0x75),
        AcceptedInputDisposition::PendingSteering {
            binding: SteeringBinding::new(turn_id(8)),
        },
    )
    .reclassify_as_turn_origin(
        turn_id(9),
        crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
    )
    .expect("the second pending steering can be reclassified");
    let chained_origin = SubmitInputTurnOriginReconstitutionInput::reclassified(
        SubmitInputReclassifiedTurnOriginConstructionInput {
            receipt,
            lifecycle,
            queue_accepted_input: accepted_input_id(0x75),
            queue_session: session_id(1),
            queue_turn: turn_id(9),
            queue_order: AcceptedInputQueueOrder::ordinary(position),
            source_terminal: SubmitInputTerminalSourceReconstitutionInput::new(
                SubmitInputTerminalSourceConstructionInput {
                    origin: source_origin,
                    turn: turn_id(8),
                    disposition: TurnDisposition::Refused,
                },
            ),
        },
    );

    SubmitInputReconstitutionInput::rejected_active_turn_present(
        SubmitInputRejectedActiveTurnPresentReconstitutionInput {
            command: start_command(0x85, "second rejected start", 1),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_active_turn: turn_id(9),
            active_turn_origin: chained_origin,
        },
    )
    .reconstitute()
    .expect("a terminal reclassified source authenticates the next reclassified origin");
}

/// Replays a rejection carrying the given cross-wired reclassified origin
/// and asserts the replay fails closed with the origin-mismatch failure.
#[track_caller]
fn assert_cross_wired_reclassified_origin_fails_closed(
    origin: SubmitInputTurnOriginReconstitutionInput,
) {
    assert_eq!(
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(0x84, "rejected start", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(8),
                active_turn_origin: origin,
            }
        )
        .reconstitute()
        .expect_err("cross-wired reclassified origin facts fail closed")
        .failure(),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch
    );
}

/// S08: a pending receipt becomes canonical origin
/// evidence only with its exact reclassified lifecycle, queue facts, and
/// earlier distinct terminal source origin.
#[test]
fn s08_reclassified_turn_origin_rejects_cross_wired_facts() {
    let mut wrong_lifecycle = reclassified_turn_origin();
    turn_origin_facts(&mut wrong_lifecycle).lifecycle = AcceptedInputLifecycle::new(
        accepted_input_id(0x73),
        AcceptedInputDisposition::OriginOf(turn_id(8)),
    );
    assert_cross_wired_reclassified_origin_fails_closed(wrong_lifecycle);

    let mut wrong_input = reclassified_turn_origin();
    turn_origin_facts(&mut wrong_input).lifecycle = AcceptedInputLifecycle::new(
        accepted_input_id(0x74),
        AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
            turn: turn_id(8),
            reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
        },
    );
    assert_cross_wired_reclassified_origin_fails_closed(wrong_input);

    let mut wrong_queue_input = reclassified_turn_origin();
    turn_origin_facts(&mut wrong_queue_input).queue_accepted_input = accepted_input_id(0x74);
    assert_cross_wired_reclassified_origin_fails_closed(wrong_queue_input);

    let mut wrong_turn = reclassified_turn_origin();
    turn_origin_facts(&mut wrong_turn).queue_turn = turn_id(9);
    assert_cross_wired_reclassified_origin_fails_closed(wrong_turn);

    let mut source_turn_reuse = reclassified_turn_origin();
    turn_origin_facts(&mut source_turn_reuse).lifecycle = AcceptedInputLifecycle::new(
        accepted_input_id(0x73),
        AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
            turn: turn_id(7),
            reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
        },
    );
    turn_origin_facts(&mut source_turn_reuse).queue_turn = turn_id(7);
    assert_cross_wired_reclassified_origin_fails_closed(source_turn_reuse);

    let mut wrong_terminal_owner = reclassified_turn_origin();
    let terminal = terminal_source_facts(&mut wrong_terminal_owner);
    terminal.turn = turn_id(9);
    terminal.disposition = TurnDisposition::Completed;
    assert_cross_wired_reclassified_origin_fails_closed(wrong_terminal_owner);

    let mut wrong_terminal_proof = reclassified_turn_origin();
    terminal_source_facts(&mut wrong_terminal_proof).disposition = TurnDisposition::Cancelled {
        cause: test_applied_interrupt_proof(command_id(0x90), turn_id(9)),
    };
    assert_cross_wired_reclassified_origin_fails_closed(wrong_terminal_proof);

    let mut reused_source_command = reclassified_turn_origin();
    replace_source_origin(
        &mut reused_source_command,
        source_turn_origin_with_identities(0x72, 0x71),
    );
    assert_cross_wired_reclassified_origin_fails_closed(reused_source_command);

    let steering_position = SessionInputPosition::first()
        .checked_next()
        .expect("the steering follows its real source");
    let mut late_source = reclassified_turn_origin();
    replace_source_origin(
        &mut late_source,
        source_turn_origin_with_position(0x70, 0x71, steering_position),
    );
    assert_cross_wired_reclassified_origin_fails_closed(late_source);

    let mut wrong_order = reclassified_turn_origin();
    turn_origin_facts(&mut wrong_order).queue_order =
        AcceptedInputQueueOrder::ordinary(SessionInputPosition::first());
    assert_cross_wired_reclassified_origin_fails_closed(wrong_order);
}

/// A coherent reclassification chain grown from the canonical source
/// origin to the given final acceptance position; command and
/// accepted-input seeds derive from each position, decorrelated, and the
/// head turn's seed is its position plus six, the derivation
/// `append_unchecked_reclassified_origin` states.
fn reclassified_origin_chain_ending_at(
    final_position: u64,
) -> SubmitInputTurnOriginReconstitutionInput {
    let mut origin = source_turn_origin();
    for position in 2..=final_position {
        origin = append_unchecked_reclassified_origin(
            origin,
            position,
            0x10_000 + u128::from(position),
            0x20_000 + u128::from(position),
        );
    }
    origin
}

/// S08: validation remains bounded by heap-backed
/// input size rather than call-stack depth.
#[test]
fn s08_reclassified_origin_validation_is_iterative() {
    let origin = reclassified_origin_chain_ending_at(16_384);

    let validated = super::validation::validate_turn_origin_reconstitution_input(&origin)
        .expect("a long coherent origin chain validates without recursion");
    assert_eq!(validated.turn, turn_id(16_390));
}

/// S08: command, accepted-input, and turn identities
/// remain unique across the complete reclassification chain, not only
/// adjacent source/origin pairs.
#[test]
fn s08_reclassified_origin_rejects_ancestor_identity_reuse() {
    let command_reuse = append_unchecked_reclassified_origin(
        append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
        3,
        0x70,
        0x203,
    );
    assert!(super::validation::validate_turn_origin_reconstitution_input(&command_reuse).is_none());

    let accepted_input_reuse = append_unchecked_reclassified_origin(
        append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
        3,
        0x103,
        0x71,
    );
    assert!(
        super::validation::validate_turn_origin_reconstitution_input(&accepted_input_reuse)
            .is_none()
    );

    let mut turn_reuse = append_unchecked_reclassified_origin(
        append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
        3,
        0x103,
        0x203,
    );
    let facts = turn_origin_facts(&mut turn_reuse);
    facts.lifecycle = AcceptedInputLifecycle::new(
        facts.lifecycle.id(),
        AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
            turn: turn_id(7),
            reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
        },
    );
    facts.queue_turn = turn_id(7);
    assert!(super::validation::validate_turn_origin_reconstitution_input(&turn_reuse).is_none());
}

/// Validates a reclassified origin whose source turn ended with the given
/// terminal disposition and asserts the tracked user-global command set
/// contains the proof command the disposition carries.
#[track_caller]
fn assert_terminal_proof_command_is_tracked(
    disposition: TurnDisposition,
    proof_command: crate::DurableCommandId,
) {
    let origin = reclassified_turn_origin_with_disposition(disposition);
    let validated = super::validation::validate_turn_origin_reconstitution_input(&origin)
        .expect("a unique terminal proof command is valid");
    assert!(
        validated.command_ids.contains(&proof_command),
        "the origin chain's command identity set must include terminal proof commands"
    );
}

/// S08: the user-global command identity set includes
/// every command carried by terminal authority in the origin chain.
#[test]
fn s08_reclassified_origin_tracks_terminal_proof_commands() {
    let proof_command = command_id(0x90);
    assert_terminal_proof_command_is_tracked(
        TurnDisposition::Cancelled {
            cause: test_applied_interrupt_proof(proof_command, turn_id(7)),
        },
        proof_command,
    );
    assert_terminal_proof_command_is_tracked(
        TurnDisposition::ReconciliationRequired {
            marker: test_reconciliation_marker(
                NonEmptyIssuedOperationRefs::try_from_operations([IssuedOperationRef::ModelCall(
                    model_call_id(0x91),
                )])
                .expect("the test ambiguity set is nonempty"),
                ReconciliationReason::UserChoseReconciliation {
                    decision: test_applied_stop_for_reconciliation_proof(proof_command, turn_id(7)),
                },
            ),
        },
        proof_command,
    );
    assert_terminal_proof_command_is_tracked(
        TurnDisposition::ReconciliationRequired {
            marker: test_reconciliation_marker(
                NonEmptyIssuedOperationRefs::try_from_operations([IssuedOperationRef::ModelCall(
                    model_call_id(0x92),
                )])
                .expect("the test ambiguity set is nonempty"),
                ReconciliationReason::InterruptRequiresReconciliation {
                    interrupt: test_applied_interrupt_proof(proof_command, turn_id(7)),
                },
            ),
        },
        proof_command,
    );
    assert_terminal_proof_command_is_tracked(
        TurnDisposition::ReconciliationRequired {
            marker: test_reconciliation_marker(
                NonEmptyIssuedOperationRefs::try_from_operations([IssuedOperationRef::ModelCall(
                    model_call_id(0x93),
                )])
                .expect("the test ambiguity set is nonempty"),
                ReconciliationReason::FatalMismatchRequiresReconciliation {
                    causes: test_fatal_mismatch_stop_causes(
                        provider_target_evidence_id(0x94),
                        crate::AppliedInterruptState::Applied {
                            proof: test_applied_interrupt_proof(proof_command, turn_id(7)),
                        },
                    ),
                },
            ),
        },
        proof_command,
    );

    let colliding_disposition = TurnDisposition::Cancelled {
        cause: test_applied_interrupt_proof(command_id(0x72), turn_id(7)),
    };
    assert!(
        super::validation::validate_turn_origin_reconstitution_input(
            &reclassified_turn_origin_with_disposition(colliding_disposition)
        )
        .is_none(),
        "terminal proof commands cannot reuse a receipt command"
    );

    let replay_command = 0x90;
    let rejection = SubmitInputReconstitutionInput::rejected_active_turn_present(
        SubmitInputRejectedActiveTurnPresentReconstitutionInput {
            command: start_command(replay_command, "rejected start", 1),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_active_turn: turn_id(8),
            active_turn_origin: reclassified_turn_origin_with_disposition(
                TurnDisposition::Cancelled {
                    cause: test_applied_interrupt_proof(command_id(replay_command), turn_id(7)),
                },
            ),
        },
    );
    assert_eq!(
        rejection
            .reconstitute()
            .expect_err("the replay command cannot reuse terminal authority")
            .failure(),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused
    );
}

/// S09: after-current replay carries the active predecessor's
/// canonical origin and must follow it in session acceptance order.
#[test]
fn s09_after_reconstitution_requires_predecessor_chronology() {
    let mut missing_predecessor = after_applied_input();
    applied_facts(&mut missing_predecessor).predecessor_origin = None;
    assert_eq!(
        missing_predecessor
            .reconstitute()
            .expect_err("after-current replay requires its predecessor origin")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
    );

    let mut premature = after_applied_input();
    let premature_facts = applied_facts(&mut premature);
    premature_facts.accepted_position = SessionInputPosition::first();
    premature_facts.queue_order = AcceptedInputQueueOrder::ordinary(SessionInputPosition::first());
    assert_eq!(
        premature
            .reconstitute()
            .expect_err("after-current acceptance must follow its predecessor origin")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentAcceptanceDoesNotFollowPredecessorOrigin
    );

    let mut unexpected_predecessor = applied_input();
    applied_facts(&mut unexpected_predecessor).predecessor_origin = Some(source_turn_origin());
    assert_eq!(
        unexpected_predecessor
            .reconstitute()
            .expect_err("vacant-slot start replay has no active predecessor")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
    );
}

/// S32: an interrupt origin may follow the exact terminal
/// delegated predecessor even though that turn has no accepted input.
#[test]
fn s32_interrupt_reconstitution_admits_exact_non_accepted_predecessor() {
    let input = interrupt_applied_input_with_non_accepted_predecessor(session_id(1), turn_id(7));

    input
        .reconstitute()
        .expect("the exact non-accepted interrupt predecessor is admitted");
}

/// S32: non-accepted predecessor evidence remains scoped to the
/// command's exact session.
#[test]
fn s32_interrupt_reconstitution_rejects_cross_session_non_accepted_predecessor() {
    let input = interrupt_applied_input_with_non_accepted_predecessor(session_id(2), turn_id(7));

    assert_eq!(
        input
            .reconstitute()
            .expect_err("a non-accepted predecessor from another session is unrelated")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
    );
}

/// S32: non-accepted predecessor evidence must name the exact
/// turn targeted by the interrupt command.
#[test]
fn s32_interrupt_reconstitution_rejects_cross_wired_non_accepted_predecessor() {
    let input = interrupt_applied_input_with_non_accepted_predecessor(session_id(1), turn_id(6));

    assert_eq!(
        input
            .reconstitute()
            .expect_err("a different non-accepted predecessor cannot authorize the interrupt")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
    );
}

/// S32: non-accepted predecessor evidence cannot weaken the
/// accepted-origin chronology required by after-current delivery.
#[test]
fn s32_after_current_rejects_non_accepted_predecessor() {
    let mut input = after_applied_input();
    let facts = applied_facts(&mut input);
    facts.predecessor_origin = None;
    facts.non_accepted_predecessor = Some(NonAcceptedTurnPredecessorReconstitutionInput {
        session: session_id(1),
        turn: turn_id(7),
    });

    assert_eq!(
        input
            .reconstitute()
            .expect_err("after-current replay requires an accepted-input predecessor")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
    );
}

/// S09: after-current replay cannot reuse any identity from its
/// active predecessor origin.
#[test]
fn s09_after_reconstitution_rejects_predecessor_identity_reuse() {
    let mut turn_reuse = after_applied_input();
    let facts = applied_facts(&mut turn_reuse);
    facts.result_turn = turn_id(7);
    facts.accepted_disposition = AcceptedInputDisposition::OriginOf(turn_id(7));
    facts.queue_turn = turn_id(7);
    assert_eq!(
        turn_reuse
            .reconstitute()
            .expect_err("after-current work cannot reuse its active predecessor turn")
            .failure(),
        SubmitInputReconstitutionFailure::QueueTurnMismatch
    );

    let mut accepted_input_reuse = after_applied_input();
    applied_facts(&mut accepted_input_reuse).predecessor_origin =
        Some(source_turn_origin_with_identities(0x70, 3));
    assert_eq!(
        accepted_input_reuse
            .reconstitute()
            .expect_err("after-current work cannot reuse its predecessor input")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorAcceptedInputReused
    );

    let mut command_reuse = after_applied_input();
    applied_facts(&mut command_reuse).predecessor_origin =
        Some(source_turn_origin_with_identities(1, 0x71));
    assert_eq!(
        command_reuse
            .reconstitute()
            .expect_err("after-current work cannot reuse its predecessor command")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorCommandReused
    );

    assert_eq!(
        after_applied_input_with_chained_predecessor(1, 0x71, turn_id(9))
            .reconstitute()
            .expect_err("after-current work cannot reuse an ancestor input")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorAcceptedInputReused
    );
    assert_eq!(
        after_applied_input_with_chained_predecessor(0x70, 3, turn_id(9))
            .reconstitute()
            .expect_err("after-current work cannot reuse an ancestor command")
            .failure(),
        SubmitInputReconstitutionFailure::AfterCurrentPredecessorCommandReused
    );
    assert_eq!(
        after_applied_input_with_chained_predecessor(1, 3, turn_id(7))
            .reconstitute()
            .expect_err("after-current work cannot reuse an ancestor turn")
            .failure(),
        SubmitInputReconstitutionFailure::QueueTurnMismatch
    );
}

/// S08: pending-steering replay cannot reuse either user-global
/// identity from its canonical source origin.
#[test]
fn s08_pending_steering_rejects_source_identity_reuse() {
    let mut accepted_input_reuse = pending_steering_input();
    pending_facts(&mut accepted_input_reuse).source_turn_origin =
        source_turn_origin_with_identities(0x70, 3);
    assert_eq!(
        accepted_input_reuse
            .reconstitute()
            .expect_err("pending steering cannot reuse its source input")
            .failure(),
        SubmitInputReconstitutionFailure::SteeringSourceAcceptedInputReused
    );

    let mut command_reuse = pending_steering_input();
    pending_facts(&mut command_reuse).source_turn_origin =
        source_turn_origin_with_identities(1, 0x71);
    assert_eq!(
        command_reuse
            .reconstitute()
            .expect_err("pending steering cannot reuse its source command")
            .failure(),
        SubmitInputReconstitutionFailure::SteeringSourceCommandReused
    );

    assert_eq!(
        pending_steering_input_with_chained_source(1, 0x71)
            .reconstitute()
            .expect_err("pending steering cannot reuse an ancestor input")
            .failure(),
        SubmitInputReconstitutionFailure::SteeringSourceAcceptedInputReused
    );
    assert_eq!(
        pending_steering_input_with_chained_source(0x70, 3)
            .reconstitute()
            .expect_err("pending steering cannot reuse an ancestor command")
            .failure(),
        SubmitInputReconstitutionFailure::SteeringSourceCommandReused
    );
}

/// Applies one cross-wiring mutation to the canonical pending-steering
/// projection and asserts the exact closed failure it must produce; the
/// mutation and expected failure stay at the call site.
#[track_caller]
fn assert_pending_steering_fact_fails_closed(
    cross_wire: impl FnOnce(&mut SubmitInputReconstitutionInput),
    expected: SubmitInputReconstitutionFailure,
) {
    let mut wrong = pending_steering_input();
    cross_wire(&mut wrong);
    assert_eq!(
        wrong
            .reconstitute()
            .expect_err("one cross-wired pending-steering fact fails closed")
            .failure(),
        expected
    );
}

/// S08: every independent pending-steering fact is
/// checked before the immutable receipt is reconstructed.
#[test]
fn pending_steering_reconstitution_rejects_cross_wired_facts() {
    assert_pending_steering_fact_fails_closed(
        |input| input.command = start_command(1, "hello", 1),
        SubmitInputReconstitutionFailure::AppliedDeliveryIsNotNextSafePoint,
    );
    assert_pending_steering_fact_fails_closed(
        |input| input.stored_actor = Actor::Recovery,
        SubmitInputReconstitutionFailure::StoredActorMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| pending_facts(input).result_session = session_id(2),
        SubmitInputReconstitutionFailure::ResultSessionMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| pending_facts(input).result_source_turn = turn_id(9),
        SubmitInputReconstitutionFailure::SteeringSourceTurnMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| pending_facts(input).accepted_command = command_id(2),
        SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| pending_facts(input).accepted_input = accepted_input_id(9),
        SubmitInputReconstitutionFailure::AcceptedInputMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| pending_facts(input).accepted_session = session_id(2),
        SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| pending_facts(input).accepted_content = content("different"),
        SubmitInputReconstitutionFailure::AcceptedContentMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| {
            pending_facts(input).accepted_delivery = DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(9),
            };
        },
        SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
    );
    assert_pending_steering_fact_fails_closed(
        |input| pending_facts(input).accepted_position = SessionInputPosition::first(),
        SubmitInputReconstitutionFailure::SteeringAcceptanceDoesNotFollowSourceOrigin,
    );

    let mut wrong_source_origin = pending_steering_input();
    pending_facts(&mut wrong_source_origin).source_turn_origin = explicit_turn_origin_input(
        after_applied_input()
            .reconstitute()
            .expect("the cross-wired origin is independently canonical"),
    );
    assert_eq!(
        wrong_source_origin
            .reconstitute()
            .expect_err("the source receipt must establish the exact source turn")
            .failure(),
        SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch
    );
}

/// Applies one cross-wiring mutation to the canonical applied projection
/// and asserts the exact closed failure it must produce; the mutation and
/// expected failure stay at the call site.
#[track_caller]
fn assert_applied_fact_fails_closed(
    cross_wire: impl FnOnce(&mut SubmitInputReconstitutionInput),
    expected: SubmitInputReconstitutionFailure,
) {
    let mut wrong = applied_input();
    cross_wire(&mut wrong);
    assert_eq!(
        wrong
            .reconstitute()
            .expect_err("one cross-wired applied fact fails closed")
            .failure(),
        expected
    );
}

/// every applied-path reconstitution failure variant
/// is reachable from exactly one cross-wired fact and fails closed
/// instead of constructing authority.
#[test]
fn applied_reconstitution_rejects_every_cross_wired_fact() {
    assert_applied_fact_fails_closed(
        |input| input.stored_actor = Actor::Recovery,
        SubmitInputReconstitutionFailure::StoredActorMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| {
            input.command = SubmitInput::new(
                command_id(1),
                session_id(1),
                content("hello"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(9),
                },
            );
        },
        SubmitInputReconstitutionFailure::AppliedDeliveryIsNotTurnOrigin,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).result_session = session_id(2),
        SubmitInputReconstitutionFailure::ResultSessionMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).accepted_command = command_id(2),
        SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).accepted_input = accepted_input_id(9),
        SubmitInputReconstitutionFailure::AcceptedInputMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).accepted_session = session_id(2),
        SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).accepted_content = content("different"),
        SubmitInputReconstitutionFailure::AcceptedContentMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| {
            applied_facts(input).accepted_delivery = DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(2, ModelSelectionOverride::UseSessionDefault),
            };
        },
        SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| {
            applied_facts(input).accepted_disposition =
                AcceptedInputDisposition::OriginOf(turn_id(9));
        },
        SubmitInputReconstitutionFailure::AcceptedDispositionMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).queue_session = session_id(2),
        SubmitInputReconstitutionFailure::QueueSessionMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).queue_turn = turn_id(9),
        SubmitInputReconstitutionFailure::QueueTurnMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| {
            applied_facts(input).accepted_position = SessionInputPosition::first()
                .checked_next()
                .expect("the second position exists");
        },
        SubmitInputReconstitutionFailure::QueuePositionMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| {
            applied_facts(input).queue_order =
                crate::AcceptedInputQueueOrder::interrupt_immediately_after(
                    SessionInputPosition::first(),
                    turn_id(9),
                );
        },
        SubmitInputReconstitutionFailure::QueuePriorityMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).defaults_session = session_id(2),
        SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| applied_facts(input).defaults_version = version(2),
        SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| {
            applied_facts(input).stored_requested_model = ModelSelectionRequest::Direct(direct(9));
        },
        SubmitInputReconstitutionFailure::RequestedModelMismatch,
    );
    assert_applied_fact_fails_closed(
        |input| {
            applied_facts(input).stored_frozen_model = FrozenModelSelection::Direct(direct(9));
        },
        SubmitInputReconstitutionFailure::FrozenModelMismatch,
    );
}

/// each rejected receipt reconstructs only from a matching
/// command-specific typed projection.
#[test]
fn rejected_reconstitution_is_checked() {
    let command = start_command(1, "hello", 1);
    let ReconstitutedSubmitInput { .. } =
        SubmitInputReconstitutionInput::rejected_session_not_found(
            SubmitInputRejectedSessionNotFoundReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
            },
        )
        .reconstitute()
        .expect("matching missing-session facts reconstruct");

    assert_eq!(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(2),
                result_current: version(3),
                active_turn_origin: None,
            }
        )
        .reconstitute()
        .expect_err("a different expected version fails closed")
        .failure(),
        SubmitInputReconstitutionFailure::ExpectedDefaultsVersionMismatch
    );
}

/// the baseline rejected-result projections fail closed for
/// independently cross-wired actor, session, delivery, configuration,
/// alias, and position facts.
#[test]
fn rejected_reconstitution_rejects_every_cross_wired_fact() {
    let start = start_command(1, "hello", 1);
    let safe_point = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(7),
        },
    );
    let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");

    assert_eq!(
        SubmitInputReconstitutionInput::rejected_session_not_found(
            SubmitInputRejectedSessionNotFoundReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::Recovery,
                result_session: session_id(1),
            }
        )
        .reconstitute()
        .expect_err("a stored non-user actor fails closed")
        .failure(),
        SubmitInputReconstitutionFailure::StoredActorMismatch
    );

    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_session_not_found(
            SubmitInputRejectedSessionNotFoundReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::User,
                result_session: session_id(2),
            },
        ),
        SubmitInputReconstitutionFailure::ResultSessionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_no_active_turn(
            SubmitInputRejectedNoActiveTurnReconstitutionInput {
                command: safe_point.clone(),
                stored_actor: Actor::User,
                result_session: session_id(2),
                result_expected_active_turn: turn_id(7),
            },
        ),
        SubmitInputReconstitutionFailure::ResultSessionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::User,
                result_session: session_id(2),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: None,
            },
        ),
        SubmitInputReconstitutionFailure::ResultSessionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::User,
                result_session: session_id(2),
                result_alias: alias(3),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                active_turn_origin: None,
            },
        ),
        SubmitInputReconstitutionFailure::ResultSessionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::User,
                result_session: session_id(2),
                result_last_position: maximum,
                active_turn_origin: None,
            },
        ),
        SubmitInputReconstitutionFailure::ResultSessionMismatch,
    );

    SubmitInputReconstitutionInput::rejected_no_active_turn(
        SubmitInputRejectedNoActiveTurnReconstitutionInput {
            command: safe_point.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_expected_active_turn: turn_id(7),
        },
    )
    .reconstitute()
    .expect("the matching expected turn reconstructs");
    assert_eq!(
        SubmitInputReconstitutionInput::rejected_no_active_turn(
            SubmitInputRejectedNoActiveTurnReconstitutionInput {
                command: safe_point.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected_active_turn: turn_id(8),
            }
        )
        .reconstitute()
        .expect_err("another expected turn fails closed")
        .failure(),
        SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch
    );

    assert_eq!(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: safe_point,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: None,
            }
        )
        .reconstitute()
        .expect_err("a non-start delivery fails closed")
        .failure(),
        SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration
    );
    assert_eq!(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(1),
                active_turn_origin: None,
            }
        )
        .reconstitute()
        .expect_err("equal versions are not a mismatch")
        .failure(),
        SubmitInputReconstitutionFailure::RejectedDefaultsVersionsAreEqual
    );

    let alias_command = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(
                1,
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
            ),
        },
    );
    SubmitInputReconstitutionInput::rejected_unknown_model_alias(
        SubmitInputRejectedUnknownModelAliasReconstitutionInput {
            command: alias_command.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_alias: alias(2),
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
            active_turn_origin: None,
        },
    )
    .reconstitute()
    .expect("the matching unresolved alias reconstructs");
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: alias_command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(2),
                defaults_session: session_id(2),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                active_turn_origin: None,
            },
        ),
        SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: alias_command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(2),
                defaults_session: session_id(1),
                defaults_version: version(2),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                active_turn_origin: None,
            },
        ),
        SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: alias_command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(3),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                active_turn_origin: None,
            },
        ),
        SubmitInputReconstitutionFailure::UnknownAliasMismatch,
    );
    assert_eq!(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(3),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                active_turn_origin: None,
            }
        )
        .reconstitute()
        .expect_err("a direct-selecting request cannot record an unknown alias")
        .failure(),
        SubmitInputReconstitutionFailure::RejectionDidNotSelectAlias
    );

    SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
        SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
            command: start.clone(),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_last_position: maximum,
            active_turn_origin: None,
        },
    )
    .reconstitute()
    .expect("the exhausted maximum position reconstructs");
    assert_eq!(
        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                command: start,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_last_position: SessionInputPosition::first(),
                active_turn_origin: None,
            }
        )
        .reconstitute()
        .expect_err("a position with a successor is not exhausted")
        .failure(),
        SubmitInputReconstitutionFailure::PositionIsNotExhausted
    );
}

/// S01 / S08 / S09: every rejection that records an
/// authoritative active turn carries that turn's exact canonical origin.
#[test]
fn active_state_rejections_reconstruct_from_canonical_origins() {
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(1, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(7),
                active_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputRejectedResult::ActiveTurnPresent {
            session: session_id(1),
            active_turn: turn_id(7),
        },
    );

    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
            SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                command: interrupt_command(1, turn_id(9)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected_active_turn: turn_id(9),
                result_actual_active_turn: turn_id(7),
                actual_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputRejectedResult::ActiveTurnMismatch {
            session: session_id(1),
            expected_active_turn: turn_id(9),
            actual_active_turn: turn_id(7),
        },
    );
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
            SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                command: safe_point_command(1, turn_id(9)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected_active_turn: turn_id(9),
                result_actual_active_turn: turn_id(7),
                actual_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputRejectedResult::ActiveTurnMismatch {
            session: session_id(1),
            expected_active_turn: turn_id(9),
            actual_active_turn: turn_id(7),
        },
    );
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
            SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                command: after_command(1, turn_id(9)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected_active_turn: turn_id(9),
                result_actual_active_turn: turn_id(7),
                actual_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputRejectedResult::ActiveTurnMismatch {
            session: session_id(1),
            expected_active_turn: turn_id(9),
            actual_active_turn: turn_id(7),
        },
    );
}

/// S01 / S08 / S09: configuration and position
/// rejections reconstruct only for delivery modes that can record them,
/// with occupied modes carrying their exact active origin.
#[test]
fn configuration_and_position_rejections_follow_delivery() {
    let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: start_command(1, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: None,
            },
        ),
        SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
            session: session_id(1),
            expected: version(1),
            current: version(2),
        },
    );
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: after_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: Some(source_turn_origin()),
            },
        ),
        SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
            session: session_id(1),
            expected: version(1),
            current: version(2),
        },
    );

    let start_alias = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: choices(
                1,
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
            ),
        },
    );
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: start_alias,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(2),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
                active_turn_origin: None,
            },
        ),
        SubmitInputRejectedResult::UnknownModelAlias {
            session: session_id(1),
            alias: alias(2),
        },
    );

    let after_alias = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: turn_id(7),
            configuration: choices(
                1,
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
            ),
        },
    );
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: after_alias,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(2),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
                active_turn_origin: Some(source_turn_origin()),
            },
        ),
        SubmitInputRejectedResult::UnknownModelAlias {
            session: session_id(1),
            alias: alias(2),
        },
    );

    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                command: start_command(1, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_last_position: maximum,
                active_turn_origin: None,
            },
        ),
        SubmitInputRejectedResult::AcceptancePositionExhausted {
            session: session_id(1),
            last: maximum,
        },
    );
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                command: safe_point_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_last_position: maximum,
                active_turn_origin: Some(source_turn_origin()),
            },
        ),
        SubmitInputRejectedResult::AcceptancePositionExhausted {
            session: session_id(1),
            last: maximum,
        },
    );
    assert_reconstitutes_rejection(
        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                command: after_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_last_position: maximum,
                active_turn_origin: Some(source_turn_origin()),
            },
        ),
        SubmitInputRejectedResult::AcceptancePositionExhausted {
            session: session_id(1),
            last: maximum,
        },
    );
}

/// S08 / S09: rejection replay fails closed when required
/// active-origin evidence is omitted, extra, cross-wired, or command-ID
/// aliased.
#[test]
fn rejected_active_origin_evidence_is_exact() {
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: after_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: None,
            },
        ),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: start_command(1, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: Some(source_turn_origin()),
            },
        ),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
    );

    let wrong_turn_origin = explicit_turn_origin_input(
        applied_input()
            .reconstitute()
            .expect("the independent turn-four origin is canonical"),
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(1, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(7),
                active_turn_origin: wrong_turn_origin,
            },
        ),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
    );

    let steering_receipt = pending_steering_input()
        .reconstitute()
        .expect("the independent pending-steering receipt is canonical");
    let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(steering)) =
        steering_receipt.result()
    else {
        panic!("the receipt remains pending steering");
    };
    let invalid_origin = SubmitInputTurnOriginReconstitutionInput::new(
        SubmitInputDirectTurnOriginConstructionInput {
            receipt: steering_receipt.clone(),
            lifecycle: AcceptedInputLifecycle::new(
                steering.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: steering.binding(),
                },
            ),
            queue_accepted_input: steering.accepted_input(),
            queue_session: steering.session(),
            queue_turn: turn_id(7),
            queue_order: AcceptedInputQueueOrder::ordinary(steering.acceptance_position()),
        },
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(1, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(7),
                active_turn_origin: invalid_origin,
            },
        ),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
    );

    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(1, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(7),
                active_turn_origin: source_turn_origin_with_identities(1, 0x71),
            },
        ),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
    );

    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(0x70, "hello", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(8),
                active_turn_origin: append_unchecked_reclassified_origin(
                    source_turn_origin(),
                    2,
                    0x102,
                    0x202,
                ),
            },
        ),
        SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
    );
}

/// S01 / S08 / S09: state-carrying rejection replay
/// validates the delivery discriminator and both expected/actual turns.
#[test]
fn state_rejections_validate_delivery_and_turns() {
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: safe_point_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(7),
                active_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputReconstitutionFailure::ActiveTurnPresentRejectionMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
            SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                command: after_command(1, turn_id(9)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected_active_turn: turn_id(8),
                result_actual_active_turn: turn_id(7),
                actual_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch,
    );
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
            SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                command: after_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected_active_turn: turn_id(7),
                result_actual_active_turn: turn_id(7),
                actual_turn_origin: source_turn_origin(),
            },
        ),
        SubmitInputReconstitutionFailure::RejectedActiveTurnsAreEqual,
    );
}

/// S07 / S08: interrupt replay admits the same
/// configuration and position rejections as preparation, while a
/// safe-point request still carries no configurable model choice.
#[test]
fn interrupt_rejections_reconstitute_exactly() {
    SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
        SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
            command: interrupt_command(1, turn_id(7)),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_expected: version(1),
            result_current: version(2),
            active_turn_origin: Some(source_turn_origin()),
        },
    )
    .reconstitute()
    .expect("an interrupt defaults-version rejection reconstructs");
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: safe_point_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: Some(source_turn_origin()),
            },
        ),
        SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
    );

    let interrupt_alias = SubmitInput::new(
        command_id(1),
        session_id(1),
        content("hello"),
        DeliveryRequest::Interrupt {
            expected_active_turn: turn_id(7),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: choices(
                1,
                ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
            ),
        },
    );
    SubmitInputReconstitutionInput::rejected_unknown_model_alias(
        SubmitInputRejectedUnknownModelAliasReconstitutionInput {
            command: interrupt_alias,
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_alias: alias(2),
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
            active_turn_origin: Some(source_turn_origin()),
        },
    )
    .reconstitute()
    .expect("an interrupt unknown-alias rejection reconstructs");

    let safe_point = safe_point_command(1, turn_id(7));
    assert_rejection_reconstitution_fails(
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: safe_point,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(2),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
                active_turn_origin: Some(source_turn_origin()),
            },
        ),
        SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
    );

    let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
    SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
        SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
            command: interrupt_command(1, turn_id(7)),
            stored_actor: Actor::User,
            result_session: session_id(1),
            result_last_position: maximum,
            active_turn_origin: Some(source_turn_origin()),
        },
    )
    .reconstitute()
    .expect("an interrupt position-exhaustion rejection reconstructs");
}

/// S01: preparation against another command's session is a
/// nonterminal correlation failure retaining the unchanged command.
#[test]
fn s01_preparation_rejects_a_cross_wired_session() {
    let command = start_command(1, "hello", 1);
    let error = command
        .clone()
        .prepare_when_no_active_turn(
            &session(2, 1, ModelSelectionRequest::Direct(direct(2))),
            accepted_input_id(3),
            Some(turn_id(4)),
            None,
            |_| None,
        )
        .expect_err("another session is an adapter correlation failure");
    assert_eq!(
        error.failure(),
        SubmitInputPreparationFailure::SessionMismatch {
            provided_session: session_id(2),
        }
    );
    assert_eq!(error.command(), &command);
}
