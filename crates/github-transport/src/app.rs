use std::{fmt, future::Future, pin::Pin, sync::Arc};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::{RequestBuilder, Response, StatusCode, header::HeaderValue};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use tokio::sync::Mutex;

/// Sanitized reasons an installation credential cannot be supplied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppCredentialFailure {
    /// The private key could not be read or decoded.
    KeyUnreadable,
    /// GitHub could not find the configured installation for this App.
    NotInstalled,
    /// GitHub did not return a usable installation token.
    ExchangeRejected,
}

/// Deployment-owned private-key reader, called only when a token needs minting.
pub type AppKeyReader = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<Vec<u8>, AppCredentialFailure>> + Send>>
        + Send
        + Sync,
>;

type TokenExchange = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<CachedToken, AppCredentialFailure>> + Send>>
        + Send
        + Sync,
>;

struct CachedToken {
    authorization: HeaderValue,
    expires_at: i64,
}

struct ResolvedAuthorization {
    header: HeaderValue,
    generation: u64,
}

/// Shared installation-token cache; diagnostics contain no key or token material.
pub struct AppAuthentication {
    exchange: TokenExchange,
    cached: Mutex<Result<Option<CachedToken>, AppCredentialFailure>>,
    generation: std::sync::atomic::AtomicU64,
}

impl fmt::Debug for AppAuthentication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppAuthentication([REDACTED])")
    }
}

impl AppAuthentication {
    /// Binds one App installation without reading its private key or sending HTTP.
    pub fn new(app_id: u64, installation_id: u64, read_key: AppKeyReader) -> Self {
        let exchange: TokenExchange = Arc::new(move || {
            let read_key = read_key.clone();
            Box::pin(async move {
                let key = read_key().await?;
                let jwt = jwt(&key, app_id, jiff::Timestamp::now().as_second())?;
                let url = reqwest::Url::parse(&format!(
                    "{}app/installations/{installation_id}/access_tokens",
                    crate::REST_BASE_URL
                ))
                .map_err(|_| AppCredentialFailure::ExchangeRejected)?;
                let client =
                    crate::client(None).map_err(|_| AppCredentialFailure::ExchangeRejected)?;
                let authorization = crate::authorization(jwt.as_bytes())
                    .map_err(|_| AppCredentialFailure::KeyUnreadable)?;
                let response = crate::authenticated_request(
                    &client,
                    reqwest::Method::POST,
                    url,
                    authorization,
                    None,
                )
                .send()
                .await
                .map_err(|_| AppCredentialFailure::ExchangeRejected)?;
                let status = response.status();
                admit_exchange_status(status)?;
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|_| AppCredentialFailure::ExchangeRejected)?;
                decode_exchange(&bytes)
            })
        });
        Self {
            exchange,
            cached: Mutex::new(Ok(None)),
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Resolves a current sensitive bearer header, coalescing concurrent refreshes.
    pub async fn authorization(&self) -> Result<HeaderValue, AppCredentialFailure> {
        self.resolve(None)
            .await
            .map(|authorization| authorization.header)
    }

    async fn resolve(
        &self,
        rejected: Option<u64>,
    ) -> Result<ResolvedAuthorization, AppCredentialFailure> {
        let observed_generation = self.generation.load(std::sync::atomic::Ordering::SeqCst);
        let mut cached = self.cached.lock().await;
        if self.generation.load(std::sync::atomic::Ordering::SeqCst) != observed_generation
            && let Err(failure) = *cached
        {
            return Err(failure);
        }
        // GitHub recommends backdating JWT issuance by 60 seconds for clock skew:
        // https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app
        // The same skew allowance keeps installation tokens out of their expiry window.
        if let Ok(Some(token)) = cached.as_ref()
            && token.expires_at > jiff::Timestamp::now().as_second() + 60
            && rejected != Some(self.generation.load(std::sync::atomic::Ordering::SeqCst))
        {
            return Ok(ResolvedAuthorization {
                header: token.authorization.clone(),
                generation: self.generation.load(std::sync::atomic::Ordering::SeqCst),
            });
        }
        *cached = Ok(None);
        let result = (self.exchange)().await;
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match result {
            Ok(token) => {
                let authorization = token.authorization.clone();
                *cached = Ok(Some(token));
                Ok(ResolvedAuthorization {
                    header: authorization,
                    generation: self.generation.load(std::sync::atomic::Ordering::SeqCst),
                })
            }
            Err(failure) => {
                *cached = Err(failure);
                Err(failure)
            }
        }
    }

    /// Sends one request and retries it once after an authentication rejection.
    /// A concurrent refresh for the rejected token is reused.
    pub async fn send(&self, request: RequestBuilder) -> Result<Response, AppRequestFailure> {
        self.send_with(request, |request| async move { request.send().await })
            .await
    }

    async fn send_with<F, Fut>(
        &self,
        request: RequestBuilder,
        send: F,
    ) -> Result<Response, AppRequestFailure>
    where
        F: Fn(RequestBuilder) -> Fut,
        Fut: Future<Output = Result<Response, reqwest::Error>>,
    {
        let retry = request.try_clone().ok_or(AppRequestFailure::Credential(
            AppCredentialFailure::ExchangeRejected,
        ))?;
        let authorization = self
            .resolve(None)
            .await
            .map_err(AppRequestFailure::Credential)?;
        let mut response = send(request.headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::AUTHORIZATION,
            authorization.header.clone(),
        )])))
        .await
        .map_err(AppRequestFailure::Request)?;
        if response.status() != StatusCode::UNAUTHORIZED {
            response
                .extensions_mut()
                .insert(ResponseCredential(authorization.header));
            return Ok(response);
        }
        let authorization = self
            .resolve(Some(authorization.generation))
            .await
            .map_err(AppRequestFailure::Credential)?;
        let mut response = send(retry.headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::AUTHORIZATION,
            authorization.header.clone(),
        )])))
        .await
        .map_err(AppRequestFailure::Request)?;
        response
            .extensions_mut()
            .insert(ResponseCredential(authorization.header));
        Ok(response)
    }
}

