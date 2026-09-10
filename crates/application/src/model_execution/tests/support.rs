use super::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
    AcceptedInputTurnActivationIdentities, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, ActiveTurnSchedulingReconstitutionInput, Actor, Arc,
    AssistantResponsePart, AssistantText, AttachmentPreparationFailure, AuthorizeModelCallOutcome,
    AuthorizeModelCallTransaction, AuthorizedModelCall, ClassifyOperatorFailure,
    CommitModelCallObservationTransaction, ContextFrontierId,
    CorrelatedModelCallTerminalObservation, DangerousToolAutoApproval, DecideToolRequest,
    DeliveryRequest, DirectModelSelection, DurableCommandId, Error,
    FailPreparedModelCallTransaction, FailedModelCallTurn, FailedModelCallTurnIdentities,
    FrozenModelSelection, Future, InProcessAttemptDispatchGate, InitialToolApproval,
    ModelCallAuthorizationReread, ModelCallCapabilityPreparation, ModelCallCredentialReference,
    ModelCallDisposition, ModelCallExecutionIdGenerator, ModelCallExecutionReconstitutionInput,
    ModelCallExecutionService, ModelCallId, ModelCallObservationCommitOutcome,
    ModelCallOriginContent, ModelCallProvider, ModelCallReconstitutionInput,
    ModelCallReconstitutionState, ModelCallTerminalIdentityCandidates,
    ModelCallTerminalObservation, ModelConversationMessage, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, ModelToolResultContent,
    ModelUserContent, ModelUserContentPart, NonZeroU64, NormalizedToolArguments,
    OperatorFailureClass, PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput,
    PrepareModelCallOutcome, PrepareModelCallTransaction, PreparedModelCallFailureCause,
    PreparedModelCallRequest, PreparedModelOperation, ProviderModelIdentity,
    ResolvedContextFrontierReconstitutionInput, ResolvedProviderTarget,
    ResolvedToolConversationEntry, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef,
    SessionAcceptanceTailEntryReconstitutionInput, SessionAcceptanceTailReconstitutionInput,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionInputPosition, SessionReconstitutionInput,
    StdMutex, SubmitInput, SubmitInputAppliedTurnOriginReconstitutionInput,
    SubmitInputDirectTurnOriginConstructionInput, SubmitInputReconstitutionInput,
    SubmitInputTurnOriginReconstitutionInput, ToolApprovalDecision,
    ToolApprovalResolutionReconstitutionInput, ToolDenialReason, ToolName, ToolRequest,
    ToolRequestId, ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolResultContent,
    TranscriptAncestry, TurnAttemptId, TurnId, UserContent, UserContentPart, Uuid, VecDeque, Write,
    fmt, io, projected_frontier_content_bytes, render_model_user_content,
};

#[derive(Clone, Default)]
pub(super) struct CapturedTelemetry(Arc<StdMutex<Vec<u8>>>);

pub(super) struct CapturedTelemetryWriter(Arc<StdMutex<Vec<u8>>>);

impl Write for CapturedTelemetryWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("captured telemetry remains available")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
    type Writer = CapturedTelemetryWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        CapturedTelemetryWriter(Arc::clone(&self.0))
    }
}

impl CapturedTelemetry {
    pub(super) fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .expect("captured telemetry remains available")
                .clone(),
        )
        .expect("captured telemetry is UTF-8")
    }
}

pub(super) fn identity<Identity>(
    value: u128,
    from_uuid: impl FnOnce(Uuid) -> Identity,
) -> Identity {
    from_uuid(Uuid::from_u128(value))
}

pub(super) fn credential_reference() -> ModelCallCredentialReference {
    ModelCallCredentialReference::new("fixture-provider-primary")
}

pub(super) fn rendered_text(content: UserContent) -> ModelUserContent {
    render_model_user_content(
        identity(1, SemanticTranscriptEntryId::from_uuid),
        content,
        |_| None,
    )
    .expect("text-only fixture needs no attachment catalog facts")
}

pub(super) fn ready(request: PreparedModelCallRequest) -> PrepareModelCallOutcome {
    PrepareModelCallOutcome::Ready {
        retained_mapped_target: None,
        invocation_capacity_reserved: false,
        reasoning_provenance: Box::new([]),
        request: Box::new(request),
        credential_reference: credential_reference(),
        dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
        recorded_user_overrides: Box::new([]),
        system_prompt: None,
        tool_entries: Box::new([]),
    }
}

/// The same reload outcome as [`ready`], carrying the exact durable
/// authority for every tool-related entry the request's frontier names.
pub(super) fn ready_with_tool_evidence(
    request: PreparedModelCallRequest,
    tool_entries: Box<[ResolvedToolConversationEntry]>,
) -> PrepareModelCallOutcome {
    PrepareModelCallOutcome::Ready {
        retained_mapped_target: None,
        invocation_capacity_reserved: false,
        reasoning_provenance: Box::new([]),
        request: Box::new(request),
        credential_reference: credential_reference(),
        dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
        recorded_user_overrides: Box::new([]),
        system_prompt: None,
        tool_entries,
    }
}

pub(super) fn tool_response() -> ModelCallTerminalObservation {
    let arguments =
        signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are valid");
    let parts = vec![
        AssistantResponsePart::Text(
            AssistantText::try_new(String::from("checking"))
                .expect("fixture assistant text is valid"),
        ),
        AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
            signalbox_domain::ToolName::try_new(String::from("automatic"))
                .expect("fixture tool name is valid"),
            arguments.clone(),
        )),
        AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
            signalbox_domain::ToolName::try_new(String::from("unknown"))
                .expect("fixture tool name is valid"),
            arguments,
        )),
    ];
    ModelCallTerminalObservation::CompletedWithTools {
        response: signalbox_domain::ToolUsingAssistantResponse::try_from_parts(parts)
            .expect("fixture response contains tools"),
        retained_input_tokens: None,
        retained_output_tokens: None,
    }
}

