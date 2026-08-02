//! Built-in web egress tools.

mod web_fetch;
mod web_search;

pub use web_fetch::{
    PublicDestinationClientError, ReqwestWebFetchConstructionError, ReqwestWebFetchTransport,
    WEB_FETCH_NAME, WebFetchBodyCompleteness, WebFetchEgressPolicy, WebFetchEgressPolicyError,
    WebFetchExecutor, WebFetchExecutorError, WebFetchRequest, WebFetchResponse, WebFetchTool,
    WebFetchToolConstructionError, WebFetchTransport, WebFetchTransportFailure,
    has_more_response_bytes, public_destination_client,
};
pub use web_search::{
    BRAVE_SEARCH_CREDENTIAL_REFERENCE, ReqwestWebSearchConstructionError,
    ReqwestWebSearchTransport, WEB_SEARCH_NAME, WebSearchConfiguration,
    WebSearchCredentialDiagnostic, WebSearchEgressPolicy, WebSearchExecutor,
    WebSearchExecutorError, WebSearchPageCompleteness, WebSearchProvider, WebSearchProviderError,
    WebSearchRequest, WebSearchResponse, WebSearchResult, WebSearchResultFields, WebSearchTool,
    WebSearchToolConstructionError, WebSearchTransport, WebSearchTransportFailure,
    WebSearchTransportOutcome,
};
