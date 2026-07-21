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

use std::sync::{Arc, Mutex};

use signalbox_model_runtime::{
    AssistantPart, CancellationSignal, CompletionFinish, ConversationMessage, DeliveryMode,
    LossCause, ModelOperation, ModelRuntime, ModelSettings, Observation, ObservationFact,
    ProviderErrorKind, ProviderRequestId, RequestedTarget, ResolvedTarget, StreamInterruption,
    TerminalEvidence, TerminalReport, UnsentCause,
};
use signalbox_model_runtime_openai::{ApiKey, OpenAiConfig, OpenAiRuntime};
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

fn runtime_for(server_base_url: &str) -> OpenAiRuntime {
    let mut config = OpenAiConfig::new(ApiKey::new("key_loop"));
    config.base_url = server_base_url.to_string();
    OpenAiRuntime::new(config).expect("loopback configuration constructs")
}

/// An operation whose correlation seed is the one knob; targets, one user
/// message, and a 64-token ceiling are canonical.
fn operation(correlation: &str) -> ModelOperation<String> {
    ModelOperation::new(
        correlation.to_string(),
        RequestedTarget::new("fast-alias"),
        ResolvedTarget::new("model-exact-1"),
        vec![ConversationMessage::user_text("hello")],
        ModelSettings::new(64),
    )
}

async fn execute(
    runtime: &OpenAiRuntime,
    operation: ModelOperation<String>,
    cancellation: CancellationSignal,
) -> (TerminalReport<String>, Vec<Observation<String>>) {
    let mut observations: Vec<Observation<String>> = Vec::new();
    let report = runtime
        .execute(operation, &mut observations, cancellation)
        .await;
    (report, observations)
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
    let sse: &[u8] = b"data: {\"id\":\"chatcmpl_loop_2\",\"model\":\"model-exact-1\",\
        \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
        data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
        data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n\
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
async fn a_redirect_is_never_followed_and_surfaces_as_evidence() {
    // The response's Location points back at the same server: a client that
    // followed redirects would replay the POST as a second request.
    let server = CannedServer::serving(vec![
        http_response(
            "307 Temporary Redirect",
            &[("location", "/v1/chat/completions")],
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
        b"data: {\"id\":\"chatcmpl_cut\",\"model\":\"model-exact-1\",\
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
        observations
            .iter()
            .any(|observation| matches!(observation.fact, ObservationFact::RequestPrepared))
    );
    assert!(
        !observations
            .iter()
            .any(|observation| matches!(observation.fact, ObservationFact::SendCommenced))
    );
}
