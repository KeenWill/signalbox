//! Frame classification protocol tests.

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn unknown_request_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_sessions","extra":true}}"#,
        );
    }

    #[test]
    fn missing_required_request_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"read_transcript"}}"#,
        );
    }

    #[test]
    fn wrong_typed_request_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":1,"request":{"type":"list_sessions"}}"#,
        );
    }

    #[test]
    fn unknown_tagged_request_variants_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"future_request"}}"#,
        );
    }

    #[test]
    fn unknown_top_level_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_sessions"},"extra":true}"#,
        );
    }

    #[test]
    fn unsupported_version_precedes_payload_decoding() {
        assert_unsupported_version("-1");
        assert_unsupported_version("2");
        assert_unsupported_version("18446744073709551616");
        assert_client_malformed(
            r#"{"version":1.0,"request_id":"9","request":{"type":"list_sessions"}}"#,
        );
    }

    #[test]
    fn unsupported_version_is_classified_at_the_container_depth_limit() {
        let future = unsupported_version_with_nested_object_payload(MAX_JSON_CONTAINER_DEPTH - 1);
        let error = decode_client_line(&line(&future))
            .expect_err("the maximum admitted depth reaches version classification");
        assert_eq!(error.kind(), FrameDecodeErrorKind::UnsupportedVersion);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn container_depth_beyond_the_limit_is_malformed() {
        let future = unsupported_version_with_nested_object_payload(MAX_JSON_CONTAINER_DEPTH);
        let error =
            decode_client_line(&line(&future)).expect_err("excessive nesting must be rejected");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn nested_duplicates_are_malformed_before_unsupported_version() {
        let error = decode_client_line(&line(
            r#"{"version":1,"request_id":"9","request":{"future":1,"future":2}}"#,
        ))
        .expect_err("nested duplicate members are malformed for every version");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn duplicate_top_level_members_are_malformed_before_classification() {
        let duplicate_version = decode_client_line(&line(
            r#"{"version":1,"version":1,"request_id":"9","request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("a duplicate version is malformed");
        assert_eq!(
            duplicate_version.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(duplicate_version.request_id().value(), 9);

        let reversed_version = decode_client_line(&line(
            r#"{"version":1,"version":1,"request_id":"9","request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("version order cannot alter duplicate classification");
        assert_eq!(
            reversed_version.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(reversed_version.request_id().value(), 9);

        let duplicate_request = decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request_id":"2","request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("a duplicate request identity is malformed");
        assert_eq!(
            duplicate_request.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(duplicate_request.request_id().value(), 0);

        let duplicate_payload = decode_client_line(&line(
            r#"{"version":1,"request_id":"9","request":{"type":"list_sessions"},"request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("a duplicate payload is malformed");
        assert_eq!(
            duplicate_payload.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(duplicate_payload.request_id().value(), 9);
    }
}
