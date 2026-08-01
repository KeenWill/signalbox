//! Bounded credentialed web search with an explicit provider boundary.

use std::{error::Error, fmt, future::Future, time::Duration};

use futures_util::StreamExt;
use icu_casemap::CaseMapper;
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
    NormalizedToolArguments, ToolAttemptDispatchCorrelation, ToolEffectClass,
    ToolExecutionErrorDetail, ToolPermissionDefault, ToolResultText,
};
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialReference, CredentialValue, redact_text,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};
use unicode_normalization::UnicodeNormalization;

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
const PROVIDER_REJECTED_DETAIL: &str = "web search provider rejected the request";
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
const MAX_CREDENTIAL_BYTES: usize = 4 * 1024;
const MAX_BOUND_EVIDENCE_DEBUG_BYTES: usize = MAX_ERROR_DETAIL_BYTES * 2;
const MAX_OVERSIZED_CREDENTIAL_INSPECTION_BYTES: usize = MAX_BOUND_EVIDENCE_DEBUG_BYTES;
const MAX_REVERSIBLE_DECODE_PASSES: usize = 4;
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
        let provider_rejected_detail =
            ToolExecutionErrorDetail::try_new(String::from(PROVIDER_REJECTED_DETAIL))
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
                provider_rejected_detail,
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
    source_url: String,
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
                source_url: fields.url,
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
        WebSearchResponseDebug {
            result_count: self.results.len(),
            completeness: self.completeness,
        }
        .fmt(formatter)
    }
}

#[derive(Clone, Copy)]
struct WebSearchResponseDebug {
    result_count: usize,
    completeness: WebSearchPageCompleteness,
}

impl fmt::Debug for WebSearchResponseDebug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchResponse")
            .field("result_count", &self.result_count)
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
        match self.completeness {
            WebSearchPageCompleteness::Complete => false,
            WebSearchPageCompleteness::MoreAvailable => true,
        }
    }
}

/// Opaque complete provider error body retained for request-key sanitization.
pub struct WebSearchProviderError {
    status: u16,
    body: Vec<u8>,
    body_failure_class: Option<WebSearchTransportFailureClass>,
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
        let status_code = StatusCode::from_u16(status).ok()?;
        (!status_code.is_success() && body.len() <= MAX_PROVIDER_RESPONSE_BYTES).then_some(Self {
            status,
            body,
            body_failure_class: None,
        })
    }

    fn with_body_failure_class(mut self, failure_class: WebSearchTransportFailureClass) -> Self {
        self.body_failure_class = Some(failure_class);
        self
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
    failure_class: WebSearchCredentialDiagnosticClass,
    transport_failure_class: Option<WebSearchTransportFailureClass>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebSearchCredentialDiagnosticClass {
    CallerOrHubBug,
    InfrastructureCommitAmbiguous,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebSearchTransportFailureClass {
    InvalidCredential,
    CredentialDiagnosticCollision,
    RequestFailed,
    ProviderRejected,
    InvalidResponse,
    ResponseTooLarge,
    DispatchUnknown,
}

impl WebSearchTransportFailure {
    const fn class(&self) -> WebSearchTransportFailureClass {
        match self {
            Self::InvalidCredential => WebSearchTransportFailureClass::InvalidCredential,
            Self::CredentialDiagnosticCollision(_) => {
                WebSearchTransportFailureClass::CredentialDiagnosticCollision
            }
            Self::RequestFailed => WebSearchTransportFailureClass::RequestFailed,
            Self::ProviderRejected(_) => WebSearchTransportFailureClass::ProviderRejected,
            Self::InvalidResponse => WebSearchTransportFailureClass::InvalidResponse,
            Self::ResponseTooLarge => WebSearchTransportFailureClass::ResponseTooLarge,
            Self::DispatchUnknown => WebSearchTransportFailureClass::DispatchUnknown,
        }
    }

    const fn response_body_failure_class(&self) -> Option<WebSearchTransportFailureClass> {
        match self {
            Self::ProviderRejected(error) => error.body_failure_class,
            Self::InvalidCredential
            | Self::CredentialDiagnosticCollision(_)
            | Self::RequestFailed
            | Self::InvalidResponse
            | Self::ResponseTooLarge
            | Self::DispatchUnknown => None,
        }
    }
}

/// Credential-sanitized result of one injected transport request.
pub struct WebSearchTransportOutcome {
    result: Result<WebSearchResponse, WebSearchTransportFailure>,
}

impl WebSearchTransportOutcome {
    /// Builds one completed outcome after request-scoped diagnostic checks.
    pub fn completed(response: WebSearchResponse, credential: &CredentialValue) -> Self {
        credential_safe_transport_outcome(Ok(response), credential)
    }

    /// Builds one failed outcome after request-scoped diagnostic checks.
    pub fn failed(failure: WebSearchTransportFailure, credential: &CredentialValue) -> Self {
        credential_safe_transport_outcome(Err(failure), credential)
    }

    fn into_result(self) -> Result<WebSearchResponse, WebSearchTransportFailure> {
        self.result
    }
}

impl fmt::Debug for WebSearchTransportOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(response) => fmt::Debug::fmt(response, formatter),
            Err(failure) => fmt::Debug::fmt(failure, formatter),
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
    ) -> impl Future<Output = WebSearchTransportOutcome> + Send;
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
    ) -> WebSearchTransportOutcome {
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
            let body = collect_complete_body(response).await;
            finish_provider_response(provider, status, body)
        }
        .await;
        credential_safe_transport_outcome(outcome, credential)
    }
}

fn credential_safe_transport_outcome(
    outcome: Result<WebSearchResponse, WebSearchTransportFailure>,
    credential: &CredentialValue,
) -> WebSearchTransportOutcome {
    let sanitized = match outcome {
        Ok(response) => {
            let credential_text =
                std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
            if credential_text.is_empty()
                || text_contains_credential_variant(&format!("{response:?}"), credential_text)
            {
                Err(WebSearchTransportFailure::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: safe_collision_diagnostic(credential_text),
                        failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                        transport_failure_class: None,
                    },
                ))
            } else {
                Ok(response)
            }
        }
        Err(failure) => Err(credential_safe_transport_failure(failure, credential)),
    };
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let result = if credential_text.is_empty()
        || text_contains_credential_variant(&format!("{sanitized:?}"), credential_text)
    {
        let failure_class = match &sanitized {
            Ok(_) => WebSearchCredentialDiagnosticClass::CallerOrHubBug,
            Err(failure) => transport_failure_diagnostic_class(failure),
        };
        let transport_failure_class = match &sanitized {
            Ok(_) => None,
            Err(failure) => transport_failure_source_class(failure),
        };
        Err(WebSearchTransportFailure::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class,
                transport_failure_class,
            },
        ))
    } else {
        sanitized
    };
    WebSearchTransportOutcome { result }
}

fn transport_failure_diagnostic_class(
    failure: &WebSearchTransportFailure,
) -> WebSearchCredentialDiagnosticClass {
    match failure {
        WebSearchTransportFailure::InvalidCredential
        | WebSearchTransportFailure::RequestFailed
        | WebSearchTransportFailure::ProviderRejected(_)
        | WebSearchTransportFailure::InvalidResponse
        | WebSearchTransportFailure::ResponseTooLarge => {
            WebSearchCredentialDiagnosticClass::CallerOrHubBug
        }
        WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.failure_class
        }
        WebSearchTransportFailure::DispatchUnknown => {
            WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
        }
    }
}

fn transport_failure_source_class(
    failure: &WebSearchTransportFailure,
) -> Option<WebSearchTransportFailureClass> {
    match failure {
        WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.transport_failure_class
        }
        other => Some(other.class()),
    }
}

fn credential_safe_transport_failure(
    failure: WebSearchTransportFailure,
    credential: &CredentialValue,
) -> WebSearchTransportFailure {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let (source_contains_credential, executor_contains_credential) = match &failure {
        WebSearchTransportFailure::ProviderRejected(error) => (
            text_contains_credential_variant(&format!("{error:?}"), credential_text)
                || text_contains_credential_variant(&error.to_string(), credential_text),
            false,
        ),
        WebSearchTransportFailure::InvalidCredential
        | WebSearchTransportFailure::CredentialDiagnosticCollision(_)
        | WebSearchTransportFailure::RequestFailed
        | WebSearchTransportFailure::InvalidResponse
        | WebSearchTransportFailure::ResponseTooLarge => (false, false),
        WebSearchTransportFailure::DispatchUnknown => {
            let executor_error = WebSearchExecutorError::DispatchUnknown;
            (
                false,
                text_contains_credential_variant(&format!("{executor_error:?}"), credential_text)
                    || text_contains_credential_variant(
                        &executor_error.to_string(),
                        credential_text,
                    ),
            )
        }
    };
    if credential_text.is_empty()
        || text_contains_credential_variant(&format!("{failure:?}"), credential_text)
        || text_contains_credential_variant(&failure.to_string(), credential_text)
        || source_contains_credential
        || executor_contains_credential
    {
        let transport_failure_class = transport_failure_source_class(&failure);
        WebSearchTransportFailure::CredentialDiagnosticCollision(WebSearchCredentialDiagnostic {
            rendered: safe_collision_diagnostic(credential_text),
            failure_class: transport_failure_diagnostic_class(&failure),
            transport_failure_class,
        })
    } else {
        failure
    }
}

fn safe_collision_diagnostic(credential: &str) -> String {
    const DIAGNOSTIC: &str = "web search credential diagnostic suppressed";
    const REDACTION: &str = "[redacted]";
    let redacted = DIAGNOSTIC.replace(credential, REDACTION);
    if !credential.is_empty() && !text_contains_credential_variant(&redacted, credential) {
        return redacted;
    }
    if credential == "!" {
        String::from("?")
    } else {
        String::from("!")
    }
}

fn credential_safe_executor_error(
    error: WebSearchExecutorError,
    credential: &CredentialValue,
) -> WebSearchExecutorError {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let failure_class = executor_error_diagnostic_class(&error);
    let transport_failure_class = executor_error_transport_failure_class(&error);
    if credential_text.is_empty()
        || text_contains_credential_variant(&format!("{error:?}"), credential_text)
        || text_contains_credential_variant(&error.to_string(), credential_text)
    {
        WebSearchExecutorError::CredentialDiagnosticCollision(WebSearchCredentialDiagnostic {
            rendered: safe_collision_diagnostic(credential_text),
            failure_class,
            transport_failure_class,
        })
    } else {
        error
    }
}

fn executor_error_transport_failure_class(
    error: &WebSearchExecutorError,
) -> Option<WebSearchTransportFailureClass> {
    match error {
        WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.transport_failure_class
        }
        WebSearchExecutorError::ArgumentValidationDrift
        | WebSearchExecutorError::EvidenceEncoding
        | WebSearchExecutorError::DispatchUnknown => None,
    }
}

fn executor_error_diagnostic_class(
    error: &WebSearchExecutorError,
) -> WebSearchCredentialDiagnosticClass {
    match error {
        WebSearchExecutorError::ArgumentValidationDrift
        | WebSearchExecutorError::EvidenceEncoding => {
            WebSearchCredentialDiagnosticClass::CallerOrHubBug
        }
        WebSearchExecutorError::DispatchUnknown => {
            WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
        }
        WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic) => {
            diagnostic.failure_class
        }
    }
}

