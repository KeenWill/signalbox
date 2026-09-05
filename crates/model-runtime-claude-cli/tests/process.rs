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
async fn nonterminal_system_events_do_not_mask_the_initialized_exchange() {
    let result = execute_scenario("nonterminal_system_events", OperationShape::Text).await;

    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker in a discarded lifecycle event remains part of
/// the redaction lookbehind, so retained assistant output cannot disclose its
/// otherwise opaque continuation.
#[tokio::test]
async fn inv_035_dropped_system_lifecycle_event_seeds_output_redaction() {
    let result = execute_scenario("system_lifecycle_event_redaction", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::FRAGMENTED_SECRET_CONTINUATION));
    assert!(diagnostic.contains("[redacted]"));
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

/// INV-035: an uncorrelated lifecycle `session_id` is provider content, not a
/// repeated identity, so it seeds the redaction lookbehind rather than being
/// discarded unexamined.
#[tokio::test]
async fn inv_035_uncorrelated_lifecycle_session_seeds_output_redaction() {
    let result = execute_scenario("lifecycle_session_precedes_init", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::FRAGMENTED_SECRET_CONTINUATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn assistant_resolved_model_may_differ_from_the_selected_init_alias() {
    let result = execute_scenario("resolved_assistant_model", OperationShape::Text).await;

    assert_eq!(completion_text(&result.evidence), fixtures::ANSWER);
    assert_eq!(result.spawns, 1);
}

/// INV-035: the provider-resolved model an assistant event newly accepts is
/// stored for the contradiction check and then discarded, so it reaches no
/// record and this ambient-delivery runtime holds no exact value to redact
/// downstream. A marker prefix ending that model must still seed the redaction
/// lookbehind, or its own first text block completes the credential across the
/// two fields and is emitted verbatim.
///
/// The init model here is the clean selected alias, so the resolved model is
/// the only source of the marker — no other chain can account for the
/// suppression.
#[tokio::test]
async fn inv_035_resolved_model_prefix_is_held_into_the_first_text_block() {
    let result = execute_scenario("resolved_model_prefix_redaction", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::MODEL_CREDENTIAL_CONTINUATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: every assistant envelope repeats and discards the resolved model,
/// so each one re-seeds the redaction lookbehind ahead of its own content. A
/// first event whose clean text spends that lookbehind must not leave a second
/// event's text free to continue the marker the model ends in.
#[tokio::test]
async fn inv_035_repeated_model_prefix_is_held_into_each_events_text() {
    let result = execute_scenario("repeated_model_prefix_redaction", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::MODEL_CREDENTIAL_CONTINUATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a repeat of the discarded resolved model is the same field, not a
/// second one. Classifying it as new would spend the discarded-field slot and
/// fail the exchange closed, destroying ordinary output that the held marker
/// legitimately releases once its candidate resolves clean.
#[tokio::test]
async fn repeated_resolved_model_does_not_over_redact_an_ordinary_exchange() {
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
async fn inv_035_harmless_terminal_credential_prefix_remains_byte_exact() {
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

/// A whole-object credential suppression remains typed and never emits an
/// executable tool proposal or argument delta.
#[tokio::test]
async fn fully_suppressed_tool_arguments_are_non_executable() {
    let result = execute_scenario("suppressed_tool_arguments", OperationShape::Tool).await;
    let completion = completed(&result.evidence);

    assert_eq!(
        completion.content,
        vec![AssistantPart::SuppressedToolCall(
            signalbox_model_runtime::ToolName::new(fixtures::TOOL_NAME),
        )]
    );
    assert!(!result.observations.iter().any(|observation| matches!(
        observation.fact,
        signalbox_model_runtime::ObservationFact::ToolCallProposed(_)
            | signalbox_model_runtime::ObservationFact::ToolArgumentsDelta { .. }
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

/// The credential boundary withholds a proposal's arguments, never its tool
/// identity, so a suppressed foreign proposal still violates a named choice.
#[tokio::test]
async fn named_tool_choice_rejects_a_suppressed_extra_declared_proposal() {
    let result = execute_scenario(
        "named_choice_suppressed_extra_tool",
        OperationShape::NamedTool,
    )
    .await;
    let loss = boundary_loss(&result.evidence);
    let proposed: Vec<&str> = result
        .observations
        .iter()
        .filter_map(|observation| match &observation.fact {
            signalbox_model_runtime::ObservationFact::ToolCallProposed(proposal) => {
                Some(proposal.name.as_str())
            }
            _ => None,
        })
        .collect();
    let argument_deltas = result
        .observations
        .iter()
        .filter(|observation| {
            matches!(
                observation.fact,
                signalbox_model_runtime::ObservationFact::ToolArgumentsDelta { .. }
            )
        })
        .count();

    // Named-tool validation rejects an admitted foreign proposal and a
    // suppressed one alike, so only the emitted proposals prove which form the
    // credential boundary produced before validation saw it: the required tool
    // proposes and streams its arguments, the suppressed one does neither.
    assert_eq!(proposed, vec![fixtures::TOOL_NAME]);
    assert_eq!(argument_deltas, 1);
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
async fn inv_035_credential_shaped_cli_text_is_redacted_from_all_evidence() {
    let result = execute_scenario("credential_redaction", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_TEXT));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn inv_035_fragmented_credential_is_redacted_from_observations_and_terminal_content() {
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
async fn inv_035_control_sequence_cannot_obfuscate_a_streamed_credential() {
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
async fn inv_035_redacted_provider_identities_still_compare_by_native_value() {
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
async fn inv_035_model_identifier_prefix_is_held_into_the_first_text_block() {
    let result = execute_scenario("model_prefix_redaction", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::MODEL_CREDENTIAL_CONTINUATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an assistant message id ending in a credential marker prefix seeds
/// the emitted-context lookbehind, so the first text block of that same message
/// cannot complete the marker across the two retained completion fields.
///
/// The assertion joins the fields rather than searching the debug rendering:
/// the rendering puts punctuation between `message_id` and `content`, so a
/// `contains` over it would pass even with the seam removed. The continuation
/// value carries no credential indicator of its own, so a pass proves the id
/// seeded the lookbehind rather than proving the value looked secret-shaped.
#[tokio::test]
async fn inv_035_message_id_credential_prefix_is_held_into_the_first_text_block() {
    let result = execute_scenario("message_id_prefix_redaction", OperationShape::Text).await;
    let completion = completed(&result.evidence);
    let message_id = completion
        .message_id
        .as_ref()
        .map_or(String::new(), |id| id.as_str().to_string());
    let joined = format!("{message_id}{}", completion_text(&result.evidence));

    assert!(!joined.contains(fixtures::MESSAGE_ID_RECONSTRUCTED_CREDENTIAL));
    assert!(
        !observation_text(&result.observations).contains(fixtures::OPAQUE_CREDENTIAL_CONTINUATION)
    );
    assert_eq!(result.spawns, 1);
}

/// INV-035: a tool proposal id ending in a credential marker prefix seeds the
/// same lookbehind, so a following text block cannot complete the marker across
/// the emitted proposal and the emitted text.
#[tokio::test]
async fn inv_035_tool_proposal_id_credential_prefix_is_held_into_following_text() {
    let result = execute_scenario("tool_id_prefix_redaction", OperationShape::Tool).await;
    let completion = completed(&result.evidence);
    let [
        AssistantPart::ToolCall(proposal),
        AssistantPart::Text(following_text),
    ] = completion.content.as_slice()
    else {
        panic!(
            "expected a proposal then text, got {:?}",
            completion.content
        )
    };
    let joined = format!("{}{following_text}", proposal.id.as_str());

    assert!(!joined.contains(fixtures::TOOL_ID_RECONSTRUCTED_CREDENTIAL));
    assert!(
        !observation_text(&result.observations).contains(fixtures::OPAQUE_CREDENTIAL_CONTINUATION)
    );
    assert_eq!(result.spawns, 1);
}

/// A generic structured error and definitive stderr remain opaque. The usage
/// the result stated still reaches the observation stream on this path.
#[tokio::test]
async fn definitive_exit_stderr_keeps_a_generic_structured_error_opaque() {
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

/// INV-035: fail-closed suppression is absorbing for the sink's lifetime. The
/// usage barrier flushes held text, but it must not re-enable
/// provider-controlled bytes: a terminal error whose message continues a marker
/// that began in suppressed content would otherwise be judged by the stateless
/// scan alone, which cannot see that marker, and the continuation would reach
/// `NativeErrorFacts`.
///
/// This is the ordering the usage fix makes reachable on the common path —
/// `UsageReported` is now emitted as the `result` event is processed, ahead of
/// the error branch that sanitizes the native facts.
#[tokio::test]
async fn inv_035_suppression_survives_the_usage_barrier_into_terminal_error_facts() {
    let result = execute_scenario(
        "suppressed_state_survives_the_usage_barrier",
        OperationShape::Text,
    )
    .await;
    let failure = provider_error(&result.evidence);
    let native = format!(
        "{:?}{:?}",
        failure.native.message, failure.native.error_token
    );

    assert!(!native.contains(fixtures::OPAQUE_CREDENTIAL_CONTINUATION));
    assert!(native.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn inv_035_credential_shaped_tool_ids_receive_distinct_safe_surrogates() {
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
async fn nonzero_exit_is_an_opaque_provider_failure() {
    let result = execute_scenario("process_nonzero", OperationShape::Text).await;
    let failure = provider_error(&result.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::Unrecognized);
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
async fn inv_035_credential_shaped_finish_tokens_are_redacted_from_typed_evidence() {
    let result = execute_scenario("credential_finish_token", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::FINISH_TOKEN_SECRET));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn inv_035_credential_shaped_error_tokens_are_redacted_from_typed_evidence() {
    let result = execute_scenario("credential_error_token", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::ERROR_TOKEN_SECRET));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn inv_035_reasoning_metadata_cannot_complete_a_streamed_credential() {
    let result = execute_scenario("reasoning_metadata_credential", OperationShape::Text).await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::REASONING_SECRET));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

#[tokio::test]
async fn inv_035_redacted_reasoning_metadata_cannot_complete_a_streamed_credential() {
    let result = execute_scenario(
        "redacted_reasoning_metadata_credential",
        OperationShape::Text,
    )
    .await;
    let diagnostic = format!("{:?}{:?}", result.evidence, result.observations);

    assert!(!diagnostic.contains(fixtures::REASONING_SECRET));
    assert!(diagnostic.contains("[redacted]"));
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
