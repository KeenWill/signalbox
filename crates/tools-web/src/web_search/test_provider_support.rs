use signalbox_model_runtime::CredentialValue;

use super::{
    egress::*, redaction::*, request::*, result::*, test_support::*, transport::*,
    transport_failure::*,
};

pub(super) const FIXTURE_QUERY: &str = "bounded rust search";

pub(super) const FIXTURE_MORE_RESULTS_AVAILABLE: bool = false;

pub(super) const ACCEPT_HEADER_COLLISION_KEY: &str = "application/json";

pub(super) const ACCEPT_HEADER_CASE_COLLISION_KEY: &str = "APPLICATION/JSON";

pub(super) const PROVIDER_HOST_CASE_COLLISION_KEY: &str = "API.SEARCH.BRAVE.COM";

pub(super) const SAFE_SUPPORTED_NAMED_ENTITY_VALUE: &str = "safe&nbsp;value";

pub(super) const LEADING_HEADER_WHITESPACE_KEY: &str = " fixture-search-key";

pub(super) const DIAGNOSTIC_REDACTION_OVERLAP_KEY: &str = "e";

pub(super) const TRANSPORT_CASE_NORMALIZED_FAILURE_COLLISION_KEY: &str = "requestfailed";

pub(super) fn request() -> WebSearchRequest {
    WebSearchRequest {
        provider: WebSearchProvider::Brave,
        query: String::from(FIXTURE_QUERY),
    }
}

#[track_caller]
pub(super) fn provider_rejection(failure: WebSearchTransportFailure) -> WebSearchProviderError {
    match failure {
        WebSearchTransportFailure::ProviderRejected(error) => error,
        other => panic!("expected provider rejection, got {other:?}"),
    }
}

pub(super) fn oversized_provider_credential() -> String {
    "x".repeat(MAX_CREDENTIAL_BYTES + 1)
}

pub(super) fn build_brave_request(
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

pub(super) fn brave_request() -> reqwest::Request {
    build_brave_request(FIXTURE_QUERY, SYNTHETIC_KEY).expect("fixture request builds")
}