/// One request in the canonical model-rendering session, turn, and call.
///
/// The request identity derives from the ordinal and is deliberately in a
/// different UUID range so an implementation cannot confuse the two.
pub(super) fn model_tool_request(ordinal: u32) -> ToolRequest {
    ToolRequestReconstitutionInput::new(
        identity(100 + u128::from(ordinal), ToolRequestId::from_uuid),
        identity(1, SessionId::from_uuid),
        identity(2, TurnId::from_uuid),
        identity(3, ModelCallId::from_uuid),
        ToolRequestOrdinal::from_u32(ordinal),
        ToolName::try_new(format!("tool_{ordinal}")).expect("fixture tool name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are valid"),
    )
    .into_request()
}

pub(super) fn model_tool_use_message(
    request_identity: u128,
    turn_identity: u128,
    call_identity: u128,
    ordinal: u32,
) -> ModelConversationMessage {
    let session = identity(1, SessionId::from_uuid);
    let turn = identity(turn_identity, TurnId::from_uuid);
    let producing_call = identity(call_identity, ModelCallId::from_uuid);
    let request = ToolRequestReconstitutionInput::new(
        identity(request_identity, ToolRequestId::from_uuid),
        session,
        turn,
        producing_call,
        ToolRequestOrdinal::from_u32(ordinal),
        ToolName::try_new(String::from("known")).expect("fixture tool name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are valid"),
    )
    .into_request();
    ModelConversationMessage::AssistantToolUse {
        source: SemanticTranscriptEntryRef::from_source(
            session,
            identity(
                request_identity + 10_000,
                SemanticTranscriptEntryId::from_uuid,
            ),
        ),
        producing_call,
        request,
    }
}

pub(super) fn prepared_fixture() -> (PreparedModelCallRequest, AuthorizedModelCall) {
    let prepared_execution = prepared_execution_fixture();
    let request = prepared_execution
        .resume_prepared_call()
        .expect("fixture Prepared request resumes");
    let authorized = prepared_execution
        .authorize_send()
        .expect("fixture Prepared call authorizes");
    (request, authorized)
}

/// The terminal failed turn a capability-failure commit records for the
/// fixture's prepared call.
pub(super) fn failed_turn_fixture() -> FailedModelCallTurn {
    prepared_execution_fixture()
        .fail_prepared_call(FailedModelCallTurnIdentities::new(
            identity(120, SemanticTranscriptEntryId::from_uuid),
            identity(121, ContextFrontierId::from_uuid),
        ))
        .expect("a prepared fixture call closes as a failed turn")
}

pub(super) fn prepared_execution_fixture() -> signalbox_domain::ModelCallExecution {
    prepared_execution_with_content_fixture(
        UserContent::try_text(String::from("exact user request"))
            .expect("fixture content is valid"),
    )
}

pub(super) fn prepared_execution_with_content_fixture(
    content: UserContent,
) -> signalbox_domain::ModelCallExecution {
    // Attachment fixtures use the widest byte-length spelling for exact stub accounting.
    let attachment_blob_facts = content
        .parts()
        .iter()
        .filter_map(|part| match part {
            UserContentPart::Attachment { digest, .. } => Some(
                signalbox_domain::AttachmentBlobFact::new(*digest, NonZeroU64::MAX),
            ),
            UserContentPart::Text { .. } => None,
        })
        .collect::<Vec<_>>();
    let session_id = identity(1, SessionId::from_uuid);
    let direct = identity(2, DirectModelSelection::from_uuid);
    let accepted_input = identity(3, AcceptedInputId::from_uuid);
    let turn_id = identity(4, TurnId::from_uuid);
    let command_id = identity(5, DurableCommandId::from_uuid);
    let version = SessionConfigurationDefaultsVersion::first();
    let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct));
    let session = SessionReconstitutionInput::new(
        session_id,
        session_id,
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        session_id,
        version,
        session_id,
        version,
        defaults.clone(),
        signalbox_domain::SessionPlacementReconstitutionFacts {
            current_pointer_session: session_id,
            current_pointer_version: signalbox_domain::SessionPlacementVersion::INITIAL,
            selected_event_session: session_id,
            selected_event: signalbox_domain::VersionedSessionPlacement::initial(
                signalbox_domain::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .expect("fixture Session facts are correlated");
    let choices =
        PerInputConfigurationChoices::new(version, ModelSelectionOverride::UseSessionDefault);
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: choices,
    };
    let command = SubmitInput::new(command_id, session_id, content.clone(), delivery);
    let position = SessionInputPosition::first();
    let order = AcceptedInputQueueOrder::ordinary(position);
    let lifecycle =
        AcceptedInputLifecycle::new(accepted_input, AcceptedInputDisposition::OriginOf(turn_id));
    let receipt = SubmitInputReconstitutionInput::applied_turn_origin(
        SubmitInputAppliedTurnOriginReconstitutionInput {
            command,
            stored_actor: Actor::User,
            result_session: session_id,
            result_accepted_input: accepted_input,
            result_turn: turn_id,
            predecessor_origin: None,
            non_accepted_predecessor: None,
            accepted_command: command_id,
            accepted_input,
            accepted_session: session_id,
            accepted_content: content,
            accepted_delivery: delivery,
            accepted_position: position,
            accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id),
            queue_session: session_id,
            queue_turn: turn_id,
            queue_order: order,
            defaults_session: session_id,
            defaults_version: version,
            defaults,
            stored_requested_model: ModelSelectionRequest::Direct(direct),
            stored_frozen_model: FrozenModelSelection::Direct(direct),
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        },
    )
    .reconstitute()
    .expect("fixture receipt facts are correlated");
    let origin = SubmitInputTurnOriginReconstitutionInput::new(
        SubmitInputDirectTurnOriginConstructionInput {
            receipt,
            lifecycle: lifecycle.clone(),
            queue_accepted_input: accepted_input,
            queue_session: session_id,
            queue_turn: turn_id,
            queue_order: order,
        },
    );
    let origin_content = ModelCallOriginContent::from_reconstituted_turn_origin(&origin)
        .expect("checked origin carries exact content");
    let checked = session
        .current_configuration_defaults()
        .derive_request(version, ModelSelectionOverride::UseSessionDefault)
        .expect("fixture defaults version is current");
    let configuration = signalbox_domain::OriginConfiguration::freeze(checked, |_| None)
        .expect("a direct selection needs no alias lookup");
    let record = AcceptedInputTurnSchedulingRecord::new(
        session_id,
        turn_id,
        session_id,
        lifecycle,
        session_id,
        turn_id,
        order,
        delivery,
        configuration,
        AcceptedInputTurnSchedulingRecordState::Queued,
    );
    let activation = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        vec![record],
        Vec::new(),
        Vec::new(),
        None,
    )
    .reconstitute()
    .expect("fixture scheduling projection is complete")
    .prepare_earliest_queued_activation(AcceptedInputTurnActivationIdentities::new(
        identity(99, SemanticTranscriptEntryId::from_uuid),
        identity(6, SemanticTranscriptEntryId::from_uuid),
        identity(7, ContextFrontierId::from_uuid),
        identity(8, TurnAttemptId::from_uuid),
    ))
    .expect("the sole queued fixture turn is eligible");
    let (active_turn, starting_entries, starting_snapshot) = activation.into_parts();
    let origin_entry = starting_entries
        .last()
        .expect("fixture activation carries its origin")
        .clone();
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        direct,
        ResolvedProviderTarget::naming(identity(9, ProviderModelIdentity::from_uuid)),
    )])
    .expect("the fixture target key is unique");
    let initial = ModelCallExecutionReconstitutionInput::new(
        active_turn.clone(),
        targets.clone(),
        starting_snapshot.clone(),
        vec![origin_entry.clone()],
        vec![origin_content.clone()],
        None,
        Vec::new(),
    )
    .with_attachment_blob_facts(attachment_blob_facts.clone())
    .reconstitute()
    .expect("fixture activation reconstructs execution");
    let prepared = initial
        .prepare_initial_call(identity(10, ModelCallId::from_uuid))
        .expect("fixture call can be prepared");
    ModelCallExecutionReconstitutionInput::new(
        active_turn,
        targets,
        starting_snapshot,
        vec![origin_entry],
        vec![origin_content],
        Some(PinnedProviderTargetReconstitutionInput::new(
            prepared.call().turn(),
            prepared.call().target(),
        )),
        vec![ModelCallReconstitutionInput::new(
            prepared.call().id(),
            prepared.call().turn(),
            prepared.call().attempt(),
            prepared.call().selection(),
            prepared.call().target(),
            prepared.call().frontier().snapshot(),
            ModelCallReconstitutionState::Prepared,
        )],
    )
    .with_attachment_blob_facts(attachment_blob_facts)
    .reconstitute()
    .expect("fixture Prepared facts reconstruct")
}

