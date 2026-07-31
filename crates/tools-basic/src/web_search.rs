//! Bounded credentialed web search with an explicit provider boundary.

use std::{error::Error, fmt, future::Future, time::Duration};

use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode, Url,
    header::{ACCEPT, HeaderValue},
    redirect::Policy,
};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_model_runtime::{
    CredentialAccess, CredentialReference, CredentialValue, redact_text,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

/// Registry name for bounded web search.
pub const WEB_SEARCH_NAME: &str = "web_search";
/// Non-secret name of the daemon-held Brave Search credential.
pub const BRAVE_SEARCH_CREDENTIAL_REFERENCE: &str = "brave-search-primary";

const BRAVE_SEARCH_ORIGIN: &str = "https://api.search.brave.com";
const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const BRAVE_SUBSCRIPTION_TOKEN_HEADER: &str = "x-subscription-token";
const BRAVE_RESULT_COUNT_QUERY: &str = "20";
const INVALID_ARGUMENTS_DETAIL: &str =
    "expected a nonempty web search query of at most 400 characters and 50 words";
const CREDENTIAL_UNAVAILABLE_DETAIL: &str = "web search credential is unavailable";
const REQUEST_FAILED_DETAIL: &str = "web search request failed";
const INVALID_RESPONSE_DETAIL: &str = "web search provider returned an invalid bounded response";
const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_QUERY_CHARACTERS: usize = 400;
const MAX_QUERY_WORDS: usize = 50;
const MAX_QUERY_BYTES: usize = MAX_QUERY_CHARACTERS * 4;
const MAX_PROVIDER_RESULTS: usize = 20;
const MAX_RETURNED_RESULTS: usize = 10;
const MAX_PROVIDER_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_RESULT_TITLE_BYTES: usize = 2 * 1024;
const MAX_RESULT_URL_BYTES: usize = 8 * 1024;
const MAX_RESULT_SNIPPET_BYTES: usize = 16 * 1024;
const MAX_ERROR_DETAIL_BYTES: usize = 4 * 1024;
const TRUNCATION_SUFFIX: &str = " … [truncated]";

/// Configured web-search provider.
///
/// The enum is deliberately explicit and non-exhaustive: adding a provider is
/// a new variant plus its exhaustive endpoint/authentication mapping. There is
/// no provider selection or fallback at execution time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WebSearchProvider {
    /// Brave Search Web API.
    Brave,
}

impl WebSearchProvider {
    fn endpoint(self) -> ProviderEndpoint {
        match self {
            Self::Brave => ProviderEndpoint {
                origin: BRAVE_SEARCH_ORIGIN,
                url: BRAVE_SEARCH_ENDPOINT,
                credential_header: BRAVE_SUBSCRIPTION_TOKEN_HEADER,
                credential_reference: BRAVE_SEARCH_CREDENTIAL_REFERENCE,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderEndpoint {
    origin: &'static str,
    url: &'static str,
    credential_header: &'static str,
    credential_reference: &'static str,
}

/// Immutable deployment configuration for one explicitly selected provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSearchConfiguration {
    provider: WebSearchProvider,
    egress_policy: WebSearchEgressPolicy,
    credential_reference: CredentialReference,
}

impl WebSearchConfiguration {
    /// Configures exactly one provider and its fixed egress/credential mapping.
    pub fn new(provider: WebSearchProvider) -> Self {
        let endpoint = provider.endpoint();
        Self {
            provider,
            egress_policy: WebSearchEgressPolicy { provider },
            credential_reference: CredentialReference::new(endpoint.credential_reference),
        }
    }

    /// The provider selected by deployment configuration.
    pub const fn provider(&self) -> WebSearchProvider {
        self.provider
    }

    /// The exact-origin egress policy derived from the selected provider.
    pub const fn egress_policy(&self) -> &WebSearchEgressPolicy {
        &self.egress_policy
    }

    /// The non-secret provider credential reference resolved per request.
    pub const fn credential_reference(&self) -> &CredentialReference {
        &self.credential_reference
    }
}

/// Exact API-origin policy derived from the configured provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebSearchEgressPolicy {
    provider: WebSearchProvider,
}

impl WebSearchEgressPolicy {
    /// The sole admitted API origin.
    pub fn allowed_origin(&self) -> &'static str {
        self.provider.endpoint().origin
    }

    fn admits(&self, url: &Url) -> bool {
        let endpoint = self.provider.endpoint();
        let Ok(origin) = Url::parse(endpoint.origin) else {
            return false;
        };
        url.scheme() == origin.scheme()
            && url.host_str() == origin.host_str()
            && url.port_or_known_default() == origin.port_or_known_default()
    }
}

/// A static declaration or production search transport could not be built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebSearchToolConstructionError {
    /// The static name was rejected.
    Name,
    /// The static schema was rejected.
    Schema,
    /// A static sanitized error detail was rejected.
    ErrorDetail,
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
    /// The production transport could not be constructed.
    Transport,
}

impl fmt::Display for WebSearchToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "web_search static name is invalid",
            Self::Schema => "web_search static schema is invalid",
            Self::ErrorDetail => "web_search static error detail is invalid",
            Self::Duplicate => "web_search catalog is duplicated",
            Self::Transport => "web_search transport could not be constructed",
        })
    }
}

impl Error for WebSearchToolConstructionError {}

/// Compiled catalog entry and matching credential-resolving executor.
#[derive(Clone)]
pub struct WebSearchTool<Credentials, Transport> {
    catalog: CompiledToolCatalog,
    executor: WebSearchExecutor<Credentials, Transport>,
}

impl<Credentials, Transport> fmt::Debug for WebSearchTool<Credentials, Transport> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchTool")
            .field("catalog", &self.catalog)
            .field("executor", &self.executor)
            .finish()
    }
}

impl<Credentials, Transport> ToolContract for WebSearchTool<Credentials, Transport> {
    type Arguments = WebSearchArguments;
    const NAME: &'static str = WEB_SEARCH_NAME;
    const DESCRIPTION: &'static str =
        "Searches the web through the explicitly configured provider and returns bounded results.";
}

/// Typed `web_search` argument shape; decoder and schema share it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchArguments {
    /// Nonempty query of at most 400 characters and 50 words.
    query: WebSearchQuery,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
struct WebSearchQuery(String);

impl schemars::JsonSchema for WebSearchQuery {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("WebSearchQuery")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "maxLength": MAX_QUERY_CHARACTERS,
            "minLength": 1,
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

impl TryFrom<String> for WebSearchQuery {
    type Error = InvalidWebSearchArguments;

