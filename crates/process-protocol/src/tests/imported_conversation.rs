//! Imported conversation protocol tests.

use super::support::*;
use crate::*;

/// import-invalid-request evidence names exact sizes and only the
/// content-silent converter class plus record ordinal.
#[test]
fn conversation_import_rejection_evidence_has_exact_closed_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("conversation import was rejected"),
            detail: ErrorDetail::invalid_request(
                RejectionDetail::ConversationImportSourceSizeMismatch {
                    declared_size_bytes: CanonicalU64::new(100),
                    actual_size_bytes: CanonicalU64::new(99),
                },
            ),
        },
        r#"{"type":"error","code":"invalid_request","message":"conversation import was rejected","detail":{"type":"conversation_import_source_size_mismatch","declared_size_bytes":"100","actual_size_bytes":"99"}}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("conversation import was rejected"),
            detail: ErrorDetail::invalid_request(
                RejectionDetail::ConversationImportConversionFailed {
                    class: ConversationImportRejectionClass::InvalidJson,
                    record_ordinal: Some(CanonicalU64::new(7)),
                },
            ),
        },
        r#"{"type":"error","code":"invalid_request","message":"conversation import was rejected","detail":{"type":"conversation_import_conversion_failed","class":"invalid_json","record_ordinal":"7"}}"#,
    )?;
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("conversation import was rejected"),
            detail: ErrorDetail::invalid_request(
                RejectionDetail::ConversationImportConversionFailed {
                    class: ConversationImportRejectionClass::EmptySource,
                    record_ordinal: None,
                },
            ),
        },
        r#"{"type":"error","code":"invalid_request","message":"conversation import was rejected","detail":{"type":"conversation_import_conversion_failed","class":"empty_source","record_ordinal":null}}"#,
    )?;
    let empty_source_with_ordinal = ServerMessage::Error {
        code: ErrorCode::InvalidRequest,
        message: String::from("conversation import was rejected"),
        detail: ErrorDetail::invalid_request(RejectionDetail::ConversationImportConversionFailed {
            class: ConversationImportRejectionClass::EmptySource,
            record_ordinal: Some(CanonicalU64::new(1)),
        }),
    };
    assert_eq!(
        ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(5)?,
            empty_source_with_ordinal,
        ),
        Err(FrameValidationError::ConversationImportShape)
    );
    let invalid_json_without_ordinal = ServerMessage::Error {
        code: ErrorCode::InvalidRequest,
        message: String::from("conversation import was rejected"),
        detail: ErrorDetail::invalid_request(RejectionDetail::ConversationImportConversionFailed {
            class: ConversationImportRejectionClass::InvalidJson,
            record_ordinal: None,
        }),
    };
    assert_eq!(
        ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(6)?,
            invalid_json_without_ordinal,
        ),
        Err(FrameValidationError::ConversationImportShape)
    );
    Ok(())
}

/// imported-frontier creation has one exact closed request shape.
#[test]
fn imported_frontier_creation_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>>
{
    let request_id = request(1)?;
    let request_value = ClientRequest::CreateSessionFromImportedFrontier {
        command_id: command(4)?,
        imported_conversation_id: uuid(5),
        through_position: CanonicalU64::new(2),
        relationship: ImportedSessionRelationship::Resume,
        initial_model_selection: ModelSelection::Direct {
            selection_id: uuid(6),
        },
        model_settings: ModelSettingsOverlay::inherit_all(),
    };

    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"create_session_from_imported_frontier\",\"command_id\":\"00000000-0000-0000-0000-000000000004\",\"imported_conversation_id\":\"00000000-0000-0000-0000-000000000005\",\"through_position\":\"2\",\"relationship\":\"resume\",\"initial_model_selection\":{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000006\"},\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}}}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

/// model-call usage has one exact closed shape.
#[test]
fn model_call_usage_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(1)?;
    let message = ServerMessage::TranscriptModelCallUsage {
        model_call_index: CanonicalU64::new(0),
        turn_id: uuid(2),
        model_call_id: uuid(3),
        usage_provenance: UsageProvenance::Reported,
        usage: ModelCallTokenUsage {
            input_tokens: Some(CanonicalU64::new(10)),
            output_tokens: Some(CanonicalU64::new(0)),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(CanonicalU64::new(4)),
        },
        cost: Some(ModelCallDollarCost {
            amount_usd: CanonicalDollarAmount::try_new(String::from("0.125"))?,
            rate_version: BillingRateVersion::try_new(String::from("rates-v7"))?,
            label: ModelCallCostLabel::MeteredEquivalent,
        }),
    };

    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request_id, message)?;
    let encoded = encode_server_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        format!(
            "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"1\",\"message\":{{\"type\":\"transcript_model_call_usage\",\"model_call_index\":\"0\",\"turn_id\":\"00000000-0000-0000-0000-000000000002\",\"model_call_id\":\"00000000-0000-0000-0000-000000000003\",\"usage_provenance\":\"reported\",\"usage\":{{\"input_tokens\":\"10\",\"output_tokens\":\"0\",\"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":\"4\"}},\"cost\":{{\"amount_usd\":\"0.125\",\"rate_version\":\"rates-v7\",\"label\":\"metered_equivalent\"}}}}}}\n"
        )
    );
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn usage_rejects_an_omitted_evidence_field() {
    let error = decode_server_line(&line(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"reported","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null},"cost":null}}"#,
    ))
    .expect_err("required-nullable evidence fields cannot be omitted");
    assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
}