fn build_provider_request(
    client: &Client,
    request: &WebSearchRequest,
    credential: &CredentialValue,
) -> Result<reqwest::Request, WebSearchTransportFailure> {
    let endpoint = request.provider.endpoint();
    if has_http_header_boundary_whitespace(credential.expose_bytes()) {
        return Err(WebSearchTransportFailure::InvalidCredential);
    }
    let credential_text = std::str::from_utf8(credential.expose_bytes())
        .map_err(|_| WebSearchTransportFailure::InvalidCredential)?;
    if credential_text.is_empty() {
        return Err(WebSearchTransportFailure::InvalidCredential);
    }
    if query_contains_credential(request.query(), credential_text)
        || fixed_request_metadata_contains_credential(request, credential_text)
        || credential_debug_contains_credential(credential, credential_text)
    {
        return Err(WebSearchTransportFailure::RequestFailed);
    }
    let url = provider_request_url(request).ok_or(WebSearchTransportFailure::RequestFailed)?;
    if text_contains_credential_variant(url.as_str(), credential_text) {
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
    if text_contains_credential_variant(&format!("{http_request:?}"), credential_text) {
        return Err(WebSearchTransportFailure::RequestFailed);
    }
    Ok(http_request)
}

fn query_contains_credential(query: &str, credential: &str) -> bool {
    text_contains_credential_variant(query, credential)
}

fn provider_request_url(request: &WebSearchRequest) -> Option<Url> {
    let mut url = Url::parse(request.provider.endpoint().url).ok()?;
    url.query_pairs_mut()
        .append_pair("q", request.query())
        .append_pair("count", BRAVE_RESULT_COUNT_QUERY)
        .append_pair("result_filter", "web")
        .append_pair("text_decorations", "false");
    Some(url)
}

fn serialized_request_url_contains_credential(
    request: &WebSearchRequest,
    credential: &str,
) -> bool {
    provider_request_url(request)
        .is_none_or(|url| text_contains_credential_variant(url.as_str(), credential))
}

fn credential_debug_contains_credential(
    credential: &CredentialValue,
    credential_text: &str,
) -> bool {
    text_contains_credential_variant(&format!("{credential:?}"), credential_text)
}

fn request_credential_debug_contains_credential(
    request: &WebSearchRequest,
    credential: &CredentialValue,
    credential_text: &str,
) -> bool {
    text_contains_credential_variant(&format!("{request:?} {credential:?}"), credential_text)
}

fn fixed_success_payload_contains_credential(credential: &str) -> bool {
    fixed_success_payloads().any(|payload| text_contains_credential_variant(&payload, credential))
}

fn fixed_success_payloads() -> impl Iterator<Item = String> {
    (0..=MAX_RETURNED_RESULTS).flat_map(|result_count| {
        [false, true].into_iter().map(move |truncated| {
            let results = (0..result_count)
                .map(|_| serde_json::json!({"title": "", "url": "", "snippet": ""}))
                .collect::<Vec<_>>();
            let payload = serde_json::json!({
                "results": results,
                "truncated": truncated,
            });
            payload.to_string()
        })
    })
}

const fn next_page_completeness_probe(
    completeness: WebSearchPageCompleteness,
) -> Option<WebSearchPageCompleteness> {
    match completeness {
        WebSearchPageCompleteness::Complete => Some(WebSearchPageCompleteness::MoreAvailable),
        WebSearchPageCompleteness::MoreAvailable => None,
    }
}

fn fixed_request_metadata_contains_credential(
    request: &WebSearchRequest,
    credential: &str,
) -> bool {
    let endpoint = request.provider.endpoint();
    [
        endpoint.url,
        endpoint.credential_header,
        "GET",
        ACCEPT.as_str(),
        "application/json",
        "q",
        "count",
        BRAVE_RESULT_COUNT_QUERY,
        "result_filter",
        "web",
        "text_decorations",
        "false",
        "results",
        "truncated",
        "title",
        "url",
        "snippet",
        "true",
    ]
    .into_iter()
    .any(|value| text_contains_credential_variant(value, credential))
        || text_contains_credential_variant(&format!("{request:?}"), credential)
        || fixed_success_payload_contains_credential(credential)
        || (0..=MAX_PROVIDER_RESULTS).any(|result_count| {
            std::iter::successors(Some(WebSearchPageCompleteness::Complete), |completeness| {
                next_page_completeness_probe(*completeness)
            })
            .any(|completeness| {
                text_contains_credential_variant(
                    &format!(
                        "{:?}",
                        WebSearchResponseDebug {
                            result_count,
                            completeness,
                        }
                    ),
                    credential,
                )
            })
        })
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

fn finish_provider_response(
    provider: WebSearchProvider,
    status: StatusCode,
    body: Result<Vec<u8>, WebSearchTransportFailure>,
) -> Result<WebSearchResponse, WebSearchTransportFailure> {
    if !status.is_success() {
        let (complete_body, body_failure_class) = match body {
            Ok(complete_body) => (complete_body, None),
            Err(failure) => (Vec::new(), Some(failure.class())),
        };
        let error = WebSearchProviderError::new(status.as_u16(), complete_body)
            .ok_or(WebSearchTransportFailure::ResponseTooLarge)?;
        let error = match body_failure_class {
            Some(failure_class) => error.with_body_failure_class(failure_class),
            None => error,
        };
        return Err(WebSearchTransportFailure::ProviderRejected(error));
    }
    if status != StatusCode::OK {
        return Err(WebSearchTransportFailure::InvalidResponse);
    }
    decode_provider_response(provider, &body?)
}

#[derive(serde::Deserialize)]
struct BraveResponse {
    #[serde(rename = "type")]
    response_type: String,
    query: BraveQueryFacts,
    web: Option<BraveWebResults>,
}

#[derive(serde::Deserialize)]
struct BraveQueryFacts {
    more_results_available: bool,
}

#[derive(serde::Deserialize)]
struct BraveWebResults {
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
    provider_rejected_detail: ToolExecutionErrorDetail,
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
    /// A diagnostic collided with its request credential.
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
            Self::DispatchUnknown => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::CredentialDiagnosticCollision(diagnostic) => match diagnostic.failure_class {
                WebSearchCredentialDiagnosticClass::CallerOrHubBug => {
                    OperatorFailureClass::CallerOrHubBug
                }
                WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous => {
                    OperatorFailureClass::Infrastructure {
                        commit_ambiguous: true,
                    }
                }
            },
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
        correlation: &ToolAttemptDispatchCorrelation,
    ) -> WebSearchRequestOutcome {
        let credential = match self
            .credentials
            .resolve(&self.configuration.credential_reference)
            .await
        {
            Ok(credential) => credential,
            Err(error) => {
                report_credential_access_failure(&error, correlation);
                return WebSearchRequestOutcome::Evidence(
                    WebSearchRequestEvidence::Uncredentialed(ToolExecutorEvidence::KnownFailed {
                        detail: Some(self.credential_unavailable_detail.clone()),
                    }),
                );
            }
        };
        let Some(scrubber) = CredentialScrubber::try_new(&credential) else {
            let _reporting = report_credential_value_failure(correlation, &credential);
            let credential_text =
                std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
            let credential_is_oversized = credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES;
            let detail = (credential_is_oversized
                || !fixed_outer_error_debug_may_contain(credential_text))
            .then(|| self.credential_unavailable_detail.clone());
            let evidence = ToolExecutorEvidence::KnownFailed { detail };
            if credential_is_oversized && !credential_text.is_empty() {
                let Some(credential) =
                    BoundedCredentialVariants::try_from_oversized(credential_text)
                else {
                    return WebSearchRequestOutcome::CredentialFreeError(
                        WebSearchExecutorError::CredentialDiagnosticCollision(
                            WebSearchCredentialDiagnostic {
                                rendered: String::new(),
                                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                                transport_failure_class: None,
                            },
                        ),
                    );
                };
                return WebSearchRequestOutcome::Evidence(
                    WebSearchRequestEvidence::BoundedCredentialVariants {
                        evidence,
                        credential,
                    },
                );
            }
            let retain_for_bound_diagnostic = credential.expose_bytes().len()
                <= MAX_CREDENTIAL_BYTES
                && !credential_text.is_empty();
            return if retain_for_bound_diagnostic {
                WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                    evidence,
                    credential,
                })
            } else {
                WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Uncredentialed(
                    evidence,
                ))
            };
        };
        let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
        if fixed_populated_failure_detail_collides(&scrubber, &self.credential_unavailable_detail) {
            return WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                evidence: ToolExecutorEvidence::KnownFailed { detail: None },
                credential,
            });
        }
        if fixed_bound_evidence_token_collides(&scrubber)
            || fixed_bound_wrapper_token_collides(&scrubber, correlation)
        {
            return WebSearchRequestOutcome::Error {
                error: WebSearchExecutorError::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: safe_collision_diagnostic(credential_text),
                        failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                        transport_failure_class: None,
                    },
                ),
                credential,
            };
        }
        if query_contains_credential(request.query(), credential_text)
            || fixed_request_metadata_contains_credential(&request, credential_text)
            || serialized_request_url_contains_credential(&request, credential_text)
            || credential_debug_contains_credential(&credential, credential_text)
            || request_credential_debug_contains_credential(&request, &credential, credential_text)
        {
            let outcome = known_failure_evidence(self.request_failed_detail.clone(), &scrubber);
            return match outcome {
                Ok(evidence) => {
                    WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                        evidence,
                        credential,
                    })
                }
                Err(error) => WebSearchRequestOutcome::Error { error, credential },
            };
        }
        let transport_result = self
            .transport
            .search(request, &credential)
            .await
            .into_result();
        if let Err(failure) = &transport_result
            && let Some(failure_class) = failure.response_body_failure_class()
        {
            let _reporting = report_response_body_failure(failure_class, correlation, &credential);
        }
        if let Err(failure) = &transport_result {
            let _reporting = report_transport_failure(failure, correlation, &credential);
        }
        let outcome = match transport_result {
            Ok(response) => match success_evidence(response, &scrubber) {
                Ok(evidence) => Ok(evidence),
                Err(WebSearchExecutorError::EvidenceEncoding) => {
                    let _reporting = report_response_sanitization_failure(correlation, &credential);
                    Ok(self.invalid_response_evidence(&scrubber))
                }
                Err(error) => Err(error),
            },
            Err(WebSearchTransportFailure::InvalidCredential) => {
                known_failure_evidence(self.credential_unavailable_detail.clone(), &scrubber)
            }
            Err(WebSearchTransportFailure::CredentialDiagnosticCollision(diagnostic)) => {
                self.credential_diagnostic_evidence(diagnostic, &scrubber)
            }
            Err(WebSearchTransportFailure::RequestFailed) => {
                known_failure_evidence(self.request_failed_detail.clone(), &scrubber)
            }
            Err(WebSearchTransportFailure::ProviderRejected(error)) => {
                provider_error_detail(error, &scrubber)
                    .map(|detail| ToolExecutorEvidence::KnownFailed { detail })
            }
            Err(
                WebSearchTransportFailure::InvalidResponse
                | WebSearchTransportFailure::ResponseTooLarge,
            ) => known_failure_evidence(self.invalid_response_detail.clone(), &scrubber),
            Err(WebSearchTransportFailure::DispatchUnknown) => {
                Err(WebSearchExecutorError::DispatchUnknown)
            }
        };
        match outcome {
            Ok(evidence) => {
                WebSearchRequestOutcome::Evidence(WebSearchRequestEvidence::Credentialed {
                    evidence,
                    credential,
                })
            }
            Err(error) => WebSearchRequestOutcome::Error {
                error: credential_safe_executor_error(error, &credential),
                credential,
            },
        }
    }

    fn credential_diagnostic_evidence(
        &self,
        diagnostic: WebSearchCredentialDiagnostic,
        scrubber: &CredentialScrubber,
    ) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
        match diagnostic.transport_failure_class {
            Some(WebSearchTransportFailureClass::InvalidCredential) => {
                known_failure_evidence(self.credential_unavailable_detail.clone(), scrubber)
            }
            Some(WebSearchTransportFailureClass::RequestFailed) => {
                known_failure_evidence(self.request_failed_detail.clone(), scrubber)
            }
            Some(WebSearchTransportFailureClass::ProviderRejected) => {
                known_failure_evidence(self.provider_rejected_detail.clone(), scrubber)
            }
            Some(
                WebSearchTransportFailureClass::InvalidResponse
                | WebSearchTransportFailureClass::ResponseTooLarge,
            ) => known_failure_evidence(self.invalid_response_detail.clone(), scrubber),
            Some(WebSearchTransportFailureClass::DispatchUnknown) => Err(
                WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic),
            ),
            Some(WebSearchTransportFailureClass::CredentialDiagnosticCollision) | None => {
                match diagnostic.failure_class {
                    WebSearchCredentialDiagnosticClass::CallerOrHubBug => {
                        known_failure_evidence(self.credential_unavailable_detail.clone(), scrubber)
                    }
                    WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous => Err(
                        WebSearchExecutorError::CredentialDiagnosticCollision(diagnostic),
                    ),
                }
            }
        }
    }

    fn invalid_response_evidence(&self, scrubber: &CredentialScrubber) -> ToolExecutorEvidence {
        let detail = (!scrubber
            .contains_case_normalized_credential(self.invalid_response_detail.as_str()))
        .then(|| self.invalid_response_detail.clone());
        ToolExecutorEvidence::KnownFailed { detail }
    }
}

enum WebSearchRequestOutcome {
    Evidence(WebSearchRequestEvidence),
    CredentialFreeError(WebSearchExecutorError),
    Error {
        error: WebSearchExecutorError,
        credential: CredentialValue,
    },
}

#[cfg(test)]
impl WebSearchRequestOutcome {
    fn into_result(self) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
        match self {
            Self::Evidence(evidence) => Ok(evidence.into_evidence()),
            Self::CredentialFreeError(error) => Err(error),
            Self::Error { error, .. } => Err(error),
        }
    }
}

enum WebSearchRequestEvidence {
    Uncredentialed(ToolExecutorEvidence),
    Credentialed {
        evidence: ToolExecutorEvidence,
        credential: CredentialValue,
    },
    BoundedCredentialVariants {
        evidence: ToolExecutorEvidence,
        credential: BoundedCredentialVariants,
    },
}

impl fmt::Debug for WebSearchRequestEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uncredentialed(evidence)
            | Self::Credentialed { evidence, .. }
            | Self::BoundedCredentialVariants { evidence, .. } => {
                fmt::Debug::fmt(evidence, formatter)
            }
        }
    }
}

#[cfg(test)]
impl WebSearchRequestEvidence {
    fn into_evidence(self) -> ToolExecutorEvidence {
        match self {
            Self::Uncredentialed(evidence)
            | Self::Credentialed { evidence, .. }
            | Self::BoundedCredentialVariants { evidence, .. } => evidence,
        }
    }
}

struct BoundedCredentialVariants {
    variants: Vec<String>,
    complete: bool,
}

impl BoundedCredentialVariants {
    fn try_from_oversized(credential: &str) -> Option<Self> {
        if credential.len() > MAX_OVERSIZED_CREDENTIAL_INSPECTION_BYTES {
            return None;
        }
        let mut variants = Vec::new();
        retain_bounded_credential_variant(&mut variants, credential);
        let Some((mut decoded, changed)) = decode_reversible_text_once(credential) else {
            return Some(Self {
                variants,
                complete: false,
            });
        };
        if !changed {
            return Some(Self {
                variants,
                complete: true,
            });
        }
        retain_bounded_credential_variant(&mut variants, &decoded);
        for _ in 1..MAX_REVERSIBLE_DECODE_PASSES {
            let Some((next, changed)) = decode_reversible_text_once(&decoded) else {
                return Some(Self {
                    variants,
                    complete: false,
                });
            };
            if !changed {
                return Some(Self {
                    variants,
                    complete: true,
                });
            }
            retain_bounded_credential_variant(&mut variants, &next);
            decoded = next;
        }
        let complete = decode_reversible_text_once(&decoded).is_some_and(|(_, changed)| !changed);
        Some(Self { variants, complete })
    }

    fn collides(&self, rendered: &str, check: BoundDiagnosticCheck) -> bool {
        if !self.complete || rendered.len() > MAX_BOUND_EVIDENCE_DEBUG_BYTES {
            return true;
        }
        self.variants.iter().any(|credential| {
            let check_variant = match check {
                BoundDiagnosticCheck::AllCredentialVariants => true,
                BoundDiagnosticCheck::PreserveDefinitiveFailureWord => {
                    !unicode_case_insensitive_contains("Failed", credential)
                }
            };
            check_variant && bound_diagnostic_contains_credential(rendered, credential)
        })
    }
}

fn retain_bounded_credential_variant(variants: &mut Vec<String>, candidate: &str) {
    if !candidate.is_empty()
        && candidate.len() <= MAX_BOUND_EVIDENCE_DEBUG_BYTES
        && !variants.iter().any(|retained| retained == candidate)
    {
        variants.push(String::from(candidate));
    }
    let normalized = unicode_case_folded_nfc(candidate);
    if !normalized.is_empty()
        && normalized.len() <= MAX_BOUND_EVIDENCE_DEBUG_BYTES
        && !variants.iter().any(|retained| retained == &normalized)
    {
        variants.push(normalized);
    }
}

enum BoundCredentialCheck {
    None,
    Exact(CredentialValue),
    BoundedVariants(BoundedCredentialVariants),
}

fn bind_request_outcome(
    invocation: ToolExecutionInvocation,
    outcome: WebSearchRequestOutcome,
) -> Result<CorrelatedToolExecutorEvidence, WebSearchExecutorError> {
    let evidence = match outcome {
        WebSearchRequestOutcome::Evidence(evidence) => evidence,
        WebSearchRequestOutcome::CredentialFreeError(error) => return Err(error),
        WebSearchRequestOutcome::Error { error, credential } => {
            let credential_text =
                std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
            let rendered_result = format!(
                "{:?}",
                Result::<&CorrelatedToolExecutorEvidence, _>::Err(&error)
            );
            if !credential_text.is_empty()
                && !text_contains_credential_variant(&rendered_result, credential_text)
            {
                return Err(error);
            }
            if executor_error_diagnostic_class(&error)
                == WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
            {
                return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                    WebSearchCredentialDiagnostic {
                        rendered: safe_collision_diagnostic(credential_text),
                        failure_class:
                            WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous,
                        transport_failure_class: executor_error_transport_failure_class(&error),
                    },
                ));
            }
            let fallback = invocation.bind(ToolExecutorEvidence::KnownFailed { detail: None });
            let rendered_fallback =
                format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&fallback));
            if !credential_text.is_empty() && !rendered_fallback.contains(credential_text) {
                return Ok(fallback);
            }
            return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                WebSearchCredentialDiagnostic {
                    rendered: safe_collision_diagnostic(credential_text),
                    failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                    transport_failure_class: executor_error_transport_failure_class(&error),
                },
            ));
        }
    };
    let (evidence, credential) = match evidence {
        WebSearchRequestEvidence::Uncredentialed(evidence) => {
            (evidence, BoundCredentialCheck::None)
        }
        WebSearchRequestEvidence::Credentialed {
            evidence,
            credential,
        } => (evidence, BoundCredentialCheck::Exact(credential)),
        WebSearchRequestEvidence::BoundedCredentialVariants {
            evidence,
            credential,
        } => (evidence, BoundCredentialCheck::BoundedVariants(credential)),
    };
    let bound_diagnostic_check = bound_diagnostic_check(&evidence);
    let bound = invocation.bind(evidence);
    let rendered_result = format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&bound));
    if let BoundCredentialCheck::BoundedVariants(credential) = &credential {
        if credential.collides(&rendered_result, bound_diagnostic_check) {
            return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
                WebSearchCredentialDiagnostic {
                    rendered: String::new(),
                    failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                    transport_failure_class: None,
                },
            ));
        }
        return Ok(bound);
    }
    let BoundCredentialCheck::Exact(credential) = credential else {
        return Ok(bound);
    };
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let check_rendered_collision = match bound_diagnostic_check {
        BoundDiagnosticCheck::AllCredentialVariants => true,
        BoundDiagnosticCheck::PreserveDefinitiveFailureWord => {
            !unicode_case_insensitive_contains("Failed", credential_text)
        }
    };
    if credential_text.is_empty()
        || (check_rendered_collision
            && bound_diagnostic_contains_credential(&rendered_result, credential_text))
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: None,
            },
        ));
    }
    Ok(bound)
}

fn bound_diagnostic_check(evidence: &ToolExecutorEvidence) -> BoundDiagnosticCheck {
    match evidence {
        ToolExecutorEvidence::CompletedText(_) | ToolExecutorEvidence::Ambiguous => {
            BoundDiagnosticCheck::AllCredentialVariants
        }
        ToolExecutorEvidence::KnownFailed { .. } => {
            BoundDiagnosticCheck::PreserveDefinitiveFailureWord
        }
    }
}

#[derive(Clone, Copy)]
enum BoundDiagnosticCheck {
    AllCredentialVariants,
    PreserveDefinitiveFailureWord,
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
        let correlation = invocation.correlation();
        let outcome = self.execute_request(request, &correlation).await;
        bind_request_outcome(invocation, outcome)
    }
}

fn report_credential_access_failure(
    error: &CredentialAccessError,
    correlation: &ToolAttemptDispatchCorrelation,
) {
    tracing::warn!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        failure = ?error.failure,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search credential resolution failed"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialValueFailure {
    Unusable,
}

fn report_credential_value_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    if credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: String::new(),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: None,
            },
        ));
    }
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search credential value was unusable failure={:?} session_id={} turn_id={}",
        CredentialValueFailure::Unusable,
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: None,
            },
        ));
    }
    tracing::warn!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        failure = ?CredentialValueFailure::Unusable,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search credential value was unusable"
    );
    Ok(())
}

fn report_transport_failure(
    failure: &WebSearchTransportFailure,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search transport failed failure={:?} session_id={} turn_id={}",
        failure.class(),
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: transport_failure_diagnostic_class(failure),
                transport_failure_class: transport_failure_source_class(failure),
            },
        ));
    }
    tracing::event!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        tracing::Level::WARN,
        failure = ?failure.class(),
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search transport failed"
    );
    Ok(())
}

fn report_response_body_failure(
    failure_class: WebSearchTransportFailureClass,
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search provider response body failed failure={failure_class:?} session_id={} turn_id={}",
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: Some(failure_class),
            },
        ));
    }
    tracing::event!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        tracing::Level::WARN,
        failure = ?failure_class,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search provider response body failed"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseSanitizationFailure {
    EvidenceEncoding,
}

fn report_response_sanitization_failure(
    correlation: &ToolAttemptDispatchCorrelation,
    credential: &CredentialValue,
) -> Result<(), WebSearchExecutorError> {
    let credential_text = std::str::from_utf8(credential.expose_bytes()).unwrap_or_default();
    let controlled_event = format!(
        "WARN signalbox_tools_basic_web_search: web search response sanitization failed failure={:?} session_id={} turn_id={}",
        ResponseSanitizationFailure::EvidenceEncoding,
        correlation.session().as_uuid(),
        correlation.turn().as_uuid()
    );
    if credential_text.is_empty()
        || compact_formatter_framing_may_contain(credential_text, &controlled_event)
        || text_contains_credential_variant(&controlled_event, credential_text)
    {
        return Err(WebSearchExecutorError::CredentialDiagnosticCollision(
            WebSearchCredentialDiagnostic {
                rendered: safe_collision_diagnostic(credential_text),
                failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
                transport_failure_class: None,
            },
        ));
    }
    tracing::event!(
        target: "signalbox_tools_basic_web_search",
        parent: None,
        tracing::Level::WARN,
        failure = ?ResponseSanitizationFailure::EvidenceEncoding,
        session_id = %correlation.session().as_uuid(),
        turn_id = %correlation.turn().as_uuid(),
        "web search response sanitization failed"
    );
    Ok(())
}

