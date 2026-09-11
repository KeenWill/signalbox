//! Configured push preparation and transport execution; see git-authority-threat-model.md.

use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::Serialize;
use signalbox_application::{
    ClassifyOperatorFailure, CorrelatedToolExecutorEvidence, OperatorFailureClass,
    ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::{ToolExecutionErrorDetail, ToolResultText};
use signalbox_tools_workspace::{WorkspaceRoot, WorkspaceRootIdentity};

use crate::GIT_PUSH_CONFIGURED_NAME;
use crate::descriptor::{RepositoryIdentity, descriptor_path};
use crate::layout::validate_repository_layout;
use crate::pinning::PinnedRepository;
use crate::push_arguments::GitPushArguments;
use crate::push_catalog::decode_push;
use crate::push_objects::PushObjectSnapshot;
use crate::push_transport::{
    ConfiguredGitRemote, GitPushRequest, GitPushTransport, GitPushTransportFailure,
};
use crate::reference_read::resolve_pinned_reference_chain_from;

/// Approval-gated configured-remote push executor.
#[derive(Debug)]
pub struct GitPushExecutor<Transport> {
    root: WorkspaceRoot,
    root_path: PathBuf,
    repository_identity: RepositoryIdentity,
    repository_authority: Arc<PinnedRepository>,
    remote: ConfiguredGitRemote,
    branch_fence: Option<String>,
    commit_fence: Option<String>,
    transport: Transport,
    repository_detail: ToolExecutionErrorDetail,
    unresolved_detail: ToolExecutionErrorDetail,
    rejected_detail: ToolExecutionErrorDetail,
}

impl<Transport> GitPushExecutor<Transport> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        root: WorkspaceRoot,
        root_path: PathBuf,
        repository_identity: RepositoryIdentity,
        repository_authority: PinnedRepository,
        remote: ConfiguredGitRemote,
        transport: Transport,
        repository_detail: ToolExecutionErrorDetail,
        unresolved_detail: ToolExecutionErrorDetail,
        rejected_detail: ToolExecutionErrorDetail,
    ) -> Self {
        Self {
            root,
            root_path,
            repository_identity,
            repository_authority: Arc::new(repository_authority),
            remote,
            branch_fence: None,
            commit_fence: None,
            transport,
            repository_detail,
            unresolved_detail,
            rejected_detail,
        }
    }

    /// Bounds object capture to commits after the retained dispatch head commit.
    pub fn with_commit_fence(mut self, commit: String) -> Self {
        self.commit_fence = Some(commit);
        self
    }

    /// Restricts pushes to the retained dispatch head branch.
    pub fn with_branch_fence(mut self, branch: String) -> Self {
        self.branch_fence = Some(branch);
        self
    }
}

#[derive(signalbox_derive::OperatorError)]
#[error("Git push executor failed")]
/// Sanitized push executor failure with explicit commit certainty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitPushExecutorError {
    class: OperatorFailureClass,
}

impl ClassifyOperatorFailure for GitPushExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.class
    }
}

impl<Transport: GitPushTransport> ToolExecutor for GitPushExecutor<Transport> {
    type Error = GitPushExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        if invocation.request().name().as_str() != GIT_PUSH_CONFIGURED_NAME {
            return Err(push_caller_bug());
        }
        let arguments =
            decode_push(invocation.request().arguments()).map_err(|()| push_caller_bug())?;
        let evidence = match self.execute_push(arguments).await {
            Ok(result) => ToolExecutorEvidence::CompletedText(result),
            Err(GitPushFailure::Repository) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.repository_detail.clone()),
            },
            Err(GitPushFailure::Unresolved) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.unresolved_detail.clone()),
            },
            Err(GitPushFailure::Rejected) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected_detail.clone()),
            },
            Err(GitPushFailure::UnsupportedMergeShape { parents }) => {
                let detail = ToolExecutionErrorDetail::try_new(format!(
                    "UnsupportedMergeShape: merges with {parents} parents are unsupported; expected two parents",
                ))
                .map_err(|_| push_infrastructure(PushCommitCertainty::DefinitelyNotCommitted))?;
                ToolExecutorEvidence::KnownFailed {
                    detail: Some(detail),
                }
            }
            Err(GitPushFailure::AmbiguousMergeBases { bases }) => {
                ToolExecutorEvidence::KnownFailed {
                    detail: Some(ambiguous_merge_bases_detail(&bases)?),
                }
            }
            Err(GitPushFailure::UnprovenMergeParents) => {
                let detail = ToolExecutionErrorDetail::try_new(
                    "UnprovenMergeParents: the retained-head fence must identify exactly one branch parent".to_owned(),
                )
                .map_err(|_| push_infrastructure(PushCommitCertainty::DefinitelyNotCommitted))?;
                ToolExecutorEvidence::KnownFailed {
                    detail: Some(detail),
                }
            }
            Err(GitPushFailure::PreDispatchInfrastructure) => {
                return Err(push_infrastructure(
                    PushCommitCertainty::DefinitelyNotCommitted,
                ));
            }
            Err(GitPushFailure::DispatchUnknown | GitPushFailure::PostDispatchInvalid) => {
                return Err(push_infrastructure(PushCommitCertainty::Ambiguous));
            }
        };
        Ok(invocation.bind(evidence))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GitPushFailure {
    Repository,
    Unresolved,
    Rejected,
    UnsupportedMergeShape { parents: usize },
    UnprovenMergeParents,
    AmbiguousMergeBases { bases: Vec<git2::Oid> },
    PreDispatchInfrastructure,
    DispatchUnknown,
    PostDispatchInvalid,
}

