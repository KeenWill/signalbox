//! Typed repository-local Git tools over an injected workspace root.
//!
//! Repository discovery and linked worktrees are deliberately unsupported. A
//! suite binds one direct main worktree whose .git directory is inside the
//! injected root. The local family has no remote operation.

use std::{
    collections::HashSet,
    error::Error,
    fmt, fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use git2::{
    BranchType, DiffFormat, DiffOptions, ErrorCode, IndexEntry, IndexTime, Oid, Repository,
    RepositoryOpenFlags, Signature, Sort, StatusOptions, build::CheckoutBuilder,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
    ToolResultText,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};
use signalbox_tools_workspace::{
    LocalWorkspaceFileSystem, WorkspaceEntryKind, WorkspaceFileSystem, WorkspaceResolveError,
    WorkspaceRoot, WorkspaceRootError,
};

/// Repository status tool name.
pub const GIT_STATUS_NAME: &str = "git_status";
/// Repository diff tool name.
pub const GIT_DIFF_NAME: &str = "git_diff";
/// Repository history tool name.
pub const GIT_LOG_NAME: &str = "git_log";
/// Index staging tool name.
pub const GIT_STAGE_NAME: &str = "git_stage";
/// Commit creation tool name.
pub const GIT_CREATE_COMMIT_NAME: &str = "git_create_commit";
/// Local branch creation tool name.
pub const GIT_BRANCH_CREATE_NAME: &str = "git_branch_create";
/// Local branch switch tool name.
pub const GIT_BRANCH_SWITCH_NAME: &str = "git_branch_switch";
/// Configured-remote branch push tool name.
pub const GIT_PUSH_CONFIGURED_NAME: &str = "git_push_configured";

/// Fixed local-family catalog order.
pub const LOCAL_GIT_TOOL_NAMES: [&str; 7] = [
    GIT_BRANCH_CREATE_NAME,
    GIT_BRANCH_SWITCH_NAME,
    GIT_CREATE_COMMIT_NAME,
    GIT_DIFF_NAME,
    GIT_LOG_NAME,
    GIT_STAGE_NAME,
    GIT_STATUS_NAME,
];

const MAX_BRANCH_BYTES: usize = 255;
const MAX_REVISION_BYTES: usize = 1024;
const MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_STAGE_PATHS: usize = 256;
const MAX_STAGE_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_STATUS_ENTRIES: usize = 128;
const MAX_STATUS_PATH_BYTES: usize = 1024;
const MAX_LOG_ENTRIES: usize = 50;
const DEFAULT_LOG_ENTRIES: usize = 25;
const MAX_LOG_IDENTITY_BYTES: usize = 256;
const MAX_LOG_MESSAGE_BYTES: usize = 2048;
const MAX_DIFF_BYTES: usize = 128 * 1024;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded Git tool arguments";
const REPOSITORY_REJECTED_DETAIL: &str = "injected Git repository was rejected";
const PATH_REJECTED_DETAIL: &str = "Git path was rejected by the workspace boundary";
const OPERATION_FAILED_DETAIL: &str = "local Git operation failed";
const PUSH_REJECTED_DETAIL: &str = "configured Git push was rejected";
const MAX_REMOTE_NAME_BYTES: usize = 255;
const MAX_REMOTE_URL_BYTES: usize = 4096;

/// Injected commit author and committer identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitIdentity {
    name: String,
    email: String,
}

impl GitIdentity {
    /// Constructs an explicit identity without consulting Git configuration or
    /// process environment.
    pub fn try_new(
        name: impl Into<String>,
        email: impl Into<String>,
    ) -> Result<Self, InvalidGitIdentity> {
        let name = name.into();
        let email = email.into();
        if invalid_identity_part(&name) || invalid_identity_part(&email) {
            return Err(InvalidGitIdentity);
        }
        Ok(Self { name, email })
    }

    /// Borrows the configured author name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows the configured author email.
    pub fn email(&self) -> &str {
        &self.email
    }

    fn signature(&self) -> Result<Signature<'static>, git2::Error> {
        Signature::now(&self.name, &self.email)
    }
}

fn invalid_identity_part(value: &str) -> bool {
    value.is_empty()
        || value.len() > MAX_IDENTITY_BYTES
        || value.contains('\0')
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('<')
        || value.contains('>')
}

/// An injected Git identity was not safe for a commit signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGitIdentity;

impl fmt::Display for InvalidGitIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid injected Git identity")
    }
}

impl Error for InvalidGitIdentity {}

/// Empty status arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitStatusArguments {}

/// Diff selection: current worktree against HEAD, or two revisions.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitDiffArguments {
    /// Includes both staged and unstaged worktree changes against HEAD.
    Worktree,
    /// Compares the trees named by two revisions.
    Revisions {
        /// Older revision expression.
        #[schemars(length(min = 1, max = MAX_REVISION_BYTES))]
        base: String,
        /// Newer revision expression.
        #[schemars(length(min = 1, max = MAX_REVISION_BYTES))]
        head: String,
    },
}

fn default_log_revision() -> String {
    "HEAD".to_owned()
}

fn default_log_entries() -> usize {
    DEFAULT_LOG_ENTRIES
}

/// Bounded history arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitLogArguments {
    /// Revision expression at which traversal starts.
    #[serde(default = "default_log_revision")]
    #[schemars(length(min = 1, max = MAX_REVISION_BYTES))]
    revision: String,
    /// Maximum commits returned.
    #[serde(default = "default_log_entries")]
    #[schemars(range(min = 1, max = MAX_LOG_ENTRIES))]
    max_entries: usize,
}

/// Exact root-relative paths to stage.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitStageArguments {
    /// Files to add, update, or remove from the index.
    #[schemars(length(min = 1, max = MAX_STAGE_PATHS))]
    paths: Vec<String>,
}

/// Verbatim commit-message arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitCommitArguments {
    /// Exact commit message, interpreted only as UTF-8 data.
    #[schemars(length(max = MAX_COMMIT_MESSAGE_BYTES))]
    message: String,
}

/// Local branch creation arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBranchCreateArguments {
    /// New local branch shorthand.
    #[schemars(length(min = 1, max = MAX_BRANCH_BYTES))]
    name: String,
    /// Revision resolving to the branch's initial commit.
    #[schemars(length(min = 1, max = MAX_REVISION_BYTES))]
    start: String,
}

/// Local branch switch arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBranchSwitchArguments {
    /// Existing local branch shorthand.
    #[schemars(length(min = 1, max = MAX_BRANCH_BYTES))]
    name: String,
}

/// Push arguments contain only a local branch; the destination is deployment
/// configuration and cannot be supplied by a model.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitPushArguments {
    /// Existing local branch to push without force.
    #[schemars(length(min = 1, max = MAX_BRANCH_BYTES))]
    branch: String,
}

/// Exact deployment-owned remote configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct ConfiguredGitRemote {
    name: String,
    url: String,
}

impl ConfiguredGitRemote {
    /// Constructs one fixed remote name and exact destination URL.
    pub fn try_new(
        name: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, InvalidConfiguredGitRemote> {
        let name = name.into();
        let url = url.into();
        let probe = format!("refs/remotes/{name}/probe");
        if name.is_empty()
            || name.len() > MAX_REMOTE_NAME_BYTES
            || name.contains('/')
            || !git2::Reference::is_valid_name(&probe)
            || url.is_empty()
            || url.len() > MAX_REMOTE_URL_BYTES
            || url.chars().any(char::is_control)
        {
            return Err(InvalidConfiguredGitRemote);
        }
        Ok(Self { name, url })
    }

    /// Borrows the configured Git remote name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows the exact configured destination URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl fmt::Debug for ConfiguredGitRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredGitRemote")
            .field("name", &self.name)
            .field("url", &"[CONFIGURED]")
            .finish()
    }
}

/// Deployment remote configuration was not a bounded name and destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidConfiguredGitRemote;

