#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

//! End-to-end adapter tests against a canned loopback HTTP server.
//!
//! No live provider is contacted and no credential exists: the server is a
//! local socket replaying canned bytes, which lets these tests assert the
//! real transport path — headers sent, redirect discipline, connect-failure
//! classification, and stream-integrity evidence — deterministically.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, DeliveryMode,
    LossCause, ModelOperation, ModelRuntime, ModelSettings, Observation, ObservationFact,
    PreparationFailure, PreparationOutcome, ProviderErrorKind, ProviderRequestId, RequestedTarget,
    ResolvedTarget, StreamInterruption, TerminalEvidence, TerminalReport, UnsentCause,
};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialAccessFailure, CredentialReference,
    CredentialValue,
};
use signalbox_model_runtime_openai::{
    OpenAiConfig, OpenAiConstructionError, OpenAiPreparedRequest, OpenAiRuntime,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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

#[derive(Debug)]
struct EmptyKey;

impl CredentialAccess for EmptyKey {
    async fn resolve(
        &self,
        _reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        Ok(CredentialValue::new(Vec::new()))
    }
}

fn runtime_for(server_base_url: &str) -> OpenAiRuntime<FixedKey> {
    let mut config = OpenAiConfig::new();
    config.base_url = server_base_url.to_string();
    OpenAiRuntime::new(config, FixedKey).expect("loopback configuration constructs")
}

/// An operation whose correlation seed is the one knob; targets, one user
/// message, and a 64-token ceiling are canonical.
fn operation(correlation: &str) -> ModelOperation<String> {
    ModelOperation::new(
        correlation.to_string(),
        CredentialReference::new("openai-primary"),
        RequestedTarget::new("fast-alias"),
        ResolvedTarget::new("model-exact-1"),
        vec![ConversationMessage::user_text("hello")],
        ModelSettings::new(64),
    )
}

async fn execute<A: CredentialAccess>(
    runtime: &OpenAiRuntime<A>,
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
    runtime: &OpenAiRuntime<A>,
    operation: ModelOperation<String>,
    cancellation: CancellationSignal,
) -> OpenAiPreparedRequest<String> {
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

#[tokio::test]
async fn buffered_completion_end_to_end_sends_the_documented_request_shape() {
    let body = br#"{
        "id": "chatcmpl_loop_1",
        "object": "chat.completion",
        "model": "model-exact-1",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 4, "completion_tokens": 2}
    }"#;
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[
            ("content-type", "application/json"),
            ("x-request-id", "req_loop_1"),
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
    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer key_loop\r\n"));
    assert!(request.contains("content-type: application/json\r\n"));
    let json_start = request.find("\r\n\r\n").expect("request has a body") + 4;
    let sent: serde_json::Value =
        serde_json::from_str(&request[json_start..]).expect("request body is JSON");
    assert_eq!(sent["model"], serde_json::json!("model-exact-1"));
    assert_eq!(sent["max_completion_tokens"], serde_json::json!(64));
    assert_eq!(sent["stream"], serde_json::json!(false));

    assert!(observations.iter().any(|observation| matches!(
        observation.fact,
        ObservationFact::ExchangeEstablished(ref exchange)
            if exchange.http_status == Some(200)
                && exchange.provider_request_id == Some(ProviderRequestId::new("req_loop_1"))
    )));
}

#[tokio::test]
async fn streamed_completion_end_to_end_emits_deltas_and_gates_on_done() {
    let sse: &[u8] = b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_loop_2\",\"model\":\"model-exact-1\",\
        \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
        data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_loop_2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
        data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_loop_2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
        data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_loop_2\",\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n\
        data: [DONE]\n\n";
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[
            ("content-type", "text/event-stream"),
            ("x-request-id", "req_loop_2"),
        ],
        sse,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);
    let mut streamed = operation("call-2");
    streamed.delivery = DeliveryMode::Streamed;

    let (report, observations) = execute(&runtime, streamed, CancellationSignal::never()).await;

    let TerminalEvidence::Completed(completion) = report.evidence else {
        panic!("a [DONE]-gated canned stream must classify as completed");
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
    assert_eq!(
        body["stream_options"],
        serde_json::json!({"include_usage": true})
    );
}

#[tokio::test]
async fn prepared_capability_keeps_its_originating_stream_settings() {
    let sse: &[u8] = b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_origin\",\"model\":\"model-exact-1\",\
        \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n\
        data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_origin\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
        data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_origin\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
        data: [DONE]\n\n";
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        sse,
    )])
    .await;

    let mut preparing_config = OpenAiConfig::new();
    preparing_config.base_url = server.base_url.clone();
    preparing_config.sse_record_limit = 1024;
    let preparing_runtime =
        OpenAiRuntime::new(preparing_config, FixedKey).expect("preparing configuration constructs");
    let mut executing_config = OpenAiConfig::new();
    executing_config.base_url = "http://127.0.0.1:1".to_string();
    executing_config.sse_record_limit = 1;
    let executing_runtime =
        OpenAiRuntime::new(executing_config, FixedKey).expect("executing configuration constructs");
    let mut streamed = operation("call-origin-settings");
    streamed.delivery = DeliveryMode::Streamed;
    let prepared = prepare(&preparing_runtime, streamed, CancellationSignal::never()).await;

    let mut observations = Vec::new();
    let report = executing_runtime
        .execute(prepared, &mut observations, CancellationSignal::never())
        .await;

    let TerminalEvidence::Completed(completion) = report.evidence else {
        panic!("execution retains the originating runtime's client and stream limit");
    };
    assert_eq!(
        completion.content,
        vec![AssistantPart::Text("hi".to_string())]
    );
    assert_eq!(server.recorded_requests().len(), 1);
}

