//! The adapter runtime: one operation, at most one HTTP interaction.
//!
//! Deliberately mirrors the Anthropic adapter's transport glue rather than
//! sharing a crate with it: the discipline is small, and each adapter's
//! evidence path stays independently reviewable. Extracting a shared
//! transport crate is a refactor candidate once a third adapter exists.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Waker};

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CancellationSignal, CompletionFinish, DeliveryMode,
    ExchangeFacts, FinishReason, LossCause, ModelOperation, ModelRuntime, NativeErrorFacts,
    Observation, ObservationFact, ObservationSink, PreparationDefect, PreparationFailure,
    PreparationOutcome, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind,
    ProviderMessageId, ProviderReportedModel, ProviderRequestId, SseFraming, StreamInterruption,
    TerminalEvidence, TerminalReport, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
    TransportFacts, UnsentCause,
};

use signalbox_model_runtime::{CredentialAccess, CredentialValue};

use crate::config::OpenAiConfig;
use crate::response::decode_buffered_response;
use crate::status::{classify_error, classify_error_envelope};
use crate::stream::{StreamDecoder, StreamStep};
use crate::translate::build_request;
use crate::wire::ErrorEnvelope;

const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_STREAMED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The OpenAI Chat Completions adapter.
///
/// Implements [`ModelRuntime`]: executes exactly one authorized operation as
/// at most one `POST /v1/chat/completions` request and reports typed
/// evidence. It holds no state between operations, retries nothing, and
/// never issues a second request for one operation.
pub struct OpenAiRuntime<A> {
    client: Client,
    completions_url: Url,
    credentials: A,
    sse_record_limit: usize,
}

/// An opaque, one-shot OpenAI request capability prepared per
/// `docs/spec/runtime-substrate.md`.
///
/// The private fields bind the complete authenticated request, its originating
/// HTTP client and execution settings, caller correlation, and exact credential
/// value needed to sanitize provider-controlled evidence. The type deliberately
/// implements neither `Clone`, serialization, nor diagnostic formatting.
#[must_use]
pub struct OpenAiPreparedRequest<C> {
    transport: PreparedTransport,
    correlation: C,
    credential: CredentialValue,
}

struct PreparedTransport {
    request: reqwest::Request,
    client: Client,
    settings: ExecutionSettings,
}

struct ExecutionSettings {
    delivery: DeliveryMode,
    sse_record_limit: usize,
    stop_sequences_declared: bool,
}

impl<A> std::fmt::Debug for OpenAiRuntime<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiRuntime")
            .field("client", &self.client)
            .field("completions_url", &self.completions_url)
            .field("credentials", &"[redacted]")
            .field("sse_record_limit", &self.sse_record_limit)
            .finish()
    }
}

/// Why an [`OpenAiRuntime`] could not be constructed.
///
/// Construction failure is a configuration defect, not operation evidence:
/// no operation exists yet, so nothing is reported as unsent.
#[derive(Debug)]
pub enum OpenAiConstructionError {
    /// The configured base URL is not an acceptable absolute HTTP(S) URL.
    InvalidBaseUrl {
        /// The parser's rendered description.
        detail: String,
    },
    /// The configured SSE record limit cannot admit any record bytes.
    InvalidSseRecordLimit,
    /// The HTTP client could not be constructed.
    ClientConstruction {
        /// The client's rendered description.
        detail: String,
    },
}

impl std::fmt::Display for OpenAiConstructionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBaseUrl { detail } => write!(f, "invalid base URL: {detail}"),
            Self::InvalidSseRecordLimit => {
                f.write_str("SSE record limit must be greater than zero")
            }
            Self::ClientConstruction { detail } => {
                write!(f, "HTTP client construction failed: {detail}")
            }
        }
    }
}

impl std::error::Error for OpenAiConstructionError {}

