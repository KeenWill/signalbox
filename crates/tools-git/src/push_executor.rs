use std::{error::Error, fmt, path::PathBuf};

use git2::Odb;
use serde::Serialize;
use signalbox_application::{
    ClassifyOperatorFailure, CorrelatedToolExecutorEvidence, OperatorFailureClass,
    ToolExecutionInvocation, ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::{ToolExecutionErrorDetail, ToolResultText};
use signalbox_tools_workspace::{WorkspaceRoot, WorkspaceRootIdentity};

use crate::GIT_PUSH_CONFIGURED_NAME;
use crate::bounded::find_bounded_commit;
use crate::descriptor::{RepositoryIdentity, descriptor_path};
use crate::layout::validate_repository_layout;
use crate::pinning::{PinnedObjectDatabase, PinnedRepository};
use crate::push_arguments::GitPushArguments;
use crate::push_catalog::decode_push;
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
    repository_authority: PinnedRepository,
    remote: ConfiguredGitRemote,
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
            repository_authority,
            remote,
            transport,
            repository_detail,
            unresolved_detail,
            rejected_detail,
        }
    }
}

/// Sanitized push executor failure with explicit commit certainty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitPushExecutorError {
    class: OperatorFailureClass,
}

impl fmt::Display for GitPushExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Git push executor failed")
    }
}

impl Error for GitPushExecutorError {}

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
        let evidence = match self.execute_push(arguments) {
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
            Err(GitPushFailure::PreDispatchInfrastructure) => {
                return Err(push_infrastructure(false));
            }
            Err(GitPushFailure::DispatchUnknown | GitPushFailure::PostDispatchInvalid) => {
                return Err(push_infrastructure(true));
            }
        };
        Ok(invocation.bind(evidence))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GitPushFailure {
    Repository,
    Unresolved,
    Rejected,
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
    pub(super) fn execute_push(
        &mut self,
        arguments: GitPushArguments,
    ) -> Result<String, GitPushFailure> {
        let WorkspaceRootIdentity { device, inode } = self.root.identity();
        if self.repository_identity.root.device != device
            || self.repository_identity.root.inode != inode
        {
            return Err(GitPushFailure::Repository);
        }
        let repository_identity = validate_repository_layout(&self.root_path, self.root.identity())
            .map_err(|_| GitPushFailure::Repository)?;
        if repository_identity != self.repository_identity {
            return Err(GitPushFailure::Repository);
        }

        let repository = self
            .repository_authority
            .repository()
            .map_err(|_| GitPushFailure::Repository)?;
        let pinned_objects = PinnedObjectDatabase::capture(&self.repository_authority)
            .map_err(|_| GitPushFailure::Repository)?;
        let object_database = Odb::new().map_err(|_| GitPushFailure::Repository)?;
        pinned_objects
            .add_to(&object_database)
            .map_err(|_| GitPushFailure::Repository)?;
        repository
            .set_odb(&object_database)
            .map_err(|_| GitPushFailure::Repository)?;

        let reference = format!("refs/heads/{}", arguments.branch);
        let (_, target) =
            resolve_pinned_reference_chain_from(&self.repository_authority, &reference, None)
                .map_err(|_| GitPushFailure::Unresolved)?;
        let target = target.ok_or(GitPushFailure::Unresolved)?;
        let commit = find_bounded_commit(&repository, target)
            .map_err(|_| GitPushFailure::Unresolved)?
            .id()
            .to_string();
        pinned_objects
            .validate_live(&self.repository_authority)
            .map_err(|_| GitPushFailure::Repository)?;

        let request = GitPushRequest::new(
            descriptor_path(&self.repository_authority.root),
            self.remote.clone(),
            arguments.branch.clone(),
            commit.clone(),
        );
        let receipt = self
            .transport
            .push(request)
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

const fn push_caller_bug() -> GitPushExecutorError {
    GitPushExecutorError {
        class: OperatorFailureClass::CallerOrHubBug,
    }
}

const fn push_infrastructure(commit_ambiguous: bool) -> GitPushExecutorError {
    GitPushExecutorError {
        class: OperatorFailureClass::Infrastructure { commit_ambiguous },
    }
}
