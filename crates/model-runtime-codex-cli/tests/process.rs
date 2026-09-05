#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, ConversationRole,
    CredentialReference, DeliveryMode, LossCause, MessagePart, ModelOperation, ModelRuntime,
    Observation, ObservationFact, PreparationFailure, PreparationOutcome, ProviderErrorKind,
    REDACTED, RequestedTarget, ResolvedTarget, StreamInterruption, StructuredDecodeFailure,
    StructuredOutputContract, TerminalEvidence, TokenUsage, ToolCallId, ToolCallProposal,
    ToolCallsAtLoss, ToolChoice, ToolDefinition, ToolName, decode_structured,
};
use signalbox_model_runtime_codex_cli::{
    CodexCliConfig, CodexCliConstructionError, CodexCliRuntime,
    DISABLED_CODEX_CLI_CAPABILITY_FEATURES,
};
use signalbox_test_bin::test_bin_path;

#[path = "support/fixtures.rs"]
mod fixtures;

const CREDENTIAL_REFERENCE: &str = "codex-subscription-primary";
const RESOLVED_TARGET: &str = "gpt-offline-exact";
const OFFLINE_HARNESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Provider text carried by the terminal half of the boundary extractor fixture.
const BOUNDARY_FIXTURE_TERMINAL_TEXT: &str = "synthetic-terminal-boundary-text";
/// Provider text carried by the observation half of the boundary extractor fixture.
const BOUNDARY_FIXTURE_OBSERVATION_TEXT: &str = "synthetic-observation-boundary-text";

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

/// Flattens only the typed, provider-controlled strings that actually cross
/// the adapter boundary. Unlike a `Debug` dump, every included value is an
/// emitted observation or terminal-evidence field and every such field is
/// visited explicitly.
fn boundary_material(result: &ExecutionResult) -> String {
    boundary_material_from(&result.evidence, &result.observations)
}

fn boundary_material_from(
    evidence: &TerminalEvidence,
    observations: &[Observation<String>],
) -> String {
    let mut material = Vec::new();
    collect_terminal_evidence(evidence, &mut material);
    for observation in observations {
        collect_observation(&observation.fact, &mut material);
    }
    material.join("\n")
}

fn collect_terminal_evidence(evidence: &TerminalEvidence, material: &mut Vec<String>) {
    match evidence {
        TerminalEvidence::Completed(completed) => {
            collect_exchange(&completed.exchange, material);
            collect_message_id(completed.message_id.as_ref(), material);
            collect_reported_model(completed.reported_model.as_ref(), material);
            collect_completion_finish(&completed.finish, material);
            collect_assistant_parts(&completed.content, material);
        }
        TerminalEvidence::Refused(refused) => {
            collect_exchange(&refused.exchange, material);
            collect_message_id(refused.message_id.as_ref(), material);
            collect_reported_model(refused.reported_model.as_ref(), material);
            collect_assistant_parts(&refused.content, material);
        }
        TerminalEvidence::ProviderError(error) => {
            collect_exchange(&error.exchange, material);
            collect_reported_model(error.reported_model.as_ref(), material);
            collect_native_error(&error.native, material);
        }
        TerminalEvidence::CancellationConfirmed(cancelled) => {
            collect_exchange(&cancelled.exchange, material);
            collect_reported_model(cancelled.reported_model.as_ref(), material);
            collect_native_error(&cancelled.native, material);
        }
        TerminalEvidence::ProvenUnsent(unsent) => collect_unsent_cause(&unsent.cause, material),
        TerminalEvidence::BoundaryLoss(loss) => {
            collect_exchange(&loss.exchange, material);
            collect_reported_model(loss.reported_model.as_ref(), material);
            if let Some(finish) = &loss.finish_reported {
                collect_finish_reason(finish, material);
            }
            collect_loss_cause(&loss.cause, material);
        }
    }
}

fn collect_observation(fact: &ObservationFact, material: &mut Vec<String>) {
    match fact {
        ObservationFact::SendCommenced | ObservationFact::UsageReported(_) => {}
        ObservationFact::ExchangeEstablished(exchange) => collect_exchange(exchange, material),
        ObservationFact::ProviderModelReported(model) => material.push(model.as_str().to_string()),
        ObservationFact::TextDelta { text, .. } | ObservationFact::ThinkingDelta { text, .. } => {
            material.push(text.clone())
        }
        ObservationFact::ToolArgumentsDelta { fragment, .. } => material.push(fragment.clone()),
        ObservationFact::ToolCallProposed(proposal) => collect_tool_proposal(proposal, material),
        ObservationFact::FinishReported(finish) => collect_finish_reason(finish, material),
    }
}

fn collect_exchange(exchange: &signalbox_model_runtime::ExchangeFacts, material: &mut Vec<String>) {
    if let Some(request_id) = &exchange.provider_request_id {
        material.push(request_id.as_str().to_string());
    }
}

fn collect_message_id(
    message_id: Option<&signalbox_model_runtime::ProviderMessageId>,
    material: &mut Vec<String>,
) {
    if let Some(message_id) = message_id {
        material.push(message_id.as_str().to_string());
    }
}

fn collect_reported_model(
    model: Option<&signalbox_model_runtime::ProviderReportedModel>,
    material: &mut Vec<String>,
) {
    if let Some(model) = model {
        material.push(model.as_str().to_string());
    }
}

fn collect_assistant_parts(parts: &[AssistantPart], material: &mut Vec<String>) {
    for part in parts {
        match part {
            AssistantPart::Text(text) => material.push(text.clone()),
            AssistantPart::Thinking { text, signature } => {
                material.push(text.clone());
                if let Some(signature) = signature {
                    material.push(signature.clone());
                }
            }
            AssistantPart::RedactedThinking { data } => material.push(data.clone()),
            AssistantPart::ToolCall(proposal) => collect_tool_proposal(proposal, material),
            AssistantPart::SuppressedToolCall(name) => {
                material.push(name.as_str().to_string());
            }
        }
    }
}

fn collect_tool_proposal(proposal: &ToolCallProposal, material: &mut Vec<String>) {
    material.push(proposal.id.as_str().to_string());
    material.push(proposal.name.as_str().to_string());
    material.push(proposal.arguments_json.clone());
}

fn collect_native_error(
    native: &signalbox_model_runtime::NativeErrorFacts,
    material: &mut Vec<String>,
) {
    material.extend(native.error_token.iter().cloned());
    material.extend(native.error_code.iter().cloned());
    material.extend(native.message.iter().cloned());
}

fn collect_completion_finish(finish: &CompletionFinish, material: &mut Vec<String>) {
    match finish {
        CompletionFinish::StopSequence { sequence } => material.extend(sequence.iter().cloned()),
        CompletionFinish::Unrecognized { provider_token } => {
            material.push(provider_token.clone());
        }
        CompletionFinish::EndTurn
        | CompletionFinish::MaxOutputTokens
        | CompletionFinish::ContextWindowExceeded
        | CompletionFinish::ToolUse => {}
    }
}

fn collect_finish_reason(
    finish: &signalbox_model_runtime::FinishReason,
    material: &mut Vec<String>,
) {
    match finish {
        signalbox_model_runtime::FinishReason::StopSequence { sequence } => {
            material.extend(sequence.iter().cloned());
        }
        signalbox_model_runtime::FinishReason::Unrecognized { provider_token } => {
            material.push(provider_token.clone());
        }
        signalbox_model_runtime::FinishReason::EndTurn
        | signalbox_model_runtime::FinishReason::MaxOutputTokens
        | signalbox_model_runtime::FinishReason::ContextWindowExceeded
        | signalbox_model_runtime::FinishReason::ToolUse
        | signalbox_model_runtime::FinishReason::Refusal => {}
    }
}

fn collect_unsent_cause(cause: &signalbox_model_runtime::UnsentCause, material: &mut Vec<String>) {
    match cause {
        signalbox_model_runtime::UnsentCause::CancelledBeforeSend => {}
        signalbox_model_runtime::UnsentCause::ConnectFailed(facts)
        | signalbox_model_runtime::UnsentCause::SendIncompleteProvenUnacceptable(facts) => {
            material.push(facts.detail.clone());
        }
    }
}

fn collect_loss_cause(cause: &LossCause, material: &mut Vec<String>) {
    match cause {
        LossCause::CancellationRequested
        | LossCause::UnexpectedHttpStatus
        | LossCause::StreamEndedWithoutTerminalMarker {
            interruption: StreamInterruption::EndOfStream,
        } => {}
        LossCause::TimedOut(facts)
        | LossCause::TransportFailed(facts)
        | LossCause::ResponseBodyLost(facts)
        | LossCause::StreamEndedWithoutTerminalMarker {
            interruption:
                StreamInterruption::TransportFailure(facts) | StreamInterruption::TimedOut(facts),
        } => material.push(facts.detail.clone()),
        LossCause::ResponseUnintelligible { detail }
        | LossCause::StreamProtocolViolation { detail } => material.push(detail.clone()),
    }
}

/// Which of a correlation's parallel delta streams a fragment extends. Part of
/// the reassembly key: two facts may share a correlation and an index yet
/// belong to streams no consumer ever concatenates together.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum EmittedStream {
    Text,
    Thinking,
    ToolArguments,
}

