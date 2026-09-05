//! End-to-end adapter tests against a canned loopback HTTP server.
//!
//! No live provider is contacted and no credential exists: the server is a
//! local socket replaying canned bytes, which lets these tests assert the
//! real transport path — headers sent, redirect discipline, connect-failure
//! classification, and stream-integrity evidence — deterministically.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionEvidence, CompletionFinish, ConversationMessage,
    ConversationRole, DeliveryMode, FastMode, FastModeTarget, InputTokenCountOutcome, LossCause,
    MessagePart, ModelCapabilities, ModelCapabilityCatalog, ModelCapabilityDefinition,
    ModelInputTokenCounter, ModelOperation, ModelRuntime, ModelSettings, Observation,
    ObservationFact, PROVIDER_JSON_NESTING_LIMIT, PreparationFailure, PreparationOutcome,
    ProviderCompactionMode, ProviderErrorKind, ProviderRequestId, ReasoningLevel, RequestedTarget,
    ResolvedTarget, StreamInterruption, StructuredOutputContract, TerminalEvidence, TerminalReport,
    ToolCallId, ToolCallProposal, ToolName, UnsentCause,
};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialAccessFailure, CredentialReference,
    CredentialValue,
};
use signalbox_model_runtime_anthropic::{
    AnthropicConfig, AnthropicConstructionError, AnthropicPreparedRequest, AnthropicRuntime,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const OVERSIZED_PROVIDER_RESPONSE_BYTES: usize = 8 * 1024 * 1024 + 1;

/// A loopback server that answers each accepted connection with the next
/// canned response and records every raw request it read.
struct CannedServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl CannedServer {
    async fn serving(responses: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback socket binds");
        let address = listener.local_addr().expect("bound socket has an address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_http_request(&mut socket).await;
                recorded.lock().expect("request log lock").push(request);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
        }
    }

    fn recorded_requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log lock").clone()
    }
}

/// Reads one HTTP/1.1 request (headers plus `content-length` body) as text.
async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let headers_end = loop {
        if let Some(position) = find_headers_end(&buffer) {
            break position;
        }
        let read = socket.read(&mut chunk).await.expect("request bytes read");
        assert!(
            read > 0,
            "connection closed before request headers completed"
        );
        buffer.extend_from_slice(&chunk[..read]);
    };
    let headers = String::from_utf8_lossy(&buffer[..headers_end]).to_string();
    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < headers_end + 4 + content_length {
        let read = socket.read(&mut chunk).await.expect("request body read");
        assert!(read > 0, "connection closed before request body completed");
        buffer.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&buffer).to_string()
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// A canned HTTP response with correct `content-length` framing.
fn http_response(status_line: &str, extra_headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {status_line}\r\n");
    for (name, value) in extra_headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = response.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// A fixed loopback credential source: every reference resolves to
/// `key_loop`.
#[derive(Debug)]
struct FixedKey;

impl CredentialAccess for FixedKey {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(b"key_loop".to_vec()))
    }
}

fn runtime_for(server_base_url: &str) -> AnthropicRuntime<FixedKey> {
    let mut config = AnthropicConfig::new(None);
    config.base_url = server_base_url.to_string();
    AnthropicRuntime::new(config, FixedKey).expect("loopback configuration constructs")
}

/// An operation whose correlation seed is the one knob; targets, one user
/// message, and a 64-token ceiling are canonical.
fn operation(correlation: &str) -> ModelOperation<String> {
    ModelOperation::new(
        correlation.to_string(),
        CredentialReference::new("anthropic-primary"),
        RequestedTarget::new("fast-alias"),
        ResolvedTarget::new("model-exact-1"),
        vec![ConversationMessage::user_text("hello")],
        ModelSettings::new(64),
    )
}

fn append_provider_compaction(operation: &mut ModelOperation<String>) {
    operation.messages.push(ConversationMessage {
        role: ConversationRole::Assistant,
        parts: vec![
            MessagePart::Text(String::from("preserved output")),
            MessagePart::ProviderCompaction {
                block_json: String::from(r#"{"type":"compaction","content":"preserved summary"}"#),
            },
        ],
    });
}

async fn execute<A: CredentialAccess>(
    runtime: &AnthropicRuntime<A>,
    operation: ModelOperation<String>,
    cancellation: CancellationSignal,
) -> (TerminalReport<String>, Vec<Observation<String>>) {
    let mut observations: Vec<Observation<String>> = Vec::new();
    let prepared = prepare(runtime, operation, CancellationSignal::never()).await;
    let report = runtime
        .execute(prepared, &mut observations, cancellation)
        .await;
    (report, observations)
}

async fn prepare<A: CredentialAccess>(
    runtime: &AnthropicRuntime<A>,
    operation: ModelOperation<String>,
    cancellation: CancellationSignal,
) -> AnthropicPreparedRequest<String> {
    match runtime.prepare(operation, cancellation).await {
        PreparationOutcome::Prepared(prepared) => prepared,
        PreparationOutcome::Cancelled { .. } => panic!("loopback preparation cancelled"),
        PreparationOutcome::Failed { failure, .. } => {
            panic!("loopback preparation failed: {failure:?}")
        }
        PreparationOutcome::Defect { defect, .. } => {
            panic!("loopback preparation was defective: {defect:?}")
        }
    }
}

/// The contract every structured-output loopback test below carries.
fn verdict_contract() -> StructuredOutputContract {
    StructuredOutputContract {
        name: ToolName::new("verdict"),
        description: "The verdict.".to_string(),
        schema: serde_json::value::to_raw_value(&serde_json::json!({"type": "object"}))
            .expect("fixture schema serializes"),
    }
}

/// The completion evidence a terminal report carries, or a failed assertion.
#[track_caller]
fn completion_evidence(report: TerminalReport<String>) -> CompletionEvidence {
    match report.evidence {
        TerminalEvidence::Completed(completion) => completion,
        other => panic!("a canned success response must classify as completed: {other:?}"),
    }
}

/// The JSON body of the one request the canned server recorded.
#[track_caller]
fn sent_request_body(server: &CannedServer) -> serde_json::Value {
    let requests = server.recorded_requests();
    let request = &requests[0];
    let json_start = request.find("\r\n\r\n").expect("request has a body") + 4;
    serde_json::from_str(&request[json_start..]).expect("request body is JSON")
}

/// A canned buffered success whose one content block is ordinary text.
fn text_response() -> Vec<u8> {
    http_response(
        "200 OK",
        &[("content-type", "application/json")],
        br#"{
        "id": "msg_plain_1",
        "type": "message",
        "role": "assistant",
        "model": "model-exact-1",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 4, "output_tokens": 2}
    }"#,
    )
}

