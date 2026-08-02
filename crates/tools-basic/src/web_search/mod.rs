//! Bounded credentialed web search with an explicit provider boundary.

// The redaction core has no in-crate caller until tool composition is wired;
// the composition slice removes this allowance.
#![allow(dead_code)]

mod diagnostic;
mod egress;
mod evidence;
mod redaction;
mod result;
mod text_decoding;

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
mod result_property_tests;
#[cfg(test)]
mod result_tests;
#[cfg(test)]
mod test_support;

pub use diagnostic::{WebSearchCredentialDiagnostic, WebSearchExecutorError};
pub use egress::{
    BRAVE_SEARCH_CREDENTIAL_REFERENCE, WebSearchConfiguration, WebSearchEgressPolicy,
    WebSearchProvider,
};
pub use result::{
    WebSearchPageCompleteness, WebSearchProviderError, WebSearchResponse, WebSearchResult,
    WebSearchResultFields,
};
