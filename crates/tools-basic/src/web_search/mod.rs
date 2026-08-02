//! Bounded credentialed web search with an explicit provider boundary.

// The provider client has no in-crate caller until tool composition is wired;
// the composition slice removes this allowance.
#![allow(dead_code)]

mod brave;
mod canonicalization;
mod diagnostic;
mod egress;
mod evidence;
mod redaction;
mod request;
mod result;
mod text_decoding;
mod transport;
mod transport_failure;

#[cfg(test)]
mod brave_tests;
#[cfg(test)]
mod canonicalization_tests;
#[cfg(test)]
mod egress_tests;
#[cfg(test)]
mod evidence_tests;
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
mod test_provider_support;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod transport_tests;

pub use diagnostic::{WebSearchCredentialDiagnostic, WebSearchExecutorError};
pub use egress::{
    BRAVE_SEARCH_CREDENTIAL_REFERENCE, WebSearchConfiguration, WebSearchEgressPolicy,
    WebSearchProvider,
};
pub use request::WebSearchRequest;
pub use result::{
    WebSearchPageCompleteness, WebSearchProviderError, WebSearchResponse, WebSearchResult,
    WebSearchResultFields,
};
pub use transport::{
    ReqwestWebSearchConstructionError, ReqwestWebSearchTransport, WebSearchTransport,
};
pub use transport_failure::{WebSearchTransportFailure, WebSearchTransportOutcome};
