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
    AssistantPart, CancellationSignal, CompletionFinish, CredentialReference, DeliveryMode,
    LossCause, ModelOperation, ModelRuntime, Observation, ObservationFact, PreparationOutcome,
    ProviderErrorKind, RequestedTarget, ResolvedTarget, StreamInterruption, TerminalEvidence,
    TokenUsage, ToolDefinition, decode_structured,
};
use signalbox_model_runtime_codex_cli::{CodexCliConfig, CodexCliRuntime};

#[path = "support/fixtures.rs"]
mod fixtures;

const CREDENTIAL_REFERENCE: &str = "codex-subscription-primary";
const RESOLVED_TARGET: &str = "gpt-offline-exact";

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
                index: 0,
                text: fixtures::STREAMED_ANSWER.to_string(),
            }
    }));
    assert_eq!(result.spawns, 1);
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

#[tokio::test]
async fn undecodable_event_fails_closed_as_unrecognized_provider_error() {
    assert_error_scenario("malformed_event", ProviderErrorKind::Unrecognized).await;
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
    let cancellation = cancel_after_spawn_record(temporary.path().join("fake-codex-spawns"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;
    let loss = boundary_loss(&report.evidence);

    assert_eq!(loss.cause, LossCause::CancellationRequested);
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
    assert!(diagnostic.contains("[redacted]"));
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
        Duration::from_secs(5),
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
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime_with_timeout(temporary.path(), fake_cli(), exchange_timeout);
    let prepared = prepare(&runtime, operation(scenario, delivery, shape)).await;
    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;
    let argv = read_optional(temporary.path().join("fake-codex-argv"));
    let prompt = read_optional(temporary.path().join("fake-codex-prompt"));

    ExecutionResult {
        evidence: report.evidence,
        observations,
        spawns: spawn_count(temporary.path()),
        argv,
        prompt,
    }
}

fn runtime(working_directory: &Path, executable: impl Into<std::path::PathBuf>) -> CodexCliRuntime {
    runtime_with_timeout(working_directory, executable, Duration::from_secs(5))
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

fn spawn_count(directory: &Path) -> usize {
    std::fs::read_to_string(directory.join("fake-codex-spawns"))
        .map(|content| content.lines().count())
        .unwrap_or_default()
}

fn read_optional(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn cancel_after_spawn_record(path: std::path::PathBuf) -> CancellationSignal {
    CancellationSignal::when(async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the fake CLI records its spawn before cancellation");
    })
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