impl fmt::Display for InvalidConfiguredGitRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid configured Git remote")
    }
}

impl Error for InvalidConfiguredGitRemote {}

struct StatusContract;
impl ToolContract for StatusContract {
    type Arguments = GitStatusArguments;
    const NAME: &'static str = GIT_STATUS_NAME;
    const DESCRIPTION: &'static str =
        "Returns bounded status for the injected repository worktree.";
}

struct DiffContract;
impl ToolContract for DiffContract {
    type Arguments = GitDiffArguments;
    const NAME: &'static str = GIT_DIFF_NAME;
    const DESCRIPTION: &'static str =
        "Returns a bounded patch for the injected worktree or between two revisions.";
}

struct LogContract;
impl ToolContract for LogContract {
    type Arguments = GitLogArguments;
    const NAME: &'static str = GIT_LOG_NAME;
    const DESCRIPTION: &'static str =
        "Returns bounded commit history from one revision in the injected repository.";
}

struct StageContract;
impl ToolContract for StageContract {
    type Arguments = GitStageArguments;
    const NAME: &'static str = GIT_STAGE_NAME;
    const DESCRIPTION: &'static str =
        "Stages exact symlink-safe paths inside the injected repository root.";
}

struct CommitContract;
impl ToolContract for CommitContract {
    type Arguments = GitCommitArguments;
    const NAME: &'static str = GIT_CREATE_COMMIT_NAME;
    const DESCRIPTION: &'static str = "Commits the current index with the model-supplied message preserved verbatim and an injected identity.";
}

struct BranchCreateContract;
impl ToolContract for BranchCreateContract {
    type Arguments = GitBranchCreateArguments;
    const NAME: &'static str = GIT_BRANCH_CREATE_NAME;
    const DESCRIPTION: &'static str = "Creates one non-forced local branch at an exact revision.";
}

struct BranchSwitchContract;
impl ToolContract for BranchSwitchContract {
    type Arguments = GitBranchSwitchArguments;
    const NAME: &'static str = GIT_BRANCH_SWITCH_NAME;
    const DESCRIPTION: &'static str =
        "Safely checks out one existing local branch in the injected worktree.";
}

struct PushContract;
impl ToolContract for PushContract {
    type Arguments = GitPushArguments;
    const NAME: &'static str = GIT_PUSH_CONFIGURED_NAME;
    const DESCRIPTION: &'static str =
        "Pushes one named local branch without force to the deployment-configured remote.";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalToolKind {
    BranchCreate,
    BranchSwitch,
    Commit,
    Diff,
    Log,
    Stage,
    Status,
}

impl LocalToolKind {
    const ALL: [Self; 7] = [
        Self::BranchCreate,
        Self::BranchSwitch,
        Self::Commit,
        Self::Diff,
        Self::Log,
        Self::Stage,
        Self::Status,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::BranchCreate => GIT_BRANCH_CREATE_NAME,
            Self::BranchSwitch => GIT_BRANCH_SWITCH_NAME,
            Self::Commit => GIT_CREATE_COMMIT_NAME,
            Self::Diff => GIT_DIFF_NAME,
            Self::Log => GIT_LOG_NAME,
            Self::Stage => GIT_STAGE_NAME,
            Self::Status => GIT_STATUS_NAME,
        }
    }

    const fn effect(self) -> ToolEffectClass {
        match self {
            Self::Diff | Self::Log | Self::Status => ToolEffectClass::EffectFree,
            Self::BranchCreate | Self::BranchSwitch | Self::Commit | Self::Stage => {
                ToolEffectClass::ExternalEffect
            }
        }
    }

    fn definition(self) -> Result<ToolDefinition, ToolContractCompileError> {
        match self {
            Self::BranchCreate => compile_contract_definition::<BranchCreateContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::BranchSwitch => compile_contract_definition::<BranchSwitchContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Commit => compile_contract_definition::<CommitContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Diff => compile_contract_definition::<DiffContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Log => compile_contract_definition::<LogContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Stage => compile_contract_definition::<StageContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
            Self::Status => compile_contract_definition::<StatusContract>(
                ToolPermissionDefault::Auto,
                self.effect(),
            ),
        }
    }
}

fn kind_for_name(name: &str) -> Option<LocalToolKind> {
    LocalToolKind::ALL
        .into_iter()
        .find(|kind| kind.name() == name)
}

#[derive(Debug)]
enum LocalOperation {
    BranchCreate(GitBranchCreateArguments),
    BranchSwitch(GitBranchSwitchArguments),
    Commit(GitCommitArguments),
    Diff(GitDiffArguments),
    Log(GitLogArguments),
    Stage(GitStageArguments),
    Status,
}

fn decode_operation(
    kind: LocalToolKind,
    arguments: &NormalizedToolArguments,
) -> Result<LocalOperation, InvalidGitArguments> {
    let operation = match kind {
        LocalToolKind::BranchCreate => {
            serde_json::from_str(arguments.as_str()).map(LocalOperation::BranchCreate)
        }
        LocalToolKind::BranchSwitch => {
            serde_json::from_str(arguments.as_str()).map(LocalOperation::BranchSwitch)
        }
        LocalToolKind::Commit => {
            serde_json::from_str(arguments.as_str()).map(LocalOperation::Commit)
        }
        LocalToolKind::Diff => serde_json::from_str(arguments.as_str()).map(LocalOperation::Diff),
        LocalToolKind::Log => serde_json::from_str(arguments.as_str()).map(LocalOperation::Log),
        LocalToolKind::Stage => serde_json::from_str(arguments.as_str()).map(LocalOperation::Stage),
        LocalToolKind::Status => serde_json::from_str::<GitStatusArguments>(arguments.as_str())
            .map(|_| LocalOperation::Status),
    }
    .map_err(|_| InvalidGitArguments)?;
    validate_operation(&operation)?;
    Ok(operation)
}

fn validate_operation(operation: &LocalOperation) -> Result<(), InvalidGitArguments> {
    match operation {
        LocalOperation::BranchCreate(arguments) => {
            validate_branch(&arguments.name)?;
            validate_revision(&arguments.start)
        }
        LocalOperation::BranchSwitch(arguments) => validate_branch(&arguments.name),
        LocalOperation::Commit(arguments) => (arguments.message.len() <= MAX_COMMIT_MESSAGE_BYTES
            && !arguments.message.contains('\0'))
        .then_some(())
        .ok_or(InvalidGitArguments),
        LocalOperation::Diff(GitDiffArguments::Worktree) | LocalOperation::Status => Ok(()),
        LocalOperation::Diff(GitDiffArguments::Revisions { base, head }) => {
            validate_revision(base)?;
            validate_revision(head)
        }
        LocalOperation::Log(arguments) => {
            validate_revision(&arguments.revision)?;
            (arguments.max_entries > 0 && arguments.max_entries <= MAX_LOG_ENTRIES)
                .then_some(())
                .ok_or(InvalidGitArguments)
        }
        LocalOperation::Stage(arguments) => {
            if arguments.paths.is_empty() || arguments.paths.len() > MAX_STAGE_PATHS {
                return Err(InvalidGitArguments);
            }
            let mut unique = HashSet::new();
            if arguments
                .paths
                .iter()
                .all(|path| unique.insert(path.as_str()) && checked_relative_path(path).is_ok())
            {
                Ok(())
            } else {
                Err(InvalidGitArguments)
            }
        }
    }
}

fn validate_branch(value: &str) -> Result<(), InvalidGitArguments> {
    if value.is_empty() || value.len() > MAX_BRANCH_BYTES || value.contains('\0') {
        return Err(InvalidGitArguments);
    }
    git2::Reference::is_valid_name(&format!("refs/heads/{value}"))
        .then_some(())
        .ok_or(InvalidGitArguments)
}