#[tokio::test]
async fn declared_stop_sequence_keeps_the_stop_finish_ambiguous() {
    let body = br#"{"id":"chatcmpl-stop","object":"chat.completion","model":"model-exact-1",
        "choices":[{"index":0,"message":{"role":"assistant","content":"partial"},
        "finish_reason":"stop"}]}"#;
    let server = CannedServer::serving(vec![http_response("200 OK", &[], body)]).await;
    let runtime = runtime_for(&server.base_url);
    let mut stopped = operation("call-stop-ambiguity");
    stopped.settings.stop_sequences = vec!["END".to_string()];

    let (report, _) = execute(&runtime, stopped, CancellationSignal::never()).await;

    let TerminalEvidence::BoundaryLoss(loss) = report.evidence else {
        panic!("the provider does not distinguish natural and caller-declared stops");
    };
    assert!(matches!(
        loss.cause,
        LossCause::ResponseUnintelligible { .. }
    ));
}

#[tokio::test]
async fn credential_rejection_is_typed_provider_error_evidence() {
    let body = br#"{"error":{"message":"Incorrect API key provided.",
                    "type":"invalid_request_error","code":"invalid_api_key"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "401 Unauthorized",
        &[
            ("content-type", "application/json"),
            ("x-request-id", "req_loop_3"),
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
    assert_eq!(error.native.error_code, Some("invalid_api_key".to_string()));
    assert_eq!(error.exchange.http_status, Some(401));
}

#[tokio::test]
async fn buffered_error_type_classifies_when_code_is_absent() {
    let body = br#"{"error":{"message":"quota exhausted","type":"insufficient_quota"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "429 Too Many Requests",
        &[("content-type", "application/json")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(
        &runtime,
        operation("call-error-type"),
        CancellationSignal::never(),
    )
    .await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a complete error envelope is definitive provider evidence");
    };
    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert_eq!(
        error.native.error_token.as_deref(),
        Some("insufficient_quota")
    );
    assert_eq!(error.native.error_code, None);
}

#[tokio::test]
async fn an_empty_credential_is_rejected_before_send() {
    let mut config = OpenAiConfig::new();
    config.base_url = "http://127.0.0.1:1".to_string();
    let runtime = OpenAiRuntime::new(config, EmptyKey).expect("configuration constructs");

    let outcome = runtime
        .prepare(operation("call-empty-key"), CancellationSignal::never())
        .await;

    assert!(matches!(
        outcome,
        PreparationOutcome::Failed {
            failure: PreparationFailure::CredentialUnusable { .. },
            ..
        }
    ));
}

#[tokio::test]
async fn a_redirect_is_never_followed_and_surfaces_as_evidence() {
    // The response's Location points back at the same server: a client that
    // followed redirects would replay the POST as a second request.
    let server = CannedServer::serving(vec![
        http_response(
            "307 Temporary Redirect",
            &[
                ("location", "/v1/chat/completions"),
                ("x-request-id", "redirect-key_loop"),
            ],
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
    assert!(!format!("{loss:?}").contains("key_loop"));
    assert_eq!(
        server.recorded_requests().len(),
        1,
        "one authorized send must remain exactly one physical request"
    );
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
async fn stream_cut_before_done_is_explicit_incomplete_stream_evidence() {
    // Close-delimited body (no content-length): the transport ends cleanly,
    // but the protocol's terminal marker never arrived.
    let mut response =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".to_vec();
    response.extend_from_slice(
        b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_cut\",\"model\":\"model-exact-1\",\
          \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"par\"}}]}\n\n",
    );
    let server = CannedServer::serving(vec![response]).await;
    let runtime = runtime_for(&server.base_url);
    let mut streamed = operation("call-6");
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
        operation("call-7"),
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
    let mut config = OpenAiConfig::new();
    config.base_url = server.base_url.clone();
    let runtime = OpenAiRuntime::new(
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
    let mut config = OpenAiConfig::new();
    config.base_url = server.base_url.clone();
    let runtime = OpenAiRuntime::new(config, PendingKey).expect("configuration constructs");

    let outcome = runtime
        .prepare(
            operation("call-cancel-prepare"),
            CancellationSignal::already_cancelled(),
        )
        .await;

    assert!(matches!(
        outcome,
        PreparationOutcome::Cancelled { correlation }
            if correlation == "call-cancel-prepare"
    ));
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

    let mut config = OpenAiConfig::new();
    config.base_url = server.base_url.clone();
    let unavailable = OpenAiRuntime::new(config, UnavailableKey).expect("configuration constructs");
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
async fn credential_with_invalid_header_bytes_is_an_ordinary_preparation_failure() {
    let server = CannedServer::serving(vec![http_response("200 OK", &[], b"{}")]).await;
    let resolutions = Arc::new(AtomicUsize::new(0));
    let mut config = OpenAiConfig::new();
    config.base_url = server.base_url.clone();
    let runtime = OpenAiRuntime::new(
        config,
        CountingKey {
            resolutions,
            value: b"invalid\nkey",
        },
    )
    .expect("configuration constructs");

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
    let mut config = OpenAiConfig::new();
    config.base_url = server.base_url.clone();
    let runtime = OpenAiRuntime::new(
        config,
        CountingKey {
            resolutions,
            value: b"non-utf8-\xff",
        },
    )
    .expect("configuration constructs");

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
    let mut config = OpenAiConfig::new();
    config.base_url = "https://user:password@example.com".to_string();

    assert!(matches!(
        OpenAiRuntime::new(config, FixedKey),
        Err(OpenAiConstructionError::InvalidBaseUrl { .. })
    ));
}

#[test]
fn plain_http_requires_a_literal_loopback_ip_host() {
    for base_url in [
        "http://example.com",
        "http://localhost:8080",
        "http://192.0.2.1",
    ] {
        let mut config = OpenAiConfig::new();
        config.base_url = base_url.to_string();

        assert!(
            matches!(
                OpenAiRuntime::new(config, FixedKey),
                Err(OpenAiConstructionError::InvalidBaseUrl { .. })
            ),
            "{base_url} must not be admitted without transport security"
        );
    }
}

#[test]
fn the_default_exchange_timeout_is_ten_minutes() {
    assert_eq!(
        OpenAiConfig::new().exchange_timeout,
        Duration::from_secs(10 * 60)
    );
}

#[test]
fn a_zero_exchange_timeout_is_rejected_at_construction() {
    let mut config = OpenAiConfig::new();
    config.exchange_timeout = Duration::ZERO;

    assert!(matches!(
        OpenAiRuntime::new(config, FixedKey),
        Err(OpenAiConstructionError::InvalidExchangeTimeout)
    ));
}

#[test]
fn zero_sse_record_limit_is_rejected_at_construction() {
    let mut config = OpenAiConfig::new();
    config.sse_record_limit = 0;

    assert!(matches!(
        OpenAiRuntime::new(config, FixedKey),
        Err(OpenAiConstructionError::InvalidSseRecordLimit)
    ));
}

#[derive(Debug)]
struct RotatingKey(Arc<Mutex<String>>);

impl CredentialAccess for RotatingKey {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        assert_eq!(reference.as_str(), "openai-primary");
        Ok(CredentialValue::new(
            self.0.lock().expect("key lock").clone().into_bytes(),
        ))
    }
}

#[tokio::test]
async fn inv_035_api_key_rotation_is_visible_to_the_next_preparation() {
    let server = CannedServer::serving(vec![
        http_response("200 OK", &[], b"{}"),
        http_response("200 OK", &[], b"{}"),
    ])
    .await;
    let value = Arc::new(Mutex::new("key_before".to_string()));
    let mut config = OpenAiConfig::new();
    config.base_url = server.base_url.clone();
    let runtime = OpenAiRuntime::new(config, RotatingKey(Arc::clone(&value)))
        .expect("configuration constructs");

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
    assert!(requests[0].contains("authorization: Bearer key_before\r\n"));
    assert!(requests[1].contains("authorization: Bearer key_after\r\n"));
}

#[tokio::test]
async fn execution_redacts_with_the_exact_credential_captured_by_preparation() {
    let body = br#"{"id":"chatcmpl-captured","object":"chat.completion","model":"model-exact-1","choices":[{
        "index":0,"message":{"role":"assistant","content":"echo key_before"},
        "finish_reason":"stop"}]}"#;
    let server = CannedServer::serving(vec![http_response("200 OK", &[], body)]).await;
    let value = Arc::new(Mutex::new("key_before".to_string()));
    let mut config = OpenAiConfig::new();
    config.base_url = server.base_url.clone();
    let runtime = OpenAiRuntime::new(config, RotatingKey(Arc::clone(&value)))
        .expect("configuration constructs");
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
    assert!(server.recorded_requests()[0].contains("authorization: Bearer key_before\r\n"));
}

#[tokio::test]
async fn provider_error_text_reflecting_the_key_is_redacted() {
    // Per `docs/spec/runtime-substrate.md`, evidence carries typed classes
    // and rendered detail, never credential values — even when an endpoint
    // reflects the key.
    let body = br#"{"error":{"message":"invalid bearer key_loop","type":"invalid_request_error",
                    "code":"invalid_api_key"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "401 Unauthorized",
        &[
            ("content-type", "application/json"),
            ("x-request-id", "request-key_loop"),
        ],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(&runtime, operation("call-8"), CancellationSignal::never()).await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a definitive error response must classify as provider error");
    };
    assert!(!format!("{error:?}").contains("key_loop"));
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
async fn credential_rejection_status_precedes_a_contradictory_error_type() {
    let body = br#"{"error":{"message":"quota","type":"insufficient_quota"}}"#;
    let server = CannedServer::serving(vec![http_response(
        "401 Unauthorized",
        &[("content-type", "application/json")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, _) = execute(
        &runtime,
        operation("call-contradictory-401"),
        CancellationSignal::never(),
    )
    .await;

    let TerminalEvidence::ProviderError(error) = report.evidence else {
        panic!("a complete error status remains definitive provider evidence");
    };
    assert_eq!(error.kind, ProviderErrorKind::CredentialRejected);
}

#[tokio::test]
async fn successful_content_reflecting_the_key_is_redacted() {
    let body = br#"{"id":"chatcmpl_key_loop","object":"chat.completion",
        "model":"model-key_loop","choices":[{
        "index":0,"message":{"role":"assistant","content":"reflected key_loop"},
        "finish_reason":"stop"}]}"#;
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[
            ("content-type", "application/json"),
            ("x-request-id", "req-key_loop"),
        ],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);

    let (report, observations) = execute(
        &runtime,
        operation("call-redacted-success"),
        CancellationSignal::never(),
    )
    .await;

    assert!(!format!("{report:?}").contains("key_loop"));
    assert!(!format!("{observations:?}").contains("key_loop"));
    assert!(format!("{report:?}").contains("[redacted]"));
}

#[tokio::test]
async fn streamed_observations_reflecting_the_key_are_redacted() {
    let body: &[u8] = b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl-key_loop\",\"model\":\"model-key_loop\",\
        \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"key_loop\"}}]}\n\n\
        data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl-key_loop\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
        data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl-key_loop\",\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n\
        data: [DONE]\n\n";
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);
    let mut operation = operation("call-redacted-stream");
    operation.delivery = DeliveryMode::Streamed;

    let (report, observations) = execute(&runtime, operation, CancellationSignal::never()).await;

    assert!(!format!("{report:?}").contains("key_loop"));
    assert!(!format!("{observations:?}").contains("key_loop"));
}

#[tokio::test]
async fn json_escaped_streamed_tool_arguments_are_redacted_before_observation() {
    let body: &[u8] = br#"data: {"object":"chat.completion.chunk","id":"chatcmpl-tool","model":"model-exact-1","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"token\":\"key_\\u00"}}]}}]}

data: {"object":"chat.completion.chunk","id":"chatcmpl-tool","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"6coop\"}"}}]}}]}

