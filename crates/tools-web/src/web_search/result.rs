use std::{error::Error, fmt};

use reqwest::StatusCode;
use url::Url;

use super::diagnostic::*;

pub(super) const MAX_PROVIDER_RESULTS: usize = 20;

pub(super) const MAX_RETURNED_RESULTS: usize = 10;

pub(super) const MAX_PROVIDER_RESPONSE_BYTES: usize = 512 * 1024;

pub(super) const MAX_ESCAPED_PROVIDER_ERROR_DETAIL_BYTES: usize =
    MAX_PROVIDER_RESPONSE_BYTES * "&quot;".len();

pub(super) const MAX_RESULT_TITLE_BYTES: usize = 2 * 1024;

pub(super) const MAX_RESULT_URL_BYTES: usize = 8 * 1024;

pub(super) const MAX_RESULT_SNIPPET_BYTES: usize = 16 * 1024;

/// Result query parameters admitted into tool output.
///
/// No provider-neutral result parameter has a security contract at launch, so
/// the allowlist is intentionally empty. A future entry must be reviewed as
/// output data, not inherited from an upstream result URL.
pub(super) const RESULT_QUERY_PARAMETER_ALLOWLIST: &[&str] = &[];

#[derive(Clone, Eq, PartialEq)]
pub(super) struct ResultTitle(String);

#[derive(Clone, Eq, PartialEq)]
pub(super) struct ParsedResultUrl {
    rendered: String,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct EscapedSnippet(String);

/// One checked provider result.
///
/// provider-controlled components remain opaque outside this crate;
/// only request-scoped evidence construction may render them.
///
/// ```compile_fail
/// fn render_provider_components(result: &signalbox_tools_web::WebSearchResult) {
///     let _ = (result.title(), result.url(), result.snippet());
/// }
/// ```
///
/// ```compile_fail
/// fn compare_provider_components(
///     response: &signalbox_tools_web::WebSearchResponse,
///     candidate: &signalbox_tools_web::WebSearchResult,
/// ) {
///     let _ = &response.results()[0] == candidate;
/// }
/// ```
#[derive(Clone)]
pub struct WebSearchResult {
    pub(super) title: ResultTitle,
    pub(super) url: ParsedResultUrl,
    pub(super) snippet: EscapedSnippet,
}

/// Named fields for one provider result.
#[derive(Clone, Eq, PartialEq)]
pub struct WebSearchResultFields {
    /// Result title.
    pub title: String,
    /// Absolute HTTP(S) result URL.
    pub url: String,
    /// Provider-supplied result snippet.
    pub snippet: String,
}

impl fmt::Debug for WebSearchResultFields {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchResultFields")
            .field("title", &"[provider-controlled]")
            .field("url", &"[provider-controlled]")
            .field("snippet", &"[provider-controlled]")
            .finish()
    }
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
    /// Parses one provider result into bounded output components.
    ///
    /// The result URL is reconstructed from its parsed components. User
    /// information and fragments are discarded, and only explicitly
    /// allowlisted query parameters can survive. Titles and snippets are
    /// entity-escaped before they can become tool output.
    pub fn try_new(fields: WebSearchResultFields) -> Option<Self> {
        Some(Self {
            title: ResultTitle::try_new(fields.title)?,
            url: ParsedResultUrl::try_new(&fields.url)?,
            snippet: EscapedSnippet::try_new(&fields.snippet)?,
        })
    }

    /// Entity-escaped provider result title.
    #[cfg(test)]
    pub(super) fn title(&self) -> &str {
        self.title.as_str()
    }

    /// Parsed HTTP(S) result URL with unsafe components discarded.
    #[cfg(test)]
    pub(super) fn url(&self) -> &str {
        self.url.as_str()
    }

    /// Entity-escaped provider result snippet.
    #[cfg(test)]
    pub(super) fn snippet(&self) -> &str {
        self.snippet.as_str()
    }
}