fn compact_formatter_framing_may_contain(credential: &str, controlled_event: &str) -> bool {
    formatter_variant_may_span_event(credential, controlled_event)
        || decoded_credential_variants(credential).is_none_or(|variants| {
            variants
                .iter()
                .any(|variant| formatter_variant_may_span_event(variant, controlled_event))
        })
}

fn formatter_variant_may_span_event(credential: &str, controlled_event: &str) -> bool {
    const DYNAMIC_METADATA_CHARACTERS: &str = "0123456789-:+.TZ \r\n";
    if credential.contains('\u{1b}')
        || credential
            .chars()
            .all(|character| DYNAMIC_METADATA_CHARACTERS.contains(character))
    {
        return true;
    }
    credential.char_indices().skip(1).any(|(split, _)| {
        let (leading, trailing) = credential.split_at(split);
        (leading
            .chars()
            .all(|character| DYNAMIC_METADATA_CHARACTERS.contains(character))
            && unicode_case_insensitive_starts_with(controlled_event, trailing))
            || (trailing
                .chars()
                .all(|character| DYNAMIC_METADATA_CHARACTERS.contains(character))
                && unicode_case_insensitive_ends_with(controlled_event, leading))
    })
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
    let truncated =
        response.results.len() > MAX_RETURNED_RESULTS || response.more_results_available();
    let results = response
        .results
        .into_iter()
        .take(MAX_RETURNED_RESULTS)
        .map(|result| {
            let normalization_conceals_credential = result.source_url != result.url
                && scrubber.contains_credential(&result.source_url)
                && !scrubber.contains_credential(&result.url);
            if normalization_conceals_credential
                || scrubber.url_contains_encoded_credential(&result.url)
            {
                return Err(WebSearchExecutorError::EvidenceEncoding);
            }
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
    if scrubber.contains_case_normalized_credential(&content) {
        return Err(WebSearchExecutorError::EvidenceEncoding);
    }
    completed_text_evidence(content)
}

fn completed_text_evidence(
    content: String,
) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
    ToolResultText::try_new(content)
        .map(|content| ToolExecutorEvidence::CompletedText(content.into_string()))
        .map_err(|_| WebSearchExecutorError::EvidenceEncoding)
}

fn known_failure_evidence(
    detail: ToolExecutionErrorDetail,
    scrubber: &CredentialScrubber,
) -> Result<ToolExecutorEvidence, WebSearchExecutorError> {
    let detail = (!scrubber.contains_case_normalized_credential(detail.as_str())).then_some(detail);
    Ok(ToolExecutorEvidence::KnownFailed { detail })
}

fn provider_error_detail(
    error: WebSearchProviderError,
    scrubber: &CredentialScrubber,
) -> Result<Option<ToolExecutionErrorDetail>, WebSearchExecutorError> {
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
    if scrubber.contains_case_normalized_credential(&bounded) {
        return Ok(None);
    }
    ToolExecutionErrorDetail::try_new(bounded)
        .map(Some)
        .map_err(|_| WebSearchExecutorError::EvidenceEncoding)
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
    decoded_variants: Vec<String>,
}

impl CredentialScrubber {
    fn try_new(credential: &CredentialValue) -> Option<Self> {
        if credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES {
            return None;
        }
        if has_http_header_boundary_whitespace(credential.expose_bytes()) {
            return None;
        }
        HeaderValue::from_bytes(credential.expose_bytes()).ok()?;
        let exact = std::str::from_utf8(credential.expose_bytes())
            .ok()?
            .to_owned();
        if exact.is_empty() || fixed_outer_error_debug_may_contain(&exact) {
            return None;
        }
        let decoded_variants = decoded_credential_variants(&exact)?;
        let encoded = serde_json::to_string(&exact).ok()?;
        let json_escaped = encoded.get(1..encoded.len().checked_sub(1)?)?.to_owned();
        Some(Self {
            exact,
            json_escaped,
            decoded_variants,
        })
    }

    fn redact_text(&self, text: &str) -> String {
        let generically_redacted = redact_text(text);
        let exact_redacted = generically_redacted.replace(&self.exact, "");
        let redacted = exact_redacted.replace(&self.json_escaped, "");
        if self.contains_credential(&redacted) {
            String::from("[redacted]")
        } else {
            redacted
        }
    }

    fn contains_credential(&self, text: &str) -> bool {
        text.contains(&self.exact)
            || text.contains(&self.json_escaped)
            || unicode_normalized_contains(text, &self.exact)
            || unicode_normalized_contains(text, &self.json_escaped)
            || unicode_case_insensitive_contains(text, &self.exact)
            || unicode_case_insensitive_contains(text, &self.json_escaped)
            || self.decoded_variants.iter().any(|variant| {
                unicode_case_insensitive_contains(text, variant)
                    || encoded_contains_credential(text, variant)
            })
            || self.contains_encoded_credential(text)
    }

    fn contains_encoded_credential(&self, text: &str) -> bool {
        encoded_contains_credential(text, &self.exact)
            || encoded_contains_credential(text, &self.json_escaped)
    }

    fn contains_case_normalized_credential(&self, text: &str) -> bool {
        self.contains_credential(text)
    }

    fn reversible_variants(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.exact.as_str()).chain(self.decoded_variants.iter().map(String::as_str))
    }

    fn url_contains_encoded_credential(&self, text: &str) -> bool {
        if self.contains_encoded_credential(text)
            || self.decoded_variants.iter().any(|variant| {
                unicode_case_insensitive_contains(text, variant)
                    || encoded_contains_credential(text, variant)
            })
        {
            return true;
        }
        if self.reversible_variants().any(|variant| {
            let slash_normalized = variant.replace('\\', "/");
            slash_normalized != variant
                && (unicode_case_insensitive_contains(text, &slash_normalized)
                    || encoded_contains_credential(text, &slash_normalized))
        }) {
            return true;
        }
        if self.reversible_variants().any(|variant| {
            normalize_url_path_dot_segments(variant).is_some_and(|normalized| {
                unicode_case_insensitive_contains(text, &normalized)
                    || encoded_contains_credential(text, &normalized)
            })
        }) {
            return true;
        }
        let Ok(url) = Url::parse(text) else {
            return true;
        };
        if self
            .reversible_variants()
            .any(|variant| url.scheme().eq_ignore_ascii_case(variant))
        {
            return true;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        if self.reversible_variants().any(|variant| {
            canonicalized_url_host(variant).is_some_and(|credential_host| {
                unicode_case_insensitive_contains(host, &credential_host)
            })
        }) {
            return true;
        }
        if let Some(result_host) = parse_ip_literal(host) {
            return self
                .reversible_variants()
                .any(|variant| parse_ip_literal(variant).is_some_and(|key| key == result_host));
        }
        if self.reversible_variants().any(|variant| {
            idna::domain_to_ascii(variant)
                .is_ok_and(|credential_host| credential_host.eq_ignore_ascii_case(host))
        }) {
            return true;
        }
        let (unicode_host, decoding) = idna::domain_to_unicode(host);
        decoding.is_err()
            || unicode_case_insensitive_contains(&unicode_host, &self.exact)
            || unicode_case_insensitive_contains(&unicode_host, &self.json_escaped)
            || self.contains_credential(&unicode_host)
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

fn normalize_url_path_dot_segments(path: &str) -> Option<String> {
    let slash_normalized = path.replace('\\', "/");
    let retain_trailing_separator =
        slash_normalized.ends_with("/.") || slash_normalized.ends_with("/..");
    let mut normalized_segments: Vec<&str> = Vec::new();
    let mut changed = slash_normalized != path;
    for segment in slash_normalized.split('/') {
        match segment {
            "." => changed = true,
            ".." => {
                changed = true;
                if normalized_segments
                    .last()
                    .is_some_and(|prior| !prior.is_empty())
                {
                    normalized_segments.pop();
                }
            }
            _ => normalized_segments.push(segment),
        }
    }
    let mut normalized = normalized_segments.join("/");
    if retain_trailing_separator && !normalized.ends_with('/') {
        normalized.push('/');
    }
    (changed && !normalized.is_empty() && normalized != slash_normalized).then_some(normalized)
}

fn has_http_header_boundary_whitespace(value: &[u8]) -> bool {
    value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
        || value
            .last()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
}

fn encoded_contains_credential(text: &str, credential: &str) -> bool {
    let mut decoded = String::from(text);
    for _ in 0..MAX_REVERSIBLE_DECODE_PASSES {
        let Some((next, changed)) = decode_reversible_text_once(&decoded) else {
            return true;
        };
        if changed && unicode_case_insensitive_contains(&next, credential) {
            return true;
        }
        if !changed {
            return false;
        }
        decoded = next;
    }
    decode_reversible_text_once(&decoded).is_none_or(|(_, changed)| changed)
}

fn decoded_credential_variants(credential: &str) -> Option<Vec<String>> {
    let mut decoded = String::from(credential);
    let mut variants = Vec::new();
    for _ in 0..MAX_REVERSIBLE_DECODE_PASSES {
        let (next, changed) = decode_reversible_text_once(&decoded)?;
        if !changed {
            return Some(variants);
        }
        variants.push(next.clone());
        decoded = next;
    }
    let (_, changed) = decode_reversible_text_once(&decoded)?;
    (!changed).then_some(variants)
}

fn text_contains_credential_variant(text: &str, credential: &str) -> bool {
    unicode_case_insensitive_contains(text, credential)
        || encoded_contains_credential(text, credential)
        || decoded_credential_variants(credential).is_none_or(|variants| {
            variants.iter().any(|variant| {
                unicode_case_insensitive_contains(text, variant)
                    || encoded_contains_credential(text, variant)
            })
        })
}

fn decode_reversible_text_once(text: &str) -> Option<(String, bool)> {
    let form_decoded = String::from_utf8(form_decode_once(text.as_bytes())).ok()?;
    let form_changed = form_decoded != text;
    let (html_decoded, html_changed) = decode_html_character_references(&form_decoded)?;
    let (json_decoded, json_changed) = decode_json_string_escapes(&html_decoded);
    Some((json_decoded, form_changed || html_changed || json_changed))
}

fn decode_html_character_references(text: &str) -> Option<(String, bool)> {
    const MAX_CHARACTER_REFERENCE_BYTES: usize = 64;
    let mut decoded = String::with_capacity(text.len());
    let mut remaining = text;
    let mut changed = false;
    while let Some(reference_start) = remaining.find('&') {
        decoded.push_str(&remaining[..reference_start]);
        let reference = &remaining[reference_start..];
        let Some(relative_end) = reference
            .bytes()
            .take(MAX_CHARACTER_REFERENCE_BYTES)
            .position(|byte| byte == b';')
        else {
            if over_window_numeric_reference_prefix(reference, MAX_CHARACTER_REFERENCE_BYTES) {
                return None;
            }
            decoded.push('&');
            remaining = &reference[1..];
            continue;
        };
        let entity = &reference[1..relative_end];
        if let Some(replacement) = decode_html_character_reference(entity) {
            decoded.push_str(&replacement);
            changed = true;
        } else {
            decoded.push_str(&reference[..=relative_end]);
        }
        remaining = &reference[relative_end + 1..];
    }
    decoded.push_str(remaining);
    Some((decoded, changed))
}

fn over_window_numeric_reference_prefix(reference: &str, scan_bytes: usize) -> bool {
    let Some(window) = reference.as_bytes().get(..scan_bytes) else {
        return false;
    };
    let Some(numeric) = window.strip_prefix(b"&#") else {
        return false;
    };
    let (digits, radix) = if let Some(hexadecimal) = numeric
        .strip_prefix(b"x")
        .or_else(|| numeric.strip_prefix(b"X"))
    {
        (hexadecimal, 16)
    } else {
        (numeric, 10)
    };
    !digits.is_empty()
        && digits.iter().all(|byte| match radix {
            16 => byte.is_ascii_hexdigit(),
            10 => byte.is_ascii_digit(),
            _ => false,
        })
}

fn unicode_case_insensitive_contains(haystack: &str, needle: &str) -> bool {
    let normalized_needle = unicode_case_folded_nfc(needle);
    !normalized_needle.is_empty() && unicode_case_folded_nfc(haystack).contains(&normalized_needle)
}

fn unicode_case_insensitive_starts_with(haystack: &str, needle: &str) -> bool {
    let normalized_needle = unicode_case_folded_nfc(needle);
    !normalized_needle.is_empty()
        && unicode_case_folded_nfc(haystack).starts_with(&normalized_needle)
}

fn unicode_case_insensitive_ends_with(haystack: &str, needle: &str) -> bool {
    let normalized_needle = unicode_case_folded_nfc(needle);
    !normalized_needle.is_empty() && unicode_case_folded_nfc(haystack).ends_with(&normalized_needle)
}

fn unicode_case_folded_nfc(text: &str) -> String {
    let decomposed = text.nfd().collect::<String>();
    let folded = CaseMapper::new().fold_string(&decomposed);
    folded.as_ref().nfc().collect()
}

fn unicode_normalized_contains(haystack: &str, needle: &str) -> bool {
    let normalized_needle = needle.nfc().collect::<String>();
    !normalized_needle.is_empty()
        && haystack
            .nfc()
            .collect::<String>()
            .contains(&normalized_needle)
}

fn decode_json_string_escapes(text: &str) -> (String, bool) {
    let mut decoded = String::with_capacity(text.len());
    let mut remaining = text;
    let mut changed = false;
    while let Some(relative_start) = remaining.find('\\') {
        decoded.push_str(&remaining[..relative_start]);
        let escape = &remaining[relative_start..];
        let decoded_escape = match escape.as_bytes().get(1) {
            Some(b'"') => Some(('"', 2)),
            Some(b'\\') => Some(('\\', 2)),
            Some(b'/') => Some(('/', 2)),
            Some(b'b') => Some(('\u{8}', 2)),
            Some(b'f') => Some(('\u{c}', 2)),
            Some(b'n') => Some(('\n', 2)),
            Some(b'r') => Some(('\r', 2)),
            Some(b't') => Some(('\t', 2)),
            Some(b'u') => decode_json_unicode_escape(escape),
            _ => None,
        };
        let Some((character, consumed)) = decoded_escape else {
            decoded.push('\\');
            remaining = &escape[1..];
            continue;
        };
        decoded.push(character);
        remaining = &escape[consumed..];
        changed = true;
    }
    decoded.push_str(remaining);
    (decoded, changed)
}

fn decode_json_unicode_escape(escape: &str) -> Option<(char, usize)> {
    const CODE_UNIT_ESCAPE_BYTES: usize = 6;
    const SURROGATE_PAIR_ESCAPE_BYTES: usize = CODE_UNIT_ESCAPE_BYTES * 2;
    let first = decode_json_code_unit(escape)?;
    if (0xd800..=0xdbff).contains(&first) {
        let second = decode_json_code_unit(escape.get(CODE_UNIT_ESCAPE_BYTES..)?)?;
        if !(0xdc00..=0xdfff).contains(&second) {
            return None;
        }
        let scalar = 0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
        return char::from_u32(scalar).map(|character| (character, SURROGATE_PAIR_ESCAPE_BYTES));
    }
    if (0xdc00..=0xdfff).contains(&first) {
        return None;
    }
    char::from_u32(u32::from(first)).map(|character| (character, CODE_UNIT_ESCAPE_BYTES))
}

fn decode_json_code_unit(escape: &str) -> Option<u16> {
    let digits = escape.strip_prefix("\\u")?.get(..4)?;
    u16::from_str_radix(digits, 16).ok()
}

fn decode_html_character_reference(entity: &str) -> Option<String> {
    let named = match entity {
        "amp" | "AMP" => Some("&"),
        "apos" => Some("'"),
        "gt" | "GT" => Some(">"),
        "lt" | "LT" => Some("<"),
        "quot" | "QUOT" => Some("\""),
        _ => None,
    };
    if let Some(named) = named {
        return Some(String::from(named));
    }
    let (digits, radix) = if let Some(digits) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        (digits, 16)
    } else {
        (entity.strip_prefix('#')?, 10)
    };
    let scalar = u32::from_str_radix(digits, radix).ok()?;
    let scalar = html_numeric_reference_scalar(scalar);
    char::from_u32(scalar).map(|character| character.to_string())
}

const fn html_numeric_reference_scalar(scalar: u32) -> u32 {
    match scalar {
        0 | 0xd800..=0xdfff | 0x11_0000..=u32::MAX => 0xfffd,
        0x80 => 0x20ac,
        0x82 => 0x201a,
        0x83 => 0x0192,
        0x84 => 0x201e,
        0x85 => 0x2026,
        0x86 => 0x2020,
        0x87 => 0x2021,
        0x88 => 0x02c6,
        0x89 => 0x2030,
        0x8a => 0x0160,
        0x8b => 0x2039,
        0x8c => 0x0152,
        0x8e => 0x017d,
        0x91 => 0x2018,
        0x92 => 0x2019,
        0x93 => 0x201c,
        0x94 => 0x201d,
        0x95 => 0x2022,
        0x96 => 0x2013,
        0x97 => 0x2014,
        0x98 => 0x02dc,
        0x99 => 0x2122,
        0x9a => 0x0161,
        0x9b => 0x203a,
        0x9c => 0x0153,
        0x9e => 0x017e,
        0x9f => 0x0178,
        scalar => scalar,
    }
}

fn fixed_outer_error_debug_may_contain(credential: &str) -> bool {
    text_contains_credential_variant("Err()", credential)
}

fn fixed_bound_evidence_token_collides(scrubber: &CredentialScrubber) -> bool {
    let mut probe = ToolExecutorEvidence::CompletedText(String::new());
    loop {
        let rendered = format!("{probe:?}");
        let check_collision = match bound_diagnostic_check(&probe) {
            BoundDiagnosticCheck::AllCredentialVariants => true,
            BoundDiagnosticCheck::PreserveDefinitiveFailureWord => {
                !scrubber.contains_case_normalized_credential("Failed")
            }
        };
        if check_collision && scrubber.contains_case_normalized_credential(&rendered) {
            return true;
        }
        let Some(next) = next_fixed_bound_evidence_probe(&probe) else {
            return false;
        };
        probe = next;
    }
}

fn fixed_populated_failure_detail_collides(
    scrubber: &CredentialScrubber,
    detail: &ToolExecutionErrorDetail,
) -> bool {
    let evidence = ToolExecutorEvidence::KnownFailed {
        detail: Some(detail.clone()),
    };
    let rendered = format!("{evidence:?}");
    !scrubber.contains_case_normalized_credential("Failed")
        && scrubber.contains_case_normalized_credential(&rendered)
}

fn fixed_bound_wrapper_token_collides(
    scrubber: &CredentialScrubber,
    correlation: &ToolAttemptDispatchCorrelation,
) -> bool {
    if fixed_success_payloads().any(|payload| {
        if scrubber.contains_case_normalized_credential(&payload) {
            return false;
        }
        let evidence = ToolExecutorEvidence::CompletedText(payload);
        bound_wrapper_evidence_collides(scrubber, correlation, &evidence)
    }) {
        return true;
    }
    let mut evidence = ToolExecutorEvidence::CompletedText(String::new());
    loop {
        if bound_wrapper_evidence_collides(scrubber, correlation, &evidence) {
            return true;
        }
        let Some(next) = next_fixed_bound_evidence_probe(&evidence) else {
            return false;
        };
        evidence = next;
    }
}

fn bound_wrapper_evidence_collides(
    scrubber: &CredentialScrubber,
    correlation: &ToolAttemptDispatchCorrelation,
    evidence: &ToolExecutorEvidence,
) -> bool {
    let probe = CorrelatedToolExecutorEvidenceDebugProbe {
        correlation,
        evidence,
    };
    let rendered = format!("{:?}", Result::<_, &WebSearchExecutorError>::Ok(&probe));
    let check_collision = match bound_diagnostic_check(evidence) {
        BoundDiagnosticCheck::AllCredentialVariants => true,
        BoundDiagnosticCheck::PreserveDefinitiveFailureWord => {
            !scrubber.contains_case_normalized_credential("Failed")
        }
    };
    check_collision && scrubber.contains_case_normalized_credential(&rendered)
}

struct CorrelatedToolExecutorEvidenceDebugProbe<'a> {
    correlation: &'a ToolAttemptDispatchCorrelation,
    evidence: &'a ToolExecutorEvidence,
}

impl fmt::Debug for CorrelatedToolExecutorEvidenceDebugProbe<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CorrelatedToolExecutorEvidence")
            .field(
                "fence",
                &IssuedExecutorFenceDebugProbe {
                    correlation: self.correlation,
                },
            )
            .field("evidence", self.evidence)
            .finish()
    }
}

