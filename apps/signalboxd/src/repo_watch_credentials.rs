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

    /// Resolves the credential and returns only an authenticated client handle.
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
            .map(|client| with_app_authentication(client, self.credentials.github_app()))
            .map_err(RepositoryWatchClientLoadError::from_construction)
    }
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
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || token.is_empty()
        || token.contains(['\r', '\n', '\0'])
    {
        return Err(unavailable);
    }
    url.set_username("x-access-token")
        .map_err(|_| unavailable)?;
    url.set_password(Some(token)).map_err(|_| unavailable)?;
    Ok(url.into())
}

pub(crate) fn with_app_authentication(
    client: GitHubClient,
    app: Option<std::sync::Arc<signalbox_github_transport::AppAuthentication>>,
) -> GitHubClient {
    let sender: Option<signalbox_module_repo_watch_v2::github::AuthenticatedRequestSender> = app
        .map(|app| {
            let send: signalbox_module_repo_watch_v2::github::AuthenticatedRequestSender =
                std::sync::Arc::new(move |request, path| {
                    let app = app.clone();
                    Box::pin(async move {
                        app.send(request, None)
                            .await
                            .map_err(|failure| match failure {
                                signalbox_github_transport::AppRequestFailure::Credential(_) => {
                                    GitHubClientError::InvalidCredential
                                }
                                signalbox_github_transport::AppRequestFailure::Request(source) => {
                                    GitHubClientError::Request {
                                        path,
                                        status: None,
                                        source,
                                    }
                                }
                            })
                    })
                });
            send
        });
    client.with_request_sender(sender)
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
