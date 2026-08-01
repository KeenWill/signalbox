use std::{fs, path::Path};

use signalbox_application::{
    CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::ToolExecutionErrorDetail;
use signalbox_tool_contract::ToolContractCompileError;
use signalbox_tools_workspace::{LocalWorkspaceFileSystem, WorkspaceFileSystem, WorkspaceRoot};

use crate::construction::LocalGitToolsConstructionError;
use crate::contracts::{LocalToolKind, kind_for_name};
use crate::decode::{GitArgumentValidator, decode_operation};
use crate::descriptor::FileIdentity;
use crate::executor::{LocalGitExecutor, LocalGitExecutorError};
use crate::failure::LocalGitFailure;
use crate::identity::GitIdentity;
use crate::layout::validate_repository_layout;
use crate::pinning::PinnedRepository;

pub(super) const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded Git tool arguments";

pub(super) const REPOSITORY_REJECTED_DETAIL: &str = "injected Git repository was rejected";

pub(super) const PATH_REJECTED_DETAIL: &str = "Git path was rejected by the workspace boundary";

pub(super) const OPERATION_FAILED_DETAIL: &str = "local Git operation failed";

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
        let repository_identity = validate_repository_layout(&root_path)?;
        let root = WorkspaceRoot::try_new(&filesystem, supplied_root)
            .map_err(LocalGitToolsConstructionError::Root)?;
        let opened_identity = root
            .identity()
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        if (FileIdentity {
            device: opened_identity.device(),
            inode: opened_identity.inode(),
        }) != repository_identity.root
        {
            return Err(LocalGitToolsConstructionError::Repository);
        }
        let repository_authority = PinnedRepository::open(&root_path, repository_identity)?;
        let identity_after_root_open = validate_repository_layout(&root_path)?;
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
        let operation = decode_operation(kind, invocation.request().arguments())
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