impl<A: CredentialAccess> OpenAiRuntime<A> {
    /// Builds the adapter and its HTTP client.
    ///
    /// # Transport discipline: one send is one physical request
    ///
    /// Per `docs/spec/runtime-substrate.md`, the client is configured so
    /// that a single send is provably a single request:
    ///
    /// - **Redirect following is disabled** ([`Policy::none`]). reqwest's
    ///   default policy follows up to ten redirects and, on a 307 or 308
    ///   response, replays the buffered POST body — a hidden second physical
    ///   provider interaction inside one send, which would corrupt the
    ///   acceptance-boundary evidence that classification consumes. With
    ///   redirects disabled, a redirect status surfaces as
    ///   [`LossCause::UnexpectedHttpStatus`] evidence instead.
    /// - **Protocol-level retries are disabled** (`reqwest::retry::never()`).
    ///   reqwest's default retry policy resends requests rejected by
    ///   protocol NACKs; a second physical POST for one authorized
    ///   operation is exactly what the one-send discipline prohibits, so
    ///   the never-retry policy is set explicitly.
    /// - **Idle-connection reuse is disabled** (`pool_max_idle_per_host(0)`).
    ///   The underlying HTTP client can transparently resend a request when
    ///   a *reused* idle connection turns out to be closed before the
    ///   request was written; with no idle connections every send opens a
    ///   fresh connection, eliminating that replay path — and making a
    ///   connect failure provably precede any request byte, which is what
    ///   lets [`UnsentCause::ConnectFailed`] claim proven-unsent.
    ///
    /// No timeout budget is specified — timeout budgets remain an open edge
    /// in `docs/spec/model-call-execution.md`: both timeouts default to none
    /// and are caller-owned configuration.
    pub fn new(config: OpenAiConfig, credentials: A) -> Result<Self, OpenAiConstructionError> {
        if config.sse_record_limit == 0 {
            return Err(OpenAiConstructionError::InvalidSseRecordLimit);
        }
        // Parse and validate the caller's base independently. Appending first
        // can turn an authority-less value such as `https://` into the
        // apparently valid but unintended authority `https://v1/...`.
        let mut completions_url = Url::parse(&config.base_url).map_err(|error| {
            OpenAiConstructionError::InvalidBaseUrl {
                detail: error.to_string(),
            }
        })?;
        if completions_url.query().is_some() || completions_url.fragment().is_some() {
            // Concatenating the endpoint path onto a base with a query or
            // fragment would route the request somewhere else entirely.
            return Err(OpenAiConstructionError::InvalidBaseUrl {
                detail: "base URL must not carry a query or fragment".to_string(),
            });
        }
        if !completions_url.username().is_empty() || completions_url.password().is_some() {
            return Err(OpenAiConstructionError::InvalidBaseUrl {
                detail: "base URL must not carry user information".to_string(),
            });
        }
        if !matches!(completions_url.scheme(), "http" | "https") {
            // A non-HTTP scheme would fail only inside send(), after
            // SendCommenced, and read as ambiguous transport loss; it is an
            // invalid configuration, caught here.
            return Err(OpenAiConstructionError::InvalidBaseUrl {
                detail: format!("unsupported scheme {:?}", completions_url.scheme()),
            });
        }
        if completions_url.host_str().is_none() {
            return Err(OpenAiConstructionError::InvalidBaseUrl {
                detail: "base URL must carry an authority".to_string(),
            });
        }
        completions_url
            .path_segments_mut()
            .map_err(|()| OpenAiConstructionError::InvalidBaseUrl {
                detail: "base URL cannot carry path segments".to_string(),
            })?
            .pop_if_empty()
            .extend(["v1", "chat", "completions"]);
        let mut builder = Client::builder()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0);
        if let Some(timeout) = config.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        if let Some(timeout) = config.exchange_timeout {
            builder = builder.timeout(timeout);
        }
        let client =
            builder
                .build()
                .map_err(|error| OpenAiConstructionError::ClientConstruction {
                    detail: error.to_string(),
                })?;
        Ok(Self {
            client,
            completions_url,
            credentials,
            sse_record_limit: config.sse_record_limit,
        })
    }

    async fn prepare_request<C: Clone + Send + Sync>(
        &self,
        operation: ModelOperation<C>,
        cancellation: &mut CancellationSignal,
    ) -> PreparationOutcome<C, OpenAiPreparedRequest<C>> {
        let correlation = operation.correlation.clone();
        let wire_request = match build_request(&operation) {
            Ok(request) => request,
            Err(failure) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure,
                };
            }
        };
        let body = match serialize_request(&wire_request) {
            Ok(body) => body,
            Err(defect) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect,
                };
            }
        };
        // `docs/spec/configuration-and-credentials.md`: the pinned reference
        // is resolved during send preparation of exactly this operation and
        // the value is scoped to this request; nothing is cached, so a
        // rotated credential is picked up by the next operation. The typed
        // reference-only error is preserved, and resolution races the
        // cancellation signal so a blocked credential read cannot hold a
        // cancelled operation.
        let resolve = self.credentials.resolve(&operation.credential_reference);
        let api_key = match with_cancellation(cancellation, resolve).await {
            None => return PreparationOutcome::Cancelled { correlation },
            Some(Err(error)) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure: PreparationFailure::CredentialUnavailable { error },
                };
            }
            Some(Ok(value)) => value,
        };
        let Some(authorization_header) = sensitive_bearer(&api_key) else {
            return PreparationOutcome::Failed {
                correlation,
                failure: PreparationFailure::CredentialUnusable {
                    detail: "credential value cannot form an HTTP header value".to_string(),
                },
            };
        };
        let delivery = operation.delivery;
        let stop_sequences_declared = !operation.settings.stop_sequences.is_empty();
        let request = match build_http_request(
            self.client
                .post(self.completions_url.clone())
                .header(AUTHORIZATION, authorization_header)
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .body(body),
        ) {
            Ok(request) => request,
            Err(defect) => {
                return PreparationOutcome::Defect {
                    correlation,
                    defect,
                };
            }
        };
        PreparationOutcome::Prepared(OpenAiPreparedRequest {
            transport: PreparedTransport {
                request,
                client: self.client.clone(),
                settings: ExecutionSettings {
                    delivery,
                    sse_record_limit: self.sse_record_limit,
                    stop_sequences_declared,
                },
            },
            correlation,
            credential: api_key,
        })
    }

    async fn exchange<C: Clone + Send + Sync>(
        &self,
        transport: PreparedTransport,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
    ) -> TerminalEvidence {
        let PreparedTransport {
            request,
            client,
            settings,
        } = transport;
        emit(correlation, sink, ObservationFact::SendCommenced);
        let send = client.execute(request);
        let response = match with_cancellation(cancellation, send).await {
            None => return pre_exchange_loss(LossCause::CancellationRequested),
            Some(Err(error)) => return classify_send_error(&error),
            Some(Ok(response)) => response,
        };
        let status = response.status();
        let exchange = ExchangeFacts {
            provider_request_id: request_id_from(response.headers()),
            http_status: Some(status.as_u16()),
        };
        emit(
            correlation,
            sink,
            ObservationFact::ExchangeEstablished(exchange.clone()),
        );
        // The Chat Completions success contract is specifically HTTP 200;
        // another 2xx is not recognized terminal-success evidence.
        if status.as_u16() == 200 {
            match settings.delivery {
                DeliveryMode::Buffered => {
                    self.finish_buffered(
                        response,
                        exchange,
                        correlation,
                        sink,
                        cancellation,
                        settings.stop_sequences_declared,
                    )
                    .await
                }
                DeliveryMode::Streamed => {
                    self.finish_streamed(
                        response,
                        exchange,
                        correlation,
                        sink,
                        cancellation,
                        &settings,
                    )
                    .await
                }
            }
        } else if status.is_client_error() || status.is_server_error() {
            finish_error(response, exchange, status.as_u16(), cancellation).await
        } else {
            // With redirects disabled a redirect (or any other status
            // outside the provider's documented contract) surfaces as
            // evidence rather than a silent second send; see `new` for the
            // rationale.
            TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
                cause: LossCause::UnexpectedHttpStatus,
                exchange,
                reported_model: None,
                finish_reported: None,
                usage: TokenUsage::unreported(),
            })
        }
    }

    async fn finish_buffered<C: Clone + Send + Sync>(
        &self,
        response: reqwest::Response,
        exchange: ExchangeFacts,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
        stop_sequences_declared: bool,
    ) -> TerminalEvidence {
        let body = match collect_response_body(response, cancellation).await {
            None => return exchange_loss(LossCause::CancellationRequested, exchange),
            Some(Err(cause)) => return exchange_loss(cause, exchange),
            Some(Ok(bytes)) => bytes,
        };
        decode_buffered_response(&body, exchange, correlation, sink, stop_sequences_declared)
    }

    async fn finish_streamed<C: Clone + Send + Sync>(
        &self,
        response: reqwest::Response,
        exchange: ExchangeFacts,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
        settings: &ExecutionSettings,
    ) -> TerminalEvidence {
        let mut framing = SseFraming::new(settings.sse_record_limit);
        let mut decoder = StreamDecoder::new(exchange, settings.stop_sequences_declared);
        let mut body = response.bytes_stream();
        let mut streamed_bytes = 0usize;
        loop {
            let chunk = match with_cancellation(cancellation, body.next()).await {
                None => return decoder.cancelled(),
                Some(chunk) => chunk,
            };
            match chunk {
                // End of transport without `[DONE]`: the explicit
                // incomplete-stream fact, never silent success.
                None => {
                    return match framing.finish() {
                        signalbox_model_runtime::SseTermination::Clean => {
                            decoder.lost(StreamInterruption::EndOfStream)
                        }
                        signalbox_model_runtime::SseTermination::TruncatedRecord => decoder
                            .violation_evidence(
                                "transport ended inside an incomplete SSE record".to_string(),
                            ),
                    };
                }
                Some(Err(error)) => {
                    let facts = transport_facts(&error);
                    let interruption = if error.is_timeout() {
                        StreamInterruption::TimedOut(facts)
                    } else {
                        StreamInterruption::TransportFailure(facts)
                    };
                    return decoder.lost(interruption);
                }
                Some(Ok(bytes)) => {
                    if let Some(evidence) = process_streamed_chunk(
                        &bytes,
                        &mut streamed_bytes,
                        &mut framing,
                        &mut decoder,
                        correlation,
                        sink,
                        cancellation,
                    ) {
                        return evidence;
                    }
                }
            }
        }
    }
}

fn serialize_request(value: &impl serde::Serialize) -> Result<Vec<u8>, PreparationDefect> {
    serde_json::to_vec(value).map_err(|error| PreparationDefect::SerializationFailed {
        detail: error.to_string(),
    })
}

