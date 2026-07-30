//! Opt-in, content-free telemetry export for the daemon.
//!
//! The tracing boundary admits only audited span and event schemas. Prometheus
//! metrics are built from closed lifecycle dispositions and never accept
//! identifiers as labels.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ffi::OsString,
    fmt,
    fs::File,
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use opentelemetry::{
    Context as OTelContext, KeyValue,
    trace::{SpanBuilder, TraceContextExt as _, Tracer as _, TracerProvider as _},
};
use opentelemetry_otlp::{
    Protocol, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::{
    Resource,
    error::OTelSdkResult,
    trace::{
        BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider, SpanData,
        SpanExporter as SdkSpanExporter,
    },
};
use prometheus::{IntCounter, IntCounterVec, Opts, Registry, TextEncoder};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue, MetadataMap};
use tonic::transport::ClientTlsConfig;
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};
use url::Url;

/// Presence of this setting enables OTLP span export.
pub const OTLP_ENDPOINT_ENVIRONMENT: &str = "SIGNALBOX_OTLP_ENDPOINT";
/// Selects `grpc` or `http/protobuf`; omission uses `grpc`.
pub const OTLP_PROTOCOL_ENVIRONMENT: &str = "SIGNALBOX_OTLP_PROTOCOL";
/// Names the bounded collector-header file; omission sends no custom headers.
pub const OTLP_HEADERS_FILE_ENVIRONMENT: &str = "SIGNALBOX_OTLP_HEADERS_FILE";
/// Sets the parent-based trace-id sampling ratio; omission uses `1.0`.
pub const OTLP_SAMPLING_RATIO_ENVIRONMENT: &str = "SIGNALBOX_OTLP_SAMPLING_RATIO";
/// Sets the checked `service.name`; omission uses `signalboxd`.
pub const OTLP_SERVICE_NAME_ENVIRONMENT: &str = "SIGNALBOX_OTLP_SERVICE_NAME";
/// Presence of this setting enables the separate Prometheus scrape listener.
pub const PROMETHEUS_BIND_ENVIRONMENT: &str = "SIGNALBOX_PROMETHEUS_BIND";

/// Maximum completed spans held before the newest span is dropped.
pub const OTLP_MAX_QUEUED_SPANS: usize = 512;
/// Maximum spans sent by one serial export request.
pub const OTLP_MAX_EXPORT_BATCH: usize = 128;
const OTLP_EXPORT_INTERVAL: Duration = Duration::from_secs(5);
const OTLP_EXPORT_TIMEOUT: Duration = Duration::from_secs(5);
const OTLP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_HEADER_FILE_BYTES: u64 = 16 * 1024;
const MAX_HEADER_COUNT: usize = 16;
const MAX_HEADER_NAME_BYTES: usize = 64;
const MAX_HEADER_VALUE_BYTES: usize = 1_024;
const MAX_SCRAPE_REQUEST_BYTES: usize = 8 * 1024;
const SCRAPE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_SERVICE_NAME: &str = "signalboxd";
const SERVICE_NAMES: &[&str] = &[
    DEFAULT_SERVICE_NAME,
    "signalboxd.development",
    "signalboxd.staging",
    "signalboxd.production",
];
const TELEMETRY_INTERNAL_TARGET: &str = "signalbox_telemetry_internal";
const AMBIENT_OTLP_ENVIRONMENTS: &[&str] = &[
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_TIMEOUT",
    "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    "OTEL_EXPORTER_OTLP_COMPRESSION",
    "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
];

/// Closed reason why opt-in telemetry configuration could not be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryConfigurationFailure {
    NotUnicode,
    Empty,
    InvalidEndpoint,
    InvalidProtocol,
    InvalidSamplingRatio,
    InvalidServiceName,
    InvalidBindAddress,
    HeaderFileUnreadable,
    HeaderFileTooLarge,
    InvalidHeader,
    TooManyHeaders,
    DuplicateHeader,
    ExporterConstruction,
    AmbientOtlpSetting,
    MetricsConstruction,
}

impl TelemetryConfigurationFailure {
    const fn description(self) -> &'static str {
        match self {
            Self::NotUnicode => "is not valid Unicode",
            Self::Empty => "is empty",
            Self::InvalidEndpoint => "is not an admitted OTLP endpoint",
            Self::InvalidProtocol => "is not grpc or http/protobuf",
            Self::InvalidSamplingRatio => "is not a finite ratio from 0 through 1",
            Self::InvalidServiceName => "is not a checked service name",
            Self::InvalidBindAddress => "is not an IP socket address",
            Self::HeaderFileUnreadable => "could not be read",
            Self::HeaderFileTooLarge => "exceeds the telemetry header-file bound",
            Self::InvalidHeader => "contains an invalid telemetry header",
            Self::TooManyHeaders => "contains too many telemetry headers",
            Self::DuplicateHeader => "contains a duplicate telemetry header",
            Self::ExporterConstruction => "could not construct the OTLP exporter",
            Self::AmbientOtlpSetting => "is an unsupported ambient OTLP setting",
            Self::MetricsConstruction => "could not construct the Prometheus registry",
        }
    }
}

/// Sanitized opt-in telemetry configuration failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryConfigurationError {
    setting: &'static str,
    failure: TelemetryConfigurationFailure,
}

impl TelemetryConfigurationError {
    const fn new(setting: &'static str, failure: TelemetryConfigurationFailure) -> Self {
        Self { setting, failure }
    }

    /// Returns only the public setting name, never its value.
    pub const fn setting(&self) -> &'static str {
        self.setting
    }

    /// Returns the closed failure classification.
    pub const fn failure(&self) -> TelemetryConfigurationFailure {
        self.failure
    }
}

impl fmt::Display for TelemetryConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "setting {} {}",
            self.setting,
            self.failure.description()
        )
    }
}

impl std::error::Error for TelemetryConfigurationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OtlpProtocol {
    Grpc,
    HttpProtobuf,
}

struct OtlpHeader {
    grpc_name: AsciiMetadataKey,
    grpc_value: AsciiMetadataValue,
    name: String,
    value: String,
}