/// A `Prepared` continuation call whose turn already recorded exactly
/// `rounds` distinct automatic tool rounds, paired with the exact durable
/// tool authority its frontier rendering demands.
///
/// Each round is one completed producing call contributing a single
/// `AssistantToolUse` frontier entry paired with its own `ToolDenied`
/// result, so the recorded round count is the one knob. Every round is
/// denied, which closes it and leaves the continuation call admissible;
/// pairing each proposal with its result is what the tool loop actually
/// produces, and a proposal-only history is a shape no round can reach.
/// Identities are seeded from a dedicated range, decorrelated from the
/// round ordinal, so an implementation counting identities instead of
/// producing calls cannot accidentally pass.
///
/// The returned failed turn closes *this* fixture's own prepared call, so
/// a test can require the committed terminalization to name the saturated
/// session and call rather than accepting any failed turn at all.
pub(super) fn tool_round_saturated_fixture(
    rounds: usize,
) -> (
    PreparedModelCallRequest,
    Box<[ResolvedToolConversationEntry]>,
    FailedModelCallTurn,
) {
    tool_round_saturated_fixture_with_assistant_text(rounds, None)
}

/// The same saturated turn, optionally carrying one assistant-text entry
/// alongside the last round's proposal.
///
/// Assistant text is durable frontier content the renderer clones but no
/// tool evidence names, which is what lets a test separate the retained
/// content a tool-only accounting sees from the content a render actually
/// holds.
pub(super) fn tool_round_saturated_fixture_with_assistant_text(
    rounds: usize,
    assistant_text: Option<&str>,
) -> (
    PreparedModelCallRequest,
    Box<[ResolvedToolConversationEntry]>,
    FailedModelCallTurn,
) {
    let session_id = identity(200, SessionId::from_uuid);
    let direct = identity(201, DirectModelSelection::from_uuid);
    let accepted_input = identity(202, AcceptedInputId::from_uuid);
    let turn_id = identity(203, TurnId::from_uuid);
    let origin_entry = SemanticTranscriptEntryRef::from_source(
        session_id,
        identity(204, SemanticTranscriptEntryId::from_uuid),
    );
    let starting_frontier = identity(205, ContextFrontierId::from_uuid);
    let current_attempt = identity(206, TurnAttemptId::from_uuid);
    let target = ResolvedProviderTarget::naming(identity(207, ProviderModelIdentity::from_uuid));
    let continuation_call = identity(208, ModelCallId::from_uuid);
    let current_frontier = identity(209, ContextFrontierId::from_uuid);
    let version = SessionConfigurationDefaultsVersion::first();
    let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct));
    let session = SessionReconstitutionInput::new(
        session_id,
        session_id,
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        session_id,
        version,
        session_id,
        version,
        defaults.clone(),
        signalbox_domain::SessionPlacementReconstitutionFacts {
            current_pointer_session: session_id,
            current_pointer_version: signalbox_domain::SessionPlacementVersion::INITIAL,
            selected_event_session: session_id,
            selected_event: signalbox_domain::VersionedSessionPlacement::initial(
                signalbox_domain::SessionPlacement::pathless(),
            ),
        },
    )
    .reconstitute()
    .expect("fixture Session facts are correlated");
    let delivery = DeliveryRequest::StartWhenNoActiveTurn {
        configuration: PerInputConfigurationChoices::new(
            version,
            ModelSelectionOverride::UseSessionDefault,
        ),
    };
    let content =
        UserContent::try_text(String::from("keep using tools")).expect("fixture content is valid");
    let position = SessionInputPosition::first();
    let lifecycle =
        AcceptedInputLifecycle::new(accepted_input, AcceptedInputDisposition::OriginOf(turn_id));
    let checked = session
        .current_configuration_defaults()
        .derive_request(version, ModelSelectionOverride::UseSessionDefault)
        .expect("fixture defaults version is current");
    let configuration = signalbox_domain::OriginConfiguration::freeze(checked, |_| None)
        .expect("a direct selection needs no alias lookup");
    let selection = *configuration.effective().model();
    let requests = (0_u128..)
        .take(rounds)
        .map(|round| {
            ToolRequestReconstitutionInput::new(
                identity(2_000 + round, ToolRequestId::from_uuid),
                session_id,
                turn_id,
                identity(1_000 + round, ModelCallId::from_uuid),
                ToolRequestOrdinal::from_u32(0),
                ToolName::try_new(String::from("saturating")).expect("fixture tool name is valid"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("fixture arguments are valid"),
            )
            .into_request()
        })
        .collect::<Vec<_>>();
    let tool_use_entries = (0_u128..)
        .zip(&requests)
        .map(|(round, _)| {
            SemanticTranscriptEntryRef::from_source(
                session_id,
                identity(3_000 + round, SemanticTranscriptEntryId::from_uuid),
            )
        })
        .collect::<Vec<_>>();
    // Every round carries its own result. A continuation frontier must
    // include the current round's complete result evidence, and proposals
    // render paired with their results, so a history of `rounds` proposals
    // closed by a single result is a shape the tool loop cannot produce.
    // Denying each round is the cheapest spec-conformant pairing: it closes
    // the round it belongs to and leaves the continuation admissible.
    let denial_entries = (0_u128..)
        .zip(&requests)
        .map(|(round, _)| {
            SemanticTranscriptEntryRef::from_source(
                session_id,
                identity(5_000 + round, SemanticTranscriptEntryId::from_uuid),
            )
        })
        .collect::<Vec<_>>();
    let denials = (0_u128..)
        .zip(&requests)
        .map(|(round, request)| {
            ToolApprovalResolutionReconstitutionInput::user_command(
                DecideToolRequest::try_new(
                    identity(6_000 + round, DurableCommandId::from_uuid),
                    request.id(),
                    ToolApprovalDecision::Deny {
                        reason: Some(
                            ToolDenialReason::try_new(String::from("fixture closes the round"))
                                .expect("fixture denial reason is valid"),
                        ),
                    },
                )
                .expect("the fixture command identity is admitted")
                .prepare_applied(request)
                .expect("the command names the exact request"),
            )
            .reconstitute()
            .expect("user denial provenance is implemented")
        })
        .collect::<Vec<_>>();
    // The text is produced by the last round's own call and precedes that
    // round's proposal, which is how a provider response carrying both text
    // and a tool request lands. Appending it after the round's results
    // instead would leave the latest round unclosed, which is a frontier
    // shape the tool loop cannot reach.
    let assistant_entry = assistant_text.map(|text| {
        (
            SemanticTranscriptEntryRef::from_source(
                session_id,
                identity(8_000, SemanticTranscriptEntryId::from_uuid),
            ),
            requests
                .last()
                .expect("the fixture carries at least one round")
                .producing_call(),
            AssistantText::try_new(String::from(text)).expect("fixture assistant text is valid"),
        )
    });
    let semantic_entries = [SemanticTranscriptEntryReconstitutionInput::new(
        origin_entry.entry(),
        session_id,
        SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input },
    )]
    .into_iter()
    .chain(
        tool_use_entries
            .iter()
            .zip(&denial_entries)
            .zip(&requests)
            .enumerate()
            .flat_map(|(round, ((proposal, result), request))| {
                let text = assistant_entry
                    .iter()
                    .filter(|_| round + 1 == rounds)
                    .map(|(source, producing_call, value)| {
                        SemanticTranscriptEntryReconstitutionInput::new(
                            source.entry(),
                            session_id,
                            SemanticTranscriptEntryPayload::AssistantText {
                                producing_call: *producing_call,
                                value: value.clone(),
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                text.into_iter().chain([
                    SemanticTranscriptEntryReconstitutionInput::new(
                        proposal.entry(),
                        session_id,
                        SemanticTranscriptEntryPayload::AssistantToolUse {
                            producing_call: request.producing_call(),
                            request: request.id(),
                        },
                    ),
                    SemanticTranscriptEntryReconstitutionInput::new(
                        result.entry(),
                        session_id,
                        SemanticTranscriptEntryPayload::ToolDenied {
                            request: request.id(),
                        },
                    ),
                ])
            }),
    )
    .collect::<Vec<_>>();
    // Only the first round is prepared from the turn's starting frontier.
    // Every continuation call is prepared from the preceding round's
    // *result* frontier, which already contains that round's proposal and
    // its paired result — so round `n` sees the origin plus `n` pairs.
    // Giving all `rounds` calls the starting snapshot would describe a
    // history the implemented tool loop cannot produce, letting the
    // saturation bound be exercised against an impossible turn.
    let round_frontiers = (0_u128..)
        .take(rounds)
        .map(|round| {
            if round == 0 {
                starting_frontier
            } else {
                identity(7_000 + round, ContextFrontierId::from_uuid)
            }
        })
        .collect::<Vec<_>>();
    let round_snapshots = round_frontiers
        .iter()
        .enumerate()
        .map(|(preceding_rounds, frontier)| {
            ResolvedContextFrontierReconstitutionInput::new(
                session_id,
                *frontier,
                [origin_entry]
                    .into_iter()
                    .chain(
                        tool_use_entries
                            .iter()
                            .zip(&denial_entries)
                            .take(preceding_rounds)
                            .flat_map(|(proposal, result)| [*proposal, *result]),
                    )
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    let producing_calls = (0_u128..)
        .zip(&requests)
        .zip(&round_frontiers)
        .map(|((round, request), frontier)| {
            ModelCallReconstitutionInput::new(
                request.producing_call(),
                turn_id,
                identity(4_000 + round, TurnAttemptId::from_uuid),
                selection,
                target,
                *frontier,
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )
        })
        .collect::<Vec<_>>();
    let projection = AcceptedInputSchedulingReconstitutionInput::new(
        session,
        vec![AcceptedInputTurnSchedulingRecord::new(
            session_id,
            turn_id,
            session_id,
            lifecycle.clone(),
            session_id,
            turn_id,
            AcceptedInputQueueOrder::ordinary(position),
            delivery,
            configuration,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier,
                phase: ActiveTurnSchedulingReconstitutionInput::prepared(turn_id, current_attempt),
            },
        )],
        semantic_entries,
        round_snapshots,
        Some(SessionAcceptanceTailReconstitutionInput::new(
            session_id,
            accepted_input,
            position,
            vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                session_id, lifecycle, position, delivery,
            )],
        )),
    )
    .with_model_call_facts(
        vec![PinnedProviderTargetReconstitutionInput::new(
            turn_id, target,
        )],
        producing_calls,
    )
    .reconstitute()
    .expect("the saturated scheduling facts are complete");
    let active_turn = projection
        .active_turn_execution()
        .expect("the saturated turn owns the active slot");
    let starting_snapshot = projection
        .resolved_snapshot(starting_frontier)
        .cloned()
        .expect("the starting snapshot is projected");
    // Proposal-ordered: each round's proposal is immediately followed by
    // its own result, which is how the renderer pairs them.
    let frontier_references = [origin_entry]
        .into_iter()
        .chain(
            tool_use_entries
                .iter()
                .zip(&denial_entries)
                .enumerate()
                .flat_map(|(round, (proposal, result))| {
                    let text = assistant_entry
                        .iter()
                        .filter(|_| round + 1 == rounds)
                        .map(|(source, _, _)| *source)
                        .collect::<Vec<_>>();
                    text.into_iter().chain([*proposal, *result])
                }),
        )
        .collect::<Vec<_>>();
    let frontier_entries = frontier_references
        .iter()
        .map(|reference| {
            projection
                .semantic_entry(*reference)
                .cloned()
                .expect("every frontier member is projected")
        })
        .collect::<Vec<_>>();
    let execution = ModelCallExecutionReconstitutionInput::new(
        active_turn,
        ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(direct, target)])
            .expect("the fixture target key is unique"),
        starting_snapshot,
        frontier_entries,
        vec![ModelCallOriginContent::from_goal_turn(
            accepted_input,
            content,
        )],
        Some(PinnedProviderTargetReconstitutionInput::new(
            turn_id, target,
        )),
        vec![ModelCallReconstitutionInput::new(
            continuation_call,
            turn_id,
            current_attempt,
            selection,
            target,
            current_frontier,
            ModelCallReconstitutionState::Prepared,
        )],
    )
    .with_tool_denial_correlations(denials.iter().cloned().map(Into::into).collect())
    .with_call_snapshot(ResolvedContextFrontierReconstitutionInput::new(
        session_id,
        current_frontier,
        frontier_references,
    ))
    .reconstitute()
    .expect("the saturated Prepared facts reconstruct");
    let tool_evidence = tool_use_entries
        .iter()
        .zip(&denial_entries)
        .zip(&requests)
        .zip(&denials)
        .flat_map(|(((proposal, result), request), approval)| {
            [
                ResolvedToolConversationEntry::AssistantToolUse {
                    source: *proposal,
                    request: request.clone(),
                },
                ResolvedToolConversationEntry::Denied {
                    source: *result,
                    request: request.clone(),
                    approval: approval.clone(),
                },
            ]
        })
        .collect::<Box<[_]>>();
    let request = execution
        .resume_prepared_call()
        .expect("the saturated Prepared request resumes");
    let failed = execution
        .fail_prepared_call(FailedModelCallTurnIdentities::new(
            identity(212, SemanticTranscriptEntryId::from_uuid),
            identity(213, ContextFrontierId::from_uuid),
        ))
        .expect("the saturated Prepared call closes as a failed turn");
    (request, tool_evidence, failed)
}

#[derive(Debug)]
pub(super) struct FixedIds {
    pub(super) calls: VecDeque<ModelCallId>,
    pub(super) entries: VecDeque<SemanticTranscriptEntryId>,
    pub(super) frontiers: VecDeque<ContextFrontierId>,
    pub(super) requests: VecDeque<ToolRequestId>,
    pub(super) attempts: VecDeque<TurnAttemptId>,
    pub(super) turns: VecDeque<TurnId>,
}

impl FixedIds {
    pub(super) fn baseline() -> Self {
        Self {
            calls: [20, 21]
                .map(|value| identity(value, ModelCallId::from_uuid))
                .into(),
            entries: (30..40)
                .map(|value| identity(value, SemanticTranscriptEntryId::from_uuid))
                .collect(),
            frontiers: (40..50)
                .map(|value| identity(value, ContextFrontierId::from_uuid))
                .collect(),
            requests: (60..70)
                .map(|value| identity(value, ToolRequestId::from_uuid))
                .collect(),
            attempts: (70..80)
                .map(|value| identity(value, TurnAttemptId::from_uuid))
                .collect(),
            turns: (50..60)
                .map(|value| identity(value, TurnId::from_uuid))
                .collect(),
        }
    }
}

impl ModelCallExecutionIdGenerator for FixedIds {
    fn next_model_call_id(&mut self) -> ModelCallId {
        self.calls.pop_front().expect("fixture call identity")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.entries.pop_front().expect("fixture entry identity")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("fixture frontier identity")
    }

    fn next_tool_request_id(&mut self) -> ToolRequestId {
        self.requests.pop_front().expect("fixture request identity")
    }

    fn next_turn_attempt_id(&mut self) -> TurnAttemptId {
        self.attempts.pop_front().expect("fixture attempt identity")
    }

    fn next_turn_id(&mut self) -> TurnId {
        self.turns.pop_front().expect("fixture turn identity")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FakeError {
    IdentityCollision,
    Infrastructure,
    CommitAmbiguous,
}

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::IdentityCollision => "fake identity collision",
            Self::Infrastructure => "fake infrastructure failure",
            Self::CommitAmbiguous => "fake commit-ambiguous failure",
        })
    }
}