/// Authentication preparation or request transport failure.
#[derive(Debug)]
pub enum AppRequestFailure {
    /// No usable installation credential was available.
    Credential(AppCredentialFailure),
    /// The authenticated request failed in transport.
    Request(reqwest::Error),
}

fn decode_exchange(bytes: &[u8]) -> Result<CachedToken, AppCredentialFailure> {
    let rejected = AppCredentialFailure::ExchangeRejected;
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| rejected)?;
    let token = value["token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or(rejected)?;
    let expires_at = value["expires_at"]
        .as_str()
        .ok_or(rejected)?
        .parse::<jiff::Timestamp>()
        .map_err(|_| rejected)?
        .as_second();
    if expires_at <= jiff::Timestamp::now().as_second() + 60 {
        return Err(rejected);
    }
    Ok(CachedToken {
        authorization: crate::authorization(token.as_bytes()).map_err(|_| rejected)?,
        expires_at,
    })
}

fn jwt(pem: &[u8], app_id: u64, now: i64) -> Result<String, AppCredentialFailure> {
    let failure = AppCredentialFailure::KeyUnreadable;
    let pem = std::str::from_utf8(pem).map_err(|_| failure)?;
    let encoded: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(encoded).map_err(|_| failure)?;
    let key = RsaKeyPair::from_der(&der)
        .or_else(|_| RsaKeyPair::from_pkcs8(&der))
        .map_err(|_| failure)?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    // GitHub permits expiry at most ten minutes in the future and recommends
    // issuance sixty seconds in the past (GitHub JWT documentation above).
    let claims = serde_json::json!({"iss": app_id.to_string(), "iat": now - 60, "exp": now + 600});
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{header}.{payload}");
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| failure)?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn admit_exchange_status(status: StatusCode) -> Result<(), AppCredentialFailure> {
    match status {
        StatusCode::CREATED => Ok(()),
        StatusCode::NOT_FOUND => Err(AppCredentialFailure::NotInstalled),
        _ => Err(AppCredentialFailure::ExchangeRejected),
    }
}

#[derive(Clone)]
struct ResponseCredential(HeaderValue);