struct OtlpConfiguration {
    endpoint: String,
    uses_tls: bool,
    protocol: OtlpProtocol,
    headers: Vec<OtlpHeader>,
    sampling_ratio: f64,
    service_name: String,
}

/// Process-lifetime opt-in export configuration.
pub struct TelemetryConfiguration {
    otlp: Option<OtlpConfiguration>,
    prometheus_bind: Option<SocketAddr>,
}

impl TelemetryConfiguration {
    /// Returns the configuration that creates no exporter or listener.
    pub const fn disabled() -> Self {
        Self {
            otlp: None,
            prometheus_bind: None,
        }
    }

    /// Reads optional telemetry settings through the daemon's environment convention.
    pub fn from_environment() -> Result<Self, TelemetryConfigurationError> {
        Self::from_values(TelemetryEnvironment {
            endpoint: env::var_os(OTLP_ENDPOINT_ENVIRONMENT),
            protocol: env::var_os(OTLP_PROTOCOL_ENVIRONMENT),
            headers_file: env::var_os(OTLP_HEADERS_FILE_ENVIRONMENT),
            sampling_ratio: env::var_os(OTLP_SAMPLING_RATIO_ENVIRONMENT),
            service_name: env::var_os(OTLP_SERVICE_NAME_ENVIRONMENT),
            prometheus_bind: env::var_os(PROMETHEUS_BIND_ENVIRONMENT),
            ambient_otlp_setting: AMBIENT_OTLP_ENVIRONMENTS
                .iter()
                .copied()
                .find(|setting| env::var_os(setting).is_some()),
        })
    }

    fn from_values(values: TelemetryEnvironment) -> Result<Self, TelemetryConfigurationError> {
        let prometheus_bind =
            optional_unicode(PROMETHEUS_BIND_ENVIRONMENT, values.prometheus_bind)?
                .map(|value| {
                    value.parse::<SocketAddr>().map_err(|_| {
                        TelemetryConfigurationError::new(
                            PROMETHEUS_BIND_ENVIRONMENT,
                            TelemetryConfigurationFailure::InvalidBindAddress,
                        )
                    })
                })
                .transpose()?;
        let Some(endpoint) = optional_unicode(OTLP_ENDPOINT_ENVIRONMENT, values.endpoint)? else {
            return Ok(Self {
                otlp: None,
                prometheus_bind,
            });
        };
        if let Some(setting) = values.ambient_otlp_setting {
            return Err(TelemetryConfigurationError::new(
                setting,
                TelemetryConfigurationFailure::AmbientOtlpSetting,
            ));
        }
        let uses_tls = validate_endpoint(&endpoint)?;
        let protocol =
            match optional_unicode(OTLP_PROTOCOL_ENVIRONMENT, values.protocol)?.as_deref() {
                None | Some("grpc") => OtlpProtocol::Grpc,
                Some("http/protobuf") => OtlpProtocol::HttpProtobuf,
                Some(_) => {
                    return Err(TelemetryConfigurationError::new(
                        OTLP_PROTOCOL_ENVIRONMENT,
                        TelemetryConfigurationFailure::InvalidProtocol,
                    ));
                }
            };
        let sampling_ratio =
            optional_unicode(OTLP_SAMPLING_RATIO_ENVIRONMENT, values.sampling_ratio)?
                .map(|value| parse_sampling_ratio(&value))
                .transpose()?
                .unwrap_or(1.0);
        let service_name = optional_unicode(OTLP_SERVICE_NAME_ENVIRONMENT, values.service_name)?
            .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_owned());
        validate_service_name(&service_name)?;
        let headers = optional_path(OTLP_HEADERS_FILE_ENVIRONMENT, values.headers_file)?
            .map(|path| read_headers(&path))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            otlp: Some(OtlpConfiguration {
                endpoint,
                uses_tls,
                protocol,
                headers,
                sampling_ratio,
                service_name,
            }),
            prometheus_bind,
        })
    }

    /// Returns the configured scrape address when Prometheus is enabled.
    pub const fn prometheus_bind(&self) -> Option<SocketAddr> {
        self.prometheus_bind
    }

    /// Constructs an OTLP runtime only when its endpoint setting is present.
    pub fn build_otlp_runtime(&self) -> Result<Option<OtlpRuntime>, TelemetryConfigurationError> {
        self.otlp.as_ref().map(OtlpRuntime::build).transpose()
    }
}

struct TelemetryEnvironment {
    endpoint: Option<OsString>,
    protocol: Option<OsString>,
    headers_file: Option<OsString>,
    sampling_ratio: Option<OsString>,
    service_name: Option<OsString>,
    prometheus_bind: Option<OsString>,
    ambient_otlp_setting: Option<&'static str>,
}