impl Error for FakeError {}

impl ClassifyOperatorFailure for FakeError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::IdentityCollision => OperatorFailureClass::IdentityCollision,
            Self::Infrastructure => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::CommitAmbiguous => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
        }
    }
}

#[derive(Debug)]
pub(super) struct FakePrepare {
    pub(super) outcomes: VecDeque<Result<PrepareModelCallOutcome, FakeError>>,
    pub(super) calls: usize,
}

impl PrepareModelCallTransaction for FakePrepare {
    type Error = FakeError;

    async fn prepare<NextSteeringIdentities>(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
        _failure_identities: FailedModelCallTurnIdentities,
        _steering_frontier: ContextFrontierId,
        _next_steering_identities: NextSteeringIdentities,
    ) -> Result<PrepareModelCallOutcome, Self::Error>
    where
        NextSteeringIdentities:
            FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
    {
        self.calls += 1;
        self.outcomes
            .pop_front()
            .expect("one fake prepare outcome per call")
    }
}

#[derive(Debug)]
pub(super) struct UnusedFailure;

impl FailPreparedModelCallTransaction for UnusedFailure {
    type Error = FakeError;

    async fn fail_prepared<NextTurn>(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
        _cause: PreparedModelCallFailureCause,
        _attachment_failure: Option<AttachmentPreparationFailure>,
        _identities: FailedModelCallTurnIdentities,
        _next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        panic!("unused failure transaction")
    }

