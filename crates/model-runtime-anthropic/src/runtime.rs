//! The adapter runtime: one operation, at most one HTTP interaction.

use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};

use signalbox_model_runtime::{
    BoundaryLossEvidence, CancellationSignal, CredentialRedactingSink, DeliveryMode, ExchangeFacts,
    InputTokenCountOutcome, LossCause,
    MAX_BUFFERED_PROVIDER_RESPONSE_BYTES as MAX_BUFFERED_RESPONSE_BYTES,
    MAX_STREAMED_PROVIDER_RESPONSE_BYTES as MAX_STREAMED_RESPONSE_BYTES, ModelInputTokenCounter,
    ModelOperation, ModelRuntime, NativeErrorFacts, ObservationFact, ObservationSink,
    PreparationDefect, PreparationFailure, PreparationOutcome, ProviderCompactionMode,
    ProviderErrorEvidence, ProviderErrorKind, ProviderRequestId,
    ResponsePrefixBudget as PrefixBudget, SseFraming, StreamInterruption, TerminalEvidence,
    TerminalReport, TokenUsage, ToolCallsAtLoss, UnsentCause,
    boundary_loss_evidence as exchange_loss, emit_provider_observation as emit, parse_retry_after,
    pre_exchange_loss_evidence as pre_exchange_loss, proven_unsent_evidence as proven_unsent,
    provider_response_body_too_large as response_body_too_large,
    provider_response_prefix_len as streamed_response_prefix_len,
    serialize_provider_request as serialize_request, transport_facts_from_error as transport_facts,
    validate_provider_json_nesting,
};

use signalbox_model_runtime::{CredentialAccess, CredentialValue, redact_evidence};
use signalbox_model_runtime::{FastMode, ModelCapabilityCatalog, ModelCapabilityError};

use crate::config::AnthropicConfig;
use crate::response::decode_buffered_response;
use crate::status::{classify_error_status, classify_error_with_proof};
use crate::stream::{LaterRecords, StreamDecoder, StreamStep};
use crate::translate::{build_request_with_fast_mode, server_compaction_supported};
use crate::wire::{CountTokensRequest, CountTokensResponse, ErrorEnvelope};

const CONTEXT_MANAGEMENT_BETAS: &str = "context-management-2025-06-27,compact-2026-01-12";
const CONTEXT_MANAGEMENT_AND_FAST_MODE_BETAS: &str =
    "context-management-2025-06-27,compact-2026-01-12,fast-mode-2026-02-01";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

const fn anthropic_beta_header(
    request_fast_mode: FastMode,
    server_compaction: bool,
) -> Option<&'static str> {
    match (server_compaction, request_fast_mode) {
        (true, FastMode::Enabled) => Some(CONTEXT_MANAGEMENT_AND_FAST_MODE_BETAS),
        (true, FastMode::Disabled) => Some(CONTEXT_MANAGEMENT_BETAS),
        (false, FastMode::Enabled) => Some(FAST_MODE_BETA),
        (false, FastMode::Disabled) => None,
    }
}

/// The Anthropic Messages adapter.
///
/// Implements [`ModelRuntime`]: executes exactly one authorized operation as
/// at most one `POST /v1/messages` request and reports typed evidence. It
/// holds no state between operations, retries nothing, and never issues a
/// second request for one operation.
pub struct AnthropicRuntime<A> {
    client: Client,
    messages_url: Url,
    count_tokens_url: Url,
    credentials: A,
    version_header: HeaderValue,
    sse_record_limit: usize,
    native_message_limit: Option<usize>,
    model_capabilities: ModelCapabilityCatalog,
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
    provider_compaction_enabled: bool,
}

