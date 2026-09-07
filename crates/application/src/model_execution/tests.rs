use std::error::Error;
use std::{
    collections::VecDeque,
    io::{self, Write},
    num::NonZeroU64,
    sync::{Arc, Mutex as StdMutex},
};

use expect_test::expect;
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
    AcceptedInputTurnActivationIdentities, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, ActiveTurnSchedulingReconstitutionInput, Actor,
    DecideToolRequest, DeliveryRequest, DirectModelSelection, DurableCommandId,
    FrozenModelSelection, ImportedMessageContentAbsence, ModelCallDisposition,
    ModelCallExecutionReconstitutionInput, ModelCallOriginContent, ModelCallReconstitutionInput,
    ModelCallReconstitutionState, ModelSelectionOverride, ModelSelectionRequest,
    ModelTargetCatalog, ModelTargetDefinition, NormalizedToolArguments,
    PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput, ProviderModelIdentity,
    ResolvedContextFrontierReconstitutionInput, ResolvedProviderTarget,
    SemanticTranscriptEntryReconstitutionInput, SessionAcceptanceTailEntryReconstitutionInput,
    SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionInputPosition, SessionReconstitutionInput, SubmitInput,
    SubmitInputAppliedTurnOriginReconstitutionInput, SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputReconstitutionInput, SubmitInputTurnOriginReconstitutionInput,
    ToolApprovalResolutionReconstitutionInput, ToolAttemptReconstitutionInput,
    ToolAttemptReconstitutionState, ToolDispatchGeneration, ToolEffectClass, ToolName,
    ToolPermissionDefault, ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolResultText,
    TranscriptAncestry,
};
use tracing::instrument::WithSubscriber as _;
use uuid::Uuid;

use super::*;

#[derive(Clone, Default)]
struct CapturedTelemetry(Arc<StdMutex<Vec<u8>>>);

struct CapturedTelemetryWriter(Arc<StdMutex<Vec<u8>>>);

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
    fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .expect("captured telemetry remains available")
                .clone(),
        )
        .expect("captured telemetry is UTF-8")
    }
}

fn identity<Identity>(value: u128, from_uuid: impl FnOnce(Uuid) -> Identity) -> Identity {
    from_uuid(Uuid::from_u128(value))
}

fn credential_reference() -> ModelCallCredentialReference {
    ModelCallCredentialReference::new("fixture-provider-primary")
}

fn rendered_text(content: UserContent) -> ModelUserContent {
    render_model_user_content(content, |_| None)
        .expect("text-only fixture needs no attachment catalog facts")
}

/// ordered attachment content becomes bounded canonical text stubs with metadata visible and
/// blob bytes absent.
#[test]
fn attachment_frontier_renders_ordered_stubs_without_bytes() {
    let digest = BlobDigest::digest(b"secret blob bytes");
    let filename = signalbox_domain::AttachmentDisplayFilename::try_new(String::from("scan\".png"))
        .expect("the fixture display filename is valid");
    let content = UserContent::try_parts(vec![
        UserContentPart::try_text(String::from("before"))
            .expect("the fixture leading text is valid"),
        UserContentPart::Attachment {
            digest,
            kind: AttachmentKind::Image,
            media_type: signalbox_domain::DeclaredMediaType::try_new(String::from("image/png"))
                .expect("the fixture media type is valid"),
            display_filename: Some(filename),
        },
        UserContentPart::try_text(String::from("after"))
            .expect("the fixture trailing text is valid"),
    ])
    .expect("the interleaved fixture content is valid");
    let expected_stub = format!(
        r#"{{"signalbox_attachment":{{"kind":"image","media_type":"image/png","display_filename":"scan\".png","byte_length":"17","digest":"{digest}"}}}}"#
    );

    let rendered = render_model_user_content(content, |candidate| {
        (candidate == digest)
            .then_some(NonZeroU64::new(17).expect("the fixture attachment length is positive"))
    })
    .expect("the catalog fact covers the exact attachment");

    assert_eq!(rendered.parts()[0].as_str(), "before");
    assert_eq!(rendered.parts()[1].as_str(), expected_stub);
    assert_eq!(rendered.parts()[2].as_str(), "after");
    assert!(!rendered.parts()[1].as_str().contains("secret blob bytes"));
}

#[test]
fn maximum_checked_attachment_metadata_fits_the_named_stub_bound() {
    let digest = BlobDigest::digest(b"maximum metadata fixture");
    let filename = signalbox_domain::AttachmentDisplayFilename::try_new("\u{1}".repeat(255))
        .expect("the maximum-byte control filename is valid metadata");
    let media_type = signalbox_domain::DeclaredMediaType::try_new("\"".repeat(255))
        .expect("the maximum-byte visible-ASCII media type is valid");
    let content = UserContent::try_parts(vec![UserContentPart::Attachment {
        digest,
        kind: AttachmentKind::Document,
        media_type,
        display_filename: Some(filename),
    }])
    .expect("the maximum metadata fixture is valid");

    let rendered = render_model_user_content(content, |_| Some(NonZeroU64::MAX))
        .expect("the derived bound covers maximum checked metadata");
    let stub = rendered.parts()[0].as_str();

    assert!(stub.len() <= MAX_RENDERED_ATTACHMENT_STUB_BYTES);
    assert_eq!(stub.len(), 2_242);
}

#[test]
fn durable_reasoning_projection_preserves_source_call_bytes_and_content_cost() {
    let raw =
        r#"{ "type":"reasoning", "id":"rs_fixture", "summary":[], "encrypted_content":"opaque" }"#;
    let source = SemanticTranscriptEntryRef::from_source(
        identity(40, SessionId::from_uuid),
        identity(41, SemanticTranscriptEntryId::from_uuid),
    );
    let producing_call = identity(42, ModelCallId::from_uuid);
    let item =
        signalbox_domain::ProviderReasoningItem::try_new(raw.to_string()).expect("durable fixture");
    let payload = SemanticTranscriptEntryPayload::ProviderReasoning {
        producing_call,
        item: item.clone(),
    };
    let messages = render_frontier_messages([(source, &payload)], |_| None, |_| None, [])
        .expect("reasoning renders");
    assert_eq!(
        messages.as_ref(),
        &[ModelConversationMessage::ProviderReasoning {
            source,
            producing_call,
            item
        }]
    );
    assert_eq!(
        projected_frontier_content_bytes([(source, &payload)], |_| None, []),
        raw.len()
    );
}

#[test]
fn delegation_task_message_and_background_result_render_as_typed_inputs() {
    let child = identity(40, SessionId::from_uuid);
    let parent = identity(41, SessionId::from_uuid);
    let spawning_request = identity(42, ToolRequestId::from_uuid);
    let awaiting_request = identity(43, ToolRequestId::from_uuid);
    let message = identity(44, DelegationMessageId::from_uuid);
    let parent_turn = identity(45, TurnId::from_uuid);
    let child_turn = identity(46, TurnId::from_uuid);
    let task_source = SemanticTranscriptEntryRef::from_source(
        child,
        identity(47, SemanticTranscriptEntryId::from_uuid),
    );
    let message_source = SemanticTranscriptEntryRef::from_source(
        child,
        identity(48, SemanticTranscriptEntryId::from_uuid),
    );
    let result_source = SemanticTranscriptEntryRef::from_source(
        parent,
        identity(49, SemanticTranscriptEntryId::from_uuid),
    );
    let task_content =
        DelegationContent::try_new("delegated work".into()).expect("fixture task is valid");
    let message_content =
        DelegationContent::try_new("peer update".into()).expect("fixture message is valid");
    let result_content =
        DelegationContent::try_new("delivered result".into()).expect("fixture result is valid");
    let outcome = DelegationOutcome::reconstitute(
        signalbox_domain::DelegationOutcomeKind::ResultReturned,
        Some(result_content),
        signalbox_domain::DelegationOutcomeReason::ChildCompleted,
        signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
            session: child,
            turn: child_turn,
        },
    )
    .expect("fixture child outcome is correlated");
    let task = SemanticTranscriptEntryPayload::DelegatedTask {
        spawning_request,
        parent_session: parent,
        parent_turn,
        content: task_content.clone(),
    };
    let peer_message = SemanticTranscriptEntryPayload::DelegationMessage {
        spawning_request,
        message,
        sender: parent,
        recipient: child,
        delivery_sequence: NonZeroU64::MIN,
        content: message_content.clone(),
    };
    let result = SemanticTranscriptEntryPayload::DelegationResult {
        awaiting_request,
        spawning_request,
        child,
        mode: DelegationWaitMode::Background,
        delivery_sequence: Some(NonZeroU64::new(2).expect("two is positive")),
        outcome: Box::new(outcome.clone()),
    };

    let rendered = render_frontier_messages(
        [
            (task_source, &task),
            (message_source, &peer_message),
            (result_source, &result),
        ],
        |_| None,
        |_| None,
        [],
    )
    .expect("typed delegation entries render without accepted-input evidence");

    assert_eq!(
        rendered.as_ref(),
        &[
            ModelConversationMessage::DelegatedTask {
                source: task_source,
                spawning_request,
                parent_session: parent,
                parent_turn,
                content: task_content,
            },
            ModelConversationMessage::DelegationMessage {
                source: message_source,
                spawning_request,
                message,
                sender: parent,
                recipient: child,
                delivery_sequence: NonZeroU64::MIN,
                content: message_content,
            },
            ModelConversationMessage::BackgroundDelegationResult {
                source: result_source,
                awaiting_request,
                spawning_request,
                child,
                delivery_sequence: NonZeroU64::new(2).expect("two is positive"),
                outcome,
            },
        ]
    );
}

