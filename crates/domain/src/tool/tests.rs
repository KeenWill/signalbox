//! Tool tests for `docs/spec/tool-loop.md`.

use super::approval::SUPPRESSED_TOOL_DENIAL_REASON;
use super::proposal::SUPPRESSED_TOOL_ARGUMENTS;
use crate::DurableCommandId;

use super::*;
use crate::{
    DirectModelSelection,
    test_support::{command_id, model_call_id, session_id, tool_request_id, turn_id},
};

fn request(id: u128) -> ToolRequest {
    ToolRequestReconstitutionInput::new(
        tool_request_id(id),
        session_id(1),
        turn_id(2),
        model_call_id(3),
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("current_time")).expect("canonical tool name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("canonical arguments are valid"),
    )
    .into_request()
}

fn tool_response_parts(count: usize) -> Vec<AssistantResponsePart> {
    (0..count)
        .map(|_| {
            AssistantResponsePart::ToolCall(ToolCallProposal::new(
                ToolName::try_new(String::from("current_time"))
                    .expect("canonical tool name is valid"),
                NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                    .expect("canonical arguments are valid"),
            ))
        })
        .collect()
}

/// request names are exact and restricted to the recorded ASCII spelling.
#[test]
fn tool_name_rejects_empty_long_and_unsafe_spelling() {
    assert_eq!(
        ToolName::try_new(String::new())
            .expect_err("empty names are invalid")
            .failure(),
        ToolNameFailure::Empty
    );
    assert_eq!(
        ToolName::try_new("x".repeat(65))
            .expect_err("overlong names are invalid")
            .failure(),
        ToolNameFailure::TooLong { bytes: 65 }
    );
    assert_eq!(
        ToolName::try_new(String::from("current/time"))
            .expect_err("slash is outside the spelling")
            .failure(),
        ToolNameFailure::InvalidCharacter {
            byte_index: 7,
            character: '/',
        }
    );
}

