//! The adapter runtime: one operation, at most one HTTP interaction.
//!
//! Concrete HTTP and TLS construction remain provider-owned; provider-neutral
//! boundary policy is supplied by `signalbox-model-runtime`.

use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};

use signalbox_model_runtime::{
    BoundaryLossEvidence, CancellationSignal, CredentialRedactingSink, DeliveryMode, ExchangeFacts,
    LossCause, MAX_BUFFERED_PROVIDER_RESPONSE_BYTES as MAX_BUFFERED_RESPONSE_BYTES,
    MAX_STREAMED_PROVIDER_RESPONSE_BYTES as MAX_STREAMED_RESPONSE_BYTES, ModelOperation,
    ModelRuntime, NativeErrorFacts, ObservationFact, ObservationSink, PreparationDefect,
    PreparationFailure, PreparationOutcome, ProviderErrorEvidence, ProviderErrorKind,
    ProviderRequestId, ResponsePrefixBudget as PrefixBudget, SseFraming, StreamInterruption,
    TerminalEvidence, TerminalReport, TokenUsage, ToolCallsAtLoss, UnsentCause,
    boundary_loss_evidence as exchange_loss, emit_provider_observation as emit, parse_retry_after,
    pre_exchange_loss_evidence as pre_exchange_loss, proven_unsent_evidence as proven_unsent,
    provider_response_body_too_large as response_body_too_large,
    provider_response_prefix_len as streamed_response_prefix_len,
    serialize_provider_request as serialize_request, transport_facts_from_error as transport_facts,
    validate_provider_json_nesting,
};

use signalbox_model_runtime::ModelCapabilityCatalog;
use signalbox_model_runtime::{CredentialAccess, CredentialValue, redact_evidence};

use crate::config::OpenAiConfig;
use crate::response::{StopSequences, decode_buffered_response};
use crate::status::{classify_error, classify_error_envelope_with_proof};
use crate::stream::{LaterRecords, StreamDecoder, StreamStep};
use crate::translate::build_request_with_fast_mode;
use crate::wire::ErrorEnvelope;

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
    native_message_limit: Option<usize>,
    model_capabilities: ModelCapabilityCatalog,
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
    stop_sequences: StopSequences,
}

impl<A> std::fmt::Debug for OpenAiRuntime<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiRuntime")
            .field("client", &self.client)
            .field("completions_url", &self.completions_url)
            .field("credentials", &"[redacted]")
            .field("sse_record_limit", &self.sse_record_limit)
            .field("model_capabilities", &self.model_capabilities)
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

impl std::fmt::Display for OpenAiConstructionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBaseUrl { detail } => write!(f, "invalid base URL: {detail}"),
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

impl std::error::Error for OpenAiConstructionError {}