#[test]
fn foreground_delegation_result_renders_as_await_tool_result() {
    let parent = identity(50, SessionId::from_uuid);
    let child = identity(51, SessionId::from_uuid);
    let awaiting_request = identity(52, ToolRequestId::from_uuid);
    let spawning_request = identity(53, ToolRequestId::from_uuid);
    let source = SemanticTranscriptEntryRef::from_source(
        parent,
        identity(54, SemanticTranscriptEntryId::from_uuid),
    );
    let outcome = DelegationOutcome::reconstitute(
        signalbox_domain::DelegationOutcomeKind::ChildFailed,
        None,
        signalbox_domain::DelegationOutcomeReason::ChildExecutionFailed,
        signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
            session: child,
            turn: identity(55, TurnId::from_uuid),
        },
    )
    .expect("fixture failure is correlated");
    let result = SemanticTranscriptEntryPayload::DelegationResult {
        awaiting_request,
        spawning_request,
        child,
        mode: DelegationWaitMode::Foreground,
        delivery_sequence: None,
        outcome: Box::new(outcome.clone()),
    };

    let rendered = render_frontier_messages([(source, &result)], |_| None, |_| None, [])
        .expect("foreground delivery is one correlated tool result");

    assert_eq!(
        rendered.as_ref(),
        &[ModelConversationMessage::ToolResult {
            source,
            request: awaiting_request,
            content: ModelToolResultContent::Delegation(outcome),
        }]
    );
}

