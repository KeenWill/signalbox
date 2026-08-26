//! Bounded daemon-local single-URL web fetch.

use std::{collections::BTreeSet, error::Error, fmt, future::Future, time::Duration};

use futures_util::StreamExt;
use reqwest::{Client, Url};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};

use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

use signalbox_egress_transport::{
    ReqwestWebFetchConstructionError, WebFetchTransportFailure, build_web_fetch_client,
    has_more_response_bytes, is_public_destination_address, parse_url_host_ip,
    public_destination_client,
};

pub const WEB_FETCH_NAME: &str = "web_fetch";
const INVALID_ARGUMENTS_DETAIL: &str =
    "expected one absolute HTTP(S) URL without user information or a fragment";
const REQUEST_FAILED_DETAIL: &str = "web fetch request failed";
const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_CONTENT_TYPE_BYTES: usize = 1024;
const MAX_ALLOWED_ORIGINS: usize = 64;
pub(crate) const MAX_WEB_FETCH_BODY_BYTES: usize = 64 * 1024;

/// Deployment-owned exact origins to which `web_fetch` may automatically
/// egress.
///
/// An empty policy disables physical web fetches. Each admitted origin is
/// canonicalized to its scheme, host, and effective port; paths and query
/// strings remain request data and do not broaden the destination set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebFetchEgressPolicy {
    allowed_origins: BTreeSet<WebFetchOrigin>,
}

impl WebFetchEgressPolicy {
    /// Constructs a fail-closed policy with no admitted egress destination.
    pub const fn deny_all() -> Self {
        Self {
            allowed_origins: BTreeSet::new(),
        }
    }

    /// Validates a bounded set of exact HTTP(S) origins.
    pub fn try_from_allowed_origins(
        origins: impl IntoIterator<Item = String>,
    ) -> Result<Self, WebFetchEgressPolicyError> {
        let mut allowed_origins = BTreeSet::new();
        for supplied in origins {
            if allowed_origins.len() == MAX_ALLOWED_ORIGINS {
                return Err(WebFetchEgressPolicyError::TooManyOrigins);
            }
            let origin = WebFetchOrigin::try_from_configuration(&supplied)?;
            if !allowed_origins.insert(origin) {
                return Err(WebFetchEgressPolicyError::DuplicateOrigin);
            }
        }
        Ok(Self { allowed_origins })
    }

    fn admits(&self, url: &Url) -> bool {
        WebFetchOrigin::from_url(url).is_some_and(|origin| self.allowed_origins.contains(&origin))
    }
}

impl Default for WebFetchEgressPolicy {
    fn default() -> Self {
        Self::deny_all()
    }
}

/// Why a deployment web-fetch egress policy was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebFetchEgressPolicyError {
    /// More than 64 exact origins were supplied.
    TooManyOrigins,
    /// Two entries canonicalized to the same exact origin.
    DuplicateOrigin,
    /// An entry was not one bare absolute HTTP(S) origin.
    InvalidOrigin,
}

impl fmt::Display for WebFetchEgressPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyOrigins => "web_fetch egress policy contains too many origins",
            Self::DuplicateOrigin => "web_fetch egress policy repeats an origin",
            Self::InvalidOrigin => "web_fetch egress policy contains an invalid origin",
        })
    }
}

impl Error for WebFetchEgressPolicyError {}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct WebFetchOrigin {
    scheme: String,
    host: String,
    port: u16,
}

impl WebFetchOrigin {
    fn try_from_configuration(supplied: &str) -> Result<Self, WebFetchEgressPolicyError> {
        let url = Url::parse(supplied).map_err(|_| WebFetchEgressPolicyError::InvalidOrigin)?;
        if url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(WebFetchEgressPolicyError::InvalidOrigin);
        }
        Self::from_url(&url).ok_or(WebFetchEgressPolicyError::InvalidOrigin)
    }

    fn from_url(url: &Url) -> Option<Self> {
        matches!(url.scheme(), "http" | "https").then_some(())?;
        Some(Self {
            scheme: url.scheme().to_owned(),
            host: url.host_str()?.to_owned(),
            port: url.port_or_known_default()?,
        })
    }
}