    fn try_from(query: String) -> Result<Self, Self::Error> {
        if query.len() > MAX_QUERY_BYTES
            || query.trim().is_empty()
            || query.chars().count() > MAX_QUERY_CHARACTERS
            || query.split_whitespace().count() > MAX_QUERY_WORDS
        {
            return Err(InvalidWebSearchArguments);
        }
        Ok(Self(query))
    }
}

impl<Credentials> WebSearchTool<Credentials, ReqwestWebSearchTransport> {
    /// Builds the production tool with the fixed bounded transport policy.
    pub fn try_new_production(
        credentials: Credentials,
        configuration: WebSearchConfiguration,
    ) -> Result<Self, WebSearchToolConstructionError> {
        let transport = ReqwestWebSearchTransport::try_new(DEFAULT_EXCHANGE_TIMEOUT)
            .map_err(|_| WebSearchToolConstructionError::Transport)?;
        Self::try_new(credentials, transport, configuration)
    }
}

impl<Credentials, Transport> WebSearchTool<Credentials, Transport> {
    /// Compiles immutable metadata around injected credential and transport boundaries.
    pub fn try_new(
        credentials: Credentials,
        transport: Transport,
        configuration: WebSearchConfiguration,
    ) -> Result<Self, WebSearchToolConstructionError> {
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let credential_unavailable_detail =
            ToolExecutionErrorDetail::try_new(String::from(CREDENTIAL_UNAVAILABLE_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let request_failed_detail =
            ToolExecutionErrorDetail::try_new(String::from(REQUEST_FAILED_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let invalid_response_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_RESPONSE_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<Self>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => WebSearchToolConstructionError::Name,
            ToolContractCompileError::Schema => WebSearchToolConstructionError::Schema,
        })?;
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition,
            WebSearchArgumentValidator {
                detail: invalid_arguments_detail,
            },
        )])
        .map_err(|_| WebSearchToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: WebSearchExecutor {
                credentials,
                transport,
                configuration,
                credential_unavailable_detail,
                request_failed_detail,
                invalid_response_detail,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(
        self,
    ) -> (
        CompiledToolCatalog,
        WebSearchExecutor<Credentials, Transport>,
    ) {
        (self.catalog, self.executor)
    }
}

#[derive(Clone, Debug)]
struct WebSearchArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for WebSearchArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// One typed query pinned to the configured provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSearchRequest {
    provider: WebSearchProvider,
    query: String,
}

impl WebSearchRequest {
    /// The explicitly configured provider for this request.
    pub const fn provider(&self) -> WebSearchProvider {
        self.provider
    }

    /// The checked query.
    pub fn query(&self) -> &str {
        &self.query
    }
}

/// One checked provider result.
#[derive(Clone, Eq, PartialEq)]
pub struct WebSearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Named fields for one provider result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSearchResultFields {
    /// Result title.
    pub title: String,
    /// Absolute HTTP(S) result URL.
    pub url: String,
    /// Provider-supplied result snippet.
    pub snippet: String,
}

impl fmt::Debug for WebSearchResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchResult")
            .field("title", &"[provider-controlled]")
            .field("url", &"[provider-controlled]")
            .field("snippet", &"[provider-controlled]")
            .finish()
    }
}

impl WebSearchResult {
    /// Constructs one provider result within the fixed field bounds.
    pub fn try_new(fields: WebSearchResultFields) -> Option<Self> {
        let parsed = Url::parse(&fields.url).ok()?;
        let normalized_url = parsed.to_string();
        (fields.title.len() <= MAX_RESULT_TITLE_BYTES
            && !fields.title.trim().is_empty()
            && fields.url.len() <= MAX_RESULT_URL_BYTES
            && normalized_url.len() <= MAX_RESULT_URL_BYTES
            && matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && fields.snippet.len() <= MAX_RESULT_SNIPPET_BYTES)
            .then_some(Self {
                title: fields.title,
                url: normalized_url,
                snippet: fields.snippet,
            })
    }

    /// Result title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Absolute HTTP(S) result URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Provider-supplied result snippet.
    pub fn snippet(&self) -> &str {
        &self.snippet
    }
}

/// One complete bounded provider response.
#[derive(Clone, Eq, PartialEq)]
pub struct WebSearchResponse {
    results: Vec<WebSearchResult>,
    completeness: WebSearchPageCompleteness,
}

/// Whether the provider page exhausts the known search results.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebSearchPageCompleteness {
    /// The provider reported no next page.
    Complete,
    /// The provider reported additional results beyond this page.
    MoreAvailable,
}

impl fmt::Debug for WebSearchResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchResponse")
            .field("result_count", &self.results.len())
            .field("completeness", &self.completeness)
            .finish()
    }
}

impl WebSearchResponse {
    /// Constructs a complete response no larger than the requested provider page.
    pub fn new(
        results: Vec<WebSearchResult>,
        completeness: WebSearchPageCompleteness,
    ) -> Option<Self> {
        (results.len() <= MAX_PROVIDER_RESULTS).then_some(Self {
            results,
            completeness,
        })
    }

    /// Checked results returned on this provider page.
    pub fn results(&self) -> &[WebSearchResult] {
        &self.results
    }

    /// Whether the provider reported another page beyond this response.
    pub const fn more_results_available(&self) -> bool {
        matches!(self.completeness, WebSearchPageCompleteness::MoreAvailable)
    }
}

/// Opaque complete provider error body retained for request-key sanitization.
pub struct WebSearchProviderError {
    status: u16,
    body: Vec<u8>,
}

impl fmt::Debug for WebSearchProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WebSearchProviderError")
    }
}

impl fmt::Display for WebSearchProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("web search provider rejection evidence")
    }
}

impl Error for WebSearchProviderError {}

impl WebSearchProviderError {
    /// Retains one complete provider error body within the exchange cap.
    pub fn new(status: u16, body: Vec<u8>) -> Option<Self> {
        (body.len() <= MAX_PROVIDER_RESPONSE_BYTES).then_some(Self { status, body })
    }
}

/// Sanitized classification of one physical provider exchange.
pub enum WebSearchTransportFailure {
    /// Client-side credential bytes could not form the provider header.
    InvalidCredential,
    /// A request-scoped credential collided with otherwise-safe diagnostics.
    CredentialDiagnosticCollision(WebSearchCredentialDiagnostic),
    /// Client setup or connection failed before dispatch.
    RequestFailed,
    /// A complete status and complete bounded provider error body were received.
    ProviderRejected(WebSearchProviderError),
    /// A complete success body did not match the provider contract.
    InvalidResponse,
    /// The provider body exceeded the fixed exchange cap.
    ResponseTooLarge,
    /// Dispatch began without a complete bounded outcome.
    DispatchUnknown,
}

/// Opaque request-scoped diagnostic proven not to contain its credential.
#[derive(Clone, Eq, PartialEq)]
pub struct WebSearchCredentialDiagnostic {
    rendered: String,
    dispatch_unknown: bool,
}

impl fmt::Debug for WebSearchCredentialDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl fmt::Display for WebSearchCredentialDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl fmt::Debug for WebSearchTransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredential => formatter.write_str("InvalidCredential"),
            Self::CredentialDiagnosticCollision(diagnostic) => {
                formatter.write_str(&diagnostic.rendered)
            }
            Self::RequestFailed => formatter.write_str("RequestFailed"),
            Self::ProviderRejected(_) => formatter.write_str("ProviderRejected"),
            Self::InvalidResponse => formatter.write_str("InvalidResponse"),
            Self::ResponseTooLarge => formatter.write_str("ResponseTooLarge"),
            Self::DispatchUnknown => formatter.write_str("DispatchUnknown"),
        }
    }
}

impl fmt::Display for WebSearchTransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredential => formatter.write_str("invalid web search credential"),
            Self::CredentialDiagnosticCollision(diagnostic) => {
                formatter.write_str(&diagnostic.rendered)
            }
            Self::RequestFailed => formatter.write_str("web search request failed before dispatch"),
            Self::ProviderRejected(_) => {
                formatter.write_str("web search provider rejected the request")
            }
            Self::InvalidResponse => {
                formatter.write_str("web search provider returned an invalid response")
            }
            Self::ResponseTooLarge => {
                formatter.write_str("web search provider response exceeded the byte cap")
            }
            Self::DispatchUnknown => formatter.write_str("web search request outcome is unknown"),
        }
    }
}