struct IssuedExecutorFenceDebugProbe<'a> {
    correlation: &'a ToolAttemptDispatchCorrelation,
}

impl fmt::Debug for IssuedExecutorFenceDebugProbe<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedExecutorFence")
            .field("correlation", self.correlation)
            .finish()
    }
}

fn bound_diagnostic_contains_credential(rendered: &str, credential: &str) -> bool {
    if text_contains_credential_variant(rendered, credential) {
        return true;
    }
    let trimmed = credential.trim_matches(|character| character == ' ' || character == '\t');
    trimmed != credential
        && !trimmed.is_empty()
        && text_contains_credential_variant(rendered, trimmed)
}

fn next_fixed_bound_evidence_probe(
    evidence: &ToolExecutorEvidence,
) -> Option<ToolExecutorEvidence> {
    match evidence {
        ToolExecutorEvidence::CompletedText(_) => {
            Some(ToolExecutorEvidence::KnownFailed { detail: None })
        }
        ToolExecutorEvidence::KnownFailed { .. } => Some(ToolExecutorEvidence::Ambiguous),
        ToolExecutorEvidence::Ambiguous => None,
    }
}

fn canonicalized_url_host(value: &str) -> Option<String> {
    let candidate = format!("http://{value}/");
    let url = Url::parse(&candidate).ok()?;
    (url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| url.host_str().map(str::to_owned))?
}

fn parse_ip_literal(value: &str) -> Option<std::net::IpAddr> {
    let unbracketed = value
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(value);
    unbracketed.parse().ok()
}