/// A static `web_fetch` declaration or production transport could not be
/// constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebFetchToolConstructionError {
    /// The static name was rejected.
    Name,
    /// The static schema was rejected.
    Schema,
    /// One static sanitized error detail was rejected.
    ErrorDetail,
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
    /// The HTTP client could not be constructed.
    Transport,
}

impl fmt::Display for WebFetchToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "web_fetch static name is invalid",
            Self::Schema => "web_fetch static schema is invalid",
            Self::ErrorDetail => "web_fetch static error detail is invalid",
            Self::Duplicate => "web_fetch catalog is duplicated",
            Self::Transport => "web_fetch transport could not be constructed",
        })
    }
}

impl Error for WebFetchToolConstructionError {}

/// Compiled catalog entry and matching executor for `web_fetch`.
///
/// Effect posture: `ExternalEffect`. Although the method is GET, the remote
/// server can observe the request, so a crash-lost dispatch is not effect-free.
#[derive(Clone, Debug)]
pub struct WebFetchTool<Transport> {
    catalog: CompiledToolCatalog,
    executor: WebFetchExecutor<Transport>,
}

impl<Transport> ToolContract for WebFetchTool<Transport> {
    type Arguments = WebFetchArguments;
    const NAME: &'static str = WEB_FETCH_NAME;
    const DESCRIPTION: &'static str =
        "Fetches one HTTP(S) URL without credentials, redirects, proxies, or retries.";
}

/// Typed `web_fetch` argument shape; decoder and rendered schema share it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebFetchArguments {
    /// Absolute HTTP(S) URL without user information or a fragment.
    url: WebFetchUrl,
}

/// One admitted fetch destination.
///
/// Admission enforces what the transport requires: an absolute HTTP(S) URL of
/// at most [`MAX_URL_BYTES`] bytes as supplied and as serialized, with a
/// host, without user information or a fragment, and — when the host is an IP
/// literal — a public destination address.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
struct WebFetchUrl(Url);

impl schemars::JsonSchema for WebFetchUrl {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("WebFetchUrl")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // `maxLength` counts code points, so it is the tightest sound schema
        // statement of the decoder's byte cap: every string it excludes also
        // exceeds the byte cap.
        schemars::json_schema!({
            "type": "string",
            "format": "uri",
            "maxLength": MAX_URL_BYTES,
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

impl TryFrom<String> for WebFetchUrl {
    type Error = InvalidWebFetchArguments;

    fn try_from(supplied: String) -> Result<Self, Self::Error> {
        if supplied.len() > MAX_URL_BYTES {
            return Err(InvalidWebFetchArguments);
        }
        let url = Url::parse(&supplied).map_err(|_| InvalidWebFetchArguments)?;
        if url.as_str().len() > MAX_URL_BYTES
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(InvalidWebFetchArguments);
        }
        if url
            .host_str()
            .and_then(parse_url_host_ip)
            .is_some_and(|address| !is_public_destination_address(address))
        {
            return Err(InvalidWebFetchArguments);
        }
        Ok(Self(url))
    }
}

impl WebFetchTool<ReqwestWebFetchTransport> {
    /// Builds the production tool with the fixed bounded transport policy.
    pub fn try_new_production(
        egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, WebFetchToolConstructionError> {
        let transport = ReqwestWebFetchTransport::try_new(DEFAULT_EXCHANGE_TIMEOUT)
            .map_err(|_| WebFetchToolConstructionError::Transport)?;
        Self::try_new(transport, egress_policy)
    }
}

impl<Transport> WebFetchTool<Transport> {
    /// Compiles immutable metadata around one injected transport.
    pub fn try_new(
        transport: Transport,
        egress_policy: WebFetchEgressPolicy,
    ) -> Result<Self, WebFetchToolConstructionError> {
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| WebFetchToolConstructionError::ErrorDetail)?;
        let request_failed_detail =
            ToolExecutionErrorDetail::try_new(String::from(REQUEST_FAILED_DETAIL))
                .map_err(|_| WebFetchToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<Self>(
            ToolPermissionDefault::Confirm,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => WebFetchToolConstructionError::Name,
            ToolContractCompileError::Schema => WebFetchToolConstructionError::Schema,
        })?;
        let compiled = CompiledTool::new(
            definition,
            WebFetchArgumentValidator {
                detail: invalid_arguments_detail,
                egress_policy: egress_policy.clone(),
            },
        );
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| WebFetchToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: WebFetchExecutor {
                transport,
                request_failed_detail,
                egress_policy,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, WebFetchExecutor<Transport>) {
        (self.catalog, self.executor)
    }
}

#[derive(Clone, Debug)]
struct WebFetchArgumentValidator {
    detail: ToolExecutionErrorDetail,
    egress_policy: WebFetchEgressPolicy,
}

impl ToolArgumentValidator for WebFetchArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_admitted_arguments(arguments, &self.egress_policy)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// One checked, credential-free fetch request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebFetchRequest {
    url: Url,
}

impl WebFetchRequest {
    /// Borrows the checked absolute URL.
    pub fn url(&self) -> &Url {
        &self.url
    }
}

/// One bounded response returned by a web transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebFetchResponse {
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
    completeness: WebFetchBodyCompleteness,
}

/// Whether the bounded body contains the complete response entity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebFetchBodyCompleteness {
    /// End-of-stream arrived within the body cap.
    Complete,
    /// More response bytes existed beyond the retained prefix.
    Truncated,
}

