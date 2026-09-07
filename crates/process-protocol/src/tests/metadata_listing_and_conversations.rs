//! Metadata listing and conversations protocol tests.

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn metadata_list_request_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(1)?,
            ClientRequest::ListSessionMetadata {
                required_tags: vec![String::from("daily"), String::from("work")],
                title_contains: Some(String::from("Plan")),
                include_archived: true,
                page_size: CanonicalU64::new(25),
                after_session_id: Some(uuid(6)),
            },
            r#"{"type":"list_session_metadata","required_tags":["daily","work"],"title_contains":"Plan","include_archived":true,"page_size":"25","after_session_id":"00000000-0000-0000-0000-000000000006"}"#,
        )
    }

    #[test]
    fn metadata_read_request_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(2)?,
            ClientRequest::ReadSessionMetadata {
                session_id: uuid(6),
            },
            r#"{"type":"read_session_metadata","session_id":"00000000-0000-0000-0000-000000000006"}"#,
        )
    }

    #[test]
    fn metadata_replacement_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(3)?,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command(5)?,
                session_id: uuid(6),
                metadata: metadata(true)?,
            },
            r#"{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":"Planning","tags":["daily","work"],"attributes":{"run":"17","trigger":""},"archived":true}}"#,
        )
    }

    /// the unified listing request has one exact closed shape.
    #[test]
    fn list_conversations_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ListConversations {
            title_contains: Some(String::from("Plan")),
            origin: ConversationOriginFilter::All,
            include_archived: true,
            page_size: CanonicalU64::new(25),
            after: Some(ConversationCursor::new(
                ConversationOrigin::ImportedConversation,
                uuid(6),
            )),
        };

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            format!(
                "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"1\",\
                 \"request\":{{\"type\":\"list_conversations\",\"title_contains\":\"Plan\",\
                 \"origin\":\"all\",\"include_archived\":true,\"page_size\":\"25\",\
                 \"after\":{{\"origin\":\"imported_conversation\",\
                 \"conversation_id\":\"00000000-0000-0000-0000-000000000006\"}}}}}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// the nullable filter and cursor members are required, the
    /// origin filter is a closed set, and the cursor rejects unknown members.
    #[test]
    fn list_conversations_members_are_required_and_closed() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"everything","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50","after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000006","extra":true}}}"#,
        );
    }

    /// structurally invalid title filters reject a listing request
    /// before application construction.
    #[test]
    fn list_conversations_validates_title_filter_shape() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":"","origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"list_conversations\",\"title_contains\":\"a\\u0000b\",\"origin\":\"all\",\"include_archived\":false,\"page_size\":\"50\",\"after\":null}}",
        );
    }

    #[test]
    fn list_conversations_page_size_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"0","after":null}}"#,
        ))?;
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"101","after":null}}"#,
        ))?;
        Ok(())
    }

    /// the three unified page messages keep their exact closed
    /// shapes across round trips.
    #[test]
    fn conversation_page_messages_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::ConversationPageStart {},
            r#"{"type":"conversation_page_start"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::ConversationSummary {
                conversation: ConversationSummary::NativeSession {
                    session_id: uuid(1),
                    title: Some(String::from("Planning")),
                    archived: false,
                    defaults_version: CanonicalU64::new(2),
                },
            },
            r#"{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":"Planning","archived":false,"defaults_version":"2"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::ConversationSummary {
                conversation: ConversationSummary::ImportedConversation {
                    imported_conversation_id: uuid(4),
                    title: Some(String::from("Imported plan")),
                    entry_count: CanonicalU64::new(7),
                    source_format: ImportedConversationSourceFormat::CodexRolloutJsonlV1,
                },
            },
            r#"{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":"Imported plan","entry_count":"7","source_format":"codex_rollout_jsonl_v1"}}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::ConversationSummary {
                conversation: ConversationSummary::ImportedConversation {
                    imported_conversation_id: uuid(4),
                    title: None,
                    entry_count: CanonicalU64::new(1),
                    source_format: ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV1,
                },
            },
            r#"{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":null,"entry_count":"1","source_format":"claude_code_session_jsonl_v1"}}"#,
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::ConversationPageEnd {
                conversation_count: CanonicalU64::new(2),
                next_after: Some(ConversationCursor::new(
                    ConversationOrigin::NativeSession,
                    uuid(1),
                )),
            },
            r#"{"type":"conversation_page_end","conversation_count":"2","next_after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000001"}}"#,
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::ConversationPageEnd {
                conversation_count: CanonicalU64::new(0),
                next_after: None,
            },
            r#"{"type":"conversation_page_end","conversation_count":"0","next_after":null}"#,
        )?;
        Ok(())
    }

    /// a page end never names a cursor for an empty page.
    #[test]
    fn conversation_page_end_rejects_cursor_for_empty_page() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_page_end","conversation_count":"0","next_after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000001"}}}"#,
        );
    }

    #[test]
    fn conversation_page_count_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_page_end","conversation_count":"101","next_after":null}}"#,
        ))?;
        Ok(())
    }

    /// a native summary title follows the metadata title rules and
    /// an imported summary restates the derived display-title shape.
    #[test]
    fn conversation_summary_shapes_are_validated() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":"","archived":false,"defaults_version":"1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":"line\nbreak","entry_count":"1","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":" padded","entry_count":"1","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":null,"entry_count":"0","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":null,"archived":false,"defaults_version":"0"}}}"#,
        );
    }

    /// imported title length is deployment policy, not wire grammar.
    #[test]
    fn conversation_summary_admits_structurally_valid_long_imported_title()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::ConversationSummary {
            conversation: ConversationSummary::ImportedConversation {
                imported_conversation_id: uuid(4),
                title: Some("long title x".repeat(31)),
                entry_count: CanonicalU64::new(1),
                source_format: ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV2,
            },
        };
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
        Ok(())
    }
}