fn ready(request: PreparedModelCallRequest) -> PrepareModelCallOutcome {
    PrepareModelCallOutcome::Ready {
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
fn ready_with_tool_evidence(
    request: PreparedModelCallRequest,
    tool_entries: Box<[ResolvedToolConversationEntry]>,
) -> PrepareModelCallOutcome {
    PrepareModelCallOutcome::Ready {
        reasoning_provenance: Box::new([]),
        request: Box::new(request),
        credential_reference: credential_reference(),
        dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
        recorded_user_overrides: Box::new([]),
        system_prompt: None,
        tool_entries,
    }
}

fn tool_response() -> ModelCallTerminalObservation {
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
fn model_tool_request(ordinal: u32) -> ToolRequest {
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

fn model_tool_use_message(
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

fn prepared_fixture() -> (PreparedModelCallRequest, AuthorizedModelCall) {
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
fn failed_turn_fixture() -> FailedModelCallTurn {
    prepared_execution_fixture()
        .fail_prepared_call(FailedModelCallTurnIdentities::new(
            identity(120, SemanticTranscriptEntryId::from_uuid),
            identity(121, ContextFrontierId::from_uuid),
        ))
        .expect("a prepared fixture call closes as a failed turn")
}

fn prepared_execution_fixture() -> signalbox_domain::ModelCallExecution {
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
    let content = UserContent::try_text(String::from("exact user request"))
        .expect("fixture content is valid");
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
fn tool_round_saturated_fixture(
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
fn tool_round_saturated_fixture_with_assistant_text(
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
    .with_tool_denial_correlations(denials.clone())
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
struct FixedIds {
    calls: VecDeque<ModelCallId>,
    entries: VecDeque<SemanticTranscriptEntryId>,
    frontiers: VecDeque<ContextFrontierId>,
    requests: VecDeque<ToolRequestId>,
    attempts: VecDeque<TurnAttemptId>,
    turns: VecDeque<TurnId>,
}

impl FixedIds {
    fn baseline() -> Self {
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
enum FakeError {
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
struct FakePrepare {
    outcomes: VecDeque<Result<PrepareModelCallOutcome, FakeError>>,
    calls: usize,
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
struct UnusedFailure;

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
struct FakeFailure {
    errors: VecDeque<FakeError>,
    rereads: VecDeque<Result<RetainedPreparedFailureStatus, FakeError>>,
    calls: usize,
    reread_calls: usize,
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
struct FailPreparedCall {
    session: SessionId,
    call: ModelCallId,
    cause: PreparedModelCallFailureCause,
    attachment_failure: Option<AttachmentPreparationFailure>,
    identities: FailedModelCallTurnIdentities,
}

#[derive(Debug)]
struct ScriptedFailure {
    results: VecDeque<Result<FailedModelCallTurn, FakeError>>,
    calls: usize,
    recorded: Vec<FailPreparedCall>,
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
struct FakeAuthorization {
    outcomes: VecDeque<Result<AuthorizedModelCall, FakeError>>,
    rereads: VecDeque<Result<ModelCallAuthorizationReread, FakeError>>,
    calls: usize,
    reread_calls: usize,
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
struct UnusedAuthorization;

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
struct NoSendAuthorization {
    calls: usize,
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
struct UnusedObservation;

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
struct FakeObservation {
    commit_errors: VecDeque<FakeError>,
    rereads: VecDeque<Result<RetainedModelCallObservationStatus, FakeError>>,
    observed: Vec<CorrelatedModelCallTerminalObservation>,
    commit_calls: usize,
    reread_calls: usize,
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
struct UnusedProvider;

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
struct AttachmentFailureProvider {
    failure: AttachmentPreparationFailure,
    preparation_count: usize,
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
struct BoundaryBlockingProvider {
    crossed: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
    interaction_count: usize,
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

fn current_turn_tool_rounds(round_count: u128) -> Vec<ModelConversationMessage> {
    (0..round_count)
        .map(|round| model_tool_use_message(1_000 + round, 2, 2_000 + round, 0))
        .collect()
}

fn one_current_batch_with_inherited_tool_history() -> Vec<ModelConversationMessage> {
    (0..32_u32)
        .map(|ordinal| model_tool_use_message(3_000 + u128::from(ordinal), 2, 4_000, ordinal))
        .chain(
            (0..32_u128).map(|round| model_tool_use_message(5_000 + round, 99, 6_000 + round, 0)),
        )
        .collect()
}

/// The user-selected rendering decision: origin input becomes a user-role
/// message carrying the semantic entry's source, in frontier order.
#[test]
fn frontier_rendering_preserves_user_role_order_and_source() {
    let (request, _) = prepared_fixture();
    let credential_reference = credential_reference();
    let operation = PreparedModelOperation::render(
        request,
        credential_reference.clone(),
        None,
        Box::new([]),
        &[],
        &[],
    )
    .expect("the baseline origin-only frontier renders");
    assert_eq!(operation.credential_reference(), &credential_reference);
    assert_eq!(operation.messages().len(), 1);
    let ModelConversationMessage::User {
        source,
        accepted_input,
        content,
    } = &operation.messages()[0]
    else {
        panic!("an origin entry must render as user content")
    };
    assert_eq!(source.source_session(), identity(1, SessionId::from_uuid));
    assert_eq!(*accepted_input, identity(3, AcceptedInputId::from_uuid));
    assert_eq!(
        content
            .parts()
            .first()
            .expect("the fixture has one provider-visible text part")
            .as_str(),
        "exact user request"
    );
}

/// rendering binds the exact optional frozen-epoch system prompt onto the provider-neutral
/// operation without rewriting it, and an epoch without a prompt renders none.
#[test]
fn render_carries_the_frozen_epoch_system_prompt() {
    let (request, _) = prepared_fixture();
    let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
        .expect("fixture prompt is admissible");

    let prompted = PreparedModelOperation::render(
        request.clone(),
        credential_reference(),
        Some(prompt.clone()),
        Box::new([]),
        &[],
        &[],
    )
    .expect("the baseline origin-only frontier renders");
    assert_eq!(prompted.system_prompt(), Some(prompt.as_str()));

    let promptless = PreparedModelOperation::render(
        request,
        credential_reference(),
        None,
        Box::new([]),
        &[],
        &[],
    )
    .expect("the baseline origin-only frontier renders");
    assert_eq!(promptless.system_prompt(), None);
}

/// The recorded turn-wide availability bound counts validated producing
/// calls, not requests or inherited tool history.
#[test]
fn automatic_tool_round_bound_counts_current_turn_producing_calls() {
    let current_turn = identity(2, TurnId::from_uuid);
    let below_limit_count = 31;
    let below_limit = current_turn_tool_rounds(below_limit_count);
    assert_eq!(
        automatic_tool_round_count(current_turn, &below_limit),
        below_limit_count as usize
    );

    let at_limit_count = 32;
    let at_limit = current_turn_tool_rounds(at_limit_count);
    assert_eq!(
        automatic_tool_round_count(current_turn, &at_limit),
        at_limit_count as usize
    );

    let one_multi_request_round = one_current_batch_with_inherited_tool_history();
    assert_eq!(
        automatic_tool_round_count(current_turn, &one_multi_request_round),
        1,
        "one current-turn batch and inherited history consume one round",
    );
}

/// The seeds of the canonical recorded-override fixture; arbitrary — they
/// only need to exist as one recorded override.
const OVERRIDE_COMMAND_SEED: u128 = 81;
const OVERRIDE_DENIED_REQUEST_SEED: u128 = 82;
const OVERRIDE_JUDGE_CALL_SEED: u128 = 83;

/// One recorded override of a denied `guarded` proposal with `{}` arguments
/// in the canonical fixture session.
fn recorded_guarded_override() -> signalbox_domain::RecordedUserOverride {
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
fn guarded_proposal(arguments: &str) -> AssistantResponsePart {
    AssistantResponsePart::ToolCall(signalbox_domain::ToolCallProposal::new(
        signalbox_domain::ToolName::try_new(String::from("guarded"))
            .expect("fixture tool name is valid"),
        signalbox_domain::NormalizedToolArguments::try_from_provider_text(String::from(arguments))
            .expect("fixture arguments are valid"),
    ))
}

/// One completed tool response containing exactly the supplied parts.
fn completed_with_tools(parts: Vec<AssistantResponsePart>) -> ModelCallTerminalObservation {
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
fn guarded_tool_approvals(
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

/// a recorded override substitutes for the judge only on the exact denied command — a proposal
/// with other arguments still parks for the judge — and the selected approval carries the
/// override command and the overridden denial.
#[test]
fn recorded_override_substitutes_for_the_judge_on_the_exact_command() {
    let recorded = recorded_guarded_override();
    let approvals = guarded_tool_approvals(
        signalbox_domain::ToolApprovalPosture::Delegated,
        vec![
            guarded_proposal("{}"),
            guarded_proposal(r#"{"timezone":"UTC"}"#),
        ],
        std::slice::from_ref(&recorded),
    );

    assert_eq!(
        approvals.as_ref(),
        [
            InitialToolApproval::UserOverride {
                command: recorded.command(),
                denied_request: recorded.denied_request(),
            },
            InitialToolApproval::Delegated,
        ]
    );
}

/// one recorded override pre-approves at most one proposal per response; a second identical
/// proposal parks for the judge again.
#[test]
fn recorded_override_is_consumed_at_most_once_per_response() {
    let recorded = recorded_guarded_override();
    let approvals = guarded_tool_approvals(
        signalbox_domain::ToolApprovalPosture::Delegated,
        vec![guarded_proposal("{}"), guarded_proposal("{}")],
        std::slice::from_ref(&recorded),
    );

    assert_eq!(
        approvals.as_ref(),
        [
            InitialToolApproval::UserOverride {
                command: recorded.command(),
                denied_request: recorded.denied_request(),
            },
            InitialToolApproval::Delegated,
        ]
    );
}

/// a recorded override substitutes only where the judge would decide; a human-frozen selection
/// is never overridden.
#[test]
fn recorded_override_never_bypasses_a_human_selection() {
    let approvals = guarded_tool_approvals(
        signalbox_domain::ToolApprovalPosture::Human,
        vec![guarded_proposal("{}")],
        &[recorded_guarded_override()],
    );

    assert_eq!(approvals.as_ref(), [InitialToolApproval::Human]);
}

/// one identity is minted per ordered response part/request, approval stays pinned to the
/// advertised catalog snapshot, mixed auto/confirm policy parks without a continuation attempt,
/// and the adapter still receives a stopped race closure.
#[test]
fn tool_response_candidates_preserve_order_and_policy() {
    let schema =
        crate::ToolInputSchema::try_new(String::from(r#"{"properties":{},"type":"object"}"#))
            .expect("fixture schema is valid");
    let definition = crate::ToolDefinition::new(
        signalbox_domain::ToolName::try_new(String::from("automatic"))
            .expect("fixture name is valid"),
        String::from("Runs automatically."),
        schema,
        signalbox_domain::ToolPermissionDefault::Auto,
        signalbox_domain::ToolEffectClass::EffectFree,
    );
    let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
        definition,
        |_: &signalbox_domain::NormalizedToolArguments| Ok(()),
    )])
    .expect("one tool is unambiguous");
    let mut service = ModelCallExecutionService::new(
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
    let observation = tool_response();
    let advertised_tools = service.catalog.definitions();
    service.catalog = Arc::new(NoToolCatalog);
    let approvals = service.tool_approvals(
        &observation,
        DangerousToolAutoApproval::Disabled,
        &advertised_tools,
        &[],
    );
    assert_eq!(
        approvals.as_ref(),
        [
            InitialToolApproval::PolicyAuto,
            InitialToolApproval::Confirm
        ]
    );

    let ModelCallTerminalIdentityCandidates::ToolRound {
        continuing,
        stopped,
    } = service.next_terminal_identities(&observation, &approvals)
    else {
        panic!("tool response requires both race-safe closures");
    };
    assert_eq!(continuing.response_parts().len(), 3);
    assert_eq!(continuing.continuation_attempt(), None);
    let [
        ToolResponsePartIdentity::Text { .. },
        ToolResponsePartIdentity::ToolCall {
            approval: first_approval,
            ..
        },
        ToolResponsePartIdentity::ToolCall {
            approval: second_approval,
            ..
        },
    ] = continuing.response_parts()
    else {
        panic!("fixture response preserves one text part then two tool calls");
    };
    assert_eq!(*first_approval, InitialToolApproval::PolicyAuto);
    assert_eq!(*second_approval, InitialToolApproval::Confirm);

    let non_overridable_approvals = [
        InitialToolApproval::PolicyAuto,
        InitialToolApproval::AlwaysConfirm,
    ];
    service.ids = FixedIds::baseline();
    let ModelCallTerminalIdentityCandidates::ToolRound { continuing, .. } =
        service.next_terminal_identities(&observation, &non_overridable_approvals)
    else {
        panic!("tool response requires both race-safe closures");
    };
    assert_eq!(continuing.continuation_attempt(), None);
    assert_eq!(
        stopped,
        StoppedToolRoundModelCallIdentities::new(
            vec![
                StoppedToolResponsePartIdentity::text(identity(
                    33,
                    SemanticTranscriptEntryId::from_uuid,
                )),
                StoppedToolResponsePartIdentity::tool_call(
                    identity(34, SemanticTranscriptEntryId::from_uuid),
                    identity(62, ToolRequestId::from_uuid),
                    identity(35, SemanticTranscriptEntryId::from_uuid),
                    InitialToolApproval::PolicyAuto,
                ),
                StoppedToolResponsePartIdentity::tool_call(
                    identity(36, SemanticTranscriptEntryId::from_uuid),
                    identity(63, ToolRequestId::from_uuid),
                    identity(37, SemanticTranscriptEntryId::from_uuid),
                    InitialToolApproval::Confirm,
                ),
            ],
            identity(38, SemanticTranscriptEntryId::from_uuid),
            identity(41, ContextFrontierId::from_uuid),
        ),
        "lifecycle-dependent candidates receive a disjoint identity inventory"
    );
}

/// a credential-suppressed proposal bypasses the advertised execution policy and receives an
/// automatic safety denial.
#[test]
fn suppressed_proposal_forces_runtime_safety_denial() {
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
    );
    let response = signalbox_domain::ToolUsingAssistantResponse::try_from_parts(vec![
        signalbox_domain::AssistantResponsePart::ToolCall(
            signalbox_domain::ToolCallProposal::suppressed(
                signalbox_domain::ToolName::try_new(String::from("sandboxed_exec"))
                    .expect("fixture tool name is valid"),
            ),
        ),
    ])
    .expect("suppressed proposal remains one bounded logical request");
    let observation = ModelCallTerminalObservation::CompletedWithTools {
        response,
        retained_input_tokens: None,
        retained_output_tokens: None,
    };

    assert_eq!(
        service
            .tool_approvals(
                &observation,
                DangerousToolAutoApproval::ApproveAll,
                &[],
                &[]
            )
            .as_ref(),
        [InitialToolApproval::RuntimeSafetyDeny]
    );
}
/// attested imported text keeps its exact source-attested role, semantic source, imported
/// authority, and decoded text without acquiring a native input or call identity.
#[test]
fn frontier_rendering_preserves_imported_text_roles_and_sources() {
    let imported_user_entry = identity(110, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
    let imported_assistant_entry =
        identity(111, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
    let projected_user = SemanticTranscriptEntryRef::from_source(
        identity(112, SessionId::from_uuid),
        identity(113, SemanticTranscriptEntryId::from_uuid),
    );
    let projected_assistant = SemanticTranscriptEntryRef::from_source(
        identity(114, SessionId::from_uuid),
        identity(115, SemanticTranscriptEntryId::from_uuid),
    );
    let exact_user = ImportedText::new(String::from(" \timported\0user\r\n"));
    let exact_assistant = ImportedText::new(String::new());
    let entries = [
        (
            projected_user,
            SemanticTranscriptEntryPayload::Imported {
                imported_entry: imported_user_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                    exact_user.clone(),
                )),
            },
        ),
        (
            projected_assistant,
            SemanticTranscriptEntryPayload::Imported {
                imported_entry: imported_assistant_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                    exact_assistant.clone(),
                )),
            },
        ),
    ];

    let messages = render_frontier_messages(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |_| panic!("imported text must not request native accepted-input content"),
        |_| panic!("imported text must not request attachment facts"),
        std::iter::empty(),
    )
    .expect("attested imported text is conservatively renderable");

    assert_eq!(messages.len(), 2);
    let ModelConversationMessage::ImportedUser {
        source,
        imported_entry,
        content,
    } = &messages[0]
    else {
        panic!("attested imported user text must retain the imported user role")
    };
    assert_eq!(*source, projected_user);
    assert_eq!(*imported_entry, imported_user_entry);
    assert_eq!(content.as_str(), exact_user.as_str());
    let ModelConversationMessage::ImportedAssistant {
        source,
        imported_entry,
        content,
    } = &messages[1]
    else {
        panic!("attested imported assistant text must retain the imported assistant role")
    };
    assert_eq!(*source, projected_assistant);
    assert_eq!(*imported_entry, imported_assistant_entry);
    assert_eq!(content.as_str(), exact_assistant.as_str());
}

/// typed imported text or speaker absence remains model-invisible rather than guessing a role
/// or fabricating content.
#[test]
fn frontier_rendering_skips_imported_text_with_typed_absence() {
    let projected_session = identity(120, SessionId::from_uuid);
    let imported_entry = identity(121, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
    let exact_text = ImportedText::new(String::from("must remain hidden"));
    let entries = [
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(122, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::AttestedAbsent),
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(123, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::NotAttested),
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(124, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::AttestedAbsent,
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                    exact_text.clone(),
                )),
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(125, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::NotAttested,
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                    exact_text,
                )),
            },
        ),
    ];

    let messages = render_frontier_messages(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |_| panic!("imported absence must not request native accepted-input content"),
        |_| panic!("imported absence must not request attachment facts"),
        std::iter::empty(),
    )
    .expect("typed imported absence is conservatively skipped");

    assert!(
        messages.is_empty(),
        "typed absence cannot become provider-visible content"
    );
}

/// the conservative frontier renderer leaves every imported non-text vocabulary member
/// model-invisible without removing it from the semantic frontier or inventing native tool
/// facts.
#[test]
fn frontier_rendering_skips_every_imported_non_text_variant() {
    let projected_session = identity(130, SessionId::from_uuid);
    let imported_entry = identity(131, signalbox_domain::ImportedTranscriptEntryId::from_uuid);
    let speaker = ImportedSourceAttestation::Attested(ImportedSpeaker::User);
    let entries = [
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(132, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker.clone(),
                content: ImportedTranscriptContent::SourceEvent {
                    source_type: ImportedSourceAttestation::NotAttested,
                },
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(133, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker.clone(),
                content: ImportedTranscriptContent::SourceMessageBlock {
                    source_type: ImportedSourceAttestation::NotAttested,
                },
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(134, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker.clone(),
                content: ImportedTranscriptContent::ToolCall {
                    source_call_id: ImportedSourceAttestation::NotAttested,
                    name: ImportedSourceAttestation::NotAttested,
                    input: ImportedSourceAttestation::NotAttested,
                    caller: ImportedSourceAttestation::NotAttested,
                },
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(135, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker.clone(),
                content: ImportedTranscriptContent::ToolResult {
                    source_call_id: ImportedSourceAttestation::NotAttested,
                    content: ImportedSourceAttestation::NotAttested,
                    is_error: ImportedSourceAttestation::NotAttested,
                },
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(136, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker.clone(),
                content: ImportedTranscriptContent::Thinking {
                    thinking: ImportedSourceAttestation::NotAttested,
                    signature: ImportedSourceAttestation::NotAttested,
                },
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(137, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker.clone(),
                content: ImportedTranscriptContent::RedactedThinking {
                    data: ImportedSourceAttestation::NotAttested,
                },
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(138, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker.clone(),
                content: ImportedTranscriptContent::Document {
                    source: ImportedSourceAttestation::NotAttested,
                },
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                projected_session,
                identity(139, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: speaker,
                content: ImportedTranscriptContent::MessageContentAbsent(
                    ImportedMessageContentAbsence::EmptyBlockArray,
                ),
            },
        ),
    ];

    let messages = render_frontier_messages(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |_| panic!("imported non-text must not request native accepted-input content"),
        |_| panic!("imported non-text must not request attachment facts"),
        std::iter::empty(),
    )
    .expect("imported non-text is conservatively skipped");

    assert!(
        messages.is_empty(),
        "non-text imported history cannot become a native provider message"
    );
}

/// mixed semantic content keeps exact role order and source-qualified provenance, including
/// entries created by a different session; terminal markers do not invent provider-visible
/// messages.
#[test]
fn frontier_rendering_preserves_mixed_roles_and_inherited_sources() {
    let inherited_session = identity(90, SessionId::from_uuid);
    let current_session = identity(1, SessionId::from_uuid);
    let inherited_input = identity(91, AcceptedInputId::from_uuid);
    let current_input = identity(92, AcceptedInputId::from_uuid);
    let failed_input = identity(99, AcceptedInputId::from_uuid);
    let producing_call = identity(93, ModelCallId::from_uuid);
    let inherited_content =
        UserContent::try_text(String::from("inherited user request")).expect("valid text");
    let current_content =
        UserContent::try_text(String::from("current user request")).expect("valid text");
    let failed_content =
        UserContent::try_text(String::from("failed user request")).expect("valid text");
    let assistant_text = AssistantText::try_new(String::from("inherited assistant reply"))
        .expect("valid assistant text");
    let origin_contents = std::collections::HashMap::from([
        (inherited_input, inherited_content.clone()),
        (current_input, current_content.clone()),
        (failed_input, failed_content.clone()),
    ]);
    let entries = [
        (
            SemanticTranscriptEntryRef::from_source(
                inherited_session,
                identity(94, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: inherited_input,
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                inherited_session,
                identity(95, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value: assistant_text.clone(),
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                inherited_session,
                identity(96, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::TurnCompleted {
                turn: identity(97, TurnId::from_uuid),
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                current_session,
                identity(98, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: failed_input,
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                current_session,
                identity(100, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::TurnFailed {
                turn: identity(101, TurnId::from_uuid),
            },
        ),
        (
            SemanticTranscriptEntryRef::from_source(
                current_session,
                identity(102, SemanticTranscriptEntryId::from_uuid),
            ),
            SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: current_input,
            },
        ),
    ];

    let messages = render_frontier_messages(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |accepted_input| origin_contents.get(&accepted_input).cloned(),
        |_| None,
        [],
    )
    .expect("the admitted mixed text frontier renders");

    expect![[r#"
        [
            User {
                source: SemanticTranscriptEntryRef {
                    source_session: SessionId(
                        00000000-0000-0000-0000-00000000005a,
                    ),
                    entry: SemanticTranscriptEntryId(
                        00000000-0000-0000-0000-00000000005e,
                    ),
                },
                accepted_input: AcceptedInputId(
                    00000000-0000-0000-0000-00000000005b,
                ),
                content: ModelUserContent {
                    parts: [
                        Text(
                            NonEmptyUnicodeText(<redacted>),
                        ),
                    ],
                },
            },
            Assistant {
                source: SemanticTranscriptEntryRef {
                    source_session: SessionId(
                        00000000-0000-0000-0000-00000000005a,
                    ),
                    entry: SemanticTranscriptEntryId(
                        00000000-0000-0000-0000-00000000005f,
                    ),
                },
                producing_call: ModelCallId(
                    00000000-0000-0000-0000-00000000005d,
                ),
                content: AssistantText(
                    NonEmptyUnicodeText(<redacted>),
                ),
            },
            User {
                source: SemanticTranscriptEntryRef {
                    source_session: SessionId(
                        00000000-0000-0000-0000-000000000001,
                    ),
                    entry: SemanticTranscriptEntryId(
                        00000000-0000-0000-0000-000000000062,
                    ),
                },
                accepted_input: AcceptedInputId(
                    00000000-0000-0000-0000-000000000063,
                ),
                content: ModelUserContent {
                    parts: [
                        Text(
                            NonEmptyUnicodeText(<redacted>),
                        ),
                    ],
                },
            },
            User {
                source: SemanticTranscriptEntryRef {
                    source_session: SessionId(
                        00000000-0000-0000-0000-000000000001,
                    ),
                    entry: SemanticTranscriptEntryId(
                        00000000-0000-0000-0000-000000000066,
                    ),
                },
                accepted_input: AcceptedInputId(
                    00000000-0000-0000-0000-00000000005c,
                ),
                content: ModelUserContent {
                    parts: [
                        Text(
                            NonEmptyUnicodeText(<redacted>),
                        ),
                    ],
                },
            },
        ]
    "#]]
    .assert_debug_eq(&messages);
    assert_eq!(
        &messages[0],
        &ModelConversationMessage::User {
            source: entries[0].0,
            accepted_input: inherited_input,
            content: rendered_text(inherited_content),
        }
    );
    assert_eq!(
        &messages[1],
        &ModelConversationMessage::Assistant {
            source: entries[1].0,
            producing_call,
            content: assistant_text,
        }
    );
    assert_eq!(
        &messages[2],
        &ModelConversationMessage::User {
            source: entries[3].0,
            accepted_input: failed_input,
            content: rendered_text(failed_content),
        }
    );
    assert_eq!(
        &messages[3],
        &ModelConversationMessage::User {
            source: entries[5].0,
            accepted_input: current_input,
            content: rendered_text(current_content),
        }
    );
}

/// durable request, attempt, and denial authority renders reference-only tool semantics into
/// their exact provider-visible roles without changing source order.
#[test]
fn frontier_rendering_resolves_exact_tool_roles_in_source_order() {
    let completed_request = model_tool_request(0);
    let denied_request = model_tool_request(1);
    let closed_request = model_tool_request(2);
    let completed_use_source = SemanticTranscriptEntryRef::from_source(
        completed_request.session(),
        identity(110, SemanticTranscriptEntryId::from_uuid),
    );
    let completed_result_source = SemanticTranscriptEntryRef::from_source(
        completed_request.session(),
        identity(111, SemanticTranscriptEntryId::from_uuid),
    );
    let denied_use_source = SemanticTranscriptEntryRef::from_source(
        denied_request.session(),
        identity(112, SemanticTranscriptEntryId::from_uuid),
    );
    let denied_result_source = SemanticTranscriptEntryRef::from_source(
        denied_request.session(),
        identity(113, SemanticTranscriptEntryId::from_uuid),
    );
    let closed_use_source = SemanticTranscriptEntryRef::from_source(
        closed_request.session(),
        identity(114, SemanticTranscriptEntryId::from_uuid),
    );
    let closed_result_source = SemanticTranscriptEntryRef::from_source(
        closed_request.session(),
        identity(115, SemanticTranscriptEntryId::from_uuid),
    );
    let completed_result = ToolResultContent::Text(
        ToolResultText::try_new(String::from(r#"{"timezone":"UTC"}"#))
            .expect("fixture result is valid"),
    );
    let attempt_id = identity(116, signalbox_domain::ToolAttemptId::from_uuid);
    let signalbox_domain::ReconstitutedToolAttempt::Ended(completed_attempt) =
        ToolAttemptReconstitutionInput::new(
            attempt_id,
            completed_request.id(),
            completed_request.session(),
            completed_request.turn(),
            identity(117, TurnAttemptId::from_uuid),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                result: completed_result.clone(),
            }),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported")
    else {
        panic!("terminal fixture reconstitutes as ended")
    };
    let denial_reason = ToolDenialReason::try_new(String::from("user declined"))
        .expect("fixture denial reason is valid");
    let denial_command = DecideToolRequest::try_new(
        identity(118, DurableCommandId::from_uuid),
        denied_request.id(),
        ToolApprovalDecision::Deny {
            reason: Some(denial_reason.clone()),
        },
    )
    .expect("the fixture command identity is admitted")
    .prepare_applied(&denied_request)
    .expect("the command names the exact request");
    let denial = ToolApprovalResolutionReconstitutionInput::user_command(denial_command)
        .reconstitute()
        .expect("user denial provenance is implemented");
    let entries = [
        (
            completed_use_source,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: completed_request.producing_call(),
                request: completed_request.id(),
            },
        ),
        (
            completed_result_source,
            SemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: attempt_id,
            },
        ),
        (
            denied_use_source,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: denied_request.producing_call(),
                request: denied_request.id(),
            },
        ),
        (
            denied_result_source,
            SemanticTranscriptEntryPayload::ToolDenied {
                request: denied_request.id(),
            },
        ),
        (
            closed_use_source,
            SemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: closed_request.producing_call(),
                request: closed_request.id(),
            },
        ),
        (
            closed_result_source,
            SemanticTranscriptEntryPayload::ToolClosed {
                request: closed_request.id(),
            },
        ),
    ];
    let evidence = [
        ResolvedToolConversationEntry::AssistantToolUse {
            source: completed_use_source,
            request: completed_request.clone(),
        },
        ResolvedToolConversationEntry::ExecutionResult {
            source: completed_result_source,
            request: completed_request.clone(),
            attempt: completed_attempt,
        },
        ResolvedToolConversationEntry::AssistantToolUse {
            source: denied_use_source,
            request: denied_request.clone(),
        },
        ResolvedToolConversationEntry::Denied {
            source: denied_result_source,
            request: denied_request.clone(),
            approval: denial,
        },
        ResolvedToolConversationEntry::AssistantToolUse {
            source: closed_use_source,
            request: closed_request.clone(),
        },
        ResolvedToolConversationEntry::Closed {
            source: closed_result_source,
            request: closed_request.clone(),
        },
    ];

    let messages = render_frontier_messages(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |_| None,
        |_| None,
        evidence.iter(),
    )
    .expect("exact tool evidence renders");

    assert_eq!(
        messages.as_ref(),
        [
            ModelConversationMessage::AssistantToolUse {
                source: completed_use_source,
                producing_call: completed_request.producing_call(),
                request: completed_request.clone(),
            },
            ModelConversationMessage::ToolResult {
                source: completed_result_source,
                request: completed_request.id(),
                content: ModelToolResultContent::Success(completed_result),
            },
            ModelConversationMessage::AssistantToolUse {
                source: denied_use_source,
                producing_call: denied_request.producing_call(),
                request: denied_request.clone(),
            },
            ModelConversationMessage::ToolResult {
                source: denied_result_source,
                request: denied_request.id(),
                content: ModelToolResultContent::Denied {
                    reason: Some(denial_reason),
                },
            },
            ModelConversationMessage::AssistantToolUse {
                source: closed_use_source,
                producing_call: closed_request.producing_call(),
                request: closed_request.clone(),
            },
            ModelConversationMessage::ToolResult {
                source: closed_result_source,
                request: closed_request.id(),
                content: ModelToolResultContent::ClosedByTurnEnd,
            },
        ]
    );
}

/// a terminal attempt from another turn cannot supply authority for a tool-result semantic
/// entry.
#[test]
fn frontier_rendering_rejects_cross_turn_tool_result_evidence() {
    let request = model_tool_request(0);
    let source = SemanticTranscriptEntryRef::from_source(
        request.session(),
        identity(120, SemanticTranscriptEntryId::from_uuid),
    );
    let attempt_id = identity(121, signalbox_domain::ToolAttemptId::from_uuid);
    let signalbox_domain::ReconstitutedToolAttempt::Ended(cross_turn_attempt) =
        ToolAttemptReconstitutionInput::new(
            attempt_id,
            request.id(),
            request.session(),
            identity(122, TurnId::from_uuid),
            identity(123, TurnAttemptId::from_uuid),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(String::from("cross-wired"))
                        .expect("fixture result is valid"),
                ),
            }),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported")
    else {
        panic!("terminal fixture reconstitutes as ended")
    };
    let payload = SemanticTranscriptEntryPayload::ToolExecutionResult {
        attempt: attempt_id,
    };
    let evidence = ResolvedToolConversationEntry::ExecutionResult {
        source,
        request,
        attempt: cross_turn_attempt,
    };

    let error = render_frontier_messages([(source, &payload)], |_| None, |_| None, [&evidence])
        .expect_err("cross-turn tool evidence must fail closed");

    assert_eq!(
        error,
        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence { entry: source }
    );
}

/// a newly committed Prepared checkpoint ends the invocation before capability preparation or
/// authorization.
#[tokio::test]
async fn checkpoint_stops_before_every_later_port() {
    let checkpoint = identity(70, ModelCallId::from_uuid);
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(PrepareModelCallOutcome::Checkpointed(checkpoint))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        UnusedProvider,
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        service
            .execute(identity(1, SessionId::from_uuid))
            .await
            .expect("checkpointing succeeds"),
        ModelCallExecutionOutcome::Checkpointed(checkpoint)
    );
    let (_, prepare, ..) = service.into_parts();
    assert_eq!(prepare.calls, 1);
}

/// a proven fresh-identity collision retries only the rolled-back prepare transaction with
/// fresh candidates.
#[tokio::test]
async fn prepare_identity_collision_retries_transaction_only() {
    let checkpoint = identity(71, ModelCallId::from_uuid);
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [
                Err(FakeError::IdentityCollision),
                Ok(PrepareModelCallOutcome::Checkpointed(checkpoint)),
            ]
            .into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        UnusedProvider,
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        service
            .execute(identity(1, SessionId::from_uuid))
            .await
            .expect("proven collision is retryable"),
        ModelCallExecutionOutcome::Checkpointed(checkpoint)
    );
    let (_, prepare, ..) = service.into_parts();
    assert_eq!(prepare.calls, 2);
}

/// durable cancellation during capability preparation is
/// authoritative no-work, not a local capability failure to terminalize.
#[tokio::test]
async fn capability_preparation_cancellation_stops_without_failure_commit() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, prepare, _, _, _, provider, ..) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
}

#[tokio::test]
async fn prepared_capability_receives_the_configured_tool_catalog_snapshot() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let definition = crate::ToolDefinition::new(
        ToolName::try_new(String::from("current_time")).expect("fixture tool name"),
        String::from("Returns the current UTC time."),
        crate::ToolInputSchema::try_new(String::from(
            r#"{"additionalProperties":false,"properties":{},"type":"object"}"#,
        ))
        .expect("fixture schema"),
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    );
    let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
        definition.clone(),
        |_arguments: &NormalizedToolArguments| Ok(()),
    )])
    .expect("fixture catalog");
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    )
    .with_tool_catalog(catalog);

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, _, _, provider, ..) = service.into_parts();
    assert_eq!(
        provider.last_prepared_tools(),
        Some([definition].as_slice())
    );
}

/// the execution loop presents the prepare transaction's exact frozen-epoch system prompt to
/// the provider port with the capability operation; a promptless epoch presents none.
#[tokio::test]
async fn prepared_capability_receives_the_frozen_epoch_system_prompt() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
        .expect("fixture prompt is admissible");
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(PrepareModelCallOutcome::Ready {
                reasoning_provenance: Box::new([]),
                request: Box::new(request.clone()),
                credential_reference: credential_reference(),
                dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
                recorded_user_overrides: Box::new([]),
                system_prompt: Some(prompt.clone()),
                tool_entries: Box::new([]),
            })]
            .into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, _, _, provider, ..) = service.into_parts();
    assert_eq!(
        provider.last_prepared_system_prompt(),
        Some(Some(prompt.as_str()))
    );

    let mut promptless_service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        promptless_service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, _, _, provider, ..) = promptless_service.into_parts();
    assert_eq!(provider.last_prepared_system_prompt(), Some(None));
}

/// docs/spec/model-call-execution.md: a trustworthy capability failure
/// survives a failed guarded closure and explicit service decomposition,
/// then resubmits without repeating capability preparation.
#[tokio::test]
async fn capability_failure_commit_retains_evidence_across_handoff() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let turn = request.turn();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::Pending)].into(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::Infrastructure
        ))
    ));
    assert_eq!(
        service.retained_state(),
        Some(&RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                session,
                turn,
                call,
                cause: PreparedModelCallFailureCause::CapabilityKnownFailure,
                attachment_failure: None,
            },
        })
    );

    let (
        ids,
        prepare,
        failure,
        authorization,
        observation,
        provider,
        gate,
        catalog,
        retained,
        tool_round_limit,
    ) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 0);
    assert_eq!(provider.capability_preparation_count(), 1);
    let mut resumed = ModelCallExecutionService::from_parts(
        ids,
        prepare,
        failure,
        authorization,
        observation,
        provider,
        gate,
        catalog,
        retained,
        tool_round_limit,
    );
    assert!(matches!(
        resumed.execute(identity(99, SessionId::from_uuid)).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::Infrastructure
        ))
    ));
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = resumed.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 2);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(
        retained,
        Some(RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                session,
                turn,
                call,
                cause: PreparedModelCallFailureCause::CapabilityKnownFailure,
                attachment_failure: None,
            },
        })
    );
}

