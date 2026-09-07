//! Review protocol tests.

use super::support::*;
use crate::*;

/// review target registration has one exact closed shape.
#[test]
fn review_target_exchange_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(1)?;
    let request_value = ClientRequest::CreateReviewTarget {
        command_id: command(2)?,
        target_id: uuid(3),
        provider: String::from("example-host"),
        repository: String::from("example/repository"),
        subject: ReviewTargetSubject::ChangeRequest {
            number: CanonicalU64::new(42),
        },
        head_revision: String::from("head-revision"),
        base_revision: Some(String::from("base-revision")),
        stack_parent_target_id: None,
    };
    let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"create_review_target\",\
         \"command_id\":\"00000000-0000-0000-0000-000000000002\",\
         \"target_id\":\"00000000-0000-0000-0000-000000000003\",\
         \"provider\":\"example-host\",\"repository\":\"example/repository\",\
         \"subject\":{\"kind\":\"change_request\",\"number\":\"42\"},\
         \"head_revision\":\"head-revision\",\"base_revision\":\"base-revision\",\
         \"stack_parent_target_id\":null}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    assert_server_message_round_trip(
        request_id,
        ServerMessage::ReviewTargetCreated { target_id: uuid(3) },
        r#"{"type":"review_target_created","target_id":"00000000-0000-0000-0000-000000000003"}"#,
    )
}

#[test]
fn review_orchestration_start_has_one_exact_v1_shape() -> Result<(), Box<dyn std::error::Error>> {
    let request_id = request(11)?;
    let frame = ClientFrame::try_new(
        request_id,
        ClientRequest::StartReviewOrchestration {
            command_id: command(2)?,
            attempt_id: uuid(3),
            target_id: uuid(4),
            concern_set_version: String::from("initial-five"),
            import_template_name: String::from("review.import"),
            judgment_template_name: String::from("review.judgment"),
            repair_template_name: String::from("review.repair"),
            publication_template_name: String::from("review.publication"),
            concerns: vec![ReviewOrchestrationConcernInput {
                key: String::from("correctness"),
                template_name: String::from("review.concern.correctness"),
            }],
        },
    )?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"11\",\"request\":{\"type\":\"start_review_orchestration\",\"command_id\":\"00000000-0000-0000-0000-000000000002\",\"attempt_id\":\"00000000-0000-0000-0000-000000000003\",\"target_id\":\"00000000-0000-0000-0000-000000000004\",\"concern_set_version\":\"initial-five\",\"import_template_name\":\"review.import\",\"judgment_template_name\":\"review.judgment\",\"repair_template_name\":\"review.repair\",\"publication_template_name\":\"review.publication\",\"concerns\":[{\"key\":\"correctness\",\"template_name\":\"review.concern.correctness\"}]}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    assert_server_message_round_trip(
        request_id,
        ServerMessage::ReviewOrchestrationStarted {
            attempt_id: uuid(3),
        },
        r#"{"type":"review_orchestration_started","attempt_id":"00000000-0000-0000-0000-000000000003"}"#,
    )
}

