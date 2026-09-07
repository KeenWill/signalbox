//! Repository-scoped credentials resolved at the external-I/O boundary.

use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

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
