use std::{fs, path::Path};

use signalbox_application::{CompiledTool, CompiledToolCatalog, ToolArgumentValidator};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};
use signalbox_tools_workspace::{WorkspaceFileSystem, WorkspaceRoot, WorkspaceRootError};

use crate::GIT_PUSH_CONFIGURED_NAME;
use crate::decode::validate_branch;
use crate::layout::validate_repository_layout;
use crate::pinning::PinnedRepository;
use crate::push_arguments::GitPushArguments;
use crate::push_executor::GitPushExecutor;
use crate::push_transport::ConfiguredGitRemote;

pub(super) const PUSH_REJECTED_DETAIL: &str = "configured Git push was rejected";
pub(super) const PUSH_UNRESOLVED_DETAIL: &str = "configured Git push branch was not resolved";

struct PushContract;

impl ToolContract for PushContract {
    type Arguments = GitPushArguments;
    const NAME: &'static str = GIT_PUSH_CONFIGURED_NAME;
    const DESCRIPTION: &'static str =
        "Pushes one named local branch without force to the deployment-configured remote.";
}

#[derive(Clone, Debug)]
struct GitPushArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for GitPushArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_push(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

pub(super) fn decode_push(arguments: &NormalizedToolArguments) -> Result<GitPushArguments, ()> {
    let arguments: GitPushArguments = serde_json::from_str(arguments.as_str()).map_err(|_| ())?;
    validate_branch(&arguments.branch).map_err(|_| ())?;
    Ok(arguments)
}

/// One non-overridable approval-gated push declaration and its
/// configured-remote executor.
#[derive(Debug)]
pub struct GitPushTools<Transport> {
    catalog: CompiledToolCatalog,
    executor: GitPushExecutor<Transport>,
}

impl<Transport> GitPushTools<Transport> {
    /// Compiles push around one pinned workspace root, configured remote, and
    /// injected physical transport.
    pub fn try_new<FileSystem: WorkspaceFileSystem>(
        filesystem: &FileSystem,
        root_path: impl AsRef<Path>,
        remote: ConfiguredGitRemote,
        transport: Transport,
    ) -> Result<Self, GitPushToolsConstructionError> {
        let supplied_root = root_path.as_ref();
        let root_path = fs::canonicalize(supplied_root)
            .map_err(|_| GitPushToolsConstructionError::Repository)?;
        let root = WorkspaceRoot::try_new(filesystem, supplied_root)
            .map_err(GitPushToolsConstructionError::Root)?;
        let repository_identity = validate_repository_layout(&root_path, root.identity())
            .map_err(|_| GitPushToolsConstructionError::Repository)?;
        let repository_authority = PinnedRepository::open(&root_path, repository_identity)
            .map_err(|_| GitPushToolsConstructionError::Repository)?;
        let identity_after_root_open = validate_repository_layout(&root_path, root.identity())
            .map_err(|_| GitPushToolsConstructionError::Repository)?;
        if identity_after_root_open != repository_identity {
            return Err(GitPushToolsConstructionError::Repository);
        }
        let repository_detail = detail("injected Git repository was rejected")?;
        let unresolved_detail = detail(PUSH_UNRESOLVED_DETAIL)?;
        let rejected_detail = detail(PUSH_REJECTED_DETAIL)?;
        let catalog = git_push_catalog()?;
        Ok(Self {
            catalog,
            executor: GitPushExecutor::new(
                root,
                root_path,
                repository_identity,
                repository_authority,
                remote,
                transport,
                repository_detail,
                unresolved_detail,
                rejected_detail,
            ),
        })
    }

    /// Sets the decoded byte limit for each pushed object and delta dependency.
    /// `None` leaves blob content unbounded, which is the default.
    /// Commits, trees, and tags retain their structural byte limit.
    pub fn with_max_object_bytes(mut self, max_bytes: Option<usize>) -> Self {
        // The suite owns its executor exclusively until into_parts exposes it.
        #[allow(clippy::expect_used)]
        let authority = std::sync::Arc::get_mut(&mut self.executor.repository_authority)
            .expect("an unexecuted Git push suite owns its authority exclusively");
        authority.max_object_bytes = max_bytes;
        self
    }

    /// Separates catalog and executor composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, GitPushExecutor<Transport>) {
        (self.catalog, self.executor)
    }
}

/// Compiles the configured push declaration without binding a session workspace.
pub fn git_push_catalog() -> Result<CompiledToolCatalog, GitPushToolsConstructionError> {
    let invalid_detail = detail("invalid bounded Git tool arguments")?;
    let definition = compile_contract_definition::<PushContract>(
        ToolPermissionDefault::AlwaysConfirm,
        ToolEffectClass::ExternalEffect,
    )
    .map_err(|error| match error {
        ToolContractCompileError::Name => GitPushToolsConstructionError::Name,
        ToolContractCompileError::Schema => GitPushToolsConstructionError::Schema,
    })?;
    CompiledToolCatalog::try_new(vec![CompiledTool::new(
        definition,
        GitPushArgumentValidator {
            detail: invalid_detail,
        },
    )])
    .map_err(|_| GitPushToolsConstructionError::Duplicate)
}

fn detail(value: &str) -> Result<ToolExecutionErrorDetail, GitPushToolsConstructionError> {
    ToolExecutionErrorDetail::try_new(value.to_owned())
        .map_err(|_| GitPushToolsConstructionError::ErrorDetail)
}

#[derive(signalbox_derive::OperatorError)]
/// Static push suite or injected-repository construction failure.
#[derive(Debug)]
pub enum GitPushToolsConstructionError {
    #[error("Git push tool construction failed")]
    /// Static tool name failed compilation.
    Name,
    #[error("Git push tool construction failed")]
    /// Static schema failed compilation.
    Schema,
    #[error("Git push tool construction failed")]
    /// Static detail failed construction.
    ErrorDetail,
    #[error("Git push tool construction failed")]
    /// The fixed catalog unexpectedly contained a duplicate.
    Duplicate,
    #[error("Git push tool construction failed")]
    /// The injected workspace root was invalid.
    Root(#[source] WorkspaceRootError),
    #[error("Git push tool construction failed")]
    /// The repository layout escaped or did not match the injected root.
    Repository,
}