fn optional_unicode(
    setting: &'static str,
    value: Option<OsString>,
) -> Result<Option<String>, TelemetryConfigurationError> {
    value
        .map(|value| {
            let value = value.into_string().map_err(|_| {
                TelemetryConfigurationError::new(setting, TelemetryConfigurationFailure::NotUnicode)
            })?;
            if value.is_empty() {
                Err(TelemetryConfigurationError::new(
                    setting,
                    TelemetryConfigurationFailure::Empty,
                ))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn optional_path(
    setting: &'static str,
    value: Option<OsString>,
) -> Result<Option<PathBuf>, TelemetryConfigurationError> {
    value
        .map(|value| {
            if value.is_empty() {
                Err(TelemetryConfigurationError::new(
                    setting,
                    TelemetryConfigurationFailure::Empty,
                ))
            } else {
                Ok(PathBuf::from(value))
            }
        })
        .transpose()
}

fn validate_endpoint(endpoint: &str) -> Result<bool, TelemetryConfigurationError> {
    let parsed = Url::parse(endpoint).map_err(|_| {
        TelemetryConfigurationError::new(
            OTLP_ENDPOINT_ENVIRONMENT,
            TelemetryConfigurationFailure::InvalidEndpoint,
        )
    })?;
    let admitted = endpoint.len() <= MAX_ENDPOINT_BYTES
        && matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if admitted {
        Ok(parsed.scheme() == "https")
    } else {
        Err(TelemetryConfigurationError::new(
            OTLP_ENDPOINT_ENVIRONMENT,
            TelemetryConfigurationFailure::InvalidEndpoint,
        ))
    }
}

fn parse_sampling_ratio(value: &str) -> Result<f64, TelemetryConfigurationError> {
    let ratio = value.parse::<f64>().map_err(|_| {
        TelemetryConfigurationError::new(
            OTLP_SAMPLING_RATIO_ENVIRONMENT,
            TelemetryConfigurationFailure::InvalidSamplingRatio,
        )
    })?;
    if ratio.is_finite() && (0.0..=1.0).contains(&ratio) {
        Ok(ratio)
    } else {
        Err(TelemetryConfigurationError::new(
            OTLP_SAMPLING_RATIO_ENVIRONMENT,
            TelemetryConfigurationFailure::InvalidSamplingRatio,
        ))
    }
}

fn validate_service_name(value: &str) -> Result<(), TelemetryConfigurationError> {
    if SERVICE_NAMES.contains(&value) {
        Ok(())
    } else {
        Err(TelemetryConfigurationError::new(
            OTLP_SERVICE_NAME_ENVIRONMENT,
            TelemetryConfigurationFailure::InvalidServiceName,
        ))
    }
}

fn read_headers(path: &Path) -> Result<Vec<OtlpHeader>, TelemetryConfigurationError> {
    let mut file = File::open(path)
        .map_err(|_| header_error(TelemetryConfigurationFailure::HeaderFileUnreadable))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_HEADER_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| header_error(TelemetryConfigurationFailure::HeaderFileUnreadable))?;
    if bytes.len() as u64 > MAX_HEADER_FILE_BYTES {
        return Err(header_error(
            TelemetryConfigurationFailure::HeaderFileTooLarge,
        ));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| header_error(TelemetryConfigurationFailure::InvalidHeader))?;
    parse_headers(&content)
}

fn parse_headers(content: &str) -> Result<Vec<OtlpHeader>, TelemetryConfigurationError> {
    let mut headers = Vec::new();
    let mut names = HashSet::new();
    for line in content.lines() {
        if headers.len() == MAX_HEADER_COUNT {
            return Err(header_error(TelemetryConfigurationFailure::TooManyHeaders));
        }
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| header_error(TelemetryConfigurationFailure::InvalidHeader))?;
        let normalized = name.to_ascii_lowercase();
        let grpc_name = normalized
            .parse::<AsciiMetadataKey>()
            .map_err(|_| header_error(TelemetryConfigurationFailure::InvalidHeader))?;
        let grpc_value = value
            .parse::<AsciiMetadataValue>()
            .map_err(|_| header_error(TelemetryConfigurationFailure::InvalidHeader))?;
        if !valid_header_name(name) || !valid_header_value(value) {
            return Err(header_error(TelemetryConfigurationFailure::InvalidHeader));
        }
        if !names.insert(normalized.clone()) {
            return Err(header_error(TelemetryConfigurationFailure::DuplicateHeader));
        }
        headers.push(OtlpHeader {
            name: normalized,
            value: value.to_owned(),
            grpc_name,
            grpc_value,
        });
    }
    Ok(headers)
}

fn header_error(failure: TelemetryConfigurationFailure) -> TelemetryConfigurationError {
    TelemetryConfigurationError::new(OTLP_HEADERS_FILE_ENVIRONMENT, failure)
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}
fn valid_header_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HEADER_VALUE_BYTES
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

/// Owns the provider and its bounded background batch processor.
pub struct OtlpRuntime {
    provider: SdkTracerProvider,
}
fn telemetry_resource(service_name: &str) -> Resource {
    Resource::builder_empty()
        .with_service_name(service_name.to_owned())
        .build()
}

impl OtlpRuntime {
    fn build(configuration: &OtlpConfiguration) -> Result<Self, TelemetryConfigurationError> {
        let exporter = match configuration.protocol {
            OtlpProtocol::Grpc => build_grpc_exporter(configuration),
            OtlpProtocol::HttpProtobuf => build_http_exporter(configuration),
        }
        .map_err(|_| {
            TelemetryConfigurationError::new(
                OTLP_ENDPOINT_ENVIRONMENT,
                TelemetryConfigurationFailure::ExporterConstruction,
            )
        })?;
        let processor = BatchSpanProcessor::builder(IsolatedSpanExporter { inner: exporter })
            .with_batch_config(
                BatchConfigBuilder::default()
                    .with_max_queue_size(OTLP_MAX_QUEUED_SPANS)
                    .with_max_export_batch_size(OTLP_MAX_EXPORT_BATCH)
                    .with_scheduled_delay(OTLP_EXPORT_INTERVAL)
                    .build(),
            )
            .build();
        let resource = telemetry_resource(&configuration.service_name);
        let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            configuration.sampling_ratio,
        )));
        let provider = SdkTracerProvider::builder()
            .with_sampler(sampler)
            .with_resource(resource)
            .with_span_processor(processor)
            .build();
        Ok(Self { provider })
    }

    /// Returns the value-validating layer installed in the tracing subscriber.
    pub fn layer(&self) -> TelemetryExportLayer {
        TelemetryExportLayer::new(self.provider.tracer("signalboxd"))
    }

    /// Flushes only during process shutdown, bounded independently of requests.
    pub fn shutdown(self) {
        if self
            .provider
            .shutdown_with_timeout(OTLP_SHUTDOWN_TIMEOUT)
            .is_err()
        {
            tracing::warn!(
                target: TELEMETRY_INTERNAL_TARGET,
                cause_code = "otlp_shutdown_failed",
                "OpenTelemetry shutdown did not complete within its bound"
            );
        }
    }
}

fn build_http_exporter(configuration: &OtlpConfiguration) -> Result<SpanExporter, ()> {
    let headers = configuration
        .headers
        .iter()
        .map(|header| (header.name.clone(), header.value.clone()))
        .collect::<HashMap<_, _>>();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::builder()
        .timeout(OTLP_EXPORT_TIMEOUT)
        .no_proxy()
        .build()
        .map_err(|_| ())?;
    SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(configuration.endpoint.clone())
        .with_timeout(OTLP_EXPORT_TIMEOUT)
        .with_http_client(client)
        .with_headers(headers)
        .build()
        .map_err(|_| ())
}