/// A capability failure that commits terminalizes the turn: the service
/// returns the exact failed turn the transaction recorded and retains
/// nothing to resubmit. Without this the turn stops with no terminal
/// outcome recorded and stays non-terminal forever.
#[tokio::test]
async fn committed_capability_failure_returns_the_recorded_terminal_turn() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    // Captured before the request moves into the fake. `ScriptedFailure`
    // returns the scripted failed turn whatever call it is handed, so a
    // commit addressed to a stale or unrelated call of the same session
    // would satisfy both the outcome comparison and a session-only
    // assertion while terminalizing the wrong call.
    let prepared_call = request.call().id();
    let failed = failed_turn_fixture();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the capability failure commits"),
        ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    // The failure must belong to the prepared turn's own session *and*
    // name the call that was prepared. Counting the attempt alone would
    // accept a terminal turn written against an unrelated fixture session,
    // and checking the session alone would accept one written against
    // another call of this session.
    assert_eq!(failure.recorded.len(), 1);
    let committed = &failure.recorded[0];
    assert_eq!(committed.session, session);
    assert_eq!(
        committed.call, prepared_call,
        "the committed failure must terminalize the prepared call"
    );
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// a typed attachment failure terminalizes the prepared call
/// before durable send authorization or provider interaction, and the
/// exact attachment evidence reaches the guarded failure transaction.
#[tokio::test]
async fn attachment_failure_closes_before_durable_authorization() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let failed = failed_turn_fixture();
    let attachment_failure = AttachmentPreparationFailure::Missing;
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        AttachmentFailureProvider {
            failure: attachment_failure,
            preparation_count: 0,
        },
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the typed attachment failure commits before authorization"),
        ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
    );
    let (_, _, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(failure.calls, 1);
    let committed = &failure.recorded[0];
    assert_eq!(
        committed.cause,
        PreparedModelCallFailureCause::CapabilityKnownFailure
    );
    assert_eq!(committed.attachment_failure, Some(attachment_failure));
    assert_eq!(provider.preparation_count, 1);
    assert!(retained.is_none());
}

