#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations"
)]

use std::path::Path;
use std::time::Duration;

use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, CredentialReference, DeliveryMode,
    FinishReason, LossCause, ModelOperation, ModelRuntime, PreparationDefect, PreparationOutcome,
    ProviderErrorKind, RequestedTarget, ResolvedTarget, TerminalEvidence, TokenUsage, ToolChoice,
    ToolDefinition, ToolName,
};
use signalbox_model_runtime_claude_cli::{
    ClaudeCliConfig, ClaudeCliPreparedRequest, ClaudeCliRuntime, DISABLED_CLAUDE_CLI_BUILTIN_TOOLS,
};

#[path = "support/fixtures.rs"]
mod fixtures;

const CREDENTIAL_REFERENCE: &str = "claude-subscription-synthetic";
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum OperationShape {
    Text,
    Tool,
    NamedTool,
}

struct ExecutionResult {
    evidence: TerminalEvidence,
    observations: Vec<signalbox_model_runtime::Observation<String>>,
    spawns: usize,
    argv: String,
}

#[tokio::test]
async fn normal_completion_requires_typed_terminal_result() {
    let result = execute_scenario("normal_completion", OperationShape::Text).await;
    let completion = completed(&result.evidence);

    assert_eq!(completion.finish, CompletionFinish::EndTurn);
    assert_eq!(
        completion.content,
        vec![AssistantPart::Text(fixtures::ANSWER.to_string())]
    );
    assert_eq!(completion.usage, expected_usage());
    assert_eq!(result.spawns, 1);
    assert!(
        result
            .argv
            .contains("--print\n--verbose\n--output-format=stream-json")
    );
    assert!(result.argv.contains("--setting-sources\n\n--settings"));
    assert!(result.argv.contains("--tools\n\n--allowedTools\n"));
    assert!(result.argv.contains(&disabled_tools_argument()));
}

