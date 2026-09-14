//! Checked anonymous repository acquisition.

use std::{error::Error, fmt, path::Path, process::Stdio};

use signalbox_runner_wire::{
    Recovery, SandboxProfile, WorkspaceProvision, WorkspaceReady, clone_url_digest,
};
use tokio::{io::AsyncReadExt as _, process::Command};

use crate::RunnerConfiguration;

use super::{
    PrepareRepositoryWorkspaceError, PreparedWorkspace, RepositoryWorkspaceRequest,
    RunnerWorkspaceError, RunnerWorkspaceStore,
};

/// Sanitized refusal or failure of one workspace acquisition.
#[derive(Debug)]
pub enum WorkspaceProvisionError {
    /// The requested repository is not configured or cannot be acquired.
    RepositoryUnavailable,
    /// The requested credential profile is unavailable for acquisition.
    CredentialUnavailable,
    /// The requested sandbox profile is unavailable.
    SandboxUnavailable,
    /// Durable workspace facts conflict with the operation.
    ManifestConflict,
    /// The private filesystem cannot publish or authenticate the workspace.
    Storage,
}

impl fmt::Display for WorkspaceProvisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RepositoryUnavailable => "runner repository_unavailable",
            Self::CredentialUnavailable => "runner credential_unavailable",
            Self::SandboxUnavailable => "runner sandbox_unavailable",
            Self::ManifestConflict => "runner manifest_conflict",
            Self::Storage => "runner workspace storage unavailable",
        })
    }
}

impl Error for WorkspaceProvisionError {}

impl From<RunnerWorkspaceError> for WorkspaceProvisionError {
    fn from(error: RunnerWorkspaceError) -> Self {
        match error {
            RunnerWorkspaceError::ManifestConflict => Self::ManifestConflict,
            _ => Self::Storage,
        }
    }
}

/// Local configuration resolved before accepting the operation into the journal.
pub(crate) struct CheckedProvision {
    operation: WorkspaceProvision,
    clone_url: String,
}

impl CheckedProvision {
    #[cfg(test)]
    pub(crate) fn with_local_clone_fixture(mut self, path: String) -> Self {
        self.clone_url = path;
        self
    }

    pub(crate) fn check(
        configuration: &RunnerConfiguration,
        operation: WorkspaceProvision,
    ) -> Result<Self, WorkspaceProvisionError> {
        let correlation = &operation.correlation;
        if correlation.sandbox_profile != SandboxProfile::Ambient
            || !configuration
                .advertisement()
                .sandbox_profiles
                .contains(&correlation.sandbox_profile)
        {
            return Err(WorkspaceProvisionError::SandboxUnavailable);
        }
        if correlation
            .credential_profile
            .as_ref()
            .is_some_and(|profile| configuration.credential(profile).is_none())
        {
            return Err(WorkspaceProvisionError::CredentialUnavailable);
        }
        let clone_url = match &correlation.repository {
            Some(key) => {
                let repository = configuration
                    .repository(key)
                    .ok_or(WorkspaceProvisionError::RepositoryUnavailable)?;
                if repository.credential_profile() != correlation.credential_profile.as_ref() {
                    return Err(WorkspaceProvisionError::CredentialUnavailable);
                }
                // Credential resolution and the confined Git helper are separate
                // from the ambient acquisition producer.
                if correlation.credential_profile.is_some() {
                    return Err(WorkspaceProvisionError::CredentialUnavailable);
                }
                repository.clone_url().to_owned()
            }
            None => return Err(WorkspaceProvisionError::RepositoryUnavailable),
        };
        Ok(Self {
            operation,
            clone_url,
        })
    }