fn build_http_request(
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::Request, PreparationDefect> {
    builder
        .build()
        .map_err(|error| PreparationDefect::RequestConstructionFailed {
            detail: error.to_string(),
        })
}

fn streamed_response_prefix_len(current: usize, chunk: usize) -> (usize, bool) {
    let remaining = MAX_STREAMED_RESPONSE_BYTES.saturating_sub(current);
    (chunk.min(remaining), chunk > remaining)
}

fn process_streamed_chunk<C: Clone>(
    bytes: &[u8],
    streamed_bytes: &mut usize,
    framing: &mut SseFraming,
    decoder: &mut StreamDecoder,
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
    cancellation: &mut CancellationSignal,
) -> Option<TerminalEvidence> {
    let (accepted, exceeded) = streamed_response_prefix_len(*streamed_bytes, bytes.len());
    *streamed_bytes += accepted;
    // Apply records completed by the in-budget prefix before reporting a
    // framing or aggregate-size failure. A terminal marker in that prefix
    // must not be lost because trailing bytes share its transport chunk.
    let outcome = framing.push(&bytes[..accepted]);
    for (index, record) in outcome.records.into_iter().enumerate() {
        if index > 0 && already_fired(cancellation) {
            return Some(decoder.cancelled());
        }
        match decoder.apply(&record, correlation, sink) {
            StreamStep::Continue => {}
            StreamStep::Terminal(evidence) => return Some(*evidence),
        }
    }
    if let Some(error) = outcome.error {
        return Some(decoder.violation_evidence(error.to_string()));
    }
    exceeded.then(|| {
        decoder.violation_evidence(format!(
            "streamed response exceeded the {MAX_STREAMED_RESPONSE_BYTES}-byte adapter limit"
        ))
    })
}

impl<C: Clone + Send + Sync, A: CredentialAccess> ModelRuntime<C> for OpenAiRuntime<A> {
    type Prepared = OpenAiPreparedRequest<C>;

    async fn prepare(
        &self,
        operation: ModelOperation<C>,
        mut cancellation: CancellationSignal,
    ) -> PreparationOutcome<C, Self::Prepared> {
        self.prepare_request(operation, &mut cancellation).await
    }

    async fn execute(
        &self,
        prepared: Self::Prepared,
        sink: &mut (dyn ObservationSink<C> + Send),
        mut cancellation: CancellationSignal,
    ) -> TerminalReport<C> {
        let OpenAiPreparedRequest {
            transport,
            correlation,
            credential,
        } = prepared;
        if already_fired(&mut cancellation) {
            return TerminalReport {
                correlation,
                evidence: proven_unsent(UnsentCause::CancelledBeforeSend),
            };
        }
        let mut redacting_sink = RedactingSink::new(sink, &credential);
        let evidence = self
            .exchange(
                transport,
                &correlation,
                &mut redacting_sink,
                &mut cancellation,
            )
            .await;
        redacting_sink.flush();
        // A buffered reqwest request provides no independent proof that an
        // early response followed the complete upload.
        // `docs/spec/model-call-execution.md` therefore forbids classifying
        // its refusal token as definitive `Refused`.
        let evidence = without_unproven_refusal(evidence);
        // Per the runtime-substrate spec, sanitize with the exact
        // preparation-time value, after no second credential lookup or
        // request reconstruction.
        let evidence = redact_evidence(evidence, &credential);
        TerminalReport {
            correlation,
            evidence,
        }
    }
}

fn without_unproven_refusal(evidence: TerminalEvidence) -> TerminalEvidence {
    match evidence {
        TerminalEvidence::Refused(refusal) => {
            TerminalEvidence::ProviderError(ProviderErrorEvidence {
                exchange: refusal.exchange,
                reported_model: refusal.reported_model,
                kind: ProviderErrorKind::Unrecognized,
                native: NativeErrorFacts {
                    // Refusal came from `finish_reason` or `message.refusal`,
                    // not from a native error-envelope token.
                    error_token: None,
                    error_code: None,
                    message: None,
                },
                usage: refusal.usage,
            })
        }
        evidence => evidence,
    }
}

async fn finish_error(
    response: reqwest::Response,
    exchange: ExchangeFacts,
    status: u16,
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    let body = match collect_response_body(response, cancellation).await {
        None => return exchange_loss(LossCause::CancellationRequested, exchange),
        Some(Err(cause)) => return exchange_loss(cause, exchange),
        Some(Ok(bytes)) => bytes,
    };
    if let Ok(ErrorEnvelope { error: Some(error) }) = serde_json::from_slice(&body) {
        let code = error.code_text();
        let kind = classify_error_envelope(status, code.as_deref(), error.error_type.as_deref());
        return TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange,
            // The Chat Completions error envelope reports no model identity.
            reported_model: None,
            kind,
            native: error.into_native_facts(),
            usage: TokenUsage::unreported(),
        });
    }
    // A complete terminal error status whose body is not the documented
    // envelope is still definitive (per the runtime-substrate spec);
    // classify by status and retain the raw body as native material.
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange,
        reported_model: None,
        kind: classify_error(status, None),
        native: NativeErrorFacts {
            error_token: None,
            error_code: None,
            message: Some(lossy_truncated(&body)),
        },
        usage: TokenUsage::unreported(),
    })
}

async fn collect_response_body(
    response: reqwest::Response,
    cancellation: &mut CancellationSignal,
) -> Option<Result<Vec<u8>, LossCause>> {
    let mut body = response.bytes_stream();
    let mut collected = Vec::new();
    loop {
        match with_cancellation(cancellation, body.next()).await {
            None => return None,
            Some(None) => return Some(Ok(collected)),
            Some(Some(Err(error))) => return Some(Err(classify_body_error(&error))),
            Some(Some(Ok(chunk))) => {
                let Some(next_len) = collected.len().checked_add(chunk.len()) else {
                    return Some(Err(response_body_too_large()));
                };
                if next_len > MAX_BUFFERED_RESPONSE_BYTES {
                    return Some(Err(response_body_too_large()));
                }
                collected.extend_from_slice(&chunk);
            }
        }
    }
}

fn response_body_too_large() -> LossCause {
    LossCause::ResponseBodyLost(TransportFacts::new(format!(
        "response body exceeded the {MAX_BUFFERED_RESPONSE_BYTES}-byte adapter limit"
    )))
}

fn emit<C: Clone>(
    correlation: &C,
    sink: &mut (dyn ObservationSink<C> + Send),
    fact: ObservationFact,
) {
    sink.observe(Observation {
        correlation: correlation.clone(),
        fact,
    });
}

fn proven_unsent(cause: UnsentCause) -> TerminalEvidence {
    TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence { cause })
}

fn pre_exchange_loss(cause: LossCause) -> TerminalEvidence {
    exchange_loss(cause, ExchangeFacts::default())
}

fn exchange_loss(cause: LossCause, exchange: ExchangeFacts) -> TerminalEvidence {
    TerminalEvidence::BoundaryLoss(BoundaryLossEvidence {
        cause,
        exchange,
        reported_model: None,
        finish_reported: None,
        usage: TokenUsage::unreported(),
    })
}

/// Classifies a send-phase transport failure per the runtime-substrate
/// spec's full-request-send rule.
///
/// Every request uses a fresh connection (see [`OpenAiRuntime::new`]), so a
/// connect failure provably precedes any request byte and classifies as
/// proven-unsent. Everything else — timeout, connection loss, interrupted
/// write — cannot be proven to precede the acceptance-capable boundary and
/// is boundary-loss (ambiguous) evidence.
fn classify_send_error(error: &reqwest::Error) -> TerminalEvidence {
    if error.is_connect() {
        proven_unsent(UnsentCause::ConnectFailed(transport_facts(error)))
    } else if error.is_timeout() {
        pre_exchange_loss(LossCause::TimedOut(transport_facts(error)))
    } else {
        pre_exchange_loss(LossCause::TransportFailed(transport_facts(error)))
    }
}

/// Renders the transport error with its source chain, as retained evidence
/// only; classification never reads this text.
fn transport_facts(error: &reqwest::Error) -> TransportFacts {
    let mut detail = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    TransportFacts::new(detail)
}

/// Classifies a body-phase read failure: a caller-configured deadline keeps
/// its typed timeout cause; anything else is a lost response body. Either
/// way the exchange lacks a definitive response (the ambiguous branch in
/// `docs/spec/model-call-execution.md`).
fn classify_body_error(error: &reqwest::Error) -> LossCause {
    if error.is_timeout() {
        LossCause::TimedOut(transport_facts(error))
    } else {
        LossCause::ResponseBodyLost(transport_facts(error))
    }
}

fn request_id_from(headers: &HeaderMap) -> Option<ProviderRequestId> {
    headers
        .get("x-request-id")
        .or_else(|| headers.get("request-id"))
        .and_then(|value| value.to_str().ok())
        .map(ProviderRequestId::new)
}