impl WebFetchResponse {
    /// Constructs response facts already bounded by the transport.
    pub fn new(
        status: u16,
        content_type: Option<String>,
        body: Vec<u8>,
        completeness: WebFetchBodyCompleteness,
    ) -> Option<Self> {
        (body.len() <= MAX_WEB_FETCH_BODY_BYTES
            && content_type
                .as_ref()
                .is_none_or(|value| value.len() <= MAX_CONTENT_TYPE_BYTES))
        .then_some(Self {
            status,
            content_type,
            body,
            completeness,
        })
    }
}

/// Injectable one-request web transport.
pub trait WebFetchTransport: Send {
    /// Performs exactly one checked request without credential lookup.
    fn fetch(
        &mut self,
        request: WebFetchRequest,
    ) -> impl Future<Output = Result<WebFetchResponse, WebFetchTransportFailure>> + Send;
}

/// Production reqwest transport with no ambient proxy, redirect, or retry.
#[derive(Clone, Debug)]
pub struct ReqwestWebFetchTransport {
    exchange_timeout: Duration,
}

impl ReqwestWebFetchTransport {
    /// Builds the transport with a positive whole-exchange timeout.
    pub fn try_new(exchange_timeout: Duration) -> Result<Self, ReqwestWebFetchConstructionError> {
        if exchange_timeout.is_zero() {
            return Err(ReqwestWebFetchConstructionError);
        }
        build_web_fetch_client(Some(exchange_timeout), None)?;
        Ok(Self { exchange_timeout })
    }
}

impl WebFetchTransport for ReqwestWebFetchTransport {
    async fn fetch(
        &mut self,
        request: WebFetchRequest,
    ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
        let client = public_destination_client(request.url(), Some(self.exchange_timeout))
            .await
            .map_err(|_| WebFetchTransportFailure::RequestFailed)?;
        fetch_with_client(client, request).await
    }
}

async fn fetch_with_client(
    client: Client,
    request: WebFetchRequest,
) -> Result<WebFetchResponse, WebFetchTransportFailure> {
    let response = client
        .get(request.url)
        .send()
        .await
        .map_err(classify_send_failure)?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= MAX_CONTENT_TYPE_BYTES)
        .map(str::to_owned);
    let mut body = Vec::new();
    let mut truncated = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| WebFetchTransportFailure::DispatchUnknown)?;
        let remaining = MAX_WEB_FETCH_BODY_BYTES.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
        if body.len() == MAX_WEB_FETCH_BODY_BYTES {
            truncated = has_more_response_bytes(&mut stream).await?;
            break;
        }
    }
    let completeness = if truncated {
        WebFetchBodyCompleteness::Truncated
    } else {
        WebFetchBodyCompleteness::Complete
    };
    WebFetchResponse::new(status, content_type, body, completeness)
        .ok_or(WebFetchTransportFailure::DispatchUnknown)
}

fn classify_send_failure(error: reqwest::Error) -> WebFetchTransportFailure {
    if error.is_connect() {
        WebFetchTransportFailure::RequestFailed
    } else {
        WebFetchTransportFailure::DispatchUnknown
    }
}

