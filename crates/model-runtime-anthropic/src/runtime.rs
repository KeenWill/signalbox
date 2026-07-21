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
    BoundaryLossEvidence, CancellationSignal, ExchangeFacts, LossCause, ModelOperation,
    ModelRuntime, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
    PreparationFailure, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind,
    ProviderRequestId, SseFraming, StreamInterruption, TerminalEvidence, TerminalReport,
    TokenUsage, TransportFacts, UnsentCause,
};

use signalbox_model_runtime::{CredentialAccess, CredentialValue};

use crate::config::AnthropicConfig;
use crate::response::decode_buffered_response;
use crate::status::{classify_error_status, classify_error_token};
use crate::stream::{StreamDecoder, StreamStep};
use crate::translate::build_request;
use crate::wire::ErrorEnvelope;

/// The Anthropic Messages adapter.
///
/// Implements [`ModelRuntime`]: executes exactly one authorized operation as
/// at most one `POST /v1/messages` request and reports typed evidence. It
/// holds no state between operations, retries nothing, and never issues a
/// second request for one operation.
#[derive(Debug)]
pub struct AnthropicRuntime<A> {
    client: Client,
    messages_url: Url,
    credentials: A,
    version_header: HeaderValue,
    sse_record_limit: usize,
}

/// Why an [`AnthropicRuntime`] could not be constructed.
///
/// Construction failure is a configuration defect, not operation evidence:
/// no operation exists yet, so nothing is reported as unsent.
#[derive(Debug)]
pub enum AnthropicConstructionError {
    /// The configured base URL does not parse as an absolute URL.
    InvalidBaseUrl {
        /// The parser's rendered description.
        detail: String,
    },
    /// The configured `anthropic-version` cannot form an HTTP header value.
    InvalidVersion,
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
    /// # Transport discipline: one send is one physical request (ADR-0005)
    ///
    /// The client is configured so that a single send is provably a single
    /// request:
    ///
    /// - **Redirect following is disabled** ([`Policy::none`]). reqwest's
    ///   default policy follows up to ten redirects and, on a 307 or 308
    ///   response, replays the buffered POST body — a hidden second physical
    ///   provider interaction inside one send, which would corrupt the
    ///   acceptance-boundary evidence ADR-0043 classification consumes. With
    ///   redirects disabled, a redirect status surfaces as
    ///   [`LossCause::UnexpectedHttpStatus`] evidence instead.
    /// - **Protocol-level retries are disabled** (`reqwest::retry::never()`).
    ///   reqwest's default retry policy resends requests rejected by
    ///   protocol NACKs; a second physical POST for one authorized
    ///   operation is exactly what ADR-0005 prohibits, so the never-retry
    ///   policy is set explicitly.
    /// - **Idle-connection reuse is disabled** (`pool_max_idle_per_host(0)`).
    ///   The underlying HTTP client can transparently resend a request when
    ///   a *reused* idle connection turns out to be closed before the
    ///   request was written; with no idle connections every send opens a
    ///   fresh connection, eliminating that replay path — and making a
    ///   connect failure provably precede any request byte, which is what
    ///   lets [`UnsentCause::ConnectFailed`] claim proven-unsent.
    ///
    /// ADR-0043 selects no timeout budget: both timeouts default to none and
    /// are caller-owned configuration.
    pub fn new(
        config: AnthropicConfig,
        credentials: A,
    ) -> Result<Self, AnthropicConstructionError> {
        let messages_url = Url::parse(&format!(
            "{}/v1/messages",
            config.base_url.trim_end_matches('/')
        ))
        .map_err(|error| AnthropicConstructionError::InvalidBaseUrl {
            detail: error.to_string(),
        })?;
        if messages_url.query().is_some() || messages_url.fragment().is_some() {
            // Concatenating the endpoint path onto a base with a query or
            // fragment would route the request somewhere else entirely.
            return Err(AnthropicConstructionError::InvalidBaseUrl {
                detail: "base URL must not carry a query or fragment".to_string(),
            });
        }
        if !matches!(messages_url.scheme(), "http" | "https") {
            // A non-HTTP scheme would fail only inside send(), after
            // SendCommenced, and read as ambiguous transport loss; it is an
            // invalid configuration, caught here.
            return Err(AnthropicConstructionError::InvalidBaseUrl {
                detail: format!("unsupported scheme {:?}", messages_url.scheme()),
            });
        }
        let version_header = HeaderValue::from_str(&config.anthropic_version)
            .map_err(|_| AnthropicConstructionError::InvalidVersion)?;
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

    async fn run<C: Clone + Send + Sync>(
        &self,
        operation: ModelOperation<C>,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
    ) -> TerminalEvidence {
        let wire_request = match build_request(&operation) {
            Ok(request) => request,
            Err(failure) => return proven_unsent(UnsentCause::PreparationFailed(failure)),
        };
        let body = match serde_json::to_vec(&wire_request) {
            Ok(body) => body,
            Err(error) => {
                return proven_unsent(UnsentCause::PreparationFailed(
                    PreparationFailure::SerializationFailed {
                        detail: error.to_string(),
                    },
                ));
            }
        };
        // ADR-0017: the pinned reference is resolved during send preparation
        // of exactly this operation and the value is scoped to this request;
        // nothing is cached, so a rotated credential is picked up by the
        // next operation. The access error is reference-only, never bytes.
        let api_key = match self
            .credentials
            .resolve(&operation.credential_reference)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return proven_unsent(UnsentCause::PreparationFailed(
                    PreparationFailure::CredentialUnavailable {
                        detail: error.to_string(),
                    },
                ));
            }
        };
        let Some(api_key_header) = sensitive_header(&api_key) else {
            return proven_unsent(UnsentCause::PreparationFailed(
                PreparationFailure::CredentialUnavailable {
                    detail: "credential value cannot form an HTTP header value".to_string(),
                },
            ));
        };
        let evidence = self
            .exchange(
                operation.delivery,
                body,
                api_key_header,
                correlation,
                sink,
                cancellation,
            )
            .await;
        // ADR-0017: provider-controlled text in the evidence (error
        // messages, raw bodies, transport detail) is credential-sanitized
        // before it leaves the adapter boundary.
        redact_evidence(evidence, &api_key)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the exchange carries exactly the facts of one prepared send"
    )]
    async fn exchange<C: Clone + Send + Sync>(
        &self,
        delivery: signalbox_model_runtime::DeliveryMode,
        body: Vec<u8>,
        api_key_header: HeaderValue,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
    ) -> TerminalEvidence {
        emit(correlation, sink, ObservationFact::RequestPrepared);
        if already_fired(cancellation) {
            return proven_unsent(UnsentCause::CancelledBeforeSend);
        }
        emit(correlation, sink, ObservationFact::SendCommenced);
        let send = self
            .client
            .post(self.messages_url.clone())
            .header("x-api-key", api_key_header)
            .header("anthropic-version", self.version_header.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body)
            .send();
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
            match delivery {
                signalbox_model_runtime::DeliveryMode::Buffered => {
                    self.finish_buffered(response, exchange, correlation, sink, cancellation)
                        .await
                }
                signalbox_model_runtime::DeliveryMode::Streamed => {
                    self.finish_streamed(response, exchange, correlation, sink, cancellation)
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
    ) -> TerminalEvidence {
        let body = match with_cancellation(cancellation, response.bytes()).await {
            None => return exchange_loss(LossCause::CancellationRequested, exchange),
            Some(Err(error)) => {
                return exchange_loss(classify_body_error(&error), exchange);
            }
            Some(Ok(bytes)) => bytes,
        };
        decode_buffered_response(&body, exchange, correlation, sink)
    }

    async fn finish_streamed<C: Clone + Send + Sync>(
        &self,
        response: reqwest::Response,
        exchange: ExchangeFacts,
        correlation: &C,
        sink: &mut (dyn ObservationSink<C> + Send),
        cancellation: &mut CancellationSignal,
    ) -> TerminalEvidence {
        let mut framing = SseFraming::new(self.sse_record_limit);
        let mut decoder = StreamDecoder::new(exchange);
        let mut body = response.bytes_stream();
        loop {
            let chunk = match with_cancellation(cancellation, body.next()).await {
                None => return decoder.cancelled(),
                Some(chunk) => chunk,
            };
            match chunk {
                // End of transport without `message_stop`: the explicit
                // incomplete-stream fact, never silent success.
                None => return decoder.lost(StreamInterruption::EndOfStream),
                Some(Err(error)) => {
                    return decoder.lost(StreamInterruption::TransportFailure(transport_facts(
                        &error,
                    )));
                }
                Some(Ok(bytes)) => {
                    // Records completed before a framing failure are applied
                    // first, so evidence they carry is never lost to how the
                    // transport batched bytes.
                    let outcome = framing.push(&bytes);
                    for record in outcome.records {
                        match decoder.apply(&record, correlation, sink) {
                            StreamStep::Continue => {}
                            StreamStep::Terminal(evidence) => return evidence,
                        }
                    }
                    if let Some(error) = outcome.error {
                        return decoder.violation_evidence(error.to_string());
                    }
                }
            }
        }
    }
}

impl<C: Clone + Send + Sync, A: CredentialAccess> ModelRuntime<C> for AnthropicRuntime<A> {
    async fn execute(
        &self,
        operation: ModelOperation<C>,
        sink: &mut (dyn ObservationSink<C> + Send),
        mut cancellation: CancellationSignal,
    ) -> TerminalReport<C> {
        let correlation = operation.correlation.clone();
        let evidence = self
            .run(operation, &correlation, sink, &mut cancellation)
            .await;
        TerminalReport {
            correlation,
            evidence,
        }
    }
}

async fn finish_error(
    response: reqwest::Response,
    exchange: ExchangeFacts,
    status: u16,
    cancellation: &mut CancellationSignal,
) -> TerminalEvidence {
    let body = match with_cancellation(cancellation, response.bytes()).await {
        None => return exchange_loss(LossCause::CancellationRequested, exchange),
        Some(Err(error)) => {
            return exchange_loss(
                LossCause::ResponseBodyLost(transport_facts(&error)),
                exchange,
            );
        }
        Some(Ok(bytes)) => bytes,
    };
    if let Ok(ErrorEnvelope { error: Some(error) }) = serde_json::from_slice(&body) {
        // Token first; when the token is absent or unrecognized, the HTTP
        // status still carries the documented category, so classification
        // never depends on incidental body shape.
        let kind = match error.error_type.as_deref().map(classify_error_token) {
            Some(ProviderErrorKind::Unrecognized) | None => classify_error_status(status),
            Some(kind) => kind,
        };
        return TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange,
            kind,
            native: error.into_native_facts(),
        });
    }
    // A complete terminal error status whose body is not the documented
    // envelope is still definitive (ADR-0043); classify by status and
    // retain the raw body as native material.
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange,
        kind: classify_error_status(status),
        native: NativeErrorFacts {
            error_token: None,
            message: Some(lossy_truncated(&body)),
        },
    })
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