#[derive(Debug, Serialize)]
struct GitPushResult {
    remote: String,
    branch: String,
    commit: String,
}

impl<Transport: GitPushTransport> GitPushExecutor<Transport> {
    pub(super) async fn execute_push(
        &mut self,
        arguments: GitPushArguments,
    ) -> Result<String, GitPushFailure> {
        if self
            .branch_fence
            .as_ref()
            .is_some_and(|branch| branch != &arguments.branch)
        {
            return Err(GitPushFailure::Rejected);
        }
        let WorkspaceRootIdentity { device, inode } = self.root.identity();
        if self.repository_identity.root.device != device
            || self.repository_identity.root.inode != inode
        {
            return Err(GitPushFailure::Repository);
        }
        let root_path = self.root_path.clone();
        let root_identity = self.root.identity();
        let expected_identity = self.repository_identity;
        let authority = Arc::clone(&self.repository_authority);
        let commit_fence = self.commit_fence.clone();
        let reference = format!("refs/heads/{}", arguments.branch);
        let (snapshot, target) = prepare_before_deadline(
            move |deadline| {
                let repository_identity = validate_repository_layout(&root_path, root_identity)
                    .map_err(|_| GitPushFailure::Repository)?;
                if repository_identity != expected_identity {
                    return Err(GitPushFailure::Repository);
                }
                let (_, target) = resolve_pinned_reference_chain_from(&authority, &reference, None)
                    .map_err(|_| GitPushFailure::Unresolved)?;
                let target = target.ok_or(GitPushFailure::Unresolved)?;
                let fence = commit_fence
                    .as_ref()
                    .map(|commit| {
                        crate::layout::parse_full_object_id(commit, authority.object_format)
                            .ok_or(GitPushFailure::Repository)
                    })
                    .transpose()?;
                crate::push_merge::verify_merge(&authority, target, fence, deadline)?;
                let snapshot = PushObjectSnapshot::capture_before_deadline(
                    &authority, target, fence, deadline,
                )
                .map_err(|_| GitPushFailure::Repository)?;
                Ok((snapshot, target))
            },
            PUSH_PREPARATION_TIMEOUT,
        )
        .await?;
        let commit = target.to_string();
        let git_directory = snapshot.repository.path().to_owned();
        let request = GitPushRequest::new(
            descriptor_path(&self.repository_authority.root),
            self.remote.clone(),
            git_directory.clone(),
            git_directory.join("objects"),
            arguments.branch.clone(),
            commit.clone(),
        );
        let receipt = self
            .transport
            .push(request)
            .await
            .map_err(|failure| match failure {
                GitPushTransportFailure::Rejected => GitPushFailure::Rejected,
                GitPushTransportFailure::PreDispatchInfrastructure => {
                    GitPushFailure::PreDispatchInfrastructure
                }
                GitPushTransportFailure::DispatchUnknown => GitPushFailure::DispatchUnknown,
            })?;
        if receipt.commit() != commit {
            return Err(GitPushFailure::PostDispatchInvalid);
        }
        let encoded = serde_json::to_string(&GitPushResult {
            remote: self.remote.name().to_owned(),
            branch: arguments.branch,
            commit,
        })
        .map_err(|_| GitPushFailure::PostDispatchInvalid)?;
        ToolResultText::try_new(encoded)
            .map(ToolResultText::into_string)
            .map_err(|_| GitPushFailure::PostDispatchInvalid)
    }
}

