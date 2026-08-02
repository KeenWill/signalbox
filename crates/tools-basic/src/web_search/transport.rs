use std::{error::Error, fmt, future::Future, time::Duration};

use futures_util::StreamExt;
use reqwest::{Client, StatusCode, redirect::Policy};
use signalbox_model_runtime::CredentialValue;

use super::{brave::*, egress::*, request::*, result::*, transport_failure::*};

pub(super) const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Injectable one-request credentialed search transport.
pub trait WebSearchTransport: Send {
    /// Performs exactly one provider-pinned query with one request-scoped credential.
    fn search(
        &mut self,
        request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> impl Future<Output = WebSearchTransportOutcome> + Send;
}

/// Production reqwest transport with no ambient proxy, redirect, or retry.
#[derive(Clone, Debug)]
pub struct ReqwestWebSearchTransport {
    pub(super) client: Client,
}

impl ReqwestWebSearchTransport {
    /// Builds a provider transport with a positive whole-exchange timeout.
    pub fn try_new(exchange_timeout: Duration) -> Result<Self, ReqwestWebSearchConstructionError> {
        if exchange_timeout.is_zero() {
            return Err(ReqwestWebSearchConstructionError);
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .timeout(exchange_timeout)
            .build()
            .map_err(|_| ReqwestWebSearchConstructionError)?;
        Ok(Self { client })
    }
}

/// The fixed production web-search client could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReqwestWebSearchConstructionError;

impl fmt::Display for ReqwestWebSearchConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("web search client construction failed")
    }
}

impl Error for ReqwestWebSearchConstructionError {}

impl WebSearchTransport for ReqwestWebSearchTransport {
    async fn search(
        &mut self,
        request: WebSearchRequest,
        credential: &CredentialValue,
    ) -> WebSearchTransportOutcome {
        let outcome = async {
            let provider = request.provider();
            let http_request = build_provider_request(&self.client, &request, credential)?;
            let request_url = http_request.url().clone();
            if !(WebSearchEgressPolicy { provider }).admits(&request_url) {
                return Err(WebSearchTransportFailure::RequestFailed);
            }
            let response = self
                .client
                .execute(http_request)
                .await
                .map_err(classify_send_failure)?;
            let status = response.status();
            let body = collect_complete_body(response).await;
            finish_provider_response(provider, status, body)
        }
        .await;
        credential_safe_transport_outcome(outcome, credential)
    }
}

pub(super) async fn collect_complete_body(
    response: reqwest::Response,
) -> Result<Vec<u8>, WebSearchTransportFailure> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| WebSearchTransportFailure::DispatchUnknown)?;
        if chunk.len() > MAX_PROVIDER_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(WebSearchTransportFailure::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
        if body.len() == MAX_PROVIDER_RESPONSE_BYTES {
            while let Some(trailing) = stream.next().await {
                let trailing = trailing.map_err(|_| WebSearchTransportFailure::DispatchUnknown)?;
                if !trailing.is_empty() {
                    return Err(WebSearchTransportFailure::ResponseTooLarge);
                }
            }
            break;
        }
    }
    Ok(body)
}

pub(super) fn classify_send_failure(error: reqwest::Error) -> WebSearchTransportFailure {
    if error.is_connect() {
        WebSearchTransportFailure::RequestFailed
    } else {
        WebSearchTransportFailure::DispatchUnknown
    }
}

pub(super) fn finish_provider_response(
    provider: WebSearchProvider,
    status: StatusCode,
    body: Result<Vec<u8>, WebSearchTransportFailure>,
) -> Result<WebSearchResponse, WebSearchTransportFailure> {
    if !status.is_success() {
        let (complete_body, body_failure_class) = match body {
            Ok(complete_body) => (complete_body, None),
            Err(failure) => (Vec::new(), Some(failure.class())),
        };
        let error = WebSearchProviderError::new(status.as_u16(), complete_body)
            .ok_or(WebSearchTransportFailure::ResponseTooLarge)?;
        let error = match body_failure_class {
            Some(failure_class) => error.with_body_failure_class(failure_class),
            None => error,
        };
        return Err(WebSearchTransportFailure::ProviderRejected(error));
    }
    if status != StatusCode::OK {
        return Err(WebSearchTransportFailure::InvalidResponse);
    }
    decode_provider_response(provider, &body?)
}