    pub(crate) async fn prepare(
        self,
        store: RunnerWorkspaceStore,
    ) -> Result<WorkspaceReady, WorkspaceProvisionError> {
        let correlation = self.operation.correlation;
        let prepared = match (&correlation.repository, self.clone_url) {
            (Some(repository), clone_url) => {
                let request = RepositoryWorkspaceRequest::new(
                    correlation.session_id,
                    correlation.placement_revision,
                    correlation.runner_id,
                    repository.clone(),
                    clone_url_digest(&clone_url),
                    correlation.credential_profile.clone(),
                    correlation.sandbox_profile,
                );
                let recovery = self.operation.recovery.clone();
                store
                    .prepare_repository_workspace(&request, |target| async move {
                        clone_repository(&clone_url, target.path(), recovery.as_ref()).await
                    })
                    .await
                    .map_err(|error| match error {
                        PrepareRepositoryWorkspaceError::Storage(error) => error.into(),
                        PrepareRepositoryWorkspaceError::Preparation(error) => error,
                    })?
            }
            _ => return Err(WorkspaceProvisionError::ManifestConflict),
        };
        if self
            .operation
            .recovery
            .as_ref()
            .is_some_and(|expected| prepared.manifest.recovery.as_ref() != Some(expected))
        {
            return Err(WorkspaceProvisionError::ManifestConflict);
        }
        Ok(WorkspaceReady {
            correlation,
            working_directory: prepared.execution_directory.as_str().to_owned(),
            ready: signalbox_runner_wire::ReadyManifest {
                manifest: prepared.manifest,
                manifest_digest: prepared.manifest_digest,
            },
        })
    }
}

pub(crate) fn prepared_receipt(
    ready: &WorkspaceReady,
) -> Result<PreparedWorkspace, WorkspaceProvisionError> {
    Ok(PreparedWorkspace {
        manifest: ready.ready.manifest.clone(),
        manifest_digest: ready.ready.manifest_digest.clone(),
        execution_directory: signalbox_runner_wire::WorkingDirectory::try_new(
            ready.working_directory.clone(),
        )
        .map_err(|_| WorkspaceProvisionError::ManifestConflict)?,
    })
}