fn lossy_truncated(body: &[u8]) -> String {
    const LIMIT: usize = 2048;
    let text = String::from_utf8_lossy(body);
    if text.len() <= LIMIT {
        return text.into_owned();
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … [truncated]", &text[..end])
}

/// True when the cancellation signal has already fired, checked without
/// blocking.
fn already_fired(cancellation: &mut CancellationSignal) -> bool {
    let mut context = Context::from_waker(Waker::noop());
    Pin::new(cancellation).poll(&mut context).is_ready()
}

/// Runs `work` unless the cancellation signal fires while it is pending.
///
/// The work future is polled first, so provider evidence that is already
/// available wins a same-poll race against cancellation: a ready definitive
/// response is never discarded in favor of ambiguous cancellation loss (the
/// runtime-substrate spec's definitive-response precedence).
async fn with_cancellation<F: Future>(
    cancellation: &mut CancellationSignal,
    work: F,
) -> Option<F::Output> {
    let work = std::pin::pin!(work);
    match select(work, cancellation).await {
        Either::Left((output, _)) => Some(output),
        Either::Right(((), _)) => None,
    }
}

struct RedactingSink<'a, C> {
    inner: &'a mut (dyn ObservationSink<C> + Send),
    credential: &'a CredentialValue,
    credential_text: &'a str,
    pending_stream_text: Option<PendingStreamText<C>>,
    pending_tool_arguments: Option<PendingToolArguments<C>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StreamField {
    Text,
    Thinking,
}

struct PendingStreamText<C> {
    field: StreamField,
    index: u32,
    correlation: C,
    text: String,
}

struct PendingToolArguments<C> {
    index: u32,
    correlation: C,
    fragment: String,
}

impl<'a, C: Clone> RedactingSink<'a, C> {
    fn new(
        inner: &'a mut (dyn ObservationSink<C> + Send),
        credential: &'a CredentialValue,
    ) -> Self {
        Self {
            inner,
            credential,
            credential_text: std::str::from_utf8(credential.expose_bytes()).unwrap_or_default(),
            pending_stream_text: None,
            pending_tool_arguments: None,
        }
    }

    fn flush_stream_text(&mut self) {
        if let Some(pending) = self.pending_stream_text.take() {
            // Pending text is exactly a credential prefix. Once another fact
            // must cross the boundary, retain ordering and fail closed.
            self.emit_stream_text(
                pending.field,
                pending.index,
                pending.correlation,
                "[redacted]".to_string(),
            );
        }
    }

    fn flush_tool_arguments(&mut self) {
        if let Some(pending) = self.pending_tool_arguments.take() {
            self.emit_tool_arguments(pending.index, pending.correlation, "[redacted]".to_string());
        }
    }

    fn flush(&mut self) {
        self.flush_stream_text();
        self.flush_tool_arguments();
    }

    fn emit_stream_text(&mut self, field: StreamField, index: u32, correlation: C, text: String) {
        if text.is_empty() {
            return;
        }
        let fact = match field {
            StreamField::Text => ObservationFact::TextDelta { index, text },
            StreamField::Thinking => ObservationFact::ThinkingDelta { index, text },
        };
        self.inner.observe(Observation { correlation, fact });
    }

    fn emit_tool_arguments(&mut self, index: u32, correlation: C, fragment: String) {
        if !fragment.is_empty() {
            self.inner.observe(Observation {
                correlation,
                fact: ObservationFact::ToolArgumentsDelta { index, fragment },
            });
        }
    }

    fn redact_stream_delta(
        &mut self,
        field: StreamField,
        index: u32,
        correlation: C,
        text: String,
    ) {
        self.flush_tool_arguments();
        if self
            .pending_stream_text
            .as_ref()
            .is_some_and(|pending| pending.field != field || pending.index != index)
        {
            self.flush_stream_text();
        }
        let mut combined = self
            .pending_stream_text
            .take()
            .map_or_else(String::new, |pending| pending.text);
        combined.push_str(&text);
        let (emitted, pending) =
            redact_complete_credentials_and_hold_prefix(combined, self.credential_text);
        self.emit_stream_text(field, index, correlation.clone(), emitted);
        if !pending.is_empty() {
            self.pending_stream_text = Some(PendingStreamText {
                field,
                index,
                correlation,
                text: pending,
            });
        }
    }

    fn redact_tool_delta(&mut self, index: u32, correlation: C, fragment: String) {
        self.flush_stream_text();
        if self
            .pending_tool_arguments
            .as_ref()
            .is_some_and(|pending| pending.index != index)
        {
            self.flush_tool_arguments();
        }
        let mut combined = self
            .pending_tool_arguments
            .take()
            .map_or_else(String::new, |pending| pending.fragment);
        combined.push_str(&fragment);
        let (emitted, pending) = redact_json_stream_fragment(combined, self.credential_text);
        self.emit_tool_arguments(index, correlation.clone(), emitted);
        if !pending.is_empty() {
            self.pending_tool_arguments = Some(PendingToolArguments {
                index,
                correlation,
                fragment: pending,
            });
        }
    }
}

impl<C: Clone> ObservationSink<C> for RedactingSink<'_, C> {
    fn observe(&mut self, observation: Observation<C>) {
        match observation.fact {
            ObservationFact::TextDelta { index, text } => {
                self.redact_stream_delta(StreamField::Text, index, observation.correlation, text)
            }
            ObservationFact::ThinkingDelta { index, text } => self.redact_stream_delta(
                StreamField::Thinking,
                index,
                observation.correlation,
                text,
            ),
            ObservationFact::ToolArgumentsDelta { index, fragment } => {
                self.redact_tool_delta(index, observation.correlation, fragment);
            }
            ObservationFact::ToolCallProposed(proposal) => {
                self.flush();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: ObservationFact::ToolCallProposed(redact_tool_proposal(
                        proposal,
                        self.credential,
                    )),
                });
            }
            fact => {
                self.flush();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: redact_observation_fact(fact, self.credential),
                });
            }
        }
    }
}

fn redact_complete_credentials_and_hold_prefix(
    mut text: String,
    credential: &str,
) -> (String, String) {
    if credential.is_empty() {
        return (text, String::new());
    }
    // Only the tail after the last complete, non-overlapping match can become
    // a credential when the next provider chunk arrives. A proper suffix that
    // starts inside an already-complete match must be redacted with that match,
    // not retained and emitted later.
    let unmatched_tail_start = text
        .match_indices(credential)
        .last()
        .map_or(0, |(start, matched)| start + matched.len());
    let unmatched_tail = &text[unmatched_tail_start..];
    let longest_prefix = (1..credential.len())
        .rev()
        .filter(|length| credential.is_char_boundary(*length))
        .find(|length| unmatched_tail.ends_with(&credential[..*length]));
    let split = longest_prefix.map_or(text.len(), |length| text.len() - length);
    let pending = text.split_off(split);
    (text.replace(credential, "[redacted]"), pending)
}

struct PendingJsonUnit {
    raw_start: usize,
    raw_end: usize,
}