/// Request-scoped installation token used for this response, for exact-value redaction.
/// The token is carried only in local extensions, never in response headers.
pub fn response_credential(response: &Response) -> Option<&[u8]> {
    response
        .extensions()
        .get::<ResponseCredential>()
        .map(|credential| &credential.0.as_bytes()[b"Bearer ".len()..])
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert fixture construction")]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const APP_ID: u64 = 42; // Arbitrary App identity for signed-claim verification.
    const NOW: i64 = 1_800_000_000; // Fixed clock for exact claim assertions.
    const TOKEN_RESPONSE: &str = include_str!("../tests/fixtures/installation-token.json");

    fn fixture_auth() -> (AppAuthentication, Arc<AtomicUsize>) {
        let exchanges = Arc::new(AtomicUsize::new(0));
        let count = exchanges.clone();
        let exchange: TokenExchange = Arc::new(move || {
            let generation = count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::task::yield_now().await;
                let mut response: serde_json::Value =
                    serde_json::from_str(TOKEN_RESPONSE).expect("recorded response shape");
                response["expires_at"] =
                    jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + 3600)
                        .expect("fixture expiry")
                        .to_string()
                        .into();
                response["token"] = format!("synthetic-installation-{generation}").into();
                decode_exchange(response.to_string().as_bytes())
            })
        });
        (
            AppAuthentication {
                exchange,
                cached: Mutex::new(Ok(None)),
                generation: std::sync::atomic::AtomicU64::new(0),
            },
            exchanges,
        )
    }

    #[test]
    fn jwt_is_rs256_signed_and_carries_skewed_app_claims() {
        let output = std::process::Command::new("openssl")
            .args(["genrsa", "2048"])
            .output()
            .expect("generate test-only RSA key");
        assert!(output.status.success());
        let signed = jwt(&output.stdout, APP_ID, NOW).expect("generated key signs JWT");
        let parts: Vec<_> = signed.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).expect("header base64"))
                .expect("header JSON");
        assert_eq!(header, serde_json::json!({"alg":"RS256", "typ":"JWT"}));
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("claims base64"))
                .expect("claims JSON");
        assert_eq!(
            claims,
            serde_json::json!({"iss":APP_ID.to_string(), "iat":NOW - 60, "exp":NOW + 600})
        );
        let pem = std::str::from_utf8(&output.stdout).expect("generated PEM");
        let der = STANDARD
            .decode(
                pem.lines()
                    .filter(|line| !line.starts_with("-----"))
                    .collect::<String>(),
            )
            .expect("generated DER");
        let key = RsaKeyPair::from_pkcs8(&der)
            .or_else(|_| RsaKeyPair::from_der(&der))
            .expect("generated RSA");
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            key.public().as_ref(),
        )
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).expect("signature base64"),
        )
        .expect("JWT signature verifies against generated public key");
    }

    #[tokio::test]
    async fn concurrent_callers_share_initial_and_preexpiry_refresh() {
        let (auth, exchanges) = fixture_auth();
        let (first, second) = tokio::join!(auth.authorization(), auth.authorization());
        assert_eq!(first.expect("first token"), second.expect("shared token"));
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        auth.cached
            .lock()
            .await
            .as_mut()
            .expect("successful cache")
            .as_mut()
            .expect("cached token")
            .expires_at = jiff::Timestamp::now().as_second() + 60;
        let (first, second) = tokio::join!(auth.authorization(), auth.authorization());
        assert_eq!(
            first.expect("refreshed token"),
            second.expect("shared refreshed token")
        );
        assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_unauthorized_requests_share_one_refresh_and_retry_once() {
        let (auth, exchanges) = fixture_auth();
        let requests = std::sync::Mutex::new(Vec::new());
        let rejected_requests = tokio::sync::Barrier::new(2);
        let send = |request: RequestBuilder| {
            let request = request.build().expect("fixture request");
            assert_eq!(
                request
                    .headers()
                    .get_all(reqwest::header::AUTHORIZATION)
                    .iter()
                    .count(),
                1
            );
            assert_eq!(request.method(), reqwest::Method::POST);
            assert_eq!(
                request.body().and_then(reqwest::Body::as_bytes),
                Some(b"synthetic-request-body".as_slice())
            );
            let token = request.headers()[reqwest::header::AUTHORIZATION].clone();
            requests
                .lock()
                .expect("record requests")
                .push(token.clone());
            let rejected_requests = &rejected_requests;
            async move {
                let status = if token == "Bearer synthetic-installation-0" {
                    rejected_requests.wait().await;
                    401
                } else {
                    200
                };
                Ok(Response::from(
                    http::Response::builder()
                        .status(status)
                        .body("")
                        .expect("recorded response"),
                ))
            }
        };
        let client = crate::client(None).expect("offline client");
        let (first, second) = tokio::join!(
            auth.send_with(
                client
                    .post(crate::GRAPHQL_URL)
                    .header(reqwest::header::AUTHORIZATION, "Bearer stale-token")
                    .body("synthetic-request-body"),
                &send
            ),
            auth.send_with(
                client
                    .post(crate::GRAPHQL_URL)
                    .header(reqwest::header::AUTHORIZATION, "Bearer stale-token")
                    .body("synthetic-request-body"),
                &send
            )
        );
        assert_eq!(first.expect("retry succeeds").status(), StatusCode::OK);
        assert_eq!(
            second.expect("concurrent retry succeeds").status(),
            StatusCode::OK
        );
        assert_eq!(exchanges.load(Ordering::SeqCst), 2);
        let requests = requests.lock().expect("recorded requests");
        assert_eq!(
            requests
                .iter()
                .filter(|token| *token == "Bearer synthetic-installation-1")
                .count(),
            2
        );
        assert_eq!(requests.len(), 4);
    }

    #[tokio::test]
    async fn a_second_unauthorized_response_is_returned_without_another_retry() {
        let (auth, exchanges) = fixture_auth();
        let requests = AtomicUsize::new(0);
        let client = crate::client(None).expect("offline client");
        let response = auth
            .send_with(client.get(crate::GRAPHQL_URL), |_| {
                requests.fetch_add(1, Ordering::SeqCst);
                async {
                    Ok(Response::from(
                        http::Response::builder()
                            .status(401)
                            .body("")
                            .expect("recorded rejection"),
                    ))
                }
            })
            .await
            .expect("definitive response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(exchanges.load(Ordering::SeqCst), 2);
        assert_eq!(
            response_credential(&response),
            Some(b"synthetic-installation-1".as_slice())
        );
    }

    #[tokio::test]
    async fn concurrent_exchange_failures_are_shared_and_later_calls_can_retry() {
        let exchanges = Arc::new(AtomicUsize::new(0));
        let count = exchanges.clone();
        let auth = AppAuthentication {
            cached: Mutex::new(Ok(None)),
            generation: std::sync::atomic::AtomicU64::new(0),
            exchange: Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    tokio::task::yield_now().await;
                    Err(AppCredentialFailure::ExchangeRejected)
                })
            }),
        };
        let (first, second) = tokio::join!(auth.authorization(), auth.authorization());
        assert_eq!(first, Err(AppCredentialFailure::ExchangeRejected));
        assert_eq!(second, first);
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(auth.authorization().await, first);
        assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rejection_refresh_is_shared_even_when_the_exchange_returns_identical_bytes() {
        let exchanges = Arc::new(AtomicUsize::new(0));
        let count = exchanges.clone();
        let auth = AppAuthentication {
            cached: Mutex::new(Ok(None)),
            generation: std::sync::atomic::AtomicU64::new(0),
            exchange: Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    tokio::task::yield_now().await;
                    Ok(CachedToken {
                        authorization: crate::authorization(b"synthetic-unchanged-token")
                            .expect("fixture header"),
                        expires_at: jiff::Timestamp::now().as_second() + 3600,
                    })
                })
            }),
        };
        let rejected = auth.resolve(None).await.expect("initial credential");
        let (first, second) = tokio::join!(
            auth.resolve(Some(rejected.generation)),
            auth.resolve(Some(rejected.generation))
        );
        assert_eq!(first.expect("refreshed").header, rejected.header);
        assert_eq!(second.expect("shared refresh").header, rejected.header);
        assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn missing_installation_and_rejected_exchange_are_distinct() {
        assert_eq!(
            admit_exchange_status(StatusCode::NOT_FOUND),
            Err(AppCredentialFailure::NotInstalled)
        );
        assert_eq!(
            admit_exchange_status(StatusCode::UNAUTHORIZED),
            Err(AppCredentialFailure::ExchangeRejected)
        );
    }
}
