use super::{
    AcceptedInputId, AssistantText, AttachmentKind, BlobDigest, DecideToolRequest,
    DelegationContent, DelegationMessageId, DelegationOutcome, DelegationWaitMode,
    DurableCommandId, ImportedMessageContentAbsence, ImportedSourceAttestation, ImportedSpeaker,
    ImportedText, ImportedTranscriptContent, MAX_RENDERED_ATTACHMENT_STUB_BYTES, ModelCallId,
    ModelConversationMessage, ModelFrontierRenderingError, ModelToolResultContent, NonZeroU64,
    PreparedModelOperation, ResolvedToolConversationEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryRef, SessionId, SessionSystemPrompt,
    ToolApprovalDecision, ToolApprovalResolutionReconstitutionInput, ToolAttemptEnd,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolDenialReason,
    ToolDispatchGeneration, ToolEffectClass, ToolRequestId, ToolResultContent, ToolResultText,
    TurnAttemptId, TurnId, UserContent, UserContentPart, automatic_tool_round_count,
    credential_reference, current_turn_tool_rounds, expect, identity, model_tool_request,
    one_current_batch_with_inherited_tool_history, prepared_fixture,
    projected_frontier_content_bytes, render_frontier_messages, render_model_user_content,
    rendered_text,
};

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

    let charged = super::super::render::user_content_retained_bytes(&content);
    let rendered = render_model_user_content(content, |_| Some(NonZeroU64::MAX))
        .expect("the derived bound covers maximum checked metadata");
    let stub = rendered.parts()[0].as_str();

    assert!(stub.len() <= MAX_RENDERED_ATTACHMENT_STUB_BYTES);
    assert_eq!(stub.len(), 2_242);
    assert!(charged >= stub.len());
}

#[test]
fn repeated_attachment_occurrences_each_count_toward_rendered_content() {
    let attachment = UserContentPart::Attachment {
        digest: BlobDigest::digest(b"repeated attachment"),
        kind: AttachmentKind::File,
        media_type: signalbox_domain::DeclaredMediaType::try_new("text/plain".into())
            .expect("fixture media type"),
        display_filename: None,
    };
    let single = UserContent::try_parts(vec![attachment.clone()]).expect("one occurrence");
    let repeated = UserContent::try_parts(vec![attachment.clone(), attachment])
        .expect("two occurrences of one digest");
    let single_charge = super::super::render::user_content_retained_bytes(&single);
    let repeated_charge = super::super::render::user_content_retained_bytes(&repeated);
    let rendered = render_model_user_content(repeated, |_| Some(NonZeroU64::MIN))
        .expect("catalog length is available");

    assert_eq!(repeated_charge, single_charge * 2);
    assert!(
        repeated_charge
            >= rendered
                .parts()
                .iter()
                .map(|part| part.as_str().len())
                .sum()
    );
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
    let admitted_text = ToolResultText::try_new(String::from("bounded context text"))
        .expect("fixture context text is valid");
    let evidence = [
        ResolvedToolConversationEntry::AssistantToolUse {
            source: completed_use_source,
            request: completed_request.clone(),
        },
        ResolvedToolConversationEntry::ExecutionResult {
            source: completed_result_source,
            request: completed_request.clone(),
            context_text: Some(admitted_text.clone()),
            context_error_detail: None,
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
                content: ModelToolResultContent::Success(ToolResultContent::Text(admitted_text)),
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

#[test]
fn frontier_rendering_uses_bounded_failure_detail_with_the_exact_kind() {
    use signalbox_domain::{ToolExecutionError, ToolExecutionErrorDetail, ToolExecutionErrorKind};

    let request = model_tool_request(0);
    let source = SemanticTranscriptEntryRef::from_source(
        request.session(),
        identity(111, SemanticTranscriptEntryId::from_uuid),
    );
    let attempt_id = identity(116, signalbox_domain::ToolAttemptId::from_uuid);
    let exact = ToolExecutionError::new(
        ToolExecutionErrorKind::ExecutionFailed,
        Some(ToolExecutionErrorDetail::try_new(String::from("exact executor detail")).unwrap()),
    );
    let admitted =
        ToolExecutionErrorDetail::try_new(String::from("bounded context detail")).unwrap();
    let signalbox_domain::ReconstitutedToolAttempt::Ended(attempt) =
        ToolAttemptReconstitutionInput::new(
            attempt_id,
            request.id(),
            request.session(),
            request.turn(),
            identity(117, TurnAttemptId::from_uuid),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
                error: exact.clone(),
            }),
        )
        .reconstitute()
        .unwrap()
    else {
        panic!("terminal fixture reconstitutes as ended")
    };
    let evidence = [ResolvedToolConversationEntry::ExecutionResult {
        source,
        request: request.clone(),
        attempt: attempt.clone(),
        context_text: None,
        context_error_detail: Some(admitted.clone()),
    }];
    let payload = SemanticTranscriptEntryPayload::ToolExecutionResult {
        attempt: attempt_id,
    };
    let messages =
        render_frontier_messages([(source, &payload)], |_| None, |_| None, evidence.iter())
            .unwrap();
    assert_eq!(
        messages.as_ref(),
        &[ModelConversationMessage::ToolResult {
            source,
            request: request.id(),
            content: ModelToolResultContent::ExecutionError(ToolExecutionError::new(
                exact.kind(),
                Some(admitted),
            )),
        }]
    );
    assert_eq!(attempt.end(), &ToolAttemptEnd::KnownFailed { error: exact });
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
        context_error_detail: None,
        context_text: match cross_turn_attempt.end() {
            ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(text),
            } => Some(text.clone()),
            _ => panic!("fixture has a completed text result"),
        },
        attempt: cross_turn_attempt,
    };

    let error = render_frontier_messages([(source, &payload)], |_| None, |_| None, [&evidence])
        .expect_err("cross-turn tool evidence must fail closed");

    assert_eq!(
        error,
        ModelFrontierRenderingError::MissingOrMismatchedToolEvidence { entry: source }
    );
}
