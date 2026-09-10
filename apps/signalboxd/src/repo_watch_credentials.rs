//! Repository-scoped credentials resolved at the external-I/O boundary.

use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

use std::{error::Error, fmt};

use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_module_repo_watch_v2::github::{GitHubClient, GitHubClientError};

use crate::{FileCredentialAccess, WatchedRepositoryConfiguration};

/// Non-secret reference for the repository-scoped push transport.
pub(crate) const GIT_PUSH_CREDENTIAL_REFERENCE: &str = "repository-watch-git-push";

/// Bounds observation credential lookups and requests, matching the push timeout.
const OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Resolves only the credential assigned to one configured repository.
#[derive(Clone, Debug)]
pub struct RepositoryWatchClientLoader {
    credentials: FileCredentialAccess,
    reference: CredentialReference,
}

impl RepositoryWatchClientLoader {
    /// Supplies Git's authentication and retains the App generation for rejection feedback.
    pub(crate) async fn git_authorization(
        &self,
        timeout: std::time::Duration,
    ) -> Result<GitCheckoutAuthentication, RepositoryWatchClientLoadError> {
        if let Some(app) = self.credentials.github_app() {
            let token = app
                .token(Some(timeout))
                .await
                .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
            return Ok(GitCheckoutAuthentication {
                authorization: git_authorization_header(token.credential_bytes())?,
                token: Some(token),
            });
        }
        let credential = self
            .credentials
            .resolve(&self.reference)
            .await
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        Ok(GitCheckoutAuthentication {
            authorization: git_authorization_header(credential.expose_bytes())?,
            token: None,
        })
    }

    pub(crate) async fn refresh_git_authorization(
        &self,
        authentication: &mut GitCheckoutAuthentication,
        timeout: std::time::Duration,
    ) -> Result<bool, RepositoryWatchClientLoadError> {
        let (Some(app), Some(token)) =
            (self.credentials.github_app(), authentication.token.as_ref())
        else {
            return Ok(false);
        };
        let refreshed = app
            .refresh(token, Some(timeout))
            .await
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        authentication.authorization = git_authorization_header(refreshed.credential_bytes())?;
        authentication.token = Some(refreshed);
        Ok(true)
    }

    pub(crate) async fn authenticated_push_url(
        &self,
        remote: &str,
        timeout: std::time::Duration,
    ) -> Result<GitPushAuthentication, RepositoryWatchClientLoadError> {
        if let Some(app) = self.credentials.github_app() {
            return GitPushAuthentication::from_app(&app, remote, timeout).await;
        }
        let credential = self
            .credentials
            .resolve(&self.reference)
            .await
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        Ok(GitPushAuthentication {
            url: authenticated_push_url(remote, credential.expose_bytes())?,
            token: None,
        })
    }

