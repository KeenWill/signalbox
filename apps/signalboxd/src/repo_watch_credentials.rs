//! Repository-scoped credentials resolved at the external-I/O boundary.

use std::{error::Error, fmt};

use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_module_repo_watch_v2::github::{GitHubClient, GitHubClientError};

use crate::{FileCredentialAccess, WatchedRepositoryConfiguration};

/// Resolves only the credential assigned to one configured repository.
#[derive(Clone, Debug)]
pub struct RepositoryWatchClientLoader {
    credentials: FileCredentialAccess,
    reference: CredentialReference,
}

impl RepositoryWatchClientLoader {
    /// Supplies Git's authentication only through one invocation's environment.
    pub(crate) async fn git_authorization(&self) -> Result<String, RepositoryWatchClientLoadError> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let credential = self
            .credentials
            .resolve(&self.reference)
            .await
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        let token = std::str::from_utf8(credential.expose_bytes())
            .map_err(|_| RepositoryWatchClientLoadError::CredentialUnavailable)?;
        if token.is_empty() || token.contains(['\r', '\n', '\0']) {
            return Err(RepositoryWatchClientLoadError::CredentialUnavailable);
        }
        Ok(format!(
            "Authorization: Basic {}",
            STANDARD.encode(format!("x-access-token:{token}"))
        ))
    }

    /// Binds a repository's file reference without reading its credential.
    pub fn new(repository: &WatchedRepositoryConfiguration) -> Self {
        let reference = repository.credential_reference();
        Self {
            credentials: FileCredentialAccess::new(
                repository.credential_file().to_path_buf(),
                reference.clone(),
            ),
            reference,
        }
    }

    /// Rereads the credential and returns only an authenticated client handle.
    pub async fn load(&self) -> Result<GitHubClient, RepositoryWatchClientLoadError> {
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

#[cfg(test)]
mod tests {
    use super::{
        FileCredentialAccess, GitHubClientError, RepositoryWatchClientLoadError,
        RepositoryWatchClientLoader,
    };
    use signalbox_model_runtime::CredentialReference;

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
