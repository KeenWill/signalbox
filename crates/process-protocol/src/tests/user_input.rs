//! User input protocol tests.

use super::support::*;
use crate::*;

#[test]
fn client_round_trip_preserves_closed_request_shape() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new(
        request(u64::MAX)?,
        ClientRequest::SubmitInput {
            command_id: command(1)?,
            session_id: uuid(2),
            content: UserInputContent::text("hello".to_owned()),
            expected_defaults_version: Some(CanonicalU64::new(u64::MAX)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: None,
        },
    )?;
    let encoded = encode_client_line(&frame)?;
    let decoded = decode_client_line(&encoded)?;
    assert_eq!(decoded, frame);
    let (decoded_version, decoded_request_id, decoded_request) = decoded.into_parts();
    assert_eq!(decoded_version, ProtocolVersion::One);
    assert_eq!(decoded_request_id, request(u64::MAX)?);
    let ClientRequest::SubmitInput { content, .. } = decoded_request else {
        return Err("decoded request changed variant".into());
    };
    assert_eq!(
        content.parts(),
        &[UserInputPart::Text {
            text: String::from("hello")
        }]
    );
    assert!(String::from_utf8(encoded)?.contains("\"request_id\":\"18446744073709551615\""));
    Ok(())
}

/// multipart request encoding preserves part order and
/// every attachment metadata field in the one canonical array shape.
#[test]
fn multipart_input_wire_is_ordered_and_exact() -> Result<(), Box<dyn std::error::Error>> {
    let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        .parse::<CanonicalBlobDigest>()?;
    let content = UserInputContent::from_parts(vec![
        UserInputPart::Text {
            text: String::from("inspect "),
        },
        UserInputPart::Attachment {
            digest,
            kind: UserAttachmentKind::Image,
            media_type: String::from("image/png"),
            display_filename: Some(String::from("chart.png")),
        },
        UserInputPart::Text {
            text: String::from(" carefully"),
        },
    ]);
    let frame = ClientFrame::try_new(
        request(9)?,
        ClientRequest::SubmitInput {
            command_id: command(10)?,
            session_id: uuid(11),
            content,
            expected_defaults_version: Some(CanonicalU64::new(1)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: None,
        },
    )?;

    let encoded = encode_client_line(&frame)?;

    assert_eq!(decode_client_line(&encoded)?, frame);
    assert_eq!(
        String::from_utf8(encoded)?,
        concat!(
            "{\"version\":1,\"request_id\":\"9\",\"request\":{",
            "\"type\":\"submit_input\",",
            "\"command_id\":\"00000000-0000-0000-0000-00000000000a\",",
            "\"session_id\":\"00000000-0000-0000-0000-00000000000b\",",
            "\"content\":[{\"type\":\"text\",\"text\":\"inspect \"},",
            "{\"type\":\"attachment\",",
            "\"digest\":\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\",",
            "\"kind\":\"image\",\"media_type\":\"image/png\",",
            "\"display_filename\":\"chart.png\"},",
            "{\"type\":\"text\",\"text\":\" carefully\"}],",
            "\"expected_defaults_version\":\"1\",",
            "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
            "\"fast_mode\":{\"kind\":\"inherit\"},",
            "\"service_tier\":{\"kind\":\"inherit\"}}}}\n"
        )
    );
    Ok(())
}

/// multipart decoding stops at the public retained-parts bound.
#[test]
fn multipart_deserialization_stops_after_the_parts_bound() -> Result<(), Box<dyn std::error::Error>>
{
    let oversized = vec![
        UserInputPart::Text {
            text: String::from("x"),
        };
        crate::MAX_USER_INPUT_PARTS + 1
    ];
    let encoded = serde_json::to_vec(&oversized)?;
    let error = serde_json::from_slice::<UserInputContent>(&encoded)
        .expect_err("one part beyond the retained bound is rejected during decoding");

    assert!(error.to_string().contains("too many user-input parts"));
    Ok(())
}

#[test]
fn user_input_debug_redacts_content_bearing_values() -> Result<(), Box<dyn std::error::Error>> {
    let private_text = "private user text";
    let private_filename = "private-filename.txt";
    let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        .parse::<CanonicalBlobDigest>()?;
    let content = UserInputContent::from_parts(vec![
        UserInputPart::Text {
            text: String::from(private_text),
        },
        UserInputPart::Attachment {
            digest,
            kind: UserAttachmentKind::File,
            media_type: String::from("text/plain"),
            display_filename: Some(String::from(private_filename)),
        },
    ]);

    let debug = format!("{content:?}");
    assert!(!debug.contains(private_text));
    assert!(!debug.contains(private_filename));
    assert!(debug.contains("<redacted>"));
    Ok(())
}

/// attachment display filenames are required-nullable
/// in both directions of the version-one wire.
#[test]
fn attachment_requires_display_filename_member() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"1","request":{"type":"submit_input","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png"}],"expected_defaults_version":"1","model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}}}"#,
    );
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png"}]}}}"#,
    );
}