/// The tool-call identity the canned contract response proposes under.
const CONTRACT_PROPOSAL_ID: &str = "toolu_c1";

/// The argument object the canned contract response proposes, spelled once so
/// a decode assertion reads the same bytes the response carried.
const CONTRACT_PROPOSAL_ARGUMENTS: &str = r#"{"ok": true}"#;

/// A canned buffered success whose one content block is the named tool use.
fn contract_proposal_response(name: &str) -> Vec<u8> {
    let body = format!(
        r#"{{
        "id": "msg_contract_1",
        "type": "message",
        "role": "assistant",
        "model": "model-exact-1",
        "content": [{{"type": "tool_use", "id": "{CONTRACT_PROPOSAL_ID}", "name": "{name}", "input": {CONTRACT_PROPOSAL_ARGUMENTS}}}],
        "stop_reason": "tool_use",
        "usage": {{"input_tokens": 4, "output_tokens": 2}}
    }}"#
    );
    http_response(
        "200 OK",
        &[("content-type", "application/json")],
        body.as_bytes(),
    )
}

#[tokio::test]
async fn a_structured_output_request_never_sends_a_forced_tool_choice() {
    let contract = verdict_contract();
    let server =
        CannedServer::serving(vec![contract_proposal_response(contract.name.as_str())]).await;
    let runtime = runtime_for(&server.base_url);
    let mut operation = operation("call-contract-shape");
    operation.output_contract = Some(contract.clone());

    let (_report, _observations) = execute(&runtime, operation, CancellationSignal::never()).await;

    let sent = sent_request_body(&server);
    assert_eq!(
        sent["tool_choice"],
        serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}),
        "the current Claude generation answers a forced tool choice with a 400"
    );
    assert_eq!(
        sent["tools"][0]["name"],
        serde_json::json!(contract.name.as_str())
    );
}

#[tokio::test]
async fn a_request_carries_no_sampling_field() {
    let server = CannedServer::serving(vec![text_response()]).await;
    let runtime = runtime_for(&server.base_url);

    let (_report, _observations) = execute(
        &runtime,
        operation("call-no-sampling"),
        CancellationSignal::never(),
    )
    .await;

    let sent = sent_request_body(&server);
    assert!(
        sent.get("temperature").is_none(),
        "an explicit null sampling control is rejected exactly as a value is"
    );
    assert!(
        sent.get("top_p").is_none(),
        "an explicit null sampling control is rejected exactly as a value is"
    );
}

#[tokio::test]
async fn a_request_carries_no_top_level_thinking_field() {
    let server = CannedServer::serving(vec![text_response()]).await;
    let runtime = runtime_for(&server.base_url);

    let (_report, _observations) = execute(
        &runtime,
        operation("call-no-thinking"),
        CancellationSignal::never(),
    )
    .await;

    assert!(
        sent_request_body(&server).get("thinking").is_none(),
        "omitting the parameter is the accepted form; an explicit null is not"
    );
}

#[tokio::test]
async fn a_contract_named_tool_use_still_decodes_as_the_contract_proposal() {
    let contract = verdict_contract();
    let server =
        CannedServer::serving(vec![contract_proposal_response(contract.name.as_str())]).await;
    let runtime = runtime_for(&server.base_url);
    let mut operation = operation("call-contract-decode");
    operation.output_contract = Some(contract.clone());

    let (report, _observations) = execute(&runtime, operation, CancellationSignal::never()).await;

    assert_eq!(
        completion_evidence(report).content,
        vec![AssistantPart::ToolCall(ToolCallProposal {
            id: ToolCallId::new(CONTRACT_PROPOSAL_ID),
            name: contract.name.clone(),
            arguments_json: CONTRACT_PROPOSAL_ARGUMENTS.to_string(),
        })],
        "the provider-independent structured decode reads the contract-named proposal"
    );
}

#[tokio::test]
async fn buffered_completion_end_to_end_sends_the_documented_request_shape() {
    let body = br#"{
        "id": "msg_loop_1",
        "type": "message",
        "role": "assistant",
        "model": "model-exact-1",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 4, "output_tokens": 2}
    }"#;
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[
            ("content-type", "application/json"),
            ("request-id", "req_loop_1"),
        ],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, observations) =
        execute(&runtime, operation("call-1"), CancellationSignal::never()).await;

    assert_eq!(report.correlation, "call-1".to_string());
    let TerminalEvidence::Completed(completion) = report.evidence else {
        panic!("a canned success response must classify as completed");
    };
    assert_eq!(completion.finish, CompletionFinish::EndTurn);
    assert_eq!(
        completion.content,
        vec![AssistantPart::Text("hi".to_string())]
    );
    assert_eq!(
        completion.exchange.provider_request_id,
        Some(ProviderRequestId::new("req_loop_1"))
    );
    assert_eq!(completion.exchange.http_status, Some(200));

    let requests = server.recorded_requests();
    assert_eq!(
        requests.len(),
        1,
        "one authorized operation is exactly one request"
    );
    let request = &requests[0];
    assert!(request.starts_with("POST /v1/messages HTTP/1.1\r\n"));
    assert!(request.contains("x-api-key: key_loop\r\n"));
    assert!(request.contains("anthropic-version: 2023-06-01\r\n"));
    assert!(!request.contains("anthropic-beta:"));
    assert!(request.contains("content-type: application/json\r\n"));
    let json_start = request.find("\r\n\r\n").expect("request has a body") + 4;
    let sent: serde_json::Value =
        serde_json::from_str(&request[json_start..]).expect("request body is JSON");
    assert_eq!(sent["model"], serde_json::json!("model-exact-1"));
    assert_eq!(sent["max_tokens"], serde_json::json!(64));
    assert!(sent.get("context_management").is_none());
    assert_eq!(sent["stream"], serde_json::json!(false));

    assert!(observations.iter().any(|observation| matches!(
        observation.fact,
        ObservationFact::ExchangeEstablished(ref exchange)
            if exchange.http_status == Some(200)
                && exchange.provider_request_id == Some(ProviderRequestId::new("req_loop_1"))
    )));
}