#[test]
fn review_finding_event_request_round_trips_under_its_generalized_name()
-> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new(
        request(12)?,
        ClientRequest::RecordReviewFindingEvent {
            command_id: command(2)?,
            run_id: uuid(3),
            pass_id: uuid(4),
            turn_id: uuid(5),
            output_frontier_id: Some(uuid(6)),
            finding_id: uuid(7),
            event_ordinal: CanonicalU64::new(2),
            event: ReviewFindingEvent::Duplicate {
                canonical_finding_id: uuid(8),
            },
        },
    )?;
    let encoded = encode_client_line(&frame)?;
    assert_eq!(
        String::from_utf8(encoded.clone())?,
        "{\"version\":1,\"request_id\":\"12\",\"request\":{\"type\":\"record_review_finding_event\",\"command_id\":\"00000000-0000-0000-0000-000000000002\",\"run_id\":\"00000000-0000-0000-0000-000000000003\",\"pass_id\":\"00000000-0000-0000-0000-000000000004\",\"turn_id\":\"00000000-0000-0000-0000-000000000005\",\"output_frontier_id\":\"00000000-0000-0000-0000-000000000006\",\"finding_id\":\"00000000-0000-0000-0000-000000000007\",\"event_ordinal\":\"2\",\"event\":{\"kind\":\"duplicate\",\"canonical_finding_id\":\"00000000-0000-0000-0000-000000000008\"}}}\n"
    );
    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn review_pass_success_round_trips_with_terminal_evidence() -> Result<(), Box<dyn std::error::Error>>
{
    let succeeded = ClientFrame::try_new(
        request(13)?,
        ClientRequest::CompleteReviewPass {
            command_id: command(2)?,
            run_id: uuid(3),
            pass_id: uuid(4),
            turn_id: Some(uuid(5)),
            output_frontier_id: Some(uuid(6)),
            outcome: ReviewPassTerminalOutcome::Succeeded,
        },
    )?;
    assert_eq!(
        decode_client_line(&encode_client_line(&succeeded)?)?,
        succeeded
    );
    Ok(())
}

#[test]
fn review_pass_completion_rejects_evidence_for_another_outcome() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"13","request":{"type":"complete_review_pass","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":null,"outcome":"succeeded"}}"#,
    );
    assert_client_malformed(
        r#"{"version":1,"request_id":"13","request":{"type":"complete_review_pass","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":"00000000-0000-0000-0000-000000000006","outcome":"failed"}}"#,
    );
}

#[test]
fn review_pass_completion_requires_the_nullable_turn_member() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"13","request":{"type":"complete_review_pass","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","output_frontier_id":null,"outcome":"cancelled"}}"#,
    );
}

#[test]
fn review_orchestration_stage_requests_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let digest = CanonicalDigest::try_new("cd".repeat(32))?;
    let import = ClientFrame::try_new(
        request(19)?,
        ClientRequest::RecordReviewImportOutcome {
            command_id: command(2)?,
            attempt_id: uuid(3),
            pass_id: Some(uuid(4)),
            external_link_id: None,
            context_digest: Some(digest),
            outcome: ReviewImportTerminalOutcome::Succeeded,
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&import)?)?, import);
    let concern = ClientFrame::try_new(
        request(20)?,
        ClientRequest::RecordReviewConcernOutcome {
            command_id: command(2)?,
            attempt_id: uuid(3),
            concern: String::from("correctness"),
            pass_id: Some(uuid(5)),
            outcome: ReviewConcernTerminalOutcome::Succeeded,
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&concern)?)?, concern);
    let plan = ClientFrame::try_new(
        request(21)?,
        ClientRequest::RecordReviewJudgmentPlan {
            command_id: command(2)?,
            attempt_id: uuid(3),
            analysis_pass_id: uuid(6),
            members: vec![ReviewJudgmentPlanMember {
                finding_id: uuid(7),
                disposition: ReviewJudgmentDisposition::Accepted {},
            }],
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&plan)?)?, plan);
    let effect = ClientFrame::try_new(
        request(22)?,
        ClientRequest::RecordReviewJudgmentEffect {
            command_id: command(2)?,
            attempt_id: uuid(3),
            finding_id: uuid(7),
            event_pass_id: Some(uuid(8)),
            outcome: ReviewJudgmentEffectTerminalOutcome::Applied,
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&effect)?)?, effect);
    let repairs = ClientFrame::try_new(
        request(23)?,
        ClientRequest::RecordReviewRepairOutcomes {
            command_id: command(2)?,
            attempt_id: uuid(3),
            outcomes: vec![ReviewRepairOutcome {
                finding_id: uuid(7),
                event_pass_id: Some(uuid(9)),
                outcome: ReviewRepairTerminalOutcome::Fixed,
            }],
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&repairs)?)?, repairs);
    let publications = ClientFrame::try_new(
        request(24)?,
        ClientRequest::RecordReviewPublicationOutcomes {
            command_id: command(2)?,
            attempt_id: uuid(3),
            outcomes: vec![ReviewPublicationOutcome {
                finding_id: uuid(7),
                external_link_id: Some(uuid(10)),
                outcome: ReviewPublicationTerminalOutcome::Published,
            }],
        },
    )?;
    assert_eq!(
        decode_client_line(&encode_client_line(&publications)?)?,
        publications
    );
    let read = ClientFrame::try_new(
        request(25)?,
        ClientRequest::ReadReviewOrchestration {
            attempt_id: uuid(3),
        },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&read)?)?, read);
    assert_server_message_round_trip(
        request(26)?,
        ServerMessage::ReviewOrchestrationAdvanced {
            attempt_id: uuid(3),
            state: ReviewOrchestrationState::AwaitingPublication,
        },
        r#"{"type":"review_orchestration_advanced","attempt_id":"00000000-0000-0000-0000-000000000003","state":"awaiting_publication"}"#,
    )
}

