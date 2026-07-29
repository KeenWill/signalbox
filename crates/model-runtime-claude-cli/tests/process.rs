#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations"
)]

use std::path::Path;
use std::time::Duration;

use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, CredentialReference, DeliveryMode,
    LossCause, ModelOperation, ModelRuntime, PreparationOutcome, ProviderErrorKind,
    RequestedTarget, ResolvedTarget, TerminalEvidence, TokenUsage, ToolDefinition,
};
use signalbox_model_runtime_claude_cli::{
    ClaudeCliConfig, ClaudeCliPreparedRequest, ClaudeCliRuntime, DISABLED_CLAUDE_CLI_BUILTIN_TOOLS,
};

#[allow(dead_code)]
#[path = "support/fixtures.rs"]
mod fixtures;

const CREDENTIAL_REFERENCE: &str = "claude-subscription-synthetic";
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(30);

enum OperationShape {
    Text,
    Tool,
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
async fn nonzero_exit_is_a_typed_provider_failure() {
    let result = execute_scenario("process_nonzero", OperationShape::Text).await;
    let failure = provider_error(&result.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::CredentialRejected);
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
async fn duplicate_stream_member_is_protocol_boundary_loss() {
    let result = execute_scenario("duplicate_stream_member", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
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
    if matches!(shape, OperationShape::Tool) {
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
