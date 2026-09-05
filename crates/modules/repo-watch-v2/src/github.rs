//! Narrow GitHub transport retained by the repository-watch module.

use std::{error::Error, fmt};

use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
};

const API_ROOT: &str = "https://api.github.com";

/// GitHub HTTP client owned by repository-watch external I/O.
#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
}

impl GitHubClient {
    /// Builds an authenticated client without exposing its credential to module state.
    pub fn try_new(user_agent: &str, token: &str) -> Result<Self, GitHubClientError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| GitHubClientError::InvalidCredential)?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
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
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .map_err(GitHubClientError::Build)?;
        Ok(Self { client })
    }

    /// Fetches one API-relative resource as exact response bytes.
    pub async fn get(&self, path: &str) -> Result<Vec<u8>, GitHubClientError> {
        validate_path(path)?;
        let response = self
            .client
            .get(format!("{API_ROOT}{path}"))
            .send()
            .await
            .map_err(GitHubClientError::Request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(GitHubClientError::Rejected(status));
        }
        response
            .bytes()
            .await
            .map(|body| body.to_vec())
            .map_err(GitHubClientError::Request)
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
    Request(reqwest::Error),
    /// GitHub returned a non-success status.
    Rejected(StatusCode),
}

impl fmt::Display for GitHubClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCredential => "GitHub credential is not an admitted HTTP header",
            Self::InvalidUserAgent => "GitHub user-agent is not an admitted HTTP header",
            Self::InvalidPath => "GitHub API path is not relative to the configured origin",
            Self::Build(_) => "GitHub HTTP client construction failed",
            Self::Request(_) => "GitHub HTTP request failed",
            Self::Rejected(_) => "GitHub rejected the HTTP request",
        })
    }
}

impl Error for GitHubClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Build(error) | Self::Request(error) => Some(error),
            Self::InvalidCredential
            | Self::InvalidUserAgent
            | Self::InvalidPath
            | Self::Rejected(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GitHubClientError, validate_path};

    #[test]
    fn github_client_rejects_an_origin_replacing_path() {
        assert!(matches!(
            validate_path("//attacker.invalid/resource"),
            Err(GitHubClientError::InvalidPath)
        ));
    }
}