#[tokio::test]
async fn streamed_completion_end_to_end_emits_deltas_and_gates_on_message_stop() {
    let sse: &[u8] = b"event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"msg_loop_2\",\
        \"model\":\"model-exact-1\",\"content\":[],\"usage\":{\"input_tokens\":4}}}\n\n\
        event: content_block_start\n\
        data: {\"type\":\"content_block_start\",\"index\":0,\
        \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\",\"index\":0,\
        \"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
        event: content_block_stop\n\
        data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
        event: message_delta\n\
        data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
        \"usage\":{\"output_tokens\":2}}\n\n\
        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[
            ("content-type", "text/event-stream"),
            ("request-id", "req_loop_2"),
        ],
        sse,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);
    let mut streamed = operation("call-2");
    streamed.delivery = DeliveryMode::Streamed;

    let (report, observations) = execute(&runtime, streamed, CancellationSignal::never()).await;

    let TerminalEvidence::Completed(completion) = report.evidence else {
        panic!("a message_stop-gated canned stream must classify as completed");
    };
    assert_eq!(
        completion.content,
        vec![AssistantPart::Text("hi".to_string())]
    );
    assert_eq!(completion.usage.output_tokens, Some(2));
    assert!(observations.contains(&Observation {
        correlation: "call-2".to_string(),
        fact: ObservationFact::TextDelta {
            index: 0,
            text: "hi".to_string()
        },
    }));
    let sent = &server.recorded_requests()[0];
    let json_start = sent.find("\r\n\r\n").expect("request has a body") + 4;
    let body: serde_json::Value =
        serde_json::from_str(&sent[json_start..]).expect("request body is JSON");
    assert_eq!(body["stream"], serde_json::json!(true));
}

#[tokio::test]
async fn prepared_capability_retains_its_originating_runtime_settings() {
    let sse: &[u8] = b"event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\
        \"id\":\"msg_origin\",\"model\":\"model-exact-1\",\"content\":[],\
        \"usage\":{\"input_tokens\":1}}}\n\n\
        event: content_block_start\n\
        data: {\"type\":\"content_block_start\",\"index\":0,\
        \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\",\"index\":0,\
        \"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n\
        event: content_block_stop\n\
        data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
        event: message_delta\n\
        data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
        \"usage\":{\"output_tokens\":1}}\n\n\
        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let server = CannedServer::serving(vec![http_response("200 OK", &[], sse)]).await;
    let mut preparing_config = AnthropicConfig::new(None);
    preparing_config.base_url = server.base_url.clone();
    preparing_config.sse_record_limit = 1024;
    let preparing_runtime =
        AnthropicRuntime::new(preparing_config, FixedKey).expect("configuration constructs");
    let mut executing_config = AnthropicConfig::new(None);
    executing_config.base_url = server.base_url.clone();
    executing_config.sse_record_limit = 16;
    let executing_runtime =
        AnthropicRuntime::new(executing_config, FixedKey).expect("configuration constructs");
    let mut streamed = operation("call-origin-runtime");
    streamed.delivery = DeliveryMode::Streamed;
    let prepared = prepare(&preparing_runtime, streamed, CancellationSignal::never()).await;

    let mut observations = Vec::new();
    let report = executing_runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    assert!(matches!(report.evidence, TerminalEvidence::Completed(_)));
}

#[tokio::test]
async fn credential_rejection_is_typed_provider_error_evidence() {
    let body = br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "401 Unauthorized",
        &[
            ("content-type", "application/json"),
            ("request-id", "req_loop_3"),
        ],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(&runtime, operation("call-3"), CancellationSignal::never()).await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a definitive provider error response must classify as provider error");
    };
    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
    assert_eq!(
        error.native.error_token,
        Some("authentication_error".to_string())
    );
    assert_eq!(error.exchange.http_status, Some(401));
}

#[tokio::test]
async fn a_malformed_error_body_falls_back_to_http_status() {
    assert_anthropic_error_body_falls_back_to_status(b"{not json").await;
}

#[tokio::test]
async fn an_overdeep_error_body_falls_back_to_http_status() {
    let nested = format!(
        "{}null{}",
        "[".repeat(PROVIDER_JSON_NESTING_LIMIT + 1),
        "]".repeat(PROVIDER_JSON_NESTING_LIMIT + 1)
    );
    let overdeep = format!(
        r#"{{"type":"error","error":{{"type":"authentication_error",
            "message":"contradictory token","future":{nested}}}}}"#
    );

    assert_anthropic_error_body_falls_back_to_status(overdeep.as_bytes()).await;
}

