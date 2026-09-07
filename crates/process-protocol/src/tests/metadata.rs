//! Metadata protocol tests.

use crate::MAX_FRAME_BYTES;

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn metadata_list_requires_title_query_member() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn metadata_list_requires_cursor_member() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"50"}}"#,
        );
    }

    #[test]
    fn metadata_list_rejects_duplicate_required_tags() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":["same","same"],"title_contains":null,"include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn metadata_list_rejects_empty_title_query() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":"","include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn metadata_list_page_size_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"0","after_session_id":null}}"#,
        ))?;
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"101","after_session_id":null}}"#,
        ))?;
        Ok(())
    }

    #[test]
    fn metadata_replacement_rejects_empty_title() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":"","tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn metadata_replacement_requires_title_member() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn metadata_replacement_rejects_duplicate_tags() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":null,"tags":["same","same"],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn duplicate_metadata_attribute_member_is_malformed() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":null,"tags":[],"attributes":{"same":"first","\u0073ame":"second"},"archived":false}}}"#,
        );
    }

    #[test]
    fn metadata_required_tag_deserializer_has_no_deployment_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let required_tags = serde_json::to_string(&numbered_metadata_strings(3))?;
        let json = format!(
            r#"{{"version":1,"request_id":"1","request":{{"type":"list_session_metadata","required_tags":{required_tags},"title_contains":null,"include_archived":false,"page_size":"50","after_session_id":null}}}}"#
        );
        decode_client_line(&line(&json))?;
        Ok(())
    }

    #[test]
    fn metadata_summary_tag_deserializer_has_no_deployment_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut tags = numbered_metadata_strings(3);
        tags.sort();
        let tags = serde_json::to_string(&tags)?;
        let json = format!(
            r#"{{"version":1,"request_id":"1","message":{{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"}},"dangerous_tool_auto_approval":false,"title":null,"tags":{tags},"archived":false,"last_writer":{{"updated_at_unix_micros":"1","actor":{{"type":"user"}}}}}}}}"#
        );
        decode_server_line(&line(&json))?;
        Ok(())
    }

    /// Pins one actor's exact bytes on both frames that carry a last writer.
    /// A failure names the actor at the call site rather than a loop position.
    #[track_caller]
    fn assert_metadata_actor_round_trips(
        actor: MetadataActor,
        expected_actor_json: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let writer = MetadataLastWriter::new(CanonicalU64::new(1), actor);
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::SessionMetadataReplaced {
                session_id: uuid(1),
                metadata: SessionMetadata::empty(),
                last_writer: writer,
            },
            &format!(
                r#"{{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{{"title":null,"tags":[],"attributes":{{}},"archived":false}},"last_writer":{{"updated_at_unix_micros":"1","actor":{expected_actor_json}}}}}"#
            ),
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::SessionMetadata {
                session_id: uuid(1),
                metadata: SessionMetadata::empty(),
                last_writer: Some(writer),
            },
            &format!(
                r#"{{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{{"title":null,"tags":[],"attributes":{{}},"archived":false}},"last_writer":{{"updated_at_unix_micros":"1","actor":{expected_actor_json}}}}}"#
            ),
        )?;
        Ok(())
    }

    /// the last-writer actor projects every agency durable metadata can
    /// record. The domain projection this pins is total by type, so a later
    /// agency reaches the wire only through a variant added here.
    #[test]
    fn metadata_writer_actor_round_trips_every_agency() -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_actor_round_trips(MetadataActor::User {}, r#"{"type":"user"}"#)?;
        assert_metadata_actor_round_trips(MetadataActor::Core {}, r#"{"type":"core"}"#)?;
        assert_metadata_actor_round_trips(
            MetadataActor::Model { turn_id: uuid(2) },
            r#"{"type":"model","turn_id":"00000000-0000-0000-0000-000000000002"}"#,
        )?;
        assert_metadata_actor_round_trips(MetadataActor::Recovery {}, r#"{"type":"recovery"}"#)?;
        assert_metadata_actor_round_trips(
            MetadataActor::Tool {
                tool_request_id: uuid(3),
            },
            r#"{"type":"tool","tool_request_id":"00000000-0000-0000-0000-000000000003"}"#,
        )?;
        Ok(())
    }

    /// the actor vocabulary stays closed — an unadmitted spelling and a
    /// variant carrying the wrong reference are both malformed frames.
    #[test]
    fn metadata_writer_actor_rejects_unadmitted_shapes() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":{"updated_at_unix_micros":"1","actor":{"type":"operator"}}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":{"updated_at_unix_micros":"1","actor":{"type":"tool","turn_id":"00000000-0000-0000-0000-000000000002"}}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":{"updated_at_unix_micros":"1","actor":{"type":"recovery","turn_id":"00000000-0000-0000-0000-000000000002"}}}}"#,
        );
    }

    #[test]
    fn metadata_capacity_matches_domain_and_frame_headroom()
    -> Result<(), Box<dyn std::error::Error>> {
        let exact = SessionMetadata::try_new(
            Some("\u{1}".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES)),
            Vec::new(),
            Vec::new(),
            false,
        )?;
        assert!(
            SessionMetadata::try_new(
                Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES + 1)),
                Vec::new(),
                Vec::new(),
                false,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new(
                None,
                vec!["x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1)],
                Vec::new(),
                false,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new_with_count_limits(
                None,
                numbered_metadata_strings(3),
                Vec::new(),
                false,
                Some(2),
                None,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new_with_count_limits(
                None,
                Vec::new(),
                numbered_metadata_attributes(3),
                false,
                None,
                Some(2),
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new(
                None,
                Vec::new(),
                vec![(
                    "x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1),
                    String::new(),
                )],
                false,
            )
            .is_err()
        );

        let encoded = encode_server_line(&ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::SessionMetadataReplaced {
                session_id: uuid(1),
                metadata: exact,
                last_writer: MetadataLastWriter::new(CanonicalU64::new(1), MetadataActor::User {}),
            },
        )?)?;
        assert!(encoded.len() < super::MAX_FRAME_BYTES);
        Ok(())
    }

    #[test]
    fn metadata_filter_capacity_is_enforced_before_mapping()
    -> Result<(), Box<dyn std::error::Error>> {
        let exact = ClientRequest::ListSessionMetadata {
            required_tags: Vec::new(),
            title_contains: Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES)),
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, exact)?;

        let over_total = ClientRequest::ListSessionMetadata {
            required_tags: Vec::new(),
            title_contains: Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES + 1)),
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, over_total),
            Err(FrameValidationError::MetadataShape)
        );

        let over_indexed = ClientRequest::ListSessionMetadata {
            required_tags: vec!["x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1)],
            title_contains: None,
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, over_indexed),
            Err(FrameValidationError::MetadataShape)
        );

        Ok(())
    }

    #[test]
    fn metadata_summary_enforces_aggregate_utf8_capacity() -> Result<(), Box<dyn std::error::Error>>
    {
        let individually_valid_but_oversized = ServerMessage::SessionMetadataSummary {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(2),
            },
            dangerous_tool_auto_approval: false,
            title: Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES)),
            tags: vec![String::from("tag")],
            archived: false,
            last_writer: Some(MetadataLastWriter::new(
                CanonicalU64::new(1),
                MetadataActor::User {},
            )),
        };

        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(1)?,
                individually_valid_but_oversized,
            ),
            Err(FrameValidationError::MetadataShape)
        );
        Ok(())
    }

    #[test]
    fn metadata_summary_requires_nullable_title_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"},"dangerous_tool_auto_approval":false,"tags":[],"archived":false,"last_writer":null}}"#,
        );
    }

    #[test]
    fn metadata_summary_requires_nullable_last_writer_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"},"dangerous_tool_auto_approval":false,"title":null,"tags":[],"archived":false}}"#,
        );
    }

    #[test]
    fn metadata_page_end_requires_nullable_cursor_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_page_end","session_count":"0"}}"#,
        );
    }

    #[test]
    fn metadata_point_read_requires_nullable_last_writer_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn metadata_point_read_requires_nullable_title_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"tags":[],"attributes":{},"archived":false},"last_writer":null}}"#,
        );
    }

    #[track_caller]
    fn assert_metadata_message_rejected(
        message: ServerMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message),
            Err(FrameValidationError::MetadataShape)
        );
        Ok(())
    }

    #[test]
    fn metadata_summary_rejects_unsorted_tags() -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_message_rejected(ServerMessage::SessionMetadataSummary {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(2),
            },
            dangerous_tool_auto_approval: false,
            title: None,
            tags: vec![String::from("z"), String::from("a")],
            archived: false,
            last_writer: Some(MetadataLastWriter::new(
                CanonicalU64::new(1),
                MetadataActor::User {},
            )),
        })
    }

    #[test]
    fn metadata_summary_rejects_unwritten_nondefault_content()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_message_rejected(ServerMessage::SessionMetadataSummary {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(2),
            },
            dangerous_tool_auto_approval: false,
            title: Some(String::from("unwritten")),
            tags: Vec::new(),
            archived: false,
            last_writer: None,
        })
    }

    #[test]
    fn metadata_read_rejects_written_content_without_a_writer()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_message_rejected(ServerMessage::SessionMetadata {
            session_id: uuid(1),
            metadata: metadata(false)?,
            last_writer: None,
        })
    }

    #[test]
    fn metadata_page_rejects_a_cursor_after_an_empty_page() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_metadata_message_rejected(ServerMessage::SessionMetadataPageEnd {
            session_count: CanonicalU64::new(0),
            next_after_session_id: Some(uuid(1)),
        })
    }

    #[test]
    fn metadata_page_count_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::SessionMetadataPageEnd {
                session_count: CanonicalU64::new(101),
                next_after_session_id: None,
            },
        )?;
        Ok(())
    }
}