fn build_grpc_exporter(configuration: &OtlpConfiguration) -> Result<SpanExporter, ()> {
    let builder = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(configuration.endpoint.clone())
        .with_timeout(OTLP_EXPORT_TIMEOUT);
    let builder = if configuration.uses_tls {
        builder.with_tls_config(ClientTlsConfig::new().with_native_roots())
    } else {
        builder
    };
    builder
        .with_metadata(grpc_metadata(&configuration.headers))
        .build()
        .map_err(|_| ())
}

fn grpc_metadata(headers: &[OtlpHeader]) -> MetadataMap {
    let mut metadata = MetadataMap::new();
    for header in headers {
        metadata.insert(header.grpc_name.clone(), header.grpc_value.clone());
    }
    metadata
}

#[derive(Debug)]
struct IsolatedSpanExporter<Exporter> {
    inner: Exporter,
}

impl<Exporter> SdkSpanExporter for IsolatedSpanExporter<Exporter>
where
    Exporter: SdkSpanExporter,
{
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        if self.inner.export(batch).await.is_err() {
            tracing::warn!(
                target: TELEMETRY_INTERNAL_TARGET,
                cause_code = "otlp_export_failed",
                "OpenTelemetry batch export failed; spans were dropped"
            );
        }
        Ok(())
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let _ = self.inner.shutdown_with_timeout(timeout);
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        let _ = self.inner.force_flush();
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// Value-validating OpenTelemetry layer for the existing tracing subscriber.
#[derive(Clone, Debug)]
pub struct TelemetryExportLayer {
    tracer: SdkTracer,
}

impl TelemetryExportLayer {
    fn new(tracer: SdkTracer) -> Self {
        Self { tracer }
    }
}

#[derive(Clone)]
struct ExportedSpan(OTelContext);

impl<S> Layer<S> for TelemetryExportLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let metadata = attributes.metadata();
        let Some(expected) = admitted_span_fields(metadata) else {
            return;
        };
        let mut values = RecordedValues::default();
        attributes.record(&mut values);
        let Some(otel_attributes) = values.uuid_attributes(expected) else {
            return;
        };
        let parent = attributes
            .parent()
            .and_then(|parent| context.span(parent))
            .or_else(|| {
                attributes
                    .is_contextual()
                    .then(|| context.lookup_current())
                    .flatten()
            })
            .and_then(|parent| {
                parent
                    .extensions()
                    .get::<ExportedSpan>()
                    .map(|exported| exported.0.clone())
            })
            .unwrap_or_default();
        let builder = SpanBuilder::from_name(metadata.name()).with_attributes(otel_attributes);
        let span = self.tracer.build_with_context(builder, &parent);
        let exported = ExportedSpan(parent.with_span(span));
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(exported);
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let Some((name, attributes)) = admitted_event_record(event) else {
            return;
        };
        let exported = event
            .parent()
            .and_then(|parent| context.span(parent))
            .or_else(|| {
                event
                    .is_contextual()
                    .then(|| context.lookup_current())
                    .flatten()
            })
            .and_then(|span| {
                span.extensions()
                    .get::<ExportedSpan>()
                    .map(|exported| exported.0.clone())
            });
        if let Some(exported) = exported {
            exported.span().add_event(name, attributes);
        }
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let exported = context
            .span(&id)
            .and_then(|span| span.extensions_mut().remove::<ExportedSpan>());
        if let Some(exported) = exported {
            exported.0.span().end();
        }
    }
}

fn admitted_span_fields(metadata: &Metadata<'_>) -> Option<&'static [&'static str]> {
    let expected: &'static [&'static str] = match (metadata.target(), metadata.name()) {
        ("signalbox_application::scheduler", "session_work") => &["session_id"],
        ("signalboxd" | "signalboxd::context_guard", "turn_work") => &["session_id", "turn_id"],
        _ => return None,
    };
    fields_are(metadata, expected).then_some(expected)
}

fn candidate_event(metadata: &Metadata<'_>) -> bool {
    matches!(
        metadata.target(),
        "signalboxd::context_guard"
            | "signalbox_application::start_eligible_turn"
            | "signalbox_application::model_execution"
            | "signalbox_application::tool_loop"
            | "signalbox_model_provider_runtime"
    ) && metadata.fields().iter().all(|field| {
        matches!(
            field.name(),
            "message"
                | "session_id"
                | "turn_id"
                | "model_call_id"
                | "turn_attempt_id"
                | "cause_code"
                | "terminal_outcome"
        )
    })
}

fn fields_are(metadata: &Metadata<'_>, expected: &[&str]) -> bool {
    metadata.fields().len() == expected.len()
        && expected
            .iter()
            .all(|name| metadata.fields().field(name).is_some())
}

fn admitted_event_record(event: &Event<'_>) -> Option<(String, Vec<KeyValue>)> {
    let metadata = event.metadata();
    if !candidate_event(metadata) {
        return None;
    }
    let mut values = RecordedValues::default();
    event.record(&mut values);
    if !admitted_event_values(metadata, &values) {
        return None;
    }
    let name = values.get("message")?.trim_matches('"').to_owned();
    let mut attributes = values.otel_attributes()?;
    attributes.push(KeyValue::new("level", metadata.level().as_str().to_owned()));
    attributes.push(KeyValue::new("target", metadata.target().to_owned()));
    Some((name, attributes))
}