/// Daemon-local bounded web executor.
///
/// Effect posture: `ExternalEffect`. Although the method is GET, the remote
/// server can observe the request; the registry therefore never describes a
/// crash-lost dispatch as effect-free.
#[derive(Clone, Debug)]
pub struct WebFetchExecutor<Transport> {
    transport: Transport,
    request_failed_detail: ToolExecutionErrorDetail,
    egress_policy: WebFetchEgressPolicy,
}

/// A checked catalog/executor assumption failed inside `web_fetch`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebFetchExecutorError {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// Compact result encoding unexpectedly failed.
    ResultEncoding,
    /// Physical dispatch began without a complete bounded acknowledgement.
    DispatchUnknown,
}

impl fmt::Display for WebFetchExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ArgumentValidationDrift => "web_fetch argument validation drifted",
            Self::ResultEncoding => "web_fetch result encoding failed",
            Self::DispatchUnknown => "web_fetch dispatch outcome is unknown",
        })
    }
}

impl Error for WebFetchExecutorError {}

impl ClassifyOperatorFailure for WebFetchExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::ArgumentValidationDrift | Self::ResultEncoding => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::DispatchUnknown => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
        }
    }
}

impl<Transport> ToolExecutor for WebFetchExecutor<Transport>
where
    Transport: WebFetchTransport,
{
    type Error = WebFetchExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let request =
            decode_admitted_arguments(invocation.request().arguments(), &self.egress_policy)
                .map_err(|_| WebFetchExecutorError::ArgumentValidationDrift)?;
        let evidence = match self.transport.fetch(request.clone()).await {
            Ok(response) => web_fetch_success_evidence(&request, response)?,
            Err(WebFetchTransportFailure::RequestFailed) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.request_failed_detail.clone()),
            },
            Err(WebFetchTransportFailure::DispatchUnknown) => {
                return Err(WebFetchExecutorError::DispatchUnknown);
            }
        };
        Ok(invocation.bind(evidence))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidWebFetchArguments;

impl fmt::Display for InvalidWebFetchArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(INVALID_ARGUMENTS_DETAIL)
    }
}

fn decode_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<WebFetchRequest, InvalidWebFetchArguments> {
    let decoded: WebFetchArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidWebFetchArguments)?;
    Ok(WebFetchRequest { url: decoded.url.0 })
}

fn decode_admitted_arguments(
    arguments: &NormalizedToolArguments,
    egress_policy: &WebFetchEgressPolicy,
) -> Result<WebFetchRequest, InvalidWebFetchArguments> {
    let request = decode_arguments(arguments)?;
    if !egress_policy.admits(request.url()) {
        return Err(InvalidWebFetchArguments);
    }
    Ok(request)
}