/// unavailable attachment verification leaves the exact call
/// prepared without durable failure or send authorization.
#[tokio::test]
async fn attachment_unavailable_leaves_prepared_without_authorization() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        AttachmentFailureProvider {
            failure: AttachmentPreparationFailure::Unavailable,
            preparation_count: 0,
        },
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("unavailable attachment verification is a typed retryable outcome"),
        ModelCallExecutionOutcome::AttachmentUnavailable
    );
    let (_, _, failure, _, _, _, _, _, retained, _) = service.into_parts();
    assert_eq!(failure.calls, 0);
    assert!(retained.is_none());
}

/// An identity collision inside failure closure is retried with fresh
/// identities instead of surfacing as an operator failure, so a raced
/// terminalization still records exactly one terminal turn.
#[tokio::test]
async fn capability_failure_commit_retries_an_identity_collision() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    // Captured before the request moves into the fake, so the retry is
    // compared against the call that was actually prepared rather than only
    // against itself.
    let prepared_call = request.call().id();
    let failed = failed_turn_fixture();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Err(FakeError::IdentityCollision), Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the retried capability failure commits"),
        ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
    );
    let (_, _, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(failure.calls, 2);
    // Both attempts must address the prepared call — agreeing only with
    // each other would still pass if the service handed the same stale or
    // unrelated call to both — and the second must carry *fresh*
    // identities, which is the whole point of retrying a collision. A
    // retry reusing the colliding identities would collide again forever.
    let [first, second] = failure.recorded.as_slice() else {
        panic!(
            "expected exactly two failure-commit attempts, got {}",
            failure.recorded.len()
        )
    };
    assert_eq!(first.session, session);
    assert_eq!(second.session, session);
    assert_eq!(first.call, prepared_call);
    assert_eq!(second.call, prepared_call);
    // Every component of the bundle has to be refreshed, not just one.
    // Whole-bundle inequality passes when a retry mints a new failure
    // entry but reuses the terminal frontier (or the reverse), and if the
    // reused component is the one that collided the real transaction
    // rejects every retry and the turn stays wedged forever.
    assert_ne!(
        first.identities.failure_entry(),
        second.identities.failure_entry(),
        "a retried identity collision must mint a fresh failure entry"
    );
    assert_ne!(
        first.identities.terminal_frontier(),
        second.identities.terminal_frontier(),
        "a retried identity collision must mint a fresh terminal frontier"
    );
    assert_eq!(provider.capability_preparation_count(), 1);
    assert!(retained.is_none());
}