data: {"object":"chat.completion.chunk","id":"chatcmpl-tool","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"object":"chat.completion.chunk","id":"chatcmpl-tool","choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2}}

data: [DONE]

"#;
    let server = CannedServer::serving(vec![http_response(
        "200 OK",
        &[("content-type", "text/event-stream")],
        body,
    )])
    .await;
    let runtime = runtime_for(&server.base_url);
    let mut operation = operation("call-redacted-tool-stream");
    operation.delivery = DeliveryMode::Streamed;

    let (report, observations) = execute(&runtime, operation, CancellationSignal::never()).await;

    assert!(matches!(report.evidence, TerminalEvidence::Completed(_)));
    assert!(!format!("{report:?}").contains("key_loop"));
    assert!(!format!("{observations:?}").contains("key_loop"));
    assert!(format!("{observations:?}").contains("[redacted]"));
}

#[test]
fn a_base_url_with_query_or_fragment_fails_construction() {
    let mut config = OpenAiConfig::new();
    config.base_url = "http://127.0.0.1:1/api?tenant=x".to_string();

    let error = OpenAiRuntime::new(config, FixedKey)
        .expect_err("a query component would corrupt the endpoint path");

    assert!(matches!(
        error,
        signalbox_model_runtime_openai::OpenAiConstructionError::InvalidBaseUrl { .. }
    ));
}

