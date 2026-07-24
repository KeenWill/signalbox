//! The adapter runtime: one operation, at most one HTTP interaction.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Waker};

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};

use signalbox_model_runtime::{
    AssistantPart, BoundaryLossEvidence, CancellationSignal, CompletionFinish, DeliveryMode,
    ExchangeFacts, FinishReason, LossCause, ModelOperation, ModelRuntime, NativeErrorFacts,
    Observation, ObservationFact, ObservationSink, PreparationDefect, PreparationFailure,
    PreparationOutcome, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind,
    ProviderMessageId, ProviderReportedModel, ProviderRequestId, SseFraming, StreamInterruption,
    TerminalEvidence, TerminalReport, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
    TransportFacts, UnsentCause, validate_provider_json_nesting,
};

use signalbox_model_runtime::{CredentialAccess, CredentialValue};

use crate::config::AnthropicConfig;
use crate::response::decode_buffered_response;
use crate::status::{classify_error, classify_error_status};
use crate::stream::{StreamDecoder, StreamStep};
use crate::translate::build_request;
use crate::wire::ErrorEnvelope;

/// A response body is provider-controlled input. Keep complete buffered
/// responses bounded independently of the requested output-token ceiling.
const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_STREAMED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The Anthropic Messages adapter.
///
/// Implements [`ModelRuntime`]: executes exactly one authorized operation as
/// at most one `POST /v1/messages` request and reports typed evidence. It
/// holds no state between operations, retries nothing, and never issues a
/// second request for one operation.
pub struct AnthropicRuntime<A> {
    client: Client,
    messages_url: Url,
    credentials: A,
    version_header: HeaderValue,
    sse_record_limit: usize,
}

/// An opaque, one-shot Anthropic request capability prepared per
/// `docs/spec/runtime-substrate.md`.
///
/// The private fields bind the complete authenticated request, caller
/// correlation, delivery mode, originating client and stream settings,
/// declared stop sequences, and exact credential value needed to sanitize
/// provider-controlled evidence. The type deliberately implements neither
/// `Clone`, serialization, nor diagnostic formatting.
#[must_use]
pub struct AnthropicPreparedRequest<C> {
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
    stop_sequences: Vec<String>,
}

impl<A> std::fmt::Debug for AnthropicRuntime<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicRuntime")
            .field("client", &self.client)
            .field("messages_url", &self.messages_url)
            .field("credentials", &"[redacted]")
            .field("version_header", &"[sensitive]")
            .field("sse_record_limit", &self.sse_record_limit)
            .finish()
    }
}

/// Why an [`AnthropicRuntime`] could not be constructed.
///
/// Construction failure is a configuration defect, not operation evidence:
/// no operation exists yet, so nothing is reported as unsent.
#[derive(Debug)]
pub enum AnthropicConstructionError {
    /// The configured base URL is not an acceptable absolute HTTP(S) URL.
    InvalidBaseUrl {
        /// The parser's rendered description.
        detail: String,
    },
    /// The configured `anthropic-version` cannot form an HTTP header value.
    InvalidVersion,
    /// The configured whole-exchange timeout is zero.
    InvalidExchangeTimeout,
    /// The configured SSE record limit cannot admit any record bytes.
    InvalidSseRecordLimit,
    /// The HTTP client could not be constructed.
    ClientConstruction {
        /// The client's rendered description.
        detail: String,
    },
}

impl std::fmt::Display for AnthropicConstructionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBaseUrl { detail } => write!(f, "invalid base URL: {detail}"),
            Self::InvalidVersion => {
                f.write_str("anthropic-version cannot form an HTTP header value")
            }
            Self::InvalidExchangeTimeout => {
                f.write_str("exchange timeout must be greater than zero")
            }
            Self::InvalidSseRecordLimit => {
                f.write_str("SSE record limit must be greater than zero")
            }
            Self::ClientConstruction { detail } => {
                write!(f, "HTTP client construction failed: {detail}")
            }
        }
    }
}

impl std::error::Error for AnthropicConstructionError {}