impl<A: CredentialAccess> OpenAiRuntime<A> {
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
    /// The caller may leave the connect or whole-exchange timeout unset. A
    /// configured whole-exchange timeout covers connection establishment,
    /// response headers, and buffered or streamed body delivery.
    pub fn new(config: OpenAiConfig, credentials: A) -> Result<Self, OpenAiConstructionError> {
        if config.sse_record_limit == 0 {
            return Err(OpenAiConstructionError::InvalidSseRecordLimit);
        }
        if config
            .exchange_timeout
            .is_some_and(|timeout| timeout.is_zero())
        {
            return Err(OpenAiConstructionError::InvalidExchangeTimeout);
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
        if completions_url.scheme() == "http"
            && !completions_url
                .host_str()
                .and_then(|host| {
                    host.trim_matches(&['[', ']'][..])
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .is_some_and(|address| address.is_loopback())
        {
            return Err(OpenAiConstructionError::InvalidBaseUrl {
                detail: "plain HTTP requires a literal loopback IP host".to_string(),
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
            .pool_max_idle_per_host(0);
        if let Some(timeout) = config.exchange_timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(timeout) = config.connect_timeout {
            builder = builder.connect_timeout(timeout);
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
            native_message_limit: config.native_message_limit,
            model_capabilities: config.model_capabilities,
        })
    }

    async fn prepare_request<C: Clone + Send + Sync>(
        &self,
        mut operation: ModelOperation<C>,
        cancellation: &mut CancellationSignal,
    ) -> PreparationOutcome<C, OpenAiPreparedRequest<C>> {
        let correlation = operation.correlation.clone();
        let capabilities = match self
            .model_capabilities
            .validate_explicit(&operation.resolved_target, &operation.settings)
        {
            Ok(capabilities) => capabilities,
            Err(error) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure: PreparationFailure::UnsupportedOperation {
                        detail: error.to_string(),
                    },
                };
            }
        };
        let mut request_fast_mode = operation.settings.fast_mode;
        if let Some(capabilities) = capabilities {
            let (target, effective_request_fast_mode) = match capabilities
                .effective_target(&operation.resolved_target, operation.settings.fast_mode)
            {
                Ok(application) => application,
                Err(error) => {
                    return PreparationOutcome::Failed {
                        correlation,
                        failure: PreparationFailure::UnsupportedOperation {
                            detail: error.to_string(),
                        },
                    };
                }
            };
            operation.resolved_target = target.clone();
            request_fast_mode = effective_request_fast_mode;
        }
        let wire_request = match build_request_with_fast_mode(&operation, request_fast_mode) {
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
        let api_key = match cancellation.run_until_cancelled(resolve).await {
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
        let stop_sequences = if operation.settings.stop_sequences.is_empty() {
            StopSequences::NotDeclared
        } else {
            StopSequences::Declared
        };
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
        let response = match cancellation.run_until_cancelled(send).await {
            None => return pre_exchange_loss(LossCause::CancellationRequested),
            Some(Err(error)) => return classify_send_error(&error),
            Some(Ok(response)) => response,
        };
        let status = response.status();
        let exchange = ExchangeFacts {
            provider_request_id: request_id_from(response.headers()),
            http_status: Some(status.as_u16()),
            retry_after: retry_after_from(response.headers()),
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
                        settings.stop_sequences,
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
                // The body is never read on this path, so no decoder saw the
                // response's tool material.
                tool_calls: ToolCallsAtLoss::Unobserved,
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
        stop_sequences: StopSequences,
    ) -> TerminalEvidence {
        let body = match collect_response_body(response, cancellation).await {
            None => return exchange_loss(LossCause::CancellationRequested, exchange),
            Some(Err(cause)) => return exchange_loss(cause, exchange),
            Some(Ok(bytes)) => bytes,
        };
        decode_buffered_response(&body, exchange, correlation, sink, stop_sequences)
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
        let mut decoder = StreamDecoder::new(exchange, settings.stop_sequences);
        let mut body = response.bytes_stream();
        let mut streamed_bytes = 0usize;
        loop {
            let chunk = match cancellation.run_until_cancelled(body.next()).await {
                None => {
                    if framing.holds_unframed_bytes() {
                        decoder.note_discarded_unexamined_bytes();
                    }
                    return decoder.cancelled();
                }
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
                            .undecoded_violation_evidence(
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
                    if framing.holds_unframed_bytes() {
                        decoder.note_discarded_unexamined_bytes();
                    }
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

fn build_http_request(
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::Request, PreparationDefect> {
    builder
        .build()
        .map_err(|error| PreparationDefect::RequestConstructionFailed {
            detail: error.to_string(),
        })
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
    let budget = streamed_response_prefix_len(*streamed_bytes, bytes.len());
    let accepted = match budget {
        PrefixBudget::Accepted { len } => len,
        PrefixBudget::Overflowed { accepted_len } => accepted_len,
    };
    *streamed_bytes += accepted;
    // Apply records completed by the in-budget prefix before reporting a
    // framing or aggregate-size failure. A terminal marker in that prefix
    // must not be lost because trailing bytes share its transport chunk.
    let outcome = framing.push(&bytes[..accepted]);
    // A framing violation in this chunk, or a suffix the aggregate budget cut
    // off, leaves bytes no record will ever carry into the decoder. Both are
    // reported only after the apply loop below, but a terminal that one of these
    // records raises returns straight out of that loop and builds its evidence
    // inside `apply` — so without recording the fact here, the loss would state
    // "no tool call opened" while an unexamined suffix that could have carried
    // one was discarded with it.
    if outcome.error.is_some() || matches!(budget, PrefixBudget::Overflowed { .. }) {
        decoder.note_discarded_unexamined_bytes();
    }
    let framed_records = outcome.records.len();
    for (index, record) in outcome.records.into_iter().enumerate() {
        if index > 0 && cancellation.is_cancelled() {
            // The records this chunk framed but the loop has not applied are
            // dropped here. They are already out of the framer, so
            // `holds_unframed_bytes` cannot see them: mark them explicitly.
            decoder.note_discarded_unexamined_bytes();
            return Some(decoder.cancelled());
        }
        // A terminal raised by this record discards everything behind it, and
        // `apply` builds that evidence itself, so the fact has to be in place
        // before the call rather than patched onto its result.
        decoder.note_later_records(if index + 1 < framed_records {
            LaterRecords::Unapplied
        } else {
            LaterRecords::AllApplied
        });
        match decoder.apply(&record, correlation, sink) {
            StreamStep::Continue => {}
            StreamStep::Terminal(evidence) => return Some(*evidence),
        }
    }
    decoder.note_later_records(LaterRecords::AllApplied);
    if let Some(error) = outcome.error {
        return Some(decoder.undecoded_violation_evidence(error.to_string()));
    }
    match budget {
        PrefixBudget::Accepted { .. } => None,
        // The suffix past the limit is dropped without ever being framed, so
        // the tool fact is withheld rather than stated negative.
        PrefixBudget::Overflowed { .. } => Some(decoder.undecoded_violation_evidence(format!(
            "streamed response exceeded the {MAX_STREAMED_RESPONSE_BYTES}-byte adapter limit"
        ))),
    }
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
        if cancellation.is_cancelled() {
            return TerminalReport {
                correlation,
                evidence: proven_unsent(UnsentCause::CancelledBeforeSend),
            };
        }
        let mut redacting_sink = CredentialRedactingSink::new(sink, &credential);
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
        let evidence = redact_evidence(evidence, &credential, self.native_message_limit);
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
                non_acceptance_proven: false,
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
    if validate_provider_json_nesting(&body).is_ok()
        && let Ok(ErrorEnvelope { error: Some(error) }) = serde_json::from_slice(&body)
    {
        let code = error.code_text();
        let (kind, non_acceptance_proven) = classify_error_envelope_with_proof(
            status,
            code.as_deref(),
            error.error_type.as_deref(),
        );
        return TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange,
            // The Chat Completions error envelope reports no model identity.
            reported_model: None,
            kind,
            non_acceptance_proven,
            native: error.into_native_facts(),
            usage: TokenUsage::unreported(),
        });
    }
    fallback_provider_error(exchange, status, &body)
}

fn fallback_provider_error(exchange: ExchangeFacts, status: u16, body: &[u8]) -> TerminalEvidence {
    // Preserve the complete bounded body until the execution boundary can
    // sanitize JSON escapes with the exact prepared credential. Truncating
    // first can make valid JSON unparseable and hide a reversible credential
    // representation from JSON-aware redaction.
    TerminalEvidence::ProviderError(ProviderErrorEvidence {
        exchange,
        reported_model: None,
        kind: classify_error(status, None),
        non_acceptance_proven: false,
        native: NativeErrorFacts {
            error_token: None,
            error_code: None,
            message: Some(String::from_utf8_lossy(body).into_owned()),
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
        match cancellation.run_until_cancelled(body.next()).await {
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

fn retry_after_from(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, SystemTime::now()))
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
    use signalbox_model_runtime::{
        CancellationSignal, CredentialRedactingSink, CredentialValue, ExchangeFacts, LossCause,
        NativeErrorFacts, Observation, ObservationFact, ObservationSink, PreparationDefect,
        RefusalEvidence, SseFraming, TerminalEvidence, TokenUsage, ToolCallsAtLoss,
    };

    use super::{
        MAX_STREAMED_RESPONSE_BYTES, build_http_request, process_streamed_chunk,
        without_unproven_refusal,
    };
    use crate::response::StopSequences;
    use crate::stream::StreamDecoder;

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

    #[test]
    fn split_json_escaped_credentials_are_redacted_before_tool_deltas_leave() {
        let credential = CredentialValue::new(b"key_loop".to_vec());
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &credential);
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
        sink.flush();
        drop(sink);

        assert_eq!(
            observed[0].fact,
            ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"{"token":""#.to_string(),
            }
        );
        assert_eq!(
            observed[1].fact,
            ObservationFact::ToolArgumentsDelta {
                index: 0,
                fragment: r#"[redacted]"}"#.to_string(),
            }
        );
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
    fn streamed_response_overflow_is_typed_protocol_loss() {
        let mut streamed_bytes = MAX_STREAMED_RESPONSE_BYTES;
        let mut framing = SseFraming::new(1024);
        let mut decoder = StreamDecoder::new(ExchangeFacts::default(), StopSequences::NotDeclared);
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
        let mut decoder = StreamDecoder::new(ExchangeFacts::default(), StopSequences::NotDeclared);
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

    /// A semantic violation raised by the last in-budget record withholds the
    /// tool fact, because the suffix the budget cut off is discarded unexamined.
    ///
    /// The violating record is the final one this chunk framed, so the
    /// unapplied-records fact is false and the loss would otherwise state "none
    /// opened" — while the bytes past the limit, which are never framed at all,
    /// could have carried the tool call.
    #[test]
    fn a_violation_before_an_over_budget_suffix_withholds_the_tool_fact() {
        let mut bytes = b"data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_1\",\
            \"model\":\"model-exact-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n\
            data: {\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl_2\",\
            \"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n"
            .to_vec();
        let in_budget_len = bytes.len();
        bytes.extend_from_slice(b"data: coalesced suffix past the adapter limit\n\n");
        let mut streamed_bytes = MAX_STREAMED_RESPONSE_BYTES - in_budget_len;
        let mut framing = SseFraming::new(1024);
        let mut decoder = StreamDecoder::new(ExchangeFacts::default(), StopSequences::NotDeclared);
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

        let Some(TerminalEvidence::BoundaryLoss(loss)) = evidence else {
            panic!("conflicting completion ids are a stream protocol violation");
        };
        assert!(matches!(
            loss.cause,
            LossCause::StreamProtocolViolation { .. }
        ));
        assert_eq!(loss.tool_calls, ToolCallsAtLoss::Unobserved);
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
        let mut decoder = StreamDecoder::new(ExchangeFacts::default(), StopSequences::NotDeclared);
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