impl ResultTitle {
    fn try_new(title: String) -> Option<Self> {
        if title.len() > MAX_RESULT_TITLE_BYTES || title.trim().is_empty() {
            return None;
        }
        Some(Self(entity_escape(&title, MAX_RESULT_TITLE_BYTES)?))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl ParsedResultUrl {
    pub(super) fn try_new(source: &str) -> Option<Self> {
        if source.len() > MAX_RESULT_URL_BYTES {
            return None;
        }
        let mut parsed = Url::parse(source).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return None;
        }

        parsed.set_username("").ok()?;
        parsed.set_password(None).ok()?;
        parsed.set_fragment(None);
        let retained_query = parsed
            .query_pairs()
            .filter(|(name, _)| result_query_parameter_is_allowed(name))
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        parsed.set_query(None);
        if !retained_query.is_empty() {
            parsed.query_pairs_mut().extend_pairs(retained_query);
        }

        let rendered = parsed.to_string();
        (rendered.len() <= MAX_RESULT_URL_BYTES).then_some(Self { rendered })
    }

    pub(super) fn as_str(&self) -> &str {
        &self.rendered
    }
}

impl EscapedSnippet {
    fn try_new(snippet: &str) -> Option<Self> {
        if snippet.len() > MAX_RESULT_SNIPPET_BYTES {
            return None;
        }
        let escaped = entity_escape(snippet, MAX_RESULT_SNIPPET_BYTES)?;
        Some(Self(escaped))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

pub(super) fn result_query_parameter_is_allowed(name: &str) -> bool {
    RESULT_QUERY_PARAMETER_ALLOWLIST.contains(&name)
}

pub(super) fn entity_escape(source: &str, maximum_bytes: usize) -> Option<String> {
    let escaped = html_escape::encode_quoted_attribute(source);
    (escaped.len() <= maximum_bytes).then(|| escaped.into_owned())
}

/// One complete bounded provider response.
#[derive(Clone)]
pub struct WebSearchResponse {
    pub(super) results: Vec<WebSearchResult>,
    pub(super) completeness: WebSearchPageCompleteness,
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
            completeness: self.completeness,
        }
        .fmt(formatter)
    }
}

#[derive(Clone, Copy)]
pub(super) struct WebSearchResponseDebug {
    pub(super) completeness: WebSearchPageCompleteness,
}

impl fmt::Debug for WebSearchResponseDebug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchResponse")
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

/// Parsed provider rejection facts with no retained raw response bytes.
pub struct WebSearchProviderError {
    pub(super) status: u16,
    pub(super) detail: Option<String>,
    pub(super) body_failure_class: Option<WebSearchTransportFailureClass>,
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

pub(super) fn fixed_result_diagnostic_outputs() -> [String; 9] {
    let fields = WebSearchResultFields {
        title: String::new(),
        url: String::new(),
        snippet: String::new(),
    };
    let result = WebSearchResult {
        title: ResultTitle(String::new()),
        url: ParsedResultUrl {
            rendered: String::new(),
        },
        snippet: EscapedSnippet(String::new()),
    };
    let complete_response = WebSearchResponse {
        results: Vec::new(),
        completeness: WebSearchPageCompleteness::Complete,
    };
    let partial_response = WebSearchResponse {
        results: Vec::new(),
        completeness: WebSearchPageCompleteness::MoreAvailable,
    };
    let provider_error = WebSearchProviderError {
        status: StatusCode::BAD_REQUEST.as_u16(),
        detail: None,
        body_failure_class: None,
    };
    [
        format!("{fields:?}"),
        format!("{result:?}"),
        format!("{:?}", Some(complete_response.clone())),
        format!("{:?}", Some(partial_response.clone())),
        format!("{complete_response:?}"),
        format!("{partial_response:?}"),
        format!("{:?}", Some(&provider_error)),
        format!("{provider_error:?}"),
        provider_error.to_string(),
    ]
}

impl WebSearchProviderError {
    /// Parses one complete provider error body without retaining raw bytes.
    ///
    /// Only the known string-valued `error.detail` component is retained, and
    /// it is entity-escaped before it can become failure evidence. Unknown or
    /// malformed bodies contribute no provider text.
    pub fn new(status: u16, body: Vec<u8>) -> Option<Self> {
        let status_code = StatusCode::from_u16(status).ok()?;
        if status_code.is_success() || body.len() > MAX_PROVIDER_RESPONSE_BYTES {
            return None;
        }
        let detail = parsed_provider_error_detail(&body);
        Some(Self {
            status,
            detail,
            body_failure_class: None,
        })
    }

    pub(super) fn with_body_failure_class(
        mut self,
        failure_class: WebSearchTransportFailureClass,
    ) -> Self {
        self.body_failure_class = Some(failure_class);
        self
    }
}

#[derive(serde::Deserialize)]
struct ProviderErrorEnvelope {
    error: ProviderErrorDetail,
}

#[derive(serde::Deserialize)]
struct ProviderErrorDetail {
    detail: String,
}

fn parsed_provider_error_detail(body: &[u8]) -> Option<String> {
    let envelope = serde_json::from_slice::<ProviderErrorEnvelope>(body).ok()?;
    entity_escape(
        &envelope.error.detail,
        MAX_ESCAPED_PROVIDER_ERROR_DETAIL_BYTES,
    )
}
