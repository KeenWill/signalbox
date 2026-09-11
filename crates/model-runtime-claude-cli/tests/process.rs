#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations"
)]

use std::path::Path;
use std::time::Duration;

use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, CredentialAccess, CredentialAccessError,
    CredentialAccessFailure, CredentialReference, CredentialValue, DeliveryMode, FinishReason,
    LossCause, ModelOperation, ModelRuntime, PreparationDefect, PreparationFailure,
    PreparationOutcome, ProviderErrorKind, RequestedTarget, ResolvedTarget, TerminalEvidence,
    TokenUsage, ToolCallsAtLoss, ToolChoice, ToolDefinition, ToolName,
};
use signalbox_model_runtime_claude_cli::{
    CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY, ClaudeCliConfig, ClaudeCliConstructionError,
    ClaudeCliPreparedRequest, ClaudeCliRuntime, DISABLED_CLAUDE_CLI_BUILTIN_TOOLS,
};
use signalbox_test_bin::test_bin_path;

#[path = "support/fixtures.rs"]
mod fixtures;

const CREDENTIAL_REFERENCE: &str = "claude-subscription-synthetic";
const CURRENT_CREDENTIAL_REFERENCE: &str = "claude-current-synthetic";
const HISTORICAL_CREDENTIAL_REFERENCE: &str = "claude-historical-synthetic";
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
    history: String,
    prompt: String,
    history_mode: String,
}

#[derive(Clone)]
struct SyntheticCredentialAccess {
    reference: CredentialReference,
    value: CredentialValue,
}

impl CredentialAccess for SyntheticCredentialAccess {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        if reference == &self.reference {
            Ok(self.value.clone())
        } else {
            Err(CredentialAccessError::new(
                reference.clone(),
                CredentialAccessFailure::Unmapped,
            ))
        }
    }
}