    pub(crate) async fn refreshed_push_url(
        &self,
        remote: &str,
        rejected: &GitPushAuthentication,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, RepositoryWatchClientLoadError> {
        let (Some(app), Some(token)) = (self.credentials.github_app(), rejected.token.as_ref())
        else {
            return Ok(None);
        };
        let refreshed = app
            .refresh(token, Some(timeout))
            .await
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        authenticated_push_url(remote, refreshed.credential_bytes()).map(Some)
    }

    /// Binds a repository's configured delivery without reading credentials.
    pub fn new(repository: &WatchedRepositoryConfiguration) -> Self {
        let reference = repository.credential_reference();
        Self {
            credentials: FileCredentialAccess::from_github(
                repository.credential(),
                reference.clone(),
            ),
            reference,
        }
    }

    pub(crate) fn for_git_push(path: PathBuf) -> Self {
        let reference = CredentialReference::new(GIT_PUSH_CREDENTIAL_REFERENCE);
        Self {
            credentials: FileCredentialAccess::new(path, reference.clone()),
            reference,
        }
    }

    pub(crate) fn for_repository_push(repository: &WatchedRepositoryConfiguration) -> Self {
        match repository.push_credential_file() {
            Some(path) => Self::for_git_push(path.to_path_buf()),
            None => Self::new(repository),
        }
    }

    /// Reads file credentials or defers App authentication to request dispatch.
    pub async fn load(&self) -> Result<GitHubClient, RepositoryWatchClientLoadError> {
        if let Some(app) = self.credentials.github_app() {
            return app_observation_client("signalbox-repository-watch", app)
                .map_err(RepositoryWatchClientLoadError::from_construction);
        }
        let credential = self
            .credentials
            .resolve(&self.reference)
            .await
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        let token = std::str::from_utf8(credential.expose_bytes())
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        if token.is_empty() {
            return Err(RepositoryWatchClientLoadError::CredentialUnavailable);
        }
        GitHubClient::try_new("signalbox-repository-watch", token)
            .map_err(RepositoryWatchClientLoadError::from_construction)
    }
}

pub(crate) struct GitCheckoutAuthentication {
    pub(crate) authorization: String,
    token: Option<signalbox_github_transport::AppToken>,
}

fn git_authorization_header(token: &[u8]) -> Result<String, RepositoryWatchClientLoadError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let token = std::str::from_utf8(token)
        .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
    if token.is_empty() || token.contains(['\r', '\n', '\0']) {
        return Err(RepositoryWatchClientLoadError::CredentialUnavailable);
    }
    Ok(format!(
        "Authorization: Basic {}",
        STANDARD.encode(format!("x-access-token:{token}"))
    ))
}

pub(crate) fn git_authentication_rejected(result: &signalbox_tools_exec::ProcessRunResult) -> bool {
    if !matches!(result.outcome, signalbox_tools_exec::ProcessOutcome::Exited { code: Some(code) } if code != 0)
    {
        return false;
    }
    let error = String::from_utf8_lossy(&result.stderr.bytes).to_ascii_lowercase();
    error.contains("fatal: authentication failed")
        || error.contains("requested url returned error: 401")
}

pub(crate) struct GitPushAuthentication {
    pub(crate) url: String,
    token: Option<signalbox_github_transport::AppToken>,
}

impl GitPushAuthentication {
    async fn from_app(
        app: &signalbox_github_transport::AppAuthentication,
        remote: &str,
        timeout: std::time::Duration,
    ) -> Result<Self, RepositoryWatchClientLoadError> {
        if url::Url::parse(remote)
            .ok()
            .is_none_or(|url| url.scheme() != "https" || url.host_str() != Some("github.com"))
        {
            return Err(RepositoryWatchClientLoadError::CredentialUnavailable);
        }
        let token = app
            .token(Some(timeout))
            .await
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        Ok(Self {
            url: authenticated_push_url(remote, token.credential_bytes())?,
            token: Some(token),
        })
    }
}

fn authenticated_push_url(
    remote: &str,
    token: &[u8],
) -> Result<String, RepositoryWatchClientLoadError> {
    let unavailable = RepositoryWatchClientLoadError::CredentialUnavailable;
    let token = std::str::from_utf8(token).map_err(|_| unavailable)?;
    let mut url = url::Url::parse(remote).map_err(|_| unavailable)?;
    if url.scheme() != "https" || token.is_empty() || token.contains(['\r', '\n', '\0']) {
        return Err(unavailable);
    }
    url.set_username("x-access-token")
        .map_err(|_| unavailable)?;
    url.set_password(Some(token)).map_err(|_| unavailable)?;
    Ok(url.into())
}

pub(crate) fn app_observation_client(
    user_agent: &str,
    app: std::sync::Arc<signalbox_github_transport::AppAuthentication>,
) -> Result<GitHubClient, GitHubClientError> {
    GitHubClient::try_with_request_sender(
        user_agent,
        std::sync::Arc::new(move |request, path| {
            let app = app.clone();
            Box::pin(async move {
                let response =
                    app.send(request, Some(OBSERVATION_TIMEOUT))
                        .await
                        .map_err(|failure| match failure {
                            signalbox_github_transport::AppRequestFailure::Credential(_) => {
                                GitHubClientError::InvalidCredential
                            }
                            signalbox_github_transport::AppRequestFailure::Request(source) => {
                                GitHubClientError::Request {
                                    path: path.clone(),
                                    status: None,
                                    source,
                                }
                            }
                        })?;
                let credential = signalbox_github_transport::response_credential(&response)
                    .ok_or(GitHubClientError::InvalidCredential)?
                    .to_vec();
                scrub_app_response(response, &path, &credential).await
            })
        }),
    )
}

