//! Scalars and rejection protocol tests.

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn canonical_decimal_spellings_are_required() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"01","request":{"type":"list_sessions"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"+1","request":{"type":"list_sessions"}}"#,
        );
    }

    #[test]
    fn canonical_dollar_amount_matches_the_wire_decimal_representation() {
        assert!(
            CanonicalDollarAmount::try_new(String::from("79228162514264337593543950335")).is_ok()
        );
        assert!(
            CanonicalDollarAmount::try_new(String::from("0.0000000000000000000000000001")).is_ok()
        );
        assert!(
            CanonicalDollarAmount::try_new(String::from("79228162514264337593543950336")).is_err()
        );
    }

    #[test]
    fn canonical_uuid_spellings_are_required() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"read_transcript","session_id":"00000000-0000-0000-0000-00000000000A"}}"#,
        );
    }

    #[test]
    fn command_sentinels_are_rejected() {
        assert_command_sentinel_rejected("00000000-0000-0000-0000-000000000000");
        assert_command_sentinel_rejected("ffffffff-ffff-ffff-ffff-ffffffffffff");
    }

    #[test]
    fn zero_client_request_id_is_rejected() {
        assert!(
            decode_client_line(&line(
                r#"{"version":1,"request_id":"0","request":{"type":"list_sessions"}}"#
            ))
            .is_err()
        );
    }

    #[test]
    fn rejection_detail_shape_is_closed_and_code_bound() -> Result<(), Box<dyn std::error::Error>> {
        assert!(
            ServerFrame::try_new(
                request(1)?,
                ServerMessage::Error {
                    code: ErrorCode::Rejected,
                    message: "rejected".to_owned(),
                    detail: ErrorDetail::none(),
                },
            )
            .is_err()
        );
        assert!(
            ServerFrame::try_new(
                request(1)?,
                ServerMessage::Error {
                    code: ErrorCode::Internal,
                    message: "failed".to_owned(),
                    detail: ErrorDetail::rejected(RejectionDetail::SessionNotFound {
                        session_id: uuid(2),
                    }),
                },
            )
            .is_err()
        );
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: "rejected".to_owned(),
                detail: ErrorDetail::rejected(RejectionDetail::SessionNotFound {
                    session_id: uuid(2),
                }),
            },
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        assert!(decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"error","code":"internal","message":"failed","detail":null}}"#
        ))
        .is_err());
        Ok(())
    }

    #[test]
    fn commit_ambiguity_has_one_stable_error_code() -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::CommitAmbiguous,
                message: "ambiguous commit".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        let encoded = encode_server_line(&frame)?;
        assert!(
            encoded
                .windows(br#""code":"commit_ambiguous""#.len())
                .any(|window| window == br#""code":"commit_ambiguous""#)
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn publication_ambiguity_has_one_stable_error_code() -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::PublicationAmbiguous,
                message: "ambiguous publication".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        let encoded = encode_server_line(&frame)?;
        assert!(
            encoded
                .windows(br#""code":"publication_ambiguous""#.len())
                .any(|window| window == br#""code":"publication_ambiguous""#)
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn uncorrelated_identity_is_reserved_for_server_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let error = ServerFrame::try_new(
            RequestId::uncorrelated(),
            ServerMessage::Error {
                code: ErrorCode::MalformedFrame,
                message: "malformed".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&error)?)?, error);
        let version_error = ServerFrame::try_new(
            RequestId::uncorrelated(),
            ServerMessage::Error {
                code: ErrorCode::UnsupportedVersion,
                message: "unsupported version".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        assert_eq!(
            decode_server_line(&encode_server_line(&version_error)?)?,
            version_error
        );
        assert!(
            ServerFrame::try_new(
                RequestId::uncorrelated(),
                ServerMessage::Error {
                    code: ErrorCode::NotFound,
                    message: "not found".to_owned(),
                    detail: ErrorDetail::none(),
                },
            )
            .is_err()
        );
        assert!(
            ServerFrame::try_new(RequestId::uncorrelated(), ServerMessage::SessionsStart {},)
                .is_err()
        );
        assert!(
            ServerFrame::try_new(
                RequestId::uncorrelated(),
                ServerMessage::TranscriptTurn {
                    turn_id: uuid(1),
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::Failed {
                        terminal_frontier_id: uuid(2),
                        terminal_attempt_id: None,
                        terminal_model_call: None,
                    },
                },
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ClientFrame>(
                r#"{"version":1,"request_id":"0","request":{"type":"list_sessions"}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ServerFrame>(
                r#"{"version":1,"request_id":"0","message":{"type":"sessions_start"}}"#
            )
            .is_err()
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"0","message":{"type":"error","code":"not_found","message":"not found"}}"#,
        );
        Ok(())
    }
}
