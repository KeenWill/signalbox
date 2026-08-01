use std::{error::Error, fmt};

use reqwest::{StatusCode, Url};

use super::diagnostic::*;

pub(super) const MAX_PROVIDER_RESULTS: usize = 20;

pub(super) const MAX_RETURNED_RESULTS: usize = 10;

pub(super) const MAX_PROVIDER_RESPONSE_BYTES: usize = 512 * 1024;

pub(super) const MAX_RESULT_TITLE_BYTES: usize = 2 * 1024;

pub(super) const MAX_RESULT_URL_BYTES: usize = 8 * 1024;

pub(super) const MAX_RESULT_SNIPPET_BYTES: usize = 16 * 1024;

/// One checked provider result.
#[derive(Clone, Eq, PartialEq)]
pub struct WebSearchResult {
    pub(super) title: String,
    pub(super) source_url: String,
    pub(super) url: String,
    pub(super) snippet: String,
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
            result_count: self.results.len(),
            completeness: self.completeness,
        }
        .fmt(formatter)
    }
}

#[derive(Clone, Copy)]
pub(super) struct WebSearchResponseDebug {
    pub(super) result_count: usize,
    pub(super) completeness: WebSearchPageCompleteness,
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
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
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

    pub(super) fn with_body_failure_class(
        mut self,
        failure_class: WebSearchTransportFailureClass,
    ) -> Self {
        self.body_failure_class = Some(failure_class);
        self
    }
}