fn validate_revision(value: &str) -> Result<(), InvalidGitArguments> {
    (!value.is_empty() && value.len() <= MAX_REVISION_BYTES && !value.contains('\0'))
        .then_some(())
        .ok_or(InvalidGitArguments)
}

fn checked_relative_path(value: &str) -> Result<PathBuf, InvalidGitArguments> {
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        return Err(InvalidGitArguments);
    }
    let path = Path::new(value);
    if !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(path.to_owned())
    } else {
        Err(InvalidGitArguments)
    }
}

/// Model arguments were outside the closed Git contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGitArguments;

impl fmt::Display for InvalidGitArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Git tool arguments")
    }
}

impl Error for InvalidGitArguments {}

#[derive(Clone, Debug)]
struct GitArgumentValidator {
    kind: LocalToolKind,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for GitArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_operation(self.kind, arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// Static suite or injected-repository construction failure.
#[derive(Debug)]
pub enum LocalGitToolsConstructionError {
    /// Static tool name failed compilation.
    Name,
    /// Static schema failed compilation.
    Schema,
    /// Static detail failed construction.
    ErrorDetail,
    /// The fixed catalog unexpectedly contained a duplicate.
    Duplicate,
    /// The injected root was invalid.
    Root(WorkspaceRootError),
    /// The repository layout escaped or did not match the injected root.
    Repository,
}

impl fmt::Display for LocalGitToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("local Git tool construction failed")
    }
}

impl Error for LocalGitToolsConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
            Self::Name | Self::Schema | Self::ErrorDetail | Self::Duplicate | Self::Repository => {
                None
            }
        }
    }
}