/// Sanitizes one accumulated streamed JSON fragment while retaining only a
/// suffix whose decoded characters could still complete the credential.
fn redact_json_stream_fragment(raw: String, credential: &str) -> (String, String) {
    if credential.is_empty() {
        return (raw, String::new());
    }

    let fallback = |raw: String| {
        if json_escapes_decode_to_credential(&raw, credential) {
            ("[redacted]".to_string(), String::new())
        } else {
            redact_complete_credentials_and_hold_prefix(raw, credential)
        }
    };
    let bytes = raw.as_bytes();
    let pattern: Vec<char> = credential.chars().collect();
    let mut prefix_lengths = vec![0; pattern.len()];
    for index in 1..pattern.len() {
        let mut prefix = prefix_lengths[index - 1];
        while prefix > 0 && pattern[index] != pattern[prefix] {
            prefix = prefix_lengths[prefix - 1];
        }
        if pattern[index] == pattern[prefix] {
            prefix += 1;
        }
        prefix_lengths[index] = prefix;
    }

    let mut cursor = 0;
    let mut matched = 0;
    let mut pending: VecDeque<PendingJsonUnit> = VecDeque::with_capacity(pattern.len());
    let mut emitted = String::with_capacity(raw.len());
    while cursor < raw.len() {
        let raw_start = cursor;
        let character = if bytes[cursor] != b'\\' {
            let Some(character) = raw[cursor..].chars().next() else {
                return fallback(raw);
            };
            cursor += character.len_utf8();
            character
        } else {
            if cursor + 1 >= raw.len() {
                break;
            }
            match bytes[cursor + 1] {
                b'"' => {
                    cursor += 2;
                    '"'
                }
                b'\\' => {
                    cursor += 2;
                    '\\'
                }
                b'/' => {
                    cursor += 2;
                    '/'
                }
                b'b' => {
                    cursor += 2;
                    '\u{0008}'
                }
                b'f' => {
                    cursor += 2;
                    '\u{000c}'
                }
                b'n' => {
                    cursor += 2;
                    '\n'
                }
                b'r' => {
                    cursor += 2;
                    '\r'
                }
                b't' => {
                    cursor += 2;
                    '\t'
                }
                b'u' => {
                    if cursor + 6 > raw.len() {
                        break;
                    }
                    let Ok(hex) = std::str::from_utf8(&bytes[cursor + 2..cursor + 6]) else {
                        return fallback(raw);
                    };
                    let Ok(first) = u16::from_str_radix(hex, 16) else {
                        return fallback(raw);
                    };
                    if (0xd800..=0xdbff).contains(&first) {
                        if cursor + 12 > raw.len() {
                            break;
                        }
                        if &bytes[cursor + 6..cursor + 8] != b"\\u" {
                            return fallback(raw);
                        }
                        let Ok(hex) = std::str::from_utf8(&bytes[cursor + 8..cursor + 12]) else {
                            return fallback(raw);
                        };
                        let Ok(second) = u16::from_str_radix(hex, 16) else {
                            return fallback(raw);
                        };
                        if !(0xdc00..=0xdfff).contains(&second) {
                            return fallback(raw);
                        }
                        let scalar = 0x1_0000
                            + ((u32::from(first) - 0xd800) << 10)
                            + (u32::from(second) - 0xdc00);
                        let Some(character) = char::from_u32(scalar) else {
                            return fallback(raw);
                        };
                        cursor += 12;
                        character
                    } else {
                        if (0xdc00..=0xdfff).contains(&first) {
                            return fallback(raw);
                        }
                        let Some(character) = char::from_u32(u32::from(first)) else {
                            return fallback(raw);
                        };
                        cursor += 6;
                        character
                    }
                }
                _ => return fallback(raw),
            }
        };

        while matched > 0 && pattern[matched] != character {
            let retained = prefix_lengths[matched - 1];
            for _ in retained..matched {
                let Some(unit) = pending.pop_front() else {
                    return fallback(raw);
                };
                emitted.push_str(&raw[unit.raw_start..unit.raw_end]);
            }
            matched = retained;
        }
        if pattern[matched] == character {
            pending.push_back(PendingJsonUnit {
                raw_start,
                raw_end: cursor,
            });
            matched += 1;
            if matched == pattern.len() {
                emitted.push_str("[redacted]");
                pending.clear();
                matched = 0;
            }
        } else {
            emitted.push_str(&raw[raw_start..cursor]);
        }
    }

    let pending_start = pending
        .front()
        .map_or(cursor, |unit: &PendingJsonUnit| unit.raw_start.min(cursor));
    (emitted, raw[pending_start..].to_string())
}

fn redact_observation_fact(fact: ObservationFact, credential: &CredentialValue) -> ObservationFact {
    match fact {
        ObservationFact::ExchangeEstablished(exchange) => {
            ObservationFact::ExchangeEstablished(redact_exchange(exchange, credential))
        }
        ObservationFact::ProviderModelReported(model) => ObservationFact::ProviderModelReported(
            ProviderReportedModel::new(redact_text(model.as_str().to_string(), credential)),
        ),
        ObservationFact::TextDelta { index, text } => ObservationFact::TextDelta {
            index,
            text: redact_text(text, credential),
        },
        ObservationFact::ThinkingDelta { index, text } => ObservationFact::ThinkingDelta {
            index,
            text: redact_text(text, credential),
        },
        ObservationFact::ToolArgumentsDelta { index, fragment } => {
            ObservationFact::ToolArgumentsDelta {
                index,
                fragment: redact_text(fragment, credential),
            }
        }
        ObservationFact::ToolCallProposed(proposal) => {
            ObservationFact::ToolCallProposed(redact_tool_proposal(proposal, credential))
        }
        ObservationFact::FinishReported(FinishReason::Unrecognized { provider_token }) => {
            ObservationFact::FinishReported(FinishReason::Unrecognized {
                provider_token: redact_text(provider_token, credential),
            })
        }
        fact @ (ObservationFact::SendCommenced
        | ObservationFact::UsageReported(_)
        | ObservationFact::FinishReported(_)) => fact,
    }
}

/// Credential-sanitizes every provider-controlled or transport-rendered
/// text in the evidence, per the runtime-substrate spec: a reflected key
/// value in an error message, raw body, or rendered detail is replaced
/// before the evidence leaves the adapter boundary. Typed facts are
/// untouched.
fn redact_evidence(evidence: TerminalEvidence, api_key: &CredentialValue) -> TerminalEvidence {
    let key_text = std::str::from_utf8(api_key.expose_bytes()).unwrap_or_default();
    let redact = move |text: String| -> String {
        if key_text.is_empty() {
            text
        } else {
            text.replace(key_text, "[redacted]")
        }
    };
    let redact_native = |mut native: NativeErrorFacts| -> NativeErrorFacts {
        native.error_token = native.error_token.map(redact);
        native.error_code = native.error_code.map(redact);
        native.message = native
            .message
            .map(|message| redact_native_message(message, api_key));
        native
    };
    let redact_transport =
        |facts: TransportFacts| -> TransportFacts { TransportFacts::new(redact(facts.detail)) };
    match evidence {
        TerminalEvidence::ProviderError(mut error) => {
            error.exchange = redact_exchange(error.exchange, api_key);
            error.reported_model = error.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            error.native = redact_native(error.native);
            TerminalEvidence::ProviderError(error)
        }
        TerminalEvidence::CancellationConfirmed(mut confirmed) => {
            confirmed.exchange = redact_exchange(confirmed.exchange, api_key);
            confirmed.reported_model = confirmed.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            confirmed.native = redact_native(confirmed.native);
            TerminalEvidence::CancellationConfirmed(confirmed)
        }
        TerminalEvidence::ProvenUnsent(unsent) => {
            let cause = match unsent.cause {
                UnsentCause::ConnectFailed(facts) => {
                    UnsentCause::ConnectFailed(redact_transport(facts))
                }
                UnsentCause::SendIncompleteProvenUnacceptable(facts) => {
                    UnsentCause::SendIncompleteProvenUnacceptable(redact_transport(facts))
                }
                UnsentCause::CancelledBeforeSend => UnsentCause::CancelledBeforeSend,
            };
            TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence { cause })
        }
        TerminalEvidence::BoundaryLoss(mut loss) => {
            loss.exchange = redact_exchange(loss.exchange, api_key);
            loss.reported_model = loss.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            loss.finish_reported = loss.finish_reported.map(|finish| match finish {
                FinishReason::Unrecognized { provider_token } => FinishReason::Unrecognized {
                    provider_token: redact_text(provider_token, api_key),
                },
                finish => finish,
            });
            loss.cause = match loss.cause {
                LossCause::TimedOut(facts) => LossCause::TimedOut(redact_transport(facts)),
                LossCause::TransportFailed(facts) => {
                    LossCause::TransportFailed(redact_transport(facts))
                }
                LossCause::ResponseBodyLost(facts) => {
                    LossCause::ResponseBodyLost(redact_transport(facts))
                }
                LossCause::ResponseUnintelligible { detail } => LossCause::ResponseUnintelligible {
                    detail: redact(detail),
                },
                LossCause::StreamProtocolViolation { detail } => {
                    LossCause::StreamProtocolViolation {
                        detail: redact(detail),
                    }
                }
                LossCause::StreamEndedWithoutTerminalMarker { interruption } => {
                    LossCause::StreamEndedWithoutTerminalMarker {
                        interruption: match interruption {
                            StreamInterruption::TransportFailure(facts) => {
                                StreamInterruption::TransportFailure(redact_transport(facts))
                            }
                            StreamInterruption::TimedOut(facts) => {
                                StreamInterruption::TimedOut(redact_transport(facts))
                            }
                            StreamInterruption::EndOfStream => StreamInterruption::EndOfStream,
                        },
                    }
                }
                cause @ (LossCause::CancellationRequested | LossCause::UnexpectedHttpStatus) => {
                    cause
                }
            };
            TerminalEvidence::BoundaryLoss(loss)
        }
        TerminalEvidence::Completed(mut completion) => {
            completion.exchange = redact_exchange(completion.exchange, api_key);
            completion.message_id = completion
                .message_id
                .map(|id| ProviderMessageId::new(redact_text(id.as_str().to_string(), api_key)));
            completion.reported_model = completion.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            completion.finish = redact_completion_finish(completion.finish, api_key);
            completion.content = completion
                .content
                .into_iter()
                .map(|part| redact_assistant_part(part, api_key))
                .collect();
            TerminalEvidence::Completed(completion)
        }
        TerminalEvidence::Refused(mut refusal) => {
            refusal.exchange = redact_exchange(refusal.exchange, api_key);
            refusal.message_id = refusal
                .message_id
                .map(|id| ProviderMessageId::new(redact_text(id.as_str().to_string(), api_key)));
            refusal.reported_model = refusal.reported_model.map(|model| {
                ProviderReportedModel::new(redact_text(model.as_str().to_string(), api_key))
            });
            refusal.content = refusal
                .content
                .into_iter()
                .map(|part| redact_assistant_part(part, api_key))
                .collect();
            TerminalEvidence::Refused(refusal)
        }
    }
}

