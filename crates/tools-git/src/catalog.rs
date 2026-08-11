use std::{fs, path::Path};

use git2::ObjectFormat;
use signalbox_application::{
    CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::ToolExecutionErrorDetail;
use signalbox_tool_contract::ToolContractCompileError;
use signalbox_tools_workspace::{
    LocalWorkspaceFileSystem, WorkspaceFileSystem, WorkspaceRoot, WorkspaceRootIdentity,
};

use crate::construction::LocalGitToolsConstructionError;
use crate::contracts::{LocalToolKind, kind_for_name};
use crate::decode::{GitArgumentValidator, decode_operation};
use crate::executor::{LocalGitExecutor, LocalGitExecutorError};
use crate::failure::LocalGitFailure;
use crate::identity::GitIdentity;
use crate::layout::validate_repository_layout;
use crate::pinning::PinnedRepository;

pub(super) const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded Git tool arguments";

pub(super) const REPOSITORY_REJECTED_DETAIL: &str = "injected Git repository was rejected";

pub(super) const PATH_REJECTED_DETAIL: &str = "Git path was rejected by the workspace boundary";

pub(super) const OPERATION_FAILED_DETAIL: &str = "local Git operation failed";

/// Object-identifier format one repository selects once.
///
/// The pinned repository's selection is compiled into this suite's argument
/// validators, so the format is a property of the compiled catalog and not only
/// of the executor. A composition that shares one catalog across several
/// repositories can therefore compare this value rather than assume agreement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitObjectFormat {
    /// Twenty-byte SHA-1 object identifiers.
    Sha1,
    /// Thirty-two-byte SHA-256 object identifiers.
    Sha256,
}

impl GitObjectFormat {
    const fn from_library(format: ObjectFormat) -> Self {
        match format {
            ObjectFormat::Sha1 => Self::Sha1,
            ObjectFormat::Sha256 => Self::Sha256,
        }
    }
}

/// The two directories one pinned repository authority holds.
///
/// Both identities were accepted by the layout validation on either side of the
/// repository open, so they name the directories this suite is bound to rather
/// than whatever a later independent resolution of the same pathname finds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedRepositoryDirectories {
    /// Worktree root the suite was constructed with.
    pub root: WorkspaceRootIdentity,
    /// The `.git` directory immediately inside that root.
    pub administration: WorkspaceRootIdentity,
}

/// Seven local Git declarations and their injected-root executor.
#[derive(Debug)]
pub struct LocalGitTools<FileSystem = LocalWorkspaceFileSystem> {
    pub(super) catalog: CompiledToolCatalog,
    executor: LocalGitExecutor<FileSystem>,
}

impl<FileSystem: WorkspaceFileSystem> LocalGitTools<FileSystem> {
    /// Compiles the local family around one filesystem, direct repository root,
    /// and explicit commit identity.
    pub fn try_new(
        filesystem: FileSystem,
        root_path: impl AsRef<Path>,
        identity: GitIdentity,
    ) -> Result<Self, LocalGitToolsConstructionError> {
        let supplied_root = root_path.as_ref();
        let root_path = fs::canonicalize(supplied_root)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let root = WorkspaceRoot::try_new(&filesystem, supplied_root)
            .map_err(LocalGitToolsConstructionError::Root)?;
        let repository_identity = validate_repository_layout(&root_path, root.identity())?;
        let repository_authority = PinnedRepository::open(&root_path, repository_identity)?;
        let identity_after_root_open = validate_repository_layout(&root_path, root.identity())?;
        if identity_after_root_open != repository_identity {
            return Err(LocalGitToolsConstructionError::Repository);
        }
        let invalid_detail = detail(INVALID_ARGUMENTS_DETAIL)?;
        let repository_detail = detail(REPOSITORY_REJECTED_DETAIL)?;
        let path_detail = detail(PATH_REJECTED_DETAIL)?;
        let operation_detail = detail(OPERATION_FAILED_DETAIL)?;
        let compiled = LocalToolKind::ALL
            .into_iter()
            .map(|kind| {
                let definition = kind.definition().map_err(|error| match error {
                    ToolContractCompileError::Name => LocalGitToolsConstructionError::Name,
                    ToolContractCompileError::Schema => LocalGitToolsConstructionError::Schema,
                })?;
                Ok(CompiledTool::new(
                    definition,
                    GitArgumentValidator {
                        kind,
                        object_format: repository_authority.object_format,
                        detail: invalid_detail.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>, LocalGitToolsConstructionError>>()?;
        let catalog = CompiledToolCatalog::try_new(compiled)
            .map_err(|_| LocalGitToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: LocalGitExecutor {
                filesystem,
                root,
                root_path,
                repository_identity,
                repository_authority,
                identity,
                repository_detail,
                path_detail,
                operation_detail,
            },
        })
    }

    /// Returns the object format the pinned repository selected once.
    ///
    /// Exposed because the compiled argument validators carry this selection: a
    /// caller sharing one compiled catalog across several pinned repositories
    /// has to reject a repository that disagrees rather than validate arguments
    /// against another repository's width.
    pub const fn object_format(&self) -> GitObjectFormat {
        GitObjectFormat::from_library(self.executor.repository_authority.object_format)
    }

    /// Returns the worktree and administration directories this suite pinned.
    ///
    /// Both were accepted by the layout validation on either side of the
    /// repository open, so they name the directories this executor is bound to
    /// rather than whatever the pathname resolves to afterwards: a `.git`
    /// replaced between the repository open and a later resolution stands there
    /// while this executor remains bound to the repository it opened. See
    /// `docs/spec/git-authority-threat-model.md` for directory pinning and
    /// `docs/spec/tool-loop.md` for how a daemon composition uses the pair.
    pub const fn pinned_directories(&self) -> PinnedRepositoryDirectories {
        PinnedRepositoryDirectories {
            root: WorkspaceRootIdentity {
                device: self.executor.repository_identity.root.device,
                inode: self.executor.repository_identity.root.inode,
            },
            administration: WorkspaceRootIdentity {
                device: self.executor.repository_identity.git_directory.device,
                inode: self.executor.repository_identity.git_directory.inode,
            },
        }
    }

    /// Separates immutable catalog and mutable executor composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, LocalGitExecutor<FileSystem>) {
        (self.catalog, self.executor)
    }
}

pub(super) fn detail(
    value: &str,
) -> Result<ToolExecutionErrorDetail, LocalGitToolsConstructionError> {
    ToolExecutionErrorDetail::try_new(value.to_owned())
        .map_err(|_| LocalGitToolsConstructionError::ErrorDetail)
}

impl<FileSystem: WorkspaceFileSystem> ToolExecutor for LocalGitExecutor<FileSystem> {
    type Error = LocalGitExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let kind =
            kind_for_name(invocation.request().name().as_str()).ok_or(LocalGitExecutorError)?;
        let operation = decode_operation(
            kind,
            invocation.request().arguments(),
            self.repository_authority.object_format,
        )
        .map_err(|_| LocalGitExecutorError)?;
        let evidence = match self.execute_operation(operation) {
            Ok(result) => ToolExecutorEvidence::CompletedText(result),
            Err(LocalGitFailure::Repository) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.repository_detail.clone()),
            },
            Err(LocalGitFailure::Path) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.path_detail.clone()),
            },
            Err(LocalGitFailure::Operation) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.operation_detail.clone()),
            },
            Err(LocalGitFailure::Encoding) => return Err(LocalGitExecutorError),
        };
        Ok(invocation.bind(evidence))
    }
}