async fn assert_anthropic_error_body_falls_back_to_status(body: &[u8]) {
    let status = 429;
    let server = CannedServer::serving(vec![http_response(
        &format!("{status} Too Many Requests"),
        &[("content-type", "application/json")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(
        &runtime,
        operation("call-invalid-error"),
        CancellationSignal::never(),
    )
    .await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a complete terminal error status remains definitive");
    };
    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert!(!error.non_acceptance_proven);
    assert_eq!(error.native.error_token, None);
    assert_eq!(error.exchange.http_status, Some(status));
}

#[tokio::test]
async fn a_redirect_is_never_followed_and_surfaces_as_evidence() {
    // The response's Location points back at the same server: a client that
    // followed redirects would replay the POST as a second request.
    let server = CannedServer::serving(vec![
        http_response(
            "307 Temporary Redirect",
            &[("location", "/v1/messages")],
            b"",
        ),
        http_response("200 OK", &[("content-type", "application/json")], b"{}"),
    ])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(&runtime, operation("call-4"), CancellationSignal::never()).await;

    let TerminalEvidence::BoundaryLoss(loss) = report.evidence else {
        panic!("a redirect must surface as boundary evidence, never be followed");
    };
    assert_eq!(loss.cause, LossCause::UnexpectedHttpStatus);
    assert_eq!(loss.exchange.http_status, Some(307));
    assert_eq!(
        server.recorded_requests().len(),
        1,
        "one authorized send must remain exactly one physical request"
    );
}

#[tokio::test]
async fn buffered_response_overflow_is_typed_body_loss() {
    let body = vec![b' '; OVERSIZED_PROVIDER_RESPONSE_BYTES];
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[("content-type", "application/json")],
        &body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(
        &runtime,
        operation("call-buffer-overflow"),
        CancellationSignal::never(),
    )
    .await;

    let TerminalEvidence::BoundaryLoss(loss) = report.evidence else {
        panic!("an oversized buffered response must fail closed as boundary loss");
    };
    assert!(matches!(loss.cause, LossCause::ResponseBodyLost(_)));
}

#[tokio::test]
async fn connection_refused_is_proven_unsent() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback socket binds");
    let address = listener.local_addr().expect("bound socket has an address");
    drop(listener);
    let runtime = runtime_for(&format!("http://{address}"));

    let (report, observations) =
        execute(&runtime, operation("call-5"), CancellationSignal::never()).await;

    let TerminalEvidence::ProvenUnsent(unsent) = report.evidence else {
        panic!("a refused connection provably precedes any request byte");
    };
    assert!(matches!(unsent.cause, UnsentCause::ConnectFailed(_)));
    assert!(
        !observations
            .iter()
            .any(|observation| matches!(observation.fact, ObservationFact::ExchangeEstablished(_)))
    );
}

#[tokio::test]
async fn response_body_cut_short_is_boundary_loss_not_error_guessing() {
    // Headers promise 4096 body bytes; the connection closes after 20.
    let mut response =
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\n\r\n"
            .to_vec();
    response.extend_from_slice(br#"{"id": "msg_trunc""#);
    let server = CannedServer::serving(vec![response]).await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(&runtime, operation("call-6"), CancellationSignal::never()).await;

    let TerminalEvidence::BoundaryLoss(loss) = report.evidence else {
        panic!("a truncated response body is not definitive evidence");
    };
    assert!(matches!(loss.cause, LossCause::ResponseBodyLost(_)));
    assert_eq!(loss.exchange.http_status, Some(200));
}

#[tokio::test]
async fn stream_cut_before_message_stop_is_explicit_incomplete_stream_evidence() {
    // Close-delimited body (no content-length): the transport ends cleanly,
    // but the protocol's terminal marker never arrived.
    let mut response =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".to_vec();
    response.extend_from_slice(
        b"event: message_start\n\
          data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"msg_cut\",\
          \"model\":\"model-exact-1\",\"content\":[],\"usage\":{\"input_tokens\":4}}}\n\n\
          event: content_block_start\n\
          data: {\"type\":\"content_block_start\",\"index\":0,\
          \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    let server = CannedServer::serving(vec![response]).await;
    let runtime = runtime_for(&server.base_url);
    let mut streamed = operation("call-7");
    streamed.delivery = DeliveryMode::Streamed;

    let (report, _) = execute(&runtime, streamed, CancellationSignal::never()).await;

    let TerminalEvidence::BoundaryLoss(loss) = report.evidence else {
        panic!("a stream without its terminal marker must never read as success");
    };
    assert_eq!(
        loss.cause,
        LossCause::StreamEndedWithoutTerminalMarker {
            interruption: StreamInterruption::EndOfStream
        }
    );
    assert_eq!(
        loss.reported_model,
        Some(signalbox_model_runtime::ProviderReportedModel::new(
            "model-exact-1"
        ))
    );
}

#[tokio::test]
async fn a_signal_cancelled_before_send_proves_the_request_unsent() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let runtime = runtime_for(&server.base_url);

    let (report, observations) = execute(
        &runtime,
        operation("call-8"),
        CancellationSignal::already_cancelled(),
    )
    .await;

    let TerminalEvidence::ProvenUnsent(unsent) = report.evidence else {
        panic!("cancellation before send is proven-unsent evidence");
    };
    assert_eq!(unsent.cause, UnsentCause::CancelledBeforeSend);
    assert_eq!(
        server.recorded_requests().len(),
        0,
        "a pre-send cancellation must reach the network never"
    );
    assert!(
        !observations
            .iter()
            .any(|observation| matches!(observation.fact, ObservationFact::SendCommenced))
    );
}

#[derive(Debug)]
struct CountingKey {
    resolutions: Arc<AtomicUsize>,
    value: &'static [u8],
}

impl CredentialAccess for CountingKey {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        Ok(CredentialValue::new(self.value.to_vec()))
    }
}

#[tokio::test]
async fn preparation_resolves_once_sends_nothing_and_execution_does_not_resolve_again() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let resolutions = Arc::new(AtomicUsize::new(0));
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let runtime = AnthropicRuntime::new(
        config,
        CountingKey {
            resolutions: Arc::clone(&resolutions),
            value: b"key_once",
        },
    )
    .expect("loopback configuration constructs");

    let prepared = prepare(
        &runtime,
        operation("call-prepare-once"),
        CancellationSignal::never(),
    )
    .await;

    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    assert!(server.recorded_requests().is_empty());

    let mut observations = Vec::new();
    runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    assert_eq!(server.recorded_requests().len(), 1);
}

#[tokio::test]
async fn input_count_rejects_unsupported_settings_before_credential_resolution() {
    let resolutions = Arc::new(AtomicUsize::new(0));
    let runtime = AnthropicRuntime::new(
        AnthropicConfig::new(None),
        CountingKey {
            resolutions: Arc::clone(&resolutions),
            value: b"key_count_rejected",
        },
    )
    .expect("configuration constructs");
    let mut unsupported = operation("count-unsupported");
    unsupported.settings.reasoning_level = Some(ReasoningLevel::Low);

    let outcome = runtime
        .count_input_tokens(unsupported, CancellationSignal::never())
        .await;

    assert_eq!(
        outcome,
        InputTokenCountOutcome::Failed {
            correlation: "count-unsupported".to_string(),
        }
    );
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn input_count_uses_the_declared_fast_target() {
    let server =
        CannedServer::serving(vec![http_response("200 OK", &[], br#"{"input_tokens":7}"#)]).await;
    let selected = ResolvedTarget::new("model-exact-1");
    let mapped = ResolvedTarget::new("model-fast-1");
    let definitions = [ModelCapabilityDefinition::new(
        selected,
        ModelCapabilities::new(
            BTreeSet::new(),
            Some(FastModeTarget::Mapped(mapped.clone())),
            BTreeSet::new(),
        ),
    )];
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    config.model_capabilities = ModelCapabilityCatalog::try_from_definitions(definitions)
        .expect("fixture capabilities are unique");
    let runtime = AnthropicRuntime::new(config, FixedKey).expect("configuration constructs");
    let mut counted = operation("count-fast");
    counted.settings.fast_mode = FastMode::Enabled;

    let outcome = runtime
        .count_input_tokens(counted, CancellationSignal::never())
        .await;
    let requests = server.recorded_requests();

    assert_eq!(
        outcome,
        InputTokenCountOutcome::Counted {
            correlation: "count-fast".to_string(),
            input_tokens: 7,
        }
    );
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains(&format!(r#""model":"{}""#, mapped.as_str())));
    assert!(!requests[0].contains(r#""speed":"fast""#));
    assert!(!requests[0].contains("anthropic-beta:"));
    assert!(
        sent_request_body(&server)
            .get("context_management")
            .is_none()
    );
}

#[tokio::test]
async fn preparation_omits_compaction_for_an_unsupported_effective_fast_target() {
    let server = CannedServer::serving(vec![text_response()]).await;
    let selected = ResolvedTarget::new("claude-opus-5");
    let mapped = ResolvedTarget::new("claude-haiku-4-5");
    let definitions = [ModelCapabilityDefinition::new(
        selected.clone(),
        ModelCapabilities::new(
            BTreeSet::new(),
            Some(FastModeTarget::Mapped(mapped.clone())),
            BTreeSet::new(),
        ),
    )];
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    config.model_capabilities = ModelCapabilityCatalog::try_from_definitions(definitions)
        .expect("fixture capabilities are unique");
    let runtime = AnthropicRuntime::new(config, FixedKey).expect("configuration constructs");
    let mut generated = operation("generate-fast-unsupported");
    generated.resolved_target = selected;
    generated.settings.fast_mode = FastMode::Enabled;
    append_provider_compaction(&mut generated);

    let _ = execute(&runtime, generated, CancellationSignal::never()).await;
    let body = sent_request_body(&server);

    assert_eq!(body["model"], mapped.as_str());
    assert!(body.get("context_management").is_none());
    assert!(!body.to_string().contains("preserved summary"));
    assert!(body.to_string().contains("preserved output"));
}

#[tokio::test]
async fn input_count_replays_compaction_for_a_supported_effective_fast_target() {
    let server =
        CannedServer::serving(vec![http_response("200 OK", &[], br#"{"input_tokens":7}"#)]).await;
    let selected = ResolvedTarget::new("claude-haiku-4-5");
    let mapped = ResolvedTarget::new("claude-opus-5");
    let definitions = [ModelCapabilityDefinition::new(
        selected.clone(),
        ModelCapabilities::new(
            BTreeSet::new(),
            Some(FastModeTarget::Mapped(mapped.clone())),
            BTreeSet::new(),
        ),
    )];
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    config.model_capabilities = ModelCapabilityCatalog::try_from_definitions(definitions)
        .expect("fixture capabilities are unique");
    config
        .provider_compaction_targets
        .insert(mapped.as_str().to_string());
    let runtime = AnthropicRuntime::new(config, FixedKey).expect("configuration constructs");
    let mut counted = operation("count-fast-supported");
    counted.resolved_target = selected;
    counted.settings.fast_mode = FastMode::Enabled;
    append_provider_compaction(&mut counted);

    let outcome = runtime
        .count_input_tokens(counted, CancellationSignal::never())
        .await;
    let body = sent_request_body(&server);

    assert_eq!(
        outcome,
        InputTokenCountOutcome::Counted {
            correlation: String::from("count-fast-supported"),
            input_tokens: 7,
        }
    );
    assert_eq!(body["model"], mapped.as_str());
    assert_eq!(
        body["context_management"],
        serde_json::json!({"edits": [{"type": "compact_20260112"}]})
    );
    assert!(body.to_string().contains("preserved summary"));
}

#[tokio::test]
async fn suppressed_generation_replay_still_sends_compaction_beta() {
    let server = CannedServer::serving(vec![text_response()]).await;
    let target = ResolvedTarget::new("claude-opus-5");
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    config
        .provider_compaction_targets
        .insert(target.as_str().to_string());
    let runtime = AnthropicRuntime::new(config, FixedKey).expect("configuration constructs");
    let mut generated = operation("generate-suppressed-replay");
    generated.resolved_target = target;
    generated.provider_compaction = ProviderCompactionMode::Suppressed;
    append_provider_compaction(&mut generated);

    let _ = execute(&runtime, generated, CancellationSignal::never()).await;
    let requests = server.recorded_requests();

    assert!(
        requests[0]
            .contains("anthropic-beta: context-management-2025-06-27,compact-2026-01-12\r\n")
    );
    assert!(
        sent_request_body(&server)
            .get("context_management")
            .is_none()
    );
}

#[tokio::test]
async fn suppressed_input_count_replay_still_sends_compaction_beta() {
    let server =
        CannedServer::serving(vec![http_response("200 OK", &[], br#"{"input_tokens":7}"#)]).await;
    let target = ResolvedTarget::new("claude-opus-5");
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    config
        .provider_compaction_targets
        .insert(target.as_str().to_string());
    let runtime = AnthropicRuntime::new(config, FixedKey).expect("configuration constructs");
    let mut counted = operation("count-suppressed-replay");
    counted.resolved_target = target;
    counted.provider_compaction = ProviderCompactionMode::Suppressed;
    append_provider_compaction(&mut counted);

    let outcome = runtime
        .count_input_tokens(counted, CancellationSignal::never())
        .await;
    let requests = server.recorded_requests();

    assert_eq!(
        outcome,
        InputTokenCountOutcome::Counted {
            correlation: String::from("count-suppressed-replay"),
            input_tokens: 7,
        }
    );
    assert!(
        requests[0]
            .contains("anthropic-beta: context-management-2025-06-27,compact-2026-01-12\r\n")
    );
    assert!(
        sent_request_body(&server)
            .get("context_management")
            .is_none()
    );
}

#[tokio::test]
async fn input_count_preserves_reasoning_and_same_target_fast_controls() {
    let correlation = "count-request-controls";
    let input_tokens = 7;
    let response_body = format!(r#"{{"input_tokens":{input_tokens}}}"#);
    let server =
        CannedServer::serving(vec![http_response("200 OK", &[], response_body.as_bytes())]).await;
    let mut counted = operation(correlation);
    counted.resolved_target = ResolvedTarget::new("claude-opus-5");
    counted.settings.reasoning_level = Some(ReasoningLevel::Low);
    counted.settings.fast_mode = FastMode::Enabled;
    let selected = counted.resolved_target.clone();
    let definitions = [ModelCapabilityDefinition::new(
        selected.clone(),
        ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Low]),
            Some(FastModeTarget::SameTarget),
            BTreeSet::new(),
        ),
    )];
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    config.model_capabilities = ModelCapabilityCatalog::try_from_definitions(definitions)
        .expect("fixture capabilities are unique");
    config
        .provider_compaction_targets
        .insert(selected.as_str().to_string());
    let runtime = AnthropicRuntime::new(config, FixedKey).expect("configuration constructs");

    let outcome = runtime
        .count_input_tokens(counted, CancellationSignal::never())
        .await;
    let requests = server.recorded_requests();

    assert_eq!(
        outcome,
        InputTokenCountOutcome::Counted {
            correlation: correlation.to_string(),
            input_tokens,
        }
    );
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains(&format!(r#""model":"{}""#, selected.as_str())));
    assert!(requests[0].contains(r#""output_config":{"effort":"low"}"#));
    assert!(requests[0].contains(r#""speed":"fast""#));
    assert!(requests[0].contains(
        "anthropic-beta: context-management-2025-06-27,compact-2026-01-12,fast-mode-2026-02-01\r\n"
    ));
    assert_eq!(
        sent_request_body(&server)["context_management"],
        serde_json::json!({"edits": [{"type": "compact_20260112"}]})
    );
}

#[derive(Debug)]
struct PendingKey;

impl CredentialAccess for PendingKey {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_during_preparation_creates_no_capability_or_http_traffic() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let runtime =
        AnthropicRuntime::new(config, PendingKey).expect("loopback configuration constructs");

    let outcome = runtime
        .prepare(
            operation("call-cancel-prepare"),
            CancellationSignal::already_cancelled(),
        )
        .await;

    let PreparationOutcome::Cancelled { correlation } = outcome else {
        panic!("entry cancellation remains a typed preparation outcome");
    };
    assert_eq!(correlation, "call-cancel-prepare");
    assert!(server.recorded_requests().is_empty());
}

#[tokio::test]
async fn a_ready_preparation_wins_a_same_poll_cancellation_race() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let runtime = runtime_for(&server.base_url);

    let outcome = runtime
        .prepare(
            operation("call-work-first"),
            CancellationSignal::already_cancelled(),
        )
        .await;

    assert!(matches!(outcome, PreparationOutcome::Prepared(_)));
    assert!(server.recorded_requests().is_empty());
}

#[derive(Debug)]
struct UnavailableKey;

impl CredentialAccess for UnavailableKey {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Err(CredentialAccessError::new(
            reference.clone(),
            CredentialAccessFailure::Unavailable,
        ))
    }
}

#[tokio::test]
async fn ordinary_validation_and_credential_failures_are_preparation_outcomes() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let mut invalid = operation("call-invalid");
    invalid.settings.temperature = Some(f64::NAN);
    let fixed = runtime_for(&server.base_url);

    assert!(matches!(
        fixed.prepare(invalid, CancellationSignal::never()).await,
        PreparationOutcome::Failed {
            failure: PreparationFailure::UnsupportedOperation { .. },
            ..
        }
    ));

    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let unavailable =
        AnthropicRuntime::new(config, UnavailableKey).expect("loopback configuration constructs");
    assert!(matches!(
        unavailable
            .prepare(operation("call-unavailable"), CancellationSignal::never())
            .await,
        PreparationOutcome::Failed {
            failure: PreparationFailure::CredentialUnavailable { .. },
            ..
        }
    ));

    assert!(server.recorded_requests().is_empty());
}

#[tokio::test]
async fn empty_credential_is_an_ordinary_preparation_failure() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let resolutions = Arc::new(AtomicUsize::new(0));
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let runtime = AnthropicRuntime::new(
        config,
        CountingKey {
            resolutions,
            value: b"",
        },
    )
    .expect("loopback configuration constructs");

    assert!(matches!(
        runtime
            .prepare(operation("call-unusable"), CancellationSignal::never())
            .await,
        PreparationOutcome::Failed {
            failure: PreparationFailure::CredentialUnusable { .. },
            ..
        }
    ));
    assert!(server.recorded_requests().is_empty());
}

#[tokio::test]
async fn credential_with_invalid_header_bytes_is_an_ordinary_preparation_failure() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let resolutions = Arc::new(AtomicUsize::new(0));
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let runtime = AnthropicRuntime::new(
        config,
        CountingKey {
            resolutions,
            value: b"invalid\nkey",
        },
    )
    .expect("loopback configuration constructs");

    assert!(matches!(
        runtime
            .prepare(operation("call-unusable"), CancellationSignal::never())
            .await,
        PreparationOutcome::Failed {
            failure: PreparationFailure::CredentialUnusable { .. },
            ..
        }
    ));
    assert!(server.recorded_requests().is_empty());
}

#[tokio::test]
async fn non_utf8_credential_is_an_ordinary_preparation_failure() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let resolutions = Arc::new(AtomicUsize::new(0));
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let runtime = AnthropicRuntime::new(
        config,
        CountingKey {
            resolutions,
            value: b"non-utf8-\xff",
        },
    )
    .expect("loopback configuration constructs");

    assert!(matches!(
        runtime
            .prepare(operation("call-unusable"), CancellationSignal::never())
            .await,
        PreparationOutcome::Failed {
            failure: PreparationFailure::CredentialUnusable { .. },
            ..
        }
    ));
    assert!(server.recorded_requests().is_empty());
}

#[test]
fn base_url_user_information_is_rejected_at_construction() {
    let mut config = AnthropicConfig::new(None);
    config.base_url = "https://user:password@example.com".to_string();

    assert!(matches!(
        AnthropicRuntime::new(config, FixedKey),
        Err(AnthropicConstructionError::InvalidBaseUrl { .. })
    ));
}

#[test]
fn plain_http_requires_a_literal_loopback_ip_host() {
    assert_anthropic_plain_http_rejected("http://example.com");
    assert_anthropic_plain_http_rejected("http://localhost:8080");
    assert_anthropic_plain_http_rejected("http://192.0.2.1");

    let mut ipv4_loopback = AnthropicConfig::new(None);
    ipv4_loopback.base_url = "http://127.0.0.1:1".to_string();
    assert!(AnthropicRuntime::new(ipv4_loopback, FixedKey).is_ok());

    let mut ipv6_loopback = AnthropicConfig::new(None);
    ipv6_loopback.base_url = "http://[::1]:1".to_string();
    assert!(AnthropicRuntime::new(ipv6_loopback, FixedKey).is_ok());
}

#[track_caller]
fn assert_anthropic_plain_http_rejected(base_url: &str) {
    let mut config = AnthropicConfig::new(None);
    config.base_url = base_url.to_string();

    assert!(
        matches!(
            AnthropicRuntime::new(config, FixedKey),
            Err(AnthropicConstructionError::InvalidBaseUrl { .. })
        ),
        "{base_url} must not be admitted without transport security"
    );
}

#[test]
fn exchange_timeout_is_unbounded_until_the_composition_root_sets_policy() {
    assert_eq!(AnthropicConfig::new(None).exchange_timeout, None);
}

#[test]
fn a_zero_exchange_timeout_is_rejected_at_construction() {
    let mut config = AnthropicConfig::new(None);
    config.exchange_timeout = Some(Duration::ZERO);

    assert!(matches!(
        AnthropicRuntime::new(config, FixedKey),
        Err(AnthropicConstructionError::InvalidExchangeTimeout)
    ));
}

#[test]
fn a_zero_sse_record_limit_is_rejected_at_construction() {
    let mut config = AnthropicConfig::new(None);
    config.sse_record_limit = 0;

    assert!(matches!(
        AnthropicRuntime::new(config, FixedKey),
        Err(AnthropicConstructionError::InvalidSseRecordLimit)
    ));
}

/// A key source whose value the test rotates between operations.
#[derive(Debug)]
struct RotatingKey(Arc<Mutex<String>>);

impl CredentialAccess for RotatingKey {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        assert_eq!(reference.as_str(), "anthropic-primary");
        Ok(CredentialValue::new(
            self.0.lock().expect("key lock").clone().into_bytes(),
        ))
    }
}

#[tokio::test]
async fn inv_035_api_key_rotation_is_visible_to_the_next_preparation() {
    // `docs/spec/configuration-and-credentials.md`: the credential is read
    // during send preparation of each physical request; a rotated value must
    // reach the next request without reconstructing the runtime.
    let server = CannedServer::serving(vec![
        http_response("200 OK", &[], b"{}"),
        http_response("200 OK", &[], b"{}"),
    ])
    .await;
    let value = Arc::new(Mutex::new("key_before".to_string()));
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let runtime = AnthropicRuntime::new(config, RotatingKey(Arc::clone(&value)))
        .expect("loopback configuration constructs");

    let before = prepare(&runtime, operation("call-9"), CancellationSignal::never()).await;
    *value.lock().expect("key lock") = "key_after".to_string();
    let after = prepare(&runtime, operation("call-10"), CancellationSignal::never()).await;
    let mut observations = Vec::new();
    runtime
        .execute(before, &mut observations, CancellationSignal::never())
        .await;
    runtime
        .execute(after, &mut observations, CancellationSignal::never())
        .await;

    let requests = server.recorded_requests();
    assert!(requests[0].contains("x-api-key: key_before\r\n"));
    assert!(requests[1].contains("x-api-key: key_after\r\n"));
}

#[tokio::test]
async fn execution_redacts_with_the_exact_credential_captured_by_preparation() {
    let body = br#"{
        "id":"msg_1","type":"message","role":"assistant",
        "model":"model-exact-1",
        "content":[{"type":"text","text":"echo key_before"}],
        "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}
    }"#;
    let server = CannedServer::serving(vec![http_response("200 OK", &[], body)]).await;
    let value = Arc::new(Mutex::new("key_before".to_string()));
    let mut config = AnthropicConfig::new(None);
    config.base_url = server.base_url.clone();
    let runtime = AnthropicRuntime::new(config, RotatingKey(Arc::clone(&value)))
        .expect("loopback configuration constructs");
    let prepared = prepare(
        &runtime,
        operation("call-captured-key"),
        CancellationSignal::never(),
    )
    .await;
    *value.lock().expect("key lock") = "key_after".to_string();

    let mut observations = Vec::new();
    let report = runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    let TerminalEvidence::Completed(completion) = report.evidence else {
        panic!("complete response remains completion evidence");
    };
    assert_eq!(
        completion.content,
        vec![AssistantPart::Text("echo [redacted]".to_string())]
    );
    assert!(server.recorded_requests()[0].contains("x-api-key: key_before\r\n"));
}

#[tokio::test]
async fn a_401_with_an_unrecognized_error_token_still_classifies_by_status() {
    let body = br#"{"type":"error","error":{"type":"brand_new_error","message":"no such key"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "401 Unauthorized",
        &[("content-type", "application/json")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(&runtime, operation("call-11"), CancellationSignal::never()).await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a definitive error response must classify as provider error");
    };
    assert_eq!(
        error.kind,
        ProviderErrorKind::CredentialRejected,
        "classification must not depend on incidental body shape"
    );
    assert_eq!(
        error.native.error_token,
        Some("brand_new_error".to_string())
    );
}

#[tokio::test]
async fn inv_035_provider_error_text_reflecting_the_key_is_redacted() {
    // Per `docs/spec/runtime-substrate.md`, evidence carries typed classes
    // and rendered detail, never credential values — even when an endpoint
    // reflects the key.
    let body = br#"{"type":"error","error":{"type":"authentication_error",
                    "message":"invalid x-api-key: key_loop"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "401 Unauthorized",
        &[("content-type", "application/json")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(&runtime, operation("call-12"), CancellationSignal::never()).await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a definitive error response must classify as provider error");
    };
    let message = error
        .native
        .message
        .expect("the rendered message is retained");
    assert!(
        !message.contains("key_loop"),
        "the key value must never leave the adapter"
    );
    assert!(message.contains("[redacted]"));
}

#[tokio::test]
async fn json_escaped_credential_in_fallback_error_body_is_redacted() {
    let body = br#"{"message":"key_\u006coop"}"#;
    let server = CannedServer::serving(vec![http_response(
        "500 Internal Server Error",
        &[("content-type", "application/json")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(
        &runtime,
        operation("call-encoded-error"),
        CancellationSignal::never(),
    )
    .await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a complete error status remains definitive provider evidence");
    };
    assert_eq!(
        error.native.message,
        Some(r#"{"message":"[redacted]"}"#.to_string())
    );
}

#[tokio::test]
async fn inv_035_success_content_reflecting_the_key_is_redacted() {
    let body = br#"{
        "id": "msg_key_loop",
        "type": "message",
        "role": "assistant",
        "model": "model-key_loop",
        "content": [{"type": "text", "text": "echo key_loop"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }"#;
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[("request-id", "req_key_loop")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, observations) =
        execute(&runtime, operation("call-13"), CancellationSignal::never()).await;

    let TerminalEvidence::Completed(completion) = report.evidence else {
        panic!("the complete response remains completion evidence after sanitization");
    };
    assert_eq!(
        completion.content,
        vec![AssistantPart::Text("echo [redacted]".to_string())]
    );
    assert!(
        !format!("{completion:?}").contains("key_loop"),
        "no successful terminal fact may expose the credential"
    );
    assert!(
        !format!("{observations:?}").contains("key_loop"),
        "no buffered observation may expose the credential"
    );
}

#[tokio::test]
async fn inv_035_streamed_delta_reflecting_the_key_is_redacted_before_observation() {
    let sse: &[u8] = b"event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
        \"role\":\"assistant\",\"id\":\"msg_1\",\"model\":\"model-exact-1\",\
        \"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
        event: content_block_start\n\
        data: {\"type\":\"content_block_start\",\"index\":0,\
        \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\",\"index\":0,\
        \"delta\":{\"type\":\"text_delta\",\"text\":\"key_\"}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\",\"index\":0,\
        \"delta\":{\"type\":\"text_delta\",\"text\":\"loop\"}}\n\n\
        event: content_block_stop\n\
        data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
        event: message_delta\n\
        data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
        \"usage\":{\"output_tokens\":2}}\n\n\
        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let server = CannedServer::serving(vec![http_response("200 OK", &[], sse)]).await;
    let runtime = runtime_for(&server.base_url);
    let mut streamed = operation("call-14");
    streamed.delivery = DeliveryMode::Streamed;

    let (report, observations) = execute(&runtime, streamed, CancellationSignal::never()).await;

    assert!(matches!(report.evidence, TerminalEvidence::Completed(_)));
    assert!(observations.iter().any(|observation| matches!(
        observation.fact,
        ObservationFact::TextDelta { ref text, .. } if text == "[redacted]"
    )));
    assert!(
        !format!("{observations:?}").contains("key_loop"),
        "streamed content must be sanitized before reaching the sink"
    );
}

#[tokio::test]
async fn truncated_sse_record_is_stream_integrity_loss() {
    let sse: &[u8] = b"event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
        \"role\":\"assistant\",\"id\":\"msg_1\",\"model\":\"model-exact-1\",\
        \"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\"";
    let server = CannedServer::serving(vec![http_response("200 OK", &[], sse)]).await;
    let runtime = runtime_for(&server.base_url);
    let mut streamed = operation("call-truncated-record");
    streamed.delivery = DeliveryMode::Streamed;

    let (report, _) = execute(&runtime, streamed, CancellationSignal::never()).await;

    let TerminalEvidence::BoundaryLoss(loss) = report.evidence else {
        panic!("a truncated SSE record must be boundary-loss evidence");
    };
    assert!(matches!(
        loss.cause,
        LossCause::StreamProtocolViolation { .. }
    ));
}

#[tokio::test]
async fn wrong_error_envelope_discriminator_falls_back_to_http_status() {
    let body = br#"{"type":"message","error":{"type":"authentication_error",
                    "message":"incidental nested shape"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "400 Bad Request",
        &[("content-type", "application/json")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(
        &runtime,
        operation("call-wrong-error-discriminator"),
        CancellationSignal::never(),
    )
    .await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a documented error status remains definitive provider error evidence");
    };
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

#[test]
fn a_base_url_with_query_or_fragment_fails_construction() {
    let mut config = AnthropicConfig::new(None);
    config.base_url = "http://127.0.0.1:1/api?tenant=x".to_string();

    let error = AnthropicRuntime::new(config, FixedKey)
        .expect_err("a query component would corrupt the endpoint path");

    assert!(matches!(
        error,
        AnthropicConstructionError::InvalidBaseUrl { .. }
    ));
}

#[test]
fn an_authority_less_base_url_fails_construction() {
    let mut config = AnthropicConfig::new(None);
    config.base_url = "https://".to_string();

    let error = AnthropicRuntime::new(config, FixedKey)
        .expect_err("an absent authority must not be repaired from the endpoint path");

    assert!(matches!(
        error,
        AnthropicConstructionError::InvalidBaseUrl { .. }
    ));
}

#[tokio::test]
async fn a_base_url_path_is_preserved_when_the_endpoint_is_appended() {
    let server = CannedServer::serving(vec![http_response(
        "400 Bad Request",
        &[("content-type", "application/json")],
        br#"{"type":"error","error":{"type":"invalid_request_error"}}"#,
    )])
    .await;
    let mut config = AnthropicConfig::new(None);
    config.base_url = format!("{}/proxy", server.base_url);
    let runtime =
        AnthropicRuntime::new(config, FixedKey).expect("path-bearing base URL constructs");

    let _ = execute(
        &runtime,
        operation("call-base-path"),
        CancellationSignal::never(),
    )
    .await;

    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /proxy/v1/messages HTTP/1.1\r\n"));
}

#[test]
fn a_non_http_base_url_scheme_fails_construction() {
    let mut config = AnthropicConfig::new(None);
    config.base_url = "file:///tmp".to_string();

    let error = AnthropicRuntime::new(config, FixedKey)
        .expect_err("a non-HTTP scheme can never reach the provider");

    assert!(matches!(
        error,
        AnthropicConstructionError::InvalidBaseUrl { .. }
    ));
}