fn redact_text(text: String, credential: &CredentialValue) -> String {
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    if key.is_empty() {
        text
    } else {
        text.replace(key, "[redacted]")
    }
}

/// Redacts complete credentials and fails closed on a trailing prefix. Final
/// content parts are independently persisted values, so a prefix may not
/// leave one part and be completed by the next.
fn redact_bounded_text(text: String, credential: &CredentialValue) -> String {
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let (mut redacted, pending) = redact_complete_credentials_and_hold_prefix(text, key);
    if !pending.is_empty() {
        redacted.push_str("[redacted]");
    }
    redacted
}

fn redact_native_message(text: String, credential: &CredentialValue) -> String {
    const TRUNCATION_SUFFIX: &str = " … [truncated]";
    if let Some(body) = text.strip_suffix(TRUNCATION_SUFFIX) {
        let mut redacted = redact_native_body(body.to_string(), credential);
        redacted.push_str(TRUNCATION_SUFFIX);
        redacted
    } else {
        redact_native_body(text, credential)
    }
}

fn redact_native_body(text: String, credential: &CredentialValue) -> String {
    if serde_json::value::RawValue::from_string(text.clone()).is_ok() {
        return redact_json(text, credential);
    }
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    if json_escapes_decode_to_credential(&text, key) {
        return "\"[redacted]\"".to_string();
    }
    let (mut redacted, pending) = redact_complete_credentials_and_hold_prefix(text, key);
    if !pending.is_empty() {
        redacted.push_str("[redacted]");
    }
    redacted
}

fn redact_completion_finish(
    finish: CompletionFinish,
    credential: &CredentialValue,
) -> CompletionFinish {
    match finish {
        CompletionFinish::StopSequence { sequence } => CompletionFinish::StopSequence {
            sequence: sequence.map(|value| redact_text(value, credential)),
        },
        CompletionFinish::Unrecognized { provider_token } => CompletionFinish::Unrecognized {
            provider_token: redact_text(provider_token, credential),
        },
        finish => finish,
    }
}

fn redact_exchange(mut exchange: ExchangeFacts, credential: &CredentialValue) -> ExchangeFacts {
    exchange.provider_request_id = exchange
        .provider_request_id
        .map(|id| ProviderRequestId::new(redact_text(id.as_str().to_string(), credential)));
    exchange
}

fn redact_tool_proposal(
    proposal: ToolCallProposal,
    credential: &CredentialValue,
) -> ToolCallProposal {
    ToolCallProposal {
        id: ToolCallId::new(redact_text(proposal.id.as_str().to_string(), credential)),
        name: ToolName::new(redact_text(proposal.name.as_str().to_string(), credential)),
        arguments_json: redact_json(proposal.arguments_json, credential),
    }
}

fn redact_json(raw: String, credential: &CredentialValue) -> String {
    let key = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    if key.is_empty() {
        return raw;
    }
    if serde_json::value::RawValue::from_string(raw.clone()).is_err() {
        // A partial or malformed JSON value can encode a credential or its
        // trailing prefix with escapes that literal replacement cannot see.
        // Reuse the streaming decoder, then fail closed on any held prefix;
        // other malformed provider bytes remain for typed decoding to judge.
        let (mut redacted, pending) = redact_json_stream_fragment(raw, key);
        if !pending.is_empty() {
            redacted.push_str("[redacted]");
        }
        return redacted;
    }

    let mut redacted = String::with_capacity(raw.len());
    let mut cursor = 0;
    while cursor < raw.len() {
        if raw.as_bytes()[cursor] == b'"' {
            let mut end = cursor + 1;
            let mut escaped = false;
            while end < raw.len() {
                let byte = raw.as_bytes()[end];
                end += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
            }
            let token = &raw[cursor..end];
            let Ok(decoded) = serde_json::from_str::<String>(token) else {
                return redact_text(raw, credential);
            };
            if decoded.contains(key) {
                let Ok(sanitized) = serde_json::to_string(&decoded.replace(key, "[redacted]"))
                else {
                    return "\"[redacted]\"".to_string();
                };
                redacted.push_str(&sanitized);
            } else {
                redacted.push_str(token);
            }
            cursor = end;
            continue;
        }

        if matches!(
            raw.as_bytes()[cursor],
            b'{' | b'}' | b'[' | b']' | b',' | b':'
        ) || raw.as_bytes()[cursor].is_ascii_whitespace()
        {
            redacted.push(raw.as_bytes()[cursor] as char);
            cursor += 1;
            continue;
        }

        let start = cursor;
        while cursor < raw.len()
            && !matches!(
                raw.as_bytes()[cursor],
                b'{' | b'}' | b'[' | b']' | b',' | b':' | b' ' | b'\t' | b'\r' | b'\n'
            )
        {
            cursor += 1;
        }
        let token = &raw[start..cursor];
        if token.contains(key) {
            redacted.push_str("\"[redacted]\"");
        } else {
            redacted.push_str(token);
        }
    }
    redacted
}