/// Seven local Git declarations and their injected-root executor.
#[derive(Debug)]
pub struct LocalGitTools<FileSystem = LocalWorkspaceFileSystem> {
    catalog: CompiledToolCatalog,
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
        let root = WorkspaceRoot::try_new(&filesystem, supplied_root)
            .map_err(LocalGitToolsConstructionError::Root)?;
        let root_path = fs::canonicalize(supplied_root)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let root_identity = validate_repository_layout(&root_path)?;
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
                root_identity,
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

fn detail(value: &str) -> Result<ToolExecutionErrorDetail, LocalGitToolsConstructionError> {
    ToolExecutionErrorDetail::try_new(value.to_owned())
        .map_err(|_| LocalGitToolsConstructionError::ErrorDetail)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootIdentity {
    device: u64,
    inode: u64,
}

fn validate_repository_layout(root: &Path) -> Result<RootIdentity, LocalGitToolsConstructionError> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let root_identity = RootIdentity {
        device: root_metadata.dev(),
        inode: root_metadata.ino(),
    };
    let dot_git = root.join(".git");
    let metadata =
        fs::symlink_metadata(&dot_git).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    if dot_git.join("commondir").exists() || dot_git.join("objects/info/alternates").exists() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    reject_administrative_symlinks(&dot_git)?;
    reject_escaping_config(&dot_git.join("config"))?;
    let repository = Repository::open_ext(
        root,
        RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&Path>(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if repository.is_bare() || repository.workdir() != Some(root) {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    Ok(root_identity)
}

fn reject_administrative_symlinks(directory: &Path) -> Result<(), LocalGitToolsConstructionError> {
    let mut pending = vec![directory.to_owned()];
    let mut inspected = 0_usize;
    while let Some(current) = pending.pop() {
        let entries =
            fs::read_dir(current).map_err(|_| LocalGitToolsConstructionError::Repository)?;
        for entry in entries {
            let entry = entry.map_err(|_| LocalGitToolsConstructionError::Repository)?;
            inspected = inspected.saturating_add(1);
            if inspected > 100_000 {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            let file_type = entry
                .file_type()
                .map_err(|_| LocalGitToolsConstructionError::Repository)?;
            if file_type.is_symlink() {
                return Err(LocalGitToolsConstructionError::Repository);
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

fn reject_escaping_config(config_path: &Path) -> Result<(), LocalGitToolsConstructionError> {
    let config =
        fs::read_to_string(config_path).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if config.len() > 1024 * 1024 {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let mut section = "";
    for line in config.lines() {
        let normalized = line.trim().to_ascii_lowercase();
        if normalized.starts_with('[') {
            section = if normalized.starts_with("[core]") {
                "core"
            } else if normalized.starts_with("[include") {
                return Err(LocalGitToolsConstructionError::Repository);
            } else {
                ""
            };
            continue;
        }
        if section == "core"
            && normalized
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "worktree")
        {
            return Err(LocalGitToolsConstructionError::Repository);
        }
    }
    Ok(())
}

/// Executor for local Git operations only.
#[derive(Debug)]
pub struct LocalGitExecutor<FileSystem> {
    filesystem: FileSystem,
    root: WorkspaceRoot,
    root_path: PathBuf,
    root_identity: RootIdentity,
    identity: GitIdentity,
    repository_detail: ToolExecutionErrorDetail,
    path_detail: ToolExecutionErrorDetail,
    operation_detail: ToolExecutionErrorDetail,
}

/// Sanitized local Git executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalGitExecutorError;

impl fmt::Display for LocalGitExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("local Git executor contract failed")
    }
}

impl Error for LocalGitExecutorError {}

impl ClassifyOperatorFailure for LocalGitExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalGitFailure {
    Repository,
    Path,
    Operation,
    Encoding,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum LocalGitResult {
    Status(StatusResult),
    Diff(DiffResult),
    Log(LogResult),
    Stage(StageResult),
    Commit(CommitResult),
    BranchCreate(BranchResult),
    BranchSwitch(BranchResult),
}

#[derive(Debug, Serialize)]
struct StatusResult {
    branch: Option<String>,
    head: Option<String>,
    entries: Vec<StatusEntry>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct StatusEntry {
    path: String,
    index: &'static str,
    worktree: &'static str,
}

#[derive(Debug, Serialize)]
struct DiffResult {
    patch: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct LogResult {
    commits: Vec<LogEntry>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct LogEntry {
    commit: String,
    author_name: String,
    author_email: String,
    message: String,
    message_truncated: bool,
}

#[derive(Debug, Serialize)]
struct StageResult {
    staged_paths: usize,
}

#[derive(Debug, Serialize)]
struct CommitResult {
    commit: String,
}

#[derive(Debug, Serialize)]
struct BranchResult {
    branch: String,
    head: String,
}

impl<FileSystem: WorkspaceFileSystem> LocalGitExecutor<FileSystem> {
    fn execute_operation(&self, operation: LocalOperation) -> Result<String, LocalGitFailure> {
        let repository = open_validated_repository(&self.root_path, self.root_identity)?;
        let result = match operation {
            LocalOperation::Status => LocalGitResult::Status(status(&repository)?),
            LocalOperation::Diff(arguments) => LocalGitResult::Diff(diff(&repository, arguments)?),
            LocalOperation::Log(arguments) => LocalGitResult::Log(log(&repository, arguments)?),
            LocalOperation::Stage(arguments) => {
                LocalGitResult::Stage(self.stage(&repository, arguments)?)
            }
            LocalOperation::Commit(arguments) => {
                LocalGitResult::Commit(commit(&repository, &self.identity, arguments)?)
            }
            LocalOperation::BranchCreate(arguments) => {
                LocalGitResult::BranchCreate(branch_create(&repository, arguments)?)
            }
            LocalOperation::BranchSwitch(arguments) => {
                LocalGitResult::BranchSwitch(self.branch_switch(&repository, arguments)?)
            }
        };
        let encoded = serde_json::to_string(&result).map_err(|_| LocalGitFailure::Encoding)?;
        ToolResultText::try_new(encoded)
            .map(ToolResultText::into_string)
            .map_err(|_| LocalGitFailure::Encoding)
    }

    fn stage(
        &self,
        repository: &Repository,
        arguments: GitStageArguments,
    ) -> Result<StageResult, LocalGitFailure> {
        let mut index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
        for supplied in &arguments.paths {
            let path = checked_relative_path(supplied).map_err(|_| LocalGitFailure::Path)?;
            match self
                .filesystem
                .read_file_prefix(&self.root, &path, MAX_STAGE_FILE_BYTES)
            {
                Ok(read) if read.truncated => return Err(LocalGitFailure::Operation),
                Ok(read) => {
                    let mode = if read.mode & 0o111 == 0 {
                        0o100644
                    } else {
                        0o100755
                    };
                    let entry = IndexEntry {
                        ctime: IndexTime::new(0, 0),
                        mtime: IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode,
                        uid: 0,
                        gid: 0,
                        file_size: 0,
                        id: Oid::ZERO_SHA1,
                        flags: 0,
                        flags_extended: 0,
                        path: supplied.as_bytes().to_vec(),
                    };
                    index
                        .add_frombuffer(&entry, &read.bytes)
                        .map_err(|_| LocalGitFailure::Operation)?;
                }
                Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
                Err(WorkspaceResolveError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound
                        && index.get_path(&path, 0).is_some() =>
                {
                    index
                        .remove_path(&path)
                        .map_err(|_| LocalGitFailure::Operation)?;
                }
                Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
            }
        }
        index.write().map_err(|_| LocalGitFailure::Operation)?;
        Ok(StageResult {
            staged_paths: arguments.paths.len(),
        })
    }

    fn branch_switch(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
    ) -> Result<BranchResult, LocalGitFailure> {
        let branch = repository
            .find_branch(&arguments.name, BranchType::Local)
            .map_err(|_| LocalGitFailure::Operation)?;
        let reference = branch.into_reference();
        let target = reference.target().ok_or(LocalGitFailure::Operation)?;
        let target_commit = repository
            .find_commit(target)
            .map_err(|_| LocalGitFailure::Operation)?;
        let current_tree = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_tree().ok());
        let target_tree = target_commit
            .tree()
            .map_err(|_| LocalGitFailure::Operation)?;
        let changes = repository
            .diff_tree_to_tree(current_tree.as_ref(), Some(&target_tree), None)
            .map_err(|_| LocalGitFailure::Operation)?;
        for delta in changes.deltas() {
            if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                validate_checkout_path(&self.filesystem, &self.root, path)?;
            }
        }
        let mut checkout = CheckoutBuilder::new();
        checkout.safe();
        repository
            .checkout_tree(target_commit.as_object(), Some(&mut checkout))
            .map_err(|_| LocalGitFailure::Operation)?;
        repository
            .set_head(reference.name().map_err(|_| LocalGitFailure::Operation)?)
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok(BranchResult {
            branch: arguments.name,
            head: target.to_string(),
        })
    }
}

fn open_validated_repository(
    root: &Path,
    expected_identity: RootIdentity,
) -> Result<Repository, LocalGitFailure> {
    let identity_before =
        validate_repository_layout(root).map_err(|_| LocalGitFailure::Repository)?;
    if identity_before != expected_identity {
        return Err(LocalGitFailure::Repository);
    }
    let repository = Repository::open_ext(
        root,
        RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&Path>(),
    )
    .map_err(|_| LocalGitFailure::Repository)?;
    let identity_after =
        validate_repository_layout(root).map_err(|_| LocalGitFailure::Repository)?;
    if identity_after != expected_identity || repository.workdir() != Some(root) {
        return Err(LocalGitFailure::Repository);
    }
    Ok(repository)
}

fn validate_checkout_path<FileSystem: WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    path: &Path,
) -> Result<(), LocalGitFailure> {
    let parent = path.parent().unwrap_or(Path::new("."));
    match filesystem.entry_kind(root, parent) {
        Ok(WorkspaceEntryKind::Directory) => {}
        Ok(_) | Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
        Err(WorkspaceResolveError::Io { .. }) => {}
    }
    match filesystem.entry_kind(root, path) {
        Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other) => Err(LocalGitFailure::Path),
        Ok(WorkspaceEntryKind::File | WorkspaceEntryKind::Directory)
        | Err(WorkspaceResolveError::Io { .. }) => Ok(()),
        Err(WorkspaceResolveError::Rejected(_)) => Err(LocalGitFailure::Path),
    }
}

fn status(repository: &Repository) -> Result<StatusResult, LocalGitFailure> {
    let branch = branch_name(repository);
    let head = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| oid.to_string());
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .exclude_submodules(true);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(|_| LocalGitFailure::Operation)?;
    let mut truncated = statuses.len() > MAX_STATUS_ENTRIES;
    let mut entries = Vec::new();
    for entry in statuses.iter().take(MAX_STATUS_ENTRIES) {
        let value = entry.status();
        let (path, path_truncated) =
            bounded_text(entry.path().unwrap_or("[non-utf8]"), MAX_STATUS_PATH_BYTES);
        truncated |= path_truncated;
        entries.push(StatusEntry {
            path,
            index: index_status(value),
            worktree: worktree_status(value),
        });
    }
    Ok(StatusResult {
        branch,
        head,
        entries,
        truncated,
    })
}

fn branch_name(repository: &Repository) -> Option<String> {
    repository
        .head()
        .ok()
        .and_then(|head| head.shorthand().ok().map(str::to_owned))
        .or_else(|| {
            repository
                .find_reference("HEAD")
                .ok()
                .and_then(|head| head.symbolic_target().ok().flatten().map(str::to_owned))
                .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_owned))
        })
}

fn index_status(status: git2::Status) -> &'static str {
    if status.is_index_new() {
        "added"
    } else if status.is_index_modified() {
        "modified"
    } else if status.is_index_deleted() {
        "deleted"
    } else if status.is_index_renamed() {
        "renamed"
    } else if status.is_index_typechange() {
        "type_changed"
    } else if status.is_conflicted() {
        "conflicted"
    } else {
        "unchanged"
    }
}

fn worktree_status(status: git2::Status) -> &'static str {
    if status.is_wt_new() {
        "untracked"
    } else if status.is_wt_modified() {
        "modified"
    } else if status.is_wt_deleted() {
        "deleted"
    } else if status.is_wt_renamed() {
        "renamed"
    } else if status.is_wt_typechange() {
        "type_changed"
    } else if status.is_conflicted() {
        "conflicted"
    } else {
        "unchanged"
    }
}

fn diff(
    repository: &Repository,
    arguments: GitDiffArguments,
) -> Result<DiffResult, LocalGitFailure> {
    let mut options = DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .ignore_submodules(true);
    let diff = match arguments {
        GitDiffArguments::Worktree => {
            let tree = repository
                .head()
                .ok()
                .and_then(|head| head.peel_to_tree().ok());
            repository.diff_tree_to_workdir_with_index(tree.as_ref(), Some(&mut options))
        }
        GitDiffArguments::Revisions { base, head } => {
            let base_tree = repository
                .revparse_single(&base)
                .and_then(|object| object.peel_to_tree())
                .map_err(|_| LocalGitFailure::Operation)?;
            let head_tree = repository
                .revparse_single(&head)
                .and_then(|object| object.peel_to_tree())
                .map_err(|_| LocalGitFailure::Operation)?;
            repository.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut options))
        }
    }
    .map_err(|_| LocalGitFailure::Operation)?;
    let mut bytes = Vec::new();
    let mut truncated = false;
    let printed = diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        let prefix = match line.origin() {
            '+' | '-' | ' ' => Some(line.origin() as u8),
            _ => None,
        };
        let content = line.content();
        let remaining = MAX_DIFF_BYTES.saturating_sub(bytes.len());
        if prefix.is_some_and(|_| remaining > 0) {
            bytes.push(prefix.unwrap_or_default());
        }
        let remaining = MAX_DIFF_BYTES.saturating_sub(bytes.len());
        if content.len() <= remaining {
            bytes.extend_from_slice(content);
        } else {
            bytes.extend_from_slice(&content[..remaining]);
            truncated = true;
        }
        !truncated
    });
    if printed.is_err_and(|error| !truncated || error.code() != ErrorCode::User) {
        return Err(LocalGitFailure::Operation);
    }
    Ok(DiffResult {
        patch: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}

fn log(repository: &Repository, arguments: GitLogArguments) -> Result<LogResult, LocalGitFailure> {
    let start = repository
        .revparse_single(&arguments.revision)
        .and_then(|object| object.peel_to_commit())
        .map_err(|_| LocalGitFailure::Operation)?
        .id();
    let mut walk = repository
        .revwalk()
        .map_err(|_| LocalGitFailure::Operation)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
        .map_err(|_| LocalGitFailure::Operation)?;
    walk.push(start).map_err(|_| LocalGitFailure::Operation)?;
    let mut commits = Vec::new();
    let mut truncated = false;
    for next in walk {
        if commits.len() == arguments.max_entries {
            truncated = true;
            break;
        }
        let oid = next.map_err(|_| LocalGitFailure::Operation)?;
        let commit = repository
            .find_commit(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        let author = commit.author();
        let (message, message_truncated) =
            bounded_text(commit.message().unwrap_or(""), MAX_LOG_MESSAGE_BYTES);
        commits.push(LogEntry {
            commit: oid.to_string(),
            author_name: bounded_text(author.name().unwrap_or(""), MAX_LOG_IDENTITY_BYTES).0,
            author_email: bounded_text(author.email().unwrap_or(""), MAX_LOG_IDENTITY_BYTES).0,
            message,
            message_truncated,
        });
    }
    Ok(LogResult { commits, truncated })
}

fn bounded_text(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (value[..boundary].to_owned(), true)
}

fn commit(
    repository: &Repository,
    identity: &GitIdentity,
    arguments: GitCommitArguments,
) -> Result<CommitResult, LocalGitFailure> {
    let mut index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
    let tree_id = index.write_tree().map_err(|_| LocalGitFailure::Operation)?;
    let tree = repository
        .find_tree(tree_id)
        .map_err(|_| LocalGitFailure::Operation)?;
    let signature = identity
        .signature()
        .map_err(|_| LocalGitFailure::Operation)?;
    let parent = match repository.head() {
        Ok(head) => Some(
            head.peel_to_commit()
                .map_err(|_| LocalGitFailure::Operation)?,
        ),
        Err(error)
            if error.code() == ErrorCode::UnbornBranch || error.code() == ErrorCode::NotFound =>
        {
            None
        }
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let parents = parent.iter().collect::<Vec<_>>();
    let oid = repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            &arguments.message,
            &tree,
            &parents,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
    Ok(CommitResult {
        commit: oid.to_string(),
    })
}

fn branch_create(
    repository: &Repository,
    arguments: GitBranchCreateArguments,
) -> Result<BranchResult, LocalGitFailure> {
    let commit = repository
        .revparse_single(&arguments.start)
        .and_then(|object| object.peel_to_commit())
        .map_err(|_| LocalGitFailure::Operation)?;
    let head = commit.id().to_string();
    repository
        .branch(&arguments.name, &commit, false)
        .map_err(|_| LocalGitFailure::Operation)?;
    Ok(BranchResult {
        branch: arguments.name,
        head,
    })
}

/// One fully resolved push handed to the deployment transport.
#[derive(Clone, Eq, PartialEq)]
pub struct GitPushRequest {
    repository_root: PathBuf,
    remote: ConfiguredGitRemote,
    branch: String,
    commit: String,
}

impl GitPushRequest {
    /// Borrows the already-validated direct repository root.
    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    /// Borrows the immutable deployment remote.
    pub const fn remote(&self) -> &ConfiguredGitRemote {
        &self.remote
    }

    /// Borrows the exact local branch shorthand.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Borrows the resolved commit expected at the remote branch.
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// Returns the non-forced exact branch refspec.
    pub fn refspec(&self) -> String {
        format!("refs/heads/{0}:refs/heads/{0}", self.branch)
    }
}

impl fmt::Debug for GitPushRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitPushRequest")
            .field("repository_root", &"[INJECTED]")
            .field("remote", &self.remote)
            .field("branch", &self.branch)
            .field("commit", &self.commit)
            .finish()
    }
}

/// Successful transport acknowledgement for the exact updated commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitPushReceipt {
    commit: String,
}

