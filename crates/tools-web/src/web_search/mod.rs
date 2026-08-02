//! Bounded credentialed web search with an explicit provider boundary.

mod arguments;
mod binding;
mod brave;
mod diagnostic;
mod egress;
mod evidence;
mod executor;
mod redaction;
mod request;
mod result;
mod telemetry;
mod text_decoding;
mod tool;
mod transport;
mod transport_failure;

#[cfg(test)]
mod binding_tests;
#[cfg(test)]
mod brave_tests;
#[cfg(test)]
mod credential_tests;
#[cfg(test)]
mod egress_tests;
#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod executor_tests;
#[cfg(test)]
mod live_smoke_tests;
#[cfg(test)]
mod property_support;
#[cfg(test)]
mod redaction_property_tests;
#[cfg(test)]
mod redaction_tests;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod result_property_tests;
#[cfg(test)]
mod result_tests;
#[cfg(test)]
mod telemetry_tests;
#[cfg(test)]
mod test_provider_support;
#[cfg(test)]
mod test_service_support;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod test_telemetry_support;
#[cfg(test)]
mod tool_tests;
#[cfg(test)]
mod transport_tests;

pub use diagnostic::{WebSearchCredentialDiagnostic, WebSearchExecutorError};
pub use egress::{
    BRAVE_SEARCH_CREDENTIAL_REFERENCE, WebSearchConfiguration, WebSearchEgressPolicy,
    WebSearchProvider,
};
pub use executor::WebSearchExecutor;
pub use request::WebSearchRequest;
pub use result::{
    WebSearchPageCompleteness, WebSearchProviderError, WebSearchResponse, WebSearchResult,
    WebSearchResultFields,
};
pub use tool::{WEB_SEARCH_NAME, WebSearchTool, WebSearchToolConstructionError};
pub use transport::{
    ReqwestWebSearchConstructionError, ReqwestWebSearchTransport, WebSearchTransport,
};
pub use transport_failure::{WebSearchTransportFailure, WebSearchTransportOutcome};