/// valid JSON is canonicalized recursively, while malformed provider text remains exact bounded
/// evidence.
#[test]
fn arguments_are_canonical_or_exactly_undecodable() {
    let json = NormalizedToolArguments::try_from_provider_text(String::from(
        r#"{ "z": [{"b": 2, "a": 1}], "a": true }"#,
    ))
    .expect("bounded JSON is valid");
    let malformed_text = String::from("{\"timezone\":");
    let malformed = NormalizedToolArguments::try_from_provider_text(malformed_text.clone())
        .expect("bounded malformed text remains evidence");

    assert_eq!(json.kind(), ToolArgumentsKind::Json);
    assert_eq!(json.as_str(), r#"{"a":true,"z":[{"a":1,"b":2}]}"#);
    assert_eq!(malformed.kind(), ToolArgumentsKind::Undecodable);
    assert_eq!(malformed.as_str(), malformed_text);
}

/// a complete JSON prefix followed by any non-whitespace provider text remains exact undecodable
/// evidence.
#[test]
fn arguments_reject_trailing_non_whitespace() {
    let provider_text = String::from(r#"{"timezone":"UTC"} trailing"#);
    let normalized = NormalizedToolArguments::try_from_provider_text(provider_text.clone())
        .expect("bounded non-JSON text remains admissible evidence");

    assert_eq!(normalized.kind(), ToolArgumentsKind::Undecodable);
    assert_eq!(normalized.as_str(), provider_text);
}

/// literal U+0000 cannot enter the durable text vocabulary even when the remaining provider text is
/// undecodable JSON.
#[test]
fn arguments_reject_literal_null() {
    let value = String::from("{\"timezone\":\0");
    let error = NormalizedToolArguments::try_from_provider_text(value.clone())
        .expect_err("PostgreSQL text cannot preserve a literal null");

    assert_eq!(error.value(), value);
    assert_eq!(error.failure(), ToolArgumentsFailure::ContainsNull);
}

/// reconstitution rejects a competing noncanonical JSON representation.
#[test]
fn stored_json_must_be_canonical() {
    let error = NormalizedToolArguments::try_from_stored(
        ToolArgumentsKind::Json,
        String::from(r#"{ "b": 2, "a": 1 }"#),
    )
    .expect_err("stored JSON must already be canonical");

    assert_eq!(
        error.failure(),
        ToolArgumentsFailure::StoredJsonNotCanonical
    );
}

/// canonicalization preserves JSON numeric values outside the native integer and floating-point
/// ranges without rounding.
#[test]
fn arguments_preserve_arbitrary_precision_numbers() {
    let normalized = NormalizedToolArguments::try_from_provider_text(String::from(
        r#"{"wide":18446744073709551617,"exponent":1e400}"#,
    ))
    .expect("valid JSON numbers remain decodable");

    assert_eq!(normalized.kind(), ToolArgumentsKind::Json);
    assert_eq!(
        normalized.as_str(),
        r#"{"exponent":1e+400,"wide":18446744073709551617}"#
    );
}

#[test]
fn arguments_preserve_reserved_number_key_objects() {
    let cases = [
        (
            r#"{"$serde_json::private::Number":"1"}"#,
            r#"{"$serde_json::private::Number":"1"}"#,
        ),
        (
            r#"{"z":[{"\u0024serde_json::private::Number":"1","tail":true}]," x":"$serde_json::private::Number"}"#,
            r#"{" x":"$serde_json::private::Number","z":[{"$serde_json::private::Number":"1","tail":true}]}"#,
        ),
        (
            r#"{"quote\"key":"value\\","$serde_json::private::Number":"1"}"#,
            r#"{"$serde_json::private::Number":"1","quote\"key":"value\\"}"#,
        ),
    ];
    for (input, expected) in cases {
        let normalized = NormalizedToolArguments::try_from_provider_text(input.to_owned())
            .expect("ordinary object members remain JSON");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Json, "{input}");
        assert_eq!(normalized.as_str(), expected, "{input}");
        assert_eq!(
            NormalizedToolArguments::try_from_stored(ToolArgumentsKind::Json, expected.to_owned())
                .expect("canonical stored object reconstitutes"),
            normalized,
            "{input}"
        );
    }
}

/// Syntactically valid nested JSON is independent of serde's default recursion cutoff.
#[test]
fn deeply_nested_arguments_remain_json() {
    let depth = 512;
    let value = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
    let normalized = NormalizedToolArguments::try_from_provider_text(value.clone())
        .expect("deep bounded JSON remains admissible");

    assert_eq!(normalized.kind(), ToolArgumentsKind::Json);
    assert_eq!(normalized.as_str(), value);
}

/// malformed input is classified before any recursively owned JSON tree exists, even after a deeply
/// nested complete child.
#[test]
fn deep_partial_json_is_dropped_stack_safely() {
    let depth = 100_000;
    let value = format!("[{}null{},!]", "[".repeat(depth), "]".repeat(depth));
    let normalized = NormalizedToolArguments::try_from_provider_text(value.clone())
        .expect("bounded malformed text remains exact evidence");

    assert_eq!(normalized.kind(), ToolArgumentsKind::Undecodable);
    assert_eq!(normalized.as_str(), value);
}

/// delegated approval narrows authority and can never approve or deny a
/// request frozen as human-only.
#[test]
fn delegate_narrows_and_never_widens_human_authority() {
    const HUMAN_ONLY_REQUEST_SEED: u128 = 40;
    const JUDGE_MODEL_SEED: u128 = 41;
    const APPROVAL_CALL_SEED: u128 = 42;
    const DENIAL_CALL_SEED: u128 = 43;
    const ESCALATION_CALL_SEED: u128 = 44;
    const HUMAN_AUTHORITY_RATIONALE: &str = "needs user authority";

    let request = request(HUMAN_ONLY_REQUEST_SEED);
    let model = DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED));
    let rationale = ToolDecisionRationale::try_new(String::from(HUMAN_AUTHORITY_RATIONALE))
        .expect("fixture rationale is admitted");
    let rejected = DelegateToolApproval::try_new(
        &request,
        model,
        model_call_id(APPROVAL_CALL_SEED),
        DelegateApprovalRecommendation::Approve,
        rationale.clone(),
    )
    .expect_err("a delegate cannot approve a human-only request");
    let rejected_denial = DelegateToolApproval::try_new(
        &request,
        model,
        model_call_id(DENIAL_CALL_SEED),
        DelegateApprovalRecommendation::Deny,
        rationale.clone(),
    )
    .expect_err("a delegate cannot deny a human-only request");
    let escalated = DelegateToolApproval::try_new(
        &request,
        model,
        model_call_id(ESCALATION_CALL_SEED),
        DelegateApprovalRecommendation::EscalateToHuman,
        rationale,
    )
    .expect("escalation preserves human authority");

    assert_eq!(rejected.posture(), ToolApprovalPosture::Human);
    assert_eq!(
        rejected.recommendation(),
        DelegateApprovalRecommendation::Approve
    );
    assert_eq!(rejected_denial.posture(), ToolApprovalPosture::Human);
    assert_eq!(
        rejected_denial.recommendation(),
        DelegateApprovalRecommendation::Deny
    );
    assert_eq!(
        escalated.recommendation(),
        DelegateApprovalRecommendation::EscalateToHuman
    );
}

#[test]
fn delegate_resolution_preserves_model_call_and_rationale() {
    const SUBJECT_REQUEST_SEED: u128 = 50;
    const SUBJECT_SESSION_SEED: u128 = 1;
    const SUBJECT_TURN_SEED: u128 = 2;
    const ISSUING_CALL_SEED: u128 = 3;
    const SUBJECT_ORDINAL: u32 = 0;
    const SUBJECT_TOOL_NAME: &str = "current_time";
    const SUBJECT_ARGUMENTS: &str = "{}";
    const JUDGE_MODEL_SEED: u128 = 51;
    const JUDGE_CALL_SEED: u128 = 52;
    const JUDGE_RATIONALE: &str = "bounded request";

    let request = ToolRequestReconstitutionInput::new(
        tool_request_id(SUBJECT_REQUEST_SEED),
        session_id(SUBJECT_SESSION_SEED),
        turn_id(SUBJECT_TURN_SEED),
        model_call_id(ISSUING_CALL_SEED),
        ToolRequestOrdinal::from_u32(SUBJECT_ORDINAL),
        ToolName::try_new(String::from(SUBJECT_TOOL_NAME)).expect("fixture name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from(SUBJECT_ARGUMENTS))
            .expect("fixture arguments are valid"),
    )
    .with_approval_posture(ToolApprovalPosture::Delegated)
    .into_request();
    let model = DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED));
    let call = model_call_id(JUDGE_CALL_SEED);
    let rationale = ToolDecisionRationale::try_new(String::from(JUDGE_RATIONALE))
        .expect("fixture rationale is admitted");
    let approval = DelegateToolApproval::try_new(
        &request,
        model,
        call,
        DelegateApprovalRecommendation::Deny,
        rationale.clone(),
    )
    .expect("delegated authority may deny");
    let stored_reason = ToolDenialReason::try_new(String::from(JUDGE_RATIONALE))
        .expect("fixture rationale is an admitted reason");
    let resolution =
        ToolApprovalResolutionReconstitutionInput::delegate(approval, Some(stored_reason.clone()))
            .reconstitute()
            .expect("checked delegate evidence restores its decision");

    assert_eq!(
        resolution.decider(),
        Some(&ToolApprovalDecider::Delegate { model, call })
    );
    assert_eq!(resolution.rationale(), Some(&rationale));
    assert_eq!(
        resolution.decision(),
        &ToolApprovalDecision::Deny {
            reason: Some(stored_reason)
        }
    );
}