async fn clone_repository(
    clone_url: &str,
    target: &Path,
    recovery: Option<&Recovery>,
) -> Result<Recovery, WorkspaceProvisionError> {
    let target_text = target
        .to_str()
        .ok_or(WorkspaceProvisionError::RepositoryUnavailable)?;
    git_required(
        target,
        &[
            "clone",
            "--no-checkout",
            "--no-hardlinks",
            "--",
            clone_url,
            target_text,
        ],
    )
    .await?;
    let git = target.join(".git");
    if !std::fs::symlink_metadata(&git).is_ok_and(|metadata| metadata.is_dir())
        || git.join("commondir").exists()
        || git.join("objects/info/alternates").exists()
    {
        return Err(WorkspaceProvisionError::ManifestConflict);
    }
    match recovery {
        Some(Recovery::Commit { revision }) => {
            git_required(target, &["checkout", "--detach", revision]).await?;
        }
        Some(Recovery::Branch { name, revision }) => {
            git_required(target, &["checkout", "-B", name, revision]).await?;
        }
        Some(Recovery::UnbornBranch { name }) => {
            let branch = format!("refs/heads/{name}");
            git_required(target, &["update-ref", "-d", &branch]).await?;
            git_required(target, &["symbolic-ref", "HEAD", &branch]).await?;
        }
        None => {
            if git_output(target, &["rev-parse", "--verify", "HEAD"])
                .await?
                .is_some()
            {
                git_required(target, &["reset", "--hard", "HEAD"]).await?;
            }
        }
    }
    let revision = git_output(target, &["rev-parse", "--verify", "HEAD"]).await?;
    let branch = git_output(target, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    match (branch, revision) {
        (Some(name), Some(revision)) => Ok(Recovery::Branch { name, revision }),
        (None, Some(revision)) => Ok(Recovery::Commit { revision }),
        (Some(name), None) => Ok(Recovery::UnbornBranch { name }),
        (None, None) => Err(WorkspaceProvisionError::RepositoryUnavailable),
    }
}

async fn git_required(path: &Path, arguments: &[&str]) -> Result<String, WorkspaceProvisionError> {
    git_output(path, arguments)
        .await?
        .ok_or(WorkspaceProvisionError::RepositoryUnavailable)
}

async fn git_output(
    path: &Path,
    arguments: &[&str],
) -> Result<Option<String>, WorkspaceProvisionError> {
    let mut command = Command::new("git");
    command
        .args(arguments)
        .current_dir(path)
        .env_clear()
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let mut child = command
        .spawn()
        .map_err(|_| WorkspaceProvisionError::RepositoryUnavailable)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(WorkspaceProvisionError::RepositoryUnavailable)?;
    let mut output = String::new();
    stdout
        .take(signalbox_runner_wire::MAX_FRAME_BYTES as u64 + 1)
        .read_to_string(&mut output)
        .await
        .map_err(|_| WorkspaceProvisionError::RepositoryUnavailable)?;
    if output.len() > signalbox_runner_wire::MAX_FRAME_BYTES {
        return Err(WorkspaceProvisionError::RepositoryUnavailable);
    }
    let status = child
        .wait()
        .await
        .map_err(|_| WorkspaceProvisionError::RepositoryUnavailable)?;
    Ok(status
        .success()
        .then(|| output.trim_end_matches(['\r', '\n']).to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunnerStateRoot;
    use signalbox_runner_wire::{CanonicalUuid, ManifestLifecycle, PositiveU64, RepositoryKey};
    use uuid::Uuid;

    #[tokio::test]
    async fn real_git_clone_publishes_an_unborn_repository_without_shared_objects() {
        check_real_clone(false).await;
    }

    #[tokio::test]
    async fn real_git_clone_publishes_a_committed_repository_without_shared_objects() {
        check_real_clone(true).await;
    }

    async fn check_real_clone(committed: bool) {
        let directory = tempfile::tempdir().expect("temporary repository parent");
        let source = directory.path().join("source");
        std::fs::create_dir(&source).expect("source directory");
        git_required(&source, &["init", "--initial-branch=main"])
            .await
            .expect("initialize Git");
        if committed {
            std::fs::write(source.join("tracked"), b"tracked repository data")
                .expect("tracked fixture");
            git_required(&source, &["add", "tracked"])
                .await
                .expect("stage fixture");
            git_required(
                &source,
                &[
                    "-c",
                    "user.name=Runner Test",
                    "-c",
                    "user.email=runner@example.invalid",
                    "commit",
                    "-m",
                    "Fixture commit",
                ],
            )
            .await
            .expect("commit fixture");
        }
        let state = RunnerStateRoot::open(&directory.path().join("state")).expect("private state");
        let url = source.to_str().expect("fixture path").to_owned();
        let request = RepositoryWorkspaceRequest::new(
            CanonicalUuid::from_uuid(Uuid::now_v7()),
            PositiveU64::try_new(1).expect("first placement"),
            CanonicalUuid::from_uuid(Uuid::now_v7()),
            RepositoryKey::try_new("fixture".to_owned()).expect("repository key"),
            clone_url_digest(&url),
            None,
            SandboxProfile::Ambient,
        );
        let store = state.workspace_store().expect("workspace store");
        let prepared = store
            .prepare_repository_workspace(&request, |target| async move {
                clone_repository(&url, target.path(), None).await
            })
            .await
            .expect("clone publishes");
        let path = Path::new(prepared.execution_directory.as_str());
        assert!(path.join(".git").is_dir());
        assert!(!path.join(".git/commondir").exists());
        assert!(!path.join(".git/objects/info/alternates").exists());
        assert_eq!(prepared.manifest.lifecycle, ManifestLifecycle::Ready);
        if committed {
            assert!(
                matches!(prepared.manifest.recovery, Some(Recovery::Branch { ref name, .. }) if name == "main")
            );
            assert_eq!(
                std::fs::read(path.join("tracked")).expect("checked out file"),
                b"tracked repository data"
            );
        } else {
            assert_eq!(
                prepared.manifest.recovery,
                Some(Recovery::UnbornBranch {
                    name: "main".to_owned()
                })
            );
        }
        let manifest = std::fs::read_to_string(
            path.parent()
                .expect("placement")
                .join(super::super::MANIFEST_FILE),
        )
        .expect("manifest document");
        assert!(
            !manifest.contains(source.to_str().expect("source path")),
            "only the clone URL digest is recorded"
        );
        store
            .activate(&prepared)
            .expect("recorded workspace activates");
        let replay = store
            .prepare_repository_workspace(&request, |_| async {
                Err::<Recovery, _>(WorkspaceProvisionError::RepositoryUnavailable)
            })
            .await
            .expect("equal replay uses the existing clone");
        assert_eq!(replay, prepared);
    }
}