    async fn reread_failure(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
        _attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
        panic!("unused prepared-failure reread")
    }
}

#[derive(Debug)]
pub(super) struct FakeFailure {
    pub(super) errors: VecDeque<FakeError>,
    pub(super) rereads: VecDeque<Result<RetainedPreparedFailureStatus, FakeError>>,
    pub(super) calls: usize,
    pub(super) reread_calls: usize,
}

impl FailPreparedModelCallTransaction for FakeFailure {
    type Error = FakeError;

    async fn fail_prepared<NextTurn>(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
        _cause: PreparedModelCallFailureCause,
        _attachment_failure: Option<AttachmentPreparationFailure>,
        _identities: FailedModelCallTurnIdentities,
        _next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        self.calls += 1;
        Err(self
            .errors
            .pop_front()
            .expect("one fake failure-commit error"))
    }

    async fn reread_failure(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
        _attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
        self.reread_calls += 1;
        self.rereads
            .pop_front()
            .expect("one fake capability-failure reread")
    }
}

/// One `fail_prepared` invocation, as its caller addressed it.
///
/// Recorded rather than discarded: a fake that only counts calls proves
/// the commit was attempted and nothing about *what* was committed, so a
/// retry reusing the colliding identities, or a failure written against an
/// unrelated session, would satisfy the tests below unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FailPreparedCall {
    pub(super) session: SessionId,
    pub(super) call: ModelCallId,
    pub(super) cause: PreparedModelCallFailureCause,
    pub(super) attachment_failure: Option<AttachmentPreparationFailure>,
    pub(super) identities: FailedModelCallTurnIdentities,
}