impl<A: CredentialAccess> AnthropicRuntime<A> {
    /// Builds the adapter and its HTTP client.
    ///
    /// # Transport discipline: one send is one physical request
    ///
    /// Per `docs/spec/runtime-substrate.md`, the client is configured so
    /// that a single send is provably a single request:
    ///
    /// - **TLS uses rustls with the platform verifier and a TLS 1.2 floor.**
    ///   Certificate and hostname verification remain enabled.
    /// - **Ambient proxy discovery is disabled** (`no_proxy()`), so provider
    ///   credentials cannot traverse an environment-selected intermediary.
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
    /// The caller may leave the separate connect timeout unset, but every
    /// exchange has a positive whole-exchange timeout that covers connection
    /// establishment, response headers, and buffered or streamed body
    /// delivery.
    pub fn new(
        config: AnthropicConfig,
        credentials: A,
    ) -> Result<Self, AnthropicConstructionError> {
        if config.sse_record_limit == 0 {
            return Err(AnthropicConstructionError::InvalidSseRecordLimit);
        }
        if config.exchange_timeout.is_zero() {
            return Err(AnthropicConstructionError::InvalidExchangeTimeout);
        }
        // Parse and validate the caller's base independently. Appending first
        // can turn an authority-less value such as `https://` into the
        // apparently valid but unintended authority `https://v1/...`.
        let base_url = Url::parse(&config.base_url).map_err(|error| {
            AnthropicConstructionError::InvalidBaseUrl {
                detail: error.to_string(),
            }
        })?;
        if base_url.query().is_some() || base_url.fragment().is_some() {
            // Concatenating the endpoint path onto a base with a query or
            // fragment would route the request somewhere else entirely.
            return Err(AnthropicConstructionError::InvalidBaseUrl {
                detail: "base URL must not carry a query or fragment".to_string(),
            });
        }
        if !base_url.username().is_empty() || base_url.password().is_some() {
            return Err(AnthropicConstructionError::InvalidBaseUrl {
                detail: "base URL must not carry user information".to_string(),
            });
        }
        if !matches!(base_url.scheme(), "http" | "https") {
            // A non-HTTP scheme would fail only inside send(), after
            // SendCommenced, and read as ambiguous transport loss; it is an
            // invalid configuration, caught here.
            return Err(AnthropicConstructionError::InvalidBaseUrl {
                detail: format!("unsupported scheme {:?}", base_url.scheme()),
            });
        }
        if base_url.scheme() == "http"
            && !base_url
                .host_str()
                .and_then(|host| {
                    host.trim_matches(&['[', ']'][..])
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .is_some_and(|address| address.is_loopback())
        {
            return Err(AnthropicConstructionError::InvalidBaseUrl {
                detail: "plain HTTP requires a literal loopback IP host".to_string(),
            });
        }
        if base_url.host_str().is_none() {
            return Err(AnthropicConstructionError::InvalidBaseUrl {
                detail: "base URL must carry an authority".to_string(),
            });
        }
        // Retain the adapter's established concatenation semantics: the
        // complete caller-supplied base path is kept and trailing slashes are
        // collapsed before the endpoint is appended.
        let messages_url = Url::parse(&format!(
            "{}/v1/messages",
            config.base_url.trim_end_matches('/')
        ))
        .map_err(|error| AnthropicConstructionError::InvalidBaseUrl {
            detail: error.to_string(),
        })?;
        let version_header = HeaderValue::from_str(&config.anthropic_version)
            .map_err(|_| AnthropicConstructionError::InvalidVersion)?;
        // The workspace graph selects only ring; installation may already
        // have occurred through SQLx in the composed process.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut builder = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .timeout(config.exchange_timeout);
        if let Some(timeout) = config.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        let client =
            builder
                .build()
                .map_err(|error| AnthropicConstructionError::ClientConstruction {
                    detail: error.to_string(),
                })?;
        Ok(Self {
            client,
            messages_url,
            credentials,
            version_header,
            sse_record_limit: config.sse_record_limit,
        })
    }

    async fn prepare_request<C: Clone + Send + Sync>(
        &self,
        operation: ModelOperation<C>,
        cancellation: &mut CancellationSignal,
    ) -> PreparationOutcome<C, AnthropicPreparedRequest<C>> {
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
        let Some(api_key_header) = sensitive_header(&api_key) else {
            return PreparationOutcome::Failed {
                correlation,
                failure: PreparationFailure::CredentialUnusable {
                    detail: "credential value cannot form an HTTP header value".to_string(),
                },
            };
        };
        let delivery = operation.delivery;
        let stop_sequences = operation.settings.stop_sequences.clone();
        let request = match build_http_request(
            self.client
                .post(self.messages_url.clone())
                .header("x-api-key", api_key_header)
                .header("anthropic-version", self.version_header.clone())
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
        PreparationOutcome::Prepared(AnthropicPreparedRequest {
            transport: PreparedTransport {
                request,
                client: self.client.clone(),
                settings: ExecutionSettings {
                    delivery,
                    sse_record_limit: self.sse_record_limit,
                    stop_sequences,
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
        // The Messages success contract is specifically HTTP 200; another
        // 2xx is not recognized terminal-success evidence.
        if status.as_u16() == 200 {
            match settings.delivery {
                DeliveryMode::Buffered => {
                    self.finish_buffered(
                        response,
                        exchange,
                        &settings,
                        correlation,
                        sink,
                        cancellation,
                    )
                    .await
                }
                DeliveryMode::Streamed => {
                    self.finish_streamed(
                        response,
                        exchange,
                        &settings,
                        correlation,
                        sink,
                        cancellation,
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
        settings: &ExecutionSettings,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
    ) -> TerminalEvidence {
        let body = match collect_response_body(response, cancellation).await {
            None => return exchange_loss(LossCause::CancellationRequested, exchange),
            Some(Err(cause)) => return exchange_loss(cause, exchange),
            Some(Ok(bytes)) => bytes,
        };
        decode_buffered_response(&body, exchange, &settings.stop_sequences, correlation, sink)
    }

    async fn finish_streamed<C: Clone + Send + Sync>(
        &self,
        response: reqwest::Response,
        exchange: ExchangeFacts,
        settings: &ExecutionSettings,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
    ) -> TerminalEvidence {
        let mut framing = SseFraming::new(settings.sse_record_limit);
        let mut decoder =
            StreamDecoder::with_stop_sequences(exchange, settings.stop_sequences.clone());
        let mut body = response.bytes_stream();
        let mut streamed_bytes = 0usize;
        loop {
            let chunk = match with_cancellation(cancellation, body.next()).await {
                None => return decoder.cancelled(),
                Some(chunk) => chunk,
            };
            match chunk {
                // End of transport without `message_stop`: the explicit
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
                    let interruption = if error.is_timeout() {
                        StreamInterruption::TimedOut(transport_facts(&error))
                    } else {
                        StreamInterruption::TransportFailure(transport_facts(&error))
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
    // Records completed before a framing failure are applied first, so
    // evidence they carry is never lost to how the transport batched bytes.
    // The same rule applies at the aggregate byte budget: process the
    // in-budget prefix so a terminal marker in it wins over coalesced trailing
    // data.
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

impl<C: Clone + Send + Sync, A: CredentialAccess> ModelRuntime<C> for AnthropicRuntime<A> {
    type Prepared = AnthropicPreparedRequest<C>;

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
        let AnthropicPreparedRequest {
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
        let mut redacting_sink = RedactingObservationSink::new(sink, &credential);
        let evidence = self
            .exchange(
                transport,
                &correlation,
                &mut redacting_sink,
                &mut cancellation,
            )
            .await;
        redacting_sink.flush();
        // A fully buffered reqwest request does not expose independent proof
        // that an early response arrived only after the complete upload.
        // `docs/spec/model-call-execution.md` therefore forbids classifying
        // its refusal token as `Refused`.
        let evidence = without_unproven_refusal(evidence);
        // Per the runtime-substrate spec, provider-controlled text in the
        // evidence (error messages, raw bodies, transport detail) is
        // credential-sanitized before it leaves the adapter boundary, using
        // the exact preparation-time value.
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
                    error_token: Some("refusal".to_string()),
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
    if validate_provider_json_nesting(&body).is_ok()
        && let Ok(ErrorEnvelope {
            envelope_type,
            error: Some(error),
        }) = serde_json::from_slice(&body)
        && envelope_type == "error"
    {
        let kind = classify_error(status, error.error_type.as_deref());
        return TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange,
            // The Messages error envelope reports no model identity.
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
        kind: classify_error_status(status),
        native: NativeErrorFacts {
            error_token: None,
            error_code: None,
            // Preserve the complete bounded body until the execution
            // boundary can sanitize JSON escapes with the exact prepared
            // credential. Truncating first could make valid JSON
            // unparseable and hide a reversible credential representation
            // from JSON-aware redaction.
            message: Some(String::from_utf8_lossy(&body).into_owned()),
        },
        usage: TokenUsage::unreported(),
    })
}

/// Reads a non-streaming provider body without allowing it to grow without
/// bound. `None` retains the caller-cancellation race used by both success
/// and error paths.
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

/// Classifies a send-phase transport failure per the full-request-send
/// rule in `docs/spec/model-call-execution.md`.
///
/// Every request uses a fresh connection (see [`AnthropicRuntime::new`]), so
/// a connect failure provably precedes any request byte and classifies as
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
        .get("request-id")
        .or_else(|| headers.get("x-request-id"))
        .and_then(|value| value.to_str().ok())
        .map(ProviderRequestId::new)
}

fn truncated(text: String) -> String {
    const LIMIT: usize = 2048;
    if text.len() <= LIMIT {
        return text;
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
/// definitive-response precedence in `docs/spec/model-call-execution.md`).
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

/// The credential as a sensitivity-marked header value, or `None` when its
/// bytes cannot form one. The value never appears in errors or logs.
fn sensitive_header(api_key: &CredentialValue) -> Option<HeaderValue> {
    if api_key.expose_bytes().is_empty() || std::str::from_utf8(api_key.expose_bytes()).is_err() {
        return None;
    }
    let mut header = HeaderValue::from_bytes(api_key.expose_bytes()).ok()?;
    header.set_sensitive(true);
    Some(header)
}

struct RedactingObservationSink<'a, C> {
    inner: &'a mut (dyn ObservationSink<C> + Send),
    credential: &'a str,
    pending_stream_text: Option<PendingStreamText<C>>,
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

impl<'a, C: Clone> RedactingObservationSink<'a, C> {
    fn new(
        inner: &'a mut (dyn ObservationSink<C> + Send),
        credential: &'a CredentialValue,
    ) -> Self {
        Self {
            inner,
            credential: std::str::from_utf8(credential.expose_bytes()).unwrap_or_default(),
            pending_stream_text: None,
        }
    }

    fn flush(&mut self) {
        if let Some(pending) = self.pending_stream_text.take() {
            self.emit_stream_text(
                pending.field,
                pending.index,
                pending.correlation,
                // The pending bytes are exactly a credential prefix. Once a
                // non-delta fact must cross the boundary, retaining stream
                // ordering and retaining those bytes are incompatible; fail
                // closed by replacing the possible secret prefix.
                "[redacted]".to_string(),
            );
        }
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

    fn redact_stream_delta(
        &mut self,
        field: StreamField,
        index: u32,
        correlation: C,
        text: String,
    ) {
        if self
            .pending_stream_text
            .as_ref()
            .is_some_and(|pending| pending.field != field || pending.index != index)
        {
            self.flush();
        }
        let mut combined = self
            .pending_stream_text
            .take()
            .map_or_else(String::new, |pending| pending.text);
        combined.push_str(&text);
        let (emitted, pending) =
            redact_complete_credentials_and_hold_prefix(combined, self.credential);
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
}

impl<C: Clone> ObservationSink<C> for RedactingObservationSink<'_, C> {
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
                self.flush();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: ObservationFact::ToolArgumentsDelta {
                        index,
                        fragment: redact_json(fragment, self.credential),
                    },
                });
            }
            ObservationFact::ToolCallProposed(proposal) => {
                self.flush();
                self.inner.observe(Observation {
                    correlation: observation.correlation,
                    fact: ObservationFact::ToolCallProposed(redact_proposal(
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

fn redact_text(text: String, credential: &str) -> String {
    if credential.is_empty() {
        text
    } else {
        text.replace(credential, "[redacted]")
    }
}

/// Redacts complete credentials and fails closed on a trailing credential
/// prefix. Final content parts are independently persisted values, so a prefix
/// may not leave one part and be completed by the next.
fn redact_bounded_text(text: String, credential: &str) -> String {
    let (mut redacted, pending) = redact_complete_credentials_and_hold_prefix(text, credential);
    if !pending.is_empty() {
        redacted.push_str("[redacted]");
    }
    redacted
}

fn redact_native_message(text: String, credential: &str) -> String {
    const TRUNCATION_SUFFIX: &str = " … [truncated]";
    if let Some(body) = text.strip_suffix(TRUNCATION_SUFFIX) {
        let mut redacted = redact_native_body(body.to_string(), credential);
        redacted.push_str(TRUNCATION_SUFFIX);
        redacted
    } else {
        truncated(redact_native_body(text, credential))
    }
}

fn redact_native_body(text: String, credential: &str) -> String {
    if serde_json::value::RawValue::from_string(text.clone()).is_ok() {
        redact_json(text, credential)
    } else {
        redact_bounded_text(text, credential)
    }
}

/// Redacts a complete credential and any trailing prefix of it. Provider
/// chunk boundaries are arbitrary: removing a credential prefix at the end
/// of every emitted delta prevents a later delta from completing the secret
/// without delaying synchronous observation delivery.
fn redact_complete_credentials_and_hold_prefix(
    mut text: String,
    credential: &str,
) -> (String, String) {
    if credential.is_empty() {
        return (text, String::new());
    }
    // Search the complete suffix, including bytes that overlap the last full
    // credential match. Otherwise a self-overlapping credential can leave an
    // emitted suffix that a later provider chunk completes.
    let longest_prefix = (1..credential.len())
        .rev()
        .filter(|length| credential.is_char_boundary(*length))
        .find(|length| text.ends_with(&credential[..*length]));
    let split = longest_prefix.map_or(text.len(), |length| text.len() - length);
    let pending = text.split_off(split);
    (text.replace(credential, "[redacted]"), pending)
}

fn redact_exchange(mut exchange: ExchangeFacts, credential: &str) -> ExchangeFacts {
    exchange.provider_request_id = exchange.provider_request_id.map(|request_id| {
        ProviderRequestId::new(redact_text(request_id.as_str().to_string(), credential))
    });
    exchange
}

fn redact_reported_model(model: ProviderReportedModel, credential: &str) -> ProviderReportedModel {
    ProviderReportedModel::new(redact_text(model.as_str().to_string(), credential))
}

fn redact_finish(finish: FinishReason, credential: &str) -> FinishReason {
    match finish {
        FinishReason::StopSequence { sequence } => FinishReason::StopSequence {
            sequence: sequence.map(|value| redact_text(value, credential)),
        },
        FinishReason::Unrecognized { provider_token } => FinishReason::Unrecognized {
            provider_token: redact_text(provider_token, credential),
        },
        finish => finish,
    }
}

fn redact_completion_finish(finish: CompletionFinish, credential: &str) -> CompletionFinish {
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

fn redact_proposal(mut proposal: ToolCallProposal, credential: &str) -> ToolCallProposal {
    proposal.id = ToolCallId::new(redact_text(proposal.id.as_str().to_string(), credential));
    proposal.name = ToolName::new(redact_text(proposal.name.as_str().to_string(), credential));
    proposal.arguments_json = redact_json(proposal.arguments_json, credential);
    proposal
}

fn redact_json(raw: String, credential: &str) -> String {
    if credential.is_empty() {
        return raw;
    }
    if serde_json::value::RawValue::from_string(raw.clone()).is_err() {
        return redact_text(raw, credential);
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
            if decoded.contains(credential) {
                let Ok(sanitized) =
                    serde_json::to_string(&decoded.replace(credential, "[redacted]"))
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
        if token.contains(credential) {
            redacted.push_str("\"[redacted]\"");
        } else {
            redacted.push_str(token);
        }
    }
    redacted
}

fn redact_part(part: AssistantPart, credential: &str) -> AssistantPart {
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
            AssistantPart::ToolCall(redact_proposal(proposal, credential))
        }
    }
}

fn redact_observation_fact(fact: ObservationFact, credential: &str) -> ObservationFact {
    match fact {
        ObservationFact::ExchangeEstablished(exchange) => {
            ObservationFact::ExchangeEstablished(redact_exchange(exchange, credential))
        }
        ObservationFact::ProviderModelReported(model) => {
            ObservationFact::ProviderModelReported(redact_reported_model(model, credential))
        }
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
                fragment: redact_json(fragment, credential),
            }
        }
        ObservationFact::ToolCallProposed(proposal) => {
            ObservationFact::ToolCallProposed(redact_proposal(proposal, credential))
        }
        ObservationFact::FinishReported(finish) => {
            ObservationFact::FinishReported(redact_finish(finish, credential))
        }
        fact @ (ObservationFact::SendCommenced | ObservationFact::UsageReported(_)) => fact,
    }
}

/// Credential-sanitizes every provider-controlled or transport-rendered
/// text in the evidence, per the runtime-substrate spec: a reflected key
/// value in an error message, raw body, or rendered detail is replaced
/// before the evidence leaves the adapter boundary. Typed facts are
/// untouched.
fn redact_evidence(evidence: TerminalEvidence, api_key: &CredentialValue) -> TerminalEvidence {
    let key_text = std::str::from_utf8(api_key.expose_bytes()).unwrap_or_default();
    let redact = move |text: String| -> String { redact_text(text, key_text) };
    let redact_native = |mut native: NativeErrorFacts| -> NativeErrorFacts {
        native.error_token = native.error_token.map(redact);
        native.error_code = native.error_code.map(redact);
        native.message = native
            .message
            .map(|message| redact_native_message(message, key_text));
        native
    };
    let redact_transport =
        |facts: TransportFacts| -> TransportFacts { TransportFacts::new(redact(facts.detail)) };
    match evidence {
        TerminalEvidence::Completed(mut completion) => {
            completion.exchange = redact_exchange(completion.exchange, key_text);
            completion.message_id = completion
                .message_id
                .map(|message_id| ProviderMessageId::new(redact(message_id.as_str().to_string())));
            completion.reported_model = completion
                .reported_model
                .map(|model| redact_reported_model(model, key_text));
            completion.finish = redact_completion_finish(completion.finish, key_text);
            completion.content = completion
                .content
                .into_iter()
                .map(|part| redact_part(part, key_text))
                .collect();
            TerminalEvidence::Completed(completion)
        }
        TerminalEvidence::Refused(mut refusal) => {
            refusal.exchange = redact_exchange(refusal.exchange, key_text);
            refusal.message_id = refusal
                .message_id
                .map(|message_id| ProviderMessageId::new(redact(message_id.as_str().to_string())));
            refusal.reported_model = refusal
                .reported_model
                .map(|model| redact_reported_model(model, key_text));
            refusal.content = refusal
                .content
                .into_iter()
                .map(|part| redact_part(part, key_text))
                .collect();
            TerminalEvidence::Refused(refusal)
        }
        TerminalEvidence::ProviderError(mut error) => {
            error.exchange = redact_exchange(error.exchange, key_text);
            error.reported_model = error
                .reported_model
                .map(|model| redact_reported_model(model, key_text));
            error.native = redact_native(error.native);
            TerminalEvidence::ProviderError(error)
        }
        TerminalEvidence::CancellationConfirmed(mut confirmed) => {
            confirmed.exchange = redact_exchange(confirmed.exchange, key_text);
            confirmed.reported_model = confirmed
                .reported_model
                .map(|model| redact_reported_model(model, key_text));
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
            loss.exchange = redact_exchange(loss.exchange, key_text);
            loss.reported_model = loss
                .reported_model
                .map(|model| redact_reported_model(model, key_text));
            loss.finish_reported = loss
                .finish_reported
                .map(|finish| redact_finish(finish, key_text));
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
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use signalbox_model_runtime::{
        AssistantPart, CancellationSignal, CompletionEvidence, CompletionFinish, CredentialValue,
        ExchangeFacts, LossCause, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
        PreparationDefect, ProviderErrorEvidence, ProviderErrorKind, RefusalEvidence, SseFraming,
        TerminalEvidence, TokenUsage,
    };

    use super::{
        MAX_STREAMED_RESPONSE_BYTES, RedactingObservationSink, build_http_request,
        process_streamed_chunk, redact_evidence, redact_json, redact_native_message,
        serialize_request, streamed_response_prefix_len, without_unproven_refusal,
    };
    use crate::stream::StreamDecoder;

    #[test]
    fn inv_035_json_escaped_credentials_are_redacted_from_tool_arguments() {
        let redacted = redact_json(r#"{"token":"key_\u006coop"}"#.to_string(), "key_loop");

        assert_eq!(redacted, r#"{"token":"[redacted]"}"#);
    }

    #[test]
    fn credential_reflected_as_a_json_primitive_is_redacted() {
        assert_eq!(
            redact_json(r#"{"value":1234}"#.to_string(), "23"),
            r#"{"value":"[redacted]"}"#
        );
        assert_eq!(
            redact_json(r#"{"value":true}"#.to_string(), "true"),
            r#"{"value":"[redacted]"}"#
        );
        assert_eq!(
            redact_json(r#"{"value":null}"#.to_string(), "null"),
            r#"{"value":"[redacted]"}"#
        );
    }

    #[test]
    fn json_redaction_preserves_untouched_raw_lexemes_and_duplicate_keys() {
        let raw = r#"{"token":"key_loop","id":184467440737095516160,"dup":1,"dup":2}"#;

        assert_eq!(
            redact_json(raw.to_string(), "key_loop"),
            r#"{"token":"[redacted]","id":184467440737095516160,"dup":1,"dup":2}"#
        );
    }

    #[test]
    fn truncated_native_body_redacts_a_credential_prefix_at_the_cut() {
        assert_eq!(
            redact_native_message("safe key_ … [truncated]".to_string(), "key_loop"),
            "safe [redacted] … [truncated]"
        );
    }

    #[test]
    fn json_escaped_credential_in_a_fallback_error_body_is_redacted() {
        assert_eq!(
            redact_native_message(r#"{"message":"key_\u006coop"}"#.to_string(), "key_loop"),
            r#"{"message":"[redacted]"}"#
        );
    }

    #[test]
    fn large_json_fallback_body_is_sanitized_before_truncation() {
        let body = format!(r#"{{"message":"key_\u006coop{}"}}"#, "x".repeat(2_200));

        let redacted = redact_native_message(body, "key_loop");

        assert!(redacted.starts_with(r#"{"message":"[redacted]"#));
        assert!(redacted.ends_with(" … [truncated]"));
        assert!(!redacted.contains(r"key_\u006coop"));
        assert!(!redacted.contains("key_loop"));
    }

    #[test]
    fn inv_035_native_error_code_is_credential_sanitized() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let evidence = TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange: ExchangeFacts::default(),
            reported_model: None,
            kind: ProviderErrorKind::Unrecognized,
            native: NativeErrorFacts {
                error_token: None,
                error_code: Some("echo-key_loop".to_string()),
                message: None,
            },
            usage: TokenUsage::unreported(),
        });

        let TerminalEvidence::ProviderError(error) = redact_evidence(evidence, &credential) else {
            panic!("provider error remains provider error");
        };
        assert_eq!(error.native.error_code.as_deref(), Some("echo-[redacted]"));
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
    fn refusal_without_full_upload_proof_is_known_failure_evidence() {
        let refusal = TerminalEvidence::Refused(RefusalEvidence {
            exchange: ExchangeFacts::default(),
            message_id: None,
            reported_model: None,
            content: Vec::new(),
            usage: TokenUsage {
                input_tokens: Some(13),
                output_tokens: Some(5),
                cache_creation_input_tokens: Some(2),
                cache_read_input_tokens: Some(3),
            },
        });

        let TerminalEvidence::ProviderError(error) = without_unproven_refusal(refusal) else {
            panic!("unproven refusal must use the non-refusal known-failure mapping");
        };
        assert_eq!(error.native.error_token.as_deref(), Some("refusal"));
        assert_eq!(error.usage.input_tokens, Some(13));
        assert_eq!(error.usage.output_tokens, Some(5));
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
        let _ = rustls::crypto::ring::default_provider().install_default();
        let builder = reqwest::Client::new()
            .get("http://127.0.0.1/")
            .header("invalid\nheader", "value");

        assert!(matches!(
            build_http_request(builder),
            Err(PreparationDefect::RequestConstructionFailed { .. })
        ));
    }

    #[test]
    fn buffered_prefix_is_redacted_before_metadata_and_cannot_join_a_later_tail() {
        let mut observed = Vec::new();
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let mut redacting = RedactingObservationSink::new(&mut observed, &credential);
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "safe key_".to_string(),
            },
        });
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::UsageReported(TokenUsage::unreported()),
        });
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "loop".to_string(),
            },
        });
        redacting.flush();
        drop(redacting);

        let metadata_index = observed
            .iter()
            .position(|observation| matches!(observation.fact, ObservationFact::UsageReported(_)))
            .expect("usage is forwarded");
        let text_before_metadata = observed[..metadata_index]
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let all_text = observed
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text_before_metadata, "safe [redacted]");
        assert_eq!(all_text, "safe [redacted]loop");
        assert!(!all_text.contains("key_loop"));
    }

    #[test]
    fn inv_035_overlapping_credential_prefixes_stay_held_between_deltas() {
        let mut observed = Vec::new();
        let credential = CredentialValue::new(b"aaaa".to_vec());
        let mut redacting = RedactingObservationSink::new(&mut observed, &credential);
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "aaaaa".to_string(),
            },
        });
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "aaab".to_string(),
            },
        });
        redacting.flush();
        drop(redacting);

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
    fn json_escaped_credential_is_redacted_before_a_tool_delta_is_forwarded() {
        let mut observed = Vec::new();
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let mut redacting = RedactingObservationSink::new(&mut observed, &credential);
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"token":"key_\u006coop"}"#.to_string(),
            },
        });
        drop(redacting);

        assert!(observed.iter().any(|observation| matches!(
            &observation.fact,
            ObservationFact::ToolArgumentsDelta { fragment, .. }
                if fragment == r#"{"token":"[redacted]"}"#
        )));
    }

    #[test]
    fn pending_text_is_flushed_before_a_tool_delta_is_forwarded() {
        let mut observed = Vec::new();
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let mut redacting = RedactingObservationSink::new(&mut observed, &credential);
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "safe key_".to_string(),
            },
        });
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ToolArgumentsDelta {
                index: 1,
                fragment: "{}".to_string(),
            },
        });
        drop(redacting);

        let text_before_tool = observed[..observed.len() - 1]
            .iter()
            .filter_map(|observation| match &observation.fact {
                ObservationFact::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text_before_tool, "safe [redacted]");
        assert!(matches!(
            observed.last().expect("tool delta is forwarded").fact,
            ObservationFact::ToolArgumentsDelta { .. }
        ));
    }

    #[test]
    fn pending_stream_text_is_flushed_before_a_different_field() {
        let mut observed = Vec::new();
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let mut redacting = RedactingObservationSink::new(&mut observed, &credential);
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::ThinkingDelta {
                index: 0,
                text: "k".to_string(),
            },
        });
        redacting.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 1,
                text: "later".to_string(),
            },
        });
        redacting.flush();
        drop(redacting);

        assert!(matches!(
            &observed[0].fact,
            ObservationFact::ThinkingDelta { text, .. } if text == "[redacted]"
        ));
        assert!(matches!(
            &observed[1].fact,
            ObservationFact::TextDelta { text, .. } if text == "later"
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
    fn streamed_response_overflow_is_typed_protocol_loss() {
        let mut streamed_bytes = MAX_STREAMED_RESPONSE_BYTES;
        let mut framing = SseFraming::new(1024);
        let mut decoder = StreamDecoder::with_stop_sequences(ExchangeFacts::default(), Vec::new());
        let mut observations = Vec::new();
        let mut cancellation = CancellationSignal::never();

        let evidence = process_streamed_chunk(
            b"x",
            &mut streamed_bytes,
            &mut framing,
            &mut decoder,
            &"call-1".to_string(),
            &mut observations,
            &mut cancellation,
        );

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("an oversized streamed response must fail closed as boundary loss");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
    }

    #[test]
    fn terminal_record_in_budget_wins_over_coalesced_trailing_bytes() {
        let mut bytes = b"event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
            \"role\":\"assistant\",\"id\":\"msg_1\",\"model\":\"model-exact-1\",\
            \"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
            event: message_delta\n\
            data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\
            \"usage\":{\"output_tokens\":1}}\n\n\
            event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            .to_vec();
        let terminal_len = bytes.len();
        bytes.extend_from_slice(b"coalesced trailing bytes");
        let mut streamed_bytes = MAX_STREAMED_RESPONSE_BYTES - terminal_len;
        let mut framing = SseFraming::new(1024);
        let mut decoder = StreamDecoder::with_stop_sequences(ExchangeFacts::default(), Vec::new());
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
        let bytes = b"event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
            \"role\":\"assistant\",\"id\":\"msg_1\",\"model\":\"model-exact-1\",\
            \"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
            event: content_block_start\n\
            data: {\"type\":\"content_block_start\",\"index\":0,\
            \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut cancellation = CancellationSignal::when(async move {
            let _ = receiver.await;
        });
        let mut streamed_bytes = 0;
        let mut framing = SseFraming::new(1024);
        let mut decoder = StreamDecoder::with_stop_sequences(ExchangeFacts::default(), Vec::new());
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
