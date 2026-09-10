use super::{
    AcceptedInputId, AssistantText, BTreeMap, CapturedTelemetry, ContextCompactionRange,
    DelegationContent, DelegationMessageId, DelegationOutcome, DelegationWaitMode,
    DirectModelSelection, FakePrepare, FixedIds, ImportedSourceAttestation, ImportedSpeaker,
    ImportedText, ImportedTranscriptContent, ImportedTranscriptEntryId,
    InProcessAttemptDispatchGate, MAX_RETAINED_FRONTIER_CONTENT_BYTES, ModelCallExecutionOutcome,
    ModelCallExecutionService, ModelCallId, ModelFrontierRenderingError, NonZeroU64,
    PreparedModelCallFailureCause, PreparedModelOperation, ScriptedFailure,
    ScriptedModelCallProvider, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryRef, SessionConfigurationDefaultsVersion, SessionId, ToolRequestId,
    TurnId, UnusedAuthorization, UnusedObservation, UserContent, counted_frontier_bytes,
    credential_reference, identity, projected_frontier_container_bytes,
    projected_frontier_content_bytes, ready_with_tool_evidence, render_frontier_messages,
    rendered_content_bytes, tool_round_saturated_fixture,
    tool_round_saturated_fixture_with_assistant_text,
};

use tracing::instrument::WithSubscriber as _;

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
    let container_bytes = projected_frontier_container_bytes(
        entries.iter().map(|(_, payload)| payload),
        |accepted_input| origin_contents.get(&accepted_input),
    );
    let messages = render_frontier_messages(
        entries.iter().map(|(source, payload)| (*source, payload)),
        |accepted_input| origin_contents.get(&accepted_input).cloned(),
        |_| None,
        std::iter::empty(),
    )
    .expect("the payload fixture renders");
    let rendered_container_bytes = std::mem::size_of_val(messages.as_ref())
        + messages
            .iter()
            .map(|message| match message {
                super::ModelConversationMessage::User { content, .. } => {
                    std::mem::size_of_val(content.parts())
                }
                _ => 0,
            })
            .sum::<usize>();
    assert_eq!(
        container_bytes, rendered_container_bytes,
        "non-rendering imports and terminal markers must not spend message container bytes"
    );

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
    let plain_bytes = counted_frontier_bytes(&plain_request, &plain_entries)
        + plain_request.frontier_entries().count()
            * std::mem::size_of::<super::ModelConversationMessage>()
        + std::mem::size_of::<super::ModelUserContentPart>();
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
            observed_bytes: plain_bytes
                + assistant_text.len()
                + std::mem::size_of::<super::ModelConversationMessage>(),
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
        "the service terminalization must expose the tool_round_limit_reached label: {telemetry}"
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

#[test]
fn message_cardinality_spends_the_retained_representation_budget() {
    let (many_request, tool_entries, _) = tool_round_saturated_fixture(64);
    let content_bytes = counted_frontier_bytes(&many_request, &tool_entries);
    let single_request = super::support::prepared_execution_with_content_fixture(
        UserContent::try_text("x".repeat(content_bytes)).expect("fixture text"),
    )
    .resume_prepared_call()
    .expect("fixture call resumes");
    let limit = content_bytes
        + single_request.frontier_entries().count()
            * std::mem::size_of::<super::ModelConversationMessage>()
        + std::mem::size_of::<super::ModelUserContentPart>();
    PreparedModelOperation::render_within(
        single_request,
        credential_reference(),
        None,
        Box::new([]),
        &[],
        &[],
        limit,
    )
    .expect("one message fits with the same total payload bytes");
    let result = PreparedModelOperation::render_within(
        many_request,
        credential_reference(),
        None,
        Box::new([]),
        &tool_entries,
        &[],
        limit,
    );
    assert!(
        matches!(
            result,
            Err(ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded { .. })
        ),
        "many short messages spend more retained representation bytes: {result:?}"
    );
}

#[tokio::test]
async fn evidence_loading_limit_closes_before_provider_preparation() {
    let (request, _, failed) = tool_round_saturated_fixture(2);
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(
                super::PrepareModelCallOutcome::RetainedContentLimitExceeded {
                    turn: request.turn(),
                    call: request.call().id(),
                },
            )]
            .into(),
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
    );
    assert_eq!(
        service
            .execute(session)
            .with_subscriber(tracing_subscriber::registry())
            .await
            .expect("the oversized prepared call closes"),
        ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
    );
}

#[test]
fn attachment_heap_content_spends_the_budget_before_rendering() {
    let content = UserContent::try_parts(vec![super::UserContentPart::Attachment {
        digest: super::BlobDigest::digest(b"attachment heap fixture"),
        kind: super::AttachmentKind::Document,
        media_type: signalbox_domain::DeclaredMediaType::try_new("\"".repeat(255))
            .expect("bounded media type"),
        display_filename: Some(
            signalbox_domain::AttachmentDisplayFilename::try_new("\u{1}".repeat(255))
                .expect("bounded filename"),
        ),
    }])
    .expect("one attachment part");
    let request = super::support::prepared_execution_with_content_fixture(content)
        .resume_prepared_call()
        .expect("fixture call resumes");
    // The reserved stub is 2,296 bytes at u64::MAX and ordinal 255, plus two
    // 255-byte metadata values. Rendering ordinal zero uses two fewer bytes.
    let expected_heap_bytes = 2_806;
    let representation_bytes = request.frontier_entries().count()
        * std::mem::size_of::<super::ModelConversationMessage>()
        + std::mem::size_of::<super::ModelUserContentPart>();
    let limit = representation_bytes + expected_heap_bytes;
    let operation = PreparedModelOperation::render_within(
        request.clone(),
        credential_reference(),
        None,
        Box::new([]),
        &[],
        &[],
        limit,
    )
    .expect("the exact retained allocation fits");
    assert_eq!(
        rendered_content_bytes(operation.messages()),
        expected_heap_bytes - 2
    );
    assert_eq!(
        PreparedModelOperation::render_within(
            request,
            credential_reference(),
            None,
            Box::new([]),
            &[],
            &[],
            limit - 1,
        )
        .expect_err("attachment heap content exceeds the one-byte-smaller budget"),
        ModelFrontierRenderingError::RetainedFrontierContentLimitExceeded {
            observed_bytes: limit,
            limit_bytes: limit - 1,
        }
    );
}