#[derive(Debug)]
pub(super) struct ScriptedFailure {
    pub(super) results: VecDeque<Result<FailedModelCallTurn, FakeError>>,
    pub(super) calls: usize,
    pub(super) recorded: Vec<FailPreparedCall>,
}

impl FailPreparedModelCallTransaction for ScriptedFailure {
    type Error = FakeError;

    async fn fail_prepared<NextTurn>(
        &mut self,
        session: SessionId,
        call: ModelCallId,
        cause: PreparedModelCallFailureCause,
        attachment_failure: Option<AttachmentPreparationFailure>,
        identities: FailedModelCallTurnIdentities,
        _next_reclassified_turn: NextTurn,
    ) -> Result<FailedModelCallTurn, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        self.calls += 1;
        self.recorded.push(FailPreparedCall {
            session,
            call,
            cause,
            attachment_failure,
            identities,
        });
        self.results
            .pop_front()
            .expect("one scripted failure-commit result")
    }

    async fn reread_failure(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
        _attachment_failure: Option<AttachmentPreparationFailure>,
    ) -> Result<RetainedPreparedFailureStatus, Self::Error> {
        panic!("a committed capability failure is never reread")
    }
}

#[derive(Debug)]
pub(super) struct FakeAuthorization {
    pub(super) outcomes: VecDeque<Result<AuthorizedModelCall, FakeError>>,
    pub(super) rereads: VecDeque<Result<ModelCallAuthorizationReread, FakeError>>,
    pub(super) calls: usize,
    pub(super) reread_calls: usize,
}

impl AuthorizeModelCallTransaction for FakeAuthorization {
    type Error = FakeError;

    async fn authorize(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
    ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
        self.calls += 1;
        self.outcomes
            .pop_front()
            .expect("one fake authorization outcome")
            .map(|authorized| AuthorizeModelCallOutcome::Authorized(Box::new(authorized)))
    }

    async fn reread_after_ambiguous_commit(
        &mut self,
        _session: SessionId,
        _prepared: &PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, Self::Error> {
        self.reread_calls += 1;
        self.rereads
            .pop_front()
            .expect("one fake authorization reread")
    }

    fn cancellation_signal(
        &self,
        _session: SessionId,
        _call: ModelCallId,
    ) -> impl Future<Output = ()> + Send + 'static {
        std::future::pending()
    }
}

#[derive(Debug)]
pub(super) struct UnusedAuthorization;

impl AuthorizeModelCallTransaction for UnusedAuthorization {
    type Error = FakeError;

    async fn authorize(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
    ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
        panic!("unused authorization transaction")
    }

    async fn reread_after_ambiguous_commit(
        &mut self,
        _session: SessionId,
        _prepared: &PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, Self::Error> {
        panic!("unused authorization reread")
    }

    fn cancellation_signal(
        &self,
        _session: SessionId,
        _call: ModelCallId,
    ) -> impl Future<Output = ()> + Send + 'static {
        std::future::pending()
    }
}

#[derive(Debug)]
pub(super) struct NoSendAuthorization {
    pub(super) calls: usize,
}

impl AuthorizeModelCallTransaction for NoSendAuthorization {
    type Error = FakeError;

    async fn authorize(
        &mut self,
        _session: SessionId,
        _call: ModelCallId,
    ) -> Result<AuthorizeModelCallOutcome, Self::Error> {
        self.calls += 1;
        Ok(AuthorizeModelCallOutcome::NoSend)
    }