/// One delegate denial whose recorded rationale is "scope exceeded".
fn denied_delegate_fixture() -> DelegateToolApproval {
    const SUBJECT_REQUEST_SEED: u128 = 60;
    const SUBJECT_SESSION_SEED: u128 = 1;
    const SUBJECT_TURN_SEED: u128 = 2;
    const ISSUING_CALL_SEED: u128 = 3;
    const SUBJECT_TOOL_NAME: &str = "current_time";
    const SUBJECT_ARGUMENTS: &str = "{}";
    const JUDGE_MODEL_SEED: u128 = 61;
    const JUDGE_CALL_SEED: u128 = 62;
    const JUDGE_RATIONALE: &str = "scope exceeded";

    let request = ToolRequestReconstitutionInput::new(
        tool_request_id(SUBJECT_REQUEST_SEED),
        session_id(SUBJECT_SESSION_SEED),
        turn_id(SUBJECT_TURN_SEED),
        model_call_id(ISSUING_CALL_SEED),
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from(SUBJECT_TOOL_NAME)).expect("fixture name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from(SUBJECT_ARGUMENTS))
            .expect("fixture arguments are valid"),
    )
    .with_approval_posture(ToolApprovalPosture::Delegated)
    .into_request();
    DelegateToolApproval::try_new(
        &request,
        DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED)),
        model_call_id(JUDGE_CALL_SEED),
        DelegateApprovalRecommendation::Deny,
        ToolDecisionRationale::try_new(String::from(JUDGE_RATIONALE))
            .expect("fixture rationale is admitted"),
    )
    .expect("delegated authority may deny")
}

/// A stored null reason is admitted exactly when the rationale derives
/// nothing; a null beside a deriving rationale is missing evidence and
/// fails closed.
#[test]
fn delegate_reconstitution_admits_null_only_for_empty_derivation() {
    let denial = denied_delegate_fixture();
    let missing_evidence = ToolApprovalResolutionReconstitutionInput::delegate(denial, None)
        .reconstitute()
        .expect_err("a null reason beside a deriving rationale is rejected");
    drop(missing_evidence);
}

/// A stored delegate denial reason the recorded rationale cannot derive
/// is corruption, not a decision to restore.
#[test]
fn delegate_reconstitution_rejects_an_unrelated_stored_reason() {
    let mismatched = ToolApprovalResolutionReconstitutionInput::delegate(
        denied_delegate_fixture(),
        Some(
            ToolDenialReason::try_new(String::from("unrelated stored text"))
                .expect("fixture reason is admitted"),
        ),
    )
    .reconstitute()
    .expect_err("a stored reason the rationale cannot derive is rejected");
    drop(mismatched);
}

fn admitted_rationale(value: &str) -> ToolDecisionRationale {
    ToolDecisionRationale::try_new(String::from(value)).expect("fixture rationale is admitted")
}

/// A rationale already inside the reason bounds derives verbatim.
#[test]
fn denial_reason_derivation_preserves_admissible_text_verbatim() {
    assert_eq!(
        ToolDenialReason::from_rationale(&admitted_rationale("scope exceeded"))
            .map(ToolDenialReason::into_string),
        Some(String::from("scope exceeded"))
    );
}

/// Internal line feeds are retained while forbidden controls become spaces and edge spaces trim.
#[test]
fn denial_reason_derivation_maps_control_characters_and_trims_edges() {
    assert_eq!(
        ToolDenialReason::from_rationale(&admitted_rationale("  first\nsecond\tthird  "))
            .map(ToolDenialReason::into_string),
        Some(String::from("first\nsecond third"))
    );
}

/// A rationale of only control characters and spaces derives nothing.
#[test]
fn denial_reason_derivation_of_whitespace_only_text_is_empty() {
    assert_eq!(
        ToolDenialReason::from_rationale(&admitted_rationale(" \n \t ")),
        None
    );
}

/// Admitted non-POSIX edge whitespace such as NBSP is preserved.
#[test]
fn denial_reason_derivation_preserves_admitted_edge_whitespace() {
    assert_eq!(
        ToolDenialReason::from_rationale(&admitted_rationale("\u{00a0}denied\u{00a0}"))
            .map(ToolDenialReason::into_string),
        Some(String::from("\u{00a0}denied\u{00a0}"))
    );
}

