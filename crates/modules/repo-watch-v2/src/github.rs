//! Narrow GitHub transport retained by the repository-watch module.

use std::{error::Error, fmt};

use reqwest::{
    Client, StatusCode,
    header::{
        ACCEPT, AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue, IF_MODIFIED_SINCE,
        IF_NONE_MATCH, LAST_MODIFIED, LINK, USER_AGENT,
    },
};

const API_ROOT: &str = "https://api.github.com";

/// Deployment-owned authenticated request dispatch; the path supplies safe error context.
pub type AuthenticatedRequestSender = std::sync::Arc<
    dyn Fn(
            reqwest::RequestBuilder,
            String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<reqwest::Response, GitHubClientError>>
                    + Send,
            >,
        > + Send
        + Sync,
>;

/// GitHub HTTP client owned by repository-watch external I/O.
#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
    request_sender: Option<AuthenticatedRequestSender>,
}

impl GitHubClient {
    /// Builds a client that retains its sensitive authorization header for its lifetime.
    pub fn try_new(user_agent: &str, token: &str) -> Result<Self, GitHubClientError> {
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| GitHubClientError::InvalidCredential)?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        Self::build(user_agent, headers, None)
    }

    /// Builds a client whose sender resolves authentication only at request dispatch.
    pub fn try_with_request_sender(
        user_agent: &str,
        sender: AuthenticatedRequestSender,
    ) -> Result<Self, GitHubClientError> {
        Self::build(user_agent, HeaderMap::new(), Some(sender))
    }

    fn build(
        user_agent: &str,
        mut headers: HeaderMap,
        request_sender: Option<AuthenticatedRequestSender>,
    ) -> Result<Self, GitHubClientError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(user_agent).map_err(|_| GitHubClientError::InvalidUserAgent)?,
        );
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .map_err(GitHubClientError::Build)?;
        Ok(Self {
            client,
            request_sender,
        })
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        path: &str,
    ) -> Result<reqwest::Response, GitHubClientError> {
        match &self.request_sender {
            Some(send) => send(request, path.to_owned()).await,
            None => request
                .send()
                .await
                .map_err(|source| GitHubClientError::Request {
                    path: path.to_owned(),
                    status: None,
                    source,
                }),
        }
    }

    /// Returns the authenticated principal's remaining REST requests.
    pub async fn rest_quota(&self) -> Result<Option<u64>, GitHubClientError> {
        let path = "/rate_limit";
        let response = self
            .send(self.client.get(format!("{API_ROOT}{path}")), path)
            .await?;
        if !response.status().is_success() {
            return Err(GitHubClientError::Rejected {
                path: path.to_owned(),
                status: response.status(),
            });
        }
        Ok(rest_remaining(response.headers()))
    }

    /// Fetches one API-relative resource as exact response bytes.
    pub async fn get(&self, path: &str) -> Result<Vec<u8>, GitHubClientError> {
        self.get_page(path).await.map(|(body, _)| body)
    }

    /// Fetches one page and reports GitHub's next-page link without following it.
    pub async fn get_page(&self, path: &str) -> Result<(Vec<u8>, bool), GitHubClientError> {
        match self.conditional_page(path, None).await? {
            ConditionalPage::Modified { body, has_next, .. } => Ok((body, has_next)),
            ConditionalPage::Unchanged => Err(GitHubClientError::Rejected {
                path: path.to_owned(),
                status: StatusCode::NOT_MODIFIED,
            }),
        }
    }

    /// Sends retained validators for one accepted resource snapshot.
    pub async fn conditional_page(
        &self,
        path: &str,
        validators: Option<&HttpValidators>,
    ) -> Result<ConditionalPage, GitHubClientError> {
        validate_path(path)?;
        let mut request = self.client.get(format!("{API_ROOT}{path}"));
        if let Some(validators) = validators {
            if let Some(etag) = &validators.etag {
                request = request.header(IF_NONE_MATCH, etag);
            }
            if let Some(modified) = &validators.last_modified {
                request = request.header(IF_MODIFIED_SINCE, modified);
            }
        }
        let response = self.send(request, path).await?;
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(ConditionalPage::Unchanged);
        }
        if !status.is_success() {
            return Err(GitHubClientError::Rejected {
                path: path.to_owned(),
                status,
            });
        }
        let has_next = response.headers().get_all(LINK).iter().any(|value| {
            value.to_str().is_ok_and(|links| {
                links
                    .split(',')
                    .any(|link| link.split(';').any(|part| part.trim() == "rel=\"next\""))
            })
        });
        let validators = HttpValidators {
            etag: response
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            last_modified: response
                .headers()
                .get(LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        };
        response
            .bytes()
            .await
            .map(|body| ConditionalPage::Modified {
                body: body.to_vec(),
                has_next,
                validators,
            })
            .map_err(|source| GitHubClientError::Request {
                path: path.to_owned(),
                status: Some(status),
                source,
            })
    }

    /// Submits the module's encoded GraphQL observation query to GitHub.
    pub async fn graphql(&self, body: Vec<u8>) -> Result<Vec<u8>, GitHubClientError> {
        let path = "/graphql";
        let request = self
            .client
            .post(format!("{API_ROOT}/graphql"))
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        let response = self.send(request, "/graphql").await?;
        let status = response.status();
        if !status.is_success() {
            return Err(GitHubClientError::Rejected {
                path: path.to_owned(),
                status,
            });
        }
        response
            .bytes()
            .await
            .map(|body| body.to_vec())
            .map_err(|source| GitHubClientError::Request {
                path: path.to_owned(),
                status: Some(status),
                source,
            })
    }
}