impl<A> std::fmt::Debug for AnthropicRuntime<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicRuntime")
            .field("client", &self.client)
            .field("messages_url", &self.messages_url)
            .field("count_tokens_url", &self.count_tokens_url)
            .field("credentials", &"[redacted]")
            .field("version_header", &"[sensitive]")
            .field("sse_record_limit", &self.sse_record_limit)
            .field("model_capabilities", &self.model_capabilities)
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
    fn apply_model_capabilities<C>(
        &self,
        operation: &mut ModelOperation<C>,
    ) -> Result<FastMode, ModelCapabilityError> {
        let mut request_fast_mode = operation.settings.fast_mode;
        let capabilities = self
            .model_capabilities
            .validate_explicit(&operation.resolved_target, &operation.settings)?;
        if let Some(capabilities) = capabilities {
            let (target, effective_request_fast_mode) = capabilities
                .effective_target(&operation.resolved_target, operation.settings.fast_mode)?;
            operation.resolved_target = target.clone();
            request_fast_mode = effective_request_fast_mode;
        }
        Ok(request_fast_mode)
    }

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
    pub fn new(
        config: AnthropicConfig,
        credentials: A,
    ) -> Result<Self, AnthropicConstructionError> {
        if config.sse_record_limit == 0 {
            return Err(AnthropicConstructionError::InvalidSseRecordLimit);
        }
        if config
            .exchange_timeout
            .is_some_and(|timeout| timeout.is_zero())
        {
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
        let count_tokens_url = Url::parse(&format!(
            "{}/v1/messages/count_tokens",
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
                .map_err(|error| AnthropicConstructionError::ClientConstruction {
                    detail: error.to_string(),
                })?;
        Ok(Self {
            client,
            messages_url,
            count_tokens_url,
            credentials,
            version_header,
            sse_record_limit: config.sse_record_limit,
            native_message_limit: config.native_message_limit,
            model_capabilities: config.model_capabilities,
        })
    }

    async fn prepare_request<C: Clone + Send + Sync>(
        &self,
        mut operation: ModelOperation<C>,
        cancellation: &mut CancellationSignal,
    ) -> PreparationOutcome<C, AnthropicPreparedRequest<C>> {
        let correlation = operation.correlation.clone();
        let request_fast_mode = match self.apply_model_capabilities(&mut operation) {
            Ok(request_fast_mode) => request_fast_mode,
            Err(error) => {
                return PreparationOutcome::Failed {
                    correlation,
                    failure: PreparationFailure::UnsupportedOperation {
                        detail: error.to_string(),
                    },
                };
            }
        };
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
        let Some(api_key_header) = sensitive_header(&api_key) else {
            return PreparationOutcome::Failed {
                correlation,
                failure: PreparationFailure::CredentialUnusable {
                    detail: "credential value cannot form an HTTP header value".to_string(),
                },
            };
        };
        let delivery = operation.delivery;
        let server_compaction = operation.provider_compaction == ProviderCompactionMode::Allowed
            && server_compaction_supported(operation.resolved_target.as_str());
        let stop_sequences = operation.settings.stop_sequences.clone();
        let mut builder = self
            .client
            .post(self.messages_url.clone())
            .header("x-api-key", api_key_header)
            .header("anthropic-version", self.version_header.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        if let Some(beta_header) = anthropic_beta_header(request_fast_mode, server_compaction) {
            builder = builder.header("anthropic-beta", beta_header);
        }
        let request = match build_http_request(builder) {
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
                    provider_compaction_enabled: server_compaction,
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
        decode_buffered_response(
            &body,
            exchange,
            &settings.stop_sequences,
            settings.provider_compaction_enabled,
            correlation,
            sink,
        )
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
        let mut decoder = StreamDecoder::with_stop_sequences(
            exchange,
            settings.stop_sequences.clone(),
            settings.provider_compaction_enabled,
        );
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
                // End of transport without `message_stop`: the explicit
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
                    let interruption = if error.is_timeout() {
                        StreamInterruption::TimedOut(transport_facts(&error))
                    } else {
                        StreamInterruption::TransportFailure(transport_facts(&error))
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
    // Records completed before a framing failure are applied first, so
    // evidence they carry is never lost to how the transport batched bytes.
    // The same rule applies at the aggregate byte budget: process the
    // in-budget prefix so a terminal marker in it wins over coalesced trailing
    // data.
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

impl<C: Clone + Send + Sync, A: CredentialAccess> ModelInputTokenCounter<C>
    for AnthropicRuntime<A>
{
    async fn count_input_tokens(
        &self,
        mut operation: ModelOperation<C>,
        mut cancellation: CancellationSignal,
    ) -> InputTokenCountOutcome<C> {
        let correlation = operation.correlation.clone();
        let request_fast_mode = match self.apply_model_capabilities(&mut operation) {
            Ok(request_fast_mode) => request_fast_mode,
            Err(_) => return InputTokenCountOutcome::Failed { correlation },
        };
        let server_compaction = operation.provider_compaction == ProviderCompactionMode::Allowed
            && server_compaction_supported(operation.resolved_target.as_str());
        let wire_request = match build_request_with_fast_mode(&operation, request_fast_mode) {
            Ok(request) => CountTokensRequest::from(request),
            Err(_) => return InputTokenCountOutcome::Failed { correlation },
        };
        let body = match serialize_request(&wire_request) {
            Ok(body) => body,
            Err(_) => return InputTokenCountOutcome::Failed { correlation },
        };
        let credential = match cancellation
            .run_until_cancelled(self.credentials.resolve(&operation.credential_reference))
            .await
        {
            None => return InputTokenCountOutcome::Cancelled { correlation },
            Some(Err(_)) => return InputTokenCountOutcome::Failed { correlation },
            Some(Ok(credential)) => credential,
        };
        let Some(api_key_header) = sensitive_header(&credential) else {
            return InputTokenCountOutcome::Failed { correlation };
        };
        let mut builder = self
            .client
            .post(self.count_tokens_url.clone())
            .header("x-api-key", api_key_header)
            .header("anthropic-version", self.version_header.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        if let Some(beta_header) = anthropic_beta_header(request_fast_mode, server_compaction) {
            builder = builder.header("anthropic-beta", beta_header);
        }
        let request = match build_http_request(builder) {
            Ok(request) => request,
            Err(_) => return InputTokenCountOutcome::Failed { correlation },
        };
        let response = match cancellation
            .run_until_cancelled(self.client.execute(request))
            .await
        {
            None => return InputTokenCountOutcome::Cancelled { correlation },
            Some(Err(_)) => return InputTokenCountOutcome::Failed { correlation },
            Some(Ok(response)) => response,
        };
        if !response.status().is_success() {
            let _ = collect_response_body(response, &mut cancellation).await;
            return InputTokenCountOutcome::Failed { correlation };
        }
        let body = match collect_response_body(response, &mut cancellation).await {
            None => return InputTokenCountOutcome::Cancelled { correlation },
            Some(Err(_)) => return InputTokenCountOutcome::Failed { correlation },
            Some(Ok(body)) => body,
        };
        if validate_provider_json_nesting(&body).is_err() {
            return InputTokenCountOutcome::Failed { correlation };
        }
        let response: CountTokensResponse = match serde_json::from_slice(&body) {
            Ok(response) => response,
            Err(_) => return InputTokenCountOutcome::Failed { correlation },
        };
        InputTokenCountOutcome::Counted {
            correlation,
            input_tokens: response.input_tokens,
        }
    }
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
        // A fully buffered reqwest request does not expose independent proof
        // that an early response arrived only after the complete upload.
        // `docs/spec/model-call-execution.md` therefore forbids classifying
        // its refusal token as `Refused`.
        let evidence = without_unproven_refusal(evidence);
        // Per the runtime-substrate spec, provider-controlled text in the
        // evidence (error messages, raw bodies, transport detail) is
        // credential-sanitized before it leaves the adapter boundary, using
        // the exact preparation-time value.
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
        let (kind, non_acceptance_proven) =
            classify_error_with_proof(status, error.error_type.as_deref());
        return TerminalEvidence::ProviderError(ProviderErrorEvidence {
            exchange,
            // The Messages error envelope reports no model identity.
            reported_model: None,
            kind,
            non_acceptance_proven,
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
        non_acceptance_proven: false,
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

fn retry_after_from(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, SystemTime::now()))
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

#[cfg(test)]
mod tests {
    use signalbox_model_runtime::{
        CancellationSignal, CredentialRedactingSink, CredentialValue, ExchangeFacts, FastMode,
        LossCause, Observation, ObservationFact, ObservationSink, PreparationDefect,
        RefusalEvidence, SseFraming, TerminalEvidence, TokenUsage, ToolCallsAtLoss,
    };

    use super::{
        CONTEXT_MANAGEMENT_AND_FAST_MODE_BETAS, CONTEXT_MANAGEMENT_BETAS, FAST_MODE_BETA,
        MAX_STREAMED_RESPONSE_BYTES, anthropic_beta_header, build_http_request,
        process_streamed_chunk, without_unproven_refusal,
    };
    use crate::stream::StreamDecoder;

    #[test]
    fn beta_header_follows_enabled_request_features() {
        assert_eq!(anthropic_beta_header(FastMode::Disabled, false), None);
        assert_eq!(
            anthropic_beta_header(FastMode::Enabled, false),
            Some(FAST_MODE_BETA)
        );
        assert_eq!(
            anthropic_beta_header(FastMode::Disabled, true),
            Some(CONTEXT_MANAGEMENT_BETAS)
        );
        assert_eq!(
            anthropic_beta_header(FastMode::Enabled, true),
            Some(CONTEXT_MANAGEMENT_AND_FAST_MODE_BETAS)
        );
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

    #[test]
    fn inv_035_complete_credential_ending_in_its_own_prefix_is_redacted_in_place() {
        let credential = CredentialValue::new(b"synthetic_s".to_vec());
        let mut observed = Vec::new();
        let mut sink = CredentialRedactingSink::new(&mut observed, &credential);
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: "echo synthetic_s".to_string(),
            },
        });
        sink.observe(Observation {
            correlation: "call-1".to_string(),
            fact: ObservationFact::TextDelta {
                index: 0,
                text: " done".to_string(),
            },
        });
        sink.flush();
        drop(sink);

        assert_eq!(
            observed[0].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: "echo [redacted]".to_string(),
            }
        );
        assert_eq!(
            observed[1].fact,
            ObservationFact::TextDelta {
                index: 0,
                text: " done".to_string(),
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
        let mut decoder =
            StreamDecoder::with_stop_sequences(ExchangeFacts::default(), Vec::new(), false);
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
        let mut decoder =
            StreamDecoder::with_stop_sequences(ExchangeFacts::default(), Vec::new(), false);
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
    /// The mirror of the OpenAI runtime's test: the violating record is the
    /// final one this chunk framed, so the unapplied-records fact is false and
    /// the loss would otherwise state "none opened" — while the bytes past the
    /// limit, never framed at all, could have carried the tool call.
    #[test]
    fn a_violation_before_an_over_budget_suffix_withholds_the_tool_fact() {
        let start = "event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\
            \"role\":\"assistant\",\"id\":\"msg_1\",\"model\":\"model-exact-1\",\
            \"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n";
        // The repeat is the violation: a second `message_start` is rejected on
        // semantics after the record itself decoded.
        let mut bytes = format!("{start}{start}").into_bytes();
        let in_budget_len = bytes.len();
        bytes.extend_from_slice(b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        let mut streamed_bytes = MAX_STREAMED_RESPONSE_BYTES - in_budget_len;
        let mut framing = SseFraming::new(1024);
        let mut decoder =
            StreamDecoder::with_stop_sequences(ExchangeFacts::default(), Vec::new(), false);
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
            panic!("a duplicate message_start is a stream protocol violation");
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
        let mut decoder =
            StreamDecoder::with_stop_sequences(ExchangeFacts::default(), Vec::new(), false);
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