    async fn reread_after_ambiguous_commit(
        &mut self,
        _session: SessionId,
        _prepared: &PreparedModelCallRequest,
    ) -> Result<ModelCallAuthorizationReread, Self::Error> {
        panic!("a known no-send result needs no reread")
    }

    fn cancellation_signal(
        &self,
        _session: SessionId,
        _call: ModelCallId,
    ) -> impl Future<Output = ()> + Send + 'static {
        std::future::pending()
    }
}

#[derive(Debug)]
pub(super) struct UnusedObservation;

impl CommitModelCallObservationTransaction for UnusedObservation {
    type Error = FakeError;

    async fn commit_observation<NextTurn>(
        &mut self,
        _session: SessionId,
        _observation: CorrelatedModelCallTerminalObservation,
        _identities: ModelCallTerminalIdentityCandidates,
        _next_reclassified_turn: NextTurn,
    ) -> Result<Option<ModelCallObservationCommitOutcome>, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        panic!("unused observation transaction")
    }

    async fn reread_observation(
        &mut self,
        _session: SessionId,
        _observation: &CorrelatedModelCallTerminalObservation,
    ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
        panic!("unused observation reread")
    }
}

#[derive(Debug)]
pub(super) struct FakeObservation {
    pub(super) commit_errors: VecDeque<FakeError>,
    pub(super) rereads: VecDeque<Result<RetainedModelCallObservationStatus, FakeError>>,
    pub(super) observed: Vec<CorrelatedModelCallTerminalObservation>,
    pub(super) commit_calls: usize,
    pub(super) reread_calls: usize,
}

impl CommitModelCallObservationTransaction for FakeObservation {
    type Error = FakeError;

    async fn commit_observation<NextTurn>(
        &mut self,
        _session: SessionId,
        observation: CorrelatedModelCallTerminalObservation,
        _identities: ModelCallTerminalIdentityCandidates,
        _next_reclassified_turn: NextTurn,
    ) -> Result<Option<ModelCallObservationCommitOutcome>, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        self.commit_calls += 1;
        self.observed.push(observation);
        Err(self
            .commit_errors
            .pop_front()
            .expect("one fake observation commit failure"))
    }

    async fn reread_observation(
        &mut self,
        _session: SessionId,
        _observation: &CorrelatedModelCallTerminalObservation,
    ) -> Result<RetainedModelCallObservationStatus, Self::Error> {
        self.reread_calls += 1;
        self.rereads
            .pop_front()
            .expect("one fake observation reread")
    }
}

#[derive(Debug)]
pub(super) struct UnusedProvider;

impl ModelCallProvider for UnusedProvider {
    type Capability = ();
    type Error = FakeError;

    async fn prepare_capability<Cancellation>(
        &mut self,
        _operation: PreparedModelOperation,
        _cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        panic!("unused provider capability preparation")
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        _authorized: AuthorizedModelCall,
        _capability: Self::Capability,
        _acceptance_possible: AcceptancePossible,
        _cancellation: Cancellation,
    ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        panic!("unused provider interaction")
    }
}

#[derive(Debug)]
pub(super) struct AttachmentFailureProvider {
    pub(super) failure: AttachmentPreparationFailure,
    pub(super) preparation_count: usize,
}

impl ModelCallProvider for AttachmentFailureProvider {
    type Capability = ();
    type Error = FakeError;

    async fn prepare_capability<Cancellation>(
        &mut self,
        _operation: PreparedModelOperation,
        _cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        self.preparation_count += 1;
        Ok(ModelCallCapabilityPreparation::AttachmentFailure(
            self.failure,
        ))
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        _authorized: AuthorizedModelCall,
        _capability: Self::Capability,
        _acceptance_possible: AcceptancePossible,
        _cancellation: Cancellation,
    ) -> Result<CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        panic!("attachment failure must prevent provider interaction")
    }
}

#[derive(Debug)]
pub(super) struct BoundaryBlockingProvider {
    pub(super) crossed: Arc<tokio::sync::Notify>,
    pub(super) finish: Arc<tokio::sync::Notify>,
    pub(super) interaction_count: usize,
}

impl ModelCallProvider for BoundaryBlockingProvider {
    type Capability = PreparedModelOperation;
    type Error = FakeError;

    async fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        _cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        Ok(ModelCallCapabilityPreparation::Ready(operation))
    }

    fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        _authorized: AuthorizedModelCall,
        _capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        _cancellation: Cancellation,
    ) -> impl Future<Output = Result<CorrelatedModelCallTerminalObservation, Self::Error>> + Send
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        self.interaction_count += 1;
        let crossed = Arc::clone(&self.crossed);
        let finish = Arc::clone(&self.finish);
        async move {
            acceptance_possible();
            crossed.notify_one();
            finish.notified().await;
            Err(FakeError::Infrastructure)
        }
    }
}

pub(super) fn current_turn_tool_rounds(round_count: u128) -> Vec<ModelConversationMessage> {
    (0..round_count)
        .map(|round| model_tool_use_message(1_000 + round, 2, 2_000 + round, 0))
        .collect()
}

pub(super) fn one_current_batch_with_inherited_tool_history() -> Vec<ModelConversationMessage> {
    (0..32_u32)
        .map(|ordinal| model_tool_use_message(3_000 + u128::from(ordinal), 2, 4_000, ordinal))
        .chain(
            (0..32_u128).map(|round| model_tool_use_message(5_000 + round, 99, 6_000 + round, 0)),
        )
        .collect()
}

/// The seeds of the canonical recorded-override fixture; arbitrary — they
/// only need to exist as one recorded override.
pub(super) const OVERRIDE_COMMAND_SEED: u128 = 81;
pub(super) const OVERRIDE_DENIED_REQUEST_SEED: u128 = 82;
pub(super) const OVERRIDE_JUDGE_CALL_SEED: u128 = 83;