/// A maximum-sized rationale derives without truncation.
#[test]
fn denial_reason_derivation_preserves_the_rationale_bound() {
    let prefix = "a".repeat(ToolDecisionRationale::MAX_UTF8_BYTES - 2);
    let at_bound = ToolDecisionRationale::try_new(format!("{prefix}é"))
        .expect("fixture rationale is admitted");
    let derived =
        ToolDenialReason::from_rationale(&at_bound).expect("nonempty text derives a reason");
    assert_eq!(derived.as_str(), at_bound.as_str());
}

/// Every derived reason re-admits through the reason validator.
#[test]
fn denial_reason_derivation_output_is_always_admissible() {
    let derived = ToolDenialReason::from_rationale(&admitted_rationale("  first\nsecond\tthird  "))
        .expect("nonempty text derives a reason");
    assert!(ToolDenialReason::try_new(derived.into_string()).is_ok());
}

/// a restored session-blanket approval requires the approve-all posture frozen for that turn.
#[test]
fn session_blanket_reconstitution_requires_frozen_authority() {
    let request = tool_request_id(4);
    let restored = ToolApprovalResolutionReconstitutionInput::session_blanket(
        request,
        DangerousToolAutoApproval::ApproveAll,
    )
    .reconstitute()
    .expect("the exact frozen approve-all posture restores blanket authority");
    let rejected = ToolApprovalResolutionReconstitutionInput::session_blanket(
        request,
        DangerousToolAutoApproval::Disabled,
    )
    .reconstitute()
    .expect_err("a disabled frozen posture cannot restore blanket authority");

    assert_eq!(restored.request(), request);
    assert_eq!(restored.source(), ToolDecisionSource::SessionBlanket);
    assert_eq!(
        rejected.input(),
        &ToolApprovalResolutionReconstitutionInput::session_blanket(
            request,
            DangerousToolAutoApproval::Disabled,
        )
    );
}

/// credential-boundary suppression constructs an inert proposal and restores only the fixed
/// automatic denial provenance.
#[test]
fn runtime_safety_denial_is_non_executable() {
    let request = tool_request_id(4);
    let proposal = ToolCallProposal::suppressed(
        ToolName::try_new(String::from("sandboxed_exec")).expect("fixture tool name is valid"),
    );
    let restored = ToolApprovalResolutionReconstitutionInput::runtime_safety(request)
        .reconstitute()
        .expect("runtime safety evidence is self-authenticating");

    assert!(proposal.is_suppressed());
    assert_eq!(proposal.arguments().as_str(), SUPPRESSED_TOOL_ARGUMENTS);
    assert_eq!(restored.request(), request);
    assert_eq!(restored.source(), ToolDecisionSource::RuntimeSafety);
    assert_eq!(
        restored.decision(),
        &ToolApprovalDecision::Deny {
            reason: Some(
                ToolDenialReason::try_new(String::from(SUPPRESSED_TOOL_DENIAL_REASON))
                    .expect("fixed denial reason is valid"),
            ),
        }
    );
    assert!(!restored.is_approved());
}

/// only the user-command preparation path can construct user-sourced approval.
#[test]
fn user_command_preparation_preserves_agency() {
    let request = request(4);
    let command =
        DecideToolRequest::new(command_id(5), request.id(), ToolApprovalDecision::Approve);
    let prepared = command
        .prepare_applied(&request)
        .expect("the exact pending request is correlated");
    let DecideToolRequestResult::Applied(applied) = prepared.result() else {
        panic!("the exact request should produce an applied candidate");
    };

    assert_eq!(applied.resolution().request(), request.id());
    assert_eq!(
        applied.resolution().source(),
        ToolDecisionSource::UserCommand
    );
    assert_eq!(
        applied.resolution().decider(),
        Some(&ToolApprovalDecider::User {
            command: prepared.command().command_id(),
        })
    );
    assert_eq!(applied.resolution().rationale(), None);
    assert!(applied.resolution().is_approved());
}

#[test]
fn tool_response_preserves_large_tool_batches() {
    let response = ToolUsingAssistantResponse::try_from_parts(tool_response_parts(40))
        .expect("the response retains admitted and rejected requests together");
    assert_eq!(response.tool_count(), 40);
    assert_eq!(response.parts().len(), 40);
}

/// user-global command sentinels never enter the canonical
/// tool-decision command space.
#[test]
fn tool_decision_rejects_reserved_command_identities() {
    let nil_command_id = DurableCommandId::from_uuid(uuid::Uuid::nil());
    let nil_error = DecideToolRequest::try_new(
        nil_command_id,
        tool_request_id(1),
        ToolApprovalDecision::Approve,
    )
    .expect_err("the nil command identity is rejected");
    assert_eq!(nil_error.command_id(), nil_command_id);

    let max_command_id = DurableCommandId::from_uuid(uuid::Uuid::max());
    let max_error = DecideToolRequest::try_new(
        max_command_id,
        tool_request_id(1),
        ToolApprovalDecision::Approve,
    )
    .expect_err("the max command identity is rejected");
    assert_eq!(max_error.command_id(), max_command_id);
}