#[test]
fn usage_rejects_an_omitted_cost_member() {
    let error = decode_server_line(&line(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"reported","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null,"cache_read_input_tokens":null}}}"#,
    ))
    .expect_err("the derived cost member is required nullable");
    assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
}

#[test]
fn usage_rejects_cost_without_a_present_axis() {
    let error = decode_server_line(&line(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"reported","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null,"cache_read_input_tokens":null},"cost":{"amount_usd":"0","rate_version":"rates-v1","label":"real"}}}"#,
    ))
    .expect_err("a cost without derivation evidence must be rejected");
    assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
}

#[test]
fn usage_provenance_rejects_unknown_values() {
    let error = decode_server_line(&line(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"inferred","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null,"cache_read_input_tokens":null},"cost":null}}"#,
    ))
    .expect_err("the usage provenance vocabulary is closed");
    assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
}

#[test]
fn imported_frontier_rejects_zero_position() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(2)?,
        ClientRequest::CreateSessionFromImportedFrontier {
            command_id: command(4)?,
            imported_conversation_id: uuid(5),
            through_position: CanonicalU64::new(0),
            relationship: ImportedSessionRelationship::Resume,
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(6),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
        },
    );

    assert_eq!(frame, Err(FrameValidationError::ImportedFrontierShape));
    Ok(())
}

#[test]
fn imported_conversation_read_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>>
{
    let request_id = request(1)?;
    let request_value = ClientRequest::ReadImportedConversation {
        imported_conversation_id: uuid(5),
    };

    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"read_imported_conversation\",\"imported_conversation_id\":\"00000000-0000-0000-0000-000000000005\"}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

/// an imported-conversation entry carries its position, exact
/// attestation, content kind, and bounded preview in one closed shape.
#[test]
fn imported_conversation_entry_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>>
{
    let message = ServerMessage::ImportedConversationEntry {
        position: CanonicalU64::new(2),
        imported_entry_id: uuid(6),
        source_speaker: ImportedSourceSpeaker::Attested {
            speaker: ImportedSpeaker::Assistant,
        },
        content_kind: ImportedContentKind::Text,
        text_preview: Some(ImportedTextPreview::of_exact_text("imported answer")),
    };

    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
    let encoded = encode_server_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"message\":{\"type\":\"imported_conversation_entry\",\"position\":\"2\",\"imported_entry_id\":\"00000000-0000-0000-0000-000000000006\",\"source_speaker\":{\"type\":\"attested\",\"speaker\":\"assistant\"},\"content_kind\":\"text\",\"text_preview\":{\"preview\":\"imported answer\",\"truncated\":false}}}\n"
    );
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

/// An entry whose content carries no exact attested text states that
/// absence as an explicit null rather than an empty preview.
#[test]
fn imported_conversation_entry_states_an_absent_preview_as_null()
-> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(1),
            imported_entry_id: uuid(6),
            source_speaker: ImportedSourceSpeaker::NotAttested {},
            content_kind: ImportedContentKind::SourceEvent,
            text_preview: None,
        },
    )?;
    let encoded = encode_server_line(&frame)?;

    assert!(String::from_utf8(encoded.clone())?.contains("\"text_preview\":null"));
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

/// An imported-conversation entry position is one-based, so zero is not a
/// selectable ordinal on the wire.
#[test]
fn imported_conversation_entry_rejects_zero_position() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(0),
            imported_entry_id: uuid(6),
            source_speaker: ImportedSourceSpeaker::NotAttested {},
            content_kind: ImportedContentKind::SourceEvent,
            text_preview: None,
        },
    );

    assert_eq!(
        frame,
        Err(FrameValidationError::ImportedConversationEntryShape)
    );
    Ok(())
}

/// A preview cuts on a Unicode scalar boundary, so it is always an exact
/// prefix of the source text and never a split encoding.
#[test]
fn imported_text_preview_cuts_on_a_scalar_boundary() {
    // 86 three-byte scalars are 258 bytes, so the configured 256-byte limit falls
    // inside the 86th scalar and the preview keeps only the first 85.
    let text = "\u{4e00}".repeat(86);
    let preview = ImportedTextPreview::of_exact_text_with_limit(&text, Some(256));

    assert_eq!(preview.preview(), "\u{4e00}".repeat(85));
    assert!(preview.truncated());
    assert!(text.starts_with(preview.preview()));
}

/// Text inside the bound is previewed exactly and is not marked truncated.
#[test]
fn imported_text_preview_retains_exact_text_within_its_bound() {
    let preview = ImportedTextPreview::of_exact_text("imported question");

    assert_eq!(preview.preview(), "imported question");
    assert!(!preview.truncated());
}

