use super::{
    CONTENT_TYPE, HOST, HTTP_DEFAULT_PORT, HeaderMap, IpAddr, JSON_CONTENT_TYPE, Next, ORIGIN,
    Request, Response, StatusCode, Url, transport_error,
};

pub(super) async fn validate_loopback_host(request: Request, next: Next) -> Response {
    if !has_loopback_host(request.headers(), request.uri()) {
        return transport_error(
            StatusCode::FORBIDDEN,
            "non_loopback_host_rejected",
            "browser requests require a loopback request authority",
        );
    }
    next.run(request).await
}

pub(super) async fn validate_blob_fetch_site(request: Request, next: Next) -> Response {
    if request.uri().path().starts_with("/api/blobs/")
        && request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|site| site == "cross-site")
    {
        return transport_error(
            StatusCode::FORBIDDEN,
            "cross_site_blob_request_rejected",
            "cross-site browser requests cannot read blobs",
        );
    }
    next.run(request).await
}

pub(super) fn has_loopback_host(headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    headers
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
        .or_else(|| uri.authority().cloned())
        .is_some_and(|authority| is_loopback_authority(&authority))
}

fn is_loopback_authority(authority: &axum::http::uri::Authority) -> bool {
    let host = normalized_authority_host(authority);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn normalized_authority_host(authority: &axum::http::uri::Authority) -> &str {
    normalized_host(authority.host())
}

fn normalized_host(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

pub(super) fn has_json_content_type(headers: &HeaderMap) -> bool {
    has_content_type(headers, JSON_CONTENT_TYPE)
}

pub(super) fn has_content_type(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .is_some_and(|value| value.essence_str() == expected)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OriginValidationError {
    Mismatch,
}

pub(super) fn validate_supplied_origin(headers: &HeaderMap) -> Result<(), OriginValidationError> {
    let Some(origin) = headers.get(ORIGIN) else {
        return Ok(());
    };
    let origin = origin
        .to_str()
        .ok()
        .and_then(|origin| Url::parse(origin).ok())
        .filter(|origin| matches!(origin.scheme(), "http" | "https"))
        .filter(|origin| origin.path() == "/")
        .filter(|origin| origin.query().is_none() && origin.fragment().is_none())
        .filter(|origin| origin.username().is_empty() && origin.password().is_none());
    let authority = headers
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok());
    let matching = origin.zip(authority).is_some_and(|(origin, authority)| {
        let authority_port = authority.port_u16().unwrap_or(HTTP_DEFAULT_PORT);
        origin.host_str().is_some_and(|host| {
            normalized_host(host).eq_ignore_ascii_case(normalized_authority_host(&authority))
        }) && origin.port_or_known_default() == Some(authority_port)
    });
    if matching {
        Ok(())
    } else {
        Err(OriginValidationError::Mismatch)
    }
}
