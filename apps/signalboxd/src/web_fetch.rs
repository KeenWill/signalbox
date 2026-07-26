//! Bounded daemon-local single-URL web fetch.

use std::{
    error::Error,
    fmt,
    future::Future,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::{Client, Url, redirect::Policy};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence, ToolInputSchema,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolName,
    ToolPermissionDefault,
};

pub(crate) const WEB_FETCH_NAME: &str = "web_fetch";
const WEB_FETCH_DESCRIPTION: &str =
    "Fetches one HTTP(S) URL without credentials, redirects, proxies, or retries.";
const WEB_FETCH_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "url": {
            "type": "string",
            "description": "Absolute HTTP(S) URL without user information or a fragment."
        }
    },
    "required": ["url"],
    "additionalProperties": false
}"#;
const INVALID_ARGUMENTS_DETAIL: &str =
    "expected one absolute HTTP(S) URL without user information or a fragment";
const REQUEST_FAILED_DETAIL: &str = "web fetch request failed";
const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_CONTENT_TYPE_BYTES: usize = 1024;
const MAX_RESOLVED_ADDRESSES: usize = 32;
pub(crate) const MAX_WEB_FETCH_BODY_BYTES: usize = 64 * 1024;

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

impl WebFetchTool<ReqwestWebFetchTransport> {
    /// Builds the production tool with the fixed bounded transport policy.
    pub fn try_new_production() -> Result<Self, WebFetchToolConstructionError> {
        let transport = ReqwestWebFetchTransport::try_new(DEFAULT_EXCHANGE_TIMEOUT)
            .map_err(|_| WebFetchToolConstructionError::Transport)?;
        Self::try_new(transport)
    }
}

impl<Transport> WebFetchTool<Transport> {
    /// Compiles immutable metadata around one injected transport.
    pub fn try_new(transport: Transport) -> Result<Self, WebFetchToolConstructionError> {
        let name = ToolName::try_new(String::from(WEB_FETCH_NAME))
            .map_err(|_| WebFetchToolConstructionError::Name)?;
        let schema = ToolInputSchema::try_new(String::from(WEB_FETCH_SCHEMA))
            .map_err(|_| WebFetchToolConstructionError::Schema)?;
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| WebFetchToolConstructionError::ErrorDetail)?;
        let request_failed_detail =
            ToolExecutionErrorDetail::try_new(String::from(REQUEST_FAILED_DETAIL))
                .map_err(|_| WebFetchToolConstructionError::ErrorDetail)?;
        let definition = ToolDefinition::new(
            name,
            String::from(WEB_FETCH_DESCRIPTION),
            schema,
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        );
        let compiled = CompiledTool::new(
            definition,
            WebFetchArgumentValidator {
                detail: invalid_arguments_detail,
            },
        );
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| WebFetchToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: WebFetchExecutor {
                transport,
                request_failed_detail,
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
}

impl ToolArgumentValidator for WebFetchArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(arguments)
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

/// Sanitized result of one physical fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebFetchTransportFailure {
    /// Destination resolution or client setup failed before dispatch.
    RequestFailed,
    /// Dispatch began but no complete bounded response was established.
    DispatchUnknown,
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
        build_web_fetch_client(exchange_timeout, None)?;
        Ok(Self { exchange_timeout })
    }
}

/// The fixed production web-fetch client could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReqwestWebFetchConstructionError;

impl fmt::Display for ReqwestWebFetchConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("credential-free web fetch client construction failed")
    }
}

impl Error for ReqwestWebFetchConstructionError {}

impl WebFetchTransport for ReqwestWebFetchTransport {
    async fn fetch(
        &mut self,
        request: WebFetchRequest,
    ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
        let client = public_destination_client(request.url(), self.exchange_timeout)
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
            if stream
                .next()
                .await
                .transpose()
                .map_err(|_| WebFetchTransportFailure::DispatchUnknown)?
                .is_some()
            {
                truncated = true;
            }
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedPublicDestination {
    host: String,
    addresses: Vec<SocketAddr>,
}

/// Builds one credential-free client pinned to a URL's complete admitted
/// public DNS result.
pub(crate) async fn public_destination_client(
    url: &Url,
    exchange_timeout: Duration,
) -> Result<Client, PublicDestinationClientError> {
    let started = tokio::time::Instant::now();
    let destination = resolve_public_destination(url, exchange_timeout).await?;
    let remaining = exchange_timeout
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(PublicDestinationClientError::Infrastructure)?;
    build_web_fetch_client(remaining, Some(&destination))
        .map_err(|_| PublicDestinationClientError::Infrastructure)
}

/// A URL could not be resolved and pinned as a public-only destination before
/// dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublicDestinationClientError {
    /// The destination shape or resolved address set was not public-only.
    DestinationRejected,
    /// DNS resolution or client construction failed before dispatch.
    Infrastructure,
}

async fn resolve_public_destination(
    url: &Url,
    exchange_timeout: Duration,
) -> Result<ResolvedPublicDestination, PublicDestinationClientError> {
    let host = url
        .host_str()
        .ok_or(PublicDestinationClientError::DestinationRejected)?;
    let port = url
        .port_or_known_default()
        .ok_or(PublicDestinationClientError::DestinationRejected)?;
    let addresses = if let Some(address) = parse_url_host_ip(host) {
        vec![SocketAddr::new(address, port)]
    } else {
        let resolved =
            tokio::time::timeout(exchange_timeout, tokio::net::lookup_host((host, port)))
                .await
                .map_err(|_| PublicDestinationClientError::Infrastructure)?
                .map_err(|_| PublicDestinationClientError::Infrastructure)?;
        resolved
            .take(MAX_RESOLVED_ADDRESSES + 1)
            .collect::<Vec<_>>()
    };
    if addresses.is_empty()
        || addresses.len() > MAX_RESOLVED_ADDRESSES
        || addresses
            .iter()
            .any(|address| !is_public_destination_address(address.ip()))
    {
        return Err(PublicDestinationClientError::DestinationRejected);
    }
    Ok(ResolvedPublicDestination {
        host: host.to_owned(),
        addresses,
    })
}

fn build_web_fetch_client(
    exchange_timeout: Duration,
    destination: Option<&ResolvedPublicDestination>,
) -> Result<Client, ReqwestWebFetchConstructionError> {
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
        .timeout(exchange_timeout);
    if let Some(destination) = destination {
        builder = builder.resolve_to_addrs(&destination.host, &destination.addresses);
    }
    builder
        .build()
        .map_err(|_| ReqwestWebFetchConstructionError)
}

fn is_public_destination_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, third, _fourth] = address.octets();
            !(first == 0
                || first == 10
                || first == 127
                || first >= 224
                || (first == 100 && (64..=127).contains(&second))
                || (first == 169 && second == 254)
                || (first == 172 && (16..=31).contains(&second))
                || (first == 192 && second == 0 && third == 0)
                || (first == 192 && second == 0 && third == 2)
                || (first == 192 && second == 88 && third == 99)
                || (first == 192 && second == 168)
                || (first == 198 && matches!(second, 18 | 19))
                || (first == 198 && second == 51 && third == 100)
                || (first == 203 && second == 0 && third == 113))
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            let in_global_unicast = (0x2000..=0x3fff).contains(&segments[0]);
            let special_2001 =
                segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8);
            let transition_6to4 = segments[0] == 0x2002;
            let documentation_3fff = segments[0] == 0x3fff && segments[1] <= 0x0fff;
            in_global_unicast && !special_2001 && !transition_6to4 && !documentation_3fff
        }
    }
}

