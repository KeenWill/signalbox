//! Imported and delegation entries protocol tests.

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn imported_text_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let imported_text = ServerMessage::TranscriptTextEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(1),
            entry_id: uuid(2),
            entry: TranscriptTextEntry::Imported {
                imported_conversation_id: uuid(3),
                imported_entry_id: uuid(4),
                source_speaker: ImportedSourceSpeaker::Attested {
                    speaker: ImportedSpeaker::User,
                },
            },
        };
        let text_frame =
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(8)?, imported_text)?;
        let encoded_text = encode_server_line(&text_frame)?;
        assert!(String::from_utf8(encoded_text.clone())?.contains(
            r#""entry":{"type":"imported","imported_conversation_id":"00000000-0000-0000-0000-000000000003","imported_entry_id":"00000000-0000-0000-0000-000000000004","source_speaker":{"type":"attested","speaker":"user"}}"#
        ));
        assert_eq!(decode_server_line(&encoded_text)?, text_frame);
        Ok(())
    }

    #[test]
    fn imported_conservative_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let imported_conservative = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(1),
            source_session_id: uuid(1),
            entry_id: uuid(5),
            entry: TranscriptEntry::Imported {
                imported_conversation_id: uuid(3),
                imported_entry_id: uuid(6),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::ToolResult,
            },
        };
        let conservative = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(9)?,
            imported_conservative,
        )?;
        let encoded_conservative = encode_server_line(&conservative)?;
        assert!(
            String::from_utf8(encoded_conservative.clone())?.contains(
                r#""source_speaker":{"type":"not_attested"},"content_kind":"tool_result""#
            )
        );
        assert_eq!(decode_server_line(&encoded_conservative)?, conservative);
        Ok(())
    }

    #[test]
    fn delegated_task_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(2),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegatedTask {
                spawning_request_id: uuid(4),
                parent_session_id: uuid(1),
                parent_turn_id: uuid(5),
                content: String::from("inspect the durable result"),
            },
        };

        assert_server_message_round_trip(
            request(10)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000002","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegated_task","spawning_request_id":"00000000-0000-0000-0000-000000000004","parent_session_id":"00000000-0000-0000-0000-000000000001","parent_turn_id":"00000000-0000-0000-0000-000000000005","content":"inspect the durable result"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn delegation_message_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(1),
            source_session_id: uuid(2),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegationMessage {
                spawning_request_id: uuid(4),
                message_id: uuid(5),
                sender_session_id: uuid(1),
                recipient_session_id: uuid(2),
                ordinal: CanonicalU64::new(1),
                delivery_sequence: CanonicalU64::new(2),
                content: String::from("continue with the checked input"),
            },
        };

        assert_server_message_round_trip(
            request(11)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"1","source_session_id":"00000000-0000-0000-0000-000000000002","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegation_message","spawning_request_id":"00000000-0000-0000-0000-000000000004","message_id":"00000000-0000-0000-0000-000000000005","sender_session_id":"00000000-0000-0000-0000-000000000001","recipient_session_id":"00000000-0000-0000-0000-000000000002","ordinal":"1","delivery_sequence":"2","content":"continue with the checked input"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn foreground_delegation_result_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(2),
            source_session_id: uuid(1),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegationResult {
                await_request_id: uuid(4),
                spawning_request_id: uuid(5),
                child_session_id: uuid(2),
                mode: DelegationWaitMode::Foreground,
                delivery_sequence: None,
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("checked result")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: uuid(2),
                    child_turn_id: uuid(6),
                },
            },
        };

        assert_server_message_round_trip(
            request(12)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"2","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegation_result","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000005","child_session_id":"00000000-0000-0000-0000-000000000002","mode":"foreground","delivery_sequence":null,"outcome":"returned","content":"checked result","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000002","child_turn_id":"00000000-0000-0000-0000-000000000006"}}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn background_delegation_result_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(3),
            source_session_id: uuid(1),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegationResult {
                await_request_id: uuid(4),
                spawning_request_id: uuid(5),
                child_session_id: uuid(2),
                mode: DelegationWaitMode::Background,
                delivery_sequence: Some(CanonicalU64::new(7)),
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("wake result")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: uuid(2),
                    child_turn_id: uuid(6),
                },
            },
        };

        assert_server_message_round_trip(
            request(13)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"3","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegation_result","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000005","child_session_id":"00000000-0000-0000-0000-000000000002","mode":"background","delivery_sequence":"7","outcome":"returned","content":"wake result","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000002","child_turn_id":"00000000-0000-0000-0000-000000000006"}}}"#,
        )?;
        Ok(())
    }
}
