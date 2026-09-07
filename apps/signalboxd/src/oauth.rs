//! Daemon-owned OAuth exchanges (docs/spec/configuration-and-credentials.md).

use base64::Engine;
use serde::Deserialize;
use signalbox_persistence::oauth_credential::{
    OauthAuthorization, OauthCredentialFailure as Failure, OauthProgress, OauthRegistration,
};
use std::time::Duration;
use tokio::time::Instant;

mod refresh;
mod service;
pub use service::OauthCredentialService;

pub(crate) struct DeviceAuthorization {
    pub progress: OauthProgress,
    device_code: String,
    deadline: Instant,
    interval: Duration,
}

pub(crate) struct OauthClient {
    client: reqwest::Client,
}

impl OauthClient {
    pub fn new() -> Result<Self, Failure> {
        Self::from_builder(reqwest::Client::builder())
    }

    fn from_builder(builder: reqwest::ClientBuilder) -> Result<Self, Failure> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = builder
            .no_proxy()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| Failure::DeviceEndpointFailed)?;
        Ok(Self { client })
    }

    pub async fn authorize(
        &self,
        registration: &OauthRegistration,
    ) -> Result<DeviceAuthorization, Failure> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &registration.client_id)
            .append_pair("scope", &registration.scopes.join(" "))
            .finish();
        let response = self
            .client
            .post(&registration.device_authorization_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| Failure::DeviceEndpointFailed)?;
        if !response.status().is_success() {
            return Err(Failure::DeviceEndpointRejected);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| Failure::DeviceEndpointFailed)?;
        let device: DeviceResponse =
            serde_json::from_slice(&bytes).map_err(|_| Failure::DeviceEndpointRejected)?;
        if device.device_code.is_empty()
            || device.device_code.contains('\0')
            || device.expires_in == 0
        {
            return Err(Failure::DeviceEndpointRejected);
        }
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(device.expires_in))
            .ok_or(Failure::DeviceEndpointRejected)?;
        Ok(DeviceAuthorization {
            progress: OauthProgress {
                user_code: device.user_code,
                verification_uri: device.verification_uri,
            },
            device_code: device.device_code,
            deadline,
            interval: Duration::from_secs(device.interval.unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)),
        })
    }

    pub async fn poll(
        &self,
        registration: &OauthRegistration,
        mut device: DeviceAuthorization,
    ) -> Result<OauthAuthorization, Failure> {
        loop {
            let next = Instant::now()
                .checked_add(device.interval)
                .ok_or(Failure::PollingExpired)?;
            if next >= device.deadline {
                return Err(Failure::PollingExpired);
            }
            tokio::time::sleep_until(next).await;
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
                .append_pair("device_code", &device.device_code)
                .append_pair("client_id", &registration.client_id)
                .finish();
            let attempt = async {
                let response = self
                    .client
                    .post(&registration.token_url)
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(body)
                    .send()
                    .await
                    .map_err(|_| Failure::TokenEndpointFailed)?;
                let status = response.status();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|_| Failure::TokenEndpointFailed)?;
                let response: TokenResponse =
                    serde_json::from_slice(&bytes).map_err(|_| Failure::TokenEndpointFailed)?;
                Ok::<_, Failure>((status, response))
            };
            let (status, response) = tokio::time::timeout_at(device.deadline, attempt)
                .await
                .map_err(|_| Failure::PollingExpired)??;
            if status.is_redirection() {
                return Err(Failure::TokenEndpointFailed);
            }
            match response.error.as_deref() {
                Some("authorization_pending") => continue,
                Some("slow_down") => {
                    device.interval = device
                        .interval
                        .checked_add(Duration::from_secs(SLOW_DOWN_SECONDS))
                        .ok_or(Failure::PollingExpired)?;
                    continue;
                }
                Some("access_denied") => return Err(Failure::AccessDenied),
                Some("expired_token") => return Err(Failure::PollingExpired),
                Some(_) => return Err(Failure::TokenEndpointFailed),
                None => {}
            }
            if !status.is_success() {
                return Err(Failure::TokenEndpointFailed);
            }
            let identity_token = response
                .id_token
                .filter(|token| !token.is_empty() && !token.contains('\0'))
                .ok_or(Failure::TokenResponseWithoutIdentity)?;
            let account_identity = identity(&identity_token)?;
            let refresh_token = response
                .refresh_token
                .filter(|token| !token.is_empty() && !token.contains('\0'))
                .ok_or(Failure::TokenEndpointFailed)?;
            return Ok(OauthAuthorization {
                refresh_token,
                identity_token,
                account_identity,
            });
        }
    }
}

// RFC 8628 sections 3.2 and 3.5 define the default and slow_down increment.
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const SLOW_DOWN_SECONDS: u64 = 5;

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    error: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

pub(crate) fn identity(token: &str) -> Result<serde_json::Value, Failure> {
    let mut parts = token.split('.');
    let _header = parts.next().ok_or(Failure::TokenEndpointFailed)?;
    let payload = parts.next().ok_or(Failure::TokenEndpointFailed)?;
    if parts.next().is_none() || parts.next().is_some() {
        return Err(Failure::TokenEndpointFailed);
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Failure::TokenEndpointFailed)?;
    let claims: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| Failure::TokenEndpointFailed)?;
    let subject = claims
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .filter(|subject| !subject.is_empty())
        .ok_or(Failure::TokenEndpointFailed)?;
    let auth = &claims["https://api.openai.com/auth"];
    Ok(serde_json::json!({
        "subject": subject,
        "account_id": auth["chatgpt_account_id"],
        "user_id": auth["chatgpt_user_id"],
    }))
}

#[cfg(test)]
mod tests;
