//! Frame bounds protocol tests.

use super::support::*;
use crate::*;

#[test]
fn fragment_bound_keeps_worst_case_json_below_frame_cap() -> Result<(), Box<dyn std::error::Error>>
{
    let fragment = ContentFragment::try_new("\u{1}".repeat(MAX_CONTENT_FRAGMENT_BYTES))?;
    let frame = ServerFrame::try_new(
        request(1)?,
        ServerMessage::TranscriptContent {
            entry_index: CanonicalU64::new(u64::MAX),
            fragment_index: CanonicalU64::new(u64::MAX),
            final_fragment: true,
            content_fragment: fragment,
        },
    )?;
    let encoded = encode_server_line(&frame)?;
    assert!(encoded.len() < crate::MAX_FRAME_BYTES);
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn content_fragmentation_preserves_empty_text_exactly() {
    let empty = crate::content_fragments("").collect::<Vec<_>>();
    assert_eq!(empty.len(), 1);
    assert_eq!(empty[0].as_str(), "");
}

#[test]
fn content_fragmentation_preserves_multibyte_boundaries_exactly() {
    let text = format!(
        "{}\u{1f980}tail",
        "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
    );
    let fragments = crate::content_fragments(&text).collect::<Vec<_>>();
    assert_eq!(fragments.len(), 2);
    assert_eq!(
        fragments[0].as_str(),
        "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
    );
    assert_eq!(fragments[1].as_str(), "\u{1f980}tail");
    assert_eq!(
        format!("{}{}", fragments[0].as_str(), fragments[1].as_str()),
        text
    );
}

#[test]
fn oversized_outgoing_frame_fails_explicitly() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new(
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::Internal,
            message: "x".repeat(crate::MAX_FRAME_BYTES),
            detail: ErrorDetail::none(),
        },
    )?;
    assert!(matches!(
        encode_server_line(&frame),
        Err(FrameEncodeError::OversizedFrame)
    ));
    Ok(())
}

#[test]
fn exact_newline_framing_is_enforced() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new(request(1)?, ClientRequest::ListSessions {})?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(encoded.last(), Some(&b'\n'));
    let missing_newline = decode_client_line(&encoded[..encoded.len() - 1])
        .expect_err("missing newline must remain a malformed frame");
    assert_eq!(missing_newline.kind(), FrameDecodeErrorKind::MalformedFrame);
    assert_eq!(missing_newline.request_id().value(), 1);
    let mut carriage_return = encoded[..encoded.len() - 1].to_vec();
    carriage_return.extend_from_slice(b"\r\n");
    let carriage_return =
        decode_client_line(&carriage_return).expect_err("CRLF must remain malformed");
    assert_eq!(carriage_return.kind(), FrameDecodeErrorKind::MalformedFrame);
    assert_eq!(carriage_return.request_id().value(), 1);
    let mut multiline = encoded.clone();
    multiline.insert(1, b'\n');
    let multiline = decode_client_line(&multiline).expect_err("embedded LF must be malformed");
    assert_eq!(multiline.kind(), FrameDecodeErrorKind::MalformedFrame);
    assert_eq!(multiline.request_id().value(), 1);
    Ok(())
}

#[test]
fn oversized_complete_frame_preserves_recoverable_request_id() {
    let oversized = padded_oversized_client_frame(r#""request_id":"9""#, crate::MAX_FRAME_BYTES);
    let error = decode_client_line(&oversized)
        .expect_err("a complete frame over the byte cap must be rejected");

    assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
    assert_eq!(error.request_id().value(), 9);
}

#[test]
fn oversized_duplicate_request_identity_is_uncorrelated() {
    let oversized = padded_oversized_client_frame(
        r#""request_id":"9","request_id":"10""#,
        crate::MAX_FRAME_BYTES,
    );
    let error = decode_client_line(&oversized).expect_err("a duplicate request identity must fail");

    assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
    assert_eq!(error.request_id().value(), 0);
}

#[test]
fn oversized_noncanonical_request_identity_is_uncorrelated() {
    let oversized = padded_oversized_client_frame(r#""request_id":"09""#, crate::MAX_FRAME_BYTES);
    let error =
        decode_client_line(&oversized).expect_err("a noncanonical request identity must fail");

    assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
    assert_eq!(error.request_id().value(), 0);
}

#[test]
fn request_identity_recovery_stops_at_the_frame_cap() {
    let far_oversized =
        padded_oversized_client_frame(r#""request_id":"9""#, crate::MAX_FRAME_BYTES * 2);
    let error = decode_client_line(&far_oversized)
        .expect_err("a frame beyond the recovery budget must be rejected");

    assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
    assert_eq!(error.request_id().value(), 0);
}

#[test]
fn bounded_client_request_identity_recovery_matches_oversized_decode() {
    let oversized = padded_oversized_client_frame(r#""request_id":"9""#, crate::MAX_FRAME_BYTES);
    let content = &oversized[..oversized.len() - 1];

    assert_eq!(crate::recover_bounded_client_request_id(content).value(), 9);
    assert_eq!(
        crate::recover_bounded_client_request_id(&oversized).value(),
        0
    );
}

#[test]
fn all_client_request_variants_encode_with_current_version()
-> Result<(), Box<dyn std::error::Error>> {
    let model = ModelSelection::Direct {
        selection_id: uuid(3),
    };
    assert_client_request_current_version(
        request(1)?,
        ClientRequest::CreateSession {
            command_id: command(4)?,
            initial_model_selection: model,
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(None),
            placement: crate::SessionPlacement::Pathless {},
            lifecycle: SessionLifecycleMembers::default(),
        },
    )?;
    assert_client_request_current_version(request(2)?, ClientRequest::ListSessions {})?;
    assert_client_request_current_version(
        request(80)?,
        ClientRequest::UpdateSessionPlacement {
            command_id: command(81)?,
            session_id: uuid(82),
            expected_placement_version: CanonicalU64::new(1),
            replacement: crate::SessionPlacement::Pathless {},
        },
    )?;
    assert_client_request_current_version(
        request(3)?,
        ClientRequest::SubmitInput {
            command_id: command(5)?,
            session_id: uuid(6),
            content: UserInputContent::text(String::from("content")),
            expected_defaults_version: Some(CanonicalU64::new(1)),
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: None,
        },
    )?;
    assert_client_request_current_version(
        request(4)?,
        ClientRequest::ReadTranscript {
            session_id: uuid(6),
            after_frontier: None,
        },
    )?;
    assert_client_request_current_version(
        request(5)?,
        ClientRequest::FollowSession {
            session_id: uuid(6),
        },
    )?;
    Ok(())
}
