#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, ConversationRole,
    CredentialReference, DeliveryMode, LossCause, MessagePart, ModelOperation, ModelRuntime,
    Observation, ObservationFact, PreparationFailure, PreparationOutcome, ProviderErrorKind,
    RequestedTarget, ResolvedTarget, StreamInterruption, StructuredDecodeFailure,
    StructuredOutputContract, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal,
    ToolChoice, ToolDefinition, ToolName, decode_structured,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliConstructionError, CodexCliRuntime,
};

#[path = "support/fixtures.rs"]
mod fixtures;

const CREDENTIAL_REFERENCE: &str = "codex-subscription-primary";
const RESOLVED_TARGET: &str = "gpt-offline-exact";
const OFFLINE_HARNESS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum OperationShape {
    Text,
    Tool,
    Structured,
}

struct ExecutionResult {
    evidence: TerminalEvidence,
    observations: Vec<Observation<String>>,
    spawns: usize,
    argv: String,
    prompt: String,
}

#[derive(Debug, Deserialize, JsonSchema, PartialEq)]
struct Verdict {
    accepted: bool,
}

/// INV-025, INV-026: one completed call crosses exactly one process-spawn
/// dispatch boundary.
#[tokio::test]
async fn buffered_completion_is_terminal_only_after_turn_completed() {
    let scenario = "buffered_completed";
    let result = execute_scenario(
        scenario,
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);

    assert_eq!(completed.finish, CompletionFinish::EndTurn);
    assert_eq!(
        completed.content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_eq!(
        completed
            .exchange
            .provider_request_id
            .as_ref()
            .map(signalbox_model_runtime::ProviderRequestId::as_str),
        Some(fixtures::THREAD_ID)
    );
    assert_eq!(
        completed.usage,
        TokenUsage {
            input_tokens: Some(fixtures::INPUT_TOKENS),
            output_tokens: Some(fixtures::OUTPUT_TOKENS),
            cache_creation_input_tokens: Some(fixtures::CACHE_CREATION_INPUT_TOKENS),
            cache_read_input_tokens: Some(fixtures::CACHE_READ_INPUT_TOKENS),
        }
    );
    assert_eq!(result.spawns, 1);
    assert!(result.argv.contains("exec\n--json\n--ephemeral"));
    assert!(result.argv.contains("--ignore-user-config"));
    assert!(result.argv.contains("--ignore-rules"));
    assert!(result.argv.contains("--disable\nshell_tool"));
    assert!(result.argv.contains("--disable\nunified_exec"));
    assert!(result.argv.contains("--config\nproject_doc_max_bytes=0"));
    assert!(result.argv.contains(RESOLVED_TARGET));
    assert!(result.prompt.contains(scenario));
}

#[tokio::test]
async fn streamed_completion_emits_redacted_progress_in_order() {
    let result = execute_scenario(
        "streamed_completed",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);

    assert_eq!(
        completed.content,
        vec![AssistantPart::Text(fixtures::STREAMED_ANSWER.to_string())]
    );
    assert!(result.observations.iter().any(|observation| {
        observation.fact
            == ObservationFact::TextDelta {
                index: 1,
                text: fixtures::STREAMED_ANSWER.to_string(),
            }
    }));
    assert!(result.observations.iter().any(|observation| {
        observation.fact
            == ObservationFact::ThinkingDelta {
                index: 0,
                text: fixtures::REASONING_TEXT.to_string(),
            }
    }));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential token split across reasoning items cannot be
/// reconstructed by concatenating streamed provider text.
#[tokio::test]
async fn inv_035_split_credential_across_reasoning_items_is_redacted() {
    let result = execute_scenario(
        "split_stream_credential_between_reasoning_items",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let streamed = streamed_provider_text(&result.observations);

    assert!(!streamed.contains(fixtures::SENSITIVE_SPLIT_STREAM_TOKEN));
    assert!(streamed.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: changing from reasoning to final text cannot flush a held
/// credential prefix as provider-controlled bytes.
#[tokio::test]
async fn inv_035_split_credential_before_final_text_is_redacted() {
    let result = execute_scenario(
        "split_stream_credential_before_final_text",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let streamed = streamed_provider_text(&result.observations);

    assert!(!streamed.contains(fixtures::SENSITIVE_SPLIT_STREAM_TOKEN));
    assert!(streamed.contains("[redacted]"));
    assert!(result.observations.iter().any(|observation| {
        observation.fact
            == ObservationFact::TextDelta {
                index: 1,
                text: "[redacted]".to_string(),
            }
    }));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential header split between reasoning and final text keeps
/// redacting through the value, not just through the marker.
#[tokio::test]
async fn inv_035_split_authorization_value_before_final_text_is_redacted() {
    let result = execute_scenario(
        "split_stream_authorization_before_final_text",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let streamed = streamed_provider_text(&result.observations);

    assert!(!streamed.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(streamed.contains("[redacted]"));
    assert_eq!(
        completed(&result.evidence).content,
        vec![AssistantPart::Text("[redacted]".to_string())],
        "terminal completion content must carry the stateful stream redaction"
    );
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential header split between streamed reasoning and a
/// provider failure message keeps redacting through the value, not just
/// through the marker: terminal failure evidence carries the stateful
/// stream redaction, never a stateless re-redaction of the raw message.
#[tokio::test]
async fn inv_035_split_authorization_value_before_failure_is_redacted() {
    let result = execute_scenario(
        "split_stream_authorization_before_failure",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let streamed = streamed_provider_text(&result.observations);
    let error = provider_error(&result.evidence);

    assert!(!streamed.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert_eq!(
        error.native.message.as_deref(),
        Some("[redacted]"),
        "terminal failure evidence must carry the stateful stream redaction"
    );
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn only_the_last_agent_message_is_decoded_as_the_terminal_envelope() {
    let result = execute_scenario(
        "last_agent_message",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        completed(&result.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
}

#[tokio::test]
async fn an_undecodable_last_agent_message_is_boundary_loss() {
    let result = execute_scenario(
        "malformed_last_agent_message",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert!(
        response_unintelligible(&boundary_loss(&result.evidence).cause)
            .contains("last agent message")
    );
}

#[tokio::test]
async fn credential_shaped_envelope_errors_are_content_silent() {
    let result = execute_scenario(
        "credential_envelope_error",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let detail = response_unintelligible(&boundary_loss(&result.evidence).cause);

    assert!(!detail.contains(fixtures::SENSITIVE_ENVELOPE_TOKEN));
    assert_eq!(
        detail,
        "last agent message does not match the response envelope"
    );
}

#[tokio::test]
async fn deeply_nested_decoded_envelope_is_boundary_loss() {
    let result = execute_scenario(
        "deep_agent_message",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;

    assert!(
        response_unintelligible(&boundary_loss(&result.evidence).cause).contains("JSON nesting")
    );
}

#[tokio::test]
async fn tool_call_arguments_remain_verbatim() {
    let result = execute_scenario(
        "tool_call",
        DeliveryMode::Streamed,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);
    let proposal = tool_proposal(&completed.content);

    assert_eq!(completed.finish, CompletionFinish::ToolUse);
    assert_eq!(proposal.name.as_str(), fixtures::TOOL_NAME);
    assert_eq!(proposal.arguments_json, fixtures::TOOL_ARGUMENTS);
    assert_eq!(
        observed_tool_arguments(&result.observations),
        Some(fixtures::TOOL_ARGUMENTS)
    );
}

#[tokio::test]
async fn buffered_tool_call_retains_the_same_verbatim_arguments() {
    let result = execute_scenario(
        "tool_call",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);

    assert_eq!(
        tool_proposal(&completed.content).arguments_json,
        fixtures::TOOL_ARGUMENTS
    );
}

/// Defect regression (found by the gated compatibility smoke): the envelope
/// carries each tool call's argument object as JSON text inside a string,
/// because strict structured output forbids a free-form object member. A
/// string that does not hold JSON is unintelligible-response boundary loss,
/// never completion material.
#[tokio::test]
async fn non_json_string_carried_tool_arguments_are_boundary_loss() {
    let result = execute_scenario(
        "tool_call_bad_arguments",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;

    assert!(
        response_unintelligible(&boundary_loss(&result.evidence).cause)
            .contains("arguments are not valid JSON")
    );
}

/// The provider nesting bound applies to the argument text carried inside
/// the envelope string, which the line-level and agent-message-level checks
/// cannot see because string content does not nest the outer JSON.
#[tokio::test]
async fn over_deep_string_carried_tool_arguments_are_boundary_loss() {
    let result = execute_scenario(
        "tool_call_deep_arguments",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;

    assert!(
        response_unintelligible(&boundary_loss(&result.evidence).cause)
            .contains("arguments: provider JSON exceeds")
    );
}

/// Defect regression (found by the gated compatibility smoke): the live API
/// rejected the adapter's original output schema as `invalid_json_schema`
/// because its `arguments` member was a free-form object
/// (`additionalProperties: true`). The fake CLI validates the output schema
/// of every spawn with `fixtures::strict_schema_violation`; this pins that
/// the validator still rejects the retired shape, so the offline corpus can
/// never again accept a schema the live API refuses.
#[test]
fn the_strict_schema_validator_rejects_the_retired_free_form_arguments_object() {
    let retired_arguments_member = serde_json::json!({
        "type": "object",
        "properties": {
            "arguments": {"type": "object", "additionalProperties": true}
        },
        "required": ["arguments"],
        "additionalProperties": false
    });

    let violation = fixtures::strict_schema_violation(&retired_arguments_member)
        .expect("strict validation must reject a free-form arguments object");

    assert!(
        violation.contains("'additionalProperties' is required to be supplied and to be false")
    );
    assert!(violation.contains("'arguments'"));
}

#[tokio::test]
async fn structured_output_uses_the_shared_forced_tool_decode() {
    let result = execute_scenario(
        "structured_output",
        DeliveryMode::Buffered,
        OperationShape::Structured,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);
    let contract =
        signalbox_model_runtime::StructuredOutputContract::of_type::<Verdict>("verdict", "verdict");
    let decoded: Verdict = decode_structured(
        &completed.content,
        &contract,
        &signalbox_model_runtime::NoDomainConstraints,
    )
    .expect("the forced structured proposal must decode");

    assert_eq!(
        decoded,
        Verdict {
            accepted: fixtures::STRUCTURED_ACCEPTED,
        }
    );
}

#[tokio::test]
async fn missing_structured_output_reaches_the_shared_typed_decode() {
    let result = execute_scenario(
        "structured_output_missing",
        DeliveryMode::Buffered,
        OperationShape::Structured,
        CancellationSignal::never(),
    )
    .await;
    let contract = StructuredOutputContract::of_type::<Verdict>("verdict", "verdict");
    let failure = decode_structured::<Verdict, _>(
        &completed(&result.evidence).content,
        &contract,
        &signalbox_model_runtime::NoDomainConstraints,
    )
    .expect_err("the shared decoder classifies a missing structured value");

    assert_eq!(failure, StructuredDecodeFailure::NoStructuredValue);
}

#[tokio::test]
async fn multiple_structured_outputs_reach_the_shared_typed_decode() {
    let result = execute_scenario(
        "structured_output_multiple",
        DeliveryMode::Buffered,
        OperationShape::Structured,
        CancellationSignal::never(),
    )
    .await;
    let contract = StructuredOutputContract::of_type::<Verdict>("verdict", "verdict");
    let failure = decode_structured::<Verdict, _>(
        &completed(&result.evidence).content,
        &contract,
        &signalbox_model_runtime::NoDomainConstraints,
    )
    .expect_err("the shared decoder classifies multiple structured values");

    assert_eq!(
        failure,
        StructuredDecodeFailure::MultipleStructuredValues { count: 2 }
    );
}

#[tokio::test]
async fn structured_output_can_end_in_explicit_refusal() {
    let result = execute_scenario(
        "structured_refused",
        DeliveryMode::Buffered,
        OperationShape::Structured,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        refused(&result.evidence).content,
        vec![AssistantPart::Text(fixtures::REFUSAL_TEXT.to_string())]
    );
}

#[tokio::test]
async fn tool_choice_is_ignored_when_no_tools_or_contract_exist() {
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.tool_choice = ToolChoice::AnyTool;
    let result = execute_operation_with_timeout(
        operation,
        CancellationSignal::never(),
        OFFLINE_HARNESS_TIMEOUT,
    )
    .await;

    assert_eq!(
        completed(&result.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn named_tool_choice_rejects_an_extra_declared_tool_proposal() {
    let mut operation = operation(
        "named_choice_extra_tool",
        DeliveryMode::Buffered,
        OperationShape::Tool,
    );
    operation.tools.push(ToolDefinition::with_schema(
        fixtures::OTHER_TOOL_NAME,
        fixtures::OTHER_TOOL_NAME,
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    ));
    operation.tool_choice = ToolChoice::Named(ToolName::new(fixtures::TOOL_NAME));
    let result = execute_operation_with_timeout(
        operation,
        CancellationSignal::never(),
        OFFLINE_HARNESS_TIMEOUT,
    )
    .await;

    assert!(
        response_unintelligible(&boundary_loss(&result.evidence).cause)
            .contains(fixtures::TOOL_NAME)
    );
}

#[tokio::test]
async fn explicit_refusal_is_refusal_evidence() {
    let result = execute_scenario(
        "refused",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        refused(&result.evidence).content,
        vec![AssistantPart::Text(fixtures::REFUSAL_TEXT.to_string())]
    );
}

#[tokio::test]
async fn credential_rejection_precedes_a_buffered_refusal() {
    assert_error_scenario(
        "credential_precedence",
        ProviderErrorKind::CredentialRejected,
    )
    .await;
}

#[tokio::test]
async fn stderr_credential_rejection_is_classified_before_exit_status() {
    let result = execute_scenario(
        "stderr_redaction",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let error = provider_error(&result.evidence);

    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
    assert!(
        !error
            .native
            .message
            .as_deref()
            .unwrap_or_default()
            .contains(fixtures::SENSITIVE_STDERR_TOKEN)
    );
}

#[tokio::test]
async fn permission_error_is_typed() {
    assert_error_scenario("error_permission", ProviderErrorKind::PermissionDenied).await;
}

#[tokio::test]
async fn invalid_request_error_is_typed() {
    assert_error_scenario("error_invalid_request", ProviderErrorKind::InvalidRequest).await;
}

#[tokio::test]
async fn target_not_found_error_is_typed() {
    assert_error_scenario("error_target_not_found", ProviderErrorKind::TargetNotFound).await;
}

#[tokio::test]
async fn request_too_large_error_is_typed() {
    assert_error_scenario(
        "error_request_too_large",
        ProviderErrorKind::RequestTooLarge,
    )
    .await;
}

#[tokio::test]
async fn rate_limit_error_is_typed() {
    assert_error_scenario("error_rate_limited", ProviderErrorKind::RateLimited).await;
}

#[tokio::test]
async fn quota_exhaustion_error_is_typed() {
    assert_error_scenario("error_quota_exhausted", ProviderErrorKind::QuotaExhausted).await;
}

#[tokio::test]
async fn overload_error_is_typed() {
    assert_error_scenario("error_overloaded", ProviderErrorKind::Overloaded).await;
}

#[tokio::test]
async fn provider_internal_error_is_typed() {
    assert_error_scenario(
        "error_provider_internal",
        ProviderErrorKind::ProviderInternal,
    )
    .await;
}

#[tokio::test]
async fn unknown_definitive_error_fails_closed() {
    assert_error_scenario("error_unrecognized", ProviderErrorKind::Unrecognized).await;
}

/// Defect regression (found by the gated compatibility smoke): the pinned
/// CLI reports a failed exchange as a stream-level `error` event followed by
/// its `turn.failed` lifecycle echo. The decoder accepts exactly that
/// trailer and keeps the stream-level message, so the typed provider error
/// is never downgraded to a post-terminal protocol violation.
#[tokio::test]
async fn a_turn_failed_echo_after_a_stream_error_keeps_the_typed_provider_error() {
    let result = execute_scenario(
        "error_then_turn_failed",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let error = provider_error(&result.evidence);

    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert_eq!(
        error.native.message.as_deref(),
        Some(fixtures::STREAM_ERROR_MESSAGE)
    );
    assert_eq!(result.spawns, 1);
}

/// A trailer that contradicts the recorded stream-level error — here a
/// `turn.completed` claiming success — still fails closed instead of being
/// absorbed as lifecycle closure.
#[tokio::test]
async fn a_completion_trailer_after_a_stream_error_still_fails_closed() {
    assert_error_scenario("error_then_turn_completed", ProviderErrorKind::Unrecognized).await;
}

#[tokio::test]
async fn undecodable_event_fails_closed_as_unrecognized_provider_error() {
    assert_error_scenario("malformed_event", ProviderErrorKind::Unrecognized).await;
}

#[tokio::test]
async fn decoded_progress_is_flushed_before_a_later_event_fails() {
    let result = execute_scenario(
        "reasoning_then_malformed_event",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        provider_error(&result.evidence).kind,
        ProviderErrorKind::Unrecognized
    );
    assert!(result.observations.iter().any(|observation| {
        observation.fact
            == ObservationFact::ThinkingDelta {
                index: 0,
                text: fixtures::PENDING_PROGRESS_TEXT.to_string(),
            }
    }));
}

#[tokio::test]
async fn malformed_known_lifecycle_event_fails_closed() {
    let result = execute_scenario(
        "malformed_known_lifecycle",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        provider_error(&result.evidence).kind,
        ProviderErrorKind::Unrecognized
    );
    assert!(
        provider_error(&result.evidence)
            .native
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("known event has invalid shape")
    );
}

#[tokio::test]
async fn empty_completed_item_identity_fails_closed() {
    let result = execute_scenario(
        "empty_completed_item_identity",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        provider_error(&result.evidence).kind,
        ProviderErrorKind::Unrecognized
    );
    assert!(
        provider_error(&result.evidence)
            .native
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("empty item identity")
    );
}

#[tokio::test]
async fn nonzero_signal_exit_fails_closed_as_unrecognized_provider_error() {
    assert_error_scenario("killed_process", ProviderErrorKind::Unrecognized).await;
}

/// INV-025, INV-026: losing the CLI terminal marker remains ambiguous and
/// never triggers a replacement spawn.
#[tokio::test]
async fn exit_zero_without_terminal_marker_is_boundary_loss() {
    let result = execute_scenario(
        "no_terminal",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let loss = boundary_loss(&result.evidence);

    assert_eq!(
        loss.cause,
        LossCause::StreamEndedWithoutTerminalMarker {
            interruption: StreamInterruption::EndOfStream,
        }
    );
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn omitted_cache_counters_remain_unreported() {
    let result = execute_scenario(
        "usage_without_cache",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        completed(&result.evidence).usage,
        TokenUsage {
            input_tokens: Some(fixtures::INPUT_TOKENS),
            output_tokens: Some(fixtures::OUTPUT_TOKENS),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }
    );
}

/// INV-025, INV-026: pre-dispatch cancellation performs no process spawn.
#[tokio::test]
async fn cancellation_before_spawn_is_proven_unsent() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(
            prepared,
            &mut observations,
            CancellationSignal::already_cancelled(),
        )
        .await;

    assert_eq!(
        proven_unsent(&report.evidence).cause,
        signalbox_model_runtime::UnsentCause::CancelledBeforeSend
    );
    assert_eq!(spawn_count(temporary.path()), 0);
    assert!(
        !observations
            .iter()
            .any(|observation| observation.fact == ObservationFact::SendCommenced)
    );
}

#[tokio::test]
async fn missing_cli_binary_is_proven_unsent() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), temporary.path().join("missing-codex"));
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    assert_eq!(unsent_kind(&report.evidence), UnsentKind::ConnectFailed);
    assert_eq!(spawn_count(temporary.path()), 0);
}

/// INV-025, INV-026: post-dispatch cancellation interrupts the original
/// process and never respawns it.
#[tokio::test]
async fn cancellation_after_spawn_interrupts_once_without_respawn() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(
        &runtime,
        operation("hang", DeliveryMode::Streamed, OperationShape::Text),
    )
    .await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-spawns"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;
    let loss = boundary_loss(&report.evidence);

    assert_eq!(loss.cause, LossCause::CancellationRequested);
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[tokio::test]
async fn cancellation_is_not_starved_by_continuously_ready_stdout() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(
        &runtime,
        operation("busy_stdout", DeliveryMode::Streamed, OperationShape::Text),
    )
    .await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-busy-stdout"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert_eq!(
        boundary_loss(&report.evidence).cause,
        LossCause::CancellationRequested
    );
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[tokio::test]
async fn buffered_terminal_output_wins_over_simultaneous_cancellation() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(
        &runtime,
        operation(
            "completion_before_cancellation",
            DeliveryMode::Streamed,
            OperationShape::Text,
        ),
    )
    .await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-completion-ready"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert_eq!(
        completed(&report.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[tokio::test]
async fn whole_process_timeout_is_boundary_loss_without_respawn() {
    let result = execute_scenario_with_timeout(
        "hang",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
        Duration::from_millis(100),
    )
    .await;
    let loss = boundary_loss(&result.evidence);

    assert_eq!(
        timed_out(&loss.cause).detail,
        "Codex CLI process exceeded its exchange timeout"
    );
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn cancellation_while_stdin_is_blocked_interrupts_the_spawned_process() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    std::fs::write(temporary.path().join("fake-codex-block-stdin"), "block")
        .expect("the fake CLI blocking marker is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(&runtime, blocked_input_operation()).await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-block-stdin-ready"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert_eq!(
        boundary_loss(&report.evidence).cause,
        LossCause::CancellationRequested
    );
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[tokio::test]
async fn timeout_while_stdin_is_blocked_covers_the_whole_spawn_lifetime() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    std::fs::write(temporary.path().join("fake-codex-block-stdin"), "block")
        .expect("the fake CLI blocking marker is created");
    let runtime = runtime_with_timeout(temporary.path(), fake_cli(), Duration::from_millis(100));
    let prepared = prepare(&runtime, blocked_input_operation()).await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    assert_eq!(
        timed_out(&boundary_loss(&report.evidence).cause).detail,
        "Codex CLI process exceeded its exchange timeout"
    );
}

#[tokio::test]
async fn nonzero_exit_while_writing_stdin_preserves_provider_error() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    std::fs::write(
        temporary.path().join(fixtures::EARLY_STDIN_EXIT_MARKER),
        "exit",
    )
    .expect("the early-exit marker is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(&runtime, blocked_input_operation()).await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    let failure = provider_error(&report.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::RequestTooLarge);
    assert!(
        failure
            .native
            .message
            .as_deref()
            .is_some_and(|message| message.contains(fixtures::EARLY_STDIN_FAILURE))
    );
    assert_eq!(
        failure
            .exchange
            .provider_request_id
            .as_ref()
            .map(signalbox_model_runtime::ProviderRequestId::as_str),
        Some(fixtures::THREAD_ID)
    );
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[tokio::test]
async fn completion_after_an_incomplete_stdin_upload_is_boundary_loss() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    std::fs::write(
        temporary
            .path()
            .join(fixtures::EARLY_STDIN_COMPLETION_MARKER),
        "complete",
    )
    .expect("the early-completion marker is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(&runtime, blocked_input_operation()).await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    let failure = transport_failed(&boundary_loss(&report.evidence).cause);

    assert!(
        failure
            .detail
            .contains("before the full request upload completed")
    );
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_completion_after_an_incomplete_stdin_upload_is_boundary_loss() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stderr_holding_incomplete_upload_cli(temporary.path());
    let runtime = runtime(temporary.path(), executable);
    let prepared = prepare(&runtime, blocked_input_operation()).await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-stderr-wait-ready"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;
    let loss = boundary_loss(&report.evidence);
    let failure = transport_failed(&loss.cause);

    assert!(
        failure
            .detail
            .contains("before the full request upload completed")
    );
    assert_eq!(
        loss.usage,
        TokenUsage {
            input_tokens: Some(fixtures::INPUT_TOKENS),
            output_tokens: Some(fixtures::OUTPUT_TOKENS),
            cache_creation_input_tokens: Some(fixtures::CACHE_CREATION_INPUT_TOKENS),
            cache_read_input_tokens: Some(fixtures::CACHE_READ_INPUT_TOKENS),
        },
        "the demoted nominal completion still carries its observed usage"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_descendants_after_the_group_leader_exits() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let mut config = CodexCliConfig::new(
        fake_cli(),
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );
    config.exchange_timeout = Duration::from_secs(5);
    config.interrupt_grace = Duration::from_millis(100);
    let runtime = CodexCliRuntime::new(config).expect("offline runtime configuration is valid");
    let prepared = prepare(
        &runtime,
        operation(
            "interrupt_with_descendant",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let descendant_path = temporary
        .path()
        .join("fake-codex-interrupt-descendant-process-group");
    let cancellation = cancel_after_record(descendant_path.clone());
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert_eq!(
        boundary_loss(&report.evidence).cause,
        LossCause::CancellationRequested
    );
    assert_recorded_process_group_exited(descendant_path);
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_execution_kills_the_spawned_process_group() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let prepared = prepare(
        &runtime,
        operation(
            "interrupt_with_descendant",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let descendant_path = temporary
        .path()
        .join("fake-codex-interrupt-descendant-process-group");
    let execution = tokio::spawn(async move {
        let mut observations = Vec::new();
        runtime
            .execute(prepared, &mut observations, CancellationSignal::never())
            .await
    });

    wait_for_record(descendant_path.clone()).await;
    execution.abort();
    let aborted = execution
        .await
        .expect_err("the execution task was explicitly aborted");

    assert!(aborted.is_cancelled());
    assert_recorded_process_group_exited(descendant_path);
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[tokio::test]
async fn cancellation_grace_cannot_extend_the_exchange_deadline() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let mut config = CodexCliConfig::new(
        fake_cli(),
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );
    config.exchange_timeout = Duration::from_secs(5);
    config.interrupt_grace = Duration::from_secs(10);
    let runtime = CodexCliRuntime::new(config).expect("offline runtime configuration is valid");
    let prepared = prepare(
        &runtime,
        operation("hang", DeliveryMode::Buffered, OperationShape::Text),
    )
    .await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-spawns"));
    let started = std::time::Instant::now();
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert_eq!(
        boundary_loss(&report.evidence).cause,
        LossCause::CancellationRequested
    );
    assert!(started.elapsed() < Duration::from_secs(7));
    assert_eq!(spawn_count(temporary.path()), 1);
}

#[tokio::test]
async fn inherited_stderr_cannot_extend_process_cleanup_past_deadline() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let result = execute_operation_in_directory(
        temporary.path(),
        operation(
            "inherited_stderr",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
        CancellationSignal::never(),
        Duration::from_secs(1),
    )
    .await;

    assert_eq!(
        completed(&result.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_eq!(result.spawns, 1);
    assert_recorded_process_group_exited(
        temporary
            .path()
            .join("fake-codex-inherited-stderr-process-group"),
    );
}

#[cfg(unix)]
#[tokio::test]
async fn successful_exit_kills_a_detached_process_group_descendant() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let result = execute_operation_in_directory(
        temporary.path(),
        operation(
            "completed_with_detached_descendant",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
        CancellationSignal::never(),
        Duration::from_secs(1),
    )
    .await;

    assert_eq!(
        completed(&result.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_eq!(result.spawns, 1);
    assert_recorded_process_group_exited(
        temporary
            .path()
            .join("fake-codex-detached-descendant-process-group"),
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stderr_cleanup_timeout_preserves_boundary_loss_evidence() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stdout_closing_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_millis(100));
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    assert_eq!(
        timed_out(&boundary_loss(&report.evidence).cause).detail,
        "Codex CLI process exceeded its exchange timeout"
    );
}

/// The retained output-schema argument is absolute, so the child's move to
/// the configured working root cannot re-root a relative schema path.
#[tokio::test]
async fn output_schema_argument_is_an_absolute_path() {
    let result = execute_scenario(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let schema_argument = result
        .argv
        .lines()
        .skip_while(|line| *line != "--output-schema")
        .nth(1)
        .expect("the argv records an --output-schema value");

    assert!(std::path::Path::new(schema_argument).is_absolute());
}

#[tokio::test]
async fn subprocess_environment_is_allowlisted() {
    let result = execute_scenario(
        "filtered_environment",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        completed(&result.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_eq!(result.spawns, 1);
}

/// INV-035: credential-shaped CLI text and tool JSON are redacted before
/// observations or terminal evidence leave the adapter.
#[tokio::test]
async fn inv_035_cli_output_is_credential_shape_redacted() {
    let result = execute_scenario(
        "redaction",
        DeliveryMode::Streamed,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_OUTPUT_TOKEN));
    assert!(!diagnostic.contains(fixtures::SENSITIVE_REFRESH_TOKEN));
    assert!(!diagnostic.contains(fixtures::SENSITIVE_COMPOSITE_SECRET));
    assert!(diagnostic.contains("[redacted]"));
}

/// INV-035: a bare JSON credential member at the start of CLI-controlled text
/// is still recognized without an enclosing object delimiter.
#[tokio::test]
async fn inv_035_bare_credential_member_is_redacted() {
    let result = execute_scenario(
        "bare_credential_text",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = format!("{:?}", result.evidence);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_COMPOSITE_SECRET));
    assert!(diagnostic.contains("[redacted]"));
}

/// INV-035: a credential member whose value is a JSON object is consumed
/// through its balanced structural close before terminal evidence leaves the
/// adapter, never released piecewise past its first structural character.
#[tokio::test]
async fn inv_035_structured_credential_value_is_redacted_whole() {
    let result = execute_scenario(
        "structured_credential_value",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = format!("{:?}", result.evidence);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_STRUCTURED_SECRET));
    assert!(diagnostic.contains("[redacted]"));
}

/// INV-035: a structured credential value split across streamed reasoning
/// items cannot be reconstructed by concatenating the emitted deltas.
#[tokio::test]
async fn inv_035_split_structured_credential_value_is_redacted() {
    let result = execute_scenario(
        "split_stream_structured_credential",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let streamed = streamed_provider_text(&result.observations);

    assert!(!streamed.contains(fixtures::SENSITIVE_STRUCTURED_SECRET));
    assert!(streamed.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn synchronous_preparation_wins_over_ready_cancellation() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let _prepared = prepare_with_cancellation(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
        CancellationSignal::already_cancelled(),
    )
    .await;

    assert_eq!(spawn_count(temporary.path()), 0);
}

async fn prepare_with_cancellation(
    runtime: &CodexCliRuntime,
    operation: ModelOperation<String>,
    cancellation: CancellationSignal,
) -> signalbox_model_runtime_codex_cli::CodexCliPreparedRequest<String> {
    match runtime.prepare(operation, cancellation).await {
        PreparationOutcome::Prepared(prepared) => prepared,
        PreparationOutcome::Cancelled { .. } => {
            panic!("synchronous offline preparation must win over cancellation")
        }
        PreparationOutcome::Failed { failure, .. } => {
            panic!("offline preparation failed: {failure:?}")
        }
        PreparationOutcome::Defect { defect, .. } => {
            panic!("offline preparation found a defect: {defect:?}")
        }
    }
}

/// INV-035: distinct credential-shaped provider ids remain distinct without
/// exposing their raw values.
#[tokio::test]
async fn inv_035_redacted_tool_ids_receive_distinct_safe_surrogates() {
    let result = execute_scenario(
        "sensitive_tool_ids",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let content = &completed(&result.evidence).content;
    let diagnostic = format!("{:?}", result.evidence);

    assert_eq!(
        tool_ids(content),
        vec![
            fixtures::REDACTED_TOOL_ID_ONE,
            fixtures::REDACTED_TOOL_ID_TWO
        ]
    );
    assert!(!diagnostic.contains(fixtures::SENSITIVE_TOOL_ID_ONE));
    assert!(!diagnostic.contains(fixtures::SENSITIVE_TOOL_ID_TWO));
}

#[tokio::test]
async fn non_object_structured_schema_fails_before_spawn() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let mut operation = operation(
        "structured_output",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.output_contract = Some(StructuredOutputContract {
        name: ToolName::new("verdict"),
        description: "verdict".to_string(),
        schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "string"}))
            .expect("the test schema serializes"),
    });

    let failure = failed_preparation(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );

    assert!(unsupported_detail(failure).contains("must describe an object"));
    assert_eq!(spawn_count(temporary.path()), 0);
}

#[tokio::test]
async fn undeclared_named_choice_fails_before_spawn() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.tool_choice = ToolChoice::Named(ToolName::new("missing"));

    let failure = failed_preparation(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );

    assert!(unsupported_detail(failure).contains("no declared tool"));
    assert_eq!(spawn_count(temporary.path()), 0);
}

#[tokio::test]
async fn malformed_replayed_tool_json_is_unsupported_before_spawn() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.messages = vec![ConversationMessage {
        role: ConversationRole::Assistant,
        parts: vec![MessagePart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("call-invalid-json"),
            name: ToolName::new(fixtures::TOOL_NAME),
            arguments_json: "{not json".to_string(),
        })],
    }];

    let failure = failed_preparation(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );

    assert!(unsupported_detail(failure).contains("not valid JSON"));
    assert_eq!(spawn_count(temporary.path()), 0);
}

#[tokio::test]
async fn precision_sensitive_replayed_json_number_is_preserved_in_the_prompt() {
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Tool,
    );
    operation.messages.push(ConversationMessage {
        role: ConversationRole::Assistant,
        parts: vec![MessagePart::ToolCall(ToolCallProposal {
            id: ToolCallId::new("call-precise-json"),
            name: ToolName::new(fixtures::TOOL_NAME),
            arguments_json: format!(r#"{{"value":{}}}"#, fixtures::PRECISE_JSON_NUMBER),
        })],
    });

    let result = execute_operation_with_timeout(
        operation,
        CancellationSignal::never(),
        OFFLINE_HARNESS_TIMEOUT,
    )
    .await;

    assert!(result.prompt.contains(fixtures::PRECISE_JSON_NUMBER));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn generation_settings_are_rendered_as_advisory_prompt_context() {
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.settings.max_output_tokens = fixtures::ADVISORY_MAX_OUTPUT_TOKENS;
    operation.settings.temperature = Some(fixtures::ADVISORY_TEMPERATURE);
    operation.settings.top_p = Some(fixtures::ADVISORY_TOP_P);
    operation.settings.stop_sequences = vec![fixtures::ADVISORY_STOP_SEQUENCE.to_string()];

    let result = execute_operation_with_timeout(
        operation,
        CancellationSignal::never(),
        OFFLINE_HARNESS_TIMEOUT,
    )
    .await;
    let request = rendered_request(&result.prompt);

    assert_eq!(
        request["settings"]["max_output_tokens"],
        fixtures::ADVISORY_MAX_OUTPUT_TOKENS
    );
    assert_eq!(
        request["settings"]["temperature"],
        fixtures::ADVISORY_TEMPERATURE
    );
    assert_eq!(request["settings"]["top_p"], fixtures::ADVISORY_TOP_P);
    assert_eq!(
        request["settings"]["stop_sequences"][0],
        fixtures::ADVISORY_STOP_SEQUENCE
    );
    assert!(result.prompt.contains("advisory intent"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn zero_output_token_limit_fails_before_spawn() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.settings.max_output_tokens = 0;

    let failure = failed_preparation(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );

    assert!(unsupported_detail(failure).contains("at least 1"));
    assert_eq!(spawn_count(temporary.path()), 0);
}

#[tokio::test]
async fn non_finite_temperature_fails_before_spawn() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.settings.temperature = Some(f64::NAN);

    let failure = failed_preparation(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );

    assert!(unsupported_detail(failure).contains("finite number"));
    assert_eq!(spawn_count(temporary.path()), 0);
}

#[tokio::test]
async fn out_of_domain_top_p_fails_before_spawn() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), fake_cli());
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.settings.top_p = Some(1.1);

    let failure = failed_preparation(
        runtime
            .prepare(operation, CancellationSignal::never())
            .await,
    );

    assert!(unsupported_detail(failure).contains("from 0 through 1"));
    assert_eq!(spawn_count(temporary.path()), 0);
}

#[test]
fn relative_executable_is_rejected_at_construction() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let config = CodexCliConfig::new(
        "codex",
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );

    let error = CodexCliRuntime::new(config)
        .expect_err("relative executable meaning would change under the child directory");

    assert_eq!(error, CodexCliConstructionError::RelativeExecutable);
}

#[test]
fn relative_working_directory_is_rejected_at_construction() {
    let config = CodexCliConfig::new(
        fake_cli(),
        ".",
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );

    let error = CodexCliRuntime::new(config)
        .expect_err("a relative working root would be resolved by both cwd and --cd");

    assert_eq!(error, CodexCliConstructionError::RelativeWorkingDirectory);
}

#[test]
fn unrepresentable_exchange_timeout_is_rejected_at_construction() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let mut config = CodexCliConfig::new(
        fake_cli(),
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );
    config.exchange_timeout = Duration::MAX;

    let error = CodexCliRuntime::new(config)
        .expect_err("execution must never panic while constructing its process deadline");

    assert_eq!(error, CodexCliConstructionError::InvalidExchangeTimeout);
}

#[cfg(not(unix))]
#[test]
fn unsupported_platform_is_rejected_at_construction() {
    let config = CodexCliConfig::new(
        "/codex",
        "/",
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );

    let error = CodexCliRuntime::new(config)
        .expect_err("process descendants cannot be supervised on this platform");

    assert_eq!(error, CodexCliConstructionError::UnsupportedPlatform);
}

async fn assert_error_scenario(scenario: &str, expected: ProviderErrorKind) {
    let result = execute_scenario(
        scenario,
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(provider_error(&result.evidence).kind, expected);
    assert_eq!(result.spawns, 1);
}

async fn execute_scenario(
    scenario: &str,
    delivery: DeliveryMode,
    shape: OperationShape,
    cancellation: CancellationSignal,
) -> ExecutionResult {
    execute_scenario_with_timeout(
        scenario,
        delivery,
        shape,
        cancellation,
        OFFLINE_HARNESS_TIMEOUT,
    )
    .await
}

async fn execute_scenario_with_timeout(
    scenario: &str,
    delivery: DeliveryMode,
    shape: OperationShape,
    cancellation: CancellationSignal,
    exchange_timeout: Duration,
) -> ExecutionResult {
    execute_operation_with_timeout(
        operation(scenario, delivery, shape),
        cancellation,
        exchange_timeout,
    )
    .await
}

async fn execute_operation_with_timeout(
    operation: ModelOperation<String>,
    cancellation: CancellationSignal,
    exchange_timeout: Duration,
) -> ExecutionResult {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    execute_operation_in_directory(temporary.path(), operation, cancellation, exchange_timeout)
        .await
}

async fn execute_operation_in_directory(
    directory: &Path,
    operation: ModelOperation<String>,
    cancellation: CancellationSignal,
    exchange_timeout: Duration,
) -> ExecutionResult {
    let runtime = runtime_with_timeout(directory, fake_cli(), exchange_timeout);
    let prepared = prepare(&runtime, operation).await;
    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;
    let argv = read_optional(directory.join("fake-codex-argv"));
    let prompt = read_optional(directory.join("fake-codex-prompt"));

    ExecutionResult {
        evidence: report.evidence,
        observations,
        spawns: spawn_count(directory),
        argv,
        prompt,
    }
}

fn runtime(working_directory: &Path, executable: impl Into<std::path::PathBuf>) -> CodexCliRuntime {
    runtime_with_timeout(working_directory, executable, OFFLINE_HARNESS_TIMEOUT)
}

fn runtime_with_timeout(
    working_directory: &Path,
    executable: impl Into<std::path::PathBuf>,
    exchange_timeout: Duration,
) -> CodexCliRuntime {
    let mut config = CodexCliConfig::new(
        executable,
        working_directory,
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );
    config.exchange_timeout = exchange_timeout;
    config.interrupt_grace = Duration::from_millis(100);
    CodexCliRuntime::new(config).expect("offline runtime configuration is valid")
}

fn operation(
    scenario: &str,
    delivery: DeliveryMode,
    shape: OperationShape,
) -> ModelOperation<String> {
    let mut operation = ModelOperation::new(
        scenario.to_string(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
        RequestedTarget::new("offline-selection"),
        ResolvedTarget::new(RESOLVED_TARGET),
        vec![signalbox_model_runtime::ConversationMessage::user_text(
            scenario,
        )],
        signalbox_model_runtime::ModelSettings::new(256),
    );
    operation.delivery = delivery;
    match shape {
        OperationShape::Text => {}
        OperationShape::Tool => {
            operation.tools = vec![ToolDefinition::with_schema(
                fixtures::TOOL_NAME,
                fixtures::TOOL_NAME,
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["city", "limit"]
                }),
            )];
        }
        OperationShape::Structured => {
            operation.output_contract = Some(
                signalbox_model_runtime::StructuredOutputContract::of_type::<Verdict>(
                    "verdict", "verdict",
                ),
            );
        }
    }
    operation
}

fn blocked_input_operation() -> ModelOperation<String> {
    let mut operation = operation(
        "buffered_completed",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    operation.messages = vec![signalbox_model_runtime::ConversationMessage::user_text(
        "x".repeat(2 * 1024 * 1024),
    )];
    operation
}

async fn prepare(
    runtime: &CodexCliRuntime,
    operation: ModelOperation<String>,
) -> signalbox_model_runtime_codex_cli::CodexCliPreparedRequest<String> {
    match runtime
        .prepare(operation, CancellationSignal::never())
        .await
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        PreparationOutcome::Cancelled { .. } => panic!("offline preparation was not cancelled"),
        PreparationOutcome::Failed { failure, .. } => {
            panic!("offline preparation failed: {failure:?}")
        }
        PreparationOutcome::Defect { defect, .. } => {
            panic!("offline preparation found a defect: {defect:?}")
        }
    }
}

fn fake_cli() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_signalbox-fake-codex-cli"))
}

/// Scripts the reproduced launder-by-cancellation sequence: refuse the
/// request upload, emit a nominal completion, close stdout, then hold stderr
/// open so cancellation arrives while the adapter waits on stderr. The
/// readiness marker is written only after stdout and stdin are closed and a
/// settling second has passed, so cancellation cannot fire before the adapter
/// has consumed the terminal marker and parked in its stderr wait.
#[cfg(unix)]
fn stderr_holding_incomplete_upload_cli(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let envelope_text = format!(
        r#"{{\"outcome\":\"completed\",\"text\":\"{}\",\"tool_calls\":[]}}"#,
        fixtures::BUFFERED_ANSWER
    );
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' '{{"type":"thread.started","thread_id":"{thread_id}"}}'
printf '%s\n' '{{"type":"turn.started"}}'
printf '%s\n' '{{"type":"item.completed","item":{{"id":"message-offline-1","type":"agent_message","text":"{envelope_text}"}}}}'
printf '%s\n' '{{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":{cache_read},"cache_write_input_tokens":{cache_write},"output_tokens":{output},"reasoning_output_tokens":3}}}}'
exec 1>&-
exec 0<&-
sleep 1
printf 'ready\n' > fake-codex-stderr-wait-ready
sleep 60
"#,
        thread_id = fixtures::THREAD_ID,
        envelope_text = envelope_text,
        input = fixtures::INPUT_TOKENS,
        cache_read = fixtures::CACHE_READ_INPUT_TOKENS,
        cache_write = fixtures::CACHE_CREATION_INPUT_TOKENS,
        output = fixtures::OUTPUT_TOKENS,
    );
    let executable = directory.join("stderr-holding-codex");
    std::fs::write(&executable, script).expect("the stderr-holding fake CLI is written");
    let mut permissions = std::fs::metadata(&executable)
        .expect("the stderr-holding fake CLI has metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions)
        .expect("the stderr-holding fake CLI is executable");
    executable
}

#[cfg(unix)]
fn stdout_closing_cli(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let executable = directory.join("stdout-closing-codex");
    std::fs::write(&executable, "#!/bin/sh\nexec 1>&-\nsleep 60\n")
        .expect("the stdout-closing fake CLI is written");
    let mut permissions = std::fs::metadata(&executable)
        .expect("the stdout-closing fake CLI has metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions)
        .expect("the stdout-closing fake CLI is executable");
    executable
}

fn rendered_request(prompt: &str) -> serde_json::Value {
    let request = prompt
        .rsplit_once("\n\n")
        .map(|(_, request)| request.trim())
        .expect("the adapter prompt ends with rendered request JSON");
    serde_json::from_str(request).expect("the adapter prompt request is JSON")
}

fn streamed_provider_text(observations: &[Observation<String>]) -> String {
    observations
        .iter()
        .filter_map(|observation| match &observation.fact {
            ObservationFact::TextDelta { text, .. }
            | ObservationFact::ThinkingDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn spawn_count(directory: &Path) -> usize {
    std::fs::read_to_string(directory.join("fake-codex-spawns"))
        .map(|content| content.lines().count())
        .unwrap_or_default()
}

fn read_optional(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(unix)]
fn assert_recorded_process_group_exited(path: std::path::PathBuf) {
    const PROCESS_EXIT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);

    let record = std::fs::read_to_string(path)
        .expect("the fake CLI records its process group and descendant identities");
    let raw_process_group = record
        .lines()
        .find_map(|line| line.strip_prefix("process_group="))
        .expect("the process-group record names the process group")
        .parse::<i32>()
        .expect("the recorded process-group identity is a process id");
    let process_group = rustix::process::Pid::from_raw(raw_process_group)
        .expect("the process-group identity is nonzero");
    let deadline = std::time::Instant::now() + PROCESS_EXIT_OBSERVATION_TIMEOUT;
    while process_group_exists(process_group) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_group_exists(process_group),
        "the recorded process group remains alive after cleanup"
    );
}

#[cfg(not(unix))]
fn assert_recorded_process_group_exited(_path: std::path::PathBuf) {}

#[cfg(unix)]
fn process_group_exists(process_group: rustix::process::Pid) -> bool {
    rustix::process::test_kill_process_group(process_group).is_ok()
        && process_group_has_live_member(process_group)
}

/// A host whose PID 1 does not promptly reap orphans can hold a fully killed
/// group as unreaped zombies that still accept the signal probe above; only a
/// member in a non-zombie state counts as alive.
#[cfg(target_os = "linux")]
fn process_group_has_live_member(process_group: rustix::process::Pid) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    entries.flatten().any(|entry| {
        std::fs::read_to_string(entry.path().join("stat")).is_ok_and(|stat| {
            proc_stat_process_group(&stat) == Some(process_group.as_raw_nonzero().get())
                && !proc_stat_is_zombie(&stat)
        })
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_group_has_live_member(_process_group: rustix::process::Pid) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn proc_stat_is_zombie(stat: &str) -> bool {
    stat.rsplit_once(") ")
        .is_some_and(|(_, fields)| fields.starts_with("Z "))
}

/// Reads the process-group field of one `/proc/<pid>/stat` line: the second
/// field after the parenthesized command, whose own closing parenthesis is
/// found from the right because the command may itself contain one.
#[cfg(target_os = "linux")]
fn proc_stat_process_group(stat: &str) -> Option<i32> {
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split(' ').nth(2)?.parse().ok()
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_stat_zombie_is_an_exited_process_state() {
    assert!(proc_stat_is_zombie("42 (sleep) Z 1 42 42 0"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_stat_running_is_not_an_exited_process_state() {
    assert!(!proc_stat_is_zombie("42 (sleep) S 1 42 42 0"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_stat_names_its_process_group() {
    assert_eq!(proc_stat_process_group("42 (sleep) Z 1 42 42 0"), Some(42));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_stat_group_parse_survives_a_parenthesized_command() {
    assert_eq!(
        proc_stat_process_group("42 (watch (x)) S 1 42 42 0"),
        Some(42)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_own_process_group_has_a_live_member() {
    assert!(process_group_has_live_member(rustix::process::getpgrp()));
}

fn cancel_after_record(path: std::path::PathBuf) -> CancellationSignal {
    let watcher = tokio::spawn(wait_for_record(path));
    CancellationSignal::when(async move {
        watcher
            .await
            .expect("the cancellation-record watcher completes");
    })
}

async fn wait_for_record(path: std::path::PathBuf) {
    tokio::time::timeout(OFFLINE_HARNESS_TIMEOUT, async {
        loop {
            if tokio::fs::read_to_string(&path)
                .await
                .map(|content| content.lines().count())
                .unwrap_or_default()
                > 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the fake CLI records the awaited marker");
}

fn completed(evidence: &TerminalEvidence) -> &signalbox_model_runtime::CompletionEvidence {
    let TerminalEvidence::Completed(completed) = evidence else {
        panic!("expected completed evidence, got {evidence:?}");
    };
    completed
}

fn provider_error(evidence: &TerminalEvidence) -> &signalbox_model_runtime::ProviderErrorEvidence {
    let TerminalEvidence::ProviderError(error) = evidence else {
        panic!("expected provider-error evidence, got {evidence:?}");
    };
    error
}

fn refused(evidence: &TerminalEvidence) -> &signalbox_model_runtime::RefusalEvidence {
    let TerminalEvidence::Refused(refusal) = evidence else {
        panic!("expected refusal evidence, got {evidence:?}");
    };
    refusal
}

fn proven_unsent(evidence: &TerminalEvidence) -> &signalbox_model_runtime::ProvenUnsentEvidence {
    let TerminalEvidence::ProvenUnsent(unsent) = evidence else {
        panic!("expected proven-unsent evidence, got {evidence:?}");
    };
    unsent
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsentKind {
    CancelledBeforeSend,
    ConnectFailed,
    SendIncompleteProvenUnacceptable,
}

fn unsent_kind(evidence: &TerminalEvidence) -> UnsentKind {
    match &proven_unsent(evidence).cause {
        signalbox_model_runtime::UnsentCause::CancelledBeforeSend => {
            UnsentKind::CancelledBeforeSend
        }
        signalbox_model_runtime::UnsentCause::ConnectFailed(_) => UnsentKind::ConnectFailed,
        signalbox_model_runtime::UnsentCause::SendIncompleteProvenUnacceptable(_) => {
            UnsentKind::SendIncompleteProvenUnacceptable
        }
    }
}

fn boundary_loss(evidence: &TerminalEvidence) -> &signalbox_model_runtime::BoundaryLossEvidence {
    let TerminalEvidence::BoundaryLoss(loss) = evidence else {
        panic!("expected boundary-loss evidence, got {evidence:?}");
    };
    loss
}

fn timed_out(cause: &LossCause) -> &signalbox_model_runtime::TransportFacts {
    let LossCause::TimedOut(facts) = cause else {
        panic!("expected timeout loss, got {cause:?}");
    };
    facts
}

fn transport_failed(cause: &LossCause) -> &signalbox_model_runtime::TransportFacts {
    let LossCause::TransportFailed(facts) = cause else {
        panic!("expected transport-failure loss, got {cause:?}");
    };
    facts
}

fn response_unintelligible(cause: &LossCause) -> &str {
    let LossCause::ResponseUnintelligible { detail } = cause else {
        panic!("expected unintelligible-response loss, got {cause:?}");
    };
    detail
}

fn tool_proposal(content: &[AssistantPart]) -> &signalbox_model_runtime::ToolCallProposal {
    let Some(AssistantPart::ToolCall(proposal)) = content.first() else {
        panic!("expected one leading tool proposal, got {content:?}");
    };
    proposal
}

fn observed_tool_arguments(observations: &[Observation<String>]) -> Option<&str> {
    observations
        .iter()
        .find_map(|observation| match &observation.fact {
            ObservationFact::ToolCallProposed(proposal) => Some(proposal.arguments_json.as_str()),
            _ => None,
        })
}

fn tool_ids(content: &[AssistantPart]) -> Vec<&str> {
    content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::ToolCall(proposal) => Some(proposal.id.as_str()),
            AssistantPart::Text(_)
            | AssistantPart::Thinking { .. }
            | AssistantPart::RedactedThinking { .. } => None,
        })
        .collect()
}

fn failed_preparation(
    outcome: PreparationOutcome<
        String,
        signalbox_model_runtime_codex_cli::CodexCliPreparedRequest<String>,
    >,
) -> PreparationFailure {
    let PreparationOutcome::Failed { failure, .. } = outcome else {
        panic!("expected failed preparation");
    };
    failure
}

fn unsupported_detail(failure: PreparationFailure) -> String {
    let PreparationFailure::UnsupportedOperation { detail } = failure else {
        panic!("expected unsupported-operation preparation failure");
    };
    detail
}