#[test]
fn cancellation_finishes_preparation_while_support_io_is_queued() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("test runtime builds");
    let (release, hold) = std::sync::mpsc::channel();
    let blocker = executor.spawn_blocking(move || hold.recv());
    let temporary = tempfile::tempdir().expect("test directory exists");
    let adapter = runtime(temporary.path(), &fake_cli());
    let outcome = executor.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(2),
            adapter.prepare(
                operation("normal_completion", OperationShape::Text),
                CancellationSignal::already_cancelled(),
            ),
        )
        .await
    });
    release.send(()).expect("release blocking worker");
    executor
        .block_on(blocker)
        .expect("worker joins")
        .expect("worker released");
    assert!(matches!(outcome, Ok(PreparationOutcome::Cancelled { .. })));
    assert_eq!(spawn_count(temporary.path()), 0);
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
async fn native_history_restores_distinct_assistant_groups_without_replaying_tools() {
    use signalbox_model_runtime::{
        ConversationMessage, ConversationRole, MessagePart, ToolCallId, ToolCallProposal,
        ToolResultRecord,
    };
    let mut request = operation("normal_completion", OperationShape::Text);
    request.messages.extend([
        ConversationMessage {
            role: ConversationRole::Assistant,
            parts: vec![MessagePart::ToolCall(ToolCallProposal {
                id: ToolCallId::new(fixtures::TOOL_ID),
                name: ToolName::new(fixtures::TOOL_NAME),
                arguments_json: fixtures::NONCANONICAL_TOOL_ARGUMENTS.into(),
            })],
        },
        ConversationMessage {
            role: ConversationRole::User,
            parts: vec![MessagePart::ToolResult(ToolResultRecord {
                tool_call_id: ToolCallId::new(fixtures::TOOL_ID),
                content: fixtures::ANSWER.into(),
                is_error: false,
            })],
        },
        ConversationMessage::assistant_text(fixtures::ANSWER),
        ConversationMessage::user_text(fixtures::OTHER_MESSAGE_ID),
    ]);
    let expected_messages = request.messages.len();
    let result = execute_operation(request).await;
    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    let rows: Vec<serde_json::Value> = result
        .history
        .lines()
        .map(|line| serde_json::from_str(line).expect("native history row is JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        expected_messages,
        "all canonical messages reach native history"
    );
    assert_eq!(rows[0]["parentUuid"], serde_json::Value::Null);
    for pair in rows.windows(2) {
        assert_eq!(
            pair[1]["parentUuid"], pair[0]["uuid"],
            "the parent chain preserves canonical order"
        );
    }
    assert_eq!(rows[1]["type"], "assistant");
    assert_eq!(rows[1]["message"]["role"], "assistant");
    assert_eq!(rows[3]["type"], "assistant");
    assert_eq!(rows[3]["message"]["role"], "assistant");
    assert_ne!(
        rows[1]["message"]["id"], rows[3]["message"]["id"],
        "distinct assistant messages form native compaction groups"
    );
    assert_eq!(
        rows[1]["message"]["content"][0]["type"], "text",
        "historical tool calls are context, not native tool invocations"
    );
    assert_eq!(
        rows[2]["message"]["content"][0]["type"], "text",
        "historical results are canonical context"
    );
    let call_text = rows[1]["message"]["content"][0]["text"]
        .as_str()
        .expect("canonical call text");
    assert!(
        call_text.contains(fixtures::NONCANONICAL_TOOL_ARGUMENTS),
        "raw historical arguments retain their exact JSON text"
    );
    let prior_result: serde_json::Value = serde_json::from_str(
        rows[2]["message"]["content"][0]["text"]
            .as_str()
            .expect("canonical result text"),
    )
    .expect("canonical result is JSON");
    assert_eq!(prior_result["parts"][0]["tool_call_id"], fixtures::TOOL_ID);
    assert_eq!(prior_result["parts"][0]["content"], fixtures::ANSWER);
    let controls: serde_json::Value = serde_json::from_str(
        result
            .prompt
            .split_once("\n\n")
            .expect("request controls follow instructions")
            .1,
    )
    .expect("controls are JSON");
    assert!(
        controls.get("messages").is_none(),
        "history is not duplicated in the control prompt"
    );
}

#[tokio::test]
async fn system_only_request_starts_without_an_empty_resume_file() {
    let mut request = operation("normal_completion", OperationShape::Text);
    request.messages.clear();
    request.system = Some("normal_completion".into());
    let result = execute_operation(request).await;
    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    assert!(!result.argv.lines().any(|argument| argument == "--resume"));
    let controls: serde_json::Value = serde_json::from_str(
        result
            .prompt
            .split_once("\n\n")
            .expect("request controls")
            .1,
    )
    .expect("controls are JSON");
    assert_eq!(controls["system"], "normal_completion");
}

#[tokio::test]
async fn native_history_is_private_and_removed_after_execution() {
    let result = execute_scenario("normal_completion", OperationShape::Text).await;
    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    let arguments: Vec<_> = result.argv.lines().collect();
    let history_path = arguments
        .windows(2)
        .find(|pair| pair[0] == "--resume")
        .expect("native resume is explicit")[1];
    assert!(arguments.contains(&"--fork-session"));
    assert!(arguments.contains(&"--no-session-persistence"));
    #[cfg(unix)]
    assert_eq!(
        result.history_mode, "600",
        "native context is private to the daemon user"
    );
    assert!(
        !Path::new(history_path).exists(),
        "the disposable context is removed when execution ends"
    );
}

#[tokio::test]
async fn native_compaction_boundary_preserves_the_completion() {
    let result = execute_scenario("native_compaction", OperationShape::Text).await;

    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn native_compaction_boundary_rejects_a_different_session() {
    let result = execute_scenario("native_compaction_wrong_session", OperationShape::Text).await;

    assert!(matches!(
        boundary_loss(&result.evidence).cause,
        LossCause::StreamProtocolViolation { .. }
    ));
}

#[tokio::test]
async fn nonterminal_system_events_do_not_mask_the_initialized_exchange() {
    let result = execute_scenario("nonterminal_system_events", OperationShape::Text).await;

    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    assert_eq!(result.spawns, 1);
}

/// A lifecycle `session_id` is dropped as a repeated identity only where it is
/// one. A differing value contradicts the correlation `system/init`
/// established, exactly as it does on a `result` event.
#[tokio::test]
async fn lifecycle_session_contradicting_init_is_a_protocol_violation() {
    let result = execute_scenario("lifecycle_session_contradicts_init", OperationShape::Text).await;

    assert!(matches!(
        boundary_loss(&result.evidence).cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn assistant_resolved_model_may_differ_from_the_selected_init_alias() {
    let result = execute_scenario("resolved_assistant_model", OperationShape::Text).await;

    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    assert_eq!(result.spawns, 1);
}

/// Repeated model metadata preserves the assistant text.
#[tokio::test]
async fn repeated_resolved_model_preserves_assistant_text() {
    let result = execute_scenario("repeated_model_prefix_release", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(diagnostic.contains(fixtures::MODEL_MARKER_RELEASED_TAIL));
    assert!(!diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn assistant_model_must_remain_stable_after_its_first_event() {
    let result = execute_scenario("conflicting_assistant_model", OperationShape::Text).await;

    assert!(matches!(
        boundary_loss(&result.evidence).cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn file_delivery_materializes_private_claude_settings_without_direct_child_key() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))
            .expect("the test working directory is user-accessible under any process umask");
    }
    let runtime = file_delivery_runtime(temporary.path(), fixtures::FILE_DELIVERED_CREDENTIAL);
    let prepared = prepare(
        &runtime,
        operation("file_credential_redaction", OperationShape::Text),
    )
    .await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    let _completion = completed(&report.evidence);

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(temporary.path().join("fake-claude-settings"))
            .expect("the fake CLI recorded its explicit settings"),
    )
    .expect("the explicit settings are JSON");
    assert!(settings.get("env").is_none());
    let helper = settings["apiKeyHelper"]
        .as_str()
        .expect("file delivery configures an API-key helper");
    assert!(helper.starts_with("exec /bin/sh '/"));
    assert!(helper.ends_with("/credential-helper'"));
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("fake-claude-helper-credential"))
            .expect("the fake CLI invoked the configured API-key helper"),
        fixtures::FILE_DELIVERED_CREDENTIAL
    );
    assert_eq!(
        std::fs::read_to_string(
            temporary
                .path()
                .join("fake-claude-direct-credential-present")
        )
        .expect("the fake CLI recorded direct-key presence"),
        "false"
    );
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("fake-claude-settings-mode"))
            .expect("the fake CLI recorded its settings mode"),
        "600"
    );
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("fake-claude-credential-mode"))
            .expect("the fake CLI recorded its credential mode"),
        "600"
    );
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("fake-claude-helper-mode"))
            .expect("the fake CLI recorded its credential-helper mode"),
        "600"
    );
    let argv = std::fs::read_to_string(temporary.path().join("fake-claude-argv"))
        .expect("the fake CLI recorded its argument vector");
    let settings_path = recorded_argument(&argv, "--settings");
    let config_directory = std::fs::read_to_string(temporary.path().join("fake-claude-config-dir"))
        .expect("the fake CLI recorded its config directory");
    assert_eq!(
        Path::new(&config_directory),
        Path::new(settings_path)
            .parent()
            .expect("the private settings have a containing directory")
    );
    assert!(
        !format!("{:?}{:?}", report.evidence, observations)
            .contains(fixtures::FILE_DELIVERED_CREDENTIAL)
    );
}

