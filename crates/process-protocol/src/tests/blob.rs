//! Blob protocol tests.

use super::support::*;
use crate::*;

/// blob upload requests carry one exact tagged digest, positive
/// length, and canonical bounded chunk without an implicit blob class.
#[test]
fn blob_upload_requests_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let begin = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::BeginBlobUpload {
            expected_digest: digest,
            expected_length_bytes: CanonicalU64::new(2),
        },
    )?;
    let append = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(2)?,
        ClientRequest::AppendBlobUpload {
            chunk: BlobChunk::new(vec![0, 255]),
        },
    )?;
    let commit = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(3)?,
        ClientRequest::CommitBlobUpload {},
    )?;
    let abort = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(4)?,
        ClientRequest::AbortBlobUpload {},
    )?;

    let encoded_begin = encode_client_line(&begin)?;
    let encoded_append = encode_client_line(&append)?;
    let encoded_commit = encode_client_line(&commit)?;
    let encoded_abort = encode_client_line(&abort)?;
    assert_eq!(
        String::from_utf8(encoded_begin.clone())?,
        format!(
            "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"begin_blob_upload\",\"expected_digest\":\"{digest}\",\"expected_length_bytes\":\"2\"}}}}\n"
        )
    );
    assert_eq!(
        String::from_utf8(encoded_append.clone())?,
        "{\"version\":1,\"request_id\":\"2\",\"request\":{\"type\":\"append_blob_upload\",\"chunk\":\"AP8=\"}}\n"
    );
    assert_eq!(
        String::from_utf8(encoded_commit.clone())?,
        "{\"version\":1,\"request_id\":\"3\",\"request\":{\"type\":\"commit_blob_upload\"}}\n"
    );
    assert_eq!(
        String::from_utf8(encoded_abort.clone())?,
        "{\"version\":1,\"request_id\":\"4\",\"request\":{\"type\":\"abort_blob_upload\"}}\n"
    );
    assert_eq!(decode_client_line(&encoded_begin)?, begin);
    assert_eq!(decode_client_line(&encoded_append)?, append);
    assert_eq!(decode_client_line(&encoded_commit)?, commit);
    assert_eq!(decode_client_line(&encoded_abort)?, abort);
    assert!(
        ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(5)?,
            ClientRequest::BeginBlobUpload {
                expected_digest: digest,
                expected_length_bytes: CanonicalU64::new(0),
            },
        )
        .is_ok()
    );
    Ok(())
}

/// one maximum decoded blob chunk fits the frame cap, while an
/// empty or one-byte-larger append is rejected before encoding.
#[test]
fn blob_upload_chunk_bound_is_enforced() -> Result<(), Box<dyn std::error::Error>> {
    let maximum = ClientRequest::AppendBlobUpload {
        chunk: BlobChunk::new(vec![b'x'; crate::MAX_BLOB_CHUNK_BYTES]),
    };
    let oversized = ClientRequest::AppendBlobUpload {
        chunk: BlobChunk::new(vec![b'x'; crate::MAX_BLOB_CHUNK_BYTES + 1]),
    };
    let empty = ClientRequest::AppendBlobUpload {
        chunk: BlobChunk::new(Vec::new()),
    };
    let maximum_frame = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        RequestId::try_new(u64::MAX)?,
        maximum,
    )?;

    assert!(encode_client_line(&maximum_frame)?.len() <= crate::MAX_FRAME_BYTES);
    assert_eq!(
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, oversized),
        Err(FrameValidationError::BlobUploadShape)
    );
    assert_eq!(
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(2)?, empty),
        Err(FrameValidationError::BlobUploadShape)
    );
    Ok(())
}

/// upload lifecycle receipts echo the exact verified identity and
/// positive cumulative sizes.
#[test]
fn blob_upload_acknowledgements_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>>
{
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::BlobUploadBegun {
            expected_digest: digest,
            expected_length_bytes: CanonicalU64::new(9),
        },
        &format!(
            "{{\"type\":\"blob_upload_begun\",\"expected_digest\":\"{digest}\",\"expected_length_bytes\":\"9\"}}"
        ),
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::BlobUploadAlreadyPresent {
            digest,
            byte_length: CanonicalU64::new(9),
        },
        &format!(
            "{{\"type\":\"blob_upload_already_present\",\"digest\":\"{digest}\",\"byte_length\":\"9\"}}"
        ),
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::BlobUploadAppended {
            assembled_length_bytes: CanonicalU64::new(7),
        },
        r#"{"type":"blob_upload_appended","assembled_length_bytes":"7"}"#,
    )?;
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::BlobUploadCommitted {
            digest,
            byte_length: CanonicalU64::new(9),
        },
        &format!(
            "{{\"type\":\"blob_upload_committed\",\"digest\":\"{digest}\",\"byte_length\":\"9\"}}"
        ),
    )?;
    assert_server_message_round_trip(
        request(5)?,
        ServerMessage::BlobUploadAborted {},
        r#"{"type":"blob_upload_aborted"}"#,
    )?;
    Ok(())
}

