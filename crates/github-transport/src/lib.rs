//! Shared GitHub HTTP mechanics; tool suites retain their own result contracts.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{
    Client, Method, RequestBuilder, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, LINK, USER_AGENT},
};
use signalbox_egress_transport::{
    ReqwestWebFetchConstructionError, WebFetchTransportFailure, has_more_response_bytes,
};

pub use signalbox_egress_transport::{PublicDestinationClientError, public_destination_client};

/// Fixed GitHub REST root.
pub const REST_BASE_URL: &str = "https://api.github.com/";
/// Fixed GitHub GraphQL endpoint.
pub const GRAPHQL_URL: &str = "https://api.github.com/graphql";
/// Fixed authenticated GitHub origin.
pub const API_ORIGIN: &str = "https://api.github.com";
/// Version selected for GitHub REST requests.
pub const API_VERSION: &str = "2026-03-10";
/// JSON representation used unless a tool requests a GitHub-specific media type.
pub const DEFAULT_ACCEPT: &str = "application/vnd.github+json";

/// Builds the same closed HTTP client used by the shared egress transport.
pub fn client(timeout: Option<Duration>) -> Result<Client, ReqwestWebFetchConstructionError> {
    signalbox_egress_transport::build_web_fetch_client(timeout, None)
}

/// Whether a URL belongs to the fixed credential-bearing GitHub origin.
pub fn admits_api_origin(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("api.github.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
}

/// Builds a sensitive bearer header without exposing invalid credential bytes.
pub fn authorization(
    credential: &[u8],
) -> Result<HeaderValue, reqwest::header::InvalidHeaderValue> {
    let mut bytes = Vec::with_capacity(7 + credential.len());
    bytes.extend_from_slice(b"Bearer ");
    bytes.extend_from_slice(credential);
    let mut value = HeaderValue::from_bytes(&bytes)?;
    value.set_sensitive(true);
    Ok(value)
}

/// Builds one authenticated JSON request; credentials never become client defaults.
pub fn authenticated_request(
    client: &Client,
    method: Method,
    url: Url,
    authorization: HeaderValue,
    body: Option<Vec<u8>>,
) -> RequestBuilder {
    let request = client
        .request(method, url)
        .header(AUTHORIZATION, authorization)
        .header(ACCEPT, DEFAULT_ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .header(USER_AGENT, "signalbox");
    match body {
        Some(body) => request.header(CONTENT_TYPE, "application/json").body(body),
        None => request,
    }
}

/// HTTP evidence before a suite applies its read or mutation outcome mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusClass {
    Informational,
    Success,
    Redirect,
    ClientError,
    ServerError,
    Other,
}

/// Classifies status without inspecting provider error prose.
pub const fn classify_status(status: u16) -> StatusClass {
    match status {
        100..=199 => StatusClass::Informational,
        200..=299 => StatusClass::Success,
        300..=399 => StatusClass::Redirect,
        400..=499 => StatusClass::ClientError,
        500..=599 => StatusClass::ServerError,
        _ => StatusClass::Other,
    }
}

/// Whether a completed status proves a non-server-error response.
pub const fn status_is_definitive(status: u16) -> bool {
    status < StatusCode::INTERNAL_SERVER_ERROR.as_u16()
}

/// Whether GitHub advertises another page; page traversal stays with each tool.
pub fn has_next_page(headers: &HeaderMap) -> bool {
    headers
        .get(LINK)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|link| link.contains("rel=\"next\"")))
}

/// Whether the stream ended within its admitted byte count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseExtent {
    Complete,
    Truncated,
}

/// Retains at most `limit` bytes and distinguishes exact-cap EOF from truncation.
pub async fn read_bounded<S, B, E>(
    mut stream: S,
    limit: usize,
) -> Result<(Vec<u8>, ResponseExtent), WebFetchTransportFailure>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| WebFetchTransportFailure::DispatchUnknown)?;
        let chunk = chunk.as_ref();
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, ResponseExtent::Truncated));
        }
        body.extend_from_slice(chunk);
        if body.len() == limit {
            let extent = if has_more_response_bytes(&mut stream).await? {
                ResponseExtent::Truncated
            } else {
                ResponseExtent::Complete
            };
            return Ok((body, extent));
        }
    }
    Ok((body, ResponseExtent::Complete))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests assert successful fixture construction"
)]
mod tests {
    use super::*;

    #[test]
    fn invalid_bearer_bytes_cannot_inject_a_header() {
        assert!(authorization(b"fixture\r\nX-Injected: yes").is_err());
    }

    #[test]
    fn bearer_credentials_are_request_scoped_and_sensitive() {
        let client = client(None).expect("fixture client constructs");
        let url = Url::parse(GRAPHQL_URL).expect("fixed endpoint parses");
        let request = authenticated_request(
            &client,
            Method::POST,
            url.clone(),
            authorization(b"synthetic-test-token").expect("fixture bearer is valid"),
            Some(b"{}".to_vec()),
        )
        .build()
        .expect("request builds without dispatch");
        assert!(request.headers()[AUTHORIZATION].is_sensitive());
        assert_eq!(
            request.headers()[AUTHORIZATION],
            "Bearer synthetic-test-token"
        );
        assert_eq!(request.headers()[CONTENT_TYPE], "application/json");
        assert!(
            client
                .get(url)
                .build()
                .expect("plain request builds")
                .headers()
                .get(AUTHORIZATION)
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_exact_cap_with_empty_trailing_frames_is_complete() {
        let stream = futures_util::stream::iter([Ok::<_, ()>(&b"abc"[..]), Ok(&b""[..])]);
        assert_eq!(
            read_bounded(stream, 3).await,
            Ok((b"abc".to_vec(), ResponseExtent::Complete))
        );
    }

    #[tokio::test]
    async fn a_nonempty_frame_after_the_cap_is_truncated() {
        let stream = futures_util::stream::iter([Ok::<_, ()>(&b"abc"[..]), Ok(&b"d"[..])]);
        assert_eq!(
            read_bounded(stream, 3).await,
            Ok((b"abc".to_vec(), ResponseExtent::Truncated))
        );
    }

    #[test]
    fn server_failure_is_not_definitive_rejection_evidence() {
        assert_eq!(classify_status(503), StatusClass::ServerError);
        assert!(!status_is_definitive(503));
        assert_eq!(classify_status(429), StatusClass::ClientError);
        assert!(status_is_definitive(429));
    }
}