fn json_escapes_decode_to_credential(raw: &str, credential: &str) -> bool {
    let mut decoded = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        let Some(escaped) = chars.next() else {
            decoded.push('\\');
            break;
        };
        match escaped {
            '"' | '\\' | '/' => decoded.push(escaped),
            'b' => decoded.push('\u{0008}'),
            'f' => decoded.push('\u{000c}'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            'u' => {
                let digits: String = chars.by_ref().take(4).collect();
                if digits.len() != 4 {
                    continue;
                }
                let Ok(first) = u16::from_str_radix(&digits, 16) else {
                    continue;
                };
                if (0xd800..=0xdbff).contains(&first) {
                    let mut pair = chars.clone();
                    if pair.next() != Some('\\') || pair.next() != Some('u') {
                        continue;
                    }
                    let low_digits: String = pair.by_ref().take(4).collect();
                    let Ok(second) = u16::from_str_radix(&low_digits, 16) else {
                        continue;
                    };
                    if !(0xdc00..=0xdfff).contains(&second) {
                        continue;
                    }
                    let scalar = 0x1_0000
                        + ((u32::from(first) - 0xd800) << 10)
                        + (u32::from(second) - 0xdc00);
                    if let Some(decoded_character) = char::from_u32(scalar) {
                        decoded.push(decoded_character);
                        chars = pair;
                    }
                } else if !(0xdc00..=0xdfff).contains(&first)
                    && let Some(decoded_character) = char::from_u32(u32::from(first))
                {
                    decoded.push(decoded_character);
                }
            }
            other => decoded.push(other),
        }
    }
    decoded.contains(credential)
}

fn redact_assistant_part(part: AssistantPart, credential: &CredentialValue) -> AssistantPart {
    match part {
        AssistantPart::Text(text) => AssistantPart::Text(redact_bounded_text(text, credential)),
        AssistantPart::Thinking { text, signature } => AssistantPart::Thinking {
            text: redact_bounded_text(text, credential),
            signature: signature.map(|value| redact_bounded_text(value, credential)),
        },
        AssistantPart::RedactedThinking { data } => AssistantPart::RedactedThinking {
            data: redact_bounded_text(data, credential),
        },
        AssistantPart::ToolCall(proposal) => {
            AssistantPart::ToolCall(redact_tool_proposal(proposal, credential))
        }
    }
}