impl Error for WebSearchTransportFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProviderRejected(error) => Some(error),
            Self::InvalidCredential
            | Self::CredentialDiagnosticCollision(_)
            | Self::RequestFailed
            | Self::InvalidResponse
            | Self::ResponseTooLarge
            | Self::DispatchUnknown => None,
        }
    }
}

/// Injectable one-request credentialed search transport.
pub trait WebSearchTransport: Send {
    /// Performs exactly one provider-pinned query with one request-scoped credential.
    fn search(
        &mut self,
        request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> impl Future<Output = Result<WebSearchResponse, WebSearchTransportFailure>> + Send;
}

/// Production reqwest transport with no ambient proxy, redirect, or retry.
#[derive(Clone, Debug)]
pub struct ReqwestWebSearchTransport {
    client: Client,
}

impl ReqwestWebSearchTransport {
    /// Builds a provider transport with a positive whole-exchange timeout.
    pub fn try_new(exchange_timeout: Duration) -> Result<Self, ReqwestWebSearchConstructionError> {
        if exchange_timeout.is_zero() {
            return Err(ReqwestWebSearchConstructionError);
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .timeout(exchange_timeout)
            .build()
            .map_err(|_| ReqwestWebSearchConstructionError)?;
        Ok(Self { client })
    }
}

/// The fixed production web-search client could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReqwestWebSearchConstructionError;

impl fmt::Display for ReqwestWebSearchConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("web search client construction failed")
    }
}

impl Error for ReqwestWebSearchConstructionError {}

impl WebSearchTransport for ReqwestWebSearchTransport {
    async fn search(
        &mut self,
        request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> Result<WebSearchResponse, WebSearchTransportFailure> {
        let outcome = async {
            let provider = request.provider();
            let http_request = build_provider_request(&self.client, &request, credential)?;
            let request_url = http_request.url().clone();
            if !(WebSearchEgressPolicy { provider }).admits(&request_url) {
                return Err(WebSearchTransportFailure::RequestFailed);
            }
            let response = self
                .client
                .execute(http_request)
                .await
                .map_err(classify_send_failure)?;
            let status = response.status();
            let body = collect_complete_body(response).await?;
            if status != StatusCode::OK {
                let error = WebSearchProviderError::new(status.as_u16(), body)
                    .ok_or(WebSearchTransportFailure::ResponseTooLarge)?;
                return Err(WebSearchTransportFailure::ProviderRejected(error));
            }
            decode_provider_response(provider, &body)
        }
        .await;
        outcome.map_err(|failure| credential_safe_transport_failure(failure, credential))
    }
}

fn credential_safe_transport_failure(
    failure: WebSearchTransportFailure,
    credential: &CredentialValue,
) -> WebSearchTransportFailure {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let (source_contains_credential, executor_contains_credential, dispatch_unknown) =
        match &failure {
            WebSearchTransportFailure::ProviderRejected(error) => (
                format!("{error:?}").contains(credential_text)
                    || error.to_string().contains(credential_text),
                false,
                false,
            ),
            WebSearchTransportFailure::InvalidCredential
            | WebSearchTransportFailure::CredentialDiagnosticCollision(_)
            | WebSearchTransportFailure::RequestFailed
            | WebSearchTransportFailure::InvalidResponse
            | WebSearchTransportFailure::ResponseTooLarge => (false, false, false),
            WebSearchTransportFailure::DispatchUnknown => {
                let executor_error = WebSearchExecutorError::DispatchUnknown;
                (
                    false,
                    format!("{executor_error:?}").contains(credential_text)
                        || executor_error.to_string().contains(credential_text),
                    true,
                )
            }
        };
    if credential_text.is_empty()
        || format!("{failure:?}").contains(credential_text)
        || failure.to_string().contains(credential_text)
        || source_contains_credential
        || executor_contains_credential
    {
        WebSearchTransportFailure::CredentialDiagnosticCollision(WebSearchCredentialDiagnostic {
            rendered: safe_collision_diagnostic(credential_text),
            dispatch_unknown,
        })
    } else {
        failure
    }
}

fn safe_collision_diagnostic(credential: &str) -> String {
    const DIAGNOSTIC: &str = "web search credential diagnostic suppressed";
    const REDACTION: &str = "[redacted]";
    let redacted = DIAGNOSTIC.replace(credential, REDACTION);
    if !credential.is_empty() && !redacted.contains(credential) {
        return redacted;
    }
    if credential == "!" {
        String::from("?")
    } else {
        String::from("!")
    }
}

fn build_provider_request(
    client: &Client,
    request: &WebSearchRequest,
    credential: &CredentialValue,
) -> Result<reqwest::Request, WebSearchTransportFailure> {
    let endpoint = request.provider.endpoint();
    let credential_text = std::str::from_utf8(credential.expose_bytes())
        .map_err(|_| WebSearchTransportFailure::InvalidCredential)?;
    if credential_text.is_empty() {
        return Err(WebSearchTransportFailure::InvalidCredential);
    }
    if request.query().contains(credential_text) {
        return Err(WebSearchTransportFailure::RequestFailed);
    }
    let mut url = Url::parse(endpoint.url).map_err(|_| WebSearchTransportFailure::RequestFailed)?;
    url.query_pairs_mut()
        .append_pair("q", request.query())
        .append_pair("count", BRAVE_RESULT_COUNT_QUERY)
        .append_pair("result_filter", "web")
        .append_pair("text_decorations", "false");
    if url.as_str().contains(credential_text) {
        return Err(WebSearchTransportFailure::RequestFailed);
    }
    let mut credential_header = HeaderValue::from_bytes(credential.expose_bytes())
        .map_err(|_| WebSearchTransportFailure::InvalidCredential)?;
    credential_header.set_sensitive(true);
    let http_request = client
        .get(url)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(endpoint.credential_header, credential_header)
        .build()
        .map_err(|_| WebSearchTransportFailure::RequestFailed)?;
    if format!("{http_request:?}").contains(credential_text) {
        return Err(WebSearchTransportFailure::RequestFailed);
    }
    Ok(http_request)
}

async fn collect_complete_body(
    response: reqwest::Response,
) -> Result<Vec<u8>, WebSearchTransportFailure> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| WebSearchTransportFailure::DispatchUnknown)?;
        if chunk.len() > MAX_PROVIDER_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(WebSearchTransportFailure::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
        if body.len() == MAX_PROVIDER_RESPONSE_BYTES {
            while let Some(trailing) = stream.next().await {
                let trailing = trailing.map_err(|_| WebSearchTransportFailure::DispatchUnknown)?;
                if !trailing.is_empty() {
                    return Err(WebSearchTransportFailure::ResponseTooLarge);
                }
            }
        }
    }
    Ok(body)
}

fn classify_send_failure(error: reqwest::Error) -> WebSearchTransportFailure {
    if error.is_connect() {
        WebSearchTransportFailure::RequestFailed
    } else {
        WebSearchTransportFailure::DispatchUnknown
    }
}

#[derive(serde::Deserialize)]
struct BraveResponse {
    #[serde(rename = "type")]
    response_type: String,
    query: BraveQueryFacts,
    #[serde(default)]
    web: Option<BraveWebResults>,
}