impl GitPushReceipt {
    /// Constructs a receipt only for a full Git object identifier.
    pub fn try_new(commit: impl Into<String>) -> Result<Self, InvalidGitPushReceipt> {
        let commit = commit.into();
        Oid::from_str(&commit).map_err(|_| InvalidGitPushReceipt)?;
        Ok(Self { commit })
    }

    /// Borrows the remote commit acknowledgement.
    pub fn commit(&self) -> &str {
        &self.commit
    }
}

/// A push transport returned an invalid acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGitPushReceipt;

impl fmt::Display for InvalidGitPushReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Git push receipt")
    }
}

impl Error for InvalidGitPushReceipt {}

/// Physical push outcome classification supplied by the injected transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitPushTransportFailure {
    /// The configured remote definitively rejected the update.
    Rejected,
    /// Dispatch could not begin.
    PreDispatchInfrastructure,
    /// Dispatch may have updated the remote.
    DispatchUnknown,
}

impl fmt::Display for GitPushTransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rejected => "configured Git remote rejected the push",
            Self::PreDispatchInfrastructure => "Git push could not be dispatched",
            Self::DispatchUnknown => "Git push outcome is unknown",
        })
    }
}

impl Error for GitPushTransportFailure {}

/// Deployment-owned push boundary. Implementations receive the fixed remote;
/// the model never supplies or modifies a destination.
pub trait GitPushTransport: Send {
    /// Pushes one non-forced branch refspec and acknowledges the remote commit.
    fn push(&mut self, request: GitPushRequest) -> Result<GitPushReceipt, GitPushTransportFailure>;
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

fn decode_push(
    arguments: &NormalizedToolArguments,
) -> Result<GitPushArguments, InvalidGitArguments> {
    let arguments: GitPushArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidGitArguments)?;
    validate_branch(&arguments.branch)?;
    Ok(arguments)
}

/// One approval-gated push declaration and its configured-remote executor.
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
        let root = WorkspaceRoot::try_new(filesystem, supplied_root)
            .map_err(GitPushToolsConstructionError::Root)?;
        let root_path = fs::canonicalize(supplied_root)
            .map_err(|_| GitPushToolsConstructionError::Repository)?;
        let root_identity = validate_repository_layout(&root_path)
            .map_err(|_| GitPushToolsConstructionError::Repository)?;
        let invalid_detail = ToolExecutionErrorDetail::try_new(INVALID_ARGUMENTS_DETAIL.to_owned())
            .map_err(|_| GitPushToolsConstructionError::ErrorDetail)?;
        let repository_detail =
            ToolExecutionErrorDetail::try_new(REPOSITORY_REJECTED_DETAIL.to_owned())
                .map_err(|_| GitPushToolsConstructionError::ErrorDetail)?;
        let rejected_detail = ToolExecutionErrorDetail::try_new(PUSH_REJECTED_DETAIL.to_owned())
            .map_err(|_| GitPushToolsConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<PushContract>(
            ToolPermissionDefault::Confirm,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => GitPushToolsConstructionError::Name,
            ToolContractCompileError::Schema => GitPushToolsConstructionError::Schema,
        })?;
        let catalog = CompiledToolCatalog::try_new(vec![CompiledTool::new(
            definition,
            GitPushArgumentValidator {
                detail: invalid_detail,
            },
        )])
        .map_err(|_| GitPushToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: GitPushExecutor {
                _root: root,
                root_path,
                root_identity,
                remote,
                transport,
                repository_detail,
                rejected_detail,
            },
        })
    }

    /// Separates catalog and executor composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, GitPushExecutor<Transport>) {
        (self.catalog, self.executor)
    }
}

