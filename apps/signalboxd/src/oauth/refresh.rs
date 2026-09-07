use super::*;
use signalbox_model_runtime::{CancellationSignal, CredentialValue};

pub(super) enum RefreshFailure {
    CancelledBeforeSend,
    NonRotating,
    Ambiguous,
    Rejected,
    IdentityChanged,
}

pub(super) struct Refreshed {
    pub authorization: OauthAuthorization,
    pub access_token: CredentialValue,
    pub expires_at: Option<Instant>,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    error: Option<String>,
}

impl OauthClient {
    pub(super) async fn refresh(
        &self,
        registration: &OauthRegistration,
        stored: &OauthAuthorization,
        cancellation: &mut CancellationSignal,
    ) -> Result<Refreshed, RefreshFailure> {
        use RefreshFailure::*;
        if cancellation.is_cancelled() {
            return Err(CancelledBeforeSend);
        }
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": &stored.refresh_token,
            "client_id": &registration.client_id,
        });
        let attempt = async {
            let response = self
                .client
                .post(&registration.refresh_token_url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|error| {
                    if error.is_connect() {
                        NonRotating
                    } else {
                        Ambiguous
                    }
                })?;
            let status = response.status();
            if status.is_redirection() {
                return Err(Ambiguous);
            }
            let bytes = response.bytes().await.map_err(|_| Ambiguous)?;
            let response: RefreshResponse =
                serde_json::from_slice(&bytes).map_err(|_| Ambiguous)?;
            match response.error.as_deref() {
                Some(
                    "invalid_grant"
                    | "refresh_token_expired"
                    | "refresh_token_reused"
                    | "refresh_token_invalidated",
                ) => return Err(Rejected),
                // RFC 6749 section 5.2 rejects these requests before token issuance.
                Some(
                    "invalid_request"
                    | "invalid_client"
                    | "unauthorized_client"
                    | "unsupported_grant_type"
                    | "invalid_scope",
                ) if status.is_client_error() => return Err(NonRotating),
                Some(_) => return Err(Ambiguous),
                None => {}
            }
            if !status.is_success() {
                return Err(Ambiguous);
            }
            let valid = |value: &String| !value.is_empty() && !value.contains('\0');
            let access_token = response.access_token.filter(valid).ok_or(Ambiguous)?;
            let mut authorization = stored.clone();
            if let Some(refresh_token) = response.refresh_token {
                if !valid(&refresh_token) {
                    return Err(Ambiguous);
                }
                authorization.refresh_token = refresh_token;
            }
            if let Some(identity_token) = response.id_token {
                if !valid(&identity_token) {
                    return Err(IdentityChanged);
                }
                let account_identity = identity(&identity_token).map_err(|_| IdentityChanged)?;
                if account_identity != stored.account_identity {
                    return Err(IdentityChanged);
                }
                authorization.identity_token = identity_token;
            }
            let expires_at = access_token_expiry(&access_token);
            Ok(Refreshed {
                authorization,
                access_token: CredentialValue::new(access_token.into_bytes()),
                expires_at,
            })
        };
        cancellation
            .run_until_cancelled(attempt)
            .await
            .unwrap_or(Err(Ambiguous))
    }
}

// The JWT claim supplies cache timing; it does not authenticate the token.
fn access_token_expiry(token: &str) -> Option<Instant> {
    #[derive(Deserialize)]
    struct Claims {
        exp: i64,
    }

    let mut parts = token.split('.');
    let (header, payload, signature) = (parts.next()?, parts.next()?, parts.next()?);
    if header.is_empty() || payload.is_empty() || signature.is_empty() || parts.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Claims = serde_json::from_slice(&bytes).ok()?;
    let deadline = std::time::UNIX_EPOCH.checked_add(Duration::from_secs(
        u64::try_from(claims.exp).unwrap_or_default(),
    ))?;
    let now = Instant::now();
    let remaining = deadline
        .duration_since(std::time::SystemTime::now())
        .unwrap_or_default();
    now.checked_add(remaining)
}