/// only an applied user command can restore user-command approval authority.
#[test]
fn rejected_user_command_cannot_restore_approval() {
    let command = DecideToolRequest::new(
        command_id(5),
        tool_request_id(4),
        ToolApprovalDecision::Approve,
    )
    .prepare_request_not_found();
    let input = ToolApprovalResolutionReconstitutionInput::user_command(command);

    assert!(
        input
            .clone()
            .reconstitute()
            .expect_err("a rejected command carries no approval authority")
            .input()
            == &input
    );
}

/// denial admission follows the persisted POSIX-whitespace contract without silently broadening it
/// to every Unicode space scalar.
#[test]
fn denial_reason_rejects_posix_edges_and_preserves_nonbreaking_space() {
    for value in [" denied", "denied\n", "\tdenied", "denied\u{000c}"] {
        assert_eq!(
            ToolDenialReason::try_new(String::from(value))
                .expect_err("POSIX edge whitespace is rejected")
                .failure(),
            ToolDenialReasonFailure::SurroundingWhitespace
        );
    }

    let admitted = ToolDenialReason::try_new(String::from("\u{00a0}denied\u{00a0}"))
        .expect("nonbreaking space is not POSIX whitespace");
    assert_eq!(admitted.as_str(), "\u{00a0}denied\u{00a0}");
}

/// the admission bound is inclusive, so a result of exactly the bounded size is admitted exactly.
#[test]
fn result_text_admits_exactly_the_bounded_size() {
    let at_bound = "r".repeat(ToolResultText::MAX_UTF8_BYTES);

    let admitted = ToolResultText::try_new(at_bound.clone())
        .expect("the bound itself is an admissible result size");
    assert_eq!(admitted.as_str(), at_bound);
}

/// one byte past the bound is refused, and the refusal reports the observed size while retaining
/// the rejected text without rewriting it.
#[test]
fn result_text_rejects_one_byte_past_the_bound() {
    let past_bound = "r".repeat(ToolResultText::MAX_UTF8_BYTES + 1);

    let error = ToolResultText::try_new(past_bound.clone())
        .expect_err("one byte past the bound is not an admissible result");

    assert_eq!(
        error.failure(),
        ToolResultTextFailure::TooLarge {
            bytes: past_bound.len(),
        }
    );
    assert_eq!(error.value(), past_bound);
}

/// literal U+0000 cannot enter the durable result vocabulary, and the refusal retains the rejected
/// text without rewriting it.
#[test]
fn result_text_rejects_a_literal_null() {
    let value = String::from("head\0tail");

    let error = ToolResultText::try_new(value.clone())
        .expect_err("PostgreSQL text cannot preserve a literal null");

    assert_eq!(error.failure(), ToolResultTextFailure::ContainsNull);
    assert_eq!(error.value(), value);
}

/// durable-command comparison equality excludes only command
/// identity and retains the exact decision payload.
#[test]
fn decision_command_equality_excludes_only_command_identity() {
    let request = tool_request_id(1);
    let approve = DecideToolRequest::new(command_id(2), request, ToolApprovalDecision::Approve);
    let replay = DecideToolRequest::new(command_id(3), request, ToolApprovalDecision::Approve);
    let deny = DecideToolRequest::new(
        command_id(2),
        request,
        ToolApprovalDecision::Deny { reason: None },
    );

    assert_eq!(approve, replay);
    assert_ne!(approve, deny);
}

/// denials remain request-bound logical resolutions and cannot name a physical attempt.
#[test]
fn denial_resolution_names_only_the_request() {
    let request = tool_request_id(9);

    assert_eq!(
        ToolRequestResolution::Denied { request },
        ToolRequestResolution::Denied { request }
    );
    assert_ne!(
        ToolRequestResolution::Denied { request },
        ToolRequestResolution::ClosedByTurnEnd { request }
    );
}

/// The request-identity seed of the canonical delegate-denied fixture; the
/// judge model and call seeds derive from it, decorrelated per testing
/// rule 4.
const DENIED_REQUEST_SEED: u128 = 70;
/// The seed of the fixture override command overriding that denial.
const OVERRIDE_COMMAND_SEED: u128 = 71;

/// One request frozen `Delegated` in the canonical fixture session.
fn delegated_request(seed: u128) -> ToolRequest {
    ToolRequestReconstitutionInput::new(
        tool_request_id(seed),
        session_id(1),
        turn_id(2),
        model_call_id(3),
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(String::from("current_time")).expect("fixture name is valid"),
        NormalizedToolArguments::try_from_provider_text(String::from("{}"))
            .expect("fixture arguments are valid"),
    )
    .with_approval_posture(ToolApprovalPosture::Delegated)
    .into_request()
}

/// The judge model and call seeds of the canonical delegate denial;
/// arbitrary — they only need to exist as one recorded judge.
const DENYING_JUDGE_MODEL_SEED: u128 = 200;
const DENYING_JUDGE_CALL_SEED: u128 = 201;

/// The delegate denial recorded against one delegated request by the
/// canonical fixture judge.
fn delegate_denial(request: &ToolRequest) -> ToolApprovalResolution {
    let denial = DelegateToolApproval::try_new(
        request,
        DirectModelSelection::from_uuid(uuid::Uuid::from_u128(DENYING_JUDGE_MODEL_SEED)),
        model_call_id(DENYING_JUDGE_CALL_SEED),
        DelegateApprovalRecommendation::Deny,
        ToolDecisionRationale::try_new(String::from("scope exceeded"))
            .expect("fixture rationale is admitted"),
    )
    .expect("delegated authority may deny");
    ToolApprovalResolution::delegate(&denial)
        .expect("a delegate denial resolves the delegated request")
}