#[derive(serde::Deserialize)]
struct BraveQueryFacts {
    more_results_available: bool,
}

#[derive(serde::Deserialize)]
struct BraveWebResults {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(serde::Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    #[serde(rename = "description")]
    snippet: String,
}

fn decode_provider_response(
    provider: WebSearchProvider,
    body: &[u8],
) -> Result<WebSearchResponse, WebSearchTransportFailure> {
    match provider {
        WebSearchProvider::Brave => decode_brave_response(body),
    }
}

fn decode_brave_response(body: &[u8]) -> Result<WebSearchResponse, WebSearchTransportFailure> {
    let response: BraveResponse =
        serde_json::from_slice(body).map_err(|_| WebSearchTransportFailure::InvalidResponse)?;
    if response.response_type != "search" {
        return Err(WebSearchTransportFailure::InvalidResponse);
    }
    let completeness = if response.query.more_results_available {
        WebSearchPageCompleteness::MoreAvailable
    } else {
        WebSearchPageCompleteness::Complete
    };
    let raw_results = response.web.map_or_else(Vec::new, |web| web.results);
    if raw_results.len() > MAX_PROVIDER_RESULTS {
        return Err(WebSearchTransportFailure::InvalidResponse);
    }
    let results = raw_results
        .into_iter()
        .map(|result| {
            WebSearchResult::try_new(WebSearchResultFields {
                title: result.title,
                url: result.url,
                snippet: result.snippet,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(WebSearchTransportFailure::InvalidResponse)?;
    WebSearchResponse::new(results, completeness).ok_or(WebSearchTransportFailure::InvalidResponse)
}

/// Credential-resolving daemon-local web-search executor.
#[derive(Clone)]
pub struct WebSearchExecutor<Credentials, Transport> {
    credentials: Credentials,
    transport: Transport,
    configuration: WebSearchConfiguration,
    credential_unavailable_detail: ToolExecutionErrorDetail,
    request_failed_detail: ToolExecutionErrorDetail,
    invalid_response_detail: ToolExecutionErrorDetail,
}

impl<Credentials, Transport> fmt::Debug for WebSearchExecutor<Credentials, Transport> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchExecutor")
            .field("configuration", &self.configuration)
            .field("credentials", &"[injected]")
            .field("transport", &"[injected]")
            .finish_non_exhaustive()
    }
}

/// A checked catalog/executor assumption failed inside `web_search`.
#[derive(Clone, Eq, PartialEq)]
pub enum WebSearchExecutorError {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// Sanitized result or error evidence could not be encoded.
    EvidenceEncoding,
    /// Physical dispatch began without a complete bounded outcome.
    DispatchUnknown,
    /// A dispatch-unknown diagnostic collided with its request credential.
    CredentialDiagnosticCollision(WebSearchCredentialDiagnostic),
}

impl fmt::Debug for WebSearchExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => formatter.write_str("ArgumentValidationDrift"),
            Self::EvidenceEncoding => formatter.write_str("EvidenceEncoding"),
            Self::DispatchUnknown => formatter.write_str("DispatchUnknown"),
            Self::CredentialDiagnosticCollision(diagnostic) => {
                formatter.write_str(&diagnostic.rendered)
            }
        }
    }
}

impl fmt::Display for WebSearchExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => {
                formatter.write_str("web_search argument validation drifted")
            }
            Self::EvidenceEncoding => formatter.write_str("web_search evidence encoding failed"),
            Self::DispatchUnknown => formatter.write_str("web_search dispatch outcome is unknown"),
            Self::CredentialDiagnosticCollision(diagnostic) => {
                formatter.write_str(&diagnostic.rendered)
            }
        }
    }
}

impl Error for WebSearchExecutorError {}

impl ClassifyOperatorFailure for WebSearchExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::ArgumentValidationDrift | Self::EvidenceEncoding => {
                OperatorFailureClass::CallerOrHubBug
            }
            Self::DispatchUnknown | Self::CredentialDiagnosticCollision(_) => {
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                }
            }
        }
    }
}

impl<Credentials, Transport> WebSearchExecutor<Credentials, Transport>
where
    Credentials: CredentialAccess,
    Transport: WebSearchTransport,
{
    async fn execute_request(
        &mut self,
        request: WebSearchRequest,
    ) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
        let credential = match self
            .credentials
            .resolve(&self.configuration.credential_reference)
            .await
        {
            Ok(credential) => credential,
            Err(_) => {
                return Ok(ToolExecutorEvidence::KnownFailed {
                    detail: Some(self.credential_unavailable_detail.clone()),
                });
            }
        };
        let Some(scrubber) = CredentialScrubber::try_new(&credential) else {
            return Ok(ToolExecutorEvidence::KnownFailed {
                detail: Some(self.credential_unavailable_detail.clone()),
            });
        };
        Ok(match self.transport.search(request, &credential).await {
            Ok(response) => success_evidence(response, &scrubber)?,
            Err(WebSearchTransportFailure::InvalidCredential) => {
                known_failure_evidence(self.credential_unavailable_detail.clone(), &scrubber)?
            }
            Err(WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic)) => {
                if diagnostic.dispatch_unknown {
                    return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                        diagnostic,
                    ));
                }
                known_failure_evidence(self.credential_unavailable_detail.clone(), &scrubber)?
            }
            Err(WebSearchTransportFailure::RequestFailed) => {
                known_failure_evidence(self.request_failed_detail.clone(), &scrubber)?
            }
            Err(WebSearchTransportFailure::ProviderRejected(error)) => {
                ToolExecutorEvidence::KnownFailed {
                    detail: Some(provider_error_detail(error, &scrubber)?),
                }
            }
            Err(
                WebSearchTransportFailure::InvalidResponse
                | WebSearchTransportFailure::ResponseTooLarge,
            ) => known_failure_evidence(self.invalid_response_detail.clone(), &scrubber)?,
            Err(WebSearchTransportFailure::DispatchUnknown) => {
                return Err(WebSearchExecutorError::DispatchUnknown);
            }
        })
    }
}

impl<Credentials, Transport> ToolExecutor for WebSearchExecutor<Credentials, Transport>
where
    Credentials: CredentialAccess,
    Transport: WebSearchTransport,
{
    type Error = WebSearchExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let request = decode_arguments_for_provider(
            invocation.request().arguments(),
            self.configuration.provider,
        )
        .map_err(|_| WebSearchExecutorError::ArgumentValidationDrift)?;
        let evidence = self.execute_request(request).await?;
        Ok(invocation.bind(evidence))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidWebSearchArguments;

impl fmt::Display for InvalidWebSearchArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(INVALID_ARGUMENTS_DETAIL)
    }
}

fn decode_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<WebSearchQuery, InvalidWebSearchArguments> {
    let decoded: WebSearchArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidWebSearchArguments)?;
    Ok(decoded.query)
}

fn decode_arguments_for_provider(
    arguments: &NormalizedToolArguments,
    provider: WebSearchProvider,
) -> Result<WebSearchRequest, InvalidWebSearchArguments> {
    let query = decode_arguments(arguments)?;
    Ok(WebSearchRequest {
        provider,
        query: query.0,
    })
}