/// Classifies a send-phase transport failure per ADR-0043's
/// full-request-send rule.
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
/// way the exchange lacks a definitive response (ADR-0043's ambiguous
/// branch).
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
/// response is never discarded in favor of ambiguous cancellation loss
/// (ADR-0043's definitive-response precedence).
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
    let mut header = HeaderValue::from_bytes(api_key.expose_bytes()).ok()?;
    header.set_sensitive(true);
    Some(header)
}

/// Credential-sanitizes every provider-controlled or transport-rendered
/// text in the evidence (ADR-0017): a reflected key value in an error
/// message, raw body, or rendered detail is replaced before the evidence
/// leaves the adapter boundary. Typed facts are untouched.
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
        native.message = native.message.map(redact);
        native
    };
    let redact_transport =
        |facts: TransportFacts| -> TransportFacts { TransportFacts::new(redact(facts.detail)) };
    match evidence {
        TerminalEvidence::ProviderError(mut error) => {
            error.native = redact_native(error.native);
            TerminalEvidence::ProviderError(error)
        }
        TerminalEvidence::CancellationConfirmed(mut confirmed) => {
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
                cause @ (UnsentCause::PreparationFailed(_) | UnsentCause::CancelledBeforeSend) => {
                    cause
                }
            };
            TerminalEvidence::ProvenUnsent(ProvenUnsentEvidence { cause })
        }
        TerminalEvidence::BoundaryLoss(mut loss) => {
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
        evidence @ (TerminalEvidence::Completed(_) | TerminalEvidence::Refused(_)) => evidence,
    }
}