/// a turn that reaches the automatic tool-round limit closes with its distinct terminal reason
/// before provider entry. This prevents a runaway paid provider loop without misreporting
/// saturation as a capability failure.
#[tokio::test]
async fn tool_round_limit_fires_before_provider_entry() {
    const CONFIGURED_TOOL_ROUND_LIMIT: usize = 7;
    let (request, tool_entries, failed) = tool_round_saturated_fixture(CONFIGURED_TOOL_ROUND_LIMIT);
    let session = request.session();
    // Captured before the request moves into the fake: the committed
    // terminalization has to name *this* saturated call.
    let saturated_call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([]),
        InProcessAttemptDispatchGate::default(),
        Some(CONFIGURED_TOOL_ROUND_LIMIT),
    );
    let captured = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();

    assert_eq!(
        service
            .execute(session)
            .with_subscriber(subscriber)
            .await
            .expect("the saturated turn closes with its own terminal reason"),
        ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
    );
    assert!(
        captured
            .text()
            .contains("terminal_outcome=\"tool_round_limit_reached\""),
        "the service terminalization must expose the tool_round_limit_reached label"
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    // A count alone accepts `fail_prepared` called with an unrelated
    // session or call, which would terminalize something other than the
    // saturated turn while this test still claimed it was closed.
    assert_eq!(failure.recorded.len(), 1);
    let committed = &failure.recorded[0];
    assert_eq!(committed.session, session);
    assert_eq!(committed.call, saturated_call);
    assert_eq!(
        committed.cause,
        PreparedModelCallFailureCause::ToolRoundLimitReached
    );
    assert_eq!(
        provider.capability_preparation_count(),
        0,
        "a saturated turn must not reach provider capability preparation"
    );
    assert_eq!(
        provider.interaction_count(),
        0,
        "a saturated turn must not reach provider interaction"
    );
    assert!(retained.is_none());
}

/// Sums the content the rendered messages hold, message kind by message
/// kind.
///
/// This is the renderer's side of the accounting-fidelity comparison. It
/// reads what each message carries *after* the clone, so a term the ceiling
/// forgot surfaces as a difference instead of as two copies of the same
/// omission agreeing with each other.
fn rendered_content_bytes(messages: &[ModelConversationMessage]) -> usize {
    messages.iter().fold(0_usize, |total, message| {
        let bytes = match message {
            ModelConversationMessage::ContextSummary { content, .. }
            | ModelConversationMessage::Assistant { content, .. } => content.as_str().len(),
            ModelConversationMessage::ProviderCompaction { block, .. } => block.as_json().len(),
            ModelConversationMessage::ProviderReasoning { item, .. } => item.as_json().len(),
            // Mirrors `user_content_text_bytes`: attachment stubs carry a
            // fixed-width digest and bounded declarations held under
            // `MAX_RENDERED_ATTACHMENT_STUB_BYTES`, so they sit outside the
            // retained-content sum on both sides of this comparison.
            ModelConversationMessage::User { content, .. } => {
                content
                    .parts()
                    .iter()
                    .fold(0_usize, |total, part| match part {
                        ModelUserContentPart::Text(value) => {
                            total.saturating_add(value.as_str().len())
                        }
                        ModelUserContentPart::AttachmentStub(_) => total,
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
fn counted_frontier_bytes(
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

/// The retained-content accounting counts exactly what a render clones,
/// including the frontier content no tool evidence names. Byte accounting
/// that drifts from the renderer would let the ceiling admit more content
/// than it names, so the sum is checked against the messages the same
/// frontier actually produces — with assistant text and origin user content
/// present, which a tool-only accounting would clone without counting.
#[test]
fn projected_frontier_content_bytes_matches_the_render_over_tool_and_text_entries() {
    let assistant_text = "the assistant narrates the round it is about to run";
    let (request, tool_entries, _) =
        tool_round_saturated_fixture_with_assistant_text(3, Some(assistant_text));
    let counted = counted_frontier_bytes(&request, &tool_entries);
    let no_entries: [(SemanticTranscriptEntryRef, &SemanticTranscriptEntryPayload); 0] = [];
    let tool_evidence_only =
        projected_frontier_content_bytes(no_entries, |_| None, tool_entries.iter());
    let operation = PreparedModelOperation::render(
        request,
        credential_reference(),
        None,
        Box::new([]),
        &tool_entries,
        &[],
    )
    .expect("the fixture frontier renders");
    let rendered = rendered_content_bytes(operation.messages());
    assert!(
        tool_evidence_only > 0,
        "the fixture must retain tool content for the comparison to mean anything"
    );
    assert_eq!(
        counted, rendered,
        "the ceiling must count exactly the bytes the renderer clones"
    );
    assert!(
        counted >= tool_evidence_only + assistant_text.len(),
        "the ceiling must count the frontier content no tool evidence names: \
         counted {counted}, tool evidence {tool_evidence_only}"
    );
}

/// Every payload kind the renderer clones is counted, term for term.
///
/// The fixture frontier reaches only tool evidence, assistant text, and
/// origin content. Delegation content, delivered outcomes, context
/// summaries, and imported text are cloned by the same renderer and were
/// the kinds a tool-only accounting left unbounded, so they are compared
/// here against both the rendered messages and an explicit per-term sum.
#[test]
fn projected_frontier_content_bytes_counts_every_payload_kind_the_render_clones() {
    let session = identity(300, SessionId::from_uuid);
    let child = identity(301, SessionId::from_uuid);
    let turn = identity(302, TurnId::from_uuid);
    let child_turn = identity(303, TurnId::from_uuid);
    let producing_call = identity(304, ModelCallId::from_uuid);
    let spawning_request = identity(305, ToolRequestId::from_uuid);
    let awaiting_request = identity(306, ToolRequestId::from_uuid);
    let background_request = identity(307, ToolRequestId::from_uuid);
    let peer_message = identity(308, DelegationMessageId::from_uuid);
    let origin_input = identity(309, AcceptedInputId::from_uuid);
    let steering_input = identity(310, AcceptedInputId::from_uuid);
    let imported_entry = identity(311, ImportedTranscriptEntryId::from_uuid);
    let selected = identity(312, DirectModelSelection::from_uuid);
    let source = |value: u128| {
        SemanticTranscriptEntryRef::from_source(
            session,
            identity(value, SemanticTranscriptEntryId::from_uuid),
        )
    };

    let imported_user = ImportedText::new(String::from("imported user question"));
    let imported_assistant = ImportedText::new(String::from("imported assistant answer"));
    let unattested = ImportedText::new(String::from("source event type never rendered"));
    let origin_text = String::from("the origin request this turn answers");
    let steering_text = String::from("steering added mid-turn");
    let task = DelegationContent::try_new(String::from("the delegated task"))
        .expect("fixture task content is valid");
    let peer = DelegationContent::try_new(String::from("a peer message"))
        .expect("fixture peer content is valid");
    let foreground_result = DelegationContent::try_new(String::from("the awaited result"))
        .expect("fixture foreground content is valid");
    let background_result = DelegationContent::try_new(String::from("a later child result"))
        .expect("fixture background content is valid");
    let summary = AssistantText::try_new(String::from("a summary standing in for a range"))
        .expect("fixture summary text is valid");
    let assistant = AssistantText::try_new(String::from("assistant prose with no bound"))
        .expect("fixture assistant text is valid");
    let outcome = |content: DelegationContent| {
        DelegationOutcome::reconstitute(
            signalbox_domain::DelegationOutcomeKind::ResultReturned,
            Some(content),
            signalbox_domain::DelegationOutcomeReason::ChildCompleted,
            signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
                session: child,
                turn: child_turn,
            },
        )
        .expect("fixture child outcome is correlated")
    };
    let origin_contents = BTreeMap::from([
        (
            origin_input,
            UserContent::try_text(origin_text.clone()).expect("fixture origin text is valid"),
        ),
        (
            steering_input,
            UserContent::try_text(steering_text.clone()).expect("fixture steering text is valid"),
        ),
    ]);

    let entries = [
        (
            source(400),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                    imported_user.clone(),
                )),
            },
        ),
        (
            source(401),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                content: ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(
                    imported_assistant.clone(),
                )),
            },
        ),
        // Renders no message at all, so it must contribute no bytes.
        (
            source(402),
            SemanticTranscriptEntryPayload::Imported {
                imported_entry,
                source_speaker: ImportedSourceAttestation::NotAttested,
                content: ImportedTranscriptContent::SourceEvent {
                    source_type: ImportedSourceAttestation::Attested(unattested),
                },
            },
        ),
        (
            source(403),
            SemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: origin_input,
            },
        ),
        (
            source(404),
            SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: steering_input,
                source_turn: turn,
            },
        ),
        (
            source(405),
            SemanticTranscriptEntryPayload::DelegatedTask {
                spawning_request,
                parent_session: session,
                parent_turn: turn,
                content: task.clone(),
            },
        ),
        (
            source(406),
            SemanticTranscriptEntryPayload::DelegationMessage {
                spawning_request,
                message: peer_message,
                sender: session,
                recipient: child,
                delivery_sequence: NonZeroU64::MIN,
                content: peer.clone(),
            },
        ),
        (
            source(407),
            SemanticTranscriptEntryPayload::DelegationResult {
                awaiting_request,
                spawning_request,
                child,
                mode: DelegationWaitMode::Foreground,
                delivery_sequence: None,
                outcome: Box::new(outcome(foreground_result.clone())),
            },
        ),
        (
            source(408),
            SemanticTranscriptEntryPayload::DelegationResult {
                awaiting_request: background_request,
                spawning_request,
                child,
                mode: DelegationWaitMode::Background,
                delivery_sequence: Some(NonZeroU64::new(2).expect("two is positive")),
                outcome: Box::new(outcome(background_result.clone())),
            },
        ),
        (
            source(409),
            SemanticTranscriptEntryPayload::ContextSummary {
                producing_call,
                summarized: ContextCompactionRange::inclusive(source(400), source(401)),
                value: summary.clone(),
            },
        ),
        (
            source(410),
            SemanticTranscriptEntryPayload::AssistantText {
                producing_call,
                value: assistant.clone(),
            },
        ),
        (
            source(411),
            SemanticTranscriptEntryPayload::ModelIdentityChanged {
                turn,
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                selected,
            },
        ),
        (
            source(412),
            SemanticTranscriptEntryPayload::TurnCompleted { turn },
        ),
    ];

    let counted = projected_frontier_content_bytes(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |accepted_input| origin_contents.get(&accepted_input),
        std::iter::empty(),
    );
    let messages = render_frontier_messages(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |accepted_input| origin_contents.get(&accepted_input).cloned(),
        |_| None,
        std::iter::empty(),
    )
    .expect("the payload fixture renders");

    assert_eq!(
        counted,
        rendered_content_bytes(&messages),
        "the ceiling must count exactly the bytes the renderer clones"
    );
    // An equality between two sums can also be satisfied by both sides
    // dropping the same term, so the expected total is spelled out.
    assert_eq!(
        counted,
        imported_user.as_str().len()
            + imported_assistant.as_str().len()
            + origin_text.len()
            + steering_text.len()
            + task.as_str().len()
            + peer.as_str().len()
            + foreground_result.as_str().len()
            + background_result.as_str().len()
            + summary.as_str().len()
            + assistant.as_str().len(),
        "every cloned payload kind must contribute its exact content bytes"
    );
}