async fn scrub_app_response(
    response: reqwest::Response,
    path: &str,
    credential: &[u8],
) -> Result<reqwest::Response, GitHubClientError> {
    let status = response.status();
    if !status.is_success() {
        return Ok(response);
    }
    let token = std::str::from_utf8(credential)
        .ok()
        .filter(|token| !token.is_empty())
        .ok_or(GitHubClientError::InvalidCredential)?;
    let encoded = serde_json::Value::String(token.to_owned()).to_string();
    let escaped = &encoded[1..encoded.len() - 1];
    let mut sanitized = http::Response::new(String::new());
    *sanitized.status_mut() = status;
    *sanitized.version_mut() = response.version();
    *sanitized.headers_mut() = response.headers().clone();
    for header in sanitized.headers_mut().values_mut() {
        if let Ok(text) = header.to_str() {
            *header =
                reqwest::header::HeaderValue::from_str(&redact_app_token(text, token, escaped))
                    .map_err(|_| GitHubClientError::InvalidCredential)?;
        }
    }
    let mut value: serde_json::Value =
        response
            .json()
            .await
            .map_err(|source| GitHubClientError::Request {
                path: path.to_owned(),
                status: Some(status),
                source,
            })?;
    scrub_app_json(&mut value, token, escaped);
    *sanitized.body_mut() = value.to_string();
    sanitized
        .headers_mut()
        .remove(reqwest::header::CONTENT_LENGTH);
    Ok(sanitized.into())
}

fn redact_app_token(text: &str, token: &str, escaped: &str) -> String {
    text.replace(escaped, "[redacted]")
        .replace(token, "[redacted]")
}

pub(crate) fn scrub_app_json(value: &mut serde_json::Value, token: &str, escaped: &str) {
    match value {
        serde_json::Value::String(text) => *text = redact_app_token(text, token, escaped),
        serde_json::Value::Array(values) => {
            for value in values {
                scrub_app_json(value, token, escaped);
            }
        }
        serde_json::Value::Object(object) => {
            for (key, mut value) in std::mem::take(object) {
                scrub_app_json(&mut value, token, escaped);
                object.insert(redact_app_token(&key, token, escaped), value);
            }
        }
        _ => {}
    }
}

impl signalbox_module_repo_watch_v2::provider::RepositoryClientLoader
    for RepositoryWatchClientLoader
{
    type Error = RepositoryWatchClientLoadError;

    async fn load_client(&self) -> Result<GitHubClient, Self::Error> {
        self.load().await
    }
}

/// Credential resolution or authenticated-client construction failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryWatchClientLoadError {
    /// The repository's credential could not be read or admitted as authorization.
    CredentialUnavailable,
    /// The authenticated HTTP client could not be constructed.
    ClientConstructionFailed,
}

impl RepositoryWatchClientLoadError {
    fn from_construction(error: GitHubClientError) -> Self {
        match error {
            GitHubClientError::InvalidCredential => Self::CredentialUnavailable,
            _ => Self::ClientConstructionFailed,
        }
    }
}

impl fmt::Display for RepositoryWatchClientLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CredentialUnavailable => "repository-watch client credential unavailable",
            Self::ClientConstructionFailed => "repository-watch HTTP client construction failed",
        })
    }
}

impl Error for RepositoryWatchClientLoadError {}

/// Compares credential references using normalized paths, symlinks, and file identity.
pub fn credential_files_conflict(left: &Path, right: &Path) -> bool {
    let left = resolved_file_reference(left);
    let right = resolved_file_reference(right);
    left == right || same_file_identity(&left, &right)
}

#[cfg(unix)]
fn same_file_identity(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &Path, _right: &Path) -> bool {
    false
}