/// Matches the daemon's per-process Git timeout for the in-process preparation phase.
pub(super) const PUSH_PREPARATION_TIMEOUT: Duration = Duration::from_secs(300);

/// Keeps repository I/O and object decoding off the async worker and bounds its wait.
pub(super) async fn prepare_before_deadline<Prepared: Send + 'static>(
    prepare: impl FnOnce(std::time::Instant) -> Result<Prepared, GitPushFailure> + Send + 'static,
    timeout: Duration,
) -> Result<Prepared, GitPushFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut task = tokio::task::spawn_blocking(move || {
        let result = prepare(deadline.into_std());
        if std::time::Instant::now() >= deadline.into_std() {
            Err(GitPushFailure::PreDispatchInfrastructure)
        } else {
            result
        }
    });
    match tokio::time::timeout_at(deadline, &mut task).await {
        Ok(result) => result.map_err(|_| GitPushFailure::PreDispatchInfrastructure)?,
        Err(_) => {
            // Abort prevents queued work from starting; running work owns only its snapshot.
            task.abort();
            Err(GitPushFailure::PreDispatchInfrastructure)
        }
    }
}

const fn push_caller_bug() -> GitPushExecutorError {
    GitPushExecutorError {
        class: OperatorFailureClass::CallerOrHubBug,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PushCommitCertainty {
    DefinitelyNotCommitted,
    Ambiguous,
}

const fn push_infrastructure(certainty: PushCommitCertainty) -> GitPushExecutorError {
    let commit_ambiguous = match certainty {
        PushCommitCertainty::DefinitelyNotCommitted => false,
        PushCommitCertainty::Ambiguous => true,
    };
    GitPushExecutorError {
        class: OperatorFailureClass::Infrastructure { commit_ambiguous },
    }
}

fn ambiguous_merge_bases_detail(
    bases: &[git2::Oid],
) -> Result<ToolExecutionErrorDetail, GitPushExecutorError> {
    let mut detail = format!("AmbiguousMergeBases: {} bases", bases.len());
    let suffix_bytes = format!("; omitted_bases={}", bases.len()).len();
    let mut omitted = bases.len();
    for base in bases {
        let entry = format!("; {base}");
        if detail.len() + entry.len() + suffix_bytes > ToolExecutionErrorDetail::MAX_UTF8_BYTES {
            break;
        }
        detail.push_str(&entry);
        omitted -= 1;
    }
    detail.push_str(&format!("; omitted_bases={omitted}"));
    ToolExecutionErrorDetail::try_new(detail)
        .map_err(|_| push_infrastructure(PushCommitCertainty::DefinitelyNotCommitted))
}

#[cfg(test)]
mod tests {
    use super::ambiguous_merge_bases_detail;
    #[test]
    fn ambiguous_merge_base_detail_names_complete_object_ids() {
        let first = git2::Oid::from_bytes(&[1; 20]).expect("first object ID");
        let second = git2::Oid::from_bytes(&[2; 20]).expect("second object ID");

        let detail = ambiguous_merge_bases_detail(&[first, second]).expect("typed detail");

        assert_eq!(
            detail.as_str(),
            format!("AmbiguousMergeBases: 2 bases; {first}; {second}; omitted_bases=0")
        );
    }

    #[test]
    fn ambiguous_merge_base_detail_omits_whole_ids_when_the_detail_is_full() {
        use signalbox_domain::ToolExecutionErrorDetail;

        let bases: Vec<_> = (0..ToolExecutionErrorDetail::MAX_UTF8_BYTES / 40 + 1)
            .map(|index| git2::Oid::from_str(&format!("{index:040x}")).expect("object ID"))
            .collect();

        let detail = ambiguous_merge_bases_detail(&bases).expect("bounded typed detail");

        let parts: Vec<_> = detail.as_str().split("; ").collect();
        let omitted: usize = parts
            .last()
            .expect("omission count")
            .strip_prefix("omitted_bases=")
            .expect("explicit omissions")
            .parse()
            .expect("count");
        let listed: Vec<_> = parts[1..parts.len() - 1]
            .iter()
            .map(|part| git2::Oid::from_str(part).expect("complete object ID"))
            .collect();
        assert!(omitted > 0);
        assert_eq!(listed, bases[..bases.len() - omitted]);
        assert!(detail.as_str().len() <= ToolExecutionErrorDetail::MAX_UTF8_BYTES);
    }
}
