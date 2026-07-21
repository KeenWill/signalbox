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
    AssistantPart, BoundaryLossEvidence, CancellationSignal, ExchangeFacts, FinishReason,
    LossCause, ModelOperation, ModelRuntime, NativeErrorFacts, Observation, ObservationFact,
    ObservationSink, PreparationFailure, ProvenUnsentEvidence, ProviderErrorEvidence,
    ProviderMessageId, ProviderReportedModel, ProviderRequestId, SseFraming, StreamInterruption,
    TerminalEvidence, TerminalReport, TokenUsage, ToolCallId, ToolCallProposal, ToolName,
    TransportFacts, UnsentCause,
};

use signalbox_model_runtime::{CredentialAccess, CredentialValue};

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
pub struct OpenAiRuntime<A> {
    client: Client,
    completions_url: Url,
    credentials: A,
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
    pub fn new(config: OpenAiConfig, credentials: A) -> Result<Self, OpenAiConstructionError> {
        let completions_url = Url::parse(&format!(
            "{}/v1/chat/completions",
            config.base_url.trim_end_matches('/')
        ))
        .map_err(|error| OpenAiConstructionError::InvalidBaseUrl {
            detail: error.to_string(),
        })?;
        if completions_url.query().is_some() || completions_url.fragment().is_some() {
            // Concatenating the endpoint path onto a base with a query or
            // fragment would route the request somewhere else entirely.
            return Err(OpenAiConstructionError::InvalidBaseUrl {
                detail: "base URL must not carry a query or fragment".to_string(),
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
        // next operation. The typed reference-only error is preserved, and
        // resolution races the cancellation signal so a blocked credential
        // read cannot hold a cancelled operation.
        let resolve = self.credentials.resolve(&operation.credential_reference);
        let api_key = match with_cancellation(cancellation, resolve).await {
            None => return proven_unsent(UnsentCause::CancelledBeforeSend),
            Some(Err(error)) => {
                return proven_unsent(UnsentCause::PreparationFailed(
                    PreparationFailure::CredentialUnavailable { error },
                ));
            }
            Some(Ok(value)) => value,
        };
        let Some(authorization_header) = sensitive_bearer(&api_key) else {
            return proven_unsent(UnsentCause::PreparationFailed(
                PreparationFailure::CredentialUnusable {
                    detail: "credential value cannot form an HTTP header value".to_string(),
                },
            ));
        };
        let mut redacting_sink = RedactingSink {
            inner: sink,
            credential: &api_key,
        };
        let evidence = self
            .exchange(
                operation.delivery,
                body,
                authorization_header,
                correlation,
                &mut redacting_sink,
                cancellation,
            )
            .await;
        // ADR-0017: all provider-controlled text is credential-sanitized
        // before it leaves the adapter boundary, including successful
        // assistant material that may become semantic history.
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
        authorization_header: HeaderValue,
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
            .post(self.completions_url.clone())
            .header(AUTHORIZATION, authorization_header)
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
        // The Chat Completions success contract is specifically HTTP 200;
        // another 2xx is not recognized terminal-success evidence.
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
                // End of transport without `[DONE]`: the explicit
                // incomplete-stream fact, never silent success.
                None => return decoder.lost(StreamInterruption::EndOfStream),
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

impl<C: Clone + Send + Sync, A: CredentialAccess> ModelRuntime<C> for OpenAiRuntime<A> {
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
            return exchange_loss(classify_body_error(&error), exchange);
        }
        Some(Ok(bytes)) => bytes,
    };
    if let Ok(ErrorEnvelope { error: Some(error) }) = serde_json::from_slice(&body) {
        let kind = classify_error(status, error.code_text().as_deref());
        return TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange,
            // The Chat Completions error envelope reports no model identity.
            reported_model: None,
            kind,
            native: error.into_native_facts(),
        });
    }
    // A complete terminal error status whose body is not the documented
    // envelope is still definitive (ADR-0043); classify by status and
    // retain the raw body as native material.
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange,
        reported_model: None,
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

struct RedactingSink<'a, C> {
    inner: &'a mut (dyn ObservationSink<C> + Send),
    credential: &'a CredentialValue,
}

impl<C> ObservationSink<C> for RedactingSink<'_, C> {
    fn observe(&mut self, mut observation: Observation<C>) {
        observation.fact = redact_observation_fact(observation.fact, self.credential);
        self.inner.observe(observation);
    }
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
        fact @ (ObservationFact::RequestPrepared
        | ObservationFact::SendCommenced
        | ObservationFact::UsageReported(_)
        | ObservationFact::FinishReported(_)) => fact,
    }
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
        native.error_code = native.error_code.map(redact);
        native.message = native.message.map(redact);
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
                cause @ (UnsentCause::PreparationFailed(_) | UnsentCause::CancelledBeforeSend) => {
                    cause
                }
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
        arguments_json: redact_text(proposal.arguments_json, credential),
    }
}

fn redact_assistant_part(part: AssistantPart, credential: &CredentialValue) -> AssistantPart {
    match part {
        AssistantPart::Text(text) => AssistantPart::Text(redact_text(text, credential)),
        AssistantPart::Thinking { text, signature } => AssistantPart::Thinking {
            text: redact_text(text, credential),
            signature: signature.map(|value| redact_text(value, credential)),
        },
        AssistantPart::RedactedThinking { data } => AssistantPart::RedactedThinking {
            data: redact_text(data, credential),
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
    if api_key.expose_bytes().is_empty() {
        return None;
    }
    let mut bytes = b"Bearer ".to_vec();
    bytes.extend_from_slice(api_key.expose_bytes());
    let mut header = HeaderValue::from_bytes(&bytes).ok()?;
    header.set_sensitive(true);
    Some(header)
}