fn resolved_file_reference(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut resolved = normalize_file_reference(&absolute);
    for _ in 0..40 {
        let mut prefix = PathBuf::new();
        let mut components = resolved.components();
        let mut replacement = None;
        while let Some(component) = components.next() {
            prefix.push(component.as_os_str());
            let Ok(metadata) = fs::symlink_metadata(&prefix) else {
                return resolved;
            };
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let Ok(target) = fs::read_link(&prefix) else {
                return resolved;
            };
            let mut target = if target.is_absolute() {
                target
            } else {
                prefix
                    .parent()
                    .map_or(target.clone(), |parent| parent.join(target))
            };
            target.extend(components.map(|remaining| remaining.as_os_str()));
            replacement = Some(normalize_file_reference(&target));
            break;
        }
        let Some(replacement) = replacement else {
            return fs::canonicalize(&resolved).unwrap_or(resolved);
        };
        resolved = replacement;
    }
    resolved
}

fn normalize_file_reference(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !path.is_absolute() {
                    normalized.push(component.as_os_str());
                }
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{
        FileCredentialAccess, GitHubClientError, RepositoryWatchClientLoadError,
        RepositoryWatchClientLoader,
    };
    use signalbox_model_runtime::CredentialReference;

    // The response credential contains characters with a distinct JSON-escaped spelling.
    const RESPONSE_TOKEN: &str = "refreshed\"installation\\token";
    const OBSERVATION_PATH: &str = "/repos/fixture/project/pulls";

    fn observation_client(body: serde_json::Value) -> super::GitHubClient {
        super::GitHubClient::try_with_request_sender(
            "repository-watch-fixture",
            std::sync::Arc::new(move |_request, path| {
                let body = body.to_string();
                Box::pin(async move {
                    let response = http::Response::builder()
                        .status(200)
                        .header("etag", format!("revision-{RESPONSE_TOKEN}"))
                        .header("last-modified", "Wed, 09 Sep 2026 12:00:00 GMT")
                        .header("link", "<https://api.github.com/next>; rel=\"next\"")
                        .header("x-ratelimit-remaining", "14987")
                        .header("content-length", body.len())
                        .body(body)
                        .expect("fixture response");
                    super::scrub_app_response(response.into(), &path, RESPONSE_TOKEN.as_bytes())
                        .await
                })
            }),
        )
        .expect("fixture client")
    }

    #[tokio::test]
    async fn rest_observations_scrub_the_response_token_before_cache_ingestion() {
        let encoded = serde_json::Value::String(RESPONSE_TOKEN.to_owned()).to_string();
        let escaped = &encoded[1..encoded.len() - 1];
        let client = observation_client(serde_json::json!([{
            "title": format!("title {RESPONSE_TOKEN}"),
            "body": format!("escaped {escaped}"),
        }]));
        let page = client
            .conditional_page(OBSERVATION_PATH, None)
            .await
            .expect("sanitized page");
        let signalbox_module_repo_watch_v2::github::ConditionalPage::Modified {
            body,
            validators,
            has_next,
        } = page
        else {
            panic!("the successful response supplies a replacement snapshot");
        };
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid cached JSON");
        assert_eq!(
            value,
            serde_json::json!([{
                "title": "title [redacted]", "body": "escaped [redacted]",
            }])
        );
        assert_eq!(validators.etag.as_deref(), Some("revision-[redacted]"));
        assert_eq!(
            validators.last_modified.as_deref(),
            Some("Wed, 09 Sep 2026 12:00:00 GMT")
        );
        assert!(has_next);
        assert_eq!(
            client.rest_quota().await.expect("App quota header"),
            Some(14987)
        );
    }

    #[tokio::test]
    async fn rest_observations_scrub_response_token_keys_before_cache_ingestion() {
        let client = observation_client(serde_json::json!([{
            "title": "Pull request",
            (format!("extra {RESPONSE_TOKEN}")): [{
                r#"nested refreshed\"installation\\token"#: RESPONSE_TOKEN,
                "ordinary": "retained",
            }],
        }]));
        let page = client
            .conditional_page(OBSERVATION_PATH, None)
            .await
            .expect("sanitized page");
        let signalbox_module_repo_watch_v2::github::ConditionalPage::Modified { body, .. } = page
        else {
            panic!("the successful response supplies a replacement snapshot");
        };
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid cached JSON");
        assert_eq!(
            value,
            serde_json::json!([{
                "title": "Pull request",
                "extra [redacted]": [{
                    "nested [redacted]": "[redacted]",
                    "ordinary": "retained",
                }],
            }])
        );
    }

    #[tokio::test]
    async fn graphql_observations_scrub_the_response_token_before_pr_state_ingestion() {
        let client = observation_client(serde_json::json!({
            "data": {"repository": {"pullRequests": {"nodes": [{
                "title": RESPONSE_TOKEN, "body": format!("body {RESPONSE_TOKEN}"),
            }]}}},
        }));
        let body = client
            .graphql(b"{}".to_vec())
            .await
            .expect("sanitized GraphQL response");
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("valid observation JSON");
        assert_eq!(
            value,
            serde_json::json!({
                "data": {"repository": {"pullRequests": {"nodes": [{
                    "title": "[redacted]", "body": "body [redacted]",
                }]}}},
            })
        );
    }

    #[tokio::test]
    async fn malformed_app_observations_fail_before_ingestion() {
        let response = http::Response::new(format!("not JSON: {RESPONSE_TOKEN}"));
        let failure =
            super::scrub_app_response(response.into(), OBSERVATION_PATH, RESPONSE_TOKEN.as_bytes())
                .await
                .expect_err("malformed observations cannot reach persistence");
        assert!(matches!(
            failure,
            GitHubClientError::Request {
                status: Some(reqwest::StatusCode::OK),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unchanged_app_pages_do_not_require_a_json_body() {
        let response = http::Response::builder().status(304).body("").unwrap();
        let response =
            super::scrub_app_response(response.into(), OBSERVATION_PATH, RESPONSE_TOKEN.as_bytes())
                .await
                .expect("unchanged responses retain their status");
        assert_eq!(response.status(), reqwest::StatusCode::NOT_MODIFIED);
    }

    #[tokio::test(start_paused = true)]
    async fn app_observations_expire_while_credentials_are_stalled() {
        // Arbitrary App identities; the pending key reader prevents any network request.
        let app = signalbox_github_transport::AppAuthentication::new(
            42,
            73,
            std::sync::Arc::new(|| Box::pin(std::future::pending())),
        );
        let client =
            super::app_observation_client("observation-timeout-fixture", std::sync::Arc::new(app))
                .expect("construction does not wait for the App key");
        let started = tokio::time::Instant::now();
        let (rest, graphql) = tokio::time::timeout(std::time::Duration::from_secs(301), async {
            tokio::join!(
                client.conditional_page(OBSERVATION_PATH, None),
                client.graphql(b"{}".to_vec()),
            )
        })
        .await
        .expect("both the exchange and its concurrent cache waiter must expire");
        assert!(matches!(rest, Err(GitHubClientError::InvalidCredential)));
        assert!(matches!(graphql, Err(GitHubClientError::InvalidCredential)));
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(300));
    }

    #[tokio::test]
    async fn app_client_loading_defers_credential_resolution_until_observation_dispatch() {
        let directory = tempfile::tempdir().expect("isolated credential fixture");
        let missing_key = directory.path().join("missing-app-key.pem");
        // Arbitrary App identities; no fixture key exists, so no network request is possible.
        let source = format!(
            "[[credential_profiles]]\nname = \"fixture-app\"\nadapter = \"github\"\ndelivery = \"github_app\"\napp_id = 42\ninstallation_id = 73\nprivate_key_file = {}\n",
            serde_json::to_string(missing_key.to_str().unwrap()).unwrap(),
        );
        let document = source
            .parse::<toml_edit::DocumentMut>()
            .expect("fixture profile");
        let profiles = crate::credential_pools::parse_github_credential_profiles(
            document.get("credential_profiles"),
        )
        .expect("profile parsing does not read the key");
        let reference = CredentialReference::new("fixture-app");
        let loader = RepositoryWatchClientLoader {
            credentials: FileCredentialAccess::from_github(
                &profiles["fixture-app"],
                reference.clone(),
            ),
            reference,
        };
        let client = loader
            .load()
            .await
            .expect("client loading does not resolve App credentials");
        assert!(matches!(
            client.get(OBSERVATION_PATH).await,
            Err(GitHubClientError::InvalidCredential),
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn push_app_lookup_expires_at_the_supplied_push_budget() {
        // Arbitrary App identities; the pending key reader prevents any network request.
        let app = signalbox_github_transport::AppAuthentication::new(
            42,
            73,
            std::sync::Arc::new(|| Box::pin(std::future::pending())),
        );
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(301),
            super::GitPushAuthentication::from_app(
                &app,
                "https://github.com/fixture/project.git",
                std::time::Duration::from_secs(300),
            ),
        )
        .await
        .expect("lookup must finish within the push budget");
        assert_eq!(
            result.err(),
            Some(RepositoryWatchClientLoadError::CredentialUnavailable)
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(300));
    }

    #[tokio::test]
    async fn push_url_carries_x_access_token_authentication() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("test token directory");
        let path = directory.path().join("token");
        std::fs::write(&path, b"synthetic-installation-token").expect("test token file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("private test token");
        let loader = RepositoryWatchClientLoader::for_git_push(path);
        let url = loader
            .authenticated_push_url(
                "https://github.com/fixture/project.git",
                std::time::Duration::from_secs(300),
            )
            .await
            .expect("authenticated URL");
        assert_eq!(
            url.url,
            "https://x-access-token:synthetic-installation-token@github.com/fixture/project.git"
        );
        let custom = loader
            .authenticated_push_url(
                "https://git.example.test/fixture/project.git",
                std::time::Duration::from_secs(300),
            )
            .await
            .expect("configured HTTPS destination accepts its token file");
        assert_eq!(
            custom.url,
            "https://x-access-token:synthetic-installation-token@git.example.test/fixture/project.git"
        );
        assert_eq!(
            loader
                .refreshed_push_url(
                    "https://github.com/fixture/project.git",
                    &url,
                    std::time::Duration::from_secs(300)
                )
                .await
                .expect("file delivery needs no App exchange"),
            None
        );
    }

    #[tokio::test]
    async fn client_loading_observes_file_rotation_and_never_falls_back() {
        let directory = tempfile::tempdir().expect("credential directory");
        let path = directory.path().join("repository-token");
        let reference = CredentialReference::new("repository-watch:example/project");
        let loader = RepositoryWatchClientLoader {
            credentials: FileCredentialAccess::new(path.clone(), reference.clone()),
            reference,
        };
        assert!(loader.load().await.is_err());
        std::fs::write(&path, "fixture-token\r\n").expect("write terminated token");
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("private credential fixture");
        assert!(loader.load().await.is_ok());
        std::fs::write(&path, "invalid\nheader").expect("rotate to invalid token");
        let error = loader.load().await.err().expect("invalid token rejected");
        assert_eq!(error, RepositoryWatchClientLoadError::CredentialUnavailable);
        assert_eq!(
            error.to_string(),
            "repository-watch client credential unavailable"
        );
        assert!(error.source().is_none());
        std::fs::write(&path, "\r\n").expect("rotate to empty token");
        assert!(loader.load().await.is_err());
        std::fs::write(&path, "replacement-token").expect("rotate to usable token");
        assert!(loader.load().await.is_ok());
    }

    #[test]
    fn client_construction_failure_keeps_its_class_without_exposing_transport_details() {
        let construction_error = reqwest::Client::builder()
            .user_agent("invalid\nheader")
            .build()
            .expect_err("invalid user-agent produces a real client construction error");
        let error = RepositoryWatchClientLoadError::from_construction(GitHubClientError::Build(
            construction_error,
        ));

        assert_eq!(
            error,
            RepositoryWatchClientLoadError::ClientConstructionFailed
        );
        assert_eq!(
            error.to_string(),
            "repository-watch HTTP client construction failed"
        );
        assert!(error.source().is_none());
    }

    use std::error::Error;
}