#[derive(serde::Serialize)]
struct RenderedSearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn success_evidence(
    response: WebSearchResponse,
    scrubber: &CredentialScrubber,
) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
    let truncated = response.results.len() > MAX_RETURNED_RESULTS
        || response.completeness == WebSearchPageCompleteness::MoreAvailable;
    let results = response
        .results
        .into_iter()
        .take(MAX_RETURNED_RESULTS)
        .map(|result| {
            let sanitized = WebSearchResult::try_new(WebSearchResultFields {
                title: scrubber.redact_text(&result.title),
                url: scrubber.redact_text(&result.url),
                snippet: scrubber.redact_text(&result.snippet),
            })
            .ok_or(WebSearchExecutorError::EvidenceEncoding)?;
            Ok(RenderedSearchResult {
                title: sanitized.title,
                url: sanitized.url,
                snippet: sanitized.snippet,
            })
        })
        .collect::<Result<Vec<_>, WebSearchExecutorError>>()?;
    let content = serde_json::to_string(&serde_json::json!({
        "results": results,
        "truncated": truncated,
    }))
    .map_err(|_| WebSearchExecutorError::EvidenceEncoding)?;
    if scrubber.contains_credential(&content) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    Ok(ToolExecutorEvidence::CompletedText(content))
}

fn known_failure_evidence(
    detail: ToolExecutionErrorDetail,
    scrubber: &CredentialScrubber,
) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
    if scrubber.contains_credential(detail.as_str()) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    Ok(ToolExecutorEvidence::KnownFailed {
        detail: Some(detail),
    })
}

fn provider_error_detail(
    error: WebSearchProviderError,
    scrubber: &CredentialScrubber,
) -> Result<ToolExecutionErrorDetail, WebSearchExecutorError> {
    let redacted = scrubber.redact_body(&error.body);
    let normalized = redacted
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let body = normalized.trim();
    let detail = if body.is_empty() {
        format!(
            "web search provider rejected the request with HTTP status {}",
            error.status
        )
    } else {
        format!(
            "web search provider rejected the request with HTTP status {}: {body}",
            error.status
        )
    };
    let bounded = truncate_after_redaction(detail);
    if scrubber.contains_credential(&bounded) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    ToolExecutionErrorDetail::try_new(bounded).map_err(|_| WebSearchExecutorError::EvidenceEncoding)
}

fn truncate_after_redaction(detail: String) -> String {
    if detail.len() <= MAX_ERROR_DETAIL_BYTES {
        return detail;
    }
    let retained_bytes = MAX_ERROR_DETAIL_BYTES.saturating_sub(TRUNCATION_SUFFIX.len());
    let mut end = retained_bytes;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &detail[..end], TRUNCATION_SUFFIX)
}

struct CredentialScrubber {
    exact: String,
    json_escaped: String,
}

impl CredentialScrubber {
    fn try_new(credential: &CredentialValue) -> Option<Self> {
        let exact = std::str::from_utf8(credential.expose_bytes())
            .ok()?
            .to_owned();
        if exact.is_empty() {
            return None;
        }
        let encoded = serde_json::to_string(&exact).ok()?;
        let json_escaped = encoded.get(1..encoded.len().checked_sub(1)?)?.to_owned();
        Some(Self {
            exact,
            json_escaped,
        })
    }

    fn redact_text(&self, text: &str) -> String {
        let generically_redacted = redact_text(text);
        let exact_redacted = generically_redacted.replace(&self.exact, "");
        exact_redacted.replace(&self.json_escaped, "")
    }

    fn contains_credential(&self, text: &str) -> bool {
        text.contains(&self.exact) || text.contains(&self.json_escaped)
    }

    fn redact_body(&self, body: &[u8]) -> String {
        let text = String::from_utf8_lossy(body);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            let Ok(canonical) = serde_json::to_string(&value) else {
                return String::from("[redacted]");
            };
            return self.redact_text(&canonical);
        }
        if text.contains('\\') {
            return String::from("[redacted]");
        }
        self.redact_text(&text)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
    use signalbox_model_runtime::CredentialAccessError;

    use super::*;

    const SYNTHETIC_KEY: &str = "fixture-search-key";
    const FIXTURE_QUERY: &str = "bounded rust search";
    const FIXTURE_RESULT_TITLE: &str = "Synthetic result";
    const FIXTURE_RESULT_URL: &str = "https://example.com/result";
    const FIXTURE_RESULT_SNIPPET: &str = "Synthetic recorded snippet";
    const FIXTURE_WHITESPACE_TITLE: &str = " \t\n";
    const FIXTURE_NORMALIZED_RESULT_URL: &str = "https://exa\nmple.com/result";
    const FIXTURE_ORIGIN_ONLY_RESULT_URL: &str = "https://example.com";
    const FIXTURE_CANONICAL_ORIGIN_RESULT_URL: &str = "https://example.com/";
    const ACCEPT_HEADER_COLLISION_KEY: &str = "application/json";
    const URL_SCHEME_COLLISION_KEY: &str = "https";
    const PROVIDER_REJECTION_STATUS: u16 = 429;

    struct CountingCredentials {
        resolutions: Arc<AtomicUsize>,
    }