/// A frontier over-bound only by its assistant text is refused, and refused
/// before anything is cloned.
///
/// Assistant text carries no length bound of its own beyond the transport
/// cap on a single response, so a ceiling that counted tool evidence alone
/// would clone this frontier while reporting it as within bounds. The limit
/// here is exactly the same frontier's byte count without the text, which
/// makes the text the only reason the render is refused; the unrenderable
/// variant then shows the refusal still precedes message construction.
#[test]
fn assistant_text_over_bound_frontiers_are_refused_before_any_clone() {
    let assistant_text = "assistant prose that no tool-evidence accounting would ever see";
    let (plain_request, plain_entries, _) = tool_round_saturated_fixture(2);
    let plain_bytes = counted_frontier_bytes(&plain_request, &plain_entries);
    // Control: at this exact ceiling the same frontier without the text
    // renders, so the refusal below is caused by the text and not by a
    // ceiling too small for the fixture's tool evidence.
    PreparedModelOperation::render_within(
        plain_request,
        credential_reference(),
        None,
        Box::new([]),
        &plain_entries,
        &[],
        plain_bytes,
    )
    .expect("the text-free frontier renders at its own byte count");

    let (request, tool_entries, _) =
        tool_round_saturated_fixture_with_assistant_text(2, Some(assistant_text));
    let error = PreparedModelOperation::render_within(
        request.clone(),
        credential_reference(),
        None,
        Box::new([]),
        &tool_entries,
        &[],
        plain_bytes,
    )
    .expect_err("an assistant-text-heavy frontier is refused");
    assert_eq!(
        error,
        ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
            observed_bytes: plain_bytes + assistant_text.len(),
            limit_bytes: plain_bytes,
        },
        "the refusal must report the assistant text it counted"
    );

    // The same refusal still wins over a rendering failure, which places it
    // before the clones the renderer would perform.
    let mut unrenderable = tool_entries.into_vec();
    unrenderable.push(
        unrenderable
            .first()
            .expect("the fixture carries tool evidence")
            .clone(),
    );
    assert!(
        matches!(
            PreparedModelOperation::render_within(
                request.clone(),
                credential_reference(),
                None,
                Box::new([]),
                &unrenderable,
                &[],
                MAX_RETAINED_FRONTIER_CONTENT_BYTES,
            ),
            Err(ModelFrontierRenderingError::DuplicateToolEvidence { .. })
        ),
        "the duplicated evidence must be unrenderable for this ordering claim to hold"
    );
    assert!(
        matches!(
            PreparedModelOperation::render_within(
                request,
                credential_reference(),
                None,
                Box::new([]),
                &unrenderable,
                &[],
                plain_bytes,
            ),
            Err(ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded { .. })
        ),
        "the content ceiling must win over the rendering failure it precedes"
    );
}

/// The ceiling is enforced ahead of message construction. A frontier that is
/// both over-bound and unrenderable is refused for its content, which places
/// the guard before `render_frontier_messages` and therefore before the
/// clones that would exhaust memory — the ordering a guard reached only
/// after rendering cannot provide.
#[test]
fn retained_frontier_content_ceiling_precedes_message_rendering() {
    let (request, tool_entries, _) = tool_round_saturated_fixture(2);
    let mut unrenderable = tool_entries.into_vec();
    unrenderable.push(
        unrenderable
            .first()
            .expect("the fixture carries tool evidence")
            .clone(),
    );
    // Control: with the ceiling out of the way this evidence fails inside
    // the renderer, so the refusal below is genuinely the earlier one.
    assert!(
        matches!(
            PreparedModelOperation::render_within(
                request.clone(),
                credential_reference(),
                None,
                Box::new([]),
                &unrenderable,
                &[],
                MAX_RETAINED_FRONTIER_CONTENT_BYTES,
            ),
            Err(ModelFrontierRenderingError::DuplicateToolEvidence { .. })
        ),
        "the fixture must be unrenderable for this ordering claim to hold"
    );
    let error = PreparedModelOperation::render_within(
        request,
        credential_reference(),
        None,
        Box::new([]),
        &unrenderable,
        &[],
        0,
    )
    .expect_err("an over-bound frontier is refused");
    assert!(
        matches!(
            error,
            ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
                limit_bytes: 0,
                ..
            }
        ),
        "the content ceiling must win over the rendering failure it precedes: {error:?}"
    );
}

/// a turn whose retained tool content exceeds its ceiling closes through the same pre-send
/// terminal contract as round saturation and never enters the provider. The round ceiling alone
/// bounds latency and spend but not retained memory, which is what this bound supplies.
#[tokio::test]
async fn retained_frontier_content_limit_fires_before_provider_entry() {
    // Two rounds and no configured round ceiling at all, so only the
    // retained-content bound can explain the closure.
    let (request, tool_entries, failed) = tool_round_saturated_fixture(2);
    let session = request.session();
    let over_bound_call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([]),
        InProcessAttemptDispatchGate::default(),
        None,
    )
    .with_retained_frontier_content_limit(0);
    let captured = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();

    assert_eq!(
        service
            .execute(session)
            .with_subscriber(subscriber)
            .await
            .expect("the over-bound turn closes with its own terminal reason"),
        ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
    );
    let telemetry = captured.text();
    assert!(
        telemetry.contains("retained frontier content limit reached"),
        "the refusal must name the bound that fired: {telemetry}"
    );
    assert!(
        telemetry.contains("terminal_outcome=\"tool_round_limit_reached\""),
        "the service terminalization must expose the tool_round_limit_reached label"
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.recorded.len(), 1);
    let committed = &failure.recorded[0];
    assert_eq!(committed.session, session);
    assert_eq!(committed.call, over_bound_call);
    assert_eq!(
        committed.cause,
        PreparedModelCallFailureCause::ToolRoundLimitReached
    );
    assert_eq!(
        provider.capability_preparation_count(),
        0,
        "an over-bound turn must not reach provider capability preparation"
    );
    assert_eq!(
        provider.interaction_count(),
        0,
        "an over-bound turn must not reach provider interaction"
    );
    assert!(retained.is_none());
}

/// an ambiguous tool-round-limit closure retains its exact cause,
/// then an authoritative reread maps the landed closure to the distinct
/// already-committed outcome without entering the provider.
#[tokio::test]
async fn tool_round_limit_ambiguous_commit_round_trips_retained_cause() {
    const CONFIGURED_TOOL_ROUND_LIMIT: usize = 5;
    let (request, tool_entries, _) = tool_round_saturated_fixture(CONFIGURED_TOOL_ROUND_LIMIT);
    let session = request.session();
    let turn = request.turn();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::CommitAmbiguous].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::AlreadyCommitted)].into(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([]),
        InProcessAttemptDispatchGate::default(),
        Some(CONFIGURED_TOOL_ROUND_LIMIT),
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::CommitAmbiguous
        ))
    ));
    assert_eq!(
        service.retained_state(),
        Some(&RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                session,
                turn,
                call,
                cause: PreparedModelCallFailureCause::ToolRoundLimitReached,
                attachment_failure: None,
            },
        })
    );
    assert_eq!(
        service
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect("the reread proves the tool-round closure landed"),
        ModelCallExecutionOutcome::ToolRoundLimitAlreadyCommitted(call)
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 0);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// if an interrupt wins after capability preparation reported a
/// known failure, the retained reread accepts the durable cancellation as
/// authoritative no-work rather than retrying failure closure forever.
#[tokio::test]
async fn capability_failure_race_rereads_cancellation_as_no_work() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::Infrastructure].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::Cancelled)].into(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::Infrastructure
        ))
    ));
    assert_eq!(
        service
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect("the cancellation reread is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: a commit-ambiguous
/// capability-failure closure is reread before any resubmission, and a
/// landed closure ends reconciliation without repeating credential
/// preparation or the guarded transaction.
#[tokio::test]
async fn ambiguous_capability_failure_commit_is_reread_before_resubmission() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::CommitAmbiguous].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::AlreadyCommitted)].into(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::CommitAmbiguous
        ))
    ));
    assert_eq!(
        service
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect("the authoritative reread proves the closure landed"),
        ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(call)
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: send authorization has no fresh
/// candidate to replace after an identity-collision classification, so
/// the same session/call pair is not retried in place.
#[tokio::test]
async fn authorization_identity_collision_returns_without_retrying_same_call() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::IdentityCollision)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::Authorization(
            FakeError::IdentityCollision
        ))
    ));
    let (_, prepare, _, authorization, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 0);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: stale or stopped authority is an