fn admitted_event_values(metadata: &Metadata<'_>, values: &RecordedValues) -> bool {
    let Some(message) = values.get("message") else {
        return false;
    };
    match (metadata.target(), message) {
        (
            "signalboxd::context_guard" | "signalbox_application::start_eligible_turn",
            "turn activated",
        ) => {
            values.has_exact(&["message", "session_id", "turn_id"])
                && values.uuid("session_id")
                && values.uuid("turn_id")
        }
        (
            "signalbox_application::model_execution" | "signalbox_application::tool_loop",
            "turn terminalized",
        ) => {
            values.has_exact(&["message", "session_id", "terminal_outcome", "turn_id"])
                && values.uuid("session_id")
                && values.uuid("turn_id")
                && values.closed("terminal_outcome", TURN_OUTCOMES)
        }
        ("signalbox_application::model_execution", "turn parked awaiting owner reconciliation") => {
            values.has_exact(&["message", "session_id", "turn_id"])
                && values.uuid("session_id")
                && values.uuid("turn_id")
        }
        ("signalbox_model_provider_runtime", "model call dispatched") => {
            values.has_exact(&[
                "message",
                "model_call_id",
                "session_id",
                "turn_attempt_id",
                "turn_id",
            ]) && values.uuid("session_id")
                && values.uuid("turn_id")
                && values.uuid("model_call_id")
                && values.uuid("turn_attempt_id")
        }
        (
            "signalbox_model_provider_runtime",
            "model runtime reported a trustworthy capability-preparation failure",
        )
        | ("signalbox_model_provider_runtime", "model call completed")
        | ("signalbox_model_provider_runtime", "model call produced no assistant material") => {
            values.has_exact(&[
                "cause_code",
                "message",
                "model_call_id",
                "session_id",
                "turn_id",
            ]) && values.uuid("session_id")
                && values.uuid("turn_id")
                && values.uuid("model_call_id")
                && values.closed("cause_code", MODEL_CAUSE_CODES)
        }
        _ => false,
    }
}

#[derive(Default)]
struct RecordedValues {
    values: BTreeMap<String, String>,
}

impl RecordedValues {
    fn uuid_attributes(&self, names: &[&str]) -> Option<Vec<KeyValue>> {
        if !self.has_exact(names) {
            return None;
        }
        names
            .iter()
            .map(|name| {
                let value = self.get(name)?.trim_matches('"');
                let value = uuid::Uuid::parse_str(value).ok()?.to_string();
                Some(KeyValue::new((*name).to_owned(), value))
            })
            .collect()
    }

    fn otel_attributes(&self) -> Option<Vec<KeyValue>> {
        self.values
            .iter()
            .filter(|(name, _value)| name.as_str() != "message")
            .map(|(name, value)| {
                let value = value.trim_matches('"');
                let value = if name.ends_with("_id") {
                    uuid::Uuid::parse_str(value).ok()?.to_string()
                } else {
                    value.to_owned()
                };
                Some(KeyValue::new(name.clone(), value))
            })
            .collect()
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn has_exact(&self, names: &[&str]) -> bool {
        self.values.len() == names.len() && names.iter().all(|name| self.values.contains_key(*name))
    }

    fn uuid(&self, name: &str) -> bool {
        self.get(name)
            .map(|value| uuid::Uuid::parse_str(value.trim_matches('"')).is_ok())
            .unwrap_or(false)
    }

    fn closed(&self, name: &str, admitted: &[&str]) -> bool {
        self.get(name)
            .map(|value| admitted.contains(&value.trim_matches('"')))
            .unwrap_or(false)
    }

    fn record(&mut self, field: &Field, value: String) {
        self.values.insert(field.name().to_owned(), value);
    }
}

impl Visit for RecordedValues {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

const TURN_OUTCOMES: &[&str] = &[
    "completed",
    "failed",
    "refused",
    "cancelled",
    "cancelled_with_tool_response",
    "target_unavailable",
    "capability_known_failure",
    "continuation_target_unavailable",
];

const MODEL_CAUSE_CODES: &[&str] = &[
    "completed",
    "provider_refused",
    "provider_credential_rejected",
    "provider_permission_denied",
    "provider_invalid_request",
    "provider_target_not_found",
    "provider_request_too_large",
    "provider_rate_limited",
    "provider_quota_exhausted",
    "provider_overloaded",
    "provider_internal",
    "provider_unrecognized_error",
    "provider_cancellation_confirmed",
    "cancelled_before_send",
    "connect_failed",
    "send_incomplete_proven_unacceptable",
    "boundary_loss_cancellation_requested",
    "boundary_loss_timed_out",
    "boundary_loss_transport_failed",
    "boundary_loss_response_body_lost",
    "boundary_loss_response_unintelligible",
    "boundary_loss_unexpected_http_status",
    "boundary_loss_stream_incomplete",
    "boundary_loss_stream_protocol_violation",
    "unsupported_operation",
    "credential_unmapped",
    "credential_unavailable",
    "credential_unreadable",
    "credential_unusable",
    "provider_target_substituted",
    "unrepresentable_tool_material",
    "finish_contradicts_content",
    "unconfigured_target",
    "preparation_defect",
    "correlation_mismatch",
    "authorization_mismatch",
    "observation_correlation_mismatch",
    "unsupported_completion_material",
    "invalid_assistant_text",
    "invalid_tool_schema",
    "invalid_tool_proposal",
];

/// Closed turn outcome accepted by the Prometheus registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnMetricOutcome {
    Completed,
    Failed,
    Refused,
    Cancelled,
    ReconciliationRequired,
}

/// Closed durable model-call disposition accepted by the Prometheus registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelMetricDisposition {
    Completed,
    KnownFailed,
    Refused,
    Cancelled,
    Ambiguous,
}

/// Private, bounded-cardinality Prometheus registry.
#[derive(Clone, Debug)]
pub struct TelemetryMetrics {
    registry: Arc<Registry>,
    turns_started: IntCounter,
    turns_completed: IntCounter,
    turns_failed: IntCounter,
    turns_refused: IntCounter,
    turns_cancelled: IntCounter,
    turns_reconciliation_required: IntCounter,
    model_completed: IntCounter,
    model_known_failed: IntCounter,
    model_refused: IntCounter,
    model_cancelled: IntCounter,
    model_ambiguous: IntCounter,
}