#[test]
fn transcript_user_entry_round_trips_ordered_multipart_content()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        .parse::<CanonicalBlobDigest>()?;
    assert_server_message_round_trip(
        request(31)?,
        ServerMessage::TranscriptUserEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(1),
            entry_id: uuid(2),
            accepted_input_id: uuid(3),
            turn_id: uuid(4),
            content: UserInputContent::from_parts(vec![
                UserInputPart::Text {
                    text: String::from("inspect "),
                },
                UserInputPart::Attachment {
                    digest,
                    kind: UserAttachmentKind::Image,
                    media_type: String::from("image/png"),
                    display_filename: Some(String::from("chart.png")),
                },
                UserInputPart::Text {
                    text: String::from(" carefully"),
                },
            ]),
        },
        r#"{"type":"transcript_user_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000002","accepted_input_id":"00000000-0000-0000-0000-000000000003","turn_id":"00000000-0000-0000-0000-000000000004","content":[{"type":"text","text":"inspect "},{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png","display_filename":"chart.png"},{"type":"text","text":" carefully"}]}"#,
    )?;
    Ok(())
}

#[test]
fn transcript_user_entry_rejects_malformed_multipart_content() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"transcript_user_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000002","accepted_input_id":"00000000-0000-0000-0000-000000000003","turn_id":"00000000-0000-0000-0000-000000000004","content":[{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png"}]}}"#,
    );
}

#[test]
fn attachment_byte_budget_rejection_requires_a_positive_maximum() {
    assert_server_malformed(
        r#"{"version":1,"request_id":"1","message":{"type":"error","code":"rejected","message":"attachment budget exceeded","detail":{"type":"rejected","rejection":{"type":"attachment_byte_budget_exceeded","maximum_bytes":"0"}}}}"#,
    );
}

#[test]
fn read_transcript_round_trips_in_the_single_vocabulary() -> Result<(), Box<dyn std::error::Error>>
{
    let frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(7)?,
        ClientRequest::ReadTranscript {
            session_id: uuid(1),
        },
    )?;
    let encoded = encode_client_line(&frame)?;

    assert_eq!(frame.version(), ProtocolVersion::One);
    assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn inv033_provider_compaction_projects_only_a_non_text_marker()
-> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::TranscriptEntry {
        entry_index: CanonicalU64::new(0),
        source_session_id: uuid(1),
        entry_id: uuid(2),
        entry: TranscriptEntry::ProviderCompaction {
            turn_id: uuid(3),
            model_call_id: uuid(4),
        },
    };

    assert_server_message_round_trip(
        request(8)?,
        message,
        r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000002","entry":{"type":"provider_compaction","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000004"}}"#,
    )?;
    Ok(())
}

#[test]
fn inv033_provider_reasoning_projects_only_a_non_text_marker()
-> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::TranscriptEntry {
        entry_index: CanonicalU64::new(0),
        source_session_id: uuid(1),
        entry_id: uuid(2),
        entry: TranscriptEntry::ProviderReasoning {
            turn_id: uuid(3),
            model_call_id: uuid(4),
        },
    };

    assert_server_message_round_trip(
        request(8)?,
        message,
        r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000002","entry":{"type":"provider_reasoning","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000004"}}"#,
    )?;
    Ok(())
}