/// Static push suite or injected-repository construction failure.
#[derive(Debug)]
pub enum GitPushToolsConstructionError {
    /// Static tool name failed compilation.
    Name,
    /// Static schema failed compilation.
    Schema,
    /// Static detail failed construction.
    ErrorDetail,
    /// The fixed catalog unexpectedly contained a duplicate.
    Duplicate,
    /// The injected workspace root was invalid.
    Root(WorkspaceRootError),
    /// The repository layout escaped or did not match the injected root.
    Repository,
}

impl fmt::Display for GitPushToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Git push tool construction failed")
    }
}

impl Error for GitPushToolsConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
            Self::Name | Self::Schema | Self::ErrorDetail | Self::Duplicate | Self::Repository => {
                None
            }
        }
    }
}

/// Approval-gated configured-remote push executor.
#[derive(Debug)]
pub struct GitPushExecutor<Transport> {
    _root: WorkspaceRoot,
    root_path: PathBuf,
    root_identity: RootIdentity,
    remote: ConfiguredGitRemote,
    transport: Transport,
    repository_detail: ToolExecutionErrorDetail,
    rejected_detail: ToolExecutionErrorDetail,
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
            decode_push(invocation.request().arguments()).map_err(|_| push_caller_bug())?;
        let evidence = match self.execute_push(arguments) {
            Ok(result) => ToolExecutorEvidence::CompletedText(result),
            Err(GitPushFailure::Repository) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.repository_detail.clone()),
            },
            Err(GitPushFailure::Rejected) => ToolExecutorEvidence::KnownFailed {
                detail: Some(self.rejected_detail.clone()),
            },
            Err(GitPushFailure::PreDispatchInfrastructure) => {
                return Err(push_infrastructure(false));
            }
            Err(GitPushFailure::DispatchUnknown) => return Err(push_infrastructure(true)),
            Err(GitPushFailure::PostDispatchInvalid) => return Err(push_infrastructure(true)),
        };
        Ok(invocation.bind(evidence))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitPushFailure {
    Repository,
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
    fn execute_push(&mut self, arguments: GitPushArguments) -> Result<String, GitPushFailure> {
        let root_identity =
            validate_repository_layout(&self.root_path).map_err(|_| GitPushFailure::Repository)?;
        if root_identity != self.root_identity {
            return Err(GitPushFailure::Repository);
        }
        let repository = Repository::open_ext(
            &self.root_path,
            RepositoryOpenFlags::NO_SEARCH,
            std::iter::empty::<&Path>(),
        )
        .map_err(|_| GitPushFailure::Repository)?;
        let branch = repository
            .find_branch(&arguments.branch, BranchType::Local)
            .map_err(|_| GitPushFailure::Rejected)?;
        let commit = branch
            .get()
            .peel_to_commit()
            .map_err(|_| GitPushFailure::Rejected)?
            .id()
            .to_string();
        let request = GitPushRequest {
            repository_root: self.root_path.clone(),
            remote: self.remote.clone(),
            branch: arguments.branch.clone(),
            commit: commit.clone(),
        };
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
            remote: self.remote.name.clone(),
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::symlink,
        path::Path,
        sync::{Arc, Mutex},
    };

    use git2::{IndexAddOption, Oid, Repository, Signature};
    use signalbox_application::ToolCatalog;
    use signalbox_domain::{ToolEffectClass, ToolName, ToolPermissionDefault};
    use tempfile::TempDir;

    use super::*;

    const AUTHOR_NAME: &str = "Signalbox Fixer";
    const AUTHOR_EMAIL: &str = "fixer@example.test";
    const INITIAL_MESSAGE: &str = "initial";
    const MODEL_MESSAGE: &str = "subject\n\nmodel data: $(not interpreted)\n";
    const FIX_BRANCH: &str = "agent/fix";
    const FIX_REFSPEC: &str = "refs/heads/agent/fix:refs/heads/agent/fix";
    const TRACKED_PATH: &str = "tracked.txt";
    const INITIAL_CONTENT: &str = "before\n";
    const CHANGED_CONTENT: &str = "after\n";
    const NESTED_TRACKED_DIRECTORY: &str = "removed";
    const NESTED_TRACKED_PATH: &str = "removed/tracked.txt";
    const SUBMODULE_PATH: &str = "dependency";
    const REMOTE_NAME: &str = "origin";
    const REMOTE_URL: &str = "https://github.com/KeenWill/signalbox.git";

    struct Fixture {
        directory: TempDir,
        initial: Oid,
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingPushTransport(Arc<Mutex<Option<GitPushRequest>>>);

    impl RecordingPushTransport {
        fn request(&self) -> GitPushRequest {
            self.0
                .lock()
                .expect("recording transport lock is available")
                .clone()
                .expect("push request was recorded")
        }
    }

    impl GitPushTransport for RecordingPushTransport {
        fn push(
            &mut self,
            request: GitPushRequest,
        ) -> Result<GitPushReceipt, GitPushTransportFailure> {
            let receipt = GitPushReceipt::try_new(request.commit().to_owned())
                .expect("resolved commit forms a receipt");
            *self
                .0
                .lock()
                .expect("recording transport lock is available") = Some(request);
            Ok(receipt)
        }
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary repository root constructs");
            let repository = Repository::init(directory.path()).expect("repository initializes");
            fs::write(directory.path().join(TRACKED_PATH), INITIAL_CONTENT)
                .expect("fixture file writes");
            let initial = commit_all(&repository, INITIAL_MESSAGE);
            Self { directory, initial }
        }

        fn root(&self) -> &Path {
            self.directory.path()
        }

        fn executor(&self) -> LocalGitExecutor<LocalWorkspaceFileSystem> {
            LocalGitTools::try_new(LocalWorkspaceFileSystem, self.root(), identity())
                .expect("local Git suite constructs")
                .into_parts()
                .1
        }
    }

    fn identity() -> GitIdentity {
        GitIdentity::try_new(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture identity is admitted")
    }

    fn commit_all(repository: &Repository, message: &str) -> Oid {
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .expect("fixture stages");
        index.write().expect("fixture index writes");
        commit_index(repository, message)
    }

    fn commit_index(repository: &Repository, message: &str) -> Oid {
        let mut index = repository.index().expect("fixture index opens");
        let tree_id = index.write_tree().expect("fixture tree writes");
        let tree = repository.find_tree(tree_id).expect("fixture tree opens");
        let signature =
            Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture signature constructs");
        let parent = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents = parent.iter().collect::<Vec<_>>();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents,
            )
            .expect("fixture commit writes")
    }

    fn long_status_path() -> PathBuf {
        let segment = "a".repeat(200);
        PathBuf::from(&segment)
            .join(&segment)
            .join(&segment)
            .join(&segment)
            .join(&segment)
            .join(&segment)
            .join(TRACKED_PATH)
    }

    fn install_gitlink(repository: &Repository, path: &str, target: Oid) {
        let mut index = repository.index().expect("fixture index opens");
        let entry = IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: target,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        };
        index.add(&entry).expect("gitlink stages");
        index.write().expect("gitlink index writes");
    }

    fn execute(
        executor: &LocalGitExecutor<LocalWorkspaceFileSystem>,
        operation: LocalOperation,
    ) -> serde_json::Value {
        let encoded = executor
            .execute_operation(operation)
            .expect("operation succeeds");
        serde_json::from_str(&encoded).expect("result is JSON")
    }

    #[test]
    fn catalog_declares_every_local_verb_auto() {
        let fixture = Fixture::new();
        let catalog = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect("suite constructs")
            .into_parts()
            .0;

        let branch_create =
            ToolName::try_new(GIT_BRANCH_CREATE_NAME.to_owned()).expect("fixture name is admitted");
        let branch_switch =
            ToolName::try_new(GIT_BRANCH_SWITCH_NAME.to_owned()).expect("fixture name is admitted");
        let commit =
            ToolName::try_new(GIT_CREATE_COMMIT_NAME.to_owned()).expect("fixture name is admitted");
        let diff = ToolName::try_new(GIT_DIFF_NAME.to_owned()).expect("fixture name is admitted");
        let log = ToolName::try_new(GIT_LOG_NAME.to_owned()).expect("fixture name is admitted");
        let stage = ToolName::try_new(GIT_STAGE_NAME.to_owned()).expect("fixture name is admitted");
        let status =
            ToolName::try_new(GIT_STATUS_NAME.to_owned()).expect("fixture name is admitted");

        assert_eq!(
            catalog
                .definition(&branch_create)
                .expect("definition exists")
                .permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            catalog
                .definition(&branch_switch)
                .expect("definition exists")
                .permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            catalog
                .definition(&commit)
                .expect("definition exists")
                .permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            catalog
                .definition(&diff)
                .expect("definition exists")
                .permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            catalog
                .definition(&log)
                .expect("definition exists")
                .permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            catalog
                .definition(&stage)
                .expect("definition exists")
                .permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            catalog
                .definition(&status)
                .expect("definition exists")
                .permission_default(),
            ToolPermissionDefault::Auto
        );
    }

    #[test]
    fn local_catalog_declares_no_remote_verb() {
        assert_eq!(LOCAL_GIT_TOOL_NAMES.len(), 7);
        assert!(
            !LOCAL_GIT_TOOL_NAMES
                .iter()
                .any(|name| name.contains("push") || name.contains("remote"))
        );
    }

    #[test]
    fn status_observes_real_worktree_state() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        assert_eq!(status["entries"][0]["path"], TRACKED_PATH);
        assert_eq!(status["entries"][0]["worktree"], "modified");
    }

    #[test]
    fn worktree_diff_observes_real_repository_state() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains("-before")
        );
        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains("+after")
        );
    }

    #[test]
    fn revision_diff_uses_real_commits() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let changed = commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();

        let diff = execute(
            &executor,
            LocalOperation::Diff(GitDiffArguments::Revisions {
                base: fixture.initial.to_string(),
                head: changed.to_string(),
            }),
        );
        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains("+after")
        );
    }

    #[test]
    fn log_uses_real_commits() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let changed = commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: changed.to_string(),
                max_entries: 1,
            }),
        );

        assert_eq!(log["commits"][0]["commit"], changed.to_string());
        assert_eq!(log["commits"][0]["message"], MODEL_MESSAGE);
    }

    #[test]
    fn stage_records_real_worktree_content() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let index = repository.index().expect("fixture index opens");
        let entry = index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("staged path exists");
        let blob = repository.find_blob(entry.id).expect("staged blob exists");

        assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
    }

    #[test]
    fn commit_preserves_message_with_injected_identity() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let executor = fixture.executor();

        let result = execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }),
        );
        let oid = Oid::from_str(result["commit"].as_str().expect("commit id is text"))
            .expect("commit id parses");
        let commit = repository.find_commit(oid).expect("created commit exists");

        assert_eq!(commit.message(), Ok(MODEL_MESSAGE));
        assert_eq!(commit.author().name(), Ok(AUTHOR_NAME));
        assert_eq!(commit.author().email(), Ok(AUTHOR_EMAIL));
        assert_eq!(commit.committer().name(), Ok(AUTHOR_NAME));
        assert_eq!(commit.committer().email(), Ok(AUTHOR_EMAIL));
    }

    #[test]
    fn stage_records_deletion_after_tracked_parent_directory_is_removed() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root().join(NESTED_TRACKED_DIRECTORY))
            .expect("nested fixture directory constructs");
        fs::write(fixture.root().join(NESTED_TRACKED_PATH), INITIAL_CONTENT)
            .expect("nested tracked file writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        commit_all(&repository, INITIAL_MESSAGE);
        fs::remove_dir_all(fixture.root().join(NESTED_TRACKED_DIRECTORY))
            .expect("tracked parent directory removes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![NESTED_TRACKED_PATH.to_owned()],
            }),
        );
        let updated_repository =
            Repository::open(fixture.root()).expect("updated fixture repository opens");
        let index = updated_repository
            .index()
            .expect("updated fixture index opens");

        assert!(index.get_path(Path::new(NESTED_TRACKED_PATH), 0).is_none());
    }

    #[test]
    fn branch_create_writes_real_non_forced_reference() {
        let fixture = Fixture::new();
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::BranchCreate(GitBranchCreateArguments {
                name: FIX_BRANCH.to_owned(),
                start: fixture.initial.to_string(),
            }),
        );
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let created = repository
            .find_branch(FIX_BRANCH, BranchType::Local)
            .expect("created branch exists");

        assert_eq!(created.get().target(), Some(fixture.initial));
    }

    #[test]
    fn branch_switch_changes_real_head() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        let executor = fixture.executor();

        let switched = execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }),
        );

        assert_eq!(switched["branch"], FIX_BRANCH);
        assert_eq!(
            repository.head().expect("head exists").shorthand(),
            Ok(FIX_BRANCH)
        );
    }

    #[test]
    fn injected_identity_rejects_signature_delimiters() {
        let invalid_name = GitIdentity::try_new("Bad<Name", AUTHOR_EMAIL);
        let invalid_email = GitIdentity::try_new(AUTHOR_NAME, "bad@example.test>");

        assert_eq!(invalid_name, Err(InvalidGitIdentity));
        assert_eq!(invalid_email, Err(InvalidGitIdentity));
    }

    #[test]
    fn status_reports_unborn_symbolic_branch() {
        let root = tempfile::tempdir().expect("temporary repository root constructs");
        let repository = Repository::init(root.path()).expect("repository initializes");
        let branch = repository
            .find_reference("HEAD")
            .expect("symbolic HEAD exists")
            .symbolic_target()
            .expect("symbolic target lookup succeeds")
            .expect("HEAD has a symbolic target")
            .strip_prefix("refs/heads/")
            .expect("HEAD targets a local branch")
            .to_owned();
        let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, root.path(), identity())
            .expect("local Git suite constructs")
            .into_parts()
            .1;

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["branch"], branch);
        assert!(status["head"].is_null());
    }

    #[test]
    fn status_marks_truncated_path_output() {
        let fixture = Fixture::new();
        let path = long_status_path();
        fs::create_dir_all(
            fixture
                .root()
                .join(&path)
                .parent()
                .expect("long path has a parent"),
        )
        .expect("long fixture directory constructs");
        fs::write(fixture.root().join(&path), CHANGED_CONTENT).expect("long fixture file writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["truncated"], true);
        assert_eq!(
            status["entries"][0]["path"]
                .as_str()
                .expect("status path is text")
                .len(),
            MAX_STATUS_PATH_BYTES
        );
    }

    #[test]
    fn log_peels_annotated_tag_to_commit() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture commit exists");
        let signature =
            Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture signature constructs");
        repository
            .tag("release", initial.as_object(), &signature, "release", false)
            .expect("annotated tag creates");
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: "refs/tags/release".to_owned(),
                max_entries: 1,
            }),
        );

        assert_eq!(log["commits"][0]["commit"], fixture.initial.to_string());
    }

    #[test]
    fn worktree_diff_ignores_submodule_repository_outside_root() {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().expect("outside repository root constructs");
        let outside_repository =
            Repository::init(outside.path()).expect("outside repository initializes");
        fs::write(outside.path().join(TRACKED_PATH), INITIAL_CONTENT)
            .expect("outside fixture file writes");
        let outside_commit = commit_all(&outside_repository, INITIAL_MESSAGE);
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        install_gitlink(&repository, SUBMODULE_PATH, outside_commit);
        commit_index(&repository, "track dependency");
        fs::create_dir(fixture.root().join(SUBMODULE_PATH))
            .expect("submodule fixture directory constructs");
        fs::write(
            fixture.root().join(SUBMODULE_PATH).join(".git"),
            format!("gitdir: {}", outside.path().join(".git").display()),
        )
        .expect("submodule gitdir indirection writes");
        fs::write(outside.path().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("outside fixture change writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert_eq!(diff["patch"], "");
        assert_eq!(diff["truncated"], false);
    }

    #[test]
    fn injected_root_symlink_is_rejected() {
        let fixture = Fixture::new();
        let parent = tempfile::tempdir().expect("link parent constructs");
        let linked_root = parent.path().join("linked");
        symlink(fixture.root(), &linked_root).expect("root symlink constructs");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, linked_root, identity())
            .expect_err("symlink root rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Root(_)));
    }

    #[test]
    fn gitdir_file_pointing_outside_root_is_rejected() {
        let root = tempfile::tempdir().expect("workspace root constructs");
        let outside = tempfile::tempdir().expect("outside repository constructs");
        Repository::init_bare(outside.path()).expect("outside repository initializes");
        fs::write(
            root.path().join(".git"),
            format!("gitdir: {}", outside.path().display()),
        )
        .expect("gitdir indirection writes");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, root.path(), identity())
            .expect_err("external gitdir rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn configured_external_worktree_is_rejected() {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().expect("outside worktree constructs");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        repository
            .config()
            .expect("config opens")
            .set_str(
                "core.worktree",
                outside.path().to_str().expect("temporary path is UTF-8"),
            )
            .expect("worktree override writes");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("external worktree rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn common_git_directory_indirection_is_rejected() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(".git/commondir"), "../outside")
            .expect("common-directory indirection writes");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("common-directory indirection rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn administrative_config_symlink_is_rejected() {
        let fixture = Fixture::new();
        let outside = tempfile::NamedTempFile::new().expect("outside config constructs");
        fs::remove_file(fixture.root().join(".git/config"))
            .expect("repository config removes for fixture");
        symlink(outside.path(), fixture.root().join(".git/config"))
            .expect("administrative symlink constructs");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("administrative symlink rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn executor_rejects_replacement_at_the_injected_root_path() {
        let parent = tempfile::tempdir().expect("workspace parent constructs");
        let root = parent.path().join("workspace");
        fs::create_dir(&root).expect("workspace root constructs");
        let original = Repository::init(&root).expect("original repository initializes");
        fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("original file writes");
        commit_all(&original, INITIAL_MESSAGE);
        let executor = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
            .expect("suite constructs")
            .into_parts()
            .1;
        let retired = parent.path().join("retired");
        fs::rename(&root, &retired).expect("original workspace retires");
        fs::create_dir(&root).expect("replacement root constructs");
        let replacement = Repository::init(&root).expect("replacement repository initializes");
        fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT).expect("replacement file writes");
        commit_all(&replacement, INITIAL_MESSAGE);

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("replacement root rejects");

        assert_eq!(failure, LocalGitFailure::Repository);
    }

    #[test]
    fn stage_rejects_intermediate_symlink_escape() {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().expect("outside directory constructs");
        fs::write(outside.path().join("outside.txt"), CHANGED_CONTENT)
            .expect("outside file writes");
        symlink(outside.path(), fixture.root().join("escape"))
            .expect("escaping symlink constructs");
        let executor = fixture.executor();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");

        let failure = executor
            .stage(
                &repository,
                GitStageArguments {
                    paths: vec!["escape/outside.txt".to_owned()],
                },
            )
            .expect_err("escaping path rejects");

        assert_eq!(failure, LocalGitFailure::Path);
    }

    #[test]
    fn stage_argument_rejects_parent_traversal_before_execution() {
        let arguments = NormalizedToolArguments::try_from_provider_text(
            serde_json::json!({"paths": ["../outside.txt"]}).to_string(),
        )
        .expect("JSON arguments normalize");

        assert!(decode_operation(LocalToolKind::Stage, &arguments).is_err());
    }

    #[test]
    fn read_verbs_are_effect_free() {
        assert_eq!(LocalToolKind::Status.effect(), ToolEffectClass::EffectFree);
        assert_eq!(LocalToolKind::Diff.effect(), ToolEffectClass::EffectFree);
        assert_eq!(LocalToolKind::Log.effect(), ToolEffectClass::EffectFree);
    }

    #[test]
    fn local_write_verbs_are_effecting() {
        assert_eq!(
            LocalToolKind::Stage.effect(),
            ToolEffectClass::ExternalEffect
        );
        assert_eq!(
            LocalToolKind::Commit.effect(),
            ToolEffectClass::ExternalEffect
        );
        assert_eq!(
            LocalToolKind::BranchCreate.effect(),
            ToolEffectClass::ExternalEffect
        );
        assert_eq!(
            LocalToolKind::BranchSwitch.effect(),
            ToolEffectClass::ExternalEffect
        );
    }

    #[test]
    fn push_contract_requires_confirmation() {
        let fixture = Fixture::new();
        let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
            .expect("configured remote is admitted");
        let catalog = GitPushTools::try_new(
            &LocalWorkspaceFileSystem,
            fixture.root(),
            remote,
            RecordingPushTransport::default(),
        )
        .expect("push suite constructs")
        .into_parts()
        .0;
        let name = ToolName::try_new(GIT_PUSH_CONFIGURED_NAME.to_owned())
            .expect("fixture name is admitted");
        let definition = catalog.definition(&name).expect("push definition exists");

        assert_eq!(
            definition.permission_default(),
            ToolPermissionDefault::Confirm
        );
        assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
    }

    #[test]
    fn push_contract_has_no_destination_argument() {
        let fixture = Fixture::new();
        let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
            .expect("configured remote is admitted");
        let catalog = GitPushTools::try_new(
            &LocalWorkspaceFileSystem,
            fixture.root(),
            remote,
            RecordingPushTransport::default(),
        )
        .expect("push suite constructs")
        .into_parts()
        .0;
        let name = ToolName::try_new(GIT_PUSH_CONFIGURED_NAME.to_owned())
            .expect("fixture name is admitted");
        let definition = catalog.definition(&name).expect("push definition exists");
        let schema: serde_json::Value =
            serde_json::from_str(definition.input_schema().as_str()).expect("push schema is JSON");
        let injected_destination = NormalizedToolArguments::try_from_provider_text(
            serde_json::json!({"branch": FIX_BRANCH, "remote": REMOTE_URL}).to_string(),
        )
        .expect("provider JSON normalizes");

        assert_eq!(schema["required"], serde_json::json!(["branch"]));
        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        assert!(decode_push(&injected_destination).is_err());
    }

    #[test]
    fn push_resolves_real_branch_into_configured_synthetic_transport_request() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture push branch creates");
        let branch = FIX_BRANCH.to_owned();
        let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
            .expect("configured remote is admitted");
        let transport = RecordingPushTransport::default();
        let mut executor = GitPushTools::try_new(
            &LocalWorkspaceFileSystem,
            fixture.root(),
            remote,
            transport.clone(),
        )
        .expect("push suite constructs")
        .into_parts()
        .1;

        let encoded = executor
            .execute_push(GitPushArguments {
                branch: branch.clone(),
            })
            .expect("synthetic push succeeds");
        let result: serde_json::Value =
            serde_json::from_str(&encoded).expect("push result is JSON");
        let request = transport.request();

        assert_eq!(request.repository_root(), fixture.root());
        assert_eq!(request.remote().name(), REMOTE_NAME);
        assert_eq!(request.remote().url(), REMOTE_URL);
        assert_eq!(request.branch(), branch);
        assert_eq!(request.commit(), fixture.initial.to_string());
        assert_eq!(request.refspec(), FIX_REFSPEC);
        assert_eq!(result["remote"], REMOTE_NAME);
        assert_eq!(result["branch"], request.branch());
        assert_eq!(result["commit"], fixture.initial.to_string());
    }
}
