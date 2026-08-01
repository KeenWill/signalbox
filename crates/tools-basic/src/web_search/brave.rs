use super::{egress::*, result::*, transport_failure::*};

#[derive(serde::Deserialize)]
pub(super) struct BraveResponse {
    #[serde(rename = "type")]
    pub(super) response_type: String,
    pub(super) query: BraveQueryFacts,
    pub(super) web: Option<BraveWebResults>,
}

#[derive(serde::Deserialize)]
pub(super) struct BraveQueryFacts {
    pub(super) more_results_available: bool,
}

#[derive(serde::Deserialize)]
pub(super) struct BraveWebResults {
    pub(super) results: Vec<BraveResult>,
}

#[derive(serde::Deserialize)]
pub(super) struct BraveResult {
    pub(super) title: String,
    pub(super) url: String,
    #[serde(rename = "description")]
    pub(super) snippet: String,
}

pub(super) fn decode_provider_response(
    provider: WebSearchProvider,
    body: &[u8],
) -> Result<WebSearchResponse, WebSearchTransportFailure> {
    match provider {
        WebSearchProvider::Brave => decode_brave_response(body),
    }
}

pub(super) fn decode_brave_response(
    body: &[u8],
) -> Result<WebSearchResponse, WebSearchTransportFailure> {
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