/// bulk-ingest ownership and every upload lifecycle failure use
/// one exhaustive content-silent invalid-request vocabulary.
#[test]
fn blob_upload_refusals_have_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let expected_digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let actual_digest = CanonicalBlobDigest::from_bytes([0xcd; 32]);
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("bulk ingest was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BulkIngestAlreadyInProgress {
                active_kind: BulkIngestKind::BlobUpload,
            }),
        },
        r#"{"type":"error","code":"invalid_request","message":"bulk ingest was rejected","detail":{"type":"bulk_ingest_already_in_progress","active_kind":"blob_upload"}}"#,
    )?;
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadAlreadyInProgress {}),
        },
        r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_already_in_progress"}}"#,
    )?;
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadNotInProgress {}),
        },
        r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_not_in_progress"}}"#,
    )?;
    assert_server_message_round_trip(
        request(4)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadLengthOutOfRange {
                min_length_bytes: CanonicalU64::new(1),
                max_length_bytes: CanonicalU64::new(8),
                declared_length_bytes: CanonicalU64::new(9),
            }),
        },
        r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_length_out_of_range","min_length_bytes":"1","max_length_bytes":"8","declared_length_bytes":"9"}}"#,
    )?;
    assert_server_message_round_trip(
        request(5)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadSizeExceeded {
                expected_length_bytes: CanonicalU64::new(8),
                actual_length_bytes: CanonicalU64::new(9),
            }),
        },
        r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_size_exceeded","expected_length_bytes":"8","actual_length_bytes":"9"}}"#,
    )?;
    assert_server_message_round_trip(
        request(6)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadLengthMismatch {
                expected_length_bytes: CanonicalU64::new(8),
                actual_length_bytes: CanonicalU64::new(7),
            }),
        },
        r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_length_mismatch","expected_length_bytes":"8","actual_length_bytes":"7"}}"#,
    )?;
    assert_server_message_round_trip(
        request(7)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadDigestMismatch {
                expected_digest,
                actual_digest,
            }),
        },
        &format!(
            "{{\"type\":\"error\",\"code\":\"invalid_request\",\"message\":\"blob upload was rejected\",\"detail\":{{\"type\":\"blob_upload_digest_mismatch\",\"expected_digest\":\"{expected_digest}\",\"actual_digest\":\"{actual_digest}\"}}}}"
        ),
    )?;
    Ok(())
}

/// a direct metadata read has exact closed request and response
/// shapes with canonical decimal facts.
#[test]
fn blob_metadata_wire_shapes_are_exact() -> Result<(), Box<dyn std::error::Error>> {
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let metadata = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::ReadBlobMetadata { digest },
    )?;
    let encoded_metadata = encode_client_line(&metadata)?;

    assert_eq!(
        String::from_utf8(encoded_metadata.clone())?,
        format!(
            "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"read_blob_metadata\",\"digest\":\"{digest}\"}}}}\n"
        )
    );
    assert_eq!(decode_client_line(&encoded_metadata)?, metadata);
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::BlobMetadata {
            digest,
            byte_length: CanonicalU64::new(9),
            replica_count: CanonicalU64::new(1),
        },
        &format!(
            "{{\"type\":\"blob_metadata\",\"digest\":\"{digest}\",\"byte_length\":\"9\",\"replica_count\":\"1\"}}"
        ),
    )?;
    Ok(())
}

/// a direct range read has exact closed request and response
/// shapes with canonical decimal bounds.
#[test]
fn blob_range_wire_shapes_are_exact() -> Result<(), Box<dyn std::error::Error>> {
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let offset = 7_u64;
    let length = 2_u64;
    let offset_bytes = CanonicalU64::new(offset);
    let length_bytes = CanonicalU64::new(length);
    let chunk = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ClientRequest::ReadBlobChunk {
            digest,
            offset_bytes,
            length_bytes,
        },
    )?;
    let encoded_chunk = encode_client_line(&chunk)?;

    assert_eq!(
        String::from_utf8(encoded_chunk.clone())?,
        format!(
            "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"read_blob_chunk\",\"digest\":\"{digest}\",\"offset_bytes\":\"{offset}\",\"length_bytes\":\"{length}\"}}}}\n"
        )
    );
    assert_eq!(decode_client_line(&encoded_chunk)?, chunk);
    assert_server_message_round_trip(
        request(2)?,
        ServerMessage::BlobChunkRead {
            blob_length_bytes: CanonicalU64::new(u64::MAX),
            digest,
            offset_bytes,
            bytes: BlobChunk::new(vec![0, 255]),
        },
        &format!(
            "{{\"type\":\"blob_chunk\",\"blob_length_bytes\":\"18446744073709551615\",\"digest\":\"{digest}\",\"offset_bytes\":\"{offset}\",\"bytes\":\"AP8=\"}}"
        ),
    )?;
    Ok(())
}

/// a successful range response must represent its exact
/// half-open byte range.
#[test]
fn blob_range_response_rejects_overflowing_end() -> Result<(), Box<dyn std::error::Error>> {
    let result = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::BlobChunkRead {
            blob_length_bytes: CanonicalU64::new(u64::MAX),
            digest: CanonicalBlobDigest::from_bytes([0xab; 32]),
            offset_bytes: CanonicalU64::new(u64::MAX),
            bytes: BlobChunk::new(vec![0]),
        },
    );

    assert_eq!(result, Err(FrameValidationError::BlobReadShape));
    Ok(())
}