#[test]
fn review_import_success_requires_pass_and_context_evidence() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"14","request":{"type":"record_review_import_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","pass_id":null,"external_link_id":null,"context_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","outcome":"succeeded"}}"#,
    );
}

#[test]
fn review_concern_failure_requires_pass_evidence() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"14","request":{"type":"record_review_concern_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","concern":"correctness","pass_id":null,"outcome":"failed"}}"#,
    );
}

#[test]
fn review_incomplete_judgment_effect_rejects_pass_evidence() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"14","request":{"type":"record_review_judgment_effect","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","finding_id":"00000000-0000-0000-0000-000000000004","event_pass_id":"00000000-0000-0000-0000-000000000005","outcome":"blocked"}}"#,
    );
}

#[test]
fn review_fixed_repair_requires_event_pass_evidence() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"14","request":{"type":"record_review_repair_outcomes","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","outcomes":[{"finding_id":"00000000-0000-0000-0000-000000000004","event_pass_id":null,"outcome":"fixed"}]}}"#,
    );
}

#[test]
fn review_published_outcome_requires_external_link_evidence() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"14","request":{"type":"record_review_publication_outcomes","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","outcomes":[{"finding_id":"00000000-0000-0000-0000-000000000004","external_link_id":null,"outcome":"published"}]}}"#,
    );
}

#[test]
fn review_blocked_finding_event_round_trips_with_null_frontier()
-> Result<(), Box<dyn std::error::Error>> {
    let frame = ClientFrame::try_new(
        request(27)?,
        ClientRequest::RecordReviewFindingEvent {
            command_id: command(2)?,
            run_id: uuid(3),
            pass_id: uuid(4),
            turn_id: uuid(5),
            output_frontier_id: None,
            finding_id: uuid(7),
            event_ordinal: CanonicalU64::new(2),
            event: ReviewFindingEvent::BlockedWithReason {
                reason: String::from("requires reconciliation"),
                external_link_id: None,
            },
        },
    )?;
    let encoded = encode_client_line(&frame)?;

    assert_eq!(decode_client_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn review_finding_event_rejects_frontier_mismatched_to_event() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"27","request":{"type":"record_review_finding_event","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":"00000000-0000-0000-0000-000000000006","finding_id":"00000000-0000-0000-0000-000000000007","event_ordinal":"1","event":{"kind":"blocked_with_reason","reason":"requires reconciliation","external_link_id":null}}}"#,
    );
    assert_client_malformed(
        r#"{"version":1,"request_id":"27","request":{"type":"record_review_finding_event","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":null,"finding_id":"00000000-0000-0000-0000-000000000007","event_ordinal":"1","event":{"kind":"accepted"}}}"#,
    );
}

#[test]
fn review_finding_event_refuses_unknown_members() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"14","request":{"type":"record_review_finding_event","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":"00000000-0000-0000-0000-000000000006","finding_id":"00000000-0000-0000-0000-000000000007","event_ordinal":"1","event":{"kind":"accepted","future":true}}}"#,
    );
}