#[tokio::test]
async fn file_delivery_resolves_a_historical_operation_pin_from_the_complete_catalog() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let historical_reference = CredentialReference::new(HISTORICAL_CREDENTIAL_REFERENCE);
    let mut config = ClaudeCliConfig::new(
        fake_cli(),
        bridge_cli(),
        temporary.path(),
        CredentialReference::new(CURRENT_CREDENTIAL_REFERENCE),
        None,
        None,
    );
    config.exchange_timeout = Some(OFFLINE_TIMEOUT);
    config.interrupt_grace = Duration::from_millis(100);
    let runtime = ClaudeCliRuntime::new_with_credential_catalog(
        config,
        SyntheticCredentialAccess {
            reference: historical_reference.clone(),
            value: CredentialValue::new(fixtures::FILE_DELIVERED_CREDENTIAL.as_bytes().to_vec()),
        },
        None,
        CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY,
    )
    .expect("the complete file catalog is valid");
    let mut historical_operation = operation("file_credential_redaction", OperationShape::Text);
    historical_operation.credential_reference = historical_reference;
    let prepared = prepare(&runtime, historical_operation).await;
    let mut observations = Vec::new();

    let _report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    assert_eq!(spawn_count(temporary.path()), 1);
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("fake-claude-helper-credential"))
            .expect("the fake CLI invoked the historical credential helper"),
        fixtures::FILE_DELIVERED_CREDENTIAL
    );
}

#[test]
fn file_delivery_rejects_any_other_environment_key() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let result = file_delivery_runtime_result(temporary.path(), "PATH", "synthetic-value");

    assert_eq!(
        construction_error(result),
        ClaudeCliConstructionError::InvalidCredentialEnvironmentKey
    );
}

#[tokio::test]
async fn file_delivery_rejects_an_empty_credential_before_spawn() {
    assert_unusable_file_credential(Vec::new()).await;
}

#[tokio::test]
async fn file_delivery_rejects_a_non_utf8_credential_before_spawn() {
    assert_unusable_file_credential(vec![0xff]).await;
}

