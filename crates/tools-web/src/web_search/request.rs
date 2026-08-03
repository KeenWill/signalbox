use reqwest::{
    Client, Url,
    header::{ACCEPT, HeaderValue},
};
use signalbox_model_runtime::CredentialValue;

use super::{egress::*, redaction::*, result::*, transport_failure::*};

/// One typed query pinned to the configured provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSearchRequest {
    pub(super) provider: WebSearchProvider,
    pub(super) query: String,
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

pub(super) fn build_provider_request(
    client: &Client,
    request: &WebSearchRequest,
    credential: &CredentialValue,
) -> Result<reqwest::Request, WebSearchTransportFailure> {
    let endpoint = request.provider.endpoint();
    if credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES
        || has_http_header_boundary_whitespace(credential.expose_bytes())
    {
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

pub(super) fn query_contains_credential(query: &str, credential: &str) -> bool {
    text_contains_credential_variant(query, credential)
}

pub(super) fn provider_request_url(request: &WebSearchRequest) -> Option<Url> {
    let mut url = Url::parse(request.provider.endpoint().url).ok()?;
    let result_count = provider_result_count_query();
    url.query_pairs_mut()
        .append_pair("q", request.query())
        .append_pair("count", &result_count)
        .append_pair("result_filter", "web")
        .append_pair("text_decorations", "false");
    Some(url)
}

pub(super) fn provider_result_count_query() -> String {
    MAX_PROVIDER_RESULTS.to_string()
}

pub(super) fn serialized_request_url_contains_credential(
    request: &WebSearchRequest,
    credential: &str,
) -> bool {
    provider_request_url(request)
        .is_none_or(|url| text_contains_credential_variant(url.as_str(), credential))
}

pub(super) fn credential_debug_contains_credential(
    credential: &CredentialValue,
    credential_text: &str,
) -> bool {
    text_contains_credential_variant(&format!("{credential:?}"), credential_text)
}

pub(super) fn request_credential_debug_contains_credential(
    request: &WebSearchRequest,
    credential: &CredentialValue,
    credential_text: &str,
) -> bool {
    text_contains_credential_variant(&format!("{request:?} {credential:?}"), credential_text)
}

pub(super) fn fixed_success_payload_contains_credential(credential: &str) -> bool {
    fixed_success_payloads().any(|payload| text_contains_credential_variant(&payload, credential))
}

pub(super) fn fixed_result_debug_contains_credential(credential: &str) -> bool {
    fixed_result_diagnostic_outputs()
        .iter()
        .any(|output| text_contains_credential_variant(output, credential))
}

pub(super) fn fixed_success_payloads() -> impl Iterator<Item = String> {
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

pub(super) fn fixed_request_metadata_contains_credential(
    request: &WebSearchRequest,
    credential: &str,
) -> bool {
    let endpoint = request.provider.endpoint();
    let result_count = provider_result_count_query();
    [
        endpoint.url,
        endpoint.credential_header,
        "GET",
        ACCEPT.as_str(),
        "application/json",
        "q",
        "count",
        result_count.as_str(),
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
        || fixed_result_debug_contains_credential(credential)
}