/// The canonical override command naming the fixture denial in its own
/// session.
fn override_command() -> OverrideDeniedToolRequest {
    OverrideDeniedToolRequest::try_new(
        command_id(OVERRIDE_COMMAND_SEED),
        session_id(1),
        tool_request_id(DENIED_REQUEST_SEED),
    )
    .expect("the fixture command identity is admitted")
}

/// The override verification predicate records exactly the denied command:
/// every conjunct holds, and the recorded override links the command, the
/// session, the denied request, and the denying judge call.
#[test]
fn override_prepare_records_the_exact_denied_command() {
    let request = delegated_request(DENIED_REQUEST_SEED);
    let denial = delegate_denial(&request);
    let prepared = override_command()
        .prepare(
            &request,
            Some(&denial),
            Some(ToolRequestResolution::Denied {
                request: request.id(),
            }),
            None,
        )
        .expect("correlated evidence prepares a terminal result");

    let OverrideDeniedToolRequestResult::Applied(applied) = prepared.result() else {
        panic!("a terminal delegate denial admits the override");
    };
    let recorded = applied.recorded();
    let Some(ToolApprovalDecider::Delegate {
        call: denying_call, ..
    }) = denial.decider()
    else {
        panic!("the fixture denial carries delegate provenance");
    };
    assert_eq!(recorded.command(), prepared.command().command_id());
    assert_eq!(recorded.session(), request.session());
    assert_eq!(recorded.denied_request(), request.id());
    assert_eq!(recorded.judge_call(), *denying_call);
    assert_eq!(recorded.tool(), request.name());
    assert_eq!(recorded.arguments(), request.arguments());
}

/// An recorded override matches only the exact denied command: equal tool
/// name and equal normalized arguments.
#[test]
fn recorded_override_matches_only_the_exact_denied_command() {
    let request = delegated_request(DENIED_REQUEST_SEED);
    let denial = delegate_denial(&request);
    let prepared = override_command()
        .prepare(
            &request,
            Some(&denial),
            Some(ToolRequestResolution::Denied {
                request: request.id(),
            }),
            None,
        )
        .expect("correlated evidence prepares a terminal result");
    let OverrideDeniedToolRequestResult::Applied(applied) = prepared.result() else {
        panic!("a terminal delegate denial admits the override");
    };
    let recorded = applied.recorded();

    let same_command = ToolCallProposal::new(request.name().clone(), request.arguments().clone());
    let other_arguments = ToolCallProposal::new(
        request.name().clone(),
        NormalizedToolArguments::try_from_provider_text(String::from(r#"{"timezone":"UTC"}"#))
            .expect("fixture arguments are valid"),
    );
    let other_tool = ToolCallProposal::new(
        ToolName::try_new(String::from("another_tool")).expect("fixture name is valid"),
        request.arguments().clone(),
    );
    assert!(recorded.matches_proposal(&same_command));
    assert!(!recorded.matches_proposal(&other_arguments));
    assert!(!recorded.matches_proposal(&other_tool));
}

/// Predicate conjunct: the request must belong to the command's session.
#[test]
fn override_prepare_rejects_another_sessions_request() {
    const OTHER_SESSION_SEED: u128 = 9;

    let request = delegated_request(DENIED_REQUEST_SEED);
    let denial = delegate_denial(&request);
    let command = OverrideDeniedToolRequest::try_new(
        command_id(OVERRIDE_COMMAND_SEED),
        session_id(OTHER_SESSION_SEED),
        request.id(),
    )
    .expect("the fixture command identity is admitted");
    let prepared = command
        .prepare(
            &request,
            Some(&denial),
            Some(ToolRequestResolution::Denied {
                request: request.id(),
            }),
            None,
        )
        .expect("correlated evidence prepares a terminal result");

    assert_eq!(
        prepared.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::RequestNotInSession {
                session: session_id(OTHER_SESSION_SEED),
                denied_request: request.id(),
            }
        )
    );
}

/// Predicate conjunct: an undecided request has no delegate denial to
/// override.
#[test]
fn override_prepare_rejects_an_undecided_request() {
    let request = delegated_request(DENIED_REQUEST_SEED);
    let prepared = override_command()
        .prepare(&request, None, None, None)
        .expect("correlated evidence prepares a terminal result");

    assert_eq!(
        prepared.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::NotDelegateDenied {
                denied_request: request.id(),
            }
        )
    );
}

/// Predicate conjunct: a user denial is not a judge denial; the override
/// can never reverse the user's own decision.
#[test]
fn override_prepare_rejects_a_user_denial() {
    const USER_DENIAL_COMMAND_SEED: u128 = 8;

    let request = delegated_request(DENIED_REQUEST_SEED);
    let user_denial = ToolApprovalResolution::user(
        command_id(USER_DENIAL_COMMAND_SEED),
        request.id(),
        ToolApprovalDecision::Deny { reason: None },
    );
    let prepared = override_command()
        .prepare(
            &request,
            Some(&user_denial),
            Some(ToolRequestResolution::Denied {
                request: request.id(),
            }),
            None,
        )
        .expect("correlated evidence prepares a terminal result");

    assert_eq!(
        prepared.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::NotDelegateDenied {
                denied_request: request.id(),
            }
        )
    );
}

