//! Conversation import protocol tests.

use crate::{MAX_CONVERSATION_IMPORT_CHUNK_BYTES, MAX_FRAME_BYTES};

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn import_request_preserves_exact_bytes_and_format() -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ImportConversation {
            format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
            source: ConversationImportSource::new(vec![0, 255]),
        };

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"import_conversation\",\
             \"format\":\"claude_code_session_jsonl_v2\",\"source\":\"AP8=\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// chunked import has one exact closed begin/append/commit/abort
    /// request vocabulary.
    #[test]
    fn chunked_import_requests_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>>
    {
        let begin = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                declared_size_bytes: CanonicalU64::new(5),
            },
        )?;
        let append = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(2)?,
            ClientRequest::AppendConversationImport {
                chunk: ConversationImportSource::new(vec![0, 255]),
            },
        )?;
        let commit = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(3)?,
            ClientRequest::CommitConversationImport {},
        )?;
        let abort = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(4)?,
            ClientRequest::AbortConversationImport {},
        )?;

        let encoded_begin = encode_client_line(&begin)?;
        let encoded_append = encode_client_line(&append)?;
        let encoded_commit = encode_client_line(&commit)?;
        let encoded_abort = encode_client_line(&abort)?;
        assert_eq!(
            String::from_utf8(encoded_begin.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"begin_conversation_import\",\"format\":\"codex_rollout_jsonl_v1\",\"declared_size_bytes\":\"5\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_append.clone())?,
            "{\"version\":1,\"request_id\":\"2\",\"request\":{\"type\":\"append_conversation_import\",\"chunk\":\"AP8=\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_commit.clone())?,
            "{\"version\":1,\"request_id\":\"3\",\"request\":{\"type\":\"commit_conversation_import\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_abort.clone())?,
            "{\"version\":1,\"request_id\":\"4\",\"request\":{\"type\":\"abort_conversation_import\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded_begin)?, begin);
        assert_eq!(decode_client_line(&encoded_append)?, append);
        assert_eq!(decode_client_line(&encoded_commit)?, commit);
        assert_eq!(decode_client_line(&encoded_abort)?, abort);
        Ok(())
    }

    /// every maximum-sized append still fits the unchanged complete
    /// frame bound, while a larger raw chunk is invalid before encoding.
    #[test]
    fn import_append_respects_the_existing_frame_bound() -> Result<(), Box<dyn std::error::Error>> {
        let maximum = ClientRequest::AppendConversationImport {
            chunk: ConversationImportSource::new(vec![
                b'x';
                super::MAX_CONVERSATION_IMPORT_CHUNK_BYTES
            ]),
        };
        let oversized = ClientRequest::AppendConversationImport {
            chunk: ConversationImportSource::new(vec![
                b'x';
                super::MAX_CONVERSATION_IMPORT_CHUNK_BYTES
                    + 1
            ]),
        };

        let maximum_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            RequestId::try_new(u64::MAX)?,
            maximum,
        )?;
        assert!(encode_client_line(&maximum_frame)?.len() <= super::MAX_FRAME_BYTES);
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, oversized),
            Err(FrameValidationError::ConversationImportShape)
        );
        Ok(())
    }

    /// chunked-import transport acknowledgements have exact closed
    /// shapes; commit deliberately keeps the existing terminal receipts.
    #[test]
    fn chunked_import_acknowledgements_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::ConversationImportBegun {
                declared_size_bytes: CanonicalU64::new(9),
            },
            r#"{"type":"conversation_import_begun","declared_size_bytes":"9"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::ConversationImportAppended {
                assembled_size_bytes: CanonicalU64::new(7),
            },
            r#"{"type":"conversation_import_appended","assembled_size_bytes":"7"}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::ConversationImportAborted {},
            r#"{"type":"conversation_import_aborted"}"#,
        )?;
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(4)?,
                ServerMessage::ConversationImportAppended {
                    assembled_size_bytes: CanonicalU64::new(0),
                },
            ),
            Err(FrameValidationError::ConversationImportShape)
        );
        Ok(())
    }
}