fn form_decode_once(encoded: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let byte = encoded[index];
        let high = encoded.get(index + 1).copied().and_then(hex_value);
        let low = encoded.get(index + 2).copied().and_then(hex_value);
        if byte == b'+' {
            decoded.push(b' ');
            index += 1;
        } else if byte == b'%'
            && let (Some(high), Some(low)) = (high, low)
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    decoded
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{self, Write},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use signalbox_application::{
        InProcessToolDispatchGate, PrepareToolContinuationOutcome,
        RetainedToolAttemptObservationStatus, ToolAttemptAuthorizationStatus, ToolCatalog,
        ToolCatalogValidationFailure, ToolContinuationIdentities, ToolCrashClosureIdentities,
        ToolExecutionService, ToolExecutionServiceOutcome, ToolExecutionTransaction,
        UuidV7ToolLoopIdGenerator,
    };
    use signalbox_domain::{
        AcceptedInputId, AuthorizedToolAttempt, ContextFrontierId,
        CorrelatedToolAttemptObservation, CurrentToolAttempt, EndedToolAttempt, ModelCallId,
        ResolvedContextFrontierReconstitutionInput, SemanticTranscriptEntryId, SessionId,
        ToolApprovalResolutionReconstitutionInput, ToolAttemptCrashOutcome,
        ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId,
        ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState,
        ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
        ToolExecutionError, ToolName, ToolRequestId, ToolRequestOrdinal,
        ToolRequestReconstitutionInput, TurnAttemptId, TurnId,
    };
    use signalbox_model_runtime::{CredentialAccessError, CredentialAccessFailure};

    use super::*;

    const SYNTHETIC_KEY: &str = "fixture-search-key";
    const FIXTURE_QUERY: &str = "bounded rust search";
    const FIXTURE_RESULT_TITLE: &str = "Synthetic result";
    const FIXTURE_RESULT_URL: &str = "https://example.com/result";
    const FIXTURE_IPV6_RESULT_URL: &str = "https://[2001:db8::1]/result";
    const FIXTURE_LEGACY_IPV4_RESULT_URL: &str = "https://2130706433/result";
    const FIXTURE_UPPERCASE_SCHEME_RESULT_URL: &str = "HTTPS://example.com/result";
    const FIXTURE_BACKSLASH_RESULT_URL: &str = "https://example.com/abc\\def";
    const FIXTURE_EMBEDDED_HOST_RESULT_URL: &str = "https://x-ABCDEF.example/result";
    const FIXTURE_UNICODE_EMBEDDED_HOST_RESULT_URL: &str = "https://x-bücher.example/result";
    const FIXTURE_RESULT_SNIPPET: &str = "Synthetic recorded snippet";
    const FIXTURE_WHITESPACE_TITLE: &str = " \t\n";
    const FIXTURE_NORMALIZED_RESULT_URL: &str = "https://exa\nmple.com/result";
    const FIXTURE_ORIGIN_ONLY_RESULT_URL: &str = "https://example.com";
    const FIXTURE_CANONICAL_ORIGIN_RESULT_URL: &str = "https://example.com/";
    const ACCEPT_HEADER_COLLISION_KEY: &str = "application/json";
    const FIXED_QUERY_PARAMETER_COLLISION_KEY: &str = "text_decorations";
    const SERIALIZED_URL_SYNTAX_COLLISION_KEY: &str = "?";
    const REQUEST_DEBUG_COLLISION_KEY: &str = "provider";
    const CREDENTIAL_DEBUG_COLLISION_KEY: &str = "REDACTED";
    const REQUEST_CREDENTIAL_DEBUG_COLLISION_KEY: &str = "} CredentialValue";
    const RESPONSE_DEBUG_COLLISION_KEY: &str = "result_count";
    const SUCCESS_PAYLOAD_COLLISION_KEY: &str = "results";
    const SUCCESS_PAYLOAD_DELIMITER_COLLISION_KEY: &str = "[";
    const SUCCESS_PAYLOAD_EMPTY_RESULTS_COLLISION_KEY: &str = "[]";
    const SUCCESS_PAYLOAD_MULTI_RESULT_COLLISION_KEY: &str = "},{";
    const ACCEPT_HEADER_CASE_COLLISION_KEY: &str = "APPLICATION/JSON";
    const PROVIDER_HOST_CASE_COLLISION_KEY: &str = "API.SEARCH.BRAVE.COM";
    const URL_SCHEME_COLLISION_KEY: &str = "https";
    const URL_DOT_SEGMENT_COLLISION_KEY: &str = "abc/./def";
    const URL_DOT_SEGMENT_NORMALIZED_VALUE: &str = "abc/def";
    const URL_SCHEME_CASE_COLLISION_KEY: &str = "HTTPS";
    const URL_ENCODED_COLLISION_KEY: &str = "secret/key";
    const URL_ENCODED_COLLISION_VALUE: &str = "secret%2Fkey";
    const URL_FORM_COLLISION_KEY: &str = "secret key";
    const URL_FORM_COLLISION_VALUE: &str = "secret+key";
    const URL_IDNA_COLLISION_KEY: &str = "bücher";
    const URL_IDNA_COLLISION_VALUE: &str = "https://bücher.example/";
    const URL_HOST_CASE_COLLISION_KEY: &str = "EXAMPLE.COM";
    const URL_IPV6_COLLISION_KEY: &str = "2001:0db8:0:0:0:0:0:1";
    const URL_LEGACY_IPV4_COLLISION_KEY: &str = "2130706433";
    const URL_BACKSLASH_COLLISION_KEY: &str = "abc\\def";
    const URL_DECODED_BACKSLASH_COLLISION_KEY: &str = "abc%5Cdef";
    const URL_DECODED_CASE_BACKSLASH_COLLISION_KEY: &str = "ABC%5CDEF";
    const URL_EMBEDDED_HOST_COLLISION_KEY: &str = "ABCDEF";
    const URL_UNICODE_HOST_COLLISION_KEY: &str = "BÜCHER";
    const URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY: &str = "BU\u{0308}CHER";
    const HTML_ENTITY_COLLISION_KEY: &str = "abc&def";
    const HTML_ENTITY_COLLISION_VALUE: &str = "abc&amp;def";
    const HTML_NUMERIC_C1_COLLISION_KEY: &str = "€";
    const HTML_NUMERIC_C1_COLLISION_VALUE: &str = "&#x80;";
    const OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY: &str = "ZXQ";
    const HTML_NESTED_ENTITY_COLLISION_VALUE: &str = "abc&amp;amp;def";
    const FORM_HTML_COLLISION_VALUE: &str = "abc%26amp%3Bdef";
    const REVERSE_ENCODED_COLLISION_KEY: &str = "abc%26def";
    const REVERSE_ENCODED_COLLISION_VALUE: &str = "abc&def";
    const JSON_UNICODE_COLLISION_KEY: &str = "abc";
    const JSON_UNICODE_COLLISION_VALUE: &str = r"\u0061\u0062\u0063";
    const JSON_SOLIDUS_COLLISION_KEY: &str = r"abc\/def";
    const JSON_SOLIDUS_COLLISION_VALUE: &str = "abc/def";
    const SAFE_UNSUPPORTED_NAMED_ENTITY_VALUE: &str = "safe&nbsp;value";
    const REQUEST_DETAIL_COLLISION_KEY: &str = "failed";
    const QUERY_CASE_NORMALIZED_COLLISION_KEY: &str = "ABCDEF";
    const QUERY_CASE_NORMALIZED_COLLISION_VALUE: &str = "%61%62%63%64%65%66";
    const SHORT_DIAGNOSTIC_COLLISION_KEY: &str = "r";
    const LEADING_HEADER_WHITESPACE_KEY: &str = " fixture-search-key";
    const TRAILING_HEADER_WHITESPACE_KEY: &[u8] = b"fixture-search-key\t";
    const EMPTY_CREDENTIAL_VALUE: &[u8] = b"";
    const NON_UTF8_CREDENTIAL_VALUE: &[u8] = &[0xff];
    const INTERIOR_NEWLINE_CREDENTIAL_VALUE: &[u8] = b"fixture\nsearch-key";
    const BOUNDARY_WHITESPACE_BOUND_COLLISION_KEY: &[u8] = b"KnownFailed ";
    const EXCESSIVE_FORM_ENCODING_VALUE: &str = "%252525252525252F";
    const DIAGNOSTIC_REDACTION_OVERLAP_KEY: &str = "e";
    const TIMESTAMP_COLLISION_KEY: &str = "2026";
    const FORMATTER_EVENT_BOUNDARY_COLLISION_KEY: &str = "Z  WARN signalbox_tools_basic_web_search";
    const EXECUTOR_OUTCOME_COLLISION_KEY: &str = "CompletedText";
    const EXECUTOR_CASE_NORMALIZED_OUTCOME_COLLISION_KEY: &str = "completedtext";
    const EXECUTOR_KNOWN_FAILURE_TOKEN_COLLISION_KEY: &str = "knownfailed";
    const EXECUTOR_KNOWN_FAILURE_SUBSTRING_COLLISION_KEY: &str = "known";
    const EXECUTOR_POPULATED_FAILURE_COLLISION_KEY: &str = "Some";
    const EXECUTOR_PUNCTUATED_OUTCOME_COLLISION_KEY: &str = "completedtext(";
    const EXECUTOR_ERROR_COLLISION_KEY: &str = "Err";
    const EXECUTOR_OK_WRAPPER_COLLISION_KEY: &str = "ok";
    const EXECUTOR_BOUND_WRAPPER_COLLISION_KEY: &str = "correlated";
    const EXECUTOR_BOUND_WRAPPER_FIELD_COLLISION_KEY: &str = "{ fence:";
    const EXECUTOR_POPULATED_SUCCESS_WRAPPER_COLLISION_KEY: &str = "CompletedText(\"{";
    const TRANSPORT_CASE_NORMALIZED_FAILURE_COLLISION_KEY: &str = "requestfailed";
    const CASE_NORMALIZED_REQUEST_DETAIL_COLLISION_KEY: &str = "FAILED";
    const UNICODE_FULL_FOLD_COLLISION_KEY: &str = "STRASSE";
    const UNICODE_FULL_FOLD_COLLISION_VALUE: &str = "Straße";
    const CREDENTIAL_FAILURE_CLASSIFICATION: &str = "failure=Unmapped";
    const CREDENTIAL_VALUE_FAILURE_CLASSIFICATION: &str = "failure=Unusable";
    const OVERSIZED_CREDENTIAL_TELEMETRY_COLLISION_VALUE: &str =
        "web search credential value was unusable failure=Unusable";
    const OVERSIZED_BOUND_WRAPPER_COLLISION_VALUE: &str =
        "CorrelatedToolExecutorEvidence { fence: IssuedExecutorFence";
    const TRANSPORT_FAILURE_CLASSIFICATION: &str = "failure=RequestFailed";
    const RESPONSE_BODY_FAILURE_CLASSIFICATION: &str = "failure=DispatchUnknown";
    const RESPONSE_SANITIZATION_FAILURE_CLASSIFICATION: &str = "failure=EvidenceEncoding";
    const RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY: &str = "evidenceencoding";
    const SESSION_IDENTITY: u128 = 1;
    const TURN_IDENTITY: u128 = 2;
    const ISSUING_ATTEMPT_IDENTITY: u128 = 3;
    const REQUEST_IDENTITY: u128 = 4;
    const ATTEMPT_IDENTITY: u128 = 5;
    const PRODUCING_CALL_IDENTITY: u128 = 6;
    const FRONTIER_IDENTITY: u128 = 7;
    const SESSION_ID_DIAGNOSTIC: &str = "session_id=00000000-0000-0000-0000-000000000001";
    const TURN_ID_DIAGNOSTIC: &str = "turn_id=00000000-0000-0000-0000-000000000002";
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
            credential: &CredentialValue,
        ) -> WebSearchTransportOutcome {
            self.searches.fetch_add(1, Ordering::Relaxed);
            WebSearchTransportOutcome::completed(response_with_result_count(1), credential)
        }
    }

    struct StaticCredentials {
        value: &'static str,
    }

    impl CredentialAccess for StaticCredentials {
        async fn resolve(
            &self,
            _reference: &CredentialReference,
        ) -> Result<CredentialValue, CredentialAccessError> {
            Ok(CredentialValue::new(self.value.as_bytes().to_vec()))
        }
    }

    struct RawCredentials {
        value: Vec<u8>,
    }

    impl CredentialAccess for RawCredentials {
        async fn resolve(
            &self,
            _reference: &CredentialReference,
        ) -> Result<CredentialValue, CredentialAccessError> {
            Ok(CredentialValue::new(self.value.clone()))
        }
    }

    struct ProviderStatusCredentials;

    impl CredentialAccess for ProviderStatusCredentials {
        async fn resolve(
            &self,
            _reference: &CredentialReference,
        ) -> Result<CredentialValue, CredentialAccessError> {
            Ok(CredentialValue::new(
                PROVIDER_REJECTION_STATUS.to_string().into_bytes(),
            ))
        }
    }

    struct SanitizedDispatchUnknownTransport;

    impl WebSearchTransport for SanitizedDispatchUnknownTransport {
        async fn search(
            &mut self,
            _request: WebSearchRequest,
            credential: &CredentialValue,
        ) -> WebSearchTransportOutcome {
            WebSearchTransportOutcome::failed(
                WebSearchTransportFailure::DispatchUnknown,
                credential,
            )
        }
    }

    struct RequestFailedTransport {
        searches: Arc<AtomicUsize>,
    }

    impl WebSearchTransport for RequestFailedTransport {
        async fn search(
            &mut self,
            _request: WebSearchRequest,
            credential: &CredentialValue,
        ) -> WebSearchTransportOutcome {
            self.searches.fetch_add(1, Ordering::Relaxed);
            WebSearchTransportOutcome::failed(WebSearchTransportFailure::RequestFailed, credential)
        }
    }

    struct ProviderRejectedTransport {
        searches: Arc<AtomicUsize>,
    }

    impl WebSearchTransport for ProviderRejectedTransport {
        async fn search(
            &mut self,
            _request: WebSearchRequest,
            credential: &CredentialValue,
        ) -> WebSearchTransportOutcome {
            self.searches.fetch_add(1, Ordering::Relaxed);
            let error = WebSearchProviderError::new(
                PROVIDER_REJECTION_STATUS,
                br#"{"message":"synthetic rejection"}"#.to_vec(),
            )
            .expect("fixture provider error is admitted");
            WebSearchTransportOutcome::failed(
                WebSearchTransportFailure::ProviderRejected(error),
                credential,
            )
        }
    }

    struct ReflectedTitleTransport;

    #[derive(Clone, Default)]
    struct CapturedTelemetry(Arc<Mutex<Vec<u8>>>);

    impl CapturedTelemetry {
        fn text(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("captured telemetry lock is available")
                    .clone(),
            )
            .expect("captured telemetry is UTF-8")
        }
    }

    impl Write for CapturedTelemetry {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("captured telemetry lock is available")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedTelemetry {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    impl WebSearchTransport for ReflectedTitleTransport {
        async fn search(
            &mut self,
            _request: WebSearchRequest,
            credential: &CredentialValue,
        ) -> WebSearchTransportOutcome {
            let title = String::from_utf8(credential.expose_bytes().to_vec())
                .expect("fixture credential is UTF-8");
            let reflected = WebSearchResult::try_new(WebSearchResultFields {
                title,
                url: String::from(FIXTURE_RESULT_URL),
                snippet: String::from(FIXTURE_RESULT_SNIPPET),
            })
            .expect("fixture reflected result is admitted");
            WebSearchTransportOutcome::completed(
                WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
                    .expect("fixture response is admitted"),
                credential,
            )
        }
    }

    struct FormattingExecutor<Executor> {
        inner: Executor,
        diagnostic: Arc<Mutex<String>>,
    }

    impl<Executor> ToolExecutor for FormattingExecutor<Executor>
    where
        Executor: ToolExecutor<Error = WebSearchExecutorError> + Send,
    {
        type Error = WebSearchExecutorError;

        async fn execute(
            &mut self,
            invocation: ToolExecutionInvocation,
        ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
            let result = self.inner.execute(invocation).await;
            *self
                .diagnostic
                .lock()
                .expect("captured executor diagnostic lock is available") = format!("{result:?}");
            result
        }
    }

    struct ExecutorFixtureTransaction {
        batch: signalbox_domain::ToolBatch,
    }

    impl ToolExecutionTransaction for ExecutorFixtureTransaction {
        type Error = WebSearchExecutorError;

        async fn load_active_batch(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
        ) -> Result<Option<signalbox_domain::ToolBatch>, Self::Error> {
            Ok(Some(self.batch.clone()))
        }

        async fn prepare_next_attempt(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
            _effect_class: ToolEffectClass,
        ) -> Result<Option<CurrentToolAttempt>, Self::Error> {
            panic!("fixture begins with one prepared attempt")
        }

        async fn authorize_attempt(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            attempt: ToolAttemptId,
        ) -> Result<AuthorizedToolAttempt, Self::Error> {
            self.batch
                .authorize_attempt(attempt)
                .map_err(|_| WebSearchExecutorError::ArgumentValidationDrift)
        }

        async fn reread_ambiguous_authorization(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
        ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
            panic!("fixture authorization is unambiguous")
        }

        async fn commit_preflight_error(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
            _error: ToolExecutionError,
        ) -> Result<EndedToolAttempt, Self::Error> {
            panic!("fixture arguments pass preflight")
        }

        async fn commit_observation(
            &mut self,
            observation: CorrelatedToolAttemptObservation,
        ) -> Result<EndedToolAttempt, Self::Error> {
            self.batch
                .authorize_attempt(observation.correlation().attempt())
                .map_err(|_| WebSearchExecutorError::ArgumentValidationDrift)?
                .into_parts()
                .0
                .apply_terminal_observation(observation)
                .map_err(|_| WebSearchExecutorError::ArgumentValidationDrift)
        }

        async fn reread_observation(
            &mut self,
            _observation: &CorrelatedToolAttemptObservation,
        ) -> Result<RetainedToolAttemptObservationStatus, Self::Error> {
            Ok(RetainedToolAttemptObservationStatus::Pending)
        }

        async fn classify_crash_loss<NextTurn>(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: ToolAttemptId,
            _identities: ToolCrashClosureIdentities,
            _next_turn: NextTurn,
        ) -> Result<ToolAttemptCrashOutcome, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            Err(WebSearchExecutorError::EvidenceEncoding)
        }

        async fn prepare_continuation<NextSteering>(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _producing_call: ModelCallId,
            _identities: ToolContinuationIdentities,
            _next_steering: NextSteering,
        ) -> Result<PrepareToolContinuationOutcome, Self::Error>
        where
            NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
        {
            panic!("fixture has one prepared attempt")
        }
    }

    fn prepared_web_search_batch() -> signalbox_domain::ToolBatch {
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(SESSION_IDENTITY));
        let turn = TurnId::from_uuid(uuid::Uuid::from_u128(TURN_IDENTITY));
        let producing_call = ModelCallId::from_uuid(uuid::Uuid::from_u128(PRODUCING_CALL_IDENTITY));
        let request = ToolRequestReconstitutionInput::new(
            ToolRequestId::from_uuid(uuid::Uuid::from_u128(REQUEST_IDENTITY)),
            session,
            turn,
            producing_call,
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from(WEB_SEARCH_NAME)).expect("fixture name is valid"),
            arguments(&serde_json::json!({"query": FIXTURE_QUERY}).to_string()),
        )
        .into_request();
        let turn_attempt =
            TurnAttemptId::from_uuid(uuid::Uuid::from_u128(ISSUING_ATTEMPT_IDENTITY));
        let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
            .reconstitute()
            .expect("policy approval fixture is valid");
        let attempt = ToolAttemptReconstitutionInput::new(
            ToolAttemptId::from_uuid(uuid::Uuid::from_u128(ATTEMPT_IDENTITY)),
            request.id(),
            session,
            turn,
            turn_attempt,
            ToolEffectClass::ExternalEffect,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Prepared,
        )
        .reconstitute()
        .expect("prepared attempt fixture is valid");
        let frontier = ResolvedContextFrontierReconstitutionInput::new(
            session,
            ContextFrontierId::from_uuid(uuid::Uuid::from_u128(FRONTIER_IDENTITY)),
            Vec::new(),
        )
        .reconstitute()
        .expect("empty frontier fixture is valid");

        ToolBatchReconstitutionInput::new(
            session,
            turn,
            producing_call,
            frontier,
            vec![request],
            vec![approval],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::Executing { turn_attempt },
        )
        .reconstitute()
        .expect("web_search batch fixture is valid")
    }

    async fn execute_raw_credential_through_service(
        value: &[u8],
    ) -> (ToolExecutionServiceOutcome, usize) {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = RawCredentials {
            value: value.to_vec(),
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("fixture web_search tool compiles")
            .into_parts();
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );
        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("invalid credential commits definitive evidence");
        (outcome, searches.load(Ordering::Relaxed))
    }

    async fn execute_formatted_raw_credential_through_service(
        value: &[u8],
    ) -> (bool, usize, String) {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = RawCredentials {
            value: value.to_vec(),
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("fixture web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: Arc::clone(&diagnostic),
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let outcome = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();
        (outcome.is_err(), searches.load(Ordering::Relaxed), rendered)
    }

    async fn execute_request_failure_through_service(
        value: &'static str,
    ) -> (ToolExecutionServiceOutcome, usize) {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials { value };
        let transport = RequestFailedTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("fixture web_search tool compiles")
            .into_parts();
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );
        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("request failure commits definitive evidence");
        (outcome, searches.load(Ordering::Relaxed))
    }

    async fn execute_provider_rejection_through_service() -> (ToolExecutionServiceOutcome, usize) {
        let searches = Arc::new(AtomicUsize::new(0));
        let transport = ProviderRejectedTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) =
            WebSearchTool::try_new(ProviderStatusCredentials, transport, configuration())
                .expect("fixture web_search tool compiles")
                .into_parts();
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );
        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("provider rejection commits definitive evidence");
        (outcome, searches.load(Ordering::Relaxed))
    }

    fn is_committed_known_failure(outcome: &ToolExecutionServiceOutcome) -> bool {
        matches!(
            outcome,
            ToolExecutionServiceOutcome::ObservationCommitted(ended)
                if matches!(
                    ended.end(),
                    signalbox_domain::ToolAttemptEnd::KnownFailed { .. }
                )
        )
    }

    fn is_committed_completed(outcome: &ToolExecutionServiceOutcome) -> bool {
        matches!(
            outcome,
            ToolExecutionServiceOutcome::ObservationCommitted(ended)
                if matches!(ended.end(), signalbox_domain::ToolAttemptEnd::Completed { .. })
        )
    }

    fn is_committed_known_failure_without_detail(outcome: &ToolExecutionServiceOutcome) -> bool {
        matches!(
            outcome,
            ToolExecutionServiceOutcome::ObservationCommitted(ended)
                if matches!(
                    ended.end(),
                    signalbox_domain::ToolAttemptEnd::KnownFailed { error }
                        if error.detail().is_none()
                )
        )
    }

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn configuration() -> WebSearchConfiguration {
        WebSearchConfiguration::new(WebSearchProvider::Brave)
    }

    fn request() -> WebSearchRequest {
        WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        }
    }

    fn dispatch_correlation() -> ToolAttemptDispatchCorrelation {
        ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session: SessionId::from_uuid(uuid::Uuid::from_u128(SESSION_IDENTITY)),
                turn: TurnId::from_uuid(uuid::Uuid::from_u128(TURN_IDENTITY)),
                issuing_attempt: TurnAttemptId::from_uuid(uuid::Uuid::from_u128(
                    ISSUING_ATTEMPT_IDENTITY,
                )),
                request: ToolRequestId::from_uuid(uuid::Uuid::from_u128(REQUEST_IDENTITY)),
                attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(ATTEMPT_IDENTITY)),
                generation: ToolDispatchGeneration::first(),
            },
        )
    }

    fn capture_credential_failure(
        error: &CredentialAccessError,
        correlation: &ToolAttemptDispatchCorrelation,
    ) -> String {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            report_credential_access_failure(error, correlation);
        });
        output.text()
    }

    fn capture_credential_failure_in_credential_span(
        error: &CredentialAccessError,
        correlation: &ToolAttemptDispatchCorrelation,
        credential: &str,
    ) -> String {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let caller = tracing::warn_span!("caller", credential);
            let _entered = caller.enter();
            report_credential_access_failure(error, correlation);
        });
        output.text()
    }

    fn capture_credential_value_failure(
        correlation: &ToolAttemptDispatchCorrelation,
        credential: &CredentialValue,
    ) -> (String, Result<(), WebSearchExecutorError>) {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            report_credential_value_failure(correlation, credential)
        });
        (output.text(), result)
    }

    fn fully_percent_encode(value: &str) -> String {
        value.bytes().map(|byte| format!("%{byte:02X}")).collect()
    }

    fn capture_credential_value_failure_in_credential_span(
        correlation: &ToolAttemptDispatchCorrelation,
        credential: &CredentialValue,
    ) -> (String, Result<(), WebSearchExecutorError>) {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .finish();
        let credential_text =
            std::str::from_utf8(credential.expose_bytes()).expect("fixture credential is UTF-8");
        let result = tracing::subscriber::with_default(subscriber, || {
            let caller = tracing::warn_span!("caller", credential = credential_text);
            let _entered = caller.enter();
            report_credential_value_failure(correlation, credential)
        });
        (output.text(), result)
    }

    fn capture_transport_failure(
        failure: &WebSearchTransportFailure,
        correlation: &ToolAttemptDispatchCorrelation,
        credential: &CredentialValue,
    ) -> (String, Result<(), WebSearchExecutorError>) {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .compact()
            .with_writer(output.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            report_transport_failure(failure, correlation, credential)
        });
        (output.text(), result)
    }

    fn capture_response_body_failure(
        failure_class: WebSearchTransportFailureClass,
        correlation: &ToolAttemptDispatchCorrelation,
        credential: &CredentialValue,
    ) -> (String, Result<(), WebSearchExecutorError>) {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .compact()
            .with_writer(output.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            report_response_body_failure(failure_class, correlation, credential)
        });
        (output.text(), result)
    }

    fn capture_response_sanitization_failure(
        correlation: &ToolAttemptDispatchCorrelation,
        credential: &CredentialValue,
    ) -> (String, Result<(), WebSearchExecutorError>) {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .compact()
            .with_writer(output.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            report_response_sanitization_failure(correlation, credential)
        });
        (output.text(), result)
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

    fn completed_text(evidence: ToolExecutorEvidence) -> String {
        match evidence {
            ToolExecutorEvidence::CompletedText(content) => content,
            other => panic!("expected completed text, got {other:?}"),
        }
    }

    fn known_failure_detail(evidence: ToolExecutorEvidence) -> Option<String> {
        match evidence {
            ToolExecutorEvidence::KnownFailed { detail } => {
                detail.map(|detail| String::from(detail.as_str()))
            }
            other => panic!("expected known failure, got {other:?}"),
        }
    }

    fn html_multibyte_boundary_reflection() -> String {
        format!("{HTML_ENTITY_COLLISION_VALUE}{}é", "x".repeat(55))
    }

    fn distant_html_reference_terminator() -> String {
        format!("{};", "&".repeat(MAX_PROVIDER_RESPONSE_BYTES - 1))
    }

    fn over_window_numeric_html_reflection() -> String {
        let leading_zeroes = "0".repeat(64);
        format!("&#{leading_zeroes}90;&#{leading_zeroes}88;&#{leading_zeroes}81;")
    }

    fn content_over_tool_result_bound() -> String {
        "x".repeat(
            MAX_RETURNED_RESULTS
                * (MAX_RESULT_TITLE_BYTES + MAX_RESULT_SNIPPET_BYTES)
                * "\\u0001".len(),
        )
    }

    fn provider_rejection(failure: WebSearchTransportFailure) -> WebSearchProviderError {
        match failure {
            WebSearchTransportFailure::ProviderRejected(error) => error,
            other => panic!("expected provider rejection, got {other:?}"),
        }
    }

    fn colliding_failure_detail(
        credential: &'static str,
        failure_class: WebSearchTransportFailureClass,
    ) -> String {
        let credentials = StaticCredentials {
            value: SYNTHETIC_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::new(AtomicUsize::new(0)),
        };
        let (_catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("fixture web_search tool compiles")
            .into_parts();
        let credential = CredentialValue::new(credential.as_bytes().to_vec());
        let scrubber =
            CredentialScrubber::try_new(&credential).expect("fixture credential is usable");
        let diagnostic = WebSearchCredentialDiagnostic {
            rendered: String::from("!"),
            failure_class: WebSearchCredentialDiagnosticClass::CallerOrHubBug,
            transport_failure_class: Some(failure_class),
        };
        let evidence = executor
            .credential_diagnostic_evidence(diagnostic, &scrubber)
            .expect("non-ambiguous failure becomes evidence");
        match evidence {
            ToolExecutorEvidence::KnownFailed {
                detail: Some(detail),
            } => String::from(detail.as_str()),
            other => panic!("expected detailed known failure, got {other:?}"),
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
        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];

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
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("synthetic transport completes");

        assert!(matches!(evidence, ToolExecutorEvidence::CompletedText(_)));
        assert_eq!(resolutions.load(Ordering::Relaxed), 1);
        assert_eq!(searches.load(Ordering::Relaxed), 1);
    }

    /// INV-035: query/credential collisions are rejected before the injected
    /// transport boundary, independent of the production request builder.
    #[tokio::test]
    async fn web_search_rejects_query_credential_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: SYNTHETIC_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(SYNTHETIC_KEY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("query collision is definitive pre-dispatch evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: fixed provider request metadata is checked before the
    /// injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_fixed_request_metadata_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: ACCEPT_HEADER_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("fixed metadata collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: every fixed provider query component is checked before the
    /// injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_fixed_query_metadata_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: FIXED_QUERY_PARAMETER_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("fixed query metadata collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: syntax introduced by serializing the provider URL is checked
    /// before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_serialized_url_syntax_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: SERIALIZED_URL_SYNTAX_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("serialized URL collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: the request's fixed Debug representation is checked before the
    /// injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_fixed_request_debug_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: REQUEST_DEBUG_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("fixed request Debug collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: the credential value's fixed redacted Debug representation is
    /// checked before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_fixed_credential_debug_before_injected_transport() {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .compact()
            .with_writer(output.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: CREDENTIAL_DEBUG_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("credential Debug collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
        assert!(output.text().is_empty());
    }

    /// INV-035: diagnostic framing that combines the request and credential
    /// Debug values is checked before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_combined_request_credential_debug_before_transport() {
        let output = CapturedTelemetry::default();
        let subscriber = tracing_subscriber::fmt()
            .compact()
            .with_writer(output.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: REQUEST_CREDENTIAL_DEBUG_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request(), &correlation)
            .await
            .into_result()
            .expect("combined Debug collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
        assert!(output.text().is_empty());
    }

    /// INV-035: every bounded response Debug representation is checked before
    /// the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_fixed_response_debug_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: RESPONSE_DEBUG_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("fixed response Debug collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: fixed successful-payload member names are checked before the
    /// injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_fixed_success_payload_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: SUCCESS_PAYLOAD_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("fixed success-payload collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: fixed JSON delimiters in every successful payload are checked
    /// before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_fixed_success_payload_delimiter_before_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: SUCCESS_PAYLOAD_DELIMITER_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(FIXTURE_QUERY),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("fixed payload delimiter collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: the empty result-list representation is checked before the
    /// injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_empty_success_payload_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: SUCCESS_PAYLOAD_EMPTY_RESULTS_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request(), &correlation)
            .await
            .into_result()
            .expect("empty result-list collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: the delimiter between multiple serialized results is checked
    /// before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_multi_result_payload_before_injected_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: SUCCESS_PAYLOAD_MULTI_RESULT_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request(), &correlation)
            .await
            .into_result()
            .expect("multi-result collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: an HTML C1 numeric reference is decoded through its standard
    /// replacement mapping before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_c1_numeric_reference_credential_before_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: HTML_NUMERIC_C1_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(HTML_NUMERIC_C1_COLLISION_VALUE),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("C1 numeric-reference collision is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: a numeric HTML reference whose terminator exceeds the scan
    /// window fails closed before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_over_window_numeric_reference_before_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: over_window_numeric_html_reflection(),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("over-window numeric reference is definitive evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: reversible query decoding and Unicode case normalization
    /// cannot conceal a credential before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_encoded_case_normalized_query_credential_before_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: QUERY_CASE_NORMALIZED_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(QUERY_CASE_NORMALIZED_COLLISION_VALUE),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("encoded case-normalized collision is definitive evidence");
        let _detail = known_failure_detail(evidence);

        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: reversible decoding of the credential itself cannot conceal a
    /// collision with a decoded query before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_query_matching_decoded_credential_before_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: REVERSE_ENCODED_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(REVERSE_ENCODED_COLLISION_VALUE),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("decoded credential collision is definitive pre-dispatch evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: JSON Unicode escapes cannot conceal a credential before the
    /// injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_json_unicode_escaped_query_credential_before_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: JSON_UNICODE_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(JSON_UNICODE_COLLISION_VALUE),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("JSON-escaped credential collision is definitive evidence");
        let _detail = known_failure_detail(evidence);

        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: reversible short JSON escapes in the credential itself cannot
    /// conceal a decoded query before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_query_matching_json_solidus_escaped_credential() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: JSON_SOLIDUS_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(JSON_SOLIDUS_COLLISION_VALUE),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("JSON solidus collision is definitive pre-dispatch evidence");

        assert!(matches!(evidence, ToolExecutorEvidence::KnownFailed { .. }));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: a multi-character full Unicode case fold cannot conceal a
    /// credential before the injected transport boundary.
    #[tokio::test]
    async fn web_search_rejects_full_case_folded_query_credential_before_transport() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: UNICODE_FULL_FOLD_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let request = WebSearchRequest {
            provider: WebSearchProvider::Brave,
            query: String::from(UNICODE_FULL_FOLD_COLLISION_VALUE),
        };
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request, &correlation)
            .await
            .into_result()
            .expect("full-fold credential collision is definitive evidence");
        let _detail = known_failure_detail(evidence);

        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: the actual `ToolExecutor::execute` result fails closed before
    /// its bound evidence diagnostic can reproduce the request credential.
    #[tokio::test]
    async fn web_search_bound_executor_result_omits_credential_collision() {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&diagnostic);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_OUTCOME_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: captured,
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let result = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();

        assert!(result.is_err());
        assert!(!rendered.contains(EXECUTOR_OUTCOME_COLLISION_KEY));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: case normalization of fixed bound-evidence Debug tokens cannot
    /// reproduce a request credential in the public executor result.
    #[tokio::test]
    async fn web_search_bound_executor_result_omits_case_normalized_credential_collision() {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&diagnostic);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_CASE_NORMALIZED_OUTCOME_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: captured,
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let result = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();

        assert!(result.is_err());
        assert!(!unicode_case_insensitive_contains(
            &rendered,
            EXECUTOR_CASE_NORMALIZED_OUTCOME_COLLISION_KEY
        ));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: fixed bound-wrapper vocabulary is checked before physical
    /// dispatch, not only after evidence is correlated.
    #[tokio::test]
    async fn web_search_bound_wrapper_collision_fails_before_dispatch() {
        let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
            EXECUTOR_BOUND_WRAPPER_COLLISION_KEY.as_bytes(),
        )
        .await;

        assert!(failed);
        assert!(!unicode_case_insensitive_contains(
            &rendered,
            EXECUTOR_BOUND_WRAPPER_COLLISION_KEY,
        ));
        assert_eq!(searches, 0);
    }

    /// INV-035: exact field framing introduced by the correlated bound wrapper
    /// is checked before physical dispatch.
    #[tokio::test]
    async fn web_search_exact_bound_wrapper_framing_fails_before_dispatch() {
        let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
            EXECUTOR_BOUND_WRAPPER_FIELD_COLLISION_KEY.as_bytes(),
        )
        .await;

        assert!(failed);
        assert!(!rendered.contains(EXECUTOR_BOUND_WRAPPER_FIELD_COLLISION_KEY));
        assert_eq!(searches, 0);
    }

    /// INV-035: populated completed evidence is combined with the correlated
    /// bound wrapper before physical dispatch.
    #[tokio::test]
    async fn web_search_populated_success_wrapper_fails_before_dispatch() {
        let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
            EXECUTOR_POPULATED_SUCCESS_WRAPPER_COLLISION_KEY.as_bytes(),
        )
        .await;

        assert!(failed);
        assert!(!rendered.contains(EXECUTOR_POPULATED_SUCCESS_WRAPPER_COLLISION_KEY));
        assert_eq!(searches, 0);
    }

    /// INV-035: boundary whitespace cannot hide a credential that normalizes
    /// to fixed bound-evidence vocabulary.
    #[tokio::test]
    async fn web_search_bound_result_checks_trimmed_unusable_credential() {
        let credential = std::str::from_utf8(BOUNDARY_WHITESPACE_BOUND_COLLISION_KEY)
            .expect("fixture credential is UTF-8");
        let trimmed = credential.trim();

        let (failed, searches, rendered) = execute_formatted_raw_credential_through_service(
            BOUNDARY_WHITESPACE_BOUND_COLLISION_KEY,
        )
        .await;

        assert!(failed);
        assert!(!unicode_case_insensitive_contains(&rendered, trimmed));
        assert_eq!(searches, 0);
    }

    /// INV-035: a credential matching the fixed `KnownFailed` Debug token is
    /// rejected before dispatch and omitted from the public executor result.
    #[tokio::test]
    async fn web_search_bound_known_failure_token_omits_case_folded_credential_collision() {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&diagnostic);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_KNOWN_FAILURE_TOKEN_COLLISION_KEY,
        };
        let transport = RequestFailedTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: captured,
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let result = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();

        assert!(result.is_err());
        assert!(!unicode_case_insensitive_contains(
            &rendered,
            EXECUTOR_KNOWN_FAILURE_TOKEN_COLLISION_KEY,
        ));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: a credential matching a substring of the fixed `KnownFailed`
    /// Debug token is rejected before dispatch and omitted from the public
    /// executor result.
    #[tokio::test]
    async fn web_search_bound_known_failure_token_omits_credential_substring_collision() {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&diagnostic);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_KNOWN_FAILURE_SUBSTRING_COLLISION_KEY,
        };
        let transport = RequestFailedTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: captured,
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let result = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();

        assert!(result.is_err());
        assert!(!unicode_case_insensitive_contains(
            &rendered,
            EXECUTOR_KNOWN_FAILURE_SUBSTRING_COLLISION_KEY,
        ));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: fixed populated-detail framing is checked before dispatch and
    /// a collision preserves definitive detail-free failure evidence.
    #[tokio::test]
    async fn web_search_populated_failure_detail_collision_commits_before_dispatch() {
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_POPULATED_FAILURE_COLLISION_KEY,
        };
        let transport = ProviderRejectedTransport {
            searches: Arc::clone(&searches),
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, transport, configuration())
                .expect("static web_search tool compiles")
                .into_parts();
        let correlation = dispatch_correlation();

        let evidence = executor
            .execute_request(request(), &correlation)
            .await
            .into_result()
            .expect("populated detail collision is definitive pre-dispatch evidence");

        assert_eq!(known_failure_detail(evidence), None);
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: the outer `Ok` wrapper is included in case-normalized checks
    /// of the complete public known-failure executor result.
    #[tokio::test]
    async fn web_search_bound_known_failure_omits_outer_ok_wrapper_collision() {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&diagnostic);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_OK_WRAPPER_COLLISION_KEY,
        };
        let transport = RequestFailedTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: captured,
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let result = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();

        assert!(result.is_err());
        assert!(!unicode_case_insensitive_contains(
            &rendered,
            EXECUTOR_OK_WRAPPER_COLLISION_KEY,
        ));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: punctuation does not conceal a case-normalized fixed Debug
    /// spelling in the public bound executor result.
    #[tokio::test]
    async fn web_search_bound_executor_result_omits_punctuated_case_collision() {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&diagnostic);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_PUNCTUATED_OUTCOME_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: captured,
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let result = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();

        assert!(result.is_err());
        assert!(!unicode_case_insensitive_contains(
            &rendered,
            EXECUTOR_PUNCTUATED_OUTCOME_COLLISION_KEY
        ));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
    }

    /// INV-035: a credential that can collide with the fixed outer `Err`
    /// marker is rejected before physical dispatch, and the resulting complete
    /// executor diagnostic does not reproduce it.
    #[tokio::test]
    async fn web_search_bound_executor_error_result_omits_credential_collision() {
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&diagnostic);
        let searches = Arc::new(AtomicUsize::new(0));
        let credentials = StaticCredentials {
            value: EXECUTOR_ERROR_COLLISION_KEY,
        };
        let transport = CountingTransport {
            searches: Arc::clone(&searches),
        };
        let (catalog, executor) = WebSearchTool::try_new(credentials, transport, configuration())
            .expect("static web_search tool compiles")
            .into_parts();
        let executor = FormattingExecutor {
            inner: executor,
            diagnostic: captured,
        };
        let batch = prepared_web_search_batch();
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let result = service.execute(batch.session(), batch.turn()).await;
        let rendered = diagnostic
            .lock()
            .expect("captured executor diagnostic lock is available")
            .clone();

        assert!(result.is_ok(), "unexpected service result: {result:?}");
        assert!(!rendered.contains(EXECUTOR_ERROR_COLLISION_KEY));
        assert_eq!(searches.load(Ordering::Relaxed), 0);
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
        let content = completed_text(evidence);
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
        let content = completed_text(evidence);
        let value: serde_json::Value =
            serde_json::from_str(&content).expect("result is valid JSON");

        assert_eq!(value["truncated"], true);
    }

    /// JSON expansion cannot carry completed evidence across the shared
    /// `ToolResultText` bound.
    #[test]
    fn web_search_rejects_encoded_output_over_tool_result_bound() {
        let content = content_over_tool_result_bound();

        assert_eq!(
            completed_text_evidence(content),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
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
        let content = completed_text(evidence);

        assert!(!content.contains(SYNTHETIC_KEY));
    }

    /// INV-035: JSON Unicode escapes in provider text are decoded within the
    /// bounded scrubber before completed evidence is formed.
    #[test]
    fn web_search_success_evidence_redacts_json_unicode_escaped_credential() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: format!("Synthetic {JSON_UNICODE_COLLISION_VALUE} result"),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("JSON-escaped fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            JSON_UNICODE_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
        let content = completed_text(evidence);

        assert!(!content.contains(JSON_UNICODE_COLLISION_VALUE));
        assert!(!unicode_case_insensitive_contains(
            &content,
            JSON_UNICODE_COLLISION_KEY,
        ));
    }

    /// INV-035: reversible short JSON escapes in the credential itself apply
    /// before provider-controlled fields enter completed evidence.
    #[test]
    fn web_search_success_evidence_redacts_json_solidus_decoded_credential() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(JSON_SOLIDUS_COLLISION_VALUE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("JSON solidus fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            JSON_SOLIDUS_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
        let content = completed_text(evidence);

        assert!(!content.contains(JSON_SOLIDUS_COLLISION_KEY));
        assert!(!content.contains(JSON_SOLIDUS_COLLISION_VALUE));
    }

    /// INV-035: multi-character full Unicode folding applies to provider text
    /// before completed evidence is formed.
    #[test]
    fn web_search_success_evidence_redacts_full_case_folded_credential() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: format!("Synthetic {UNICODE_FULL_FOLD_COLLISION_VALUE} result"),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("full-fold fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            UNICODE_FULL_FOLD_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
        let content = completed_text(evidence);

        assert!(!unicode_case_insensitive_contains(
            &content,
            UNICODE_FULL_FOLD_COLLISION_KEY,
        ));
    }

    /// INV-035: reversible decoding of the credential itself applies before
    /// provider-controlled fields enter completed evidence.
    #[test]
    fn web_search_success_evidence_redacts_text_matching_decoded_credential() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(REVERSE_ENCODED_COLLISION_VALUE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("decoded credential fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            REVERSE_ENCODED_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response encodes safely");
        let content = completed_text(evidence);

        assert!(!content.contains(REVERSE_ENCODED_COLLISION_KEY));
        assert!(!content.contains(REVERSE_ENCODED_COLLISION_VALUE));
    }

    /// Unsupported named HTML references stay ordinary text instead of
    /// becoming false credential collisions in queries or provider fields.
    #[test]
    fn web_search_unsupported_named_html_reference_remains_ordinary_text() {
        let scrubber = scrubber();

        let sanitized = scrubber.redact_text(SAFE_UNSUPPORTED_NAMED_ENTITY_VALUE);

        assert!(!query_contains_credential(
            SAFE_UNSUPPORTED_NAMED_ENTITY_VALUE,
            SYNTHETIC_KEY,
        ));
        assert_eq!(sanitized, SAFE_UNSUPPORTED_NAMED_ENTITY_VALUE);
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

    /// INV-035: a reversibly percent-encoded credential in a provider result
    /// URL is rejected before completed evidence can retain it.
    #[test]
    fn web_search_rejects_percent_encoded_credential_in_result_url() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: format!("{FIXTURE_RESULT_URL}?token={URL_ENCODED_COLLISION_VALUE}"),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("encoded fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_ENCODED_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: a form-encoded space cannot retain a reversible credential in
    /// a provider result URL.
    #[test]
    fn web_search_rejects_form_encoded_credential_in_result_url() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: format!("{FIXTURE_RESULT_URL}?token={URL_FORM_COLLISION_VALUE}"),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("form-encoded fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_FORM_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: IDNA serialization cannot retain a reversible credential in a
    /// provider result host.
    #[test]
    fn web_search_rejects_idna_encoded_credential_in_result_host() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(URL_IDNA_COLLISION_VALUE),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("IDNA fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_IDNA_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: case-insensitive domain canonicalization cannot conceal a
    /// credential reflected in a provider result host.
    #[test]
    fn web_search_rejects_case_normalized_credential_in_result_host() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_ORIGIN_ONLY_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("domain fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_HOST_CASE_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: canonical Unicode normalization cannot conceal a decomposed
    /// credential reflected in a provider result host.
    #[test]
    fn web_search_rejects_unicode_normalized_credential_in_result_host() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_UNICODE_EMBEDDED_HOST_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("Unicode host fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY
                .as_bytes()
                .to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: case-insensitive scheme canonicalization cannot conceal a
    /// credential reflected in a provider result URL.
    #[test]
    fn web_search_rejects_case_normalized_credential_in_result_scheme() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_UPPERCASE_SCHEME_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("scheme fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_SCHEME_CASE_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: URL backslash normalization cannot conceal a credential in a
    /// provider result path.
    #[test]
    fn web_search_rejects_backslash_normalized_credential_in_result_url() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_BACKSLASH_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("backslash fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_BACKSLASH_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: URL backslash normalization is composed with reversible
    /// credential decoding before completed evidence is retained.
    #[test]
    fn web_search_rejects_decoded_backslash_normalized_credential_in_result_url() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_BACKSLASH_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("decoded backslash fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_DECODED_BACKSLASH_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: URL path dot-segment removal cannot conceal any reversible
    /// credential variant in completed result evidence.
    #[test]
    fn web_search_rejects_dot_segment_normalized_credential_in_result_url() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: format!("{FIXTURE_ORIGIN_ONLY_RESULT_URL}/{URL_DOT_SEGMENT_NORMALIZED_VALUE}"),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("dot-segment-normalized fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_DOT_SEGMENT_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: Unicode case folding is composed with reversible decoding and
    /// URL backslash normalization before completed evidence is retained.
    #[test]
    fn web_search_rejects_case_folded_decoded_backslash_credential_in_result_url() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_BACKSLASH_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("case-folded decoded backslash fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_DECODED_CASE_BACKSLASH_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: case-insensitive host canonicalization cannot conceal an
    /// embedded credential in a provider result host.
    #[test]
    fn web_search_rejects_embedded_case_normalized_credential_in_result_host() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_EMBEDDED_HOST_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("embedded host fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_EMBEDDED_HOST_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: Unicode host decoding and case normalization cannot conceal an
    /// embedded credential in a provider result host.
    #[test]
    fn web_search_rejects_embedded_unicode_case_normalized_credential_in_result_host() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_UNICODE_EMBEDDED_HOST_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("Unicode host fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_UNICODE_HOST_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// IP-literal result hosts bypass domain-only IDNA decoding and remain
    /// valid structured search evidence.
    #[test]
    fn web_search_preserves_ipv6_literal_result_host() {
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_IPV6_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("IPv6 fixture result is admitted");
        let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");

        assert!(success_evidence(response, &scrubber()).is_ok());
    }

    /// INV-035: equivalent IPv6 spellings cannot conceal a credential in a
    /// provider result host after URL canonicalization.
    #[test]
    fn web_search_rejects_canonicalized_ipv6_credential_in_result_host() {
        let result = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_IPV6_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("IPv6 fixture result is admitted");
        let response = WebSearchResponse::new(vec![result], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_IPV6_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: WHATWG legacy IPv4 serialization cannot conceal a credential
    /// reflected in a provider result host.
    #[test]
    fn web_search_rejects_canonicalized_legacy_ipv4_credential_in_result_host() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: String::from(FIXTURE_LEGACY_IPV4_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("legacy IPv4 fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_LEGACY_IPV4_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        assert_eq!(
            success_evidence(response, &scrubber),
            Err(WebSearchExecutorError::EvidenceEncoding)
        );
    }

    /// INV-035: a standard HTML character reference cannot conceal a
    /// credential reflected in provider-controlled text.
    #[test]
    fn web_search_redacts_html_encoded_credential_in_result_text() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(HTML_ENTITY_COLLISION_VALUE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("HTML entity fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            HTML_ENTITY_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
        let content = completed_text(evidence);

        assert!(!content.contains(HTML_ENTITY_COLLISION_KEY));
        assert!(!content.contains(HTML_ENTITY_COLLISION_VALUE));
    }

    /// INV-035: an over-window numeric HTML reference fails closed before
    /// provider-controlled evidence is retained.
    #[test]
    fn web_search_rejects_over_window_numeric_reference_in_result_text() {
        let reflection = over_window_numeric_html_reflection();
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: reflection.clone(),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("over-window numeric-reference fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber)
            .expect("over-window numeric reference is fail-closed redacted");
        let content = completed_text(evidence);

        assert!(!content.contains(OVER_WINDOW_NUMERIC_HTML_COLLISION_KEY));
        assert!(!content.contains(&reflection));
    }

    /// INV-035: an HTML C1 numeric reference is decoded through its standard
    /// replacement mapping before provider evidence is retained.
    #[test]
    fn web_search_redacts_c1_numeric_reference_credential_in_result_text() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(HTML_NUMERIC_C1_COLLISION_VALUE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("HTML numeric-reference fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            HTML_NUMERIC_C1_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
        let content = completed_text(evidence);

        assert!(!content.contains(HTML_NUMERIC_C1_COLLISION_KEY));
        assert!(!content.contains(HTML_NUMERIC_C1_COLLISION_VALUE));
    }

    /// INV-035: canonical Unicode normalization cannot conceal a credential
    /// reflected in an ordinary provider title.
    #[test]
    fn web_search_redacts_unicode_normalized_credential_in_result_text() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(URL_UNICODE_HOST_COLLISION_KEY),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("Unicode text fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY
                .as_bytes()
                .to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
        let content = completed_text(evidence);

        assert!(!content.contains(URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY));
        assert!(!content.contains(URL_UNICODE_HOST_COLLISION_KEY));
    }

    /// INV-035: repeated HTML character-reference decoding cannot conceal a
    /// credential reflected in provider-controlled text.
    #[test]
    fn web_search_redacts_nested_html_encoded_credential_in_result_text() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(HTML_NESTED_ENTITY_COLLISION_VALUE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("nested HTML entity fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            HTML_ENTITY_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
        let content = completed_text(evidence);

        assert!(!content.contains(HTML_ENTITY_COLLISION_KEY));
        assert!(!content.contains(HTML_NESTED_ENTITY_COLLISION_VALUE));
    }

    /// INV-035: composed form and HTML decoding cannot conceal a credential
    /// reflected in provider-controlled text.
    #[test]
    fn web_search_redacts_form_then_html_encoded_credential_in_result_text() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FORM_HTML_COLLISION_VALUE),
            url: String::from(FIXTURE_RESULT_URL),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("cross-codec fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            HTML_ENTITY_COLLISION_KEY.as_bytes().to_vec(),
        ))
        .expect("fixture credential is usable");

        let evidence = success_evidence(response, &scrubber).expect("response is safely redacted");
        let content = completed_text(evidence);

        assert!(!content.contains(HTML_ENTITY_COLLISION_KEY));
        assert!(!content.contains(FORM_HTML_COLLISION_VALUE));
    }

    /// INV-035: an early HTML reference is still decoded when a later
    /// multibyte scalar crosses the character-reference scan bound.
    #[test]
    fn web_search_html_reference_scan_handles_multibyte_boundaries() {
        let reflection = html_multibyte_boundary_reflection();

        assert!(encoded_contains_credential(
            &reflection,
            HTML_ENTITY_COLLISION_KEY
        ));
    }

    /// Character-reference terminator search is bounded at every ampersand in
    /// a maximum-size provider body.
    #[test]
    fn web_search_html_reference_scan_bounds_distant_terminator_work() {
        let source = distant_html_reference_terminator();

        let (decoded, changed) = decode_html_character_references(&source)
            .expect("ordinary distant terminator remains literal text");

        assert_eq!(decoded, source);
        assert!(!changed);
    }

    #[test]
    fn web_search_rejects_result_url_encoding_beyond_the_decode_bound() {
        let reflected = WebSearchResult::try_new(WebSearchResultFields {
            title: String::from(FIXTURE_RESULT_TITLE),
            url: format!("{FIXTURE_RESULT_URL}?token={EXCESSIVE_FORM_ENCODING_VALUE}"),
            snippet: String::from(FIXTURE_RESULT_SNIPPET),
        })
        .expect("deeply encoded fixture result is admitted");
        let response = WebSearchResponse::new(vec![reflected], WebSearchPageCompleteness::Complete)
            .expect("fixture response is admitted");

        assert_eq!(
            success_evidence(response, &scrubber()),
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
        let content = completed_text(evidence);

        assert!(!content.contains(SENTINEL_OVERLAPPING_KEY));
        assert!(!content.contains(SHAPED_SECRET));
    }

    /// INV-035: fixed JSON member names cannot collide with the credential in
    /// completed evidence, even when provider fields contain no credential.
    #[test]
    fn web_search_final_success_payload_rejects_credential_collision() {
        let scrubber = CredentialScrubber::try_new(&CredentialValue::new(
            SUCCESS_PAYLOAD_COLLISION_KEY.as_bytes().to_vec(),
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
        let detail = provider_error_detail(error, &scrubber())
            .expect("detail is admitted")
            .expect("detail does not collide");

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
        let detail = provider_error_detail(error, &scrubber())
            .expect("detail is admitted")
            .expect("detail does not collide");

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

        assert_eq!(provider_error_detail(error, &scrubber), Ok(None));
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

    /// INV-035: canonical Unicode normalization cannot conceal a credential
    /// in a query before provider dispatch.
    #[test]
    fn brave_request_rejects_unicode_normalized_query_credential_collision() {
        let failure = build_brave_request(
            URL_UNICODE_HOST_COLLISION_KEY,
            URL_DECOMPOSED_UNICODE_HOST_COLLISION_KEY,
        )
        .expect_err("Unicode-normalized credential query is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::RequestFailed
        );
    }

    /// INV-035: a reversibly encoded API key in the query fails before URL
    /// serialization can double-encode it or dispatch it to the provider.
    #[test]
    fn brave_request_rejects_percent_encoded_query_credential_collision() {
        assert!(matches!(
            build_brave_request(URL_ENCODED_COLLISION_VALUE, URL_ENCODED_COLLISION_KEY),
            Err(WebSearchTransportFailure::RequestFailed)
        ));
    }

    /// INV-035: an HTML-reference spelling of the API key fails before URL
    /// construction can dispatch it as provider-controlled query text.
    #[test]
    fn brave_request_rejects_html_encoded_query_credential_collision() {
        let failure = build_brave_request(HTML_ENTITY_COLLISION_VALUE, HTML_ENTITY_COLLISION_KEY)
            .expect_err("HTML-encoded credential query is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::RequestFailed
        );
    }

    /// INV-035: repeated HTML character-reference decoding cannot conceal an
    /// API key in a query before dispatch.
    #[test]
    fn brave_request_rejects_nested_html_encoded_query_credential_collision() {
        let failure = build_brave_request(
            HTML_NESTED_ENTITY_COLLISION_VALUE,
            HTML_ENTITY_COLLISION_KEY,
        )
        .expect_err("nested HTML-encoded credential query is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::RequestFailed
        );
    }

    /// INV-035: composed form and HTML decoding cannot conceal an API key in a
    /// query before dispatch.
    #[test]
    fn brave_request_rejects_form_then_html_encoded_query_credential_collision() {
        let failure = build_brave_request(FORM_HTML_COLLISION_VALUE, HTML_ENTITY_COLLISION_KEY)
            .expect_err("cross-codec credential query is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::RequestFailed
        );
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

    /// INV-035: canonicalized scheme spelling in fixed request metadata cannot
    /// conceal the credential before dispatch.
    #[test]
    fn brave_request_rejects_case_normalized_fixed_scheme_collision() {
        let failure = build_brave_request(FIXTURE_QUERY, URL_SCHEME_CASE_COLLISION_KEY)
            .expect_err("case-normalized fixed scheme collision is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::RequestFailed
        );
    }

    /// INV-035: canonicalized host spelling in fixed request metadata cannot
    /// conceal the credential before dispatch.
    #[test]
    fn brave_request_rejects_case_normalized_fixed_host_collision() {
        let failure = build_brave_request(FIXTURE_QUERY, PROVIDER_HOST_CASE_COLLISION_KEY)
            .expect_err("case-normalized fixed host collision is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::RequestFailed
        );
    }

    /// INV-035: canonicalized media-type spelling in fixed request metadata
    /// cannot conceal the credential before dispatch.
    #[test]
    fn brave_request_rejects_case_normalized_fixed_media_type_collision() {
        let failure = build_brave_request(FIXTURE_QUERY, ACCEPT_HEADER_CASE_COLLISION_KEY)
            .expect_err("case-normalized fixed media-type collision is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::RequestFailed
        );
    }

    /// INV-035: optional HTTP field whitespace cannot alter a credential at
    /// the header boundary before dispatch.
    #[test]
    fn brave_request_rejects_leading_header_whitespace_in_credential() {
        let failure = build_brave_request(FIXTURE_QUERY, LEADING_HEADER_WHITESPACE_KEY)
            .expect_err("boundary-whitespace credential is rejected");

        assert_eq!(
            failure.class(),
            WebSearchTransportFailureClass::InvalidCredential
        );
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

    /// INV-035: a successful response whose fixed Debug rendering collides
    /// with the request credential is replaced before leaving the transport.
    #[test]
    fn web_search_transport_rejects_success_diagnostic_credential_collision() {
        const RESPONSE_DIAGNOSTIC_COLLISION_KEY: &str = "Complete";
        let credential =
            CredentialValue::new(RESPONSE_DIAGNOSTIC_COLLISION_KEY.as_bytes().to_vec());
        let outcome =
            credential_safe_transport_outcome(Ok(response_with_result_count(1)), &credential);

        assert!(!format!("{outcome:?}").contains(RESPONSE_DIAGNOSTIC_COLLISION_KEY));

        let failure = outcome
            .into_result()
            .expect_err("colliding success diagnostic fails closed");

        assert!(!format!("{failure:?}").contains(RESPONSE_DIAGNOSTIC_COLLISION_KEY));
        assert!(
            !failure
                .to_string()
                .contains(RESPONSE_DIAGNOSTIC_COLLISION_KEY)
        );
        assert!(matches!(
            failure,
            WebSearchTransportFailure::CredentialDiagnosticCollision(_)
        ));
    }

    /// INV-035: a transport failure's case-normalized fixed Debug spelling
    /// cannot survive in the public transport outcome.
    #[test]
    fn web_search_transport_rejects_case_normalized_failure_collision() {
        let credential = CredentialValue::new(
            TRANSPORT_CASE_NORMALIZED_FAILURE_COLLISION_KEY
                .as_bytes()
                .to_vec(),
        );
        let outcome = WebSearchTransportOutcome::failed(
            WebSearchTransportFailure::RequestFailed,
            &credential,
        );

        assert!(!unicode_case_insensitive_contains(
            &format!("{outcome:?}"),
            TRANSPORT_CASE_NORMALIZED_FAILURE_COLLISION_KEY
        ));
        assert!(matches!(
            outcome.into_result(),
            Err(WebSearchTransportFailure::CredentialDiagnosticCollision(_))
        ));
    }

    /// INV-035: the public successful transport outcome cannot synthesize a
    /// request credential in its outer `Result` diagnostic.
    #[test]
    fn web_search_transport_rejects_ok_wrapper_credential_collision() {
        const OK_WRAPPER_COLLISION_KEY: &str = "Ok";
        let credential = CredentialValue::new(OK_WRAPPER_COLLISION_KEY.as_bytes().to_vec());
        let outcome =
            credential_safe_transport_outcome(Ok(response_with_result_count(1)), &credential);

        assert!(!format!("{outcome:?}").contains(OK_WRAPPER_COLLISION_KEY));

        let failure = outcome
            .into_result()
            .expect_err("colliding success wrapper fails closed");

        assert_eq!(
            transport_failure_diagnostic_class(&failure),
            WebSearchCredentialDiagnosticClass::CallerOrHubBug
        );
    }

    /// INV-035: the public failed transport outcome cannot synthesize a
    /// request credential in its outer `Result` diagnostic and preserves the
    /// original failure class.
    #[test]
    fn web_search_transport_rejects_err_wrapper_credential_collision() {
        const ERR_WRAPPER_COLLISION_KEY: &str = "Err";
        let credential = CredentialValue::new(ERR_WRAPPER_COLLISION_KEY.as_bytes().to_vec());
        let outcome = credential_safe_transport_outcome(
            Err(WebSearchTransportFailure::DispatchUnknown),
            &credential,
        );

        assert!(!format!("{outcome:?}").contains(ERR_WRAPPER_COLLISION_KEY));

        let failure = outcome
            .into_result()
            .expect_err("colliding failure wrapper fails closed");

        assert_eq!(
            transport_failure_diagnostic_class(&failure),
            WebSearchCredentialDiagnosticClass::InfrastructureCommitAmbiguous
        );
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
    #[tokio::test]
    async fn web_search_sanitized_dispatch_unknown_stays_commit_ambiguous() {
        const UNKNOWN_PROSE_COLLISION_KEY: &str = "unknown";
        let credentials = StaticCredentials {
            value: UNKNOWN_PROSE_COLLISION_KEY,
        };
        let (_catalog, mut executor) = WebSearchTool::try_new(
            credentials,
            SanitizedDispatchUnknownTransport,
            configuration(),
        )
        .expect("fixture web_search tool compiles")
        .into_parts();
        let correlation = dispatch_correlation();
        let executor_error = executor
            .execute_request(request(), &correlation)
            .await
            .into_result()
            .expect_err("dispatch remains ambiguous");

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

    #[test]
    fn credential_collision_retains_invalid_credential_evidence() {
        let detail = colliding_failure_detail(
            "InvalidCredential",
            WebSearchTransportFailureClass::InvalidCredential,
        );

        assert_eq!(detail, CREDENTIAL_UNAVAILABLE_DETAIL);
    }

    #[test]
    fn credential_collision_retains_request_failure_evidence() {
        let detail = colliding_failure_detail(
            "RequestFailed",
            WebSearchTransportFailureClass::RequestFailed,
        );

        assert_eq!(detail, REQUEST_FAILED_DETAIL);
    }

    #[test]
    fn credential_collision_retains_provider_rejection_evidence() {
        let detail = colliding_failure_detail(
            "ProviderRejected",
            WebSearchTransportFailureClass::ProviderRejected,
        );

        assert_eq!(detail, PROVIDER_REJECTED_DETAIL);
    }

    #[test]
    fn credential_collision_retains_invalid_response_evidence() {
        let detail = colliding_failure_detail(
            "InvalidResponse",
            WebSearchTransportFailureClass::InvalidResponse,
        );

        assert_eq!(detail, INVALID_RESPONSE_DETAIL);
    }

    #[test]
    fn credential_collision_retains_response_overflow_evidence() {
        let detail = colliding_failure_detail(
            "ResponseTooLarge",
            WebSearchTransportFailureClass::ResponseTooLarge,
        );

        assert_eq!(detail, INVALID_RESPONSE_DETAIL);
    }

    /// INV-035: a definitive failure detail that collides with the credential
    /// is omitted without converting the failure into an executor error.
    #[test]
    fn credential_collision_omits_known_failure_detail() {
        let credential = CredentialValue::new(REQUEST_DETAIL_COLLISION_KEY.as_bytes().to_vec());
        let scrubber = CredentialScrubber::try_new(&credential).expect("fixture key is usable");
        let detail = ToolExecutionErrorDetail::try_new(String::from(REQUEST_FAILED_DETAIL))
            .expect("fixture failure detail is valid");

        let evidence = known_failure_evidence(detail, &scrubber)
            .expect("detail collision preserves definitive evidence");

        assert_eq!(known_failure_detail(evidence), None);
    }

    /// INV-035: a colliding definitive request-failure detail commits through
    /// the public service path rather than invoking crash classification.
    #[tokio::test]
    async fn credential_collision_commits_request_failure_without_crash() {
        let (outcome, searches) =
            execute_request_failure_through_service(REQUEST_DETAIL_COLLISION_KEY).await;

        assert!(is_committed_known_failure(&outcome));
        assert_eq!(searches, 1);
    }

    /// INV-035: a case-normalized definitive detail collision is omitted while
    /// the public service still commits the completed request failure.
    #[tokio::test]
    async fn case_normalized_detail_collision_commits_request_failure() {
        let (outcome, searches) =
            execute_request_failure_through_service(CASE_NORMALIZED_REQUEST_DETAIL_COLLISION_KEY)
                .await;

        assert!(is_committed_known_failure_without_detail(&outcome));
        assert_eq!(searches, 1);
    }

    /// INV-035: a dynamic provider-rejection detail that collides with the
    /// credential is omitted while the public service commits known failure.
    #[tokio::test]
    async fn credential_collision_commits_provider_rejection_without_crash() {
        let (outcome, searches) = execute_provider_rejection_through_service().await;

        assert!(is_committed_known_failure_without_detail(&outcome));
        assert_eq!(searches, 1);
    }

    /// INV-035: a post-response sanitization failure is definitive invalid
    /// response evidence rather than a dispatch-ambiguous executor error.
    #[tokio::test]
    async fn web_search_post_response_sanitization_failure_is_known_failed() {
        const EVIDENCE_ERROR_COLLISION_KEY: &str = "encoding";
        let credentials = StaticCredentials {
            value: EVIDENCE_ERROR_COLLISION_KEY,
        };
        let (_catalog, mut executor) =
            WebSearchTool::try_new(credentials, ReflectedTitleTransport, configuration())
                .expect("fixture web_search tool compiles")
                .into_parts();
        let correlation = dispatch_correlation();
        let evidence = executor
            .execute_request(request(), &correlation)
            .await
            .into_result()
            .expect("completed response sanitization has a definitive outcome");
        let detail = known_failure_detail(evidence).expect("invalid response detail is safe");

        assert!(!detail.contains(EVIDENCE_ERROR_COLLISION_KEY));
        assert_eq!(detail, INVALID_RESPONSE_DETAIL);
    }

    /// INV-035: credential-resolution telemetry carries only its safe closed
    /// classification and request correlation.
    #[test]
    fn credential_failure_diagnostic_preserves_safe_classification() {
        let error = CredentialAccessError::new(
            CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
            CredentialAccessFailure::Unmapped,
        );
        let correlation = dispatch_correlation();

        let diagnostic = capture_credential_failure(&error, &correlation);

        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(CREDENTIAL_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_KEY));
    }

    /// INV-035: credential-resolution telemetry cannot inherit credential
    /// bytes from an entered caller span.
    #[test]
    fn credential_failure_diagnostic_ignores_credential_bearing_caller_span() {
        let error = CredentialAccessError::new(
            CredentialReference::new(BRAVE_SEARCH_CREDENTIAL_REFERENCE),
            CredentialAccessFailure::Unmapped,
        );
        let correlation = dispatch_correlation();

        let diagnostic =
            capture_credential_failure_in_credential_span(&error, &correlation, SYNTHETIC_KEY);

        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(CREDENTIAL_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_KEY));
    }

    /// INV-035: unusable-credential telemetry cannot retain the resolved
    /// credential bytes.
    #[test]
    fn unusable_credential_value_diagnostic_preserves_safe_classification() {
        let correlation = dispatch_correlation();
        let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

        let (diagnostic, result) = capture_credential_value_failure(&correlation, &credential);

        result.expect("safe credential value diagnostic is emitted");
        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(CREDENTIAL_VALUE_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_KEY));
    }

    /// INV-035: unusable-credential telemetry cannot inherit resolved
    /// credential bytes from an entered caller span.
    #[test]
    fn unusable_credential_value_diagnostic_ignores_credential_bearing_caller_span() {
        let correlation = dispatch_correlation();
        let credential = CredentialValue::new(LEADING_HEADER_WHITESPACE_KEY.as_bytes().to_vec());

        let (diagnostic, result) =
            capture_credential_value_failure_in_credential_span(&correlation, &credential);

        result.expect("safe credential value diagnostic is emitted outside the caller span");
        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(CREDENTIAL_VALUE_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(LEADING_HEADER_WHITESPACE_KEY));
    }

    /// INV-035: an unusable one-byte credential suppresses colliding telemetry
    /// and cannot form a colliding public bound result.
    #[tokio::test]
    async fn unusable_short_credential_value_diagnostic_is_suppressed() {
        let correlation = dispatch_correlation();
        let credential = CredentialValue::new(SHORT_DIAGNOSTIC_COLLISION_KEY.as_bytes().to_vec());

        let (diagnostic, result) = capture_credential_value_failure(&correlation, &credential);
        let report_error = result.expect_err("credential collision suppresses the diagnostic");
        let (failed, searches, _rendered) = execute_formatted_raw_credential_through_service(
            SHORT_DIAGNOSTIC_COLLISION_KEY.as_bytes(),
        )
        .await;

        assert!(!diagnostic.contains(SHORT_DIAGNOSTIC_COLLISION_KEY));
        assert!(!format!("{report_error:?}").contains(SHORT_DIAGNOSTIC_COLLISION_KEY));
        assert!(
            !report_error
                .to_string()
                .contains(SHORT_DIAGNOSTIC_COLLISION_KEY)
        );
        assert!(failed);
        assert_eq!(searches, 0);
    }

    /// An empty credential is a definitive pre-dispatch known failure, not an
    /// executor crash classification.
    #[tokio::test]
    async fn empty_credential_value_commits_known_failure_without_dispatch() {
        let (outcome, searches) =
            execute_raw_credential_through_service(EMPTY_CREDENTIAL_VALUE).await;

        assert!(is_committed_known_failure(&outcome));
        assert_eq!(searches, 0);
    }

    /// A non-UTF-8 credential is a definitive pre-dispatch known failure, not
    /// an executor crash classification.
    #[tokio::test]
    async fn non_utf8_credential_value_commits_known_failure_without_dispatch() {
        let (outcome, searches) =
            execute_raw_credential_through_service(NON_UTF8_CREDENTIAL_VALUE).await;

        assert!(is_committed_known_failure(&outcome));
        assert_eq!(searches, 0);
    }

    /// INV-035: an interior HTTP-header-invalid byte is a definitive
    /// pre-dispatch failure and never reaches the injected transport.
    #[tokio::test]
    async fn interior_newline_credential_commits_without_dispatch() {
        let (outcome, searches) =
            execute_raw_credential_through_service(INTERIOR_NEWLINE_CREDENTIAL_VALUE).await;

        assert!(is_committed_known_failure(&outcome));
        assert_eq!(searches, 0);
    }

    /// A resolved credential at the application byte bound remains usable.
    #[tokio::test]
    async fn credential_at_byte_bound_may_reach_transport() {
        let credential = vec![b'x'; MAX_CREDENTIAL_BYTES];

        let (outcome, searches) = execute_raw_credential_through_service(&credential).await;

        assert!(is_committed_completed(&outcome));
        assert_eq!(searches, 1);
    }

    /// INV-035: an oversized resolved credential is a definitive pre-dispatch
    /// failure and is never expanded by the scrubber or sent to transport.
    #[tokio::test]
    async fn oversized_credential_commits_without_dispatch() {
        let credential = vec![b'x'; MAX_CREDENTIAL_BYTES + 1];

        let (outcome, searches) = execute_raw_credential_through_service(&credential).await;

        assert!(is_committed_known_failure(&outcome));
        assert_eq!(searches, 0);
    }

    /// INV-035: a credential beyond the bounded inspection budget fails
    /// closed before whole-value normalization, decoding, or dispatch.
    #[tokio::test]
    async fn oversized_credential_above_inspection_bound_fails_closed() {
        let credential = vec![b'x'; MAX_OVERSIZED_CREDENTIAL_INSPECTION_BYTES + 1];

        let (failed, searches, rendered) =
            execute_formatted_raw_credential_through_service(&credential).await;

        assert!(failed);
        assert_eq!(searches, 0);
        assert!(!rendered.contains(std::str::from_utf8(&credential).expect("fixture is UTF-8")));
    }

    /// INV-035: oversized credential values cannot reach telemetry through a
    /// bounded reversible decoding chain.
    #[test]
    fn oversized_encoded_credential_value_diagnostic_is_suppressed() {
        let encoded_once = fully_percent_encode(OVERSIZED_CREDENTIAL_TELEMETRY_COLLISION_VALUE);
        let encoded_twice = fully_percent_encode(&encoded_once);
        let encoded_thrice = fully_percent_encode(&encoded_twice);
        let encoded_four_times = fully_percent_encode(&encoded_thrice);
        let correlation = dispatch_correlation();
        let credential = CredentialValue::new(encoded_four_times.as_bytes().to_vec());

        let (diagnostic, result) = capture_credential_value_failure(&correlation, &credential);
        let report_error = result.expect_err("oversized credential telemetry is suppressed");

        assert!(encoded_four_times.len() > MAX_CREDENTIAL_BYTES);
        assert!(diagnostic.is_empty());
        assert!(!format!("{report_error:?}").contains(&encoded_four_times));
        assert!(
            !format!("{report_error:?}").contains(OVERSIZED_CREDENTIAL_TELEMETRY_COLLISION_VALUE)
        );
    }

    /// INV-035: bounded reversible variants of an oversized credential remain
    /// checked against the public executor result.
    #[tokio::test]
    async fn oversized_encoded_bound_wrapper_collision_is_suppressed() {
        let encoded_once = fully_percent_encode(OVERSIZED_BOUND_WRAPPER_COLLISION_VALUE);
        let encoded_twice = fully_percent_encode(&encoded_once);
        let encoded_thrice = fully_percent_encode(&encoded_twice);
        let encoded_four_times = fully_percent_encode(&encoded_thrice);

        let (failed, searches, rendered) =
            execute_formatted_raw_credential_through_service(encoded_four_times.as_bytes()).await;

        assert!(encoded_four_times.len() > MAX_CREDENTIAL_BYTES);
        assert!(failed);
        assert_eq!(searches, 0);
        assert!(!rendered.contains(&encoded_four_times));
        assert!(!rendered.contains(OVERSIZED_BOUND_WRAPPER_COLLISION_VALUE));
    }

    /// INV-035: a credential with trailing HTTP field whitespace commits
    /// definitive pre-dispatch evidence without reaching injected transport.
    #[tokio::test]
    async fn trailing_header_whitespace_credential_commits_without_dispatch() {
        let (outcome, searches) =
            execute_raw_credential_through_service(TRAILING_HEADER_WHITESPACE_KEY).await;

        assert!(is_committed_known_failure(&outcome));
        assert_eq!(searches, 0);
    }

    /// INV-035: transport-failure telemetry cannot retain the request
    /// credential.
    #[test]
    fn transport_failure_diagnostic_preserves_safe_classification() {
        let correlation = dispatch_correlation();
        let failure = WebSearchTransportFailure::RequestFailed;
        let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

        let (diagnostic, result) = capture_transport_failure(&failure, &correlation, &credential);

        result.expect("safe transport diagnostic is emitted");
        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TRANSPORT_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_KEY));
    }

    /// INV-035: an incomplete provider-rejection body emits its retained safe
    /// failure class before the definitive rejection is coarsened.
    #[test]
    fn provider_rejection_body_failure_reports_safe_classification() {
        let correlation = dispatch_correlation();
        let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

        let (diagnostic, result) = capture_response_body_failure(
            WebSearchTransportFailureClass::DispatchUnknown,
            &correlation,
            &credential,
        );

        result.expect("safe provider-response body diagnostic is emitted");
        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(RESPONSE_BODY_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_KEY));
    }

    /// INV-035: post-response sanitization reports a credential-safe closed
    /// discriminant before its typed error becomes invalid-response evidence.
    #[test]
    fn response_sanitization_failure_reports_safe_classification() {
        let correlation = dispatch_correlation();
        let credential = CredentialValue::new(SYNTHETIC_KEY.as_bytes().to_vec());

        let (diagnostic, result) = capture_response_sanitization_failure(&correlation, &credential);

        result.expect("safe response-sanitization diagnostic is emitted");
        assert!(diagnostic.contains(SESSION_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(TURN_ID_DIAGNOSTIC));
        assert!(diagnostic.contains(RESPONSE_SANITIZATION_FAILURE_CLASSIFICATION));
        assert!(!diagnostic.contains(SYNTHETIC_KEY));
    }

    /// INV-035: a case-normalized credential collision in the controlled
    /// response-sanitization event suppresses telemetry and stays opaque.
    #[test]
    fn response_sanitization_failure_omits_case_normalized_credential_collision() {
        let correlation = dispatch_correlation();
        let credential = CredentialValue::new(
            RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY
                .as_bytes()
                .to_vec(),
        );

        let (diagnostic, result) = capture_response_sanitization_failure(&correlation, &credential);

        let error = result.expect_err("case-normalized credential suppresses the event");
        assert!(!unicode_case_insensitive_contains(
            &diagnostic,
            RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY,
        ));
        assert!(!unicode_case_insensitive_contains(
            &format!("{error:?}"),
            RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY,
        ));
        assert!(!unicode_case_insensitive_contains(
            &error.to_string(),
            RESPONSE_SANITIZATION_CASE_NORMALIZED_COLLISION_KEY,
        ));
    }

    /// INV-035: compact formatter timestamps and ANSI metadata are accounted
    /// for before a post-credential transport event can be emitted.
    #[test]
    fn web_search_transport_event_omits_timestamp_credential_collision() {
        let correlation = dispatch_correlation();
        let failure = WebSearchTransportFailure::RequestFailed;
        let credential = CredentialValue::new(TIMESTAMP_COLLISION_KEY.as_bytes().to_vec());

        let (diagnostic, result) = capture_transport_failure(&failure, &correlation, &credential);

        let error = result.expect_err("timestamp-shaped credential suppresses the event");
        assert!(!diagnostic.contains(TIMESTAMP_COLLISION_KEY));
        assert!(!format!("{error:?}").contains(TIMESTAMP_COLLISION_KEY));
        assert!(!error.to_string().contains(TIMESTAMP_COLLISION_KEY));
    }

    /// INV-035: a credential spanning compact formatter metadata and event
    /// text suppresses the complete daemon-shaped event.
    #[test]
    fn web_search_transport_event_omits_formatter_boundary_collision() {
        let correlation = dispatch_correlation();
        let failure = WebSearchTransportFailure::RequestFailed;
        let credential =
            CredentialValue::new(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY.as_bytes().to_vec());

        let (diagnostic, result) = capture_transport_failure(&failure, &correlation, &credential);

        let error = result.expect_err("formatter-boundary credential suppresses the event");
        assert!(!diagnostic.contains(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY));
        assert!(diagnostic.is_empty());
        assert!(!format!("{error:?}").contains(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY));
        assert!(
            !error
                .to_string()
                .contains(FORMATTER_EVENT_BOUNDARY_COLLISION_KEY)
        );
    }

    /// INV-035: generating a safe transport diagnostic fails closed when the
    /// ordinary redaction sentinel itself contains the credential.
    #[test]
    fn web_search_transport_diagnostic_redaction_overlap_fails_closed() {
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

    #[test]
    fn provider_error_constructor_rejects_non_http_and_success_statuses() {
        assert!(WebSearchProviderError::new(0, Vec::new()).is_none());
        assert!(WebSearchProviderError::new(StatusCode::OK.as_u16(), Vec::new()).is_none());
        assert!(WebSearchProviderError::new(StatusCode::CREATED.as_u16(), Vec::new()).is_none());
        assert!(WebSearchProviderError::new(PROVIDER_REJECTION_STATUS, Vec::new()).is_some());
    }

    /// A received non-success status proves provider rejection even when the
    /// response body stream subsequently fails; no partial body is retained.
    #[test]
    fn provider_rejection_survives_incomplete_error_body() {
        let failure = finish_provider_response(
            WebSearchProvider::Brave,
            StatusCode::TOO_MANY_REQUESTS,
            Err(WebSearchTransportFailure::DispatchUnknown),
        )
        .expect_err("received rejection status is conclusive");
        let error = provider_rejection(failure);

        assert_eq!(error.status, PROVIDER_REJECTION_STATUS);
        assert!(error.body.is_empty());
        assert_eq!(
            error.body_failure_class,
            Some(WebSearchTransportFailureClass::DispatchUnknown)
        );
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
        assert_eq!(response.results().len(), 1);
        let decoded = &response.results()[0];

        assert_eq!(decoded.title(), FIXTURE_RESULT_TITLE);
        assert_eq!(decoded.url(), FIXTURE_RESULT_URL);
        assert_eq!(decoded.snippet(), FIXTURE_RESULT_SNIPPET);
        assert!(!response.more_results_available());
    }

    /// A recorded Brave success envelope with `web: null` is an empty page;
    /// the required pagination fact still determines completeness.
    #[test]
    fn brave_recorded_null_web_response_decodes_empty_results() {
        let body = serde_json::to_vec(&serde_json::json!({
            "type": "search",
            "query": {
                "original": FIXTURE_QUERY,
                "more_results_available": false,
            },
            "web": null,
        }))
        .expect("recorded response fixture encodes");

        let response = decode_provider_response(WebSearchProvider::Brave, &body)
            .expect("recorded empty provider response decodes");

        assert!(response.results().is_empty());
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

    /// A success envelope without a provider result list cannot fabricate an
    /// authoritative empty page.
    #[test]
    fn brave_response_without_result_list_is_invalid() {
        let body = serde_json::to_vec(&serde_json::json!({
            "type": "search",
            "query": {
                "original": FIXTURE_QUERY,
                "more_results_available": false,
            },
            "web": {
                "type": "search",
            },
        }))
        .expect("recorded response fixture encodes");

        assert!(matches!(
            decode_provider_response(WebSearchProvider::Brave, &body),
            Err(WebSearchTransportFailure::InvalidResponse)
        ));
    }
}