/// The credential as a sensitivity-marked bearer header value, or `None`
/// when its bytes cannot form one. The value never appears in errors or
/// logs.
fn sensitive_bearer(api_key: &CredentialValue) -> Option<HeaderValue> {
    if api_key.expose_bytes().is_empty() || std::str::from_utf8(api_key.expose_bytes()).is_err() {
        return None;
    }
    let mut bytes = b"Bearer ".to_vec();
    bytes.extend_from_slice(api_key.expose_bytes());
    let mut header = HeaderValue::from_bytes(&bytes).ok()?;
    header.set_sensitive(true);
    Some(header)
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use signalbox_model_runtime::{
        AssistantPart, CancellationSignal, CompletionEvidence, CompletionFinish, CredentialValue,
        ExchangeFacts, LossCause, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
        PreparationDefect, RefusalEvidence, SseFraming, TerminalEvidence, TokenUsage, ToolCallId,
        ToolCallProposal, ToolName,
    };

    use super::{
        MAX_STREAMED_RESPONSE_BYTES, RedactingSink, build_http_request, process_streamed_chunk,
        redact_evidence, redact_json, redact_json_stream_fragment, redact_native_message,
        serialize_request, streamed_response_prefix_len, without_unproven_refusal,
    };
    use crate::stream::StreamDecoder;

    #[test]
    fn inv_035_split_streamed_credentials_are_redacted_before_observation() {
        let credential = CredentialValue::new(b"secret".to_vec());
        let mut observed = Vec::new();
        let mut sink = RedactingSink::new(&mut observed, &credential);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "sec".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "ret".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::UsageReported(TokenUsage::unreported()),
        });

        let text = observed
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "[redacted]");
    }

    #[test]
    fn inv_035_overlapping_credential_prefixes_stay_held_between_deltas() {
        let credential = CredentialValue::new(b"aaaa".to_vec());
        let mut observed = Vec::new();
        let mut sink = RedactingSink::new(&mut observed, &credential);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "aaaaa".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "aaab".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        let emitted = observed
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(!emitted.contains("aaaa"));
        assert!(emitted.contains("[redacted]"));
    }

    #[test]
    fn inv_035_complete_self_overlapping_credentials_are_redacted_before_suffix_retention() {
        let credential = CredentialValue::new(b"abcab".to_vec());
        let mut observed = Vec::new();
        let mut sink = RedactingSink::new(&mut observed, &credential);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "abcab".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "!".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        let emitted = observed
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(emitted, "[redacted]!");
    }

    #[test]
    fn inv_035_json_escaped_credentials_are_redacted_from_tool_arguments() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let redacted = redact_json(r#"{"token":"key_\u006coop"}"#.to_string(), &credential);

        assert_eq!(redacted, r#"{"token":"[redacted]"}"#);
        assert_eq!(
            redact_json(r#"{"city":"#.to_string(), &credential),
            r#"{"city":"#,
            "malformed non-secret bytes remain available for typed decoding"
        );
    }

    #[test]
    fn credential_reflected_as_a_json_number_is_redacted() {
        let credential = CredentialValue::new(b"23".to_vec());

        assert_eq!(
            redact_json(r#"{"value":1234}"#.to_string(), &credential),
            r#"{"value":"[redacted]"}"#
        );
    }

    #[test]
    fn credential_reflected_as_a_json_boolean_is_redacted() {
        let credential = CredentialValue::new(b"true".to_vec());

        assert_eq!(
            redact_json(r#"{"value":true}"#.to_string(), &credential),
            r#"{"value":"[redacted]"}"#
        );
    }

    #[test]
    fn credential_reflected_as_json_null_is_redacted() {
        let credential = CredentialValue::new(b"null".to_vec());

        assert_eq!(
            redact_json(r#"{"value":null}"#.to_string(), &credential),
            r#"{"value":"[redacted]"}"#
        );
    }

    #[test]
    fn json_redaction_preserves_untouched_raw_lexemes_and_duplicate_keys() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let raw = r#"{"token":"key_loop","id":184467440737095516160,"dup":1,"dup":2}"#;

        assert_eq!(
            redact_json(raw.to_string(), &credential),
            r#"{"token":"[redacted]","id":184467440737095516160,"dup":1,"dup":2}"#
        );
    }

    #[test]
    fn unrecognized_completion_finish_is_credential_sanitized() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            finish: CompletionFinish::Unrecognized {
                provider_token: "echo-key_loop".to_string(),
            },
            content: Vec::new(),
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Completed(completion) = redact_evidence(evidence, &credential) else {
            panic!("completion remains completion evidence");
        };
        assert_eq!(
            completion.finish,
            CompletionFinish::Unrecognized {
                provider_token: "echo-[redacted]".to_string(),
            }
        );
    }

    #[test]
    fn truncated_native_body_redacts_a_credential_prefix_at_the_cut() {
        let credential = CredentialValue::new(b"key_loop".to_vec());

        assert_eq!(
            redact_native_message("safe key_ … [truncated]".to_string(), &credential),
            "safe [redacted] … [truncated]"
        );
    }

    #[test]
    fn json_escaped_credential_in_a_fallback_error_body_is_redacted() {
        let credential = CredentialValue::new(b"key_loop".to_vec());

        assert_eq!(
            redact_native_message(r#"{"message":"key_\u006coop"}"#.to_string(), &credential),
            r#"{"message":"[redacted]"}"#
        );
    }

    #[test]
    fn surrogate_pair_credential_in_a_malformed_fallback_body_is_redacted() {
        let credential = CredentialValue::new("key_🔑".as_bytes().to_vec());

        assert_eq!(
            redact_native_message(r#"gateway key_\ud83d\udd11"#.to_string(), &credential),
            r#""[redacted]""#
        );
    }

    #[test]
    fn inv_035_split_json_escaped_credentials_are_redacted_before_tool_deltas_leave() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let mut observed = Vec::new();
        let mut sink = RedactingSink::new(&mut observed, &credential);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"token":"key_\u00"#.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"6coop"}"#.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolCallProposed(ToolCallProposal {
                id: ToolCallId::new("call_1"),
                name: ToolName::new("lookup"),
                arguments_json: r#"{"token":"key_\u006coop"}"#.to_string(),
            }),
        });
        drop(sink);

        let fragments = observed
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::ToolArgumentsDelta { fragment, .. } => Some(fragment.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(fragments, vec![r#"{"token":""#, r#"[redacted]"}"#]);
        assert!(!format!("{observed:?}").contains("key_loop"));
    }

    #[test]
    fn parallel_tool_argument_deltas_preserve_provider_arrival_order() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let mut observed = Vec::new();
        let mut sink = RedactingSink::new(&mut observed, &credential);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 1,
                fragment: r#"{"later":1}"#.to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"earlier":0}"#.to_string(),
            },
        });
        drop(sink);

        let indexes = observed
            .iter()
            .filter_map(|observation| match observation.fact {
                ObservationFact::ToolArgumentsDelta { index, .. } => Some(index),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(indexes, vec![1, 0]);
    }

    #[test]
    fn inv_035_final_content_cannot_reconstruct_a_credential_across_parts() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            finish: CompletionFinish::EndTurn,
            content: vec![
                AssistantPart::Text("safe key_".to_string()),
                AssistantPart::Text("loop tail".to_string()),
            ],
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Completed(completion) = redact_evidence(evidence, &credential) else {
            panic!("completion remains completion");
        };
        let joined = completion
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert_eq!(joined, "safe [redacted]loop tail");
        assert!(!joined.contains("key_loop"));
    }

    #[test]
    fn inv_035_malformed_tool_arguments_cannot_reconstruct_a_credential_across_parts() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let evidence = TerminalEvidence::Completed(CompletionEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            finish: CompletionFinish::ToolUse,
            content: vec![
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_1"),
                    name: ToolName::new("lookup"),
                    arguments_json: r#"safe key_\u006c"#.to_string(),
                }),
                AssistantPart::ToolCall(ToolCallProposal {
                    id: ToolCallId::new("call_2"),
                    name: ToolName::new("lookup"),
                    arguments_json: "oop".to_string(),
                }),
            ],
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::Completed(completion) = redact_evidence(evidence, &credential) else {
            panic!("completion remains completion");
        };
        let joined = completion
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall(proposal) => Some(proposal.arguments_json.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert_eq!(joined, "safe [redacted]oop");
        assert!(!joined.contains("key_loop"));
    }

    #[test]
    fn streamed_argument_redaction_handles_many_matches_in_one_forward_pass() {
        let raw = r#"\u006b\u0065\u0079"#.repeat(512);

        let (emitted, pending) = redact_json_stream_fragment(raw, "key");

        assert_eq!(emitted, "[redacted]".repeat(512));
        assert!(pending.is_empty());
    }

    #[test]
    fn streamed_argument_redaction_retains_only_a_credential_sized_suffix() {
        let raw = format!("{}ke", "safe".repeat(1024 * 1024));

        let (emitted, pending) = redact_json_stream_fragment(raw, "key");

        assert_eq!(emitted.len(), 4 * 1024 * 1024);
        assert_eq!(pending, "ke");
    }

    #[test]
    fn refusal_without_full_upload_proof_is_known_failure_evidence() {
        let refusal = TerminalEvidence::Refused(RefusalEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            content: Vec::new(),
            usage: TokenUsage {
                input_tokens: Some(11),
                output_tokens: Some(2),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(3),
            },
        });

        let TerminalEvidence::ProviderError(error) = without_unproven_refusal(refusal) else {
            panic!("unproven refusal must use the non-refusal known-failure mapping");
        };
        assert_eq!(error.native, NativeErrorFacts::default());
        assert_eq!(error.usage.input_tokens, Some(11));
        assert_eq!(error.usage.output_tokens, Some(2));
        assert_eq!(error.usage.cache_read_input_tokens, Some(3));
    }

    struct SerializationFails;

    impl Serialize for SerializationFails {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("fixture serialization failure"))
        }
    }

    #[test]
    fn serialization_failure_is_a_preparation_defect() {
        assert!(matches!(
            serialize_request(&SerializationFails),
            Err(PreparationDefect::SerializationFailed { .. })
        ));
    }

    #[test]
    fn request_build_failure_is_a_preparation_defect() {
        let builder = reqwest::Client::new()
            .get("http://127.0.0.1/")
            .header("invalid\nheader", "value");

        assert!(matches!(
            build_http_request(builder),
            Err(PreparationDefect::RequestConstructionFailed { .. })
        ));
    }

    #[test]
    fn streamed_response_budget_rejects_aggregate_overflow() {
        assert_eq!(
            streamed_response_prefix_len(MAX_STREAMED_RESPONSE_BYTES - 1, 1),
            (1, false)
        );
        assert_eq!(
            streamed_response_prefix_len(MAX_STREAMED_RESPONSE_BYTES - 1, 2),
            (1, true)
        );
        assert_eq!(
            streamed_response_prefix_len(MAX_STREAMED_RESPONSE_BYTES, usize::MAX),
            (0, true)
        );
    }

    #[test]
    fn terminal_record_in_budget_wins_over_coalesced_trailing_bytes() {
        let mut bytes = b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
            \"model\":\"model-exact-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\
            \"finish_reason\":\"stop\"}]}\n\n\
            data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\"choices\":[],\
            \"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
            data: [DONE]\n\n"
            .to_vec();
        let terminal_len = bytes.len();
        bytes.extend_from_slice(b"coalesced trailing bytes");
        let mut streamed_bytes = MAX_STREAMED_RESPONSE_BYTES - terminal_len;
        let mut framing = SseFraming::new(1024);
        let mut decoder = StreamDecoder::new(ExchangeFacts::default(), false);
        let mut observations = Vec::new();
        let mut cancellation = CancellationSignal::never();

        let evidence = process_streamed_chunk(
            &bytes,
            &mut streamed_bytes,
            &mut framing,
            &mut decoder,
            &"call-1".to_string(),
            &mut observations,
            &mut cancellation,
        );

        assert!(matches!(evidence, Some(TerminalEvidence::Completed(_))));
    }

    struct CancelOnModel {
        observations: Vec<Observation<String>>,
        sender: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl ObservationSink<String> for CancelOnModel {
        fn observe(&mut self, observation: Observation<String>) {
            if matches!(observation.fact, ObservationFact::ProviderModelReported(_))
                && let Some(sender) = self.sender.take()
            {
                let _ = sender.send(());
            }
            self.observations.push(observation);
        }
    }

    #[test]
    fn cancellation_is_rechecked_between_coalesced_sse_records() {
        let bytes = b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
            \"model\":\"model-exact-1\",\"choices\":[{\"index\":0,\
            \"delta\":{\"role\":\"assistant\"}}]}\n\n\
            data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
            \"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"}}]}\n\n";
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut cancellation = CancellationSignal::when(async move {
            let _ = receiver.await;
        });
        let mut streamed_bytes = 0;
        let mut framing = SseFraming::new(1024);
        let mut decoder = StreamDecoder::new(ExchangeFacts::default(), false);
        let mut sink = CancelOnModel {
            observations: Vec::new(),
            sender: Some(sender),
        };

        let evidence = process_streamed_chunk(
            bytes,
            &mut streamed_bytes,
            &mut framing,
            &mut decoder,
            &"call-1".to_string(),
            &mut sink,
            &mut cancellation,
        );

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("cancellation after the first record must pause the coalesced chunk");
        };
        assert_eq!(loss.cause, LossCause::CancellationRequested);
        assert!(
            !sink
                .observations
                .iter()
                .any(|observation| matches!(observation.fact, ObservationFact::TextDelta { .. }))
        );
    }
}