fn parse_url_host_ip(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
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
        let request = decode_arguments(invocation.request().arguments())
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

fn decode_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<WebFetchRequest, InvalidWebFetchArguments> {
    let serde_json::Value::Object(object) =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidWebFetchArguments)?
    else {
        return Err(InvalidWebFetchArguments);
    };
    if object.len() != 1 {
        return Err(InvalidWebFetchArguments);
    }
    let supplied = object
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.len() <= MAX_URL_BYTES)
        .ok_or(InvalidWebFetchArguments)?;
    let url = Url::parse(supplied).map_err(|_| InvalidWebFetchArguments)?;
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
    Ok(WebFetchRequest { url })
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

    const REDIRECT_STATUS: u16 = 302;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    /// The read-only operation is auto-approved but crash-relevant because a
    /// remote server observes the GET.
    #[test]
    fn web_fetch_definition_carries_exact_policy() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport)
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definitions = catalog.definitions();
        let [definition] = definitions.as_ref() else {
            panic!("web_fetch is the one compiled definition")
        };

        assert_eq!(definition.name().as_str(), WEB_FETCH_NAME);
        assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
    }

    /// Typed decoding accepts one absolute credential-free URL.
    #[test]
    fn web_fetch_typed_decode_accepts_https_url() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport)
            .expect("static web_fetch tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert_eq!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"url":"https://example.com/path?q=one"}"#),
            ),
            Ok(())
        );
    }

    /// Typed decoding rejects URL-embedded credentials.
    #[test]
    fn web_fetch_typed_decode_rejects_user_information() {
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport)
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
        let supplied_url = format!(
            "https://example.com/{}",
            "\u{00e9}".repeat(MAX_URL_BYTES / 4)
        );
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
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport)
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
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport)
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
        let (catalog, _executor) = WebFetchTool::try_new(FailingTransport)
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

    /// Public address classification admits ordinary global-unicast
    /// destinations.
    #[test]
    fn web_fetch_public_destination_classification_accepts_global_addresses() {
        let public_v4 = "93.184.216.34".parse().expect("fixture IPv4 parses");
        let public_v6 = "2606:2800:220:1:248:1893:25c8:1946"
            .parse()
            .expect("fixture IPv6 parses");

        assert!(is_public_destination_address(public_v4));
        assert!(is_public_destination_address(public_v6));
    }

    /// Public address classification rejects link-local and documentation
    /// ranges that must never become fetch destinations.
    #[test]
    fn web_fetch_public_destination_classification_rejects_non_public_addresses() {
        let link_local_v4 = "169.254.169.254".parse().expect("fixture IPv4 parses");
        let documentation_v4 = "192.0.2.1".parse().expect("fixture IPv4 parses");
        let documentation_v6 = "2001:db8::1".parse().expect("fixture IPv6 parses");

        assert!(!is_public_destination_address(link_local_v4));
        assert!(!is_public_destination_address(documentation_v4));
        assert!(!is_public_destination_address(documentation_v6));
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

        let resolution = public_destination_client(request.url(), Duration::from_secs(2)).await;

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
        let client = build_web_fetch_client(Duration::from_secs(2), Some(&destination))
            .expect("fixed test client builds");

        let response = fetch_with_client(client, request).await;

        assert_eq!(response, Err(WebFetchTransportFailure::RequestFailed));
    }

    /// Bounded binary input becomes deterministic lossy text plus metadata.
    #[test]
    fn web_fetch_result_is_bounded_text_with_metadata() {
        let expected_url = "https://example.com/data";
        let expected_status = 200;
        let expected_content_type = "text/plain";
        let expected_body = vec![b'o', b'k', 0xff];
        let supplied = serde_json::json!({"url": expected_url}).to_string();
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
            "url": expected_url,
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
        let client = build_web_fetch_client(Duration::from_secs(2), Some(&destination))
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