    impl CredentialAccess for CountingCredentials {
        async fn resolve(
            &self,
            _reference: &CredentialReference,
        ) -> Result<CredentialValue, CredentialAccessError> {
            self.resolutions.fetch_add(1, Ordering::Relaxed);
            Ok(CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec()))
        }
    }

    struct CountingTransport {
        searches: Arc<AtomicUsize>,
    }

    impl WebSearchTransport for CountingTransport {
        async fn search(
            &mut self,
            _request: WebSearchRequest,
            _credential: &CredentialValue,
        ) -> Result<WebSearchResponse, WebSearchTransportFailure> {
            self.searches.fetch_add(1, Ordering::Relaxed);
            Ok(response_with_result_count(1))
        }
    }

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn configuration() -> WebSearchConfiguration {
        WebSearchConfiguration::new(WebSearchProvider::Brave)
    }

    fn result(title: impl Into<String>) -> WebSearchResult {
        WebSearchResult::try_new(WebSearchResultFields {
            title: title.into(),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from("synthetic recorded snippet"),
        })
        .expect("fixture result is admitted")
    }

    fn response_with_result_count(count: usize) -> WebSearchResponse {
        let results = (0..count)
            .map(|index| result(format!("recorded result {index}")))
            .collect();
        WebSearchResponse::new(results, WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted")
    }

    fn scrubber() -> CredentialScrubber {
        CredentialScrubber::try_new(&CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec()))
            .expect("fixture credential is usable")
    }

    fn collision_executor_error(failure: WebSearchTransportFailure) -> WebSearchExecutorError {
        match failure {
            WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic)
                if diagnostic.dispatch_unknown =>
            {
                WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic)
            }
            other => panic!("fixture expected a dispatch-unknown collision, got {other:?}"),
        }
    }

    fn build_brave_request(
        query: &str,
        credential: &str,
    ) -> Result<reqwest::Request, WebSearchTransportFailure> {
        let transport = ReqwestWebSearchTransport::try_new(DEFAULT_EXCHANGE_TIMEOUT)
            .expect("fixture client builds");
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(query),
        };
        let credential = CredentialValue::new(credential.as_bytes().to_vec());
        build_provider_request(&transport.client, &request, &credential)
    }

    fn brave_request() -> reqwest::Request {
        build_brave_request(FIXTURE_QUERY, SYNTHETIC_KEY).expect("fixture request builds")
    }

    /// The provider read is auto-approved but remains crash-relevant because
    /// the remote provider observes the authenticated GET.
    #[test]
    fn web_search_definition_carries_exact_policy() {
        let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let definitions = catalog.definitions();
        let [definition] = definitions.as_ref() else {
            panic!("web_search is the one compiled definition")
        };

        assert_eq!(definition.name().as_str(), WEB_SEARCH_NAME);
        assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
    }

    /// Explicit provider configuration derives exactly Brave's API origin and
    /// fixed non-secret credential reference.
    #[test]
    fn brave_configuration_carries_one_provider_origin_and_reference() {
        let configuration = configuration();

        assert_eq!(configuration.provider(), WebSearchProvider::Brave);
        assert_eq!(
            configuration.egress_policy().allowed_origin(),
            BRAVE_SEARCH_ORIGIN
        );
        assert_eq!(
            configuration.credential_reference().as_str(),
            BRAVE_SEARCH_CREDENTIAL_REFERENCE
        );
    }

    /// The provider policy compares scheme, host, and effective port, so a
    /// different origin is never admitted by the automatic read.
    #[test]
    fn brave_egress_policy_rejects_every_other_origin() {
        let policy = configuration().egress_policy;
        let endpoint = Url::parse(BRAVE_SEARCH_ENDPOINT).expect("provider endpoint is valid");
        let other = Url::parse("https://collector.example/search")
            .expect("fixture alternate origin is valid");
        let other_scheme = Url::parse("http://api.search.brave.com/res/v1/web/search")
            .expect("fixture alternate scheme is valid");
        let other_port = Url::parse("https://api.search.brave.com:444/res/v1/web/search")
            .expect("fixture alternate port is valid");
        let subdomain = Url::parse("https://sub.api.search.brave.com/res/v1/web/search")
            .expect("fixture subdomain is valid");

        assert!(policy.admits(&endpoint));
        assert!(!policy.admits(&other));
        assert!(!policy.admits(&other_scheme));
        assert!(!policy.admits(&other_port));
        assert!(!policy.admits(&subdomain));
    }

    /// One physical query resolves its pinned credential once and dispatches
    /// the injected transport once.
    #[tokio::test]
    async fn web_search_resolves_one_credential_per_physical_request() {
        let resolutions = Arc::new(AtomicUsize::new(0));
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = CountingCredentials {
            resolutions: Arc::clone(&resolutions),
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let supplied = serde_json::json!({"query": FIXTURE_QUERY}).to_string();
        let request =
            decode_arguments_for_provider(&arguments(&supplied), WebSearchProvider::Brave)
                .expect("fixture request decodes");

        let evidence = executor
            .execute_request(request)
            .await
            .expect("synthetic transport completes");

        assert!(matches!(evidence, ToolExecutorEvidence::CompletedText(_)));
        assert_eq!(resolutions.load(Ordering::Relaxed), 1);
        assert_eq!(searches.load(Ordering::Relaxed), 1);
    }

    /// The rendered schema is the exact query-only wire artifact.
    #[test]
    fn web_search_rendered_schema_is_the_exact_wire_artifact() {
        let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];
        let schema: serde_json::Value = serde_json::from_str(definition.input_schema().as_str())
            .expect("registry schema is valid JSON");

        expect_test::expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "query": {
                  "description": "Nonempty query of at most 400 characters and 50 words.",
                  "maxLength": 400,
                  "minLength": 1,
                  "type": "string"
                }
              },
              "required": [
                "query"
              ],
              "type": "object"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
        assert_eq!(definition.input_schema().as_str(), schema.to_string());
    }

    /// Typed decoding accepts the documented bounded query shape.
    #[test]
    fn web_search_typed_decode_accepts_bounded_query() {
        let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];
        let supplied = serde_json::json!({"query": FIXTURE_QUERY}).to_string();

        assert_eq!(
            catalog.validate_arguments(definition.name(), &arguments(&supplied)),
            Ok(())
        );
    }

    /// Typed decoding rejects a query with no non-whitespace content.
    #[test]
    fn web_search_typed_decode_rejects_blank_query() {
        let (catalog, _executor) = WebSearchTool::try_new((), (), configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(definition.name(), &arguments(r#"{"query":"   "}"#)),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// The structured output retains ten results and reports that the
    /// provider returned an omitted eleventh result.
    #[test]
    fn web_search_result_reports_provider_result_truncation() {
        let response = response_with_result_count(MAX_RETURNED_RESULTS + 1);
        let evidence = success_evidence(response, &scrubber()).expect("response encodes");
        let ToolExecutorEvidence::CompletedText(content) = evidence else {
            panic!("a successful provider response completes with text")
        };
        let value: serde_json::Value =
            serde_json::from_str(&content).expect("result is valid JSON");

        assert_eq!(
            value["results"]
                .as_array()
                .expect("results are an array")
                .len(),
            MAX_RETURNED_RESULTS
        );
        assert_eq!(value["truncated"], true);
    }

    /// Complete provider pagination evidence reports omitted search results
    /// even when the current page itself fits the output count.
    #[test]
    fn web_search_result_reports_provider_pagination_truncation() {
        let response = WebSearchResponse::new(
            vec![result("only result")],
            WebSearchPageCompleteness::MoreAvailable,
        )
        .expect("fixture response is admitted");
        let evidence = success_evidence(response, &scrubber()).expect("response encodes");
        let ToolExecutorEvidence::CompletedText(content) = evidence else {
            panic!("a successful provider response completes with text")
        };
        let value: serde_json::Value =
            serde_json::from_str(&content).expect("result is valid JSON");

        assert_eq!(value["truncated"], true);
    }

    /// A provider result title must retain non-whitespace content.
    #[test]
    fn web_search_result_rejects_whitespace_only_title() {
        assert!(
            WebSearchResult::try_new(WebSearchResultFields {
                title: String::from(FIXTURE_WHITESPACE_TITLE),
                url: String::from(FIXTURE_RESULT_URL),
                snippet: String::from(FIXTURE_RESULT_SNIPPET),
            })
            .is_none()
        );
    }

    /// A checked result stores the parser-validated URL serialization rather
    /// than provider text discarded during parsing.
    #[test]
    fn web_search_result_stores_url_text_normalized_by_parser() {
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_NORMALIZED_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("normalized fixture URL is admitted");

        assert_eq!(result.url(), FIXTURE_RESULT_URL);
    }

    /// Routine URL canonicalization, including an origin-only trailing slash,
    /// is retained as the validated result URL.
    #[test]
    fn web_search_result_preserves_canonicalizable_origin_url() {
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_ORIGIN_ONLY_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("origin-only fixture URL is admitted");

        assert_eq!(result.url(), FIXTURE_CANONICAL_ORIGIN_RESULT_URL);
    }

    /// INV-035: provider-controlled successful fields are credential-scrubbed
    /// before entering completed tool evidence.
    #[test]
    fn web_search_success_evidence_redacts_reflected_credential() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: format!("reflected {SYNTHETIC_KEY}"),
            url: format!("{FIXTURE_RESULT_URL}?token={SYNTHETIC_KEY}"),
            snippet: format!("snippet {SYNTHETIC_KEY}"),
        })
        .expect("reflected fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let evidence = success_evidence(response, &scrubber()).expect("response encodes");
        let ToolExecutorEvidence::CompletedText(content) = evidence else {
            panic!("a successful provider response completes with text")
        };

        assert!(!content.contains(SYNTHETIC_KEY));
    }

    /// INV-035: credential scrubbing cannot turn a checked result title into
    /// an empty title in completed evidence.
    #[test]
    fn web_search_rejects_result_with_title_invalidated_by_credential_scrubbing() {
        const TITLE_COLLISION_KEY: &str = "synthetic-title-key";
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(TITLE_COLLISION_KEY),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("reflected fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            TITLE_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: credential scrubbing cannot turn a checked result URL into an
    /// invalid URL in completed evidence.
    #[test]
    fn web_search_rejects_result_with_url_invalidated_by_credential_scrubbing() {
        let response = WebSearchResponse::new(
            vec![result(FIXTURE_RESULT_TITLE)],
            WebSearchPageCompleteness::Complete,
        )
        .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_SCHEME_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: credential removal cannot reproduce a key that overlaps the
    /// ordinary redaction sentinel.
    #[test]
    fn web_search_redaction_sentinel_cannot_reproduce_credential() {
        const SENTINEL_OVERLAPPING_KEY: &str = "red";
        const SHAPED_SECRET: &str = "SYNTHETIC-SHAPED-SECRET";
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: format!("x{SENTINEL_OVERLAPPING_KEY}x"),
            url: format!("{FIXTURE_RESULT_URL}?q={SENTINEL_OVERLAPPING_KEY}"),
            snippet: format!("y{SENTINEL_OVERLAPPING_KEY}y api_key={SHAPED_SECRET}"),
        })
        .expect("reflected fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            SENTINEL_OVERLAPPING_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");
        let evidence = success_evidence(response, &scrubber).expect("response encodes");
        let ToolExecutorEvidence::CompletedText(content) = evidence else {
            panic!("a successful provider response completes with text")
        };

        assert!(!content.contains(SENTINEL_OVERLAPPING_KEY));
        assert!(!content.contains(SHAPED_SECRET));
    }

    /// INV-035: fixed JSON member names cannot collide with the credential in
    /// completed evidence, even when provider fields contain no credential.
    #[test]
    fn web_search_final_success_payload_rejects_credential_collision() {
        const RENDERED_MEMBER_COLLISION_KEY: &str = "results";
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            RENDERED_MEMBER_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response_with_result_count(1), &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: JSON-aware error sanitization decodes an escaped credential
    /// before the body can enter durable failure evidence.
    #[test]
    fn web_search_error_body_redacts_json_escaped_credential() {
        let body = br#"{"message":"fixture-search-\u006bey"}"#.to_vec();
        let error = WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, body)
            .expect("fixture error body is bounded");
        let detail = provider_error_detail(error, &scrubber()).expect("detail is admitted");

        assert!(!detail.as_str().contains(SYNTHETIC_KEY));
    }

    /// INV-035: error redaction precedes evidence truncation, so a credential
    /// crossing the retained prefix is replaced before the suffix is added.
    #[test]
    fn web_search_error_body_is_redacted_before_truncation() {
        let reflected = format!(
            "{}{}{}",
            "a".repeat(MAX_ERROR_DETAIL_BYTES - 100),
            SYNTHETIC_KEY,
            "z".repeat(MAX_ERROR_DETAIL_BYTES)
        );
        let error = WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, reflected.into_bytes())
            .expect("fixture error body is bounded");
        let detail = provider_error_detail(error, &scrubber()).expect("detail is admitted");

        assert!(!detail.as_str().contains(SYNTHETIC_KEY));
        assert!(detail.as_str().ends_with(TRUNCATION_SUFFIX));
    }

    /// INV-035: fixed provider-error prose cannot collide with the credential
    /// after the provider body has been sanitized.
    #[test]
    fn web_search_final_error_detail_rejects_credential_collision() {
        const ERROR_PREFIX_COLLISION_KEY: &str = "provider";
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            ERROR_PREFIX_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");
        let error = WebSearchProviderError::new(
            PROVIDER_REJECTION_STATUS,
            br#"{"message":"synthetic rejection"}"#.to_vec(),
        )
        .expect("fixture error body is bounded");

        assert_eq!(
            provider_error_detail(error, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// The Brave provider mapping owns the exact endpoint and bounded web-only
    /// query parameters used by its one physical request.
    #[test]
    fn brave_request_uses_the_mapped_endpoint_and_parameters() {
        let built = brave_request();
        let endpoint = Url::parse(BRAVE_SEARCH_ENDPOINT).expect("provider endpoint is valid");
        let parameters = built
            .url()
            .query_pairs()
            .into_owned()
            .collect::<BTreeMap<_, _>>();

        assert_eq!(built.url().scheme(), endpoint.scheme());
        assert_eq!(built.url().host_str(), endpoint.host_str());
        assert_eq!(
            built.url().port_or_known_default(),
            endpoint.port_or_known_default()
        );
        assert_eq!(built.url().path(), endpoint.path());
        assert_eq!(parameters.get("q").map(String::as_str), Some(FIXTURE_QUERY));
        assert_eq!(
            parameters.get("count").map(String::as_str),
            Some(BRAVE_RESULT_COUNT_QUERY)
        );
        assert_eq!(
            parameters.get("result_filter").map(String::as_str),
            Some("web")
        );
        assert_eq!(
            parameters.get("text_decorations").map(String::as_str),
            Some("false")
        );
    }

    /// INV-035: the API key is header-only, marked sensitive, and absent from
    /// both the request URL and its diagnostic rendering.
    #[test]
    fn brave_request_never_records_credential_in_url_or_debug() {
        let built = brave_request();
        let diagnostic = format!("{built:?}");
        let header = built
            .headers()
            .get(BRAVE_SUBSCRIPTION_TOKEN_HEADER)
            .expect("credential header is present");

        assert!(!built.url().as_str().contains(SYNTHETIC_KEY));
        assert!(header.is_sensitive());
        assert!(!diagnostic.contains(SYNTHETIC_KEY));
    }

    /// INV-035: a query containing the resolved API key fails before a request
    /// URL can leave the builder or be dispatched.
    #[test]
    fn brave_request_rejects_query_credential_collision() {
        assert!(matches!(
            build_brave_request(SYNTHETIC_KEY, SYNTHETIC_KEY),
            Err(WebSearchTransportFailure::RequestFailed)
        ));
    }

    /// INV-035: a key matching fixed provider URL text fails before the URL
    /// can be dispatched or recorded.
    #[test]
    fn brave_request_rejects_fixed_url_credential_collision() {
        assert!(matches!(
            build_brave_request(FIXTURE_QUERY, URL_SCHEME_COLLISION_KEY),
            Err(WebSearchTransportFailure::RequestFailed)
        ));
    }

    /// INV-035: a key matching fixed request metadata fails before the request
    /// diagnostic can leave the transport boundary.
    #[test]
    fn brave_request_rejects_fixed_header_credential_collision() {
        assert!(matches!(
            build_brave_request(FIXTURE_QUERY, ACCEPT_HEADER_COLLISION_KEY),
            Err(WebSearchTransportFailure::RequestFailed)
        ));
    }

    /// INV-035: a provider status equal to the API key is retained for
    /// request-scoped sanitization but omitted from raw public diagnostics.
    #[test]
    fn web_search_transport_diagnostics_omit_credential_colliding_status() {
        let status_collision_key = PROVIDER_REJECTION_STATUS.to_string();
        let provider_error = WebSearchProviderError::new(
            PROVIDER_REJECTION_STATUS,
            br#"{"message":"synthetic rejection"}"#.to_vec(),
        )
        .expect("fixture provider error is admitted");

        assert!(!format!("{provider_error:?}").contains(&status_collision_key));
        assert!(!provider_error.to_string().contains(&status_collision_key));

        let failure = WebSearchTransportFailure::ProviderRejected(provider_error);

        assert!(!format!("{failure:?}").contains(&status_collision_key));
        assert!(!failure.to_string().contains(&status_collision_key));
        assert!(failure.source().is_some());
    }

    /// INV-035: a credential colliding with fixed provider-rejection prose is
    /// rejected before that public diagnostic can leave the transport.
    #[test]
    fn web_search_transport_rejects_credential_colliding_provider_prose() {
        const PROVIDER_PROSE_COLLISION_KEY: &str = "provider";
        let credential = CredentialValue::new(PROVIDER_PROSE_COLLISION_KEY.as_bytes().to_vec());
        let provider_error = WebSearchProviderError::new(
            PROVIDER_REJECTION_STATUS,
            br#"{"message":"synthetic rejection"}"#.to_vec(),
        )
        .expect("fixture provider error is admitted");
        let failure = credential_safe_transport_failure(
            WebSearchTransportFailure::ProviderRejected(provider_error),
            &credential,
        );

        assert!(!format!("{failure:?}").contains(PROVIDER_PROSE_COLLISION_KEY));
        assert!(!failure.to_string().contains(PROVIDER_PROSE_COLLISION_KEY));
        assert!(failure.source().is_none());
        assert!(matches!(
            failure,
            WebSearchTransportFailure::CredentialDiagnosticCollision(_)
        ));
    }

    /// INV-035: every transport failure is sanitized against its request key,
    /// including a pre-dispatch fixed-URL collision whose error prose overlaps.
    #[test]
    fn web_search_transport_sanitizes_fixed_url_and_failure_prose_collision() {
        const SEARCH_PROSE_COLLISION_KEY: &str = "search";
        let credential = CredentialValue::new(SEARCH_PROSE_COLLISION_KEY.as_bytes().to_vec());
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let client = Client::builder()
            .build()
            .expect("fixture HTTP client builds without network");
        let failure = build_provider_request(&client, &request, &credential)
            .expect_err("fixed endpoint collides with the fixture credential");
        let failure = credential_safe_transport_failure(failure, &credential);

        assert!(!format!("{failure:?}").contains(SEARCH_PROSE_COLLISION_KEY));
        assert!(!failure.to_string().contains(SEARCH_PROSE_COLLISION_KEY));
        assert!(failure.source().is_none());
        assert!(matches!(
            failure,
            WebSearchTransportFailure::CredentialDiagnosticCollision(_)
        ));
    }

    /// INV-035: sanitizing a dispatch-unknown diagnostic preserves its
    /// commit-ambiguous classification through a credential-safe executor error.
    #[test]
    fn web_search_sanitized_dispatch_unknown_stays_commit_ambiguous() {
        const UNKNOWN_PROSE_COLLISION_KEY: &str = "unknown";
        let credential = CredentialValue::new(UNKNOWN_PROSE_COLLISION_KEY.as_bytes().to_vec());
        let failure = credential_safe_transport_failure(
            WebSearchTransportFailure::DispatchUnknown,
            &credential,
        );

        assert!(!format!("{failure:?}").contains(UNKNOWN_PROSE_COLLISION_KEY));
        assert!(!failure.to_string().contains(UNKNOWN_PROSE_COLLISION_KEY));

        let executor_error = collision_executor_error(failure);

        assert!(!format!("{executor_error:?}").contains(UNKNOWN_PROSE_COLLISION_KEY));
        assert!(
            !executor_error
                .to_string()
                .contains(UNKNOWN_PROSE_COLLISION_KEY)
        );
        assert_eq!(
            executor_error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true
            }
        );
    }

    /// INV-035: generating a safe transport diagnostic fails closed when the
    /// ordinary redaction sentinel itself contains the credential.
    #[test]
    fn web_search_transport_diagnostic_redaction_overlap_fails_closed() {
        const DIAGNOSTIC_REDACTION_OVERLAP_KEY: &str = "e";
        let credential = CredentialValue::new(DIAGNOSTIC_REDACTION_OVERLAP_KEY.as_bytes().to_vec());
        let failure = credential_safe_transport_failure(
            WebSearchTransportFailure::RequestFailed,
            &credential,
        );

        assert!(!format!("{failure:?}").contains(DIAGNOSTIC_REDACTION_OVERLAP_KEY));
        assert!(
            !failure
                .to_string()
                .contains(DIAGNOSTIC_REDACTION_OVERLAP_KEY)
        );
        assert!(matches!(
            failure,
            WebSearchTransportFailure::CredentialDiagnosticCollision(_)
        ));
    }

    /// INV-035: provider response and error diagnostics never render
    /// provider-controlled fields that could reflect the API key.
    #[test]
    fn web_search_debug_output_omits_reflected_credential() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(SYNTHETIC_KEY),
            url: format!("{FIXTURE_RESULT_URL}?token={SYNTHETIC_KEY}"),
            snippet: String::from(SYNTHETIC_KEY),
        })
        .expect("reflected fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let error = WebSearchProviderError::new(
            PROVIDER_REJECTION_STATUS,
            SYNTHETIC_KEY.as_bytes().to_vec(),
        )
        .expect("fixture error body is bounded");

        assert!(!format!("{response:?}").contains(SYNTHETIC_KEY));
        assert!(!format!("{error:?}").contains(SYNTHETIC_KEY));
    }

    /// The recorded synthetic Brave envelope decodes only web results and the
    /// provider's pagination fact; no transport or network is involved.
    #[test]
    fn brave_recorded_response_decodes_structured_results() {
        let body = serde_json::to_vec(&serde_json::json!({
            "type": "search",
            "query": {
                "original": FIXTURE_QUERY,
                "more_results_available": false,
            },
            "web": {
                "type": "search",
                "results": [{
                    "type": "search_result",
                    "title": FIXTURE_RESULT_TITLE,
                    "url": FIXTURE_RESULT_URL,
                    "description": FIXTURE_RESULT_SNIPPET,
                }],
            },
        }))
        .expect("recorded response fixture encodes");

        let response = decode_provider_response(WebSearchProvider::Brave, &body)
            .expect("recorded provider response decodes");
        let [decoded] = response.results() else {
            panic!("recorded response contains one web result")
        };

        assert_eq!(decoded.title(), FIXTURE_RESULT_TITLE);
        assert_eq!(decoded.url(), FIXTURE_RESULT_URL);
        assert_eq!(decoded.snippet(), FIXTURE_RESULT_SNIPPET);
        assert!(!response.more_results_available());
    }

    /// A success envelope without provider pagination facts cannot claim that
    /// its bounded result page is complete.
    #[test]
    fn brave_response_without_pagination_facts_is_invalid() {
        let body = serde_json::to_vec(&serde_json::json!({
            "type": "search",
            "web": {
                "type": "search",
                "results": [],
            },
        }))
        .expect("recorded response fixture encodes");

        assert!(matches!(
            decode_provider_response(WebSearchProvider::Brave, &body),
            Err(WebSearchTransportFailure::InvalidResponse)
        ));
    }
}