/// Attested empty text previews as exact empty text, distinguishing it
/// from an entry that carries no attested text at all.
#[test]
fn imported_text_preview_retains_attested_empty_text() {
    let preview = ImportedTextPreview::of_exact_text("");

    assert_eq!(preview.preview(), "");
    assert!(!preview.truncated());
}

/// A preview deserialized on its own is checked exactly as an embedded one
/// is, so no consumer can hold a bounded preview that violates its bound.
#[test]
fn imported_text_preview_validates_on_direct_deserialization() {
    let oversized = format!(
        "{{\"preview\":\"{}\",\"truncated\":false}}",
        "a".repeat(MAX_CONTENT_FRAGMENT_BYTES + 1)
    );

    assert!(serde_json::from_str::<ImportedTextPreview>(&oversized).is_err());
    assert!(
        serde_json::from_str::<ImportedTextPreview>(r#"{"preview":"","truncated":true}"#).is_err()
    );
    assert_eq!(
        serde_json::from_str::<ImportedTextPreview>(r#"{"preview":"ab","truncated":true}"#)
            .expect("a bounded truncated preview decodes"),
        ImportedTextPreview {
            preview: String::from("ab"),
            truncated: true,
        }
    );
}

/// A truncation marker over an empty preview contradicts the scalar cut,
/// which always keeps at least one scalar of nonempty text.
#[test]
fn imported_text_preview_rejects_truncated_empty_text() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(1),
            imported_entry_id: uuid(6),
            source_speaker: ImportedSourceSpeaker::NotAttested {},
            content_kind: ImportedContentKind::Text,
            text_preview: Some(ImportedTextPreview {
                preview: String::new(),
                truncated: true,
            }),
        },
    );

    assert_eq!(frame, Err(FrameValidationError::ImportedTextPreviewShape));
    Ok(())
}

/// A preview states an entry's exact attested text, so attaching one to a
/// kind that has no such text is a contradictory frame rather than extra
/// information the client may present.
#[test]
fn imported_conversation_entry_rejects_a_preview_on_nontext_content()
-> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(1),
            imported_entry_id: uuid(6),
            source_speaker: ImportedSourceSpeaker::NotAttested {},
            content_kind: ImportedContentKind::ToolCall,
            text_preview: Some(ImportedTextPreview::of_exact_text("lookup")),
        },
    );

    assert_eq!(
        frame,
        Err(FrameValidationError::ImportedConversationEntryShape)
    );
    Ok(())
}

/// A requested ordinal inside the stated range contradicts the rejection
/// carrying it, so the frame is refused rather than rendered.
#[test]
fn imported_range_rejection_refuses_a_selectable_requested_position()
-> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the command was rejected by current durable state"),
            detail: ErrorDetail::rejected(RejectionDetail::ImportedFrontierPositionOutOfRange {
                imported_conversation_id: uuid(5),
                requested_position: CanonicalU64::new(2),
                last_position: CanonicalU64::new(2),
            }),
        },
    );

    assert_eq!(frame, Err(FrameValidationError::ImportedFrontierRangeShape));
    Ok(())
}

/// An imported conversation is nonempty, so a zero selectable bound cannot
/// describe one.
#[test]
fn imported_range_rejection_refuses_an_empty_selectable_range()
-> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the command was rejected by current durable state"),
            detail: ErrorDetail::rejected(RejectionDetail::ImportedFrontierPositionOutOfRange {
                imported_conversation_id: uuid(5),
                requested_position: CanonicalU64::new(1),
                last_position: CanonicalU64::new(0),
            }),
        },
    );

    assert_eq!(frame, Err(FrameValidationError::ImportedFrontierRangeShape));
    Ok(())
}

/// an out-of-range imported position is a rejection naming the
/// conversation's selectable range, never the absent-session `not_found`.
#[test]
fn names_the_imported_position_range() -> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::Error {
        code: ErrorCode::Rejected,
        message: String::from("the command was rejected by current durable state"),
        detail: ErrorDetail::rejected(RejectionDetail::ImportedFrontierPositionOutOfRange {
            imported_conversation_id: uuid(5),
            requested_position: CanonicalU64::new(999_999),
            last_position: CanonicalU64::new(2),
        }),
    };

    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
    let encoded = encode_server_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"message\":{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"the command was rejected by current durable state\",\"detail\":{\"type\":\"imported_frontier_position_out_of_range\",\"imported_conversation_id\":\"00000000-0000-0000-0000-000000000005\",\"requested_position\":\"999999\",\"last_position\":\"2\"}}}\n"
    );
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

/// an absent imported conversation names an imported conversation
/// as the missing target.
#[test]
fn names_the_absent_imported_conversation() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the command was rejected by current durable state"),
            detail: ErrorDetail::rejected(RejectionDetail::ImportedConversationNotFound {
                imported_conversation_id: uuid(5),
            }),
        },
    )?;
    let encoded = encode_server_line(&frame)?;

    assert!(
        String::from_utf8(encoded.clone())?
            .contains("\"type\":\"imported_conversation_not_found\"")
    );
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}