fn web_fetch_success_evidence(
    request: &WebFetchRequest,
    response: WebFetchResponse,
) -> Result<ToolExecutorEvidence, WebFetchExecutorError> {
    let body = String::from_utf8_lossy(&response.body);
    let result = serde_json::to_string(&serde_json::json!({
        "body": body,
        "content_type": response.content_type,
        "status": response.status,
        "truncated": response.completeness == WebFetchBodyCompleteness::Truncated,
        "url": request.url.as_str(),
    }))
    .map_err(|_| WebFetchExecutorError::ResultEncoding)?;
    Ok(ToolExecutorEvidence::CompletedText(result))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use signalbox_egress_transport::{PublicDestinationClientError, ResolvedPublicDestination};

    const FIXTURE_ORIGIN: &str = "https://example.com";
    const REDIRECT_STATUS: u16 = 302;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn fixture_egress_policy() -> WebFetchEgressPolicy {
        WebFetchEgressPolicy::try_from_allowed_origins([String::from(FIXTURE_ORIGIN)])
            .expect("fixture origin is admitted")
    }

    /// The read-only operation defaults to confirmation because a remote
    /// server observes the GET.
    #[test]
    fn web_fetch_definition_carries_exact_policy() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definitions = catalog.definitions();
        let [definition] = definitions.as_ref() else {
            panic!("web_fetch is the one compiled definition")
        };

        assert_eq!(definition.name().as_str(), WEB_FETCH_NAME);
        assert_eq!(
            definition.permission_default(),
            ToolPermissionDefault::Confirm
        );
        assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
    }

    /// Confirmation does not replace the exact deployment allowlist: an absent
    /// origin remains invalid while an ordinary path at an admitted origin is
    /// valid.
    #[test]
    fn web_fetch_confirm_permission_retains_the_configured_origin_bound() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        let admitted = serde_json::json!({
            "url": format!("{FIXTURE_ORIGIN}/ordinary"),
        })
        .to_string();

        assert_eq!(
            definition.permission_default(),
            ToolPermissionDefault::Confirm
        );
        assert_eq!(
            catalog.validate_arguments(definition.name(), &arguments(&admitted)),
            Ok(())
        );
        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"url":"https://collector.example/encoded-secret"}"#),
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// The complete rendered wire schema. The pretty golden is the review
    /// surface; the byte-exact assertion pins the canonical compact form the
    /// registry stores and providers receive as its exact serialization. The
    /// `format` and `maxLength` members state constraints the decoder already
    /// enforced while the earlier literal declared a bare string.
    #[test]
    fn web_fetch_rendered_schema_is_the_exact_wire_artifact() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];
        let schema: serde_json::Value = serde_json::from_str(definition.input_schema().as_str())
            .expect("registry schema is valid JSON");

        expect_test::expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "url": {
                  "description": "Absolute HTTP(S) URL without user information or a fragment.",
                  "format": "uri",
                  "maxLength": 8192,
                  "type": "string"
                }
              },
              "required": [
                "url"
              ],
              "type": "object"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
        assert_eq!(definition.input_schema().as_str(), schema.to_string());
    }

    /// Typed decoding accepts one absolute credential-free URL.
    #[test]
    fn web_fetch_typed_decode_accepts_https_url() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        let supplied = serde_json::json!({
            "url": format!("{FIXTURE_ORIGIN}/path?q=one"),
        })
        .to_string();

        assert_eq!(
            catalog.validate_arguments(definition.name(), &arguments(&supplied)),
            Ok(())
        );
    }

    /// Typed decoding rejects URL-embedded credentials.
    #[test]
    fn web_fetch_typed_decode_rejects_user_information() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"url":"https://user:secret@example.com/"}"#),
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// Typed decoding applies the URL bound to the final serialized URL, so
    /// percent encoding cannot expand a short supplied value past the cap.
    #[test]
    fn web_fetch_typed_decode_rejects_percent_encoded_url_over_cap() {
        let supplied_url = format!("{FIXTURE_ORIGIN}/{}", "\u{00e9}".repeat(MAX_URL_BYTES / 4));
        let supplied = serde_json::json!({"url": supplied_url}).to_string();

        assert!(supplied_url.len() <= MAX_URL_BYTES);
        assert_eq!(
            decode_arguments(&arguments(&supplied)),
            Err(InvalidWebFetchArguments)
        );
    }

    /// Typed decoding rejects a direct loopback destination before execution.
    #[test]
    fn web_fetch_typed_decode_rejects_loopback_ip() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"url":"http://127.0.0.1/private"}"#),
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// Typed decoding rejects a direct private destination before execution.
    #[test]
    fn web_fetch_typed_decode_rejects_private_ip() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"url":"https://10.0.0.1/private"}"#),
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// Typed decoding rejects an IPv6 unique-local destination before
    /// execution.
    #[test]
    fn web_fetch_typed_decode_rejects_unique_local_ipv6() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport, fixture_egress_policy())
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"url":"https://[fd00::1]/private"}"#),
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// Loss after physical dispatch is classified as commit-ambiguous
    /// infrastructure failure.
    #[test]
    fn web_fetch_dispatch_unknown_is_commit_ambiguous() {
        assert_eq!(
            WebFetchExecutorError::DispatchUnknown.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    /// Hostname resolution rejects a destination set containing only loopback
    /// addresses before request dispatch.
    #[tokio::test]
    async fn web_fetch_resolution_rejects_loopback_hostname() {
        let request = WebFetchRequest {
            url: Url::parse("http://localhost/private").expect("fixture URL is valid"),
        };

        let resolution =
            public_destination_client(request.url(), Some(Duration::from_secs(2))).await;

        assert!(matches!(
            resolution,
            Err(PublicDestinationClientError::DestinationRejected)
        ));
    }

    /// A definite connection-establishment failure occurs before request
    /// dispatch and is therefore a sanitized known failure.
    #[tokio::test]
    async fn web_fetch_connection_failure_is_known() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener binds");
        let address = listener
            .local_addr()
            .expect("listener address is available");
        drop(listener);
        let host = "example.test";
        let request = WebFetchRequest {
            url: Url::parse(&format!("http://{host}:{}/", address.port()))
                .expect("fixture URL is valid"),
        };
        let destination = ResolvedPublicDestination {
            host: String::from(host),
            addresses: vec![address],
        };
        let client = build_web_fetch_client(Some(Duration::from_secs(2)), Some(&destination))
            .expect("fixed test client builds");

        let response = fetch_with_client(client, request).await;

        assert_eq!(response, Err(WebFetchTransportFailure::RequestFailed));
    }

    /// Bounded binary input becomes deterministic lossy text plus metadata.
    #[test]
    fn web_fetch_result_is_bounded_text_with_metadata() {
        let expected_url = format!("{FIXTURE_ORIGIN}/data");
        let expected_status = 200;
        let expected_content_type = "text/plain";
        let expected_body = vec![b'o', b'k', 0xff];
        let supplied = serde_json::json!({"url": expected_url.as_str()}).to_string();
        let request = decode_arguments(&arguments(&supplied)).expect("fixture URL is valid");
        let response = WebFetchResponse::new(
            expected_status,
            Some(String::from(expected_content_type)),
            expected_body,
            WebFetchBodyCompleteness::Complete,
        )
        .expect("fixture response is bounded");

        let evidence =
            web_fetch_success_evidence(&request, response).expect("bounded response encodes");
        let expected = serde_json::json!({
            "body": "ok�",
            "content_type": expected_content_type,
            "status": expected_status,
            "truncated": false,
            "url": expected_url.as_str(),
        })
        .to_string();

        assert_eq!(evidence, ToolExecutorEvidence::CompletedText(expected));
    }

    /// The response constructor independently refuses bytes beyond the
    /// transport cap.
    #[test]
    fn web_fetch_response_rejects_body_over_cap() {
        let oversized = vec![b'x'; MAX_WEB_FETCH_BODY_BYTES + 1];

        let response =
            WebFetchResponse::new(200, None, oversized, WebFetchBodyCompleteness::Truncated);

        assert_eq!(response, None);
    }

    /// The production transport sends one credential-free request and exposes
    /// redirect status instead of following the location.
    #[tokio::test]
    async fn web_fetch_transport_does_not_follow_redirect_or_send_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener binds");
        let address = listener
            .local_addr()
            .expect("listener address is available");
        let server = tokio::spawn(serve_one_redirect(listener, address));
        let host = "example.test";
        let request = WebFetchRequest {
            url: Url::parse(&format!("http://{host}:{}/start", address.port()))
                .expect("fixture URL is valid"),
        };
        let destination = ResolvedPublicDestination {
            host: String::from(host),
            addresses: vec![address],
        };
        let client = build_web_fetch_client(Some(Duration::from_secs(2)), Some(&destination))
            .expect("fixed test client builds");

        let response = fetch_with_client(client, request)
            .await
            .expect("redirect headers form a bounded response");
        let observed = server.await.expect("loopback server task completes");

        assert_eq!(response.status, REDIRECT_STATUS);
        assert!(!observed.followed);
        assert!(
            !observed
                .request
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        assert!(!observed.request.to_ascii_lowercase().contains("cookie:"));
    }

    #[derive(Debug)]
    struct RedirectObservation {
        request: String,
        followed: bool,
    }

    async fn serve_one_redirect(
        listener: tokio::net::TcpListener,
        address: std::net::SocketAddr,
    ) -> RedirectObservation {
        let (mut stream, _) = listener.accept().await.expect("one request connects");
        let mut request = vec![0_u8; 4096];
        let length = stream
            .read(&mut request)
            .await
            .expect("request is readable");
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {REDIRECT_STATUS} Found\r\nLocation: http://{address}/followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("redirect response is writable");
        drop(stream);
        let followed = tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_ok();
        RedirectObservation {
            request: String::from_utf8_lossy(&request[..length]).into_owned(),
            followed,
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct FailingTransport;

    impl WebFetchTransport for FailingTransport {
        async fn fetch(
            &mut self,
            _request: WebFetchRequest,
        ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
            Err(WebFetchTransportFailure::RequestFailed)
        }
    }
}