/// One recorded override of a denied `guarded` proposal with `{}` arguments
/// in the canonical fixture session.
pub(super) fn recorded_guarded_override() -> signalbox_domain::RecordedUserOverride {
    signalbox_domain::RecordedUserOverride::new(
        identity(
            OVERRIDE_COMMAND_SEED,
            signalbox_domain::DurableCommandId::from_uuid,
        ),
        identity(1, SessionId::from_uuid),
        identity(OVERRIDE_DENIED_REQUEST_SEED, ToolRequestId::from_uuid),
        identity(OVERRIDE_JUDGE_CALL_SEED, ModelCallId::from_uuid),
        signalbox_domain::ToolName::try_new(String::from("guarded"))
            .expect("fixture tool name is valid"),
        signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are valid"),
    )
}

/// One `guarded` proposal with the given provider argument text.
pub(super) fn guarded_proposal(arguments: &str) -> AssistantResponsePart {
    AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
        signalbox_domain::ToolName::try_new(String::from("guarded"))
            .expect("fixture tool name is valid"),
        signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from(arguments))
            .expect("fixture arguments are valid"),
    ))
}

/// One completed tool response containing exactly the supplied parts.
pub(super) fn completed_with_tools(
    parts: Vec<AssistantResponsePart>,
) -> ModelCallTerminalObservation {
    ModelCallTerminalObservation::CompletedWithTools {
        response: signalbox_domain::ToolUsingAssistantResponse::try_from_parts(parts)
            .expect("fixture response contains tools"),
        retained_input_tokens: None,
        retained_output_tokens: None,
    }
}

/// Selects initial approvals for the parts through a service advertising
/// one `guarded` tool frozen at the given posture.
#[track_caller]
pub(super) fn guarded_tool_approvals(
    posture: signalbox_domain::ToolApprovalPosture,
    parts: Vec<AssistantResponsePart>,
    recorded: &[signalbox_domain::RecordedUserOverride],
) -> Box<[InitialToolApproval]> {
    let schema =
        crate::ToolInputSchema::try_new(String::from(r#"{"properties":{},"type":"object"}"#))
            .expect("fixture schema is valid");
    let definition = crate::ToolDefinition::new(
        signalbox_domain::ToolName::try_new(String::from("guarded"))
            .expect("fixture name is valid"),
        String::from("Awaits its frozen approval posture."),
        schema,
        signalbox_domain::ToolPermissionDefault::Confirm,
        signalbox_domain::ToolEffectClass::ExternalEffect,
    )
    .with_approval_posture(posture);
    let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
        definition,
        |_: &signalbox_domain::NormalizedToolArguments| Ok(()),
    )])
    .expect("one tool is unambiguous");
    let service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: VecDeque::new(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        UnusedProvider,
        InProcessAttemptDispatchGate::default(),
        None,
    )
    .with_tool_catalog(catalog);
    let advertised_tools = service.catalog.definitions();
    service.tool_approvals(
        &completed_with_tools(parts),
        DangerousToolAutoApproval::Disabled,
        &advertised_tools,
        recorded,
    )
}

/// Sums the content the rendered messages hold, message kind by message
/// kind.
///
/// This is the renderer's side of the accounting-fidelity comparison. It
/// reads what each message carries *after* the clone, so a term the ceiling
/// forgot surfaces as a difference instead of as two copies of the same
/// omission agreeing with each other.
pub(super) fn rendered_content_bytes(messages: &[ModelConversationMessage]) -> usize {
    messages.iter().fold(0_usize, |total, message| {
        let bytes = match message {
            ModelConversationMessage::ContextSummary { content, .. }
            | ModelConversationMessage::Assistant { content, .. } => content.as_str().len(),
            ModelConversationMessage::ProviderCompaction { block, .. } => block.as_json().len(),
            ModelConversationMessage::ProviderReasoning { item, .. } => item.as_json().len(),
            ModelConversationMessage::User { content, .. } => {
                content
                    .parts()
                    .iter()
                    .fold(0_usize, |total, part| match part {
                        ModelUserContentPart::Text(value) => {
                            total.saturating_add(value.as_str().len())
                        }
                        ModelUserContentPart::AttachmentStub(stub) => total
                            .saturating_add(stub.rendered.len())
                            .saturating_add(stub.media_type.as_str().len())
                            .saturating_add(
                                stub.display_filename
                                    .as_ref()
                                    .map_or(0, |name| name.as_str().len()),
                            ),
                    })
            }
            ModelConversationMessage::DelegatedTask { content, .. }
            | ModelConversationMessage::DelegationMessage { content, .. } => content.as_str().len(),
            ModelConversationMessage::BackgroundDelegationResult { outcome, .. } => outcome
                .content()
                .map_or(0, |content| content.as_str().len()),
            ModelConversationMessage::AssistantToolUse { request, .. } => {
                request.arguments().as_str().len()
            }
            ModelConversationMessage::ToolResult { content, .. } => match content {
                ModelToolResultContent::Success(ToolResultContent::Text(text)) => {
                    text.as_str().len()
                }
                ModelToolResultContent::ExecutionError(error) => {
                    error.detail().map_or(0, |detail| detail.as_str().len())
                }
                ModelToolResultContent::Denied { reason } => {
                    reason.as_ref().map_or(0, |reason| reason.as_str().len())
                }
                ModelToolResultContent::ClosedByTurnEnd => 0,
                ModelToolResultContent::Delegation(outcome) => outcome
                    .content()
                    .map_or(0, |content| content.as_str().len()),
            },
            ModelConversationMessage::ImportedUser { content, .. }
            | ModelConversationMessage::ImportedAssistant { content, .. } => content.as_str().len(),
            // An identity change carries fixed-width facts only.
            ModelConversationMessage::ModelIdentityChanged { .. }
            | ModelConversationMessage::RunnerPlacementChanged { .. } => 0,
        };
        total + bytes
    })
}

/// Counts one prepared request's projected content the way the ceiling
/// does, reading the durable frontier and its origin content by reference.
pub(super) fn counted_frontier_bytes(
    request: &PreparedModelCallRequest,
    tool_entries: &[ResolvedToolConversationEntry],
) -> usize {
    projected_frontier_content_bytes(
        request
            .frontier_entry_slice()
            .iter()
            .map(|entry| (entry.reference(), entry.payload())),
        |accepted_input| request.origin_content(accepted_input),
        tool_entries.iter(),
    )
}