impl TelemetryMetrics {
    /// Builds every bounded label series before the daemon begins work.
    pub fn new() -> Result<Self, TelemetryConfigurationError> {
        let registry = Registry::new();
        let turns_started = IntCounter::with_opts(Opts::new(
            "signalbox_turns_started_total",
            "Durably activated turns observed by the daemon outbox.",
        ))
        .map_err(|_| metrics_error())?;
        let turn_terminal = IntCounterVec::new(
            Opts::new(
                "signalbox_turns_terminalized_total",
                "Durably terminalized turns by closed lifecycle outcome.",
            ),
            &["outcome"],
        )
        .map_err(|_| metrics_error())?;
        let model_terminal = IntCounterVec::new(
            Opts::new(
                "signalbox_model_calls_terminalized_total",
                "Durably terminalized model calls by closed disposition.",
            ),
            &["disposition"],
        )
        .map_err(|_| metrics_error())?;
        registry
            .register(Box::new(turns_started.clone()))
            .map_err(|_| metrics_error())?;
        registry
            .register(Box::new(turn_terminal.clone()))
            .map_err(|_| metrics_error())?;
        registry
            .register(Box::new(model_terminal.clone()))
            .map_err(|_| metrics_error())?;
        let turns_completed = metric_child(&turn_terminal, "completed")?;
        let turns_failed = metric_child(&turn_terminal, "failed")?;
        let turns_refused = metric_child(&turn_terminal, "refused")?;
        let turns_cancelled = metric_child(&turn_terminal, "cancelled")?;
        let turns_reconciliation_required =
            metric_child(&turn_terminal, "reconciliation_required")?;
        let model_completed = metric_child(&model_terminal, "completed")?;
        let model_known_failed = metric_child(&model_terminal, "known_failed")?;
        let model_refused = metric_child(&model_terminal, "refused")?;
        let model_cancelled = metric_child(&model_terminal, "cancelled")?;
        let model_ambiguous = metric_child(&model_terminal, "ambiguous")?;
        Ok(Self {
            registry: Arc::new(registry),
            turns_started,
            turns_completed,
            turns_failed,
            turns_refused,
            turns_cancelled,
            turns_reconciliation_required,
            model_completed,
            model_known_failed,
            model_refused,
            model_cancelled,
            model_ambiguous,
        })
    }

    pub(crate) fn observe_turn_started(&self) {
        self.turns_started.inc();
    }

    pub(crate) fn observe_turn_terminal(&self, outcome: TurnMetricOutcome) {
        match outcome {
            TurnMetricOutcome::Completed => self.turns_completed.inc(),
            TurnMetricOutcome::Failed => self.turns_failed.inc(),
            TurnMetricOutcome::Refused => self.turns_refused.inc(),
            TurnMetricOutcome::Cancelled => self.turns_cancelled.inc(),
            TurnMetricOutcome::ReconciliationRequired => {
                self.turns_reconciliation_required.inc();
            }
        }
    }

    pub(crate) fn observe_model_terminal(&self, disposition: ModelMetricDisposition) {
        match disposition {
            ModelMetricDisposition::Completed => self.model_completed.inc(),
            ModelMetricDisposition::KnownFailed => self.model_known_failed.inc(),
            ModelMetricDisposition::Refused => self.model_refused.inc(),
            ModelMetricDisposition::Cancelled => self.model_cancelled.inc(),
            ModelMetricDisposition::Ambiguous => self.model_ambiguous.inc(),
        }
    }

    pub(crate) fn render(&self) -> Result<String, prometheus::Error> {
        TextEncoder::new().encode_to_string(&self.registry.gather())
    }
}

fn metrics_error() -> TelemetryConfigurationError {
    TelemetryConfigurationError::new(
        PROMETHEUS_BIND_ENVIRONMENT,
        TelemetryConfigurationFailure::MetricsConstruction,
    )
}

fn metric_child(
    family: &IntCounterVec,
    value: &str,
) -> Result<IntCounter, TelemetryConfigurationError> {
    family
        .get_metric_with_label_values(&[value])
        .map_err(|_| metrics_error())
}

/// Separate HTTP scrape listener; dropping it cancels its bounded server task.
#[derive(Debug)]
pub struct PrometheusServer {
    task: JoinHandle<()>,
}

impl PrometheusServer {
    /// Binds only the explicitly configured IP socket and starts one-at-a-time scrapes.
    pub async fn bind(address: SocketAddr, metrics: TelemetryMetrics) -> io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        let task = tokio::spawn(serve_prometheus(listener, metrics));
        Ok(Self { task })
    }
}

impl Drop for PrometheusServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_prometheus(listener: TcpListener, metrics: TelemetryMetrics) {
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            tracing::warn!(
                target: TELEMETRY_INTERNAL_TARGET,
                cause_code = "prometheus_accept_failed",
                "Prometheus scrape listener stopped after an accept failure"
            );
            return;
        };
        if timeout(
            SCRAPE_CONNECTION_TIMEOUT,
            serve_prometheus_connection(stream, &metrics),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                target: TELEMETRY_INTERNAL_TARGET,
                cause_code = "prometheus_scrape_timed_out",
                "Prometheus scrape connection exceeded its time bound"
            );
        }
    }
}