#[test]
fn an_authority_less_base_url_fails_construction() {
    let mut config = OpenAiConfig::new();
    config.base_url = "https://".to_string();

    let error = OpenAiRuntime::new(config, FixedKey)
        .expect_err("an absent authority must not be repaired from the endpoint path");

    assert!(matches!(
        error,
        signalbox_model_runtime_openai::OpenAiConstructionError::InvalidBaseUrl { .. }
    ));
}

#[tokio::test]
async fn a_base_url_path_is_preserved_when_the_endpoint_is_appended() {
    let server = CannedServer::serving(vec![http_response(
        "400 Bad Request",
        &[("content-type", "application/json")],
        br#"{"error":{"type":"invalid_request_error"}}"#,
    )])
    .await;
    let mut config = OpenAiConfig::new();
    config.base_url = format!("{}/proxy", server.base_url);
    let runtime = OpenAiRuntime::new(config, FixedKey).expect("path-bearing base URL constructs");

    let _ = execute(
        &runtime,
        operation("call-base-path"),
        CancellationSignal::never(),
    )
    .await;

    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /proxy/v1/chat/completions HTTP/1.1\r\n"));
}

#[test]
fn a_non_http_base_url_scheme_fails_construction() {
    let mut config = OpenAiConfig::new();
    config.base_url = "file:///tmp".to_string();

    let error = OpenAiRuntime::new(config, FixedKey)
        .expect_err("a non-HTTP scheme can never reach the provider");

    assert!(matches!(
        error,
        signalbox_model_runtime_openai::OpenAiConstructionError::InvalidBaseUrl { .. }
    ));
}