/// Reassembles each delta stream the adapter emitted, in emission order,
/// grouped by the stream a fragment extends — correlation, kind, and index.
///
/// [`boundary_material`] joins the facts it visits with newlines, which is what
/// a *field*-level claim needs but not what a stream-level one does: the
/// redactor may emit a safe prefix and its continuation as two fragments of one
/// delta stream, and a credential spanning that split is recoverable by any
/// consumer that concatenates the stream while appearing in no single fragment
/// and in no newline-joined dump. A claim about what the caller can read is
/// therefore checked against the reconstruction the caller assembles.
///
/// Exhaustive over `ObservationFact` so a new text-bearing fact cannot be added
/// without deciding whether it joins a stream. Facts that carry one complete
/// value rather than a fragment (a decoded tool proposal, the thread id) are
/// not fragments of anything and stay covered by [`boundary_material`].
fn emitted_streams(observations: &[Observation<String>]) -> Vec<String> {
    let mut streams: BTreeMap<(&str, EmittedStream, u32), String> = BTreeMap::new();
    for observation in observations {
        let correlation = observation.correlation.as_str();
        let (kind, index, fragment) = match &observation.fact {
            ObservationFact::TextDelta { index, text } => (EmittedStream::Text, *index, text),
            ObservationFact::ThinkingDelta { index, text } => {
                (EmittedStream::Thinking, *index, text)
            }
            ObservationFact::ToolArgumentsDelta { index, fragment } => {
                (EmittedStream::ToolArguments, *index, fragment)
            }
            ObservationFact::SendCommenced
            | ObservationFact::ExchangeEstablished(_)
            | ObservationFact::ProviderModelReported(_)
            | ObservationFact::ToolCallProposed(_)
            | ObservationFact::UsageReported(_)
            | ObservationFact::FinishReported(_) => continue,
        };
        streams
            .entry((correlation, kind, index))
            .or_default()
            .push_str(fragment);
    }
    streams.into_values().collect()
}

/// Asserts `secret` is unrecoverable from everything that crossed the adapter
/// boundary: every field [`boundary_material`] visits, and every delta stream
/// reassembled as the caller would read it. `label` names the fixture, so a
/// leak in one case of a multi-scenario test is reported as itself.
///
/// Both checks, because they fail differently. Every fixture this helper guards
/// is suppressed whole today and emits one fragment, so the field-level dump
/// alone would catch each of them; the reassembly is what keeps that from being
/// the assertion's ceiling. A regression that released the safe prefix and held
/// only the tail would put the credential in no single fragment and in no
/// newline-joined dump, and the case guarding the leak would go on passing.
#[track_caller]
fn assert_no_emitted_stream_carries(label: &str, result: &ExecutionResult, secret: &str) {
    let material = boundary_material(result);
    assert!(
        !material.contains(secret),
        "{label}: the credential must not cross the boundary, found it in: {material}"
    );
    for stream in emitted_streams(&result.observations) {
        assert!(
            !stream.contains(secret),
            "{label}: the credential must not be recoverable from a reassembled stream: {stream}"
        );
    }
}

#[test]
fn boundary_material_reads_terminal_and_observation_text() {
    let result = ExecutionResult {
        evidence: TerminalEvidence::BoundaryLoss(signalbox_model_runtime::BoundaryLossEvidence {
            cause: LossCause::StreamProtocolViolation {
                detail: BOUNDARY_FIXTURE_TERMINAL_TEXT.to_string(),
            },
            exchange: signalbox_model_runtime::ExchangeFacts::default(),
            reported_model: None,
            finish_reported: None,
            tool_calls: signalbox_model_runtime::ToolCallsAtLoss::Unobserved,
            usage: TokenUsage::unreported(),
        }),
        observations: vec![Observation {
            correlation: "boundary-fixture".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: BOUNDARY_FIXTURE_OBSERVATION_TEXT.to_string(),
            },
        }],
        spawns: 0,
        argv: String::new(),
        prompt: String::new(),
    };
    let material = boundary_material(&result);

    assert!(material.contains(BOUNDARY_FIXTURE_TERMINAL_TEXT));
    assert!(material.contains(BOUNDARY_FIXTURE_OBSERVATION_TEXT));
}

fn text_delta_fixture(correlation: &str, index: u32, text: &str) -> Observation<String> {
    Observation {
        correlation: correlation.to_string(),
        fact: ObservationFact::TextDelta {
            index,
            text: text.to_string(),
        },
    }
}

/// The reassembly every stream-level absence check depends on: a value the
/// redactor emitted as two fragments of one stream is one reconstruction, so a
/// credential spanning the split cannot hide in the gap between them.
#[test]
fn emitted_streams_rejoin_the_fragments_of_one_stream() {
    let observations = vec![
        text_delta_fixture("reassembly-fixture", 0, "synthetic-reassembly-"),
        text_delta_fixture("reassembly-fixture", 0, "secret"),
    ];

    assert_eq!(
        emitted_streams(&observations),
        vec!["synthetic-reassembly-secret".to_string()]
    );
}

/// Grouping is per stream: fragments that spell a value only across streams no
/// consumer concatenates — a different part index, a different correlation —
/// are separate reconstructions, so the check does not report a leak the caller
/// cannot read.
#[test]
fn emitted_streams_keep_separate_streams_apart() {
    let observations = vec![
        text_delta_fixture("reassembly-fixture", 0, "synthetic-reassembly-"),
        text_delta_fixture("reassembly-fixture", 1, "secret"),
        text_delta_fixture("other-correlation", 0, "secret"),
    ];

    // Three reconstructions, none of them the joined value: the fragments stay
    // in the streams that carried them, keyed correlation-first.
    assert_eq!(
        emitted_streams(&observations),
        vec![
            "secret".to_string(),
            "synthetic-reassembly-".to_string(),
            "secret".to_string(),
        ]
    );
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
    assert!(result.argv.contains("--output-last-message"));
    assert!(result.argv.contains("--ignore-user-config"));
    assert!(result.argv.contains("--ignore-rules"));
    assert!(result.argv.contains(&disabled_capability_argv()));
    assert!(result.argv.contains("--config\nagents.enabled=false"));
    assert!(
        result
            .argv
            .contains("--config\nskills.include_instructions=false")
    );
    assert!(result.argv.contains("--config\nmcp_servers={}"));
    assert!(result.argv.contains("--config\nweb_search=\"disabled\""));
    assert!(result.argv.contains("--config\nproject_doc_max_bytes=0"));
    assert!(result.argv.contains(RESOLVED_TARGET));
    assert!(!result.argv.contains("model_context_window="));
    assert!(result.prompt.contains(scenario));
}

#[tokio::test]
async fn exact_model_context_window_override_reaches_the_codex_process() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let mut config = CodexCliConfig::new(
        fake_cli(),
        temporary.path(),
        CredentialReference::new(CREDENTIAL_REFERENCE),
        None,
    );
    config
        .model_context_window_overrides
        .insert(RESOLVED_TARGET.to_string(), 1_000_000);
    let runtime = CodexCliRuntime::new(config).expect("offline runtime configuration is valid");
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
    let argv = read_optional(temporary.path().join("fake-codex-argv"));

    assert!(matches!(report.evidence, TerminalEvidence::Completed(_)));
    assert!(argv.contains("--config\nmodel_context_window=1000000"));
}