/// ordinary no-send result, not a caller/hub defect and never provider
/// entry.
#[tokio::test]
async fn stale_authorization_returns_no_work_without_provider_entry() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        NoSendAuthorization { calls: 0 },
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("stale authority is a normal no-send result"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, authorization, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// A resumed prepared call constructs one opaque
/// capability, commits InFlight first, and invokes the provider once. An
/// operator failure produces no fabricated observation commit.
#[tokio::test]
async fn resumed_provider_failure_stays_at_provider_stage() {
    let (request, authorized) = prepared_fixture();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Ok(authorized)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::InteractionOperatorFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    let error = service
        .execute(identity(1, SessionId::from_uuid))
        .await
        .expect_err("the script reports no trustworthy observation");
    assert!(matches!(
        error,
        ModelCallExecutionError::Provider(ScriptedModelCallError::InteractionOperatorFailure)
    ));
    let (_, prepare, _, authorization, _, provider, _, _, _, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(authorization.calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 1);
}

/// a non-collision observation failure retains the exact result; later passes authoritatively
/// resubmit it unchanged while absent and stop once the original commit is observed.
#[tokio::test]
async fn failed_observation_commit_is_retained_and_reread() {
    let (request, authorized) = prepared_fixture();
    let call = authorized.call().id();
    let session = authorized.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Ok(authorized)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
            rereads: [
                Ok(RetainedModelCallObservationStatus::Pending),
                Ok(RetainedModelCallObservationStatus::AlreadyCommitted),
            ]
            .into(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    let error = service
        .execute(session)
        .await
        .expect_err("the first observation commit fails");
    let retained = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected failure: {error}"),
    };
    assert_eq!(retained.call(), call);
    assert_eq!(service.retained_observation(), Some(&retained));

    let error = service
        .execute(session)
        .await
        .expect_err("the unchanged resubmission also fails");
    let resubmitted = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected failure: {error}"),
    };
    assert_eq!(resubmitted, retained);

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the authoritative reread proves the retained commit landed"),
        ModelCallExecutionOutcome::ObservationAlreadyCommitted(call)
    );
    assert!(service.retained_observation().is_none());
    let (_, _, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 0);
    assert_eq!(observation.commit_calls, 2);
    assert_eq!(observation.reread_calls, 2);
    assert_eq!(observation.observed, vec![retained.clone(), retained]);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 1);
}

/// when authorization acknowledgement is lost, the still-owned capability proves `invoke` was
/// never entered. An authoritative InFlight reread becomes a correlated known-failure
/// observation without any provider interaction.
#[tokio::test]
async fn ambiguous_authorization_classifies_unconsumed_in_flight() {
    let (request, authorized) = prepared_fixture();
    let call = authorized.call().id();
    let session = authorized.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous)].into(),
            rereads: [Ok(ModelCallAuthorizationReread::InFlight(Box::new(
                authorized,
            )))]
            .into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure].into(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("must not be sent"))
                        .expect("fixture text is valid"),
                ],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    let error = service
        .execute(session)
        .await
        .expect_err("the fake non-consumption commit fails visibly");
    let retained = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected failure: {error}"),
    };
    assert_eq!(retained.call(), call);
    assert_eq!(
        retained.observation(),
        &ModelCallTerminalObservation::KnownFailed
    );
    let (_, _, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 1);
    assert_eq!(observation.commit_calls, 1);
    assert_eq!(observation.observed, vec![retained]);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
}

/// an ambiguous authorization reread accepts a complete
/// concurrent direct cancellation of the exact unsent call as
/// authoritative no-work without entering the provider.
#[tokio::test]
async fn ambiguous_authorization_accepts_terminal_cancellation() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous)].into(),
            rereads: [Ok(ModelCallAuthorizationReread::Cancelled)].into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: VecDeque::new(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the complete terminal cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, authorization, observation, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 1);
    assert_eq!(observation.commit_calls, 0);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: a failed ambiguous-authorization
/// reread retains the exact non-consumption proof across handoff and
/// later classifies a committed `InFlight` authorization without invoking
/// the provider.
#[tokio::test]
async fn ambiguous_authorization_reread_retains_non_consumption_across_handoff() {
    let (request, authorized) = prepared_fixture();
    let session = request.session();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request.clone()))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous)].into(),
            rereads: [
                Err(FakeError::Infrastructure),
                Ok(ModelCallAuthorizationReread::InFlight(Box::new(authorized))),
            ]
            .into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure].into(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("must not be sent"))
                        .expect("fixture text is valid"),
                ],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::AuthorizationReread {
            authorization_error: FakeError::CommitAmbiguous,
            reread_error: FakeError::Infrastructure,
        })
    ));
    assert!(matches!(
        service.retained_state(),
        Some(RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                session: retained_session,
                prepared,
            },
        }) if *retained_session == session && **prepared == request
    ));

    let (
        ids,
        prepare,
        failure,
        authorization,
        observation,
        provider,
        gate,
        catalog,
        retained,
        tool_round_limit,
    ) = service.into_parts();
    let mut resumed = ModelCallExecutionService::from_parts(
        ids,
        prepare,
        failure,
        authorization,
        observation,
        provider,
        gate,
        catalog,
        retained,
        tool_round_limit,
    );
    let error = resumed
        .execute(identity(99, SessionId::from_uuid))
        .await
        .expect_err("the retained known-failure observation commit is visible");
    let retained_observation = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected reconciliation error: {error}"),
    };
    assert_eq!(retained_observation.call(), call);
    assert_eq!(
        retained_observation.observation(),
        &ModelCallTerminalObservation::KnownFailed
    );
    let (_, prepare, _, authorization, observation, provider, _, _, retained, _) =
        resumed.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 2);
    assert_eq!(observation.commit_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(matches!(
        retained,
        Some(RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::TerminalObservation {
                observation,
                ..
            },
        }) if observation.as_ref() == &retained_observation
    ));
}

/// when an ambiguous authorization is proven to have rolled
/// back to Prepared, the unconsumed scripted interaction action can
/// prepare again and still produces exactly one physical interaction.
#[tokio::test]
async fn authorization_rollback_reprepares_one_scripted_interaction_action() {
    let (request, authorized) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request.clone())), Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous), Ok(authorized)].into(),
            rereads: [
                Err(FakeError::Infrastructure),
                Ok(ModelCallAuthorizationReread::Prepared),
            ]
            .into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure].into(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::AuthorizationReread {
            authorization_error: FakeError::CommitAmbiguous,
            reread_error: FakeError::Infrastructure,
        })
    ));
    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            ..
        })
    ));

    let (_, prepare, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
    assert_eq!(prepare.calls, 2);
    assert_eq!(authorization.calls, 2);
    assert_eq!(authorization.reread_calls, 2);
    assert_eq!(observation.commit_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 2);
    assert_eq!(provider.interaction_count(), 1);
    assert_eq!(provider.remaining_step_count(), 0);
}

/// the attempt gate transfers into the provider interaction and is released at its
/// acceptance-capable boundary while the slow terminal response remains pending.
#[tokio::test]
async fn dispatch_gate_releases_at_acceptance_boundary() {
    let (request, authorized) = prepared_fixture();
    let session = authorized.session();
    let attempt = authorized.attempt().id();
    let crossed = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let gate = InProcessAttemptDispatchGate::default();
    let gate_probe = gate.clone();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Ok(authorized)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedObservation,
        BoundaryBlockingProvider {
            crossed: Arc::clone(&crossed),
            finish: Arc::clone(&finish),
            interaction_count: 0,
        },
        gate,
        None,
    );
    {
        let execution = service.execute(session);
        tokio::pin!(execution);

        tokio::select! {
            () = crossed.notified() => {}
            result = &mut execution => panic!("provider returned before boundary probe: {result:?}"),
        }
        let after_boundary = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate_probe.acquire(attempt),
        )
        .await
        .expect("the same-attempt gate is released at provider acceptance");
        drop(after_boundary);
        finish.notify_one();
        assert!(matches!(
            execution.as_mut().await,
            Err(ModelCallExecutionError::Provider(FakeError::Infrastructure))
        ));
    }
    let (_, _, _, _, _, provider, _, _, _, _) = service.into_parts();
    assert_eq!(provider.interaction_count, 1);
}

#[test]
fn in_process_gate_serializes_the_same_attempt_but_not_distinct_attempts() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime builds");
    runtime.block_on(async {
        let gate = InProcessAttemptDispatchGate::default();
        let attempt = identity(80, TurnAttemptId::from_uuid);
        let other = identity(81, TurnAttemptId::from_uuid);
        let first = gate.acquire(attempt).await;
        let same = gate.acquire(attempt);
        tokio::pin!(same);
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut same)
                .await
                .is_err()
        );
        let distinct =
            tokio::time::timeout(std::time::Duration::from_millis(10), gate.acquire(other))
                .await
                .expect("distinct attempts do not block one another");
        drop(distinct);
        drop(first);
        tokio::time::timeout(std::time::Duration::from_millis(10), same)
            .await
            .expect("same attempt proceeds after permit release");
    });
}