async fn serve_prometheus_connection(
    mut stream: TcpStream,
    metrics: &TelemetryMetrics,
) -> io::Result<()> {
    let mut request = [0_u8; MAX_SCRAPE_REQUEST_BYTES];
    let mut received = 0;
    let mut complete = false;
    while received < request.len() {
        let read = stream.read(&mut request[received..]).await?;
        if read == 0 {
            break;
        }
        received += read;
        complete = request[..received]
            .windows(4)
            .any(|window| window == b"\r\n\r\n");
        if complete {
            break;
        }
    }
    let request = &request[..received];
    let response = if complete
        && (request.starts_with(b"GET /metrics HTTP/1.0\r\n")
            || request.starts_with(b"GET /metrics HTTP/1.1\r\n"))
    {
        match metrics.render() {
            Ok(body) => http_response("200 OK", "text/plain; version=0.0.4; charset=utf-8", &body),
            Err(_) => http_response(
                "500 Internal Server Error",
                "text/plain; charset=utf-8",
                "metrics unavailable\n",
            ),
        }
    } else {
        http_response("404 Not Found", "text/plain; charset=utf-8", "not found\n")
    };
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, net::SocketAddr};

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::{
        error::OTelSdkError,
        trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing_subscriber::prelude::*;

    use super::{
        IsolatedSpanExporter, ModelMetricDisposition, SdkSpanExporter, TelemetryConfiguration,
        TelemetryEnvironment, TelemetryExportLayer, TelemetryMetrics, TurnMetricOutcome,
        serve_prometheus,
    };

    const SESSION_ID: &str = "018f52a2-26ca-7a89-bb89-d601f688e5d8";
    const TURN_ID: &str = "018f52a2-26ca-7a89-bb89-d601f688e5d9";
    const MODEL_CALL_ID: &str = "018f52a2-26ca-7a89-bb89-d601f688e5da";
    const SYNTHETIC_CREDENTIAL: &str = "sk-synthetic-not-a-real-credential";
    const SYNTHETIC_CONTENT: &str =
        "synthetic prompt completion and tool arguments: delete_everything=true";

    fn capture_spans(emit: impl FnOnce()) -> Vec<SpanData> {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let layer = TelemetryExportLayer::new(provider.tracer("signalboxd-test"));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, emit);
        provider.force_flush().expect("test exporter flushes");
        exporter
            .get_finished_spans()
            .expect("test exporter retains finished spans")
    }

    fn names(span: &SpanData) -> Vec<String> {
        let mut names = span
            .attributes
            .iter()
            .map(|attribute| attribute.key.as_str().to_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn event_names(span: &SpanData) -> Vec<String> {
        let mut names = span.events[0]
            .attributes
            .iter()
            .map(|attribute| attribute.key.as_str().to_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn disabled_environment() -> TelemetryEnvironment {
        TelemetryEnvironment {
            endpoint: None,
            protocol: None,
            headers_file: None,
            sampling_ratio: None,
            service_name: None,
            prometheus_bind: None,
            ambient_otlp_setting: None,
        }
    }

    #[derive(Debug)]
    struct FailingExporter;

    impl SdkSpanExporter for FailingExporter {
        fn export(
            &self,
            _batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = Result<(), OTelSdkError>> + Send {
            std::future::ready(Err(OTelSdkError::InternalFailure(
                SYNTHETIC_CONTENT.to_owned(),
            )))
        }
    }

    #[test]
    fn credential_and_content_shaped_values_do_not_reach_exported_records() {
        let spans = capture_spans(|| {
            let span = tracing::info_span!(
                target: "signalboxd::context_guard",
                "turn_work",
                session_id = %SESSION_ID,
                turn_id = %TURN_ID,
            );
            let _guard = span.enter();
            tracing::info!(
                target: "signalbox_application::model_execution",
                session_id = %SESSION_ID,
                turn_id = %TURN_ID,
                terminal_outcome = SYNTHETIC_CREDENTIAL,
                "turn terminalized"
            );
            tracing::warn!(
                target: "signalbox_model_provider_runtime",
                cause_code = SYNTHETIC_CONTENT,
                session_id = %SESSION_ID,
                turn_id = %TURN_ID,
                model_call_id = %MODEL_CALL_ID,
                "model call produced no assistant material"
            );
            tracing::error!(
                target: "signalboxd::context_guard",
                error = SYNTHETIC_CONTENT,
                "synthetic unsafe event"
            );
            let rejected_values = tracing::info_span!(
                target: "signalboxd::context_guard",
                "turn_work",
                session_id = %SYNTHETIC_CREDENTIAL,
                turn_id = %SYNTHETIC_CONTENT,
            );
            let _rejected_value_guard = rejected_values.enter();
            let rejected_span = tracing::info_span!(
                target: "signalboxd::context_guard",
                "turn_work",
                session_id = %SESSION_ID,
                turn_id = %TURN_ID,
                prompt = SYNTHETIC_CONTENT,
            );
            let _rejected_guard = rejected_span.enter();
        });
        let exported = format!("{spans:?}");

        assert_eq!(spans.len(), 1);
        assert!(spans[0].events.is_empty());
        assert!(!exported.contains(SYNTHETIC_CREDENTIAL));
        assert!(!exported.contains(SYNTHETIC_CONTENT));
    }

    #[test]
    fn session_span_exports_only_its_daemon_minted_identifier() {
        let spans = capture_spans(|| {
            let span = tracing::info_span!(
                target: "signalbox_application::scheduler",
                "session_work",
                session_id = %SESSION_ID,
            );
            let _guard = span.enter();
        });

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "session_work");
        assert_eq!(names(&spans[0]), vec!["session_id"]);
        assert!(spans[0].events.is_empty());
    }

    #[test]
    fn turn_span_preserves_the_exported_session_parent() {
        let spans = capture_spans(|| {
            let session = tracing::info_span!(
                target: "signalbox_application::scheduler",
                "session_work",
                session_id = %SESSION_ID,
            );
            let _session_guard = session.enter();
            let turn = tracing::info_span!(
                target: "signalboxd::context_guard",
                "turn_work",
                session_id = %SESSION_ID,
                turn_id = %TURN_ID,
            );
            let _turn_guard = turn.enter();
        });

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "turn_work");
        assert_eq!(spans[1].name, "session_work");
        assert_eq!(spans[0].parent_span_id, spans[1].span_context.span_id());
    }

    #[test]
    fn admitted_span_and_event_export_only_the_documented_fields() {
        let spans = capture_spans(|| {
            let span = tracing::info_span!(
                target: "signalboxd::context_guard",
                "turn_work",
                session_id = %SESSION_ID,
                turn_id = %TURN_ID,
            );
            let _guard = span.enter();
            tracing::info!(
                target: "signalbox_application::model_execution",
                session_id = %SESSION_ID,
                turn_id = %TURN_ID,
                terminal_outcome = "completed",
                "turn terminalized"
            );
        });

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "turn_work");
        assert_eq!(names(&spans[0]), vec!["session_id", "turn_id"]);
        assert_eq!(spans[0].events.len(), 1);
        assert_eq!(spans[0].events[0].name, "turn terminalized");
        assert_eq!(
            event_names(&spans[0]),
            vec![
                "level",
                "session_id",
                "target",
                "terminal_outcome",
                "turn_id"
            ]
        );
    }

    #[test]
    fn absent_endpoint_ignores_otlp_secondary_settings_and_builds_nothing() {
        let mut environment = disabled_environment();
        environment.protocol = Some(OsString::from("synthetic-invalid-protocol"));
        environment.headers_file = Some(OsString::from("synthetic-missing-header-file"));
        environment.sampling_ratio = Some(OsString::from("not-a-ratio"));
        environment.service_name = Some(OsString::from(SYNTHETIC_CREDENTIAL));

        let configuration = TelemetryConfiguration::from_values(environment)
            .expect("absent endpoint keeps OTLP disabled");

        assert!(configuration.otlp.is_none());
        assert!(configuration.prometheus_bind().is_none());
        assert!(
            configuration
                .build_otlp_runtime()
                .expect("disabled OTLP constructs no exporter")
                .is_none()
        );
    }

    #[test]
    fn http_protobuf_https_exporter_constructs_without_connecting() {
        let mut environment = disabled_environment();
        environment.endpoint = Some(OsString::from("https://127.0.0.1:4318"));
        environment.protocol = Some(OsString::from("http/protobuf"));
        let configuration = TelemetryConfiguration::from_values(environment)
            .expect("HTTPS HTTP configuration is admitted");
        let runtime = configuration
            .build_otlp_runtime()
            .expect("HTTPS HTTP exporter constructs")
            .expect("endpoint enables exporter");

        runtime.shutdown();
    }

    #[test]
    fn resource_service_name_rejects_credential_shaped_free_text() {
        let mut environment = disabled_environment();
        environment.endpoint = Some(OsString::from("http://127.0.0.1:4317"));
        environment.service_name =
            Some(OsString::from(format!("signalboxd.{SYNTHETIC_CREDENTIAL}")));

        let error = TelemetryConfiguration::from_values(environment)
            .err()
            .expect("credential-shaped service name is rejected");
        let displayed = error.to_string();

        assert!(!displayed.contains(SYNTHETIC_CREDENTIAL));
        assert!(displayed.contains("SIGNALBOX_OTLP_SERVICE_NAME"));
    }

    #[test]
    fn resource_contains_only_the_closed_service_name() {
        let resource = super::telemetry_resource("signalboxd.production");
        let attributes = resource
            .iter()
            .map(|(key, value)| (key.as_str().to_owned(), value.to_string()))
            .collect::<Vec<_>>();

        assert_eq!(
            attributes,
            vec![(
                "service.name".to_owned(),
                "signalboxd.production".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn exporter_failure_is_logged_and_reduced_to_success() {
        let exporter = IsolatedSpanExporter {
            inner: FailingExporter,
        };

        let result = exporter.export(Vec::new()).await;

        assert!(result.is_ok());
    }

    #[test]
    fn enabled_otlp_refuses_ambient_standard_header_channels() {
        let mut environment = disabled_environment();
        environment.endpoint = Some(OsString::from("http://127.0.0.1:4317"));
        environment.ambient_otlp_setting = Some("OTEL_EXPORTER_OTLP_HEADERS");

        let error = TelemetryConfiguration::from_values(environment)
            .err()
            .expect("ambient header channel is rejected");
        let displayed = error.to_string();

        assert!(displayed.contains("OTEL_EXPORTER_OTLP_HEADERS"));
        assert!(!displayed.contains(SYNTHETIC_CREDENTIAL));
    }

    #[test]
    fn configuration_errors_never_display_rejected_values() {
        let mut environment = disabled_environment();
        environment.endpoint = Some(OsString::from(SYNTHETIC_CREDENTIAL));

        let error = TelemetryConfiguration::from_values(environment)
            .err()
            .expect("credential-shaped endpoint is rejected");
        let displayed = error.to_string();

        assert!(!displayed.contains(SYNTHETIC_CREDENTIAL));
        assert!(displayed.contains("SIGNALBOX_OTLP_ENDPOINT"));
    }

    #[test]
    fn prometheus_registry_has_only_closed_labels_and_no_value_input_surface() {
        let metrics = TelemetryMetrics::new().expect("static metric descriptors are valid");
        metrics.observe_turn_started();
        metrics.observe_turn_terminal(TurnMetricOutcome::Completed);
        metrics.observe_turn_terminal(TurnMetricOutcome::Failed);
        metrics.observe_turn_terminal(TurnMetricOutcome::Refused);
        metrics.observe_turn_terminal(TurnMetricOutcome::Cancelled);
        metrics.observe_turn_terminal(TurnMetricOutcome::ReconciliationRequired);
        metrics.observe_model_terminal(ModelMetricDisposition::Completed);
        metrics.observe_model_terminal(ModelMetricDisposition::KnownFailed);
        metrics.observe_model_terminal(ModelMetricDisposition::Refused);
        metrics.observe_model_terminal(ModelMetricDisposition::Cancelled);
        metrics.observe_model_terminal(ModelMetricDisposition::Ambiguous);
        let rendered = metrics.render().expect("static registry encodes");

        assert!(rendered.contains("signalbox_turns_started_total 1"));
        assert!(rendered.contains("outcome=\"reconciliation_required\""));
        assert!(rendered.contains("disposition=\"ambiguous\""));
        assert!(!rendered.contains("session_id"));
        assert!(!rendered.contains("turn_id"));
        assert!(!rendered.contains("model_call_id"));
        assert!(!rendered.contains(SYNTHETIC_CREDENTIAL));
        assert!(!rendered.contains(SYNTHETIC_CONTENT));
    }

    #[tokio::test]
    async fn prometheus_serves_metrics_only_on_the_separate_http_listener() {
        let metrics = TelemetryMetrics::new().expect("static metric descriptors are valid");
        metrics.observe_turn_started();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener binds");
        let address: SocketAddr = listener.local_addr().expect("listener has an address");
        let server = tokio::spawn(serve_prometheus(listener, metrics));
        let mut stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("test client connects");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\n")
            .await
            .expect("test request head writes");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        stream
            .write_all(b"Host: localhost\r\n\r\n")
            .await
            .expect("test request tail writes");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("test response reads");
        server.abort();

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("signalbox_turns_started_total 1"));
    }
}