#[test]
fn review_concern_outcome_refuses_unknown_tokens() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"14","request":{"type":"record_review_concern_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","concern":"correctness","pass_id":null,"outcome":"future"}}"#,
    );
}

#[test]
fn review_import_refuses_noncanonical_digest() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"15","request":{"type":"record_review_import_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","external_link_id":null,"context_digest":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","outcome":"succeeded"}}"#,
    );
}

#[test]
fn review_orchestration_read_refuses_noncanonical_identity() {
    assert_client_malformed(
        r#"{"version":1,"request_id":"15","request":{"type":"read_review_orchestration","attempt_id":"00000000-0000-0000-0000-00000000000A"}}"#,
    );
}

#[test]
fn review_orchestration_refuses_oversized_concern_inventory()
-> Result<(), Box<dyn std::error::Error>> {
    let concern = ReviewOrchestrationConcernInput {
        key: String::from("correctness"),
        template_name: String::from("correctness"),
    };
    let oversized = ClientFrame::try_new(
        request(15)?,
        ClientRequest::StartReviewOrchestration {
            command_id: command(2)?,
            attempt_id: uuid(3),
            target_id: uuid(4),
            concern_set_version: String::from("initial"),
            import_template_name: String::from("import"),
            judgment_template_name: String::from("judgment"),
            repair_template_name: String::from("repair"),
            publication_template_name: String::from("publication"),
            concerns: vec![concern; 33],
        },
    );
    assert_eq!(oversized, Err(FrameValidationError::ReviewShape));
    Ok(())
}

#[test]
fn review_orchestration_refuses_oversized_judgment_inventory()
-> Result<(), Box<dyn std::error::Error>> {
    let oversized = ClientFrame::try_new(
        request(16)?,
        ClientRequest::RecordReviewJudgmentPlan {
            command_id: command(2)?,
            attempt_id: uuid(3),
            analysis_pass_id: uuid(4),
            members: vec![
                ReviewJudgmentPlanMember {
                    finding_id: uuid(5),
                    disposition: ReviewJudgmentDisposition::Accepted {},
                };
                1_025
            ],
        },
    );
    assert_eq!(oversized, Err(FrameValidationError::ReviewShape));
    Ok(())
}

#[test]
fn review_orchestration_snapshot_round_trips_with_frozen_inventory()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = CanonicalDigest::try_new("ab".repeat(32))?;
    let snapshot = ReviewOrchestrationSnapshot {
        attempt_id: uuid(3),
        target_id: uuid(4),
        state: ReviewOrchestrationState::AwaitingJudgment,
        concern_set_version: String::from("initial-five"),
        stage_template_digests: ReviewOrchestrationStageTemplateDigests {
            import: digest.clone(),
            judgment: digest.clone(),
            repair: digest.clone(),
            publication: digest.clone(),
        },
        concerns: vec![ReviewOrchestrationConcernSnapshot {
            key: String::from("correctness"),
            template_digest: digest,
            status: ReviewOrchestrationConcernStatus::Succeeded,
            pass_id: Some(uuid(5)),
        }],
        counts: ReviewOrchestrationCounts {
            finding_count: CanonicalU64::new(2),
            judgment_member_count: CanonicalU64::new(0),
            judgment_effect_applied_count: CanonicalU64::new(0),
            repair_fixed_count: CanonicalU64::new(0),
            publication_published_count: CanonicalU64::new(0),
        },
    };
    let frame = ServerFrame::try_new(
        request(17)?,
        ServerMessage::ReviewOrchestration { snapshot },
    )?;
    let encoded = encode_server_line(&frame)?;
    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn review_orchestration_snapshot_preserves_superseded_concern_status()