#[tokio::test]
async fn harmless_terminal_credential_prefix_remains_byte_exact() {
    let result = execute_scenario("safe_terminal_prefix", OperationShape::Text).await;

    assert_eq!(
        completion_text(&result.evidence),
        fixtures::SAFE_CREDENTIAL_PREFIX
    );
    assert_eq!(
        observation_text(&result.observations),
        fixtures::SAFE_CREDENTIAL_PREFIX
    );
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn tool_call_and_mcp_result_round_trip_returns_typed_proposal() {
    let result = execute_scenario("tool_round_trip", OperationShape::Tool).await;
    let completion = completed(&result.evidence);
    let proposal = tool_call(&completion.content);

    assert_eq!(completion.finish, CompletionFinish::ToolUse);
    assert_eq!(proposal.id.as_str(), fixtures::TOOL_ID);
    assert_eq!(proposal.name.as_str(), fixtures::TOOL_NAME);
    assert_eq!(proposal.arguments_json, fixtures::TOOL_ARGUMENTS);
    assert!(
        result
            .argv
            .contains("--allowedTools\nmcp__signalbox_tools__synthetic_lookup")
    );
    assert!(result.observations.iter().any(|observation| matches!(
        observation.fact,
        signalbox_model_runtime::ObservationFact::ToolCallProposed(_)
    )));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn named_tool_choice_rejects_an_extra_declared_proposal() {
    let result = execute_scenario("named_choice_extra_tool", OperationShape::NamedTool).await;
    let loss = boundary_loss(&result.evidence);

    assert!(response_unintelligible(&loss.cause).contains(fixtures::TOOL_NAME));
    assert_eq!(loss.finish_reported, Some(FinishReason::ToolUse));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn refusal_requires_the_typed_refusal_stop_reason() {
    let result = execute_scenario("refusal", OperationShape::Text).await;
    let refusal = refused(&result.evidence);

    assert_eq!(
        refusal.content,
        vec![AssistantPart::Text(fixtures::REFUSAL.to_string())]
    );
    assert_eq!(refusal.usage, expected_usage());
    assert_eq!(result.spawns, 1);
}
#[tokio::test]
async fn credential_shaped_cli_text_is_redacted_from_all_evidence() {
    let result = execute_scenario("credential_redaction", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_TEXT));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn fragmented_credential_is_redacted_from_observations_and_terminal_content() {
    let result = execute_scenario("fragmented_credential_redaction", OperationShape::Text).await;
    let observed = observation_text(&result.observations);
    let terminal = completion_text(&result.evidence);

    assert!(!observed.contains(fixtures::FRAGMENTED_SECRET));
    assert!(!terminal.contains(fixtures::FRAGMENTED_SECRET));
    assert!(observed.contains("[redacted]"));
    assert!(terminal.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn control_sequence_cannot_obfuscate_a_streamed_credential() {
    let result = execute_scenario(
        "control_sequence_credential_redaction",
        OperationShape::Text,
    )
    .await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::CONTROL_SEQUENCE_SECRET));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn redacted_provider_identities_still_compare_by_native_value() {
    let result = execute_scenario("redacted_native_identity", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert_eq!(
        completed(&result.evidence).finish,
        CompletionFinish::EndTurn
    );
    assert!(!diagnostic.contains(fixtures::CREDENTIAL_SHAPED_SESSION_ID));
    assert!(!diagnostic.contains(fixtures::CREDENTIAL_SHAPED_MODEL));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn model_identifier_prefix_is_held_into_the_first_text_block() {
    let result = execute_scenario("model_prefix_redaction", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::MODEL_CREDENTIAL_CONTINUATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn credential_shaped_tool_ids_receive_distinct_safe_surrogates() {
    let result = execute_scenario("redacted_tool_ids", OperationShape::Tool).await;
    let completion = completed(&result.evidence);
    let (first, second) = two_tool_calls(&completion.content);
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert_eq!(completion.finish, CompletionFinish::ToolUse);
    assert_ne!(first.id, second.id);
    assert!(!diagnostic.contains(fixtures::CREDENTIAL_TOOL_ID_ONE));
    assert!(!diagnostic.contains(fixtures::CREDENTIAL_TOOL_ID_TWO));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn refusal_precedes_the_success_only_named_tool_requirement() {
    let result = execute_scenario("refusal", OperationShape::NamedTool).await;
    let refusal = refused(&result.evidence);

    assert_eq!(
        refusal.content,
        vec![AssistantPart::Text(fixtures::REFUSAL.to_string())]
    );
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn tool_proposal_with_end_turn_is_protocol_boundary_loss() {
    let result = execute_scenario("tool_with_end_turn", OperationShape::Tool).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::ResponseUnintelligible { .. }
    ));
    assert_eq!(loss.finish_reported, Some(FinishReason::EndTurn));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn tool_use_stop_without_a_proposal_is_protocol_boundary_loss() {
    let result = execute_scenario("text_with_tool_use", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::ResponseUnintelligible { .. }
    ));
    assert_eq!(loss.finish_reported, Some(FinishReason::ToolUse));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn success_rejects_every_contradictory_error_field_shape() {
    let errors = execute_scenario("success_with_errors", OperationShape::Text).await;
    let status = execute_scenario("success_with_api_status", OperationShape::Text).await;

    assert!(matches!(
        boundary_loss(&errors.evidence).cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert!(matches!(
        boundary_loss(&status.evidence).cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(errors.spawns, 1);
    assert_eq!(status.spawns, 1);
}

#[tokio::test]
async fn nonzero_exit_is_a_typed_provider_failure() {
    let result = execute_scenario("process_nonzero", OperationShape::Text).await;
    let failure = provider_error(&result.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::CredentialRejected);
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn api_error_status_classifies_a_generic_terminal_error() {
    let result = execute_scenario("api_status_error", OperationShape::Text).await;
    let failure = provider_error(&result.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::RateLimited);
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn truncated_stream_is_boundary_loss() {
    let result = execute_scenario("truncated_stream", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamEndedWithoutTerminalMarker { .. }
    ));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn malformed_stream_line_is_protocol_boundary_loss() {
    let result = execute_scenario("malformed_stream", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn conflicting_assistant_message_id_is_protocol_boundary_loss() {
    let result = execute_scenario("conflicting_message_id", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn success_without_stop_reason_is_protocol_boundary_loss() {
    let result = execute_scenario("success_without_stop_reason", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn duplicate_stream_member_is_protocol_boundary_loss() {
    let result = execute_scenario("duplicate_stream_member", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(stream_protocol_detail(&loss.cause).contains(fixtures::DUPLICATE_MEMBER_DETAIL));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn process_spawn_failure_is_proven_unsent() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), Path::new("/synthetic/missing/claude"));
    let prepared = prepare(
        &runtime,
        operation("normal_completion", OperationShape::Text),
    )
    .await;
    let report = runtime
        .execute(prepared, &mut Vec::new(), CancellationSignal::never())
        .await;

    assert!(matches!(report.evidence, TerminalEvidence::ProvenUnsent(_)));
    assert_eq!(spawn_count(temporary.path()), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn non_utf8_bridge_path_is_a_preparation_defect_before_spawn() {
    use std::os::unix::ffi::OsStringExt;

    let temporary = tempfile::tempdir().expect("test working directory is created");
    let bridge = temporary
        .path()
        .join(std::ffi::OsString::from_vec(vec![b'm', b'c', b'p', 0xff]));
    let config = ClaudeCliConfig::new(
        fake_cli(),
        bridge,
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );
    let runtime = ClaudeCliRuntime::new(config).expect("runtime accepts an absolute bridge path");
    let outcome = runtime
        .prepare(
            operation("normal_completion", OperationShape::Text),
            CancellationSignal::never(),
        )
        .await;

    assert!(request_construction_defect(outcome).contains("UTF-8"));
    assert_eq!(spawn_count(temporary.path()), 0);
}

async fn execute_scenario(scenario: &str, shape: OperationShape) -> ExecutionResult {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), &fake_cli());
    let prepared = prepare(&runtime, operation(scenario, shape)).await;
    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    ExecutionResult {
        evidence: report.evidence,
        observations,
        spawns: spawn_count(temporary.path()),
        argv: std::fs::read_to_string(temporary.path().join("fake-claude-argv"))
            .unwrap_or_default(),
    }
}

fn runtime(working_directory: &Path, executable: &Path) -> ClaudeCliRuntime {
    let mut config = ClaudeCliConfig::new(
        executable,
        bridge_cli(),
        working_directory,
        CredentialReference::new(CREDENTIAL_REFERENCE),
    );
    config.exchange_timeout = OFFLINE_TIMEOUT;
    config.interrupt_grace = Duration::from_millis(100);
    ClaudeCliRuntime::new(config).expect("offline runtime configuration is valid")
}

fn operation(scenario: &str, shape: OperationShape) -> ModelOperation<String> {
    let mut operation = ModelOperation::new(
        scenario.to_string(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
        RequestedTarget::new("synthetic-selection"),
        ResolvedTarget::new(fixtures::MODEL),
        vec![signalbox_model_runtime::ConversationMessage::user_text(
            scenario,
        )],
        signalbox_model_runtime::ModelSettings::new(256),
    );
    operation.delivery = DeliveryMode::Streamed;
    if matches!(shape, OperationShape::Tool | OperationShape::NamedTool) {
        operation.tools = vec![ToolDefinition::with_schema(
            fixtures::TOOL_NAME,
            "Synthetic lookup",
            serde_json::json!({
                "type": "object",
                "properties": {"subject": {"type": "string"}},
                "required": ["subject"]
            }),
        )];
    }
    if matches!(shape, OperationShape::NamedTool) {
        operation.tools.push(ToolDefinition::with_schema(
            fixtures::OTHER_TOOL_NAME,
            "Synthetic other tool",
            serde_json::json!({
                "type": "object",
                "properties": {"subject": {"type": "string"}},
                "required": ["subject"]
            }),
        ));
        operation.tool_choice = ToolChoice::Named(ToolName::new(fixtures::TOOL_NAME));
    }
    operation
}

async fn prepare(
    runtime: &ClaudeCliRuntime,
    operation: ModelOperation<String>,
) -> ClaudeCliPreparedRequest<String> {
    match runtime
        .prepare(operation, CancellationSignal::never())
        .await
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        PreparationOutcome::Cancelled { .. } => panic!("offline preparation was cancelled"),
        PreparationOutcome::Failed { failure, .. } => {
            panic!("offline preparation failed: {failure:?}")
        }
        PreparationOutcome::Defect { defect, .. } => {
            panic!("offline preparation found a defect: {defect:?}")
        }
    }
}

fn request_construction_defect(
    outcome: PreparationOutcome<String, ClaudeCliPreparedRequest<String>>,
) -> String {
    match outcome {
        PreparationOutcome::Defect {
            defect: PreparationDefect::RequestConstructionFailed { detail },
            ..
        } => detail,
        _ => panic!("expected request-construction defect"),
    }
}

fn expected_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(fixtures::INPUT_TOKENS),
        output_tokens: Some(fixtures::OUTPUT_TOKENS),
        cache_creation_input_tokens: Some(fixtures::CACHE_CREATION_TOKENS),
        cache_read_input_tokens: Some(fixtures::CACHE_READ_TOKENS),
    }
}

fn completed(evidence: &TerminalEvidence) -> &signalbox_model_runtime::CompletionEvidence {
    let TerminalEvidence::Completed(value) = evidence else {
        panic!("expected completion, got {evidence:?}")
    };
    value
}

fn refused(evidence: &TerminalEvidence) -> &signalbox_model_runtime::RefusalEvidence {
    let TerminalEvidence::Refused(value) = evidence else {
        panic!("expected refusal, got {evidence:?}")
    };
    value
}

fn provider_error(evidence: &TerminalEvidence) -> &signalbox_model_runtime::ProviderErrorEvidence {
    let TerminalEvidence::ProviderError(value) = evidence else {
        panic!("expected provider error, got {evidence:?}")
    };
    value
}

fn boundary_loss(evidence: &TerminalEvidence) -> &signalbox_model_runtime::BoundaryLossEvidence {
    let TerminalEvidence::BoundaryLoss(value) = evidence else {
        panic!("expected boundary loss, got {evidence:?}")
    };
    value
}

fn response_unintelligible(cause: &LossCause) -> &str {
    let LossCause::ResponseUnintelligible { detail } = cause else {
        panic!("expected unintelligible response, got {cause:?}")
    };
    detail
}

fn stream_protocol_detail(cause: &LossCause) -> &str {
    let LossCause::StreamProtocolViolation { detail } = cause else {
        panic!("expected stream protocol violation, got {cause:?}")
    };
    detail
}

fn observation_text(observations: &[signalbox_model_runtime::Observation<String>]) -> String {
    observations
        .iter()
        .filter_map(|observation| match &observation.fact {
            signalbox_model_runtime::ObservationFact::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn completion_text(evidence: &TerminalEvidence) -> String {
    completed(evidence)
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn two_tool_calls(
    content: &[AssistantPart],
) -> (
    &signalbox_model_runtime::ToolCallProposal,
    &signalbox_model_runtime::ToolCallProposal,
) {
    let [
        AssistantPart::ToolCall(first),
        AssistantPart::ToolCall(second),
    ] = content
    else {
        panic!("expected two tool calls, got {content:?}")
    };
    (first, second)
}

fn tool_call(content: &[AssistantPart]) -> &signalbox_model_runtime::ToolCallProposal {
    let [AssistantPart::ToolCall(value)] = content else {
        panic!("expected one tool call, got {content:?}")
    };
    value
}

fn disabled_tools_argument() -> String {
    format!(
        "--disallowedTools\n{}",
        DISABLED_CLAUDE_CLI_BUILTIN_TOOLS.join(",")
    )
}

fn spawn_count(directory: &Path) -> usize {
    std::fs::read_to_string(directory.join("fake-claude-spawns"))
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn fake_cli() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_signalbox-fake-claude-cli"))
}

fn bridge_cli() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_signalbox-claude-mcp-bridge"))
}
