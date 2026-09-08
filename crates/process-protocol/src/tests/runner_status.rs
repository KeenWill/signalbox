use super::support::*;
use crate::*;

#[test]
fn runner_status_rejects_page_sizes_outside_the_contract() -> Result<(), Box<dyn std::error::Error>>
{
    for page_size in [0, 101, u32::MAX] {
        assert_eq!(
            ClientFrame::try_new(
                request(1)?,
                ClientRequest::ReadRunnerStatus {
                    page_size,
                    after: None
                }
            ),
            Err(FrameValidationError::RunnerStatusShape)
        );
    }
    for page_size in [1, 100] {
        let frame = ClientFrame::try_new(
            request(1)?,
            ClientRequest::ReadRunnerStatus {
                page_size,
                after: None,
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&frame)?)?, frame);
    }
    Ok(())
}

#[test]
fn runner_status_cursor_requires_one_closed_identity() {
    for text in [
        r#"{"type":"read_runner_status","page_size":1}"#,
        r#"{"type":"read_runner_status","page_size":1,"after":{"type":"operation_failure"}}"#,
        r#"{"type":"read_runner_status","page_size":1,"after":null,"offset":2}"#,
        r#"{"type":"read_runner_status","page_size":1,"after":{"type":"workspace_leak","runner_id":"00000000-0000-0000-0000-000000000001","locator":"work","entry_digest":"invalid","extra":1}}"#,
    ] {
        assert!(
            serde_json::from_str::<ClientRequest>(text).is_err(),
            "{text}"
        );
    }
}

#[test]
fn runner_status_empty_page_cannot_claim_continuation() -> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::RunnerStatusEnd {
        runner_count: CanonicalU64::new(0),
        failure_count: CanonicalU64::new(0),
        leak_count: CanonicalU64::new(0),
        next_after: Some(RunnerStatusCursor::OperationFailure {
            authorization_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(1)),
        }),
    };
    assert_eq!(
        ServerFrame::try_new(request(1)?, message),
        Err(FrameValidationError::RunnerStatusShape)
    );
    Ok(())
}

#[test]
fn runner_status_count_limit_includes_current_facts() -> Result<(), Box<dyn std::error::Error>> {
    for (runners, failures, accepted) in [
        (100, 0, true),
        (99, 1, true),
        (100, 1, false),
        (101, 0, false),
        (u64::MAX, 1, false),
    ] {
        let result = ServerFrame::try_new(
            request(1)?,
            ServerMessage::RunnerStatusEnd {
                runner_count: CanonicalU64::new(runners),
                failure_count: CanonicalU64::new(failures),
                leak_count: CanonicalU64::new(0),
                next_after: None,
            },
        );
        assert_eq!(result.is_ok(), accepted);
    }
    for after in [
        RunnerStatusCursor::Enrollment {
            runner_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(1)),
        },
        RunnerStatusCursor::Placement {
            session_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(2)),
        },
    ] {
        let frame = ClientFrame::try_new(
            request(1)?,
            ClientRequest::ReadRunnerStatus {
                page_size: 1,
                after: Some(after),
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&frame)?)?, frame);
    }
    Ok(())
}