#[tokio::test]
async fn completed_turn_recovers_the_clis_independently_retained_final_message() {
    let result = execute_scenario(
        "output_last_message_recovery",
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

/// INV-035: a final message recovered from the CLI-owned file still follows
/// the preceding JSONL redaction state; the second channel cannot bypass a
/// credential marker retained from an earlier event.
#[tokio::test]
async fn inv_035_output_last_message_consults_the_jsonl_redaction_state() {
    let result = execute_scenario(
        "output_last_message_split_credential",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_STREAM_TOKEN));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// Exact ordered argv fragment generated from the audited production fixture,
/// so the process regression proves every classified capability reaches the
/// spawned CLI as a hard disable without re-encoding the list in the test.
fn disabled_capability_argv() -> String {
    DISABLED_CODEX_CLI_CAPABILITY_FEATURES
        .iter()
        .map(|feature| format!("--disable\n{feature}"))
        .collect::<Vec<_>>()
        .join("\n")
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

/// INV-035: buffered delivery drops reasoning from the output, but a
/// credential marker inside the dropped bytes still marks the final text's
/// value as a secret — the same bytes the streamed path suppresses must not
/// surface verbatim in buffered completion evidence.
#[tokio::test]
async fn inv_035_buffered_reasoning_marker_suppresses_the_final_text_value() {
    let result = execute_scenario(
        "split_stream_authorization_before_final_text",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: the dropped-reasoning marker also governs buffered tool
/// arguments, which reach terminal evidence without passing through streamed
/// deltas.
#[tokio::test]
async fn inv_035_buffered_reasoning_marker_suppresses_tool_arguments() {
    let result = execute_scenario(
        "split_stream_authorization_before_tool_arguments",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(
        completed(&result.evidence).content,
        vec![
            AssistantPart::Text(REDACTED.to_string()),
            AssistantPart::SuppressedToolCall(signalbox_model_runtime::ToolName::new(
                fixtures::TOOL_NAME,
            )),
        ]
    );
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential header split between streamed reasoning and the
/// final envelope's tool arguments keeps redacting through the value: the
/// argument bytes consult the held lookbehind state before the streamed
/// argument delta and the terminal proposal are built.
#[tokio::test]
async fn inv_035_split_authorization_value_before_tool_arguments_is_redacted() {
    let result = execute_scenario(
        "split_stream_authorization_before_tool_arguments",
        DeliveryMode::Streamed,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker held from streamed reasoning also governs a
/// tool-call id, so an id that extends the marker is replaced with a safe
/// surrogate instead of leaking through the proposal or terminal content.
#[tokio::test]
async fn inv_035_split_authorization_value_before_tool_id_is_redacted() {
    let result = execute_scenario(
        "split_stream_authorization_before_tool_id",
        DeliveryMode::Streamed,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a tool argument continuing a credential marker at the end of the
/// same-envelope final text is redacted in both the streamed delta and the
/// terminal proposal, not only when the marker came from earlier reasoning.
#[tokio::test]
async fn inv_035_final_text_marker_before_tool_arguments_is_redacted() {
    let result = execute_scenario(
        "final_text_marker_before_tool_arguments",
        DeliveryMode::Streamed,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker ending the final envelope text also governs
/// the agent-message item id — the same same-envelope context the tool-call id
/// path consults — so an id carrying the marker's continuation never surfaces
/// as `ProviderMessageId` beside the independently redacted text.
#[tokio::test]
async fn inv_035_final_text_marker_before_message_id_is_redacted() {
    let result = execute_scenario(
        "final_text_marker_before_message_id",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a thread id ending in a credential-marker prefix (`api_`) seeds
/// the lookbehind when it is emitted in `ExchangeEstablished`, so streamed
/// text carrying the marker's continuation (`key=value`) is suppressed
/// instead of emitted beside the id, where the two records would reconstruct
/// the credential.
#[tokio::test]
async fn inv_035_thread_id_marker_prefix_suppresses_streamed_continuation() {
    let result = execute_scenario(
        "credential_prefix_thread_id_before_text",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_THREAD_CONTINUATION));
    assert!(!diagnostic.contains("opaque-thread-continuation"));
    // The id itself is harmless alone and keeps its diagnostic fidelity; the
    // suppression lands on the continuation text, not the exchange facts.
    assert!(diagnostic.contains(fixtures::CREDENTIAL_PREFIX_THREAD_ID));
    assert_eq!(result.spawns, 1);
}

/// INV-035: the same reconstruction is caught in buffered delivery, where the
/// final text reaches terminal evidence without passing through streamed
/// deltas — the buffered text consults the emitted thread-id context too.
#[tokio::test]
async fn inv_035_thread_id_marker_prefix_suppresses_buffered_continuation() {
    let result = execute_scenario(
        "credential_prefix_thread_id_before_text",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_THREAD_CONTINUATION));
    assert!(!diagnostic.contains("opaque-thread-continuation"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker inside a dropped error item governs the
/// streamed final text that follows — the marker appears in no record, but
/// the value completing it is a secret the stream must suppress.
#[tokio::test]
async fn inv_035_error_item_marker_suppresses_streamed_continuation() {
    let result = execute_scenario(
        "credential_split_across_error_item",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: two independent object fields each ending in a distinct credential
/// marker fail closed — a following value could complete either, and the
/// single dropped chain cannot track both.
#[tokio::test]
async fn inv_035_two_independent_sibling_markers_fail_closed() {
    let result = execute_scenario(
        "two_independent_sibling_markers",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an additive credential marker on a `thread.started` event governs
/// the following final text.
#[tokio::test]
async fn inv_035_thread_started_additive_field_marker_suppresses_the_value() {
    let result = execute_scenario(
        "credential_split_across_thread_started_field",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// A streamed completion whose final text is empty and whose only provisional
/// content was a held-credential `[redacted]` placeholder — replaced by an
/// empty capture — fails closed as ResponseUnintelligible, not a contentless
/// Completed.
#[tokio::test]
async fn streamed_empty_completion_with_held_credential_is_unintelligible() {
    let result = execute_scenario(
        "streamed_empty_final_text_with_held_credential",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    let cause = response_unintelligible(&boundary_loss(&result.evidence).cause);
    assert!(cause.contains("no completion material"));
    assert_eq!(
        boundary_loss(&result.evidence).finish_reported,
        Some(signalbox_model_runtime::FinishReason::EndTurn)
    );
}

/// Repeated members are ambiguous stream input, never additive evolution: the
/// adapter fails closed before a last-value-wins JSON projection can choose
/// which occurrence becomes evidence.
#[tokio::test]
async fn duplicate_event_members_are_stream_protocol_violations() {
    let result = execute_scenario(
        "duplicate_unknown_event_member",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    let detail = stream_protocol_violation(&boundary_loss(&result.evidence).cause);
    assert!(detail.contains("duplicate"));
}

/// Repeated members stay ambiguous at nested object depth; the validation walk
/// cannot stop at the event envelope before serde projects its child objects.
#[tokio::test]
async fn nested_duplicate_event_members_are_stream_protocol_violations() {
    let result = execute_scenario(
        "nested_duplicate_unknown_event_member",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    let detail = stream_protocol_violation(&boundary_loss(&result.evidence).cause);
    assert!(detail.contains("duplicate"));
}

/// The response envelope is provider input even though the CLI transports it as
/// escaped agent-message text; repeated envelope members remain ambiguous and
/// must fail before serde can select the last occurrence.
#[tokio::test]
async fn duplicate_response_envelope_members_are_stream_protocol_violations() {
    let result = execute_scenario(
        "duplicate_response_envelope_member",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    let detail = stream_protocol_violation(&boundary_loss(&result.evidence).cause);
    assert!(detail.contains("response envelope"));
}

/// INV-035: a marker-bearing object field that sorts before a benign sibling
/// (so a key-sorted concatenation would drop the marker) still governs the
/// following final text — sibling fields are seeded as independent units.
#[tokio::test]
async fn inv_035_sibling_object_field_marker_is_not_erased() {
    let result = execute_scenario(
        "credential_split_across_sibling_object_fields",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an additive credential marker on an otherwise-accepted
/// `turn.started` event governs the following final text.
#[tokio::test]
async fn inv_035_turn_started_additive_field_marker_suppresses_the_value() {
    let result = execute_scenario(
        "credential_split_across_turn_started_field",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a retained agent message ending in a marker, superseded by a
/// `turn.failed` whose message supplies the value, is folded so the failure
/// message's value is suppressed in native error evidence.
#[tokio::test]
async fn inv_035_retained_agent_message_folds_before_failure() {
    let result = execute_scenario(
        "credential_split_across_agent_message_then_failure",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker ending an agent message superseded by a later
/// one, with the value in the final message, is folded into the lookbehind and
/// suppressed rather than reconstructed across the discard.
#[tokio::test]
async fn inv_035_superseded_agent_message_marker_suppresses_the_value() {
    let result = execute_scenario(
        "credential_split_across_superseded_agent_message",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker carried by an ignored lifecycle event's
/// additive field governs the following final text.
#[tokio::test]
async fn inv_035_lifecycle_event_marker_suppresses_the_value() {
    let result = execute_scenario(
        "credential_split_across_lifecycle_event",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker in an additively-tolerated unknown top-level
/// event governs the following final text.
#[tokio::test]
async fn inv_035_unknown_event_marker_suppresses_the_value() {
    let result = execute_scenario(
        "credential_split_across_unknown_event",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an unmodeled item's ordered-array leaves that jointly form a
/// marker (`["api", "_key="]`) seed the lookbehind in document order, so the
/// following value is suppressed even though no single leaf is a marker.
#[tokio::test]
async fn inv_035_ordered_unsupported_leaves_form_a_marker() {
    let result = execute_scenario(
        "credential_split_across_ordered_unsupported_leaves",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an agent-message id ending in a credential-marker prefix whose
/// continuation opens the final text is redacted, breaking the credential
/// reconstruction across the id and content fields of terminal evidence.
#[tokio::test]
async fn inv_035_message_id_prefixing_final_text_is_redacted() {
    let result = execute_scenario(
        "message_id_prefixes_final_text",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    // The unsafe id prefix is redacted, so neither the `api_` marker prefix
    // nor the reconstructed `api_key=` marker survives across the id and
    // content fields — the credential shape cannot reassemble even though the
    // value continues to appear only in its marker-less `key=…` form.
    assert!(!diagnostic.contains("api_"));
    assert!(!diagnostic.contains("api_key="));
    assert_eq!(result.spawns, 1);
}

/// Runs one dropped-identity fixture and asserts the credential is
/// unrecoverable from everything the adapter emitted for it.
///
/// Absorbs only the spawn plumbing every case repeats; the two
/// behaviour-relevant values — which fixture, which delivery mode — stay at the
/// call site. `#[track_caller]` cannot name a call site through an async fn, so
/// the case identity travels in the failure label instead.
async fn assert_identity_marker_suppresses_the_continuation(
    scenario: &str,
    delivery: DeliveryMode,
) {
    let result = execute_scenario(
        scenario,
        delivery,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_no_emitted_stream_carries(
        &format!("{scenario} {delivery:?}"),
        &result,
        fixtures::SENSITIVE_SPLIT_AUTHORIZATION,
    );
    assert_eq!(result.spawns, 1);
}

/// INV-035: a bare lifecycle event is dropped whole after its identity is only
/// validated as nonempty, so an `id` ending in a credential-marker prefix
/// (`api_`) is dropped provider text that seeds the lookbehind — the value
/// opening the text that follows (`key=<secret>`) is suppressed instead of
/// crossing the boundary in retained evidence.
#[tokio::test]
async fn inv_035_lifecycle_item_id_marker_suppresses_the_continuation() {
    assert_identity_marker_suppresses_the_continuation(
        "lifecycle_item_id_marker",
        DeliveryMode::Streamed,
    )
    .await;
    assert_identity_marker_suppresses_the_continuation(
        "lifecycle_item_id_marker",
        DeliveryMode::Buffered,
    )
    .await;
}

/// INV-035: the same boundary one field over. A lifecycle event's item `type`
/// is matched against nothing the adapter chose, so a `type` ending in the
/// marker prefix seeds the lookbehind exactly as the id does.
#[tokio::test]
async fn inv_035_lifecycle_item_type_marker_suppresses_the_continuation() {
    assert_identity_marker_suppresses_the_continuation(
        "lifecycle_item_type_marker",
        DeliveryMode::Streamed,
    )
    .await;
    assert_identity_marker_suppresses_the_continuation(
        "lifecycle_item_type_marker",
        DeliveryMode::Buffered,
    )
    .await;
}

/// INV-035: an unsupported item's `type` selected the catch-all arm rather than
/// one of the adapter's literals, so it is provider-chosen text the adapter
/// drops, and a marker prefix ending it governs the text that follows.
#[tokio::test]
async fn inv_035_unsupported_item_type_marker_suppresses_the_continuation() {
    assert_identity_marker_suppresses_the_continuation(
        "unsupported_item_type_marker",
        DeliveryMode::Streamed,
    )
    .await;
    assert_identity_marker_suppresses_the_continuation(
        "unsupported_item_type_marker",
        DeliveryMode::Buffered,
    )
    .await;
}

/// INV-035: a modeled reasoning item interprets its `type` (the adapter's own
/// literal) and its text, but never retains the id. The dropped id carries the
/// marker prefix that the item's own `key=` and the final-text value complete.
#[tokio::test]
async fn inv_035_reasoning_item_id_marker_suppresses_the_continuation() {
    assert_identity_marker_suppresses_the_continuation(
        "reasoning_item_id_marker",
        DeliveryMode::Streamed,
    )
    .await;
    assert_identity_marker_suppresses_the_continuation(
        "reasoning_item_id_marker",
        DeliveryMode::Buffered,
    )
    .await;
}

/// INV-035: the same shape on the dropped error item, whose message is
/// interpreted while its id is not.
#[tokio::test]
async fn inv_035_error_item_id_marker_suppresses_the_continuation() {
    assert_identity_marker_suppresses_the_continuation(
        "error_item_id_marker",
        DeliveryMode::Streamed,
    )
    .await;
    assert_identity_marker_suppresses_the_continuation(
        "error_item_id_marker",
        DeliveryMode::Buffered,
    )
    .await;
}

/// The control on the cases above, in buffered delivery: routine item identity
/// is not a credential. An ordinary id and a real Codex item type (`todo_list`,
/// whose trailing bytes the lookbehind does hold conservatively — they could
/// still grow into a credential name) fold like every other dropped field, and
/// the answer still reaches the caller byte-verbatim. Folding the identity
/// widens what can arm the scrubber; it does not make routine metadata suppress
/// the response.
#[tokio::test]
async fn buffered_benign_item_identity_leaves_the_answer_verbatim() {
    let result = execute_scenario(
        "benign_item_identity_before_answer",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);

    assert_eq!(
        completed.content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert!(!boundary_material(&result).contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// The same control in streamed delivery, which carries the further claim
/// buffered delivery has no stream to make: the answer is verbatim in the
/// delta stream too, read as the caller reads it — reassembled, since a
/// conservatively held trailing byte may arrive as its own fragment.
#[tokio::test]
async fn streamed_benign_item_identity_leaves_the_answer_verbatim() {
    let result = execute_scenario(
        "benign_item_identity_before_answer",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);

    assert_eq!(
        completed.content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert!(!boundary_material(&result).contains("[redacted]"));
    assert_eq!(
        emitted_streams(&result.observations),
        vec![fixtures::BUFFERED_ANSWER.to_string()]
    );
    assert_eq!(result.spawns, 1);
}

/// INV-035: an unsupported (unmodeled) streamed item whose text ends in a
/// credential marker prefix (`api_`) seeds the dropped lookbehind, so a final
/// text beginning with the continuation (`key=<secret>`) is suppressed rather
/// than releasing the credential the adapter never surfaced the item for.
#[tokio::test]
async fn inv_035_streamed_unsupported_item_marker_suppresses_the_continuation() {
    let result = execute_scenario(
        "credential_split_across_unsupported_item",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: the same unsupported-item marker governs the buffered final text,
/// which reaches terminal evidence without streamed deltas.
#[tokio::test]
async fn inv_035_buffered_unsupported_item_marker_suppresses_the_continuation() {
    let result = execute_scenario(
        "credential_split_across_unsupported_item",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential split as dropped `api_`, a held streamed `key` (safe
/// alone but unsafe as a continuation of `api_`), a dropped `=`, then the
/// value — the held bytes are treated as unsafe in the dropped context, so the
/// value is suppressed rather than emitted verbatim.
#[tokio::test]
async fn inv_035_context_dependent_held_bytes_stay_in_the_dropped_chain() {
    let result = execute_scenario(
        "credential_split_across_dropped_pending_and_error_separator",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker in a non-standard field of an unsupported
/// item (not `text`/`message`) still seeds the lookbehind, so a final text
/// completing it is suppressed.
#[tokio::test]
async fn inv_035_unsupported_item_nonstandard_field_seeds_the_lookbehind() {
    let result = execute_scenario(
        "unsupported_item_marker_in_nonstandard_field",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an unsupported item's benign field must not erase the credential
/// marker another field establishes, so a final text completing that marker is
/// still suppressed.
#[tokio::test]
async fn inv_035_unsupported_item_benign_field_does_not_erase_a_marker() {
    let result = execute_scenario(
        "unsupported_item_marker_beside_benign_field",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a held credential-prefix (`api_` from a reasoning delta) followed
/// by an unrelated dropped error item stays adjacent to a later emitted
/// `key=<secret>` in the output, so the value is suppressed — the redacted
/// prefix still marks the future value even though the dropped bytes broke
/// the internal candidate.
#[tokio::test]
async fn inv_035_held_prefix_suppresses_value_across_unrelated_error_item() {
    let result = execute_scenario(
        "held_credential_prefix_then_unrelated_error_item",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: held reasoning bytes unrelated to the credential (`Auth`), a
/// dropped error marker (`api_`), then a final-text value (`key=<secret>`)
/// reassemble chronologically as `api_key=<secret>` — the held bytes are
/// resolved out of the way rather than scanned between the dropped marker and
/// the value, so the credential is suppressed.
#[tokio::test]
async fn inv_035_credential_reassembles_across_held_reasoning_and_error_item() {
    let result = execute_scenario(
        "credential_reassembled_across_held_reasoning_and_error_item",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a marker held in the stream lookbehind (`Authorization` from a
/// reasoning delta), its separator supplied by an intervening dropped error
/// item (`:`), and the value in the final text must rejoin chronologically —
/// the dropped bytes fold through the pending held text rather than being
/// scanned in isolation, so the value is suppressed.
#[tokio::test]
async fn inv_035_error_item_separator_folds_through_held_reasoning() {
    let result = execute_scenario(
        "credential_split_across_error_item_after_held_reasoning",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: the dropped error-item marker also governs the buffered final
/// text, which reaches terminal evidence without streamed deltas.
#[tokio::test]
async fn inv_035_error_item_marker_suppresses_buffered_continuation() {
    let result = execute_scenario(
        "credential_split_across_error_item",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a credential marker held from streamed reasoning also governs the
/// agent-message item id, so an id extending the marker never surfaces as
/// `ProviderMessageId` in terminal evidence.
#[tokio::test]
async fn inv_035_split_authorization_value_before_message_id_is_redacted() {
    let result = execute_scenario(
        "split_stream_authorization_before_message_id",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an undeclared tool name that extends a held credential marker is
/// redacted in the resulting boundary-loss detail, not left verbatim.
#[tokio::test]
async fn inv_035_split_authorization_value_before_tool_name_is_redacted() {
    let result = execute_scenario(
        "split_stream_authorization_before_tool_name",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: the same rule where no joined-form scan could ever see the pair
/// rejoin — a marker prefix (`api_`) held in the dropped chain and its
/// continuation (`key=<value>`) quoted inside serde's own prose. The detail is
/// content-silent while any context is held, so the continuation cannot cross
/// in the provider error.
#[tokio::test]
async fn inv_035_decode_failure_detail_is_silent_while_a_dropped_marker_is_held() {
    let result = execute_scenario(
        "dropped_marker_then_malformed_usage",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let error = provider_error(&result.evidence);

    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(
        error.native.message.as_deref(),
        Some("undecodable Codex event: [redacted]")
    );
}

/// INV-035: a decode-failure detail that quotes provider-controlled bytes
/// consults the held lookbehind state *before* the adapter prefixes its own
/// prose, so a credential split between streamed reasoning and a malformed
/// event's quoted value is suppressed whole. Prefixing first would insert the
/// prose between the held marker and its continuation, leaving the pair unable
/// to rejoin and the continuation intact inside the message; the adapter's own
/// prose survives because it is not provider-derived.
#[tokio::test]
async fn inv_035_decode_failure_detail_consults_held_redaction_state() {
    let result = execute_scenario(
        "reasoning_then_malformed_usage",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let error = provider_error(&result.evidence);

    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(
        error.native.message.as_deref(),
        Some("undecodable Codex event: [redacted]")
    );
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

/// the tool fact is withheld, not fabricated, when this adapter
/// cannot observe it.
///
/// The CLI's item lifecycle carries no tool item and a turn's tool calls live
/// only inside the agent-message envelope, so a loss raised before that
/// envelope parses leaves the adapter with no view of the turn's tool
/// material. Reporting "none opened" there would be a claim it never
/// established.
#[tokio::test]
async fn a_loss_before_the_envelope_parses_withholds_the_tool_fact() {
    let result = execute_scenario(
        "credential_envelope_error",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        boundary_loss(&result.evidence).tool_calls,
        ToolCallsAtLoss::Unobserved
    );
}

/// Once the envelope parses the fact is in hand, so the withholding above is
/// not a blanket refusal to answer.
#[tokio::test]
async fn a_loss_after_the_envelope_parses_states_the_tool_fact() {
    let opened = execute_scenario(
        "tool_call",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    assert!(
        response_unintelligible(&boundary_loss(&opened.evidence).cause).contains("undeclared tool")
    );
    assert_eq!(
        boundary_loss(&opened.evidence).tool_calls,
        ToolCallsAtLoss::Opened
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

/// Defect regression: the envelope carries each tool call's provider-supplied
/// argument text inside a string because strict structured output forbids a
/// free-form object member. Malformed text remains an authoritative proposal
/// the provider-independent typed decoders classify, which the tool loop
/// projects as its `invalid_arguments` result.
#[tokio::test]
async fn non_json_string_carried_tool_arguments_are_preserved() {
    let result = execute_scenario(
        "tool_call_bad_arguments",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);

    assert_eq!(
        tool_proposal(&completed.content).arguments_json,
        fixtures::MALFORMED_TOOL_ARGUMENTS
    );
}

#[tokio::test]
async fn non_object_string_carried_tool_arguments_are_preserved() {
    let result = execute_scenario(
        "tool_call_non_object_arguments",
        DeliveryMode::Buffered,
        OperationShape::Tool,
        CancellationSignal::never(),
    )
    .await;
    let completed = completed(&result.evidence);

    assert_eq!(
        tool_proposal(&completed.content).arguments_json,
        fixtures::NON_OBJECT_TOOL_ARGUMENTS
    );
}

/// Preservation stops at the shared nesting bound. The bound applies to the
/// argument text carried inside the envelope string, which the line-level and
/// agent-message-level checks cannot see because string content does not nest
/// the outer JSON, and which the shared typed decoders could not decode past
/// serde_json's recursion boundary.
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

/// INV-035: an additively tolerated sibling field on a *known* completed item
/// is discarded by serde, so its credential marker seeds nothing unless the
/// decoder folds it; the envelope text the same item carries then completes the
/// marker and would reach observations and terminal evidence verbatim.
#[tokio::test]
async fn inv_035_agent_message_additive_field_marker_suppresses_the_continuation() {
    let result = execute_scenario(
        "agent_message_additive_field_marker",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: the same hole on `turn.failed` — an additive field holds the
/// marker its interpreted failure message completes, and the message is the
/// text that leaves the adapter as provider-error evidence.
#[tokio::test]
async fn inv_035_turn_failed_additive_field_marker_suppresses_the_message() {
    let result = execute_scenario(
        "turn_failed_additive_field_marker",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: a superseded agent message ending in a live marker (`api_`) beside
/// a clean id is folded id-first, in wire order, so the id cannot resolve the
/// marker away and release the value the replacing envelope text completes.
#[tokio::test]
async fn inv_035_superseded_message_marker_survives_a_clean_id() {
    let result = execute_scenario(
        "superseded_message_marker_before_clean_id",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an object nested inside an array keeps its fields as independent
/// dropped units; flattening them into the array's wire-adjacent run lets a
/// benign sibling erase the marker the other field holds.
#[tokio::test]
async fn inv_035_object_inside_an_array_keeps_its_fields_independent() {
    let result = execute_scenario(
        "unsupported_item_object_inside_an_array",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// INV-035: an unknown event's own metadata is dropped provider content — the
/// adapter matched its `type` against no known event and never reads its `id` —
/// so a credential marker either field holds still governs the final text that
/// completes it.
#[tokio::test]
async fn inv_035_unknown_event_metadata_marker_suppresses_the_continuation() {
    let result = execute_scenario(
        "unknown_event_metadata_marker",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
    assert_eq!(result.spawns, 1);
}

/// A textless explicit refusal is refusal evidence in streamed delivery, as it
/// already is in buffered delivery: the outcome is definitive on its own, so
/// the empty-material rule that guards ordinary completions does not
/// reclassify it as boundary loss by delivery mode.
#[tokio::test]
async fn streamed_textless_refusal_is_refusal_evidence() {
    let result = execute_scenario(
        "textless_refusal",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(refused(&result.evidence).content, Vec::new());
}

/// The same textless refusal in buffered delivery, which the streamed arm must
/// agree with.
#[tokio::test]
async fn buffered_textless_refusal_is_refusal_evidence() {
    let result = execute_scenario(
        "textless_refusal",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(refused(&result.evidence).content, Vec::new());
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
    assert!(!error.non_acceptance_proven);
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

/// Defect regression: the pinned CLI reports a failed exchange as a
/// stream-level `error` event followed by its `turn.failed` lifecycle echo. The
/// decoder accepts exactly that trailer and keeps the stream-level message, so
/// the typed provider error is never downgraded to a post-terminal protocol
/// violation. This fixture is the record of that sequencing; the gated
/// compatibility smoke could not have supplied it, because no run of that
/// workflow had ever authenticated when this case was written.
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
    assert!(error.non_acceptance_proven);
    assert_eq!(
        error.native.message.as_deref(),
        Some(fixtures::STREAM_ERROR_MESSAGE)
    );
    assert_eq!(result.spawns, 1);
}

/// A stream-level `error` that the process never echoes as `turn.failed`
/// classifies the cause but proves nothing about acceptance.
///
/// The substitution contract admits the Codex proof only on the exact,
/// noncontradictory `turn.failed` closure. A truncated stream, or an exit after
/// a lone `error` event, may still follow a request the provider accepted, so
/// reporting the proof here would let credential-pool rotation reissue the turn
/// under a second account and duplicate billed work.
#[tokio::test]
async fn a_stream_error_without_its_turn_failed_echo_proves_no_non_acceptance() {
    let result = execute_scenario(
        "error_without_turn_failed",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let error = provider_error(&result.evidence);

    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert!(!error.non_acceptance_proven);
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

/// A syntactically valid `turn.failed` trailer carrying a different failure
/// than the stream error it follows is a contradiction, not the lifecycle
/// echo, and fails closed instead of silently keeping either message.
#[tokio::test]
async fn a_contradictory_turn_failed_trailer_still_fails_closed() {
    assert_error_scenario(
        "error_then_contradictory_turn_failed",
        ProviderErrorKind::Unrecognized,
    )
    .await;
}

/// Completion without the thread that establishes the exchange is protocol
/// drift, not success: it fails closed instead of returning completion
/// evidence with empty exchange facts.
#[cfg(unix)]
#[tokio::test]
async fn completion_without_an_established_thread_fails_closed() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = threadless_completed_cli(temporary.path());
    let runtime = runtime(temporary.path(), executable);
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
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::Unrecognized);
    assert!(
        error
            .native
            .message
            .as_deref()
            .expect("the failure names the missing thread")
            .contains("before thread.started")
    );
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

#[tokio::test]
async fn partial_usage_records_only_the_axes_codex_reports() {
    let result = execute_scenario(
        "usage_partial_axes",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(
        completed(&result.evidence).usage,
        TokenUsage {
            input_tokens: None,
            output_tokens: Some(fixtures::OUTPUT_TOKENS),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(fixtures::CACHE_READ_INPUT_TOKENS),
        }
    );
}

#[tokio::test]
async fn total_only_usage_does_not_invent_axis_counts() {
    let result = execute_scenario(
        "usage_total_only",
        DeliveryMode::Buffered,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(completed(&result.evidence).usage, TokenUsage::unreported());
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

/// A leader that already exited while a surviving descendant held the
/// inherited stdin read end (keeping the oversized upload blocked past the
/// deadline) is preserved at the upload-deadline arm: its definitive nonzero
/// status classifies as a typed provider error instead of being laundered
/// into timeout loss by the group kill, mirroring the cancellation arm's
/// work-first probe.
#[cfg(unix)]
#[tokio::test]
async fn upload_deadline_preserves_an_exited_leader_with_held_stdin() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    std::fs::write(
        temporary
            .path()
            .join(fixtures::EARLY_STDIN_HELD_EXIT_MARKER),
        "hold",
    )
    .expect("the held-stdin exit marker is created");
    let runtime = runtime_with_timeout(temporary.path(), fake_cli(), Duration::from_secs(2));
    let prepared = prepare(&runtime, blocked_input_operation()).await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    let failure = provider_error(&report.evidence);

    assert_eq!(failure.kind, ProviderErrorKind::CredentialRejected);
    assert!(
        failure
            .native
            .message
            .as_deref()
            .is_some_and(|message| message.contains("authentication failed"))
    );
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-stdin-held-group"));
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
        None,
    );
    config.exchange_timeout = Some(Duration::from_secs(5));
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
        None,
    );
    config.exchange_timeout = Some(Duration::from_secs(5));
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

/// Cancellation that lands after the terminal marker, while the leader has
/// closed both output pipes but keeps running, drives immediate group
/// cleanup and returns the definitive completion instead of waiting out the
/// exchange deadline.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_after_terminal_with_closed_pipes_preserves_completion_evidence() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = pipes_closing_completed_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(5));
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-pipes-close-ready"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert_eq!(
        completed(&report.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-pipes-close-group"));
}

/// INV-035: stderr appended to exit-status detail consults the held
/// lookbehind state before adapter-owned prose is prefixed, so a credential
/// split between streamed text and stderr cannot reassemble in
/// provider-error evidence.
#[tokio::test]
async fn inv_035_stderr_exit_detail_consults_held_redaction_state() {
    let result = execute_scenario(
        "stderr_credential_continuation",
        DeliveryMode::Streamed,
        OperationShape::Text,
        CancellationSignal::never(),
    )
    .await;
    let error = provider_error(&result.evidence);

    let diagnostic = boundary_material(&result);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_STDERR_CONTINUATION));
    assert!(diagnostic.contains("[redacted]"));
    assert!(
        error
            .native
            .message
            .as_deref()
            .expect("the failure carries redacted stderr detail")
            .contains("[redacted]")
    );
}

/// Cancellation that lands after the provider terminal marker, while an open
/// stdout handle still blocks end-of-stream, drives immediate group cleanup
/// and returns the definitive completion instead of discarding it.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_after_a_terminal_marker_preserves_completion_evidence() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stdout_holding_completed_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(5));
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-stdout-hold-ready"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert_eq!(
        completed(&report.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-stdout-hold-group"));
}

/// A leader that already exited zero after its terminal marker keeps its
/// completion evidence at the exchange deadline even while a surviving
/// descendant holds the inherited stdout handle open.
#[cfg(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
))]
#[tokio::test(start_paused = true)]
async fn inherited_stdout_cannot_extend_process_cleanup_past_deadline() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stdout_inheriting_completed_cli(temporary.path());
    let exchange_timeout = Duration::from_secs(5);
    let runtime = runtime_with_timeout(temporary.path(), executable, exchange_timeout);
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let process_group_path = temporary.path().join("fake-codex-stdout-inherit-group");
    let execution = tokio::spawn(async move {
        let mut observations = Vec::new();
        runtime
            .execute(prepared, &mut observations, CancellationSignal::never())
            .await
    });

    let readiness_path = process_group_path.clone();
    tokio::task::spawn_blocking(move || {
        wait_for_recorded_process_group_leader_exit(&readiness_path);
    })
    .await
    .expect("the wall-clock readiness barrier completes");
    tokio::time::advance(exchange_timeout).await;
    let report = execution
        .await
        .expect("the deadline-driven execution task completes");

    assert_eq!(
        completed(&report.evidence).content,
        vec![AssistantPart::Text(fixtures::BUFFERED_ANSWER.to_string())]
    );
    assert_recorded_process_group_exited(process_group_path);
}

/// A leader that exited nonzero before any terminal marker keeps its exit
/// status as provider-error evidence at the exchange deadline even while a
/// surviving descendant holds the inherited stdout handle open.
#[cfg(unix)]
#[tokio::test]
async fn inherited_stdout_cannot_demote_a_nonzero_exit_to_timeout_loss() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stdout_inheriting_failure_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(5));
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
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::Unrecognized);
    assert!(
        error
            .native
            .message
            .as_deref()
            .expect("the failure names the exit status")
            .contains("exit status: 7")
    );
    assert_recorded_process_group_exited(
        temporary
            .path()
            .join("fake-codex-stdout-inherit-failure-group"),
    );
}

/// A leader already dead from a kill signal before the cleanup deadline — as
/// under an out-of-memory kill — keeps that signal exit as provider-error
/// evidence instead of being read as a cleanup kill, even while a surviving
/// descendant holds the inherited stderr handle open.
#[cfg(unix)]
#[tokio::test]
async fn stderr_deadline_preserves_a_pre_existing_kill_signal_exit() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stderr_inheriting_killed_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(5));
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
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::Unrecognized);
    assert!(
        error
            .native
            .message
            .as_deref()
            .expect("the failure names the exit signal")
            .contains("signal: 9")
    );
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-stderr-kill-group"));
}

/// INV-035: a `thread_id` that continues a credential marker held from a
/// drifted earlier reasoning delta is sanitized against the held state, so it
/// escapes neither the `ExchangeEstablished` observation nor the terminal
/// exchange facts.
#[cfg(unix)]
#[tokio::test]
async fn inv_035_drifted_thread_id_is_redacted_against_held_state() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = reasoning_before_thread_started_cli(temporary.path());
    let runtime = runtime(temporary.path(), executable);
    let prepared = prepare(
        &runtime,
        operation("drift", DeliveryMode::Streamed, OperationShape::Text),
    )
    .await;
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;
    let diagnostic = boundary_material_from(&report.evidence, &observations);

    assert!(!diagnostic.contains(fixtures::SENSITIVE_SPLIT_AUTHORIZATION));
    assert!(diagnostic.contains("[redacted]"));
}

/// A nonzero exit is classified from the bounded raw stderr, so an error
/// phrase sharing a line with a consumed credential marker still yields the
/// correct typed kind while the emitted message stays sanitized.
#[cfg(unix)]
#[tokio::test]
async fn stderr_is_classified_before_credential_redaction() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stdout_holding_masked_credential_failure_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(10));
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
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
    assert!(
        !error
            .native
            .message
            .as_deref()
            .expect("the failure carries a message")
            .contains("opaque-session-secret")
    );
    assert_recorded_process_group_exited(
        temporary.path().join("fake-codex-masked-credential-group"),
    );
}

/// Work-first: a cancellation arriving after a terminal marker but a nonzero
/// exit classifies the failed invocation as a provider error, so cancellation
/// cannot launder it into the recorded completion.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_after_terminal_marker_preserves_a_nonzero_exit() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = completed_then_nonzero_exit_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(60));
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let cancellation =
        cancel_after_record(temporary.path().join("fake-codex-completed-nonzero-ready"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;

    assert!(
        matches!(report.evidence, TerminalEvidence::ProviderError(_)),
        "a nonzero exit after a terminal marker is a provider error, got {:?}",
        report.evidence
    );
}

/// Work-first: a cancellation arriving after the leader has already exited
/// nonzero keeps the definitive provider-error evidence instead of reporting
/// cancellation loss.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_after_a_nonzero_exit_keeps_provider_error() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = exited_then_cancellable_failure_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(10));
    let prepared = prepare(
        &runtime,
        operation(
            "buffered_completed",
            DeliveryMode::Buffered,
            OperationShape::Text,
        ),
    )
    .await;
    let cancellation = cancel_after_record(temporary.path().join("fake-codex-cancel-exit-ready"));
    let mut observations = Vec::new();

    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-cancel-exit-group"));
}

/// INV-035 / evidence: a leader that wrote a classifiable stderr failure,
/// closed stderr, and exited nonzero keeps that failure's typed kind at the
/// stdout-cleanup deadline — even while a descendant holds stdout open —
/// instead of degrading to the synthetic "stderr unavailable" message.
#[cfg(unix)]
#[tokio::test]
async fn completed_stderr_is_preserved_during_stdout_cleanup() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stdout_holding_credential_failure_cli(temporary.path());
    // A generous deadline so the already-finished stderr reader is reliably
    // observed under heavy parallel test load, not raced by the deadline.
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(10));
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
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
    assert!(
        error
            .native
            .message
            .as_deref()
            .expect("the failure carries the stderr detail")
            .contains("authentication failed")
    );
    assert_recorded_process_group_exited(
        temporary.path().join("fake-codex-stderr-credential-group"),
    );
}

/// INV / evidence: a leader that wrote a classifiable stderr failure but handed
/// its stderr to a surviving descendant (so the reader is not yet finished at
/// the cleanup deadline) still keeps that failure's typed kind. The group kill
/// closes the descendant's write end, and the bounded drain awaits the reader
/// rather than aborting it, so the buffered `authentication failed` is not
/// discarded and degraded to `Unrecognized`.
#[cfg(unix)]
#[tokio::test]
async fn held_stderr_is_drained_during_stdout_cleanup() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = stderr_held_by_descendant_credential_failure_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(2));
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
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
    assert!(
        error
            .native
            .message
            .as_deref()
            .expect("the failure carries the drained stderr detail")
            .contains("authentication failed")
    );
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-stderr-held-group"));
}

/// A nonterminal line read after the deadline — while a descendant keeps
/// stdout alive but the leader has already exited — preserves the leader's
/// definitive nonzero status at the post-line deadline check: it classifies
/// as a typed provider error instead of being force-killed into timeout loss,
/// mirroring the adjacent cancellation arm's work-first probe.
#[cfg(unix)]
#[tokio::test]
async fn post_line_deadline_preserves_an_exited_leader() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = keepalive_after_exit_credential_failure_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(2));
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
    let error = provider_error(&report.evidence);

    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
    assert!(
        error
            .native
            .message
            .as_deref()
            .expect("the failure carries the drained stderr detail")
            .contains("authentication failed")
    );
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-keepalive-group"));
}

/// An adversarial continuous flood of padded keepalive events — which keeps
/// the biased read arm always ready AND the reader buffer non-empty — cannot
/// starve the exchange deadline: the post-line control checks run after every
/// decoded line, so the exchange ends in bounded time as timeout loss.
#[cfg(unix)]
#[tokio::test]
async fn adversarial_keepalive_flood_cannot_starve_the_deadline() {
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let executable = starving_keepalive_flood_cli(temporary.path());
    let runtime = runtime_with_timeout(temporary.path(), executable, Duration::from_secs(1));
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

    let report = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.execute(prepared, &mut observations, CancellationSignal::never()),
    )
    .await
    .expect("a continuous flood must not starve the exchange deadline");

    assert_eq!(
        timed_out(&boundary_loss(&report.evidence).cause).detail,
        "Codex CLI process exceeded its exchange timeout"
    );
    assert_recorded_process_group_exited(temporary.path().join("fake-codex-starving-flood-group"));
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

#[tokio::test]
async fn selected_member_home_reaches_each_spawn_and_changes_with_the_reference() {
    // Pool rotation reaches the adapter by pinning the successor member's
    // credential reference on the next operation. Exercise both sides of that
    // boundary: the same runtime must deliver the pre- and post-rotation homes
    // from those exact references.
    let temporary = tempfile::tempdir().expect("test working directory is created");
    let first_home = temporary.path().join("synthetic-home-a");
    let second_home = temporary.path().join("synthetic-home-b");
    std::fs::create_dir(&first_home).expect("first synthetic home is created");
    std::fs::create_dir(&second_home).expect("second synthetic home is created");
    std::fs::write(first_home.join("fixture-marker"), "synthetic")
        .expect("first synthetic home is nonempty");
    std::fs::write(second_home.join("fixture-marker"), "synthetic")
        .expect("second synthetic home is nonempty");
    let first_reference = CredentialReference::new("codex-a");
    let second_reference = CredentialReference::new("codex-b");
    let mut config =
        CodexCliConfig::new(fake_cli(), temporary.path(), first_reference.clone(), None)
            .with_credential_homes([
                (first_reference.clone(), first_home.clone()),
                (second_reference.clone(), second_home.clone()),
            ]);
    config.exchange_timeout = Some(OFFLINE_HARNESS_TIMEOUT);
    config.interrupt_grace = Duration::from_millis(100);
    let runtime = CodexCliRuntime::new(config).expect("synthetic homes are admitted");
    let mut first_operation = operation(
        "selected_credential_home",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    first_operation.credential_reference = first_reference;
    let first_prepared = prepare(&runtime, first_operation).await;
    let mut first_observations = Vec::new();
    runtime
        .execute(
            first_prepared,
            &mut first_observations,
            CancellationSignal::never(),
        )
        .await;
    let delivered_first =
        std::fs::read_to_string(temporary.path().join("fake-codex-selected-home"))
            .expect("first spawn records its synthetic home");
    assert_eq!(delivered_first, first_home.to_string_lossy());
    let mut second_operation = operation(
        "selected_credential_home",
        DeliveryMode::Buffered,
        OperationShape::Text,
    );
    second_operation.credential_reference = second_reference;
    let second_prepared = prepare(&runtime, second_operation).await;
    let mut second_observations = Vec::new();
    runtime
        .execute(
            second_prepared,
            &mut second_observations,
            CancellationSignal::never(),
        )
        .await;
    let delivered_second =
        std::fs::read_to_string(temporary.path().join("fake-codex-selected-home"))
            .expect("second spawn records its synthetic home");
    assert_eq!(delivered_second, second_home.to_string_lossy());
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
    let diagnostic = boundary_material(&result);

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
    let diagnostic = boundary_material(&result);

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
    let diagnostic = boundary_material(&result);

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
    let diagnostic = boundary_material(&result);

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
        None,
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
        None,
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
        None,
    );
    config.exchange_timeout = Some(Duration::MAX);

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
        None,
    );
    config.exchange_timeout = Some(exchange_timeout);
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
    test_bin_path!("signalbox-fake-codex-cli")
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

/// Writes an executable shell-script fake CLI and returns its path.
///
/// Every scripted CLI consumes the request upload before its own body runs.
/// The adapter writes the rendered request to the child's stdin and only then
/// begins reading stdout, so a script that exits without reading stdin races
/// that write. The leader is the sole holder of the pipe's read end — POSIX
/// hands an asynchronous list `/dev/null` for stdin, so a backgrounded
/// descendant never inherits it — and once the leader exits, the adapter's
/// write fails with `EPIPE`. The adapter records that as an incomplete request
/// upload, which demotes an otherwise complete exchange to boundary loss and
/// displaces the message of a bare nonzero exit. Draining first makes every
/// scripted exit lose that race by construction, so these fixtures assert on
/// the behaviour they name rather than on process scheduling.
///
/// Fixtures that must leave the upload incomplete — `stderr_holding_incomplete_upload_cli`
/// closes stdin outright — write their own executable instead of using this helper.
#[cfg(unix)]
fn script_cli(directory: &Path, name: &str, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let body = script
        .strip_prefix("#!/bin/sh\n")
        .expect("a scripted fake CLI opens with the POSIX shell shebang");
    let script = format!("#!/bin/sh\ncat >/dev/null\n{body}");
    let executable = directory.join(name);
    std::fs::write(&executable, script).expect("the scripted fake CLI is written");
    let mut permissions = std::fs::metadata(&executable)
        .expect("the scripted fake CLI has metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions)
        .expect("the scripted fake CLI is executable");
    executable
}

/// The JSONL lines of one complete buffered exchange, shell-quoted for the
/// scripted fake CLIs below.
#[cfg(unix)]
fn completed_exchange_script_lines() -> String {
    let envelope_text = format!(
        r#"{{\"outcome\":\"completed\",\"text\":\"{}\",\"tool_calls\":[]}}"#,
        fixtures::BUFFERED_ANSWER
    );
    format!(
        r#"printf '%s\n' '{{"type":"thread.started","thread_id":"{thread_id}"}}'
printf '%s\n' '{{"type":"turn.started"}}'
printf '%s\n' '{{"type":"item.completed","item":{{"id":"message-offline-1","type":"agent_message","text":"{envelope_text}"}}}}'
printf '%s\n' '{{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":{cache_read},"cache_write_input_tokens":{cache_write},"output_tokens":{output},"reasoning_output_tokens":3}}}}'
"#,
        thread_id = fixtures::THREAD_ID,
        envelope_text = envelope_text,
        input = fixtures::INPUT_TOKENS,
        cache_read = fixtures::CACHE_READ_INPUT_TOKENS,
        cache_write = fixtures::CACHE_CREATION_INPUT_TOKENS,
        output = fixtures::OUTPUT_TOKENS,
    )
}

/// Scripts a CLI whose reasoning (ending in a credential marker) drifts ahead
/// of `thread.started`, whose `thread_id` is an opaque continuation of that
/// marker. Exercises the held-state sanitization of the thread id.
#[cfg(unix)]
fn reasoning_before_thread_started_cli(directory: &Path) -> std::path::PathBuf {
    let envelope_text = format!(
        r#"{{\"outcome\":\"completed\",\"text\":\"{}\",\"tool_calls\":[]}}"#,
        fixtures::BUFFERED_ANSWER
    );
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' '{{"type":"turn.started"}}'
printf '%s\n' '{{"type":"item.completed","item":{{"id":"reason-drift","type":"reasoning","text":"Authorization:"}}}}'
printf '%s\n' '{{"type":"thread.started","thread_id":" {authorization}"}}'
printf '%s\n' '{{"type":"item.completed","item":{{"id":"message-offline-1","type":"agent_message","text":"{envelope_text}"}}}}'
printf '%s\n' '{{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":{cache_read},"cache_write_input_tokens":{cache_write},"output_tokens":{output},"reasoning_output_tokens":3}}}}'
"#,
        authorization = fixtures::SENSITIVE_SPLIT_AUTHORIZATION,
        envelope_text = envelope_text,
        input = fixtures::INPUT_TOKENS,
        cache_read = fixtures::CACHE_READ_INPUT_TOKENS,
        cache_write = fixtures::CACHE_CREATION_INPUT_TOKENS,
        output = fixtures::OUTPUT_TOKENS,
    );
    script_cli(directory, "reasoning-drift-codex", &script)
}

/// Scripts a CLI whose stderr places an explicit error phrase after a
/// line-scoped credential marker, then exits nonzero with a stdout-holding
/// descendant. The sanitized message consumes the marker's line, so the
/// failure can only classify correctly from the bounded raw stderr.
#[cfg(unix)]
fn stdout_holding_masked_credential_failure_cli(directory: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
printf 'Authorization: opaque-session-secret authentication failed\n' >&2
sleep 60 0<&- 2>&- &
printf 'process_group=%s\ndescendant=%s\n' "$$" "$!" > fake-codex-masked-credential-group
exec 2>&-
exit 7
"#;
    script_cli(directory, "stderr-masked-codex", script)
}

/// Scripts a CLI that finishes a complete `turn.completed` exchange, then
/// exits nonzero while a descendant holds stdout open and — after the leader
/// exits — signals readiness. A cancellation arriving after the exit must not
/// launder the failed invocation into the recorded completion.
#[cfg(unix)]
fn completed_then_nonzero_exit_cli(directory: &Path) -> std::path::PathBuf {
    // The leader exits at once, becoming an unreaped zombie the adapter only
    // reaps at its deadline; a `kill -0` poll cannot see that exit (zombies
    // still answer the signal probe), so a fixed delay well under the long
    // exchange timeout below lets the cancellation fire while the leader is
    // already exited, deterministically reaching the cancellation arm.
    let script = format!(
        r#"#!/bin/sh
{lines}( sleep 3
  printf 'ready\n' > fake-codex-completed-nonzero-ready
  sleep 60 ) 0<&- 2>&- &
printf 'process_group=%s\ndescendant=%s\n' "$$" "$!" > fake-codex-completed-nonzero-group
exit 7
"#,
        lines = completed_exchange_script_lines()
    );
    script_cli(directory, "completed-nonzero-codex", &script)
}

/// Scripts a CLI that exits nonzero after a classifiable stderr while a
/// descendant both holds stdout open and signals readiness, so a cancellation
/// can be timed to arrive after the leader has already exited.
#[cfg(unix)]
fn exited_then_cancellable_failure_cli(directory: &Path) -> std::path::PathBuf {
    // The descendant polls until the leader has actually exited before writing
    // the readiness marker, so the cancellation deterministically arrives after
    // the exit even under heavy parallel test load rather than after a fixed
    // sleep that could race the exit.
    let script = r#"#!/bin/sh
leader=$$
printf 'authentication failed\n' >&2
( while kill -0 "$leader" 2>/dev/null; do sleep 0.05; done
  printf 'ready\n' > fake-codex-cancel-exit-ready
  sleep 60 ) 0<&- 2>&- &
printf 'process_group=%s\ndescendant=%s\n' "$$" "$!" > fake-codex-cancel-exit-group
exec 2>&-
exit 7
"#;
    script_cli(directory, "cancel-after-exit-codex", script)
}

/// Scripts a CLI that writes a classifiable credential-rejection to stderr,
/// closes stderr, hands a stdout-holding descendant the pipe, and exits
/// nonzero. The stdout-decode loop then reaches its deadline with the leader
/// already exited and stderr already complete, exercising the branch that
/// must consume the finished stderr instead of the synthetic cleanup message.
#[cfg(unix)]
fn stdout_holding_credential_failure_cli(directory: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
printf 'authentication failed
' >&2
sleep 60 0<&- 2>&- &
printf 'process_group=%s
descendant=%s
' "$$" "$!" > fake-codex-stderr-credential-group
exec 2>&-
exit 7
"#;
    script_cli(directory, "stderr-credential-codex", script)
}

/// Scripts a CLI that emits its nonterminal preamble, writes a classifiable
/// stderr failure, hands stdout to a descendant that floods benign unknown
/// keepalive events continuously, and exits nonzero. The flood keeps the
/// biased select's read arm always ready, so the deadline is only ever
/// noticed by the post-line check — on a freshly read line while the
/// leader's definitive status is already waitable.
#[cfg(unix)]
fn keepalive_after_exit_credential_failure_cli(directory: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
printf '%s
' '{{"type":"thread.started","thread_id":"{thread_id}"}}'
printf '%s
' '{{"type":"turn.started"}}'
printf 'authentication failed
' >&2
yes '{{"type":"keepalive"}}' &
printf 'process_group=%s
descendant=%s
' "$$" "$!" > fake-codex-keepalive-group
exit 7
"#,
        thread_id = fixtures::THREAD_ID
    );
    script_cli(directory, "keepalive-exit-codex", &script)
}

/// Scripts a CLI that floods keepalive events continuously without exiting,
/// from a pregenerated block whose event boundaries never land on a multiple
/// of the reader's 8 KiB refill (offsets 0 and 4096 mod 8192 are avoided and
/// the block length is 4096 mod 8192, so the avoidance holds across `cat`
/// iterations while the pipe stays saturated). While the post-line control
/// checks were gated on an empty reader buffer, this shape starved the
/// exchange deadline; the checks now run after every decoded line.
#[cfg(unix)]
fn starving_keepalive_flood_cli(directory: &Path) -> std::path::PathBuf {
    // Preamble bytes already on stdout when the flood starts.
    let preamble = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{}\"}}\n{{\"type\":\"turn.started\"}}\n",
        fixtures::THREAD_ID
    );
    let mut block = String::new();
    let mut total = preamble.len();
    // Build ~16 MiB whose line ends avoid 0 and 4096 mod 8192; ending the
    // block at 4096 mod 8192 shifts the orbit {0, 4096} each iteration, so
    // the avoided residues cover every pass.
    while total < preamble.len() + 16 * 1024 * 1024 || (total - preamble.len()) % 8192 != 4096 {
        let mut line = format!(
            "{{\"type\":\"keepalive\",\"padding\":\"{}\"}}\n",
            "a".repeat(900)
        );
        let mut end = (total + line.len()) % 8192;
        while end == 0 || end == 4096 {
            line = line.replace("\"}}\n", "a\"}}\n");
            end = (total + line.len()) % 8192;
        }
        block.push_str(&line);
        total += line.len();
    }
    std::fs::write(directory.join("fake-codex-flood-block"), block)
        .expect("the flood block is written");
    let script = format!(
        r#"#!/bin/sh
printf '%s
' '{{"type":"thread.started","thread_id":"{thread_id}"}}'
printf '%s
' '{{"type":"turn.started"}}'
printf 'process_group=%s
descendant=%s
' "$$" "$$" > fake-codex-starving-flood-group
while :; do cat fake-codex-flood-block; done
"#,
        thread_id = fixtures::THREAD_ID
    );
    script_cli(directory, "starving-flood-codex", &script)
}

/// Scripts a CLI that writes a classifiable stderr failure, then hands its
/// stdout and stderr handles to a surviving descendant (so neither pipe reaches
/// EOF on its own) and exits nonzero. Unlike `stdout_holding_credential_failure_cli`,
/// the leader never closes stderr, so at the cleanup deadline the reader is not
/// yet `is_finished()`; recovering the buffered failure requires draining it
/// after the group kill closes the descendant's write end.
#[cfg(unix)]
fn stderr_held_by_descendant_credential_failure_cli(directory: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
printf 'authentication failed
' >&2
sleep 60 &
printf 'process_group=%s
descendant=%s
' "$$" "$!" > fake-codex-stderr-held-group
exit 7
"#;
    script_cli(directory, "stderr-held-codex", script)
}

/// Scripts a CLI that finishes a complete exchange, then keeps stdout open
/// without exiting. The readiness marker is written a settling second after
/// the terminal marker so a marker-watching cancellation cannot fire before
/// the adapter has consumed the terminal.
#[cfg(unix)]
fn stdout_holding_completed_cli(directory: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
{lines}printf 'process_group=%s\ndescendant=%s\n' "$$" "$$" > fake-codex-stdout-hold-group
sleep 1
printf 'ready\n' > fake-codex-stdout-hold-ready
sleep 60
"#,
        lines = completed_exchange_script_lines()
    );
    script_cli(directory, "stdout-holding-codex", &script)
}

/// Scripts a CLI that finishes a complete exchange, hands its stdout and
/// stderr handles to a surviving descendant, and exits successfully.
#[cfg(unix)]
fn stdout_inheriting_completed_cli(directory: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
{lines}sleep 60 &
printf 'process_group=%s\ndescendant=%s\n' "$$" "$!" > fake-codex-stdout-inherit-group
exit 0
"#,
        lines = completed_exchange_script_lines()
    );
    script_cli(directory, "stdout-inheriting-codex", &script)
}

/// Scripts a CLI that hands its stdout and stderr handles to a surviving
/// descendant and exits nonzero before any terminal marker.
#[cfg(unix)]
fn stdout_inheriting_failure_cli(directory: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' '{{"type":"thread.started","thread_id":"{thread_id}"}}'
printf '%s\n' '{{"type":"turn.started"}}'
sleep 60 &
printf 'process_group=%s\ndescendant=%s\n' "$$" "$!" > fake-codex-stdout-inherit-failure-group
exit 7
"#,
        thread_id = fixtures::THREAD_ID
    );
    script_cli(directory, "stdout-failing-codex", &script)
}

/// Scripts a CLI that hands only its stderr handle to a surviving
/// descendant, closes stdout, and dies from an externally shaped kill signal
/// before cleanup begins.
#[cfg(unix)]
fn stderr_inheriting_killed_cli(directory: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' '{{"type":"thread.started","thread_id":"{thread_id}"}}'
printf '%s\n' '{{"type":"turn.started"}}'
sleep 60 >/dev/null &
printf 'process_group=%s\ndescendant=%s\n' "$$" "$!" > fake-codex-stderr-kill-group
exec 1>&-
kill -KILL $$
"#,
        thread_id = fixtures::THREAD_ID
    );
    script_cli(directory, "stderr-killed-codex", &script)
}

/// Scripts a CLI that finishes a complete exchange, closes both output
/// pipes, and keeps running. The readiness marker is written a settling
/// second later so a marker-watching cancellation cannot fire before the
/// adapter has consumed the terminal and parked in its exit wait.
#[cfg(unix)]
fn pipes_closing_completed_cli(directory: &Path) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
{lines}printf 'process_group=%s\ndescendant=%s\n' "$$" "$$" > fake-codex-pipes-close-group
exec 1>&- 2>&-
sleep 1
printf 'ready\n' > fake-codex-pipes-close-ready
sleep 60
"#,
        lines = completed_exchange_script_lines()
    );
    script_cli(directory, "pipes-closing-codex", &script)
}

/// Scripts a CLI that completes a turn without ever establishing a thread.
#[cfg(unix)]
fn threadless_completed_cli(directory: &Path) -> std::path::PathBuf {
    let envelope_text = format!(
        r#"{{\"outcome\":\"completed\",\"text\":\"{}\",\"tool_calls\":[]}}"#,
        fixtures::BUFFERED_ANSWER
    );
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' '{{"type":"turn.started"}}'
printf '%s\n' '{{"type":"item.completed","item":{{"id":"message-offline-1","type":"agent_message","text":"{envelope_text}"}}}}'
printf '%s\n' '{{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":{cache_read},"cache_write_input_tokens":{cache_write},"output_tokens":{output},"reasoning_output_tokens":3}}}}'
"#,
        envelope_text = envelope_text,
        input = fixtures::INPUT_TOKENS,
        cache_read = fixtures::CACHE_READ_INPUT_TOKENS,
        cache_write = fixtures::CACHE_CREATION_INPUT_TOKENS,
        output = fixtures::OUTPUT_TOKENS,
    );
    script_cli(directory, "threadless-codex", &script)
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

    let process_group = recorded_process_group(&path);
    let deadline = std::time::Instant::now() + PROCESS_EXIT_OBSERVATION_TIMEOUT;
    while process_group_exists(process_group) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_group_exists(process_group),
        "the recorded process group remains alive after cleanup"
    );
}

#[cfg(unix)]
fn recorded_process_group(path: &Path) -> rustix::process::Pid {
    let record = std::fs::read_to_string(path)
        .expect("the fake CLI records its process group and descendant identities");
    let raw_process_group = record
        .lines()
        .find_map(|line| line.strip_prefix("process_group="))
        .expect("the process-group record names the process group")
        .parse::<i32>()
        .expect("the recorded process-group identity is a process id");
    rustix::process::Pid::from_raw(raw_process_group)
        .expect("the process-group identity is nonzero")
}

#[cfg(not(unix))]
fn assert_recorded_process_group_exited(_path: std::path::PathBuf) {}

#[cfg(unix)]
fn process_group_exists(process_group: rustix::process::Pid) -> bool {
    rustix::process::test_kill_process_group(process_group).is_ok()
        && process_group_has_live_member(process_group)
}

#[cfg(all(
    unix,
    not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))
))]
fn wait_for_recorded_process_group_leader_exit(path: &Path) {
    let deadline = std::time::Instant::now() + OFFLINE_HARNESS_TIMEOUT;
    while std::fs::read_to_string(path)
        .map(|content| content.lines().count())
        .unwrap_or_default()
        == 0
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the fake CLI records its process group before the wall-clock bound"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let process_group = recorded_process_group(path);
    loop {
        let exited = rustix::process::waitid(
            rustix::process::WaitId::Pid(process_group),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOWAIT
                | rustix::process::WaitIdOptions::NOHANG,
        )
        .expect("the fake CLI leader remains waitable")
        .is_some();
        if exited {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fake CLI leader exits before the wall-clock bound"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
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

fn stream_protocol_violation(cause: &LossCause) -> &str {
    let LossCause::StreamProtocolViolation { detail } = cause else {
        panic!("expected stream-protocol violation, got {cause:?}");
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
            | AssistantPart::RedactedThinking { .. }
            | AssistantPart::SuppressedToolCall(_) => None,
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
