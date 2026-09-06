//! Repository-scoped credentials resolved at the external-I/O boundary.

use std::{error::Error, fmt};

use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_module_repo_watch_v2::github::GitHubClient;

use crate::{FileCredentialAccess, WatchedRepositoryConfiguration};

/// Resolves only the credential assigned to one configured repository.
#[derive(Clone, Debug)]
pub struct RepositoryWatchClientLoader {
    credentials: FileCredentialAccess,
    reference: CredentialReference,
}

impl RepositoryWatchClientLoader {
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
    pub async fn load(&self) -> Result<GitHubClient, RepositoryWatchCredentialError> {
        let credential = self
            .credentials
            .resolve(&self.reference)
            .await
            .map_err(|_| RepositoryWatchCredentialError)?;
        let token = std::str::from_utf8(credential.expose_bytes())
            .map_err(|_| RepositoryWatchCredentialError)?;
        if token.is_empty() {
            return Err(RepositoryWatchCredentialError);
        }
        GitHubClient::try_new("signalbox-repository-watch", token)
            .map_err(|_| RepositoryWatchCredentialError)
    }
}

/// Credential resolution or authenticated-client construction failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryWatchCredentialError;

impl fmt::Display for RepositoryWatchCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("repository-watch client credential unavailable")
    }
}

impl Error for RepositoryWatchCredentialError {}

#[cfg(test)]
mod tests {
    use super::{FileCredentialAccess, RepositoryWatchClientLoader};
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

    use std::error::Error;
}
