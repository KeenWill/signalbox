//! The adapter runtime: one operation, at most one HTTP interaction.
//!
//! Deliberately mirrors the Anthropic adapter's transport glue rather than
//! sharing a crate with it: the discipline is small, and each adapter's
//! evidence path stays independently reviewable. Extracting a shared
//! transport crate is a refactor candidate once a third adapter exists.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Waker};

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};

use signalbox_model_runtime::{
    BoundaryLossEvidence, CancellationSignal, ExchangeFacts, LossCause, ModelOperation,
    ModelRuntime, NativeErrorFacts, Observation, ObservationFact, ObservationSink,
    PreparationFailure, ProvenUnsentEvidence, ProviderErrorEvidence, ProviderRequestId, SseFraming,
    StreamInterruption, TerminalEvidence, TerminalReport, TokenUsage, TransportFacts, UnsentCause,
};

use crate::config::OpenAiConfig;
use crate::response::decode_buffered_response;
use crate::status::classify_error;
use crate::stream::{StreamDecoder, StreamStep};
use crate::translate::build_request;
use crate::wire::ErrorEnvelope;

/// The OpenAI Chat Completions adapter.
///
/// Implements [`ModelRuntime`]: executes exactly one authorized operation as
/// at most one `POST /v1/chat/completions` request and reports typed
/// evidence. It holds no state between operations, retries nothing, and
/// never issues a second request for one operation.
#[derive(Debug)]
pub struct OpenAiRuntime {
    client: Client,
    completions_url: Url,
    authorization_header: HeaderValue,
    sse_record_limit: usize,
}

/// Why an [`OpenAiRuntime`] could not be constructed.
///
/// Construction failure is a configuration defect, not operation evidence:
/// no operation exists yet, so nothing is reported as unsent.
#[derive(Debug)]
pub enum OpenAiConstructionError {
    /// The configured base URL does not parse as an absolute URL.
    InvalidBaseUrl {
        /// The parser's rendered description.
        detail: String,
    },
    /// The API key contains bytes that cannot form an HTTP header value.
    /// The value itself is deliberately not rendered.
    InvalidApiKey,
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
            Self::InvalidApiKey => f.write_str("API key cannot form an HTTP header value"),
            Self::ClientConstruction { detail } => {
                write!(f, "HTTP client construction failed: {detail}")
            }
        }
    }
}

impl std::error::Error for OpenAiConstructionError {}

impl OpenAiRuntime {
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
    pub fn new(config: OpenAiConfig) -> Result<Self, OpenAiConstructionError> {
        let completions_url = Url::parse(&format!(
            "{}/v1/chat/completions",
            config.base_url.trim_end_matches('/')
        ))
        .map_err(|error| OpenAiConstructionError::InvalidBaseUrl {
            detail: error.to_string(),
        })?;
        let mut authorization_header =
            HeaderValue::from_str(&format!("Bearer {}", config.api_key.value()))
                .map_err(|_| OpenAiConstructionError::InvalidApiKey)?;
        authorization_header.set_sensitive(true);
        let mut builder = Client::builder()
            .redirect(Policy::none())
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
            authorization_header,
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
        emit(correlation, sink, ObservationFact::RequestPrepared);
        if already_fired(cancellation) {
            return proven_unsent(UnsentCause::CancelledBeforeSend);
        }
        emit(correlation, sink, ObservationFact::SendCommenced);
        let send = self
            .client
            .post(self.completions_url.clone())
            .header(AUTHORIZATION, self.authorization_header.clone())
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
        if status.is_success() {
            match operation.delivery {
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
            // With redirects disabled a redirect (or any other non-success,
            // non-error status) surfaces as evidence rather than a silent
            // second send; see `new` for the rationale.
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
                return exchange_loss(
                    LossCause::ResponseBodyLost(transport_facts(&error)),
                    exchange,
                );
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
                // End of transport without `[DONE]`: the explicit
                // incomplete-stream fact, never silent success.
                None => return decoder.lost(StreamInterruption::EndOfStream),
                Some(Err(error)) => {
                    return decoder.lost(StreamInterruption::TransportFailure(transport_facts(
                        &error,
                    )));
                }
                Some(Ok(bytes)) => {
                    let records = match framing.push(&bytes) {
                        Ok(records) => records,
                        Err(error) => return decoder.violation_evidence(error.to_string()),
                    };
                    for record in records {
                        match decoder.apply(&record, correlation, sink) {
                            StreamStep::Continue => {}
                            StreamStep::Terminal(evidence) => return evidence,
                        }
                    }
                }
            }
        }
    }
}

impl<C: Clone + Send + Sync> ModelRuntime<C> for OpenAiRuntime {
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
        let kind = classify_error(status, error.code_text().as_deref());
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
        kind: classify_error(status, None),
        native: NativeErrorFacts {
            error_token: None,
            error_code: None,
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

/// Runs `work` unless the cancellation signal fires first.
async fn with_cancellation<F: Future>(
    cancellation: &mut CancellationSignal,
    work: F,
) -> Option<F::Output> {
    let work = std::pin::pin!(work);
    match select(cancellation, work).await {
        Either::Left(((), _)) => None,
        Either::Right((output, _)) => Some(output),
    }
}