#[tokio::test]
async fn file_delivery_rejects_a_nul_bearing_credential_before_spawn() {
    assert_unusable_file_credential(b"synthetic\0value".to_vec()).await;
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
async fn tool_acknowledgement_tail_returns_only_the_original_proposal() {
    let result = execute_scenario("tool_acknowledgement_tail", OperationShape::Tool).await;
    let completion = completed(&result.evidence);
    let proposal = tool_call(&completion.content);

    assert_eq!(completion.finish, CompletionFinish::ToolUse);
    assert_eq!(
        completion.message_id.as_ref().map(|id| id.as_str()),
        Some(fixtures::MESSAGE_ID)
    );
    assert_eq!(completion.content.len(), 1);
    assert_eq!(proposal.id.as_str(), fixtures::TOOL_ID);
    assert_eq!(proposal.arguments_json, fixtures::TOOL_ARGUMENTS);
    assert_eq!(observation_text(&result.observations), "");
    assert_eq!(completion.usage, expected_usage());
    let finishes = result
        .observations
        .iter()
        .filter_map(|observation| match &observation.fact {
            signalbox_model_runtime::ObservationFact::FinishReported(reason) => {
                Some(reason.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finishes, vec![FinishReason::EndTurn]);
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn tool_acknowledgement_discards_thinking_before_its_text() {
    let result = execute_scenario(
        "tool_acknowledgement_thinking_then_text",
        OperationShape::Tool,
    )
    .await;
    let completion = completed(&result.evidence);
    assert_eq!(completion.finish, CompletionFinish::ToolUse);
    assert_eq!(completion.content.len(), 1);
    assert_eq!(
        tool_call(&completion.content).id.as_str(),
        fixtures::TOOL_ID
    );
    assert_eq!(
        completion.message_id.as_ref().map(|id| id.as_str()),
        Some(fixtures::MESSAGE_ID)
    );
    assert!(!result.observations.iter().any(|observation| matches!(
        observation.fact,
        signalbox_model_runtime::ObservationFact::ThinkingDelta { .. }
            | signalbox_model_runtime::ObservationFact::TextDelta { .. }
    )));
    assert_eq!(completion.usage, expected_usage());
}

#[tokio::test]
async fn tool_acknowledgement_tail_rejects_incomplete_or_conflicting_evidence() {
    for scenario in [
        "tool_acknowledgement_thinking_only",
        "tool_acknowledgement_tail_without_result",
        "tool_acknowledgement_tail_reports_tool_use",
        "tool_acknowledgement_tail_is_empty",
        "tool_acknowledgement_tail_changes_model",
        "tool_acknowledgement_tail_proposes_tool",
    ] {
        let result = execute_scenario(scenario, OperationShape::Tool).await;
        assert!(
            matches!(
                boundary_loss(&result.evidence).cause,
                LossCause::StreamProtocolViolation { .. }
            ),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn tool_arguments_preserve_the_provider_json_lexeme() {
    let result = execute_scenario("noncanonical_tool_arguments", OperationShape::Tool).await;
    let completion = completed(&result.evidence);
    let proposal = tool_call(&completion.content);

    assert_eq!(
        proposal.arguments_json,
        fixtures::NONCANONICAL_TOOL_ARGUMENTS
    );
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn tool_arguments_preserve_reserved_number_key_objects() {
    let result = execute_scenario("reserved_key_tool_arguments", OperationShape::Tool).await;

    assert_eq!(
        tool_call(&completed(&result.evidence).content).arguments_json,
        fixtures::RESERVED_KEY_TOOL_ARGUMENTS
    );
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
async fn a_native_refusal_notice_allows_the_terminal_refusal() {
    let result = execute_scenario("refusal_notice", OperationShape::Text).await;
    let refusal = refused(&result.evidence);

    assert_eq!(
        refusal.content,
        vec![AssistantPart::Text(fixtures::REFUSAL.to_string())]
    );
    assert_eq!(refusal.usage, expected_usage());
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn a_native_refusal_notice_without_a_result_is_incomplete() {
    let result = execute_scenario("refusal_notice_without_result", OperationShape::Text).await;
    assert!(matches!(
        boundary_loss(&result.evidence).cause,
        LossCause::StreamEndedWithoutTerminalMarker { .. }
    ));
}

#[tokio::test]
async fn a_native_refusal_notice_must_match_the_initialized_session() {
    let result = execute_scenario("refusal_notice_wrong_session", OperationShape::Text).await;
    assert!(matches!(
        boundary_loss(&result.evidence).cause,
        LossCause::StreamProtocolViolation { .. }
    ));
}

#[tokio::test]
async fn refusal_requires_the_typed_refusal_stop_reason() {
    let result = execute_scenario("refusal", OperationShape::Text).await;
    let refusal = refused(&result.evidence);

    assert_eq!(
        refused(&result.evidence).reason,
        signalbox_model_runtime::RefusalReason::Unspecified
    );
    assert_eq!(
        refusal.content,
        vec![AssistantPart::Text(fixtures::REFUSAL.to_string())]
    );
    assert_eq!(refusal.usage, expected_usage());
    assert_eq!(result.spawns, 1);
}

/// Generic structured errors retain usage without classifying stderr prose.
#[tokio::test]
async fn generic_terminal_error_retains_usage_without_classifying_stderr() {
    let result = execute_scenario(
        "generic_error_then_definitive_stderr_exit",
        OperationShape::Text,
    )
    .await;
    let failure = provider_error(&result.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::Unrecognized);
    assert_eq!(reported_usage(&result.observations), vec![failure.usage]);
    assert_eq!(result.spawns, 1);
}

/// Usage is a provider fact stated in the `result` event, so it is observed
/// when that event is processed — ahead of the finish fact drawn from the same
/// event, and exactly once across the whole terminal path.
#[tokio::test]
async fn reported_usage_precedes_the_finish_fact_from_the_same_result() {
    let result = execute_scenario("normal_completion", OperationShape::Text).await;

    assert_eq!(reported_usage(&result.observations), vec![expected_usage()]);
    assert!(
        observation_kinds(&result.observations)
            .iter()
            .position(|kind| *kind == "UsageReported")
            < observation_kinds(&result.observations)
                .iter()
                .position(|kind| *kind == "FinishReported")
    );
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
async fn native_stdin_error_without_a_status_stays_unrecognized() {
    let result = execute_scenario("piped_stdin_too_large", OperationShape::Text).await;
    let TerminalEvidence::ProviderError(failure) = result.evidence else {
        panic!("native stdin rejection must be a typed provider failure");
    };
    assert_eq!(failure.kind, ProviderErrorKind::Unrecognized);
    assert_eq!(failure.usage, TokenUsage::unreported());
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn generic_native_prompt_error_stays_unrecognized() {
    let result = execute_scenario("native_prompt_too_large", OperationShape::Text).await;
    let failure = provider_error(&result.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::Unrecognized);
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn nonzero_exit_is_a_typed_provider_failure() {
    let result = execute_scenario("process_nonzero", OperationShape::Text).await;
    let failure = provider_error(&result.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::Unrecognized);
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn terminal_classification_uses_native_subtypes_and_status_without_prose() {
    for (scenario, expected, token) in [
        (
            "session_window_status",
            ProviderErrorKind::RateLimited,
            "success",
        ),
        (
            "session_window_without_status",
            ProviderErrorKind::Unrecognized,
            "success",
        ),
        (
            "request_size_status",
            ProviderErrorKind::RequestTooLarge,
            "success",
        ),
        (
            "native_budget_subtype",
            ProviderErrorKind::QuotaExhausted,
            "error_max_budget_usd",
        ),
        (
            "unknown_error_subtype",
            ProviderErrorKind::Unrecognized,
            "synthetic_unknown_error",
        ),
    ] {
        let result = execute_scenario(scenario, OperationShape::Text).await;
        let failure = provider_error(&result.evidence);
        assert_eq!(failure.kind, expected, "{scenario}");
        assert_eq!(
            failure.native.error_token.as_deref(),
            Some(token),
            "{scenario}"
        );
        assert_eq!(result.spawns, 1);
    }
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
    assert!(loss.response_content_observed);

    assert!(matches!(
        loss.cause,
        LossCause::StreamEndedWithoutTerminalMarker { .. }
    ));
    assert_eq!(result.spawns, 1);
}

/// a tool call the decoder rejects before registering it is still
/// reported as opened.
///
/// The CLI announces a `tool_use` block for a tool this operation never
/// declared. The decoder refuses it before `proposal_indexes` or any
/// observation records it, so the loss evidence is the only place the fact can
/// survive. The refusal is itself a decode failure, so this also pins that an
/// established `Opened` outranks the withholding the next test asserts.
#[tokio::test]
async fn a_rejected_tool_use_still_reports_the_opened_call() {
    let result = execute_scenario("undeclared_tool_use", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(loss.tool_calls, ToolCallsAtLoss::Opened);
}

/// A loss raised from a decoded prefix carries the negative fact, so it is told
/// apart from the case above by type rather than by the rendered detail.
///
/// The stream here ends without its terminal marker after events the decoder
/// read and classified in full — unlike a decode failure, nothing about this
/// response went unexamined, so "none opened" is a fact the adapter can state.
#[tokio::test]
async fn a_loss_from_a_decoded_prefix_without_tool_calls_reports_none_opened() {
    let result = execute_scenario("truncated_stream", OperationShape::Text).await;

    assert_eq!(
        boundary_loss(&result.evidence).tool_calls,
        ToolCallsAtLoss::NoneOpened
    );
}

/// An event whose content decoded and was then rejected on semantics states the
/// negative: the adapter read the blocks and no tool call was among them.
///
/// The rejection is a decode failure like the one below, so this is what makes
/// the withholding a statement about unexamined material rather than about the
/// failure class.
///
/// The rejected event ends the stream deliberately. A writer that continues past
/// it can leave the next line buffered in the reader when the failure is raised,
/// and that undelivered line withholds the fact on its own — which is the
/// separate behavior pinned by the prefetched-line test below. Ending here keeps
/// this test measuring only the rejected event's own examination.
#[tokio::test]
async fn a_decoded_event_rejected_on_semantics_reports_none_opened() {
    let result = execute_scenario(
        "conflicting_message_id_at_end_of_stream",
        OperationShape::Text,
    )
    .await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
}

/// The same semantic rejection on an event that *did* announce a tool call
/// reports it, which pins that the scan runs before the identity checks.
#[tokio::test]
async fn a_decoded_event_rejected_after_announcing_a_tool_reports_opened() {
    let result =
        execute_scenario("tool_use_with_conflicting_message_id", OperationShape::Text).await;

    assert_eq!(
        boundary_loss(&result.evidence).tool_calls,
        ToolCallsAtLoss::Opened
    );
}

/// A line that never decoded withholds the fact instead of stating a negative.
///
/// The failing line was never classified, so it could itself have carried the
/// `tool_use` block. Reporting "none opened" would claim a negative about
/// material the adapter never read.
#[tokio::test]
async fn a_line_that_never_decodes_withholds_the_tool_fact() {
    let result = execute_scenario("malformed_stream", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
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

/// The `system/init` version handshake is the adapter's binding between the
/// derived pin and the process it is actually talking to. Proving it rejects a
/// drifted version keeps the derivation honest: the scripted successes above
/// pass because the fake reports the derived version, not because the check is
/// inert.
#[tokio::test]
async fn version_handshake_mismatch_is_protocol_boundary_loss() {
    let result = execute_scenario("version_drift", OperationShape::Text).await;
    let loss = boundary_loss(&result.evidence);

    assert!(stream_protocol_detail(&loss.cause).contains(fixtures::DRIFTED_VERSION));
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
        None,
        None,
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

/// A line the runner rejects by its own bound never reaches the decoder, so the
/// tool fact is withheld: that line may itself have been the `assistant` event
/// carrying a `tool_use` block.
///
/// The bound is lowered rather than the fixture enlarged so the test states the
/// one value the behavior depends on.
#[tokio::test]
async fn a_line_rejected_by_the_event_bound_withholds_the_tool_fact() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let mut config = ClaudeCliConfig::new(
        fake_cli(),
        bridge_cli(),
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
        None,
        None,
    );
    config.exchange_timeout = Some(OFFLINE_TIMEOUT);
    config.interrupt_grace = Duration::from_millis(100);
    config.event_limit = 16;
    let runtime = ClaudeCliRuntime::new(config).expect("offline runtime configuration is valid");
    let prepared = prepare(
        &runtime,
        operation("normal_completion", OperationShape::Text),
    )
    .await;
    let report = runtime
        .execute(prepared, &mut Vec::new(), CancellationSignal::never())
        .await;

    let TerminalEvidence::BoundaryLoss(loss) = report.evidence else {
        panic!("a line past the event bound is boundary loss");
    };
    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
}

/// A deadline that fires at a line boundary discarded nothing, so every event
/// the adapter received was examined and the negative is a fact.
///
/// This is the companion to the test below: without it, marking every deadline
/// as undelivered would pass, and the fact would be withheld across ordinary
/// idle cancellations.
#[tokio::test]
async fn a_deadline_at_a_line_boundary_states_the_negative() {
    let report = execute_hanging_scenario("complete_event_then_hang").await;

    let TerminalEvidence::BoundaryLoss(loss) = report else {
        panic!("an exchange deadline is boundary loss");
    };
    assert_eq!(loss.tool_calls, ToolCallsAtLoss::NoneOpened);
}

/// A deadline that fires while the bounded-line reader holds a partial event
/// discards those bytes without ever decoding them, so the fact is withheld.
///
/// `read_bounded_line` accumulates into a local buffer, so dropping its future
/// loses the prefix it consumed — the discarded suffix may have carried a
/// `tool_use`.
#[tokio::test]
async fn a_deadline_dropping_a_partial_line_withholds_the_tool_fact() {
    let report = execute_hanging_scenario("partial_assistant_then_hang").await;

    let TerminalEvidence::BoundaryLoss(loss) = report else {
        panic!("an exchange deadline is boundary loss");
    };
    assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
}

/// A decode failure that leaves a prefetched line buffered withholds the tool
/// fact, because that discarded line is exactly what would have answered it.
///
/// The undecodable event is one the adapter *did* examine — its `type`
/// discriminator alone precludes content blocks — so the decode-failure path
/// would otherwise state the negative from that event's own examination while a
/// `tool_use` event sat unread in the reader's buffer. Both lines are written as
/// one batch so the reader holds the second while the first is being decoded.
#[tokio::test]
async fn a_decode_failure_holding_a_prefetched_line_withholds_the_tool_fact() {
    let result = execute_scenario(
        "undecodable_event_then_buffered_tool_use",
        OperationShape::Text,
    )
    .await;

    let TerminalEvidence::BoundaryLoss(loss) = result.evidence else {
        panic!("an undecodable event is boundary loss");
    };
    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
    assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
}

/// Runs a scenario that never terminates, against a deadline short enough to
/// keep the test quick.
///
/// Plumbing: the timeout is the only value these two tests depend on, and they
/// depend on it identically.
async fn execute_hanging_scenario(scenario: &str) -> TerminalEvidence {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let mut config = ClaudeCliConfig::new(
        fake_cli(),
        bridge_cli(),
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
        None,
        None,
    );
    // The deadline starts before environment setup and spawn, so it has to
    // cover both and still fire well inside the scenario's own 60s hang. A
    // tighter bound races process startup under load.
    config.exchange_timeout = Some(Duration::from_secs(3));
    config.interrupt_grace = Duration::from_millis(100);
    let runtime = ClaudeCliRuntime::new(config).expect("offline runtime configuration is valid");
    let prepared = prepare(&runtime, operation(scenario, OperationShape::Text)).await;
    runtime
        .execute(prepared, &mut Vec::new(), CancellationSignal::never())
        .await
        .evidence
}

async fn execute_scenario(scenario: &str, shape: OperationShape) -> ExecutionResult {
    execute_operation(operation(scenario, shape)).await
}

async fn execute_operation(operation: ModelOperation<String>) -> ExecutionResult {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = runtime(temporary.path(), &fake_cli());
    let prepared = prepare(&runtime, operation).await;
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
        history: std::fs::read_to_string(temporary.path().join("fake-claude-history"))
            .unwrap_or_default(),
        prompt: std::fs::read_to_string(temporary.path().join("fake-claude-prompt"))
            .unwrap_or_default(),
        history_mode: std::fs::read_to_string(temporary.path().join("fake-claude-history-mode"))
            .unwrap_or_default(),
    }
}

fn runtime(working_directory: &Path, executable: &Path) -> ClaudeCliRuntime {
    let mut config = ClaudeCliConfig::new(
        executable,
        bridge_cli(),
        working_directory,
        CredentialReference::new(CREDENTIAL_REFERENCE),
        None,
        None,
    );
    config.exchange_timeout = Some(OFFLINE_TIMEOUT);
    config.interrupt_grace = Duration::from_millis(100);
    ClaudeCliRuntime::new(config).expect("offline runtime configuration is valid")
}

fn file_delivery_runtime(working_directory: &Path, value: &str) -> ClaudeCliRuntime {
    file_delivery_runtime_result(working_directory, CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY, value)
        .expect("offline file-delivery runtime configuration is valid")
}

fn file_delivery_runtime_result(
    working_directory: &Path,
    env_key: &str,
    value: &str,
) -> Result<ClaudeCliRuntime, ClaudeCliConstructionError> {
    file_delivery_runtime_bytes_result(working_directory, env_key, value.as_bytes().to_vec())
}

fn file_delivery_runtime_bytes_result(
    working_directory: &Path,
    env_key: &str,
    value: Vec<u8>,
) -> Result<ClaudeCliRuntime, ClaudeCliConstructionError> {
    let reference = CredentialReference::new(CREDENTIAL_REFERENCE);
    let mut config = ClaudeCliConfig::new(
        fake_cli(),
        bridge_cli(),
        working_directory,
        reference.clone(),
        None,
        None,
    );
    config.exchange_timeout = Some(OFFLINE_TIMEOUT);
    config.interrupt_grace = Duration::from_millis(100);
    ClaudeCliRuntime::new_with_file_delivery(
        config,
        SyntheticCredentialAccess {
            reference,
            value: CredentialValue::new(value),
        },
        env_key,
    )
}

async fn assert_unusable_file_credential(value: Vec<u8>) {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let runtime = file_delivery_runtime_bytes_result(
        temporary.path(),
        CLAUDE_CLI_FILE_CREDENTIAL_ENV_KEY,
        value,
    )
    .expect("the file-delivery runtime construction is valid");

    let outcome = runtime
        .prepare(
            operation("normal_completion", OperationShape::Text),
            CancellationSignal::never(),
        )
        .await;

    assert_eq!(
        credential_unusable_detail(outcome),
        "Claude file credential must be nonempty, UTF-8, and NUL-free"
    );
    assert_eq!(spawn_count(temporary.path()), 0);
}

fn credential_unusable_detail(
    outcome: PreparationOutcome<String, ClaudeCliPreparedRequest<String>>,
) -> String {
    match outcome {
        PreparationOutcome::Failed {
            failure: PreparationFailure::CredentialUnusable { detail },
            ..
        } => detail,
        _ => panic!("expected an unusable credential preparation failure"),
    }
}

fn construction_error(
    result: Result<ClaudeCliRuntime, ClaudeCliConstructionError>,
) -> ClaudeCliConstructionError {
    match result {
        Ok(_) => panic!("expected Claude runtime construction to fail"),
        Err(error) => error,
    }
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

/// Every reported usage fact, in observation order, so a test can assert both
/// the value and that it is emitted exactly once.
fn reported_usage(
    observations: &[signalbox_model_runtime::Observation<String>],
) -> Vec<TokenUsage> {
    observations
        .iter()
        .filter_map(|observation| match &observation.fact {
            signalbox_model_runtime::ObservationFact::UsageReported(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

/// Observation variant names in order, for ordering assertions that do not
/// depend on the payloads.
fn observation_kinds(
    observations: &[signalbox_model_runtime::Observation<String>],
) -> Vec<&'static str> {
    observations
        .iter()
        .map(|observation| match &observation.fact {
            signalbox_model_runtime::ObservationFact::SendCommenced => "SendCommenced",
            signalbox_model_runtime::ObservationFact::ExchangeEstablished(_) => {
                "ExchangeEstablished"
            }
            signalbox_model_runtime::ObservationFact::ProviderModelReported(_) => {
                "ProviderModelReported"
            }
            signalbox_model_runtime::ObservationFact::TextDelta { .. } => "TextDelta",
            signalbox_model_runtime::ObservationFact::ThinkingDelta { .. } => "ThinkingDelta",
            signalbox_model_runtime::ObservationFact::ToolArgumentsDelta { .. } => {
                "ToolArgumentsDelta"
            }
            signalbox_model_runtime::ObservationFact::ToolCallProposed(_) => "ToolCallProposed",
            signalbox_model_runtime::ObservationFact::UsageReported(_) => "UsageReported",
            signalbox_model_runtime::ObservationFact::FinishReported(_) => "FinishReported",
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

fn recorded_argument<'a>(arguments: &'a str, name: &str) -> &'a str {
    let values = arguments.lines().collect::<Vec<_>>();
    values
        .windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1])
        .expect("the recorded argument is present")
}

fn spawn_count(directory: &Path) -> usize {
    std::fs::read_to_string(directory.join("fake-claude-spawns"))
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn fake_cli() -> std::path::PathBuf {
    test_bin_path!("signalbox-fake-claude-cli")
}

fn bridge_cli() -> std::path::PathBuf {
    test_bin_path!("signalbox-claude-mcp-bridge")
}

#[tokio::test]
async fn ambient_cli_preserves_fragmented_credential_shaped_text() {
    let result = execute_scenario("fragmented_credential_redaction", OperationShape::Text).await;
    assert_eq!(
        observation_text(&result.observations),
        fixtures::FRAGMENTED_SECRET
    );
    assert_eq!(
        completion_text(&result.evidence),
        fixtures::FRAGMENTED_SECRET
    );
}

#[tokio::test]
async fn ambient_cli_preserves_credential_shaped_tool_json() {
    let result = execute_scenario("suppressed_tool_arguments", OperationShape::Tool).await;
    let completion = completed(&result.evidence);
    assert_eq!(
        tool_call(&completion.content).arguments_json,
        fixtures::SUPPRESSED_TOOL_ARGUMENTS
    );
    assert!(result.observations.iter().any(|observation| matches!(&observation.fact, signalbox_model_runtime::ObservationFact::ToolCallProposed(call) if call.arguments_json == fixtures::SUPPRESSED_TOOL_ARGUMENTS)));
}