-> Result<(), Box<dyn std::error::Error>> {
    let snapshot = orchestration_snapshot_fixture(
        ReviewOrchestrationState::FanoutIncomplete,
        ReviewOrchestrationConcernStatus::Superseded,
        Some(uuid(5)),
        ReviewOrchestrationCounts {
            finding_count: CanonicalU64::new(0),
            judgment_member_count: CanonicalU64::new(0),
            judgment_effect_applied_count: CanonicalU64::new(0),
            repair_fixed_count: CanonicalU64::new(0),
            publication_published_count: CanonicalU64::new(0),
        },
    )?;
    let frame = ServerFrame::try_new(
        request(18)?,
        ServerMessage::ReviewOrchestration { snapshot },
    )?;
    let encoded = encode_server_line(&frame)?;

    assert_eq!(decode_server_line(&encoded)?, frame);
    Ok(())
}

#[test]
fn review_orchestration_snapshot_rejects_terminal_state_with_pending_concern()
-> Result<(), Box<dyn std::error::Error>> {
    let snapshot = orchestration_snapshot_fixture(
        ReviewOrchestrationState::Complete,
        ReviewOrchestrationConcernStatus::Pending,
        None,
        ReviewOrchestrationCounts {
            finding_count: CanonicalU64::new(0),
            judgment_member_count: CanonicalU64::new(0),
            judgment_effect_applied_count: CanonicalU64::new(0),
            repair_fixed_count: CanonicalU64::new(0),
            publication_published_count: CanonicalU64::new(0),
        },
    )?;

    let frame = ServerFrame::try_new(
        request(18)?,
        ServerMessage::ReviewOrchestration { snapshot },
    );

    assert_eq!(frame, Err(FrameValidationError::ReviewShape));
    Ok(())
}

#[test]
fn review_orchestration_snapshot_rejects_incomplete_state_with_complete_judgment()
-> Result<(), Box<dyn std::error::Error>> {
    let snapshot = orchestration_snapshot_fixture(
        ReviewOrchestrationState::AwaitingJudgmentEffects,
        ReviewOrchestrationConcernStatus::Succeeded,
        Some(uuid(5)),
        ReviewOrchestrationCounts {
            finding_count: CanonicalU64::new(1),
            judgment_member_count: CanonicalU64::new(1),
            judgment_effect_applied_count: CanonicalU64::new(1),
            repair_fixed_count: CanonicalU64::new(0),
            publication_published_count: CanonicalU64::new(0),
        },
    )?;

    let frame = ServerFrame::try_new(
        request(19)?,
        ServerMessage::ReviewOrchestration { snapshot },
    );

    assert_eq!(frame, Err(FrameValidationError::ReviewShape));
    Ok(())
}

#[test]
fn review_orchestration_snapshot_rejects_overlapping_terminal_counts()
-> Result<(), Box<dyn std::error::Error>> {
    let snapshot = orchestration_snapshot_fixture(
        ReviewOrchestrationState::Complete,
        ReviewOrchestrationConcernStatus::Succeeded,
        Some(uuid(5)),
        ReviewOrchestrationCounts {
            finding_count: CanonicalU64::new(1),
            judgment_member_count: CanonicalU64::new(1),
            judgment_effect_applied_count: CanonicalU64::new(1),
            repair_fixed_count: CanonicalU64::new(1),
            publication_published_count: CanonicalU64::new(1),
        },
    )?;

    let frame = ServerFrame::try_new(
        request(20)?,
        ServerMessage::ReviewOrchestration { snapshot },
    );

    assert_eq!(frame, Err(FrameValidationError::ReviewShape));
    Ok(())
}

#[test]
fn review_pass_completed_receipt_round_trips_terminal_state()
-> Result<(), Box<dyn std::error::Error>> {
    let terminal = ServerFrame::try_new(
        request(18)?,
        ServerMessage::ReviewPassCompleted {
            run_id: uuid(6),
            pass_id: uuid(7),
            state: ReviewPassLifecycle::Blocked,
        },
    )?;
    assert_eq!(
        decode_server_line(&encode_server_line(&terminal)?)?,
        terminal
    );
    Ok(())
}