/// invalid direct range lengths remain decodable so the daemon
/// can return the contracted typed invalid-request response.
#[test]
fn blob_read_length_bound_reaches_request_handling() -> Result<(), Box<dyn std::error::Error>> {
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let zero = ClientRequest::ReadBlobChunk {
        digest,
        offset_bytes: CanonicalU64::new(0),
        length_bytes: CanonicalU64::new(0),
    };
    let oversized = ClientRequest::ReadBlobChunk {
        digest,
        offset_bytes: CanonicalU64::new(0),
        length_bytes: CanonicalU64::new(crate::MAX_BLOB_READ_BYTES as u64 + 1),
    };
    assert!(ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, zero).is_ok());
    assert!(ClientFrame::try_new_for_version(ProtocolVersion::One, request(2)?, oversized).is_ok());
    assert_server_message_round_trip(
        request(3)?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob read was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobReadLengthOutOfRange {
                min_length_bytes: CanonicalU64::new(1),
                max_length_bytes: CanonicalU64::new(crate::MAX_BLOB_READ_BYTES as u64),
                requested_length_bytes: CanonicalU64::new(0),
            }),
        },
        r#"{"type":"error","code":"invalid_request","message":"blob read was rejected","detail":{"type":"blob_read_length_out_of_range","min_length_bytes":"1","max_length_bytes":"4194304","requested_length_bytes":"0"}}"#,
    )?;
    Ok(())
}

/// an exact maximum direct range response remains inside the
/// unchanged frame ceiling.
#[test]
fn maximum_blob_read_response_fits_one_frame() -> Result<(), Box<dyn std::error::Error>> {
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let maximum = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        RequestId::try_new(u64::MAX)?,
        ServerMessage::BlobChunkRead {
            blob_length_bytes: CanonicalU64::new(u64::MAX),
            digest,
            offset_bytes: CanonicalU64::new(0),
            bytes: BlobChunk::new(vec![b'x'; crate::MAX_BLOB_READ_BYTES]),
        },
    )?;

    assert!(encode_server_line(&maximum)?.len() <= crate::MAX_FRAME_BYTES);
    Ok(())
}

/// exhausting absent replicas has a content-silent missing code.
#[test]
fn blob_missing_failure_is_typed() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::BlobMissing,
            message: String::from("all recorded blob replicas are missing"),
            detail: ErrorDetail::none(),
        },
        r#"{"type":"error","code":"blob_missing","message":"all recorded blob replicas are missing"}"#,
    )?;
    Ok(())
}

/// exhausting corrupt replicas has a content-silent corruption
/// code.
#[test]
fn blob_corrupt_failure_is_typed() -> Result<(), Box<dyn std::error::Error>> {
    assert_server_message_round_trip(
        request(1)?,
        ServerMessage::Error {
            code: ErrorCode::BlobCorrupt,
            message: String::from("all usable blob replicas are corrupt"),
            detail: ErrorDetail::none(),
        },
        r#"{"type":"error","code":"blob_corrupt","message":"all usable blob replicas are corrupt"}"#,
    )?;
    Ok(())
}

/// a size-exceeded refusal cannot claim the impossible zero
/// expected length that begin-upload admission rejects.
#[test]
fn blob_upload_size_exceeded_rejects_zero_expected_length() -> Result<(), Box<dyn std::error::Error>>
{
    let message = ServerMessage::Error {
        code: ErrorCode::InvalidRequest,
        message: String::from("blob upload was rejected"),
        detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadSizeExceeded {
            expected_length_bytes: CanonicalU64::new(0),
            actual_length_bytes: CanonicalU64::new(1),
        }),
    };

    assert_eq!(
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message),
        Err(FrameValidationError::BlobUploadShape)
    );
    Ok(())
}

/// a length-mismatch refusal cannot claim the impossible zero
/// expected length that begin-upload admission rejects.
#[test]
fn blob_upload_length_mismatch_rejects_zero_expected_length()
-> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::Error {
        code: ErrorCode::InvalidRequest,
        message: String::from("blob upload was rejected"),
        detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadLengthMismatch {
            expected_length_bytes: CanonicalU64::new(0),
            actual_length_bytes: CanonicalU64::new(1),
        }),
    };

    assert_eq!(
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message),
        Err(FrameValidationError::BlobUploadShape)
    );
    Ok(())
}

#[test]
fn blob_range_response_accepts_empty_bytes_beyond_eof() -> Result<(), Box<dyn std::error::Error>> {
    let frame = ServerFrame::try_new_for_version(
        ProtocolVersion::One,
        request(1)?,
        ServerMessage::BlobChunkRead {
            digest: CanonicalBlobDigest::from_bytes([0xab; 32]),
            blob_length_bytes: CanonicalU64::new(9),
            offset_bytes: CanonicalU64::new(u64::MAX),
            bytes: BlobChunk::new(Vec::new()),
        },
    )?;
    assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    Ok(())
}