/// Predicate conjunct: a delegate approval is not a denial; there is
/// nothing to override.
#[test]
fn override_prepare_rejects_a_delegate_approval() {
    const APPROVING_JUDGE_CALL_SEED: u128 = 12;

    let request = delegated_request(DENIED_REQUEST_SEED);
    let approval = DelegateToolApproval::try_new(
        &request,
        DirectModelSelection::from_uuid(uuid::Uuid::from_u128(DENYING_JUDGE_MODEL_SEED)),
        model_call_id(APPROVING_JUDGE_CALL_SEED),
        DelegateApprovalRecommendation::Approve,
        ToolDecisionRationale::try_new(String::from("bounded request"))
            .expect("fixture rationale is admitted"),
    )
    .expect("delegated authority may approve");
    let approval = ToolApprovalResolution::delegate(&approval)
        .expect("a delegate approval resolves the delegated request");
    let prepared = override_command()
        .prepare(
            &request,
            Some(&approval),
            Some(ToolRequestResolution::Denied {
                request: request.id(),
            }),
            None,
        )
        .expect("correlated evidence prepares a terminal result");

    assert_eq!(
        prepared.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::NotDelegateDenied {
                denied_request: request.id(),
            }
        )
    );
}

/// Predicate conjunct: a delegate denial whose denied result is not yet
/// materialized is still resolving and cannot be overridden.
#[test]
fn override_prepare_rejects_a_denial_still_resolving() {
    let request = delegated_request(DENIED_REQUEST_SEED);
    let denial = delegate_denial(&request);
    let prepared = override_command()
        .prepare(&request, Some(&denial), None, None)
        .expect("correlated evidence prepares a terminal result");

    assert_eq!(
        prepared.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied {
                denied_request: request.id(),
            }
        )
    );
}

/// Predicate conjunct: the terminal resolution must be this exact
/// request's denial, so mismatched terminal evidence fails closed.
#[test]
fn override_prepare_rejects_a_foreign_terminal_denial() {
    const FOREIGN_REQUEST_SEED: u128 = 6;

    let request = delegated_request(DENIED_REQUEST_SEED);
    let denial = delegate_denial(&request);
    let prepared = override_command()
        .prepare(
            &request,
            Some(&denial),
            Some(ToolRequestResolution::Denied {
                request: tool_request_id(FOREIGN_REQUEST_SEED),
            }),
            None,
        )
        .expect("correlated evidence prepares a terminal result");

    assert_eq!(
        prepared.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied {
                denied_request: request.id(),
            }
        )
    );
}

/// Predicate conjunct: each denial admits at most one override ever.
#[test]
fn override_prepare_rejects_an_already_overridden_denial() {
    const EARLIER_OVERRIDE_COMMAND_SEED: u128 = 7;

    let request = delegated_request(DENIED_REQUEST_SEED);
    let denial = delegate_denial(&request);
    let prepared = override_command()
        .prepare(
            &request,
            Some(&denial),
            Some(ToolRequestResolution::Denied {
                request: request.id(),
            }),
            Some(command_id(EARLIER_OVERRIDE_COMMAND_SEED)),
        )
        .expect("correlated evidence prepares a terminal result");

    assert_eq!(
        prepared.result(),
        &OverrideDeniedToolRequestResult::Rejected(
            OverrideDeniedToolRequestRejectedResult::AlreadyOverridden {
                denied_request: request.id(),
            }
        )
    );
}

/// Evidence for another request is an adapter correlation error, never a
/// recorded rejection.
#[test]
fn override_prepare_correlates_supplied_evidence() {
    const UNCORRELATED_REQUEST_SEED: u128 = 5;

    let uncorrelated = delegated_request(UNCORRELATED_REQUEST_SEED);
    let error = override_command()
        .prepare(&uncorrelated, None, None, None)
        .expect_err("mismatched request evidence must fail as a preparation error");

    assert_eq!(error.provided_request(), uncorrelated.id());
    assert_eq!(error.command(), &override_command());
}

/// The recorded applied receipt restores from its durable recorded row and
/// rejects a row that does not correlate with the command.
#[test]
fn override_reconstitute_applied_restores_the_recorded_receipt() {
    const FOREIGN_OVERRIDE_REQUEST_SEED: u128 = 4;
    const FIXTURE_JUDGE_CALL_SEED: u128 = 930;

    let request = delegated_request(DENIED_REQUEST_SEED);
    let recorded = RecordedUserOverride::new(
        command_id(OVERRIDE_COMMAND_SEED),
        request.session(),
        request.id(),
        model_call_id(FIXTURE_JUDGE_CALL_SEED),
        request.name().clone(),
        request.arguments().clone(),
    );
    let restored = override_command()
        .reconstitute_applied(recorded.clone())
        .expect("the correlated recorded row restores the applied receipt");
    let OverrideDeniedToolRequestResult::Applied(applied) = restored.result() else {
        panic!("the recorded row restores an applied result");
    };
    assert_eq!(applied.recorded(), &recorded);

    let foreign = RecordedUserOverride::new(
        command_id(OVERRIDE_COMMAND_SEED),
        request.session(),
        tool_request_id(FOREIGN_OVERRIDE_REQUEST_SEED),
        model_call_id(FIXTURE_JUDGE_CALL_SEED),
        request.name().clone(),
        request.arguments().clone(),
    );
    let error = override_command()
        .reconstitute_applied(foreign)
        .expect_err("an uncorrelated recorded row must fail closed");
    assert_eq!(
        error.provided_request(),
        tool_request_id(FOREIGN_OVERRIDE_REQUEST_SEED)
    );
}