fn validate_path(path: &str) -> Result<(), GitHubClientError> {
    if path.starts_with('/') && !path.starts_with("//") {
        Ok(())
    } else {
        Err(GitHubClientError::InvalidPath)
    }
}

/// Closed failures from repository-watch's GitHub transport boundary.
#[derive(Debug)]
pub enum GitHubClientError {
    /// The credential cannot form an HTTP authorization value.
    InvalidCredential,
    /// The user-agent cannot form an HTTP header value.
    InvalidUserAgent,
    /// Only one API-relative path is accepted.
    InvalidPath,
    /// The HTTP client could not be built.
    Build(reqwest::Error),
    /// The request or response body transport failed.
    Request {
        path: String,
        status: Option<StatusCode>,
        source: reqwest::Error,
    },
    /// GitHub returned a non-success status.
    Rejected { path: String, status: StatusCode },
}

impl fmt::Display for GitHubClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request {
                path,
                status,
                source,
            } => write!(
                formatter,
                "GitHub request {path} (HTTP {status:?}) failed: {source}"
            ),
            Self::Rejected { path, status } => {
                write!(formatter, "GitHub request {path} rejected: HTTP {status}")
            }
            Self::InvalidCredential => {
                formatter.write_str("GitHub credential is not an admitted HTTP header")
            }
            Self::InvalidUserAgent => {
                formatter.write_str("GitHub user-agent is not an admitted HTTP header")
            }
            Self::InvalidPath => {
                formatter.write_str("GitHub API path is not relative to the configured origin")
            }
            Self::Build(error) => {
                write!(formatter, "GitHub HTTP client construction failed: {error}")
            }
        }
    }
}

impl Error for GitHubClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Build(error) | Self::Request { source: error, .. } => Some(error),
            Self::InvalidCredential
            | Self::InvalidUserAgent
            | Self::InvalidPath
            | Self::Rejected { .. } => None,
        }
    }
}

fn rest_remaining(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("x-ratelimit-remaining")?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{GitHubClientError, validate_path};

    #[test]
    fn rest_budget_uses_authenticated_response_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-ratelimit-limit",
            reqwest::header::HeaderValue::from_static("15000"),
        );
        headers.insert(
            "x-ratelimit-remaining",
            reqwest::header::HeaderValue::from_static("14987"),
        );
        headers.insert(
            "x-ratelimit-resource",
            reqwest::header::HeaderValue::from_static("core"),
        );
        assert_eq!(super::rest_remaining(&headers), Some(14987));
    }

    #[test]
    fn rejected_request_reports_path_page_and_status() {
        let path = "/repos/example/project/actions/runs?per_page=100&page=2";
        let error = GitHubClientError::Rejected {
            path: path.to_owned(),
            status: reqwest::StatusCode::FORBIDDEN,
        };
        assert_eq!(
            error.to_string(),
            format!("GitHub request {path} rejected: HTTP 403 Forbidden")
        );
        assert!(format!("{error:?}").contains(path));
    }

    #[test]
    fn github_client_rejects_an_origin_replacing_path() {
        assert!(matches!(
            validate_path("//attacker.invalid/resource"),
            Err(GitHubClientError::InvalidPath)
        ));
    }
}

/// HTTP validators retained with an accepted page; neither value is a credential.
#[derive(Clone, Debug, Default)]
pub struct HttpValidators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// A conditional GET either supplies a replacement or reuses the accepted page.
pub enum ConditionalPage {
    Modified {
        body: Vec<u8>,
        has_next: bool,
        validators: HttpValidators,
    },
    Unchanged,
}