/// the reserved user-global nil and max command sentinels cannot
/// claim override commands.
#[test]
fn override_command_identity_rejects_reserved_sentinels() {
    let nil = OverrideDeniedToolRequest::try_new(
        DurableCommandId::from_uuid(uuid::Uuid::nil()),
        session_id(1),
        tool_request_id(DENIED_REQUEST_SEED),
    )
    .expect_err("the nil sentinel is reserved");
    let max = OverrideDeniedToolRequest::try_new(
        DurableCommandId::from_uuid(uuid::Uuid::max()),
        session_id(1),
        tool_request_id(DENIED_REQUEST_SEED),
    )
    .expect_err("the max sentinel is reserved");

    assert_eq!(
        nil.command_id(),
        DurableCommandId::from_uuid(uuid::Uuid::nil())
    );
    assert_eq!(
        max.command_id(),
        DurableCommandId::from_uuid(uuid::Uuid::max())
    );
}

/// override-command comparison equality excludes only command
/// identity and retains the session and the denied request.
#[test]
fn override_command_equality_excludes_only_command_identity() {
    const REPLAY_COMMAND_SEED: u128 = 72;
    const OTHER_SESSION_SEED: u128 = 9;

    let replay = OverrideDeniedToolRequest::try_new(
        command_id(REPLAY_COMMAND_SEED),
        session_id(1),
        tool_request_id(DENIED_REQUEST_SEED),
    )
    .expect("the fixture command identity is admitted");
    let other_session = OverrideDeniedToolRequest::try_new(
        command_id(OVERRIDE_COMMAND_SEED),
        session_id(OTHER_SESSION_SEED),
        tool_request_id(DENIED_REQUEST_SEED),
    )
    .expect("the fixture command identity is admitted");

    assert_eq!(override_command(), replay);
    assert_ne!(override_command(), other_session);
}

/// A consumed user override records approval under override provenance:
/// the override source, the override command, and the overridden denial.
#[test]
fn user_override_initial_approval_records_override_provenance() {
    const CONSUMING_REQUEST_SEED: u128 = 73;

    let approval = InitialToolApproval::UserOverride {
        command: command_id(OVERRIDE_COMMAND_SEED),
        denied_request: tool_request_id(DENIED_REQUEST_SEED),
    };
    let resolution = approval
        .resolution(tool_request_id(CONSUMING_REQUEST_SEED))
        .expect("a consumed override records its approval at proposal time");

    assert_eq!(
        resolution.request(),
        tool_request_id(CONSUMING_REQUEST_SEED)
    );
    assert_eq!(resolution.source(), ToolDecisionSource::UserOverride);
    assert_eq!(
        resolution.decider(),
        Some(&ToolApprovalDecider::UserOverride {
            command: command_id(OVERRIDE_COMMAND_SEED),
            denied_request: tool_request_id(DENIED_REQUEST_SEED),
        })
    );
    assert_eq!(resolution.decision(), &ToolApprovalDecision::Approve);
    assert_eq!(resolution.rationale(), None);
    assert_eq!(approval.posture(), ToolApprovalPosture::Delegated);
    assert!(!approval.requires_decision());
}

/// a restored user-override approval requires the delegated posture frozen on its request — the
/// posture the judge would otherwise decide.
#[test]
fn user_override_reconstitution_requires_delegated_posture() {
    const CONSUMING_REQUEST_SEED: u128 = 73;

    let restored = ToolApprovalResolutionReconstitutionInput::user_override(
        tool_request_id(CONSUMING_REQUEST_SEED),
        command_id(OVERRIDE_COMMAND_SEED),
        tool_request_id(DENIED_REQUEST_SEED),
        ToolApprovalPosture::Delegated,
    )
    .reconstitute()
    .expect("the frozen delegated posture restores override authority");
    assert_eq!(restored.source(), ToolDecisionSource::UserOverride);
    assert_eq!(restored.request(), tool_request_id(CONSUMING_REQUEST_SEED));

    let human = ToolApprovalResolutionReconstitutionInput::user_override(
        tool_request_id(CONSUMING_REQUEST_SEED),
        command_id(OVERRIDE_COMMAND_SEED),
        tool_request_id(DENIED_REQUEST_SEED),
        ToolApprovalPosture::Human,
    )
    .reconstitute()
    .expect_err("a human-frozen request cannot restore override authority");
    drop(human);
    let auto = ToolApprovalResolutionReconstitutionInput::user_override(
        tool_request_id(CONSUMING_REQUEST_SEED),
        command_id(OVERRIDE_COMMAND_SEED),
        tool_request_id(DENIED_REQUEST_SEED),
        ToolApprovalPosture::Auto,
    )
    .reconstitute()
    .expect_err("an auto-frozen request cannot restore override authority");
    drop(auto);
}
