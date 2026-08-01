//! Typed repository-local Git tools over an injected workspace root.
//!
//! Repository discovery and linked worktrees are deliberately unsupported. A
//! suite binds one direct main worktree whose .git directory is inside the
//! injected root. The local family has no remote operation.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{Read, Seek, Write},
    os::fd::{AsRawFd, OwnedFd},
    os::unix::ffi::{OsStrExt, OsStringExt},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use git2::{
    BranchType, Config, Delta, DiffFindOptions, DiffFormat, DiffOptions, ErrorCode, Index,
    IndexEntry, IndexTime, Patch, Repository, RepositoryOpenFlags, RepositoryState, Signature,
    build::CheckoutBuilder,
};
use rustix::fs::{
    AtFlags, CWD, Mode, OFlags, RenameFlags, mkdirat, openat, renameat_with, unlinkat,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
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
const MAX_STAGE_FILE_BYTES: usize = MAX_OBJECT_BYTES;
const MAX_STAGE_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_WORKTREE_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPOSITORY_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_PACKED_REFS_BYTES: usize = 1024 * 1024;
const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;
const MAX_INDEX_ENTRIES: usize = MAX_WORKTREE_INSPECTIONS;
const MAX_OBJECT_BYTES: usize = 1024 * 1024;
const MAX_WORKTREE_INSPECTIONS: usize = 4096;
const MAX_MERGE_PARENTS: usize = 64;
const MAX_MERGE_HEAD_BYTES: usize = MAX_MERGE_PARENTS * 41;
const MAX_WORKTREE_PATH_BYTES: usize = 4 * 1024 * 1024;
const MAX_STATUS_ENTRIES: usize = 128;
const MAX_STATUS_PATH_BYTES: usize = 1024;
const MAX_LOG_ENTRIES: usize = 50;
const DEFAULT_LOG_ENTRIES: usize = 25;
const MAX_LOG_IDENTITY_BYTES: usize = 256;
const MAX_LOG_MESSAGE_BYTES: usize = 2048;
const MAX_DIFF_BYTES: usize = 128 * 1024;
const GITLINK_MODE: u32 = 0o160000;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded Git tool arguments";
const REPOSITORY_REJECTED_DETAIL: &str = "injected Git repository was rejected";
const PATH_REJECTED_DETAIL: &str = "Git path was rejected by the workspace boundary";
const OPERATION_FAILED_DETAIL: &str = "local Git operation failed";

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
    let exact_object = git2::Oid::from_str(value).is_ok();
    let exact_reference =
        value == "HEAD" || (value.starts_with("refs/") && git2::Reference::is_valid_name(value));
    (value.len() <= MAX_REVISION_BYTES && (exact_object || exact_reference))
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

fn parse_gitdir_marker(directory: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let marker = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let target = marker.strip_prefix(b"gitdir: ")?;
    if target.is_empty() || target.contains(&0) {
        return None;
    }
    let target = Path::new(std::ffi::OsStr::from_bytes(target));
    if target.is_absolute() {
        return None;
    }
    let mut resolved = directory.to_owned();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => resolved.push(component),
            Component::ParentDir if resolved.pop() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (resolved.as_os_str().as_bytes().len() <= 4096).then_some(resolved)
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

fn detail(value: &str) -> Result<ToolExecutionErrorDetail, LocalGitToolsConstructionError> {
    ToolExecutionErrorDetail::try_new(value.to_owned())
        .map_err(|_| LocalGitToolsConstructionError::ErrorDetail)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RepositoryIdentity {
    root: FileIdentity,
    git_directory: FileIdentity,
}

struct PinnedRepository {
    _root: fs::File,
    git_directory: fs::File,
    _config: fs::File,
    repository: Mutex<Repository>,
}

impl fmt::Debug for PinnedRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedRepository")
            .finish_non_exhaustive()
    }
}

impl PinnedRepository {
    fn open(
        root_path: &Path,
        expected: RepositoryIdentity,
    ) -> Result<Self, LocalGitToolsConstructionError> {
        let root =
            fs::File::open(root_path).map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let git_directory = fs::File::open(root_path.join(".git"))
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        let config = open_repository_config(&root_path.join(".git/config"))?;
        let observed = RepositoryIdentity {
            root: file_identity(
                &root
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
            git_directory: file_identity(
                &git_directory
                    .metadata()
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?,
            ),
        };
        if observed != expected {
            return Err(LocalGitToolsConstructionError::Repository);
        }
        let repository = open_pinned_repository(&root, &git_directory, &config)
            .map_err(|_| LocalGitToolsConstructionError::Repository)?;
        Ok(Self {
            _root: root,
            git_directory,
            _config: config,
            repository: Mutex::new(repository),
        })
    }

    fn repository(&self) -> Result<std::sync::MutexGuard<'_, Repository>, LocalGitFailure> {
        self.repository
            .lock()
            .map_err(|_| LocalGitFailure::Repository)
    }

    fn git_path(&self, path: &str) -> PathBuf {
        descriptor_path(&self.git_directory).join(path)
    }
}

fn open_pinned_repository(
    root: &fs::File,
    git_directory: &fs::File,
    config: &fs::File,
) -> Result<Repository, git2::Error> {
    let root_path = descriptor_path(root);
    let git_directory_path = descriptor_path(git_directory);
    let repository = Repository::open_ext(
        &git_directory_path,
        RepositoryOpenFlags::BARE | RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&Path>(),
    )?;
    repository.set_workdir(&root_path, false)?;
    let config = Config::open(&descriptor_path(config))?;
    repository.set_config(&config)?;
    Ok(repository)
}

fn descriptor_path(file: &fs::File) -> PathBuf {
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn validate_repository_layout(
    root: &Path,
) -> Result<RepositoryIdentity, LocalGitToolsConstructionError> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let root_identity = file_identity(&root_metadata);
    let dot_git = root.join(".git");
    let metadata =
        fs::symlink_metadata(&dot_git).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let git_directory_identity = file_identity(&metadata);
    if dot_git.join("commondir").exists() || dot_git.join("objects/info/alternates").exists() {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    reject_administrative_symlinks(&dot_git)?;
    reject_escaping_config(&dot_git.join("config"))?;
    Ok(RepositoryIdentity {
        root: root_identity,
        git_directory: git_directory_identity,
    })
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
            } else if !file_type.is_file() {
                return Err(LocalGitToolsConstructionError::Repository);
            } else {
                let relative = entry
                    .path()
                    .strip_prefix(directory)
                    .map_err(|_| LocalGitToolsConstructionError::Repository)?
                    .to_owned();
                let limit = if relative == Path::new("HEAD") || relative.starts_with("refs") {
                    Some(MAX_REVISION_BYTES)
                } else if relative == Path::new("packed-refs") {
                    Some(MAX_PACKED_REFS_BYTES)
                } else {
                    None
                };
                if limit.is_some_and(|limit| {
                    entry
                        .metadata()
                        .map_or(true, |metadata| metadata.len() > limit as u64)
                }) {
                    return Err(LocalGitToolsConstructionError::Repository);
                }
            }
        }
    }
    Ok(())
}

fn reject_escaping_config(config_path: &Path) -> Result<(), LocalGitToolsConstructionError> {
    open_repository_config(config_path).map(drop)
}

fn open_repository_config(config_path: &Path) -> Result<fs::File, LocalGitToolsConstructionError> {
    let descriptor = openat(
        CWD,
        config_path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut file = fs::File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if !metadata.is_file() || metadata.len() > MAX_REPOSITORY_CONFIG_BYTES as u64 {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_REPOSITORY_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitToolsConstructionError::Repository)?;
    if bytes.len() > MAX_REPOSITORY_CONFIG_BYTES {
        return Err(LocalGitToolsConstructionError::Repository);
    }
    let config =
        String::from_utf8(bytes).map_err(|_| LocalGitToolsConstructionError::Repository)?;
    let mut section = "";
    for line in config.lines() {
        let normalized = line.trim().to_ascii_lowercase();
        if normalized.starts_with('[') {
            section = if normalized.starts_with("[core]") {
                "core"
            } else if normalized.starts_with("[extensions]") {
                "extensions"
            } else if normalized.starts_with("[filter ") || normalized.starts_with("[include") {
                return Err(LocalGitToolsConstructionError::Repository);
            } else {
                ""
            };
            continue;
        }
        if section == "core" {
            let file_valued = normalized.split_once('=').is_some_and(|(key, _)| {
                matches!(
                    key.trim(),
                    "worktree" | "excludesfile" | "attributesfile" | "hookspath" | "fsmonitor"
                )
            });
            if file_valued {
                return Err(LocalGitToolsConstructionError::Repository);
            }
        }
        if section == "extensions"
            && normalized
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "worktreeconfig")
        {
            return Err(LocalGitToolsConstructionError::Repository);
        }
    }
    Ok(file)
}

/// Executor for local Git operations only.
#[derive(Debug)]
pub struct LocalGitExecutor<FileSystem> {
    filesystem: FileSystem,
    root: WorkspaceRoot,
    root_path: PathBuf,
    repository_identity: RepositoryIdentity,
    repository_authority: PinnedRepository,
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
    branch_truncated: bool,
    head: Option<String>,
    entries: Vec<StatusEntry>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct StatusEntry {
    path: String,
    previous_path: Option<String>,
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
    author_name_truncated: bool,
    author_email: String,
    author_email_truncated: bool,
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
    state_cleaned: bool,
}

#[derive(Debug, Serialize)]
struct BranchResult {
    branch: String,
    head: String,
}

impl<FileSystem: WorkspaceFileSystem> LocalGitExecutor<FileSystem> {
    fn execute_operation(&self, operation: LocalOperation) -> Result<String, LocalGitFailure> {
        self.validate_current_repository()?;
        let mut repository = self.repository_authority.repository()?;
        let result = match operation {
            LocalOperation::Status => {
                let _index_lock = self.bind_locked_index(&repository)?;
                let untracked = self.discover_untracked_paths(&repository)?;
                LocalGitResult::Status(status(
                    &repository,
                    &self.filesystem,
                    &self.root,
                    untracked,
                )?)
            }
            LocalOperation::Diff(arguments) => {
                let index_lock = if matches!(arguments, GitDiffArguments::Worktree) {
                    Some(self.bind_locked_index(&repository)?)
                } else {
                    None
                };
                let untracked = if index_lock.is_some() {
                    self.discover_untracked_paths(&repository)?
                } else {
                    Vec::new()
                };
                LocalGitResult::Diff(diff(
                    &repository,
                    arguments,
                    &self.filesystem,
                    &self.root,
                    untracked,
                )?)
            }
            LocalOperation::Log(arguments) => LocalGitResult::Log(log(&repository, arguments)?),
            LocalOperation::Stage(arguments) => {
                let result = LocalGitResult::Stage(self.stage(&repository, arguments)?);
                return encode_result(&result);
            }
            LocalOperation::Commit(arguments) => {
                let result = LocalGitResult::Commit(commit(
                    &mut repository,
                    &self.identity,
                    arguments,
                    &self.repository_authority.git_path("index"),
                    &self.repository_authority.git_path("index.lock"),
                )?);
                return encode_result(&result);
            }
            LocalOperation::BranchCreate(arguments) => {
                let result = LocalGitResult::BranchCreate(branch_create(
                    &repository,
                    &self.repository_authority,
                    arguments,
                )?);
                return encode_result(&result);
            }
            LocalOperation::BranchSwitch(arguments) => {
                let result =
                    LocalGitResult::BranchSwitch(self.branch_switch(&repository, arguments)?);
                return encode_result(&result);
            }
        };
        self.validate_current_repository()?;
        encode_result(&result)
    }

    fn stage(
        &self,
        repository: &Repository,
        arguments: GitStageArguments,
    ) -> Result<StageResult, LocalGitFailure> {
        let index_path = self.repository_authority.git_path("index");
        let index_lock_path = self.repository_authority.git_path("index.lock");
        let (index_lock, mut index) = IndexLock::acquire(&index_path, &index_lock_path)?;
        let filemode = repository_filemode(repository)?;
        let mut planned = Vec::with_capacity(arguments.paths.len());
        let mut total_bytes = 0_usize;
        for supplied in &arguments.paths {
            let path = checked_relative_path(supplied).map_err(|_| LocalGitFailure::Path)?;
            match self
                .filesystem
                .read_file_prefix(&self.root, &path, MAX_STAGE_FILE_BYTES)
            {
                Ok(read) if read.truncated => return Err(LocalGitFailure::Operation),
                Ok(read) => {
                    total_bytes = total_bytes
                        .checked_add(read.bytes.len())
                        .filter(|total| *total <= MAX_STAGE_TOTAL_BYTES)
                        .ok_or(LocalGitFailure::Operation)?;
                    let observed_mode = if read.mode & 0o111 == 0 {
                        0o100644
                    } else {
                        0o100755
                    };
                    let mode = if filemode {
                        observed_mode
                    } else {
                        index
                            .get_path(&path, 0)
                            .map_or(observed_mode, |entry| entry.mode)
                    };
                    planned.push(PlannedStage::Add {
                        supplied: supplied.clone(),
                        bytes: read.bytes,
                        mode,
                    });
                }
                Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
                Err(WorkspaceResolveError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    if let Some(indexed) = index.get_path(&path, 0) {
                        if indexed.mode == GITLINK_MODE {
                            return Err(LocalGitFailure::Operation);
                        }
                        planned.push(PlannedStage::Remove { path });
                    } else if index_path_is_conflicted(&index, &path) {
                        planned.push(PlannedStage::RemoveConflict { path });
                    } else {
                        return Err(LocalGitFailure::Operation);
                    }
                }
                Err(WorkspaceResolveError::Io { .. }) if index.get_path(&path, 0).is_some() => {
                    let indexed = index.get_path(&path, 0).ok_or(LocalGitFailure::Operation)?;
                    match self.filesystem.entry_kind(&self.root, &path) {
                        Ok(WorkspaceEntryKind::Directory) if indexed.mode != GITLINK_MODE => {
                            planned.push(PlannedStage::Remove { path });
                        }
                        Ok(WorkspaceEntryKind::Directory) => {
                            return Err(LocalGitFailure::Operation);
                        }
                        Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other)
                        | Err(WorkspaceResolveError::Rejected(_)) => {
                            return Err(LocalGitFailure::Path);
                        }
                        Ok(WorkspaceEntryKind::File) | Err(WorkspaceResolveError::Io { .. }) => {
                            return Err(LocalGitFailure::Operation);
                        }
                    }
                }
                Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
            }
        }
        self.validate_current_repository()?;
        for operation in planned {
            match operation {
                PlannedStage::Add {
                    supplied,
                    bytes,
                    mode,
                } => {
                    // Path-aware blob writers reopen attribute files; insert
                    // the already-bounded descriptor bytes without a second
                    // model-writable pathname lookup.
                    let oid = repository
                        .blob(&bytes)
                        .map_err(|_| LocalGitFailure::Operation)?;
                    let entry = IndexEntry {
                        ctime: IndexTime::new(0, 0),
                        mtime: IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode,
                        uid: 0,
                        gid: 0,
                        file_size: u32::try_from(bytes.len())
                            .map_err(|_| LocalGitFailure::Operation)?,
                        id: oid,
                        flags: 0,
                        flags_extended: 0,
                        path: supplied.as_bytes().to_vec(),
                    };
                    if index_path_is_conflicted(&index, Path::new(&supplied)) {
                        index
                            .conflict_remove(Path::new(&supplied))
                            .map_err(|_| LocalGitFailure::Operation)?;
                    }
                    index.add(&entry).map_err(|_| LocalGitFailure::Operation)?;
                }
                PlannedStage::Remove { path } => index
                    .remove_path(&path)
                    .map_err(|_| LocalGitFailure::Operation)?,
                PlannedStage::RemoveConflict { path } => index
                    .conflict_remove(&path)
                    .map_err(|_| LocalGitFailure::Operation)?,
            }
        }
        index_lock.write(&index)?;
        drop(index);
        index_lock.commit()?;
        self.validate_current_repository()?;
        Ok(StageResult {
            staged_paths: arguments.paths.len(),
        })
    }

    fn validate_current_repository(&self) -> Result<(), LocalGitFailure> {
        let observed =
            validate_repository_layout(&self.root_path).map_err(|_| LocalGitFailure::Repository)?;
        if observed == self.repository_identity {
            Ok(())
        } else {
            Err(LocalGitFailure::Repository)
        }
    }

    fn bind_locked_index(&self, repository: &Repository) -> Result<IndexLock, LocalGitFailure> {
        let (index_lock, mut index) = IndexLock::acquire(
            &self.repository_authority.git_path("index"),
            &self.repository_authority.git_path("index.lock"),
        )?;
        repository
            .set_index(&mut index)
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok(index_lock)
    }

    fn discover_untracked_paths(
        &self,
        repository: &Repository,
    ) -> Result<Vec<PathBuf>, LocalGitFailure> {
        let index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
        if index.len() > MAX_WORKTREE_INSPECTIONS {
            return Err(LocalGitFailure::Operation);
        }
        let tracked_directories = tracked_directories(&index);
        let mut pending = vec![PathBuf::from(".")];
        let mut untracked = Vec::new();
        let mut inspected = 0_usize;
        let mut inspected_path_bytes = 0_usize;
        while let Some(directory) = pending.pop() {
            if !tracked_directories.contains(&directory)
                && self.is_embedded_repository(&directory)?
            {
                untracked.push(directory);
                continue;
            }
            let remaining_entries = MAX_WORKTREE_INSPECTIONS.saturating_sub(inspected);
            let remaining_path_bytes = MAX_WORKTREE_PATH_BYTES.saturating_sub(inspected_path_bytes);
            let requested_entries = remaining_entries.saturating_add(1);
            let read = self
                .filesystem
                .read_directory(
                    &self.root,
                    &directory,
                    requested_entries,
                    requested_entries,
                    remaining_path_bytes,
                )
                .map_err(|error| match error {
                    WorkspaceResolveError::Rejected(_) => LocalGitFailure::Path,
                    WorkspaceResolveError::Io { .. } => LocalGitFailure::Operation,
                })?;
            if read.truncated
                || read.inspected_entries > remaining_entries
                || read.inspected_path_bytes > remaining_path_bytes
            {
                return Err(LocalGitFailure::Operation);
            }
            inspected = inspected.saturating_add(read.inspected_entries);
            inspected_path_bytes = inspected_path_bytes.saturating_add(read.inspected_path_bytes);
            for entry in read.entries {
                if entry.path == Path::new(".git") {
                    continue;
                }
                match entry.kind {
                    WorkspaceEntryKind::Directory => {
                        if index
                            .get_path(&entry.path, 0)
                            .is_none_or(|indexed| indexed.mode != GITLINK_MODE)
                        {
                            pending.push(entry.path);
                        }
                    }
                    WorkspaceEntryKind::File
                    | WorkspaceEntryKind::Symlink
                    | WorkspaceEntryKind::Other => {
                        if index.get_path(&entry.path, 0).is_none() {
                            untracked.push(entry.path);
                        }
                    }
                }
            }
        }
        Ok(untracked)
    }

    fn is_embedded_repository(&self, directory: &Path) -> Result<bool, LocalGitFailure> {
        if directory == Path::new(".") {
            return Ok(false);
        }
        let dot_git = directory.join(".git");
        match self.filesystem.entry_kind(&self.root, &dot_git) {
            Ok(WorkspaceEntryKind::Directory) => self.is_repository_directory(&dot_git),
            Ok(WorkspaceEntryKind::File) => {
                let marker = self
                    .filesystem
                    .read_file_prefix(&self.root, &dot_git, MAX_REVISION_BYTES)
                    .map_err(|error| match error {
                        WorkspaceResolveError::Rejected(_) => LocalGitFailure::Path,
                        WorkspaceResolveError::Io { .. } => LocalGitFailure::Operation,
                    })?;
                if marker.truncated {
                    return Ok(false);
                }
                let target = parse_gitdir_marker(directory, &marker.bytes);
                target.map_or(Ok(false), |target| self.is_repository_directory(&target))
            }
            Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other) => Ok(false),
            Err(WorkspaceResolveError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(false)
            }
            Err(WorkspaceResolveError::Rejected(_)) => Err(LocalGitFailure::Path),
            Err(WorkspaceResolveError::Io { .. }) => Err(LocalGitFailure::Operation),
        }
    }

    fn is_repository_directory(&self, directory: &Path) -> Result<bool, LocalGitFailure> {
        let head = self
            .filesystem
            .entry_kind(&self.root, &directory.join("HEAD"));
        let objects = self
            .filesystem
            .entry_kind(&self.root, &directory.join("objects"));
        match (head, objects) {
            (Ok(WorkspaceEntryKind::File), Ok(WorkspaceEntryKind::Directory)) => Ok(true),
            (Err(WorkspaceResolveError::Rejected(_)), _)
            | (_, Err(WorkspaceResolveError::Rejected(_))) => Err(LocalGitFailure::Path),
            _ => Ok(false),
        }
    }

    fn branch_switch(
        &self,
        repository: &Repository,
        arguments: GitBranchSwitchArguments,
    ) -> Result<BranchResult, LocalGitFailure> {
        let index_lock = self.bind_locked_index(repository)?;
        if repository.state() != RepositoryState::Clean {
            return Err(LocalGitFailure::Operation);
        }
        let branch = repository
            .find_branch(&arguments.name, BranchType::Local)
            .map_err(|_| LocalGitFailure::Operation)?;
        let reference = branch.into_reference();
        let reference_name = reference
            .name()
            .map_err(|_| LocalGitFailure::Operation)?
            .to_owned();
        drop(reference);
        let (reference_chain, _target) = resolve_reference_chain(repository, &reference_name)?;
        let signature = self
            .identity
            .signature()
            .map_err(|_| LocalGitFailure::Operation)?;
        let mut transaction = repository
            .transaction()
            .map_err(|_| LocalGitFailure::Operation)?;
        transaction
            .lock_ref("HEAD")
            .map_err(|_| LocalGitFailure::Operation)?;
        for locked_reference in &reference_chain {
            transaction
                .lock_ref(locked_reference)
                .map_err(|_| LocalGitFailure::Operation)?;
        }
        let (locked_chain, target) = resolve_reference_chain(repository, &reference_name)?;
        if locked_chain != reference_chain {
            return Err(LocalGitFailure::Operation);
        }
        let target = target.ok_or(LocalGitFailure::Operation)?;
        let target_commit = find_bounded_commit(repository, target)?;
        let current_tree = worktree_head_tree(repository)?;
        let target_tree = find_bounded_tree(repository, target_commit.tree_id())?;
        if let Some(current_tree) = &current_tree {
            validate_tree_discovery(repository, current_tree)?;
        }
        validate_tree_discovery(repository, &target_tree)?;
        let current_index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
        if current_index.has_conflicts() {
            return Err(LocalGitFailure::Operation);
        }
        validate_index_objects(repository, &current_index)?;
        let staged = repository
            .diff_tree_to_index(current_tree.as_ref(), Some(&current_index), None)
            .map_err(|_| LocalGitFailure::Operation)?;
        let staged_paths = staged
            .deltas()
            .flat_map(|delta| {
                [delta.old_file().path(), delta.new_file().path()]
                    .into_iter()
                    .flatten()
                    .map(Path::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        let staged_entries = staged_paths
            .into_iter()
            .map(|path| {
                let entry = current_index
                    .get_path(&path, 0)
                    .map(|entry| clone_index_entry(&entry));
                (path, entry)
            })
            .collect::<Vec<_>>();
        let changes = repository
            .diff_tree_to_tree(current_tree.as_ref(), Some(&target_tree), None)
            .map_err(|_| LocalGitFailure::Operation)?;
        for delta in changes.deltas() {
            if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                validate_checkout_path(&self.filesystem, &self.root, path)?;
            }
        }
        let mut checkout = CheckoutBuilder::new();
        checkout
            .safe()
            .update_index(false)
            .refresh(false)
            .disable_filters(true);
        repository
            .checkout_tree(target_commit.as_object(), Some(&mut checkout))
            .map_err(|_| LocalGitFailure::Operation)?;
        let mut next_index = Index::new().map_err(|_| LocalGitFailure::Operation)?;
        next_index
            .read_tree(&target_tree)
            .map_err(|_| LocalGitFailure::Operation)?;
        for (path, entry) in staged_entries {
            if let Some(entry) = entry {
                next_index
                    .add(&entry)
                    .map_err(|_| LocalGitFailure::Operation)?;
            } else if next_index.get_path(&path, 0).is_some() {
                next_index
                    .remove_path(&path)
                    .map_err(|_| LocalGitFailure::Operation)?;
            }
        }
        index_lock.write(&next_index)?;
        index_lock.commit()?;
        transaction
            .set_symbolic_target(
                "HEAD",
                &reference_name,
                Some(&signature),
                "checkout: moving to configured local branch",
            )
            .and_then(|_| transaction.commit())
            .map_err(|_| LocalGitFailure::Operation)?;
        Ok(BranchResult {
            branch: arguments.name,
            head: target.to_string(),
        })
    }
}

fn encode_result(result: &LocalGitResult) -> Result<String, LocalGitFailure> {
    let encoded = serde_json::to_string(result).map_err(|_| LocalGitFailure::Encoding)?;
    ToolResultText::try_new(encoded)
        .map(ToolResultText::into_string)
        .map_err(|_| LocalGitFailure::Encoding)
}

fn resolve_reference_chain(
    repository: &Repository,
    start: &str,
) -> Result<(Vec<String>, Option<git2::Oid>), LocalGitFailure> {
    const MAX_SYMBOLIC_REFERENCE_DEPTH: usize = 16;
    let mut names = Vec::new();
    let mut current = start.to_owned();
    loop {
        if names.len() == MAX_SYMBOLIC_REFERENCE_DEPTH || names.contains(&current) {
            return Err(LocalGitFailure::Operation);
        }
        let reference = match repository.find_reference(&current) {
            Ok(reference) => reference,
            Err(error) if error.code() == ErrorCode::NotFound && !names.is_empty() => {
                names.push(current);
                return Ok((names, None));
            }
            Err(_) => return Err(LocalGitFailure::Operation),
        };
        names.push(current);
        if let Some(target) = reference.target() {
            return Ok((names, Some(target)));
        }
        current = reference
            .symbolic_target()
            .map_err(|_| LocalGitFailure::Operation)?
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
    }
}

fn repository_filemode(repository: &Repository) -> Result<bool, LocalGitFailure> {
    let config = repository
        .config()
        .map_err(|_| LocalGitFailure::Repository)?;
    match config.get_bool("core.filemode") {
        Ok(filemode) => Ok(filemode),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(true),
        Err(_) => Err(LocalGitFailure::Repository),
    }
}

struct IndexLock {
    index_path: PathBuf,
    lock_path: PathBuf,
    lock: fs::File,
    identity: FileIdentity,
    committed: bool,
}

impl IndexLock {
    fn acquire(index_path: &Path, lock_path: &Path) -> Result<(Self, Index), LocalGitFailure> {
        let mut lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
            .map_err(|_| LocalGitFailure::Operation)?;
        match openat(
            CWD,
            index_path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(descriptor) => {
                let mut source = fs::File::from(descriptor);
                let metadata = source.metadata().map_err(|_| LocalGitFailure::Repository)?;
                if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES as u64 {
                    return Err(LocalGitFailure::Repository);
                }
                let copied = std::io::copy(
                    &mut Read::by_ref(&mut source).take((MAX_INDEX_BYTES + 1) as u64),
                    &mut lock,
                )
                .map_err(|_| LocalGitFailure::Operation)?;
                if copied > MAX_INDEX_BYTES as u64 {
                    return Err(LocalGitFailure::Repository);
                }
            }
            Err(rustix::io::Errno::NOENT) => write_empty_index(&mut lock)?,
            Err(_) => return Err(LocalGitFailure::Repository),
        }
        lock.sync_all().map_err(|_| LocalGitFailure::Operation)?;
        let identity = file_identity(&lock.metadata().map_err(|_| LocalGitFailure::Operation)?);
        let index = Index::open(&descriptor_path(&lock)).map_err(|_| LocalGitFailure::Operation)?;
        let guard = Self {
            index_path: index_path.to_owned(),
            lock_path: lock_path.to_owned(),
            lock,
            identity,
            committed: false,
        };
        Ok((guard, index))
    }

    fn write(&self, index: &Index) -> Result<(), LocalGitFailure> {
        let mut bytes = Vec::new();
        let version = if index.iter().any(|entry| entry.flags_extended != 0) {
            3_u32
        } else {
            2_u32
        };
        bytes.extend_from_slice(b"DIRC");
        bytes.extend_from_slice(&version.to_be_bytes());
        bytes.extend_from_slice(
            &u32::try_from(index.len())
                .map_err(|_| LocalGitFailure::Operation)?
                .to_be_bytes(),
        );
        for entry in index.iter() {
            let entry_start = bytes.len();
            for value in [
                entry.ctime.seconds() as u32,
                entry.ctime.nanoseconds(),
                entry.mtime.seconds() as u32,
                entry.mtime.nanoseconds(),
                entry.dev,
                entry.ino,
                entry.mode,
                entry.uid,
                entry.gid,
                entry.file_size,
            ] {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            bytes.extend_from_slice(entry.id.as_bytes());
            let path_length = u16::try_from(entry.path.len())
                .unwrap_or(u16::MAX)
                .min(0x0fff);
            let extended = entry.flags_extended != 0;
            let flags = (entry.flags & !0x4fff) | path_length | if extended { 0x4000 } else { 0 };
            bytes.extend_from_slice(&flags.to_be_bytes());
            if extended {
                bytes.extend_from_slice(&entry.flags_extended.to_be_bytes());
            }
            bytes.extend_from_slice(&entry.path);
            bytes.push(0);
            while (bytes.len() - entry_start) % 8 != 0 {
                bytes.push(0);
            }
        }
        bytes.extend_from_slice(&Sha1::digest(&bytes));
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(LocalGitFailure::Operation);
        }
        let mut lock = &self.lock;
        lock.set_len(0).map_err(|_| LocalGitFailure::Operation)?;
        lock.seek(std::io::SeekFrom::Start(0))
            .map_err(|_| LocalGitFailure::Operation)?;
        lock.write_all(&bytes)
            .and_then(|()| lock.sync_all())
            .map_err(|_| LocalGitFailure::Operation)
    }

    fn commit(mut self) -> Result<(), LocalGitFailure> {
        let path_identity = fs::symlink_metadata(&self.lock_path)
            .map(|metadata| file_identity(&metadata))
            .map_err(|_| LocalGitFailure::Operation)?;
        let descriptor_identity = file_identity(
            &self
                .lock
                .metadata()
                .map_err(|_| LocalGitFailure::Operation)?,
        );
        if path_identity != self.identity || descriptor_identity != self.identity {
            return Err(LocalGitFailure::Operation);
        }
        fs::rename(&self.lock_path, &self.index_path).map_err(|_| LocalGitFailure::Operation)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let still_owned = fs::symlink_metadata(&self.lock_path)
            .map(|metadata| file_identity(&metadata) == self.identity)
            .unwrap_or(false);
        if !self.committed && still_owned {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

fn write_empty_index(file: &mut fs::File) -> Result<(), LocalGitFailure> {
    const EMPTY_INDEX_HEADER: &[u8; 12] = b"DIRC\0\0\0\x02\0\0\0\0";
    file.write_all(EMPTY_INDEX_HEADER)
        .map_err(|_| LocalGitFailure::Operation)?;
    file.write_all(&Sha1::digest(EMPTY_INDEX_HEADER))
        .map_err(|_| LocalGitFailure::Operation)
}

fn pin_optional_git_file(
    path: &Path,
    max_bytes: usize,
) -> Result<Option<fs::File>, LocalGitFailure> {
    match openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let file = fs::File::from(descriptor);
            let metadata = file.metadata().map_err(|_| LocalGitFailure::Repository)?;
            if metadata.is_file() && metadata.len() <= max_bytes as u64 {
                Ok(Some(file))
            } else {
                Err(LocalGitFailure::Repository)
            }
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(LocalGitFailure::Repository),
    }
}

fn read_merge_parent_ids(path: &Path) -> Result<Vec<git2::Oid>, LocalGitFailure> {
    let mut file =
        pin_optional_git_file(path, MAX_MERGE_HEAD_BYTES)?.ok_or(LocalGitFailure::Repository)?;
    let mut bytes = Vec::with_capacity(MAX_MERGE_HEAD_BYTES);
    Read::by_ref(&mut file)
        .take((MAX_MERGE_HEAD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Repository)?;
    if bytes.len() > MAX_MERGE_HEAD_BYTES {
        return Err(LocalGitFailure::Repository);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| LocalGitFailure::Repository)?;
    let mut parents = Vec::new();
    for line in text.lines() {
        if parents.len() == MAX_MERGE_PARENTS {
            return Err(LocalGitFailure::Repository);
        }
        parents.push(git2::Oid::from_str(line).map_err(|_| LocalGitFailure::Repository)?);
    }
    if parents.is_empty() {
        Err(LocalGitFailure::Repository)
    } else {
        Ok(parents)
    }
}

enum PlannedStage {
    Add {
        supplied: String,
        bytes: Vec<u8>,
        mode: u32,
    },
    Remove {
        path: PathBuf,
    },
    RemoveConflict {
        path: PathBuf,
    },
}

fn clone_index_entry(entry: &IndexEntry) -> IndexEntry {
    IndexEntry {
        ctime: entry.ctime,
        mtime: entry.mtime,
        dev: entry.dev,
        ino: entry.ino,
        mode: entry.mode,
        uid: entry.uid,
        gid: entry.gid,
        file_size: entry.file_size,
        id: entry.id,
        flags: entry.flags,
        flags_extended: entry.flags_extended,
        path: entry.path.clone(),
    }
}

fn index_path_is_conflicted(index: &git2::Index, path: &Path) -> bool {
    index.get_path(path, 1).is_some()
        || index.get_path(path, 2).is_some()
        || index.get_path(path, 3).is_some()
}

fn validate_checkout_path<FileSystem: WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    path: &Path,
) -> Result<(), LocalGitFailure> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
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

fn worktree_head_tree(repository: &Repository) -> Result<Option<git2::Tree<'_>>, LocalGitFailure> {
    match repository.head() {
        Ok(head) => {
            let commit = head.target().ok_or(LocalGitFailure::Operation)?;
            tree_for_commit(repository, commit).map(Some)
        }
        Err(error) if error.code() == ErrorCode::UnbornBranch => Ok(None),
        Err(_) => Err(LocalGitFailure::Operation),
    }
}

fn status<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<StatusResult, LocalGitFailure> {
    let (branch, branch_truncated) = branch_name(repository);
    let head = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| oid.to_string());
    let head_tree = worktree_head_tree(repository)?;
    let index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
    if let Some(head_tree) = &head_tree {
        validate_tree_discovery(repository, head_tree)?;
    }
    validate_index_objects(repository, &index)?;
    let mut staged = repository
        .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
        .map_err(|_| LocalGitFailure::Operation)?;
    staged
        .find_similar(Some(DiffFindOptions::new().renames(true)))
        .map_err(|_| LocalGitFailure::Operation)?;
    let filemode = repository_filemode(repository)?;
    let mut worktree_bytes = 0_usize;
    let mut raw = BTreeMap::new();
    for delta in staged.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .ok_or(LocalGitFailure::Operation)?
            .to_owned();
        let previous_path = (delta.status() == Delta::Renamed)
            .then(|| delta.old_file().path().map(Path::to_owned))
            .flatten();
        raw.insert(
            path,
            RawStatusEntry {
                previous_path,
                index: delta_status(delta.status()),
                worktree: "unchanged",
            },
        );
    }
    let indexed = index_files(&index);
    let mut deleted = Vec::new();
    for (path, (oid, mode)) in &indexed {
        if *mode == GITLINK_MODE {
            continue;
        }
        match filesystem.read_file_prefix(root, path, MAX_OBJECT_BYTES) {
            Ok(read) => {
                charge_worktree_bytes(&mut worktree_bytes, read.bytes.len())?;
                let observed_mode = if read.mode & 0o111 == 0 {
                    0o100644
                } else {
                    0o100755
                };
                if read.truncated || blob_oid(&read.bytes)? != *oid {
                    set_worktree_status(&mut raw, path, "modified");
                } else if filemode && observed_mode != *mode {
                    set_worktree_status(&mut raw, path, "type_changed");
                }
            }
            Err(WorkspaceResolveError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                deleted.push((path.clone(), *oid));
                set_worktree_status(&mut raw, path, "deleted");
            }
            Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
            Err(WorkspaceResolveError::Io { .. }) => {
                set_worktree_status(&mut raw, path, "type_changed");
            }
        }
    }
    for path in untracked {
        let rename = match filesystem.entry_kind(root, &path) {
            Ok(WorkspaceEntryKind::File) => {
                match filesystem.read_file_prefix(root, &path, MAX_OBJECT_BYTES) {
                    Ok(read) => {
                        charge_worktree_bytes(&mut worktree_bytes, read.bytes.len())?;
                        (!read.truncated)
                            .then(|| blob_oid(&read.bytes).ok())
                            .flatten()
                            .and_then(|oid| {
                                deleted
                                    .iter()
                                    .position(|(_, deleted_oid)| *deleted_oid == oid)
                            })
                    }
                    Err(WorkspaceResolveError::Rejected(_)) => {
                        return Err(LocalGitFailure::Path);
                    }
                    Err(WorkspaceResolveError::Io { .. }) => None,
                }
            }
            Ok(WorkspaceEntryKind::Directory)
            | Ok(WorkspaceEntryKind::Symlink)
            | Ok(WorkspaceEntryKind::Other)
            | Err(_) => None,
        };
        if let Some(position) = rename {
            let (previous_path, _) = deleted.remove(position);
            let staged_delta = raw
                .get(&previous_path)
                .is_some_and(|entry| entry.index != "unchanged");
            if !staged_delta {
                raw.remove(&previous_path);
            }
            raw.insert(
                path,
                RawStatusEntry {
                    previous_path: Some(previous_path),
                    index: "unchanged",
                    worktree: "renamed",
                },
            );
        } else {
            raw.entry(path)
                .and_modify(|entry| entry.worktree = "untracked")
                .or_insert(RawStatusEntry {
                    previous_path: None,
                    index: "unchanged",
                    worktree: "untracked",
                });
        }
    }
    let mut truncated = raw.len() > MAX_STATUS_ENTRIES;
    let mut entries = Vec::new();
    for (path, entry) in raw.into_iter().take(MAX_STATUS_ENTRIES) {
        let (path, path_truncated) = bounded_status_path(path.as_os_str().as_bytes());
        let (previous_path, previous_truncated) =
            entry.previous_path.map_or((None, false), |path| {
                let (path, truncated) = bounded_status_path(path.as_os_str().as_bytes());
                (Some(path), truncated)
            });
        truncated |= path_truncated || previous_truncated;
        entries.push(StatusEntry {
            path,
            previous_path,
            index: entry.index,
            worktree: entry.worktree,
        });
    }
    Ok(StatusResult {
        branch,
        branch_truncated,
        head,
        entries,
        truncated,
    })
}

struct RawStatusEntry {
    previous_path: Option<PathBuf>,
    index: &'static str,
    worktree: &'static str,
}

fn set_worktree_status(
    entries: &mut BTreeMap<PathBuf, RawStatusEntry>,
    path: &Path,
    worktree: &'static str,
) {
    entries
        .entry(path.to_owned())
        .or_insert(RawStatusEntry {
            previous_path: None,
            index: "unchanged",
            worktree,
        })
        .worktree = worktree;
}

const fn delta_status(status: Delta) -> &'static str {
    match status {
        Delta::Added => "added",
        Delta::Deleted => "deleted",
        Delta::Modified => "modified",
        Delta::Renamed => "renamed",
        Delta::Typechange => "type_changed",
        Delta::Conflicted => "conflicted",
        Delta::Unmodified
        | Delta::Copied
        | Delta::Ignored
        | Delta::Untracked
        | Delta::Unreadable => "unchanged",
    }
}

fn index_files(index: &Index) -> BTreeMap<PathBuf, (git2::Oid, u32)> {
    index
        .iter()
        .filter(|entry| entry.flags & 0x3000 == 0)
        .map(|entry| {
            (
                PathBuf::from(std::ffi::OsString::from_vec(entry.path)),
                (entry.id, entry.mode),
            )
        })
        .collect()
}

fn tracked_directories(index: &Index) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for path in index_files(index).into_keys() {
        for parent in path.ancestors().skip(1) {
            if !parent.as_os_str().is_empty() {
                directories.insert(parent.to_owned());
            }
        }
    }
    directories
}

fn blob_oid(bytes: &[u8]) -> Result<git2::Oid, LocalGitFailure> {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    git2::Oid::from_bytes(&hasher.finalize()).map_err(|_| LocalGitFailure::Operation)
}

fn charge_worktree_bytes(total: &mut usize, bytes: usize) -> Result<(), LocalGitFailure> {
    *total = total
        .checked_add(bytes)
        .filter(|total| *total <= MAX_WORKTREE_TOTAL_BYTES)
        .ok_or(LocalGitFailure::Operation)?;
    Ok(())
}

fn bounded_status_path(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(path) => bounded_text(path, MAX_STATUS_PATH_BYTES),
        Err(_) => ("[non-utf8]".to_owned(), true),
    }
}

fn branch_name(repository: &Repository) -> (Option<String>, bool) {
    let branch = repository
        .find_reference("HEAD")
        .ok()
        .and_then(|head| head.symbolic_target_bytes().map(<[u8]>::to_owned))
        .and_then(|target| target.strip_prefix(b"refs/heads/").map(<[u8]>::to_owned));
    match branch {
        Some(branch) => {
            let (branch, truncated) = bounded_bytes(&branch, MAX_BRANCH_BYTES);
            (Some(branch), truncated)
        }
        None => (None, false),
    }
}

fn diff<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    arguments: GitDiffArguments,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<DiffResult, LocalGitFailure> {
    let GitDiffArguments::Revisions { base, head } = arguments else {
        return worktree_diff(repository, filesystem, root, untracked);
    };
    let mut options = DiffOptions::new();
    options.ignore_submodules(true);
    let base_tree = resolve_bounded_tree(repository, &base)?;
    let head_tree = resolve_bounded_tree(repository, &head)?;
    validate_tree_discovery(repository, &base_tree)?;
    validate_tree_discovery(repository, &head_tree)?;
    let diff = repository
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut options))
        .map_err(|_| LocalGitFailure::Operation)?;
    render_diff(&diff)
}

fn worktree_diff<FileSystem: WorkspaceFileSystem>(
    repository: &Repository,
    filesystem: &FileSystem,
    root: &WorkspaceRoot,
    untracked: Vec<PathBuf>,
) -> Result<DiffResult, LocalGitFailure> {
    let head_tree = worktree_head_tree(repository)?;
    let head_files = match head_tree.as_ref() {
        Some(tree) => {
            validate_tree_discovery(repository, tree)?;
            tree_files(repository, tree)?
        }
        None => BTreeMap::new(),
    };
    let index = repository.index().map_err(|_| LocalGitFailure::Operation)?;
    let index_files = index_files(&index);
    let untracked_files = untracked
        .into_iter()
        .filter(|path| {
            matches!(
                filesystem.entry_kind(root, path),
                Ok(WorkspaceEntryKind::File)
            )
        })
        .collect::<BTreeSet<_>>();
    let gitlink_diff = staged_gitlink_diff(
        repository,
        head_tree.as_ref(),
        &index,
        &head_files,
        &index_files,
    )?;
    let mut bytes = gitlink_diff.patch.into_bytes();
    let mut truncated = gitlink_diff.truncated;
    let filemode = repository_filemode(repository)?;
    let mut worktree_bytes = 0_usize;
    if truncated {
        return render_patch_bytes(bytes, true);
    }
    let paths = head_files
        .keys()
        .chain(index_files.keys())
        .chain(untracked_files.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        if head_files
            .get(&path)
            .is_some_and(|(_, mode)| *mode == GITLINK_MODE)
            || index_files
                .get(&path)
                .is_some_and(|(_, mode)| *mode == GITLINK_MODE)
        {
            continue;
        }
        let old_blob = match head_files.get(&path) {
            Some((oid, _mode)) => Some(
                repository
                    .find_blob(*oid)
                    .map_err(|_| LocalGitFailure::Operation)?,
            ),
            None => None,
        };
        let new_buffer = if index_files.contains_key(&path) || untracked_files.contains(&path) {
            match filesystem.entry_kind(root, &path) {
                Ok(WorkspaceEntryKind::Directory) => None,
                Ok(WorkspaceEntryKind::Symlink | WorkspaceEntryKind::Other) => {
                    return Err(LocalGitFailure::Path);
                }
                Err(WorkspaceResolveError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(WorkspaceResolveError::Rejected(_)) => return Err(LocalGitFailure::Path),
                Err(WorkspaceResolveError::Io { .. }) => return Err(LocalGitFailure::Operation),
                Ok(WorkspaceEntryKind::File) => {
                    match filesystem.read_file_prefix(root, &path, MAX_OBJECT_BYTES) {
                        Ok(read) if read.truncated => return Err(LocalGitFailure::Operation),
                        Ok(read) => {
                            charge_worktree_bytes(&mut worktree_bytes, read.bytes.len())?;
                            let observed_mode = if read.mode & 0o111 == 0 {
                                0o100644
                            } else {
                                0o100755
                            };
                            let mode = if filemode {
                                observed_mode
                            } else {
                                index_files
                                    .get(&path)
                                    .map_or(observed_mode, |(_, mode)| *mode)
                            };
                            Some((read.bytes, mode))
                        }
                        Err(WorkspaceResolveError::Rejected(_)) => {
                            return Err(LocalGitFailure::Path);
                        }
                        Err(WorkspaceResolveError::Io { .. }) => {
                            return Err(LocalGitFailure::Operation);
                        }
                    }
                }
            }
        } else {
            None
        };
        let mut options = DiffOptions::new();
        options.force_text(true);
        let patch = match (old_blob.as_ref(), new_buffer.as_ref()) {
            (Some(blob), Some((buffer, _mode))) => Patch::from_blob_and_buffer(
                blob,
                Some(&path),
                buffer,
                Some(&path),
                Some(&mut options),
            ),
            (Some(blob), None) => {
                Patch::from_blob_and_buffer(blob, Some(&path), b"", None, Some(&mut options))
            }
            (None, Some((buffer, _mode))) => {
                Patch::from_buffers(b"", None, buffer, Some(&path), Some(&mut options))
            }
            (None, None) => continue,
        }
        .map_err(|_| LocalGitFailure::Operation)?;
        let mode_change = head_files.get(&path).zip(new_buffer.as_ref()).and_then(
            |((_, old_mode), (_, new_mode))| (old_mode != new_mode).then_some((old_mode, new_mode)),
        );
        append_bounded(&mut bytes, patch, &path, mode_change, &mut truncated)?;
        if truncated {
            break;
        }
    }
    render_patch_bytes(bytes, truncated)
}

fn staged_gitlink_diff(
    repository: &Repository,
    head: Option<&git2::Tree<'_>>,
    index: &Index,
    head_files: &BTreeMap<PathBuf, (git2::Oid, u32)>,
    index_files: &BTreeMap<PathBuf, (git2::Oid, u32)>,
) -> Result<DiffResult, LocalGitFailure> {
    let paths = head_files
        .iter()
        .filter(|(_, (_, mode))| *mode == GITLINK_MODE)
        .map(|(path, _)| path)
        .chain(
            index_files
                .iter()
                .filter(|(_, (_, mode))| *mode == GITLINK_MODE)
                .map(|(path, _)| path),
        )
        .collect::<BTreeSet<_>>();
    if paths.is_empty() {
        return Ok(DiffResult {
            patch: String::new(),
            truncated: false,
        });
    }
    let mut options = DiffOptions::new();
    options
        .ignore_submodules(false)
        .disable_pathspec_match(true);
    for path in paths {
        options.pathspec(path);
    }
    let diff = repository
        .diff_tree_to_index(head, Some(index), Some(&mut options))
        .map_err(|_| LocalGitFailure::Operation)?;
    render_diff(&diff)
}

fn append_bounded(
    bytes: &mut Vec<u8>,
    mut patch: Patch<'_>,
    path: &Path,
    mode_change: Option<(&u32, &u32)>,
    truncated: &mut bool,
) -> Result<(), LocalGitFailure> {
    let patch = patch.to_buf().map_err(|_| LocalGitFailure::Operation)?;
    let patch = match mode_change {
        Some((old_mode, new_mode)) => patch_with_mode_change(path, &patch, *old_mode, *new_mode)?,
        None => patch.to_vec(),
    };
    let remaining = MAX_DIFF_BYTES.saturating_sub(bytes.len());
    if patch.len() <= remaining {
        bytes.extend_from_slice(&patch);
    } else {
        bytes.extend_from_slice(&patch[..remaining]);
        *truncated = true;
    }
    Ok(())
}

fn patch_with_mode_change(
    path: &Path,
    patch: &[u8],
    old_mode: u32,
    new_mode: u32,
) -> Result<Vec<u8>, LocalGitFailure> {
    let path = path.to_str().ok_or(LocalGitFailure::Operation)?;
    let mode = format!("old mode {old_mode:06o}\nnew mode {new_mode:06o}\n");
    if patch.is_empty() {
        return Ok(format!("diff --git a/{path} b/{path}\n{mode}").into_bytes());
    }
    let first_line = patch
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|position| position + 1)
        .ok_or(LocalGitFailure::Operation)?;
    let mut rendered = Vec::with_capacity(patch.len().saturating_add(mode.len()));
    rendered.extend_from_slice(&patch[..first_line]);
    rendered.extend_from_slice(mode.as_bytes());
    rendered.extend_from_slice(&patch[first_line..]);
    Ok(rendered)
}

fn render_diff(diff: &git2::Diff<'_>) -> Result<DiffResult, LocalGitFailure> {
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
    render_patch_bytes(bytes, truncated)
}

fn render_patch_bytes(bytes: Vec<u8>, mut truncated: bool) -> Result<DiffResult, LocalGitFailure> {
    let patch = match String::from_utf8(bytes) {
        Ok(patch) => patch,
        Err(error) => {
            truncated = true;
            let lossy = String::from_utf8_lossy(error.as_bytes());
            let (patch, _) = bounded_text(&lossy, MAX_DIFF_BYTES);
            patch
        }
    };
    Ok(DiffResult { patch, truncated })
}

fn log(repository: &Repository, arguments: GitLogArguments) -> Result<LogResult, LocalGitFailure> {
    let start = resolve_bounded_commit(repository, &arguments.revision)?.id();
    let mut pending = vec![start];
    let mut seen = HashSet::new();
    let mut parents_by_commit = HashMap::new();
    let mut remaining_children = HashMap::<git2::Oid, usize>::new();
    let mut commit_times = HashMap::new();
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if seen.len() > MAX_WORKTREE_INSPECTIONS {
            return Err(LocalGitFailure::Operation);
        }
        let commit = find_bounded_commit(repository, oid)?;
        if commit.parent_count() > MAX_MERGE_PARENTS {
            return Err(LocalGitFailure::Operation);
        }
        let parents = commit.parent_ids().collect::<Vec<_>>();
        for parent in &parents {
            *remaining_children.entry(*parent).or_default() += 1;
        }
        pending.extend(parents.iter().copied());
        commit_times.insert(oid, commit.time().seconds());
        parents_by_commit.insert(oid, parents);
    }
    let mut available = BTreeSet::from([(
        *commit_times.get(&start).ok_or(LocalGitFailure::Operation)?,
        start,
    )]);
    let mut ordered = Vec::with_capacity(seen.len());
    while let Some((_, oid)) = available.pop_last() {
        ordered.push(oid);
        for parent in parents_by_commit
            .get(&oid)
            .ok_or(LocalGitFailure::Operation)?
        {
            let remaining = remaining_children
                .get_mut(parent)
                .ok_or(LocalGitFailure::Operation)?;
            *remaining = remaining.checked_sub(1).ok_or(LocalGitFailure::Operation)?;
            if *remaining == 0 {
                available.insert((
                    *commit_times.get(parent).ok_or(LocalGitFailure::Operation)?,
                    *parent,
                ));
            }
        }
    }
    if ordered.len() != seen.len() {
        return Err(LocalGitFailure::Operation);
    }
    let mut commits = Vec::new();
    let truncated = ordered.len() > arguments.max_entries;
    for oid in ordered.into_iter().take(arguments.max_entries) {
        let commit = find_bounded_commit(repository, oid)?;
        let author = commit.author();
        let (author_name, author_name_truncated) =
            bounded_bytes(author.name_bytes(), MAX_LOG_IDENTITY_BYTES);
        let (author_email, author_email_truncated) =
            bounded_bytes(author.email_bytes(), MAX_LOG_IDENTITY_BYTES);
        let (message, message_truncated) =
            bounded_bytes(commit.message_raw_bytes(), MAX_LOG_MESSAGE_BYTES);
        commits.push(LogEntry {
            commit: oid.to_string(),
            author_name,
            author_name_truncated,
            author_email,
            author_email_truncated,
            message,
            message_truncated,
        });
    }
    Ok(LogResult { commits, truncated })
}

fn tree_files(
    repository: &Repository,
    root: &git2::Tree<'_>,
) -> Result<BTreeMap<PathBuf, (git2::Oid, u32)>, LocalGitFailure> {
    let mut pending = vec![(root.id(), PathBuf::new())];
    let mut files = BTreeMap::new();
    while let Some((oid, prefix)) = pending.pop() {
        let tree = repository
            .find_tree(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        for entry in &tree {
            let mut path = prefix.clone();
            path.push(std::ffi::OsStr::from_bytes(entry.name_bytes()));
            match entry.kind() {
                Some(git2::ObjectType::Tree) => pending.push((entry.id(), path)),
                Some(git2::ObjectType::Blob) => {
                    let mode =
                        u32::try_from(entry.filemode()).map_err(|_| LocalGitFailure::Operation)?;
                    files.insert(path, (entry.id(), mode));
                }
                Some(git2::ObjectType::Commit) if entry.filemode() == GITLINK_MODE as i32 => {
                    files.insert(path, (entry.id(), GITLINK_MODE));
                }
                _ => return Err(LocalGitFailure::Operation),
            }
        }
    }
    Ok(files)
}

fn validate_tree_discovery(
    repository: &Repository,
    root: &git2::Tree<'_>,
) -> Result<(), LocalGitFailure> {
    let object_database = repository.odb().map_err(|_| LocalGitFailure::Operation)?;
    let mut pending = vec![(root.id(), PathBuf::new())];
    let mut inspected = 0_usize;
    let mut inspected_path_bytes = 0_usize;
    while let Some((oid, prefix)) = pending.pop() {
        let (size, kind) = object_database
            .read_header(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        if kind != git2::ObjectType::Tree || size > MAX_OBJECT_BYTES {
            return Err(LocalGitFailure::Operation);
        }
        let tree = repository
            .find_tree(oid)
            .map_err(|_| LocalGitFailure::Operation)?;
        for entry in &tree {
            inspected = inspected.saturating_add(1);
            let mut path = prefix.clone();
            path.push(std::ffi::OsStr::from_bytes(entry.name_bytes()));
            inspected_path_bytes =
                inspected_path_bytes.saturating_add(path.as_os_str().as_bytes().len());
            if inspected > MAX_WORKTREE_INSPECTIONS
                || inspected_path_bytes > MAX_WORKTREE_PATH_BYTES
            {
                return Err(LocalGitFailure::Operation);
            }
            match entry.kind() {
                Some(git2::ObjectType::Tree) => pending.push((entry.id(), path)),
                Some(git2::ObjectType::Blob) => {
                    if !matches!(entry.filemode(), 0o100644 | 0o100755) {
                        return Err(LocalGitFailure::Operation);
                    }
                    let (size, kind) = object_database
                        .read_header(entry.id())
                        .map_err(|_| LocalGitFailure::Operation)?;
                    if kind != git2::ObjectType::Blob || size > MAX_OBJECT_BYTES {
                        return Err(LocalGitFailure::Operation);
                    }
                }
                Some(git2::ObjectType::Commit) if entry.filemode() == GITLINK_MODE as i32 => {}
                _ => return Err(LocalGitFailure::Operation),
            }
        }
    }
    Ok(())
}

fn validate_index_objects(repository: &Repository, index: &Index) -> Result<(), LocalGitFailure> {
    if index.len() > MAX_INDEX_ENTRIES {
        return Err(LocalGitFailure::Operation);
    }
    for entry in index.iter().filter(|entry| entry.flags & 0x3000 == 0) {
        if entry.mode == GITLINK_MODE {
            continue;
        }
        if validate_object_header(repository, entry.id)? != git2::ObjectType::Blob {
            return Err(LocalGitFailure::Operation);
        }
    }
    Ok(())
}

fn validate_object_header(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::ObjectType, LocalGitFailure> {
    let (size, kind) = repository
        .odb()
        .and_then(|object_database| object_database.read_header(oid))
        .map_err(|_| LocalGitFailure::Operation)?;
    if size > MAX_OBJECT_BYTES {
        Err(LocalGitFailure::Operation)
    } else {
        Ok(kind)
    }
}

fn find_bounded_commit(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::Commit<'_>, LocalGitFailure> {
    if validate_object_header(repository, oid)? != git2::ObjectType::Commit {
        return Err(LocalGitFailure::Operation);
    }
    repository
        .find_commit(oid)
        .map_err(|_| LocalGitFailure::Operation)
}

fn find_bounded_tree(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::Tree<'_>, LocalGitFailure> {
    if validate_object_header(repository, oid)? != git2::ObjectType::Tree {
        return Err(LocalGitFailure::Operation);
    }
    repository
        .find_tree(oid)
        .map_err(|_| LocalGitFailure::Operation)
}

fn tree_for_commit(
    repository: &Repository,
    oid: git2::Oid,
) -> Result<git2::Tree<'_>, LocalGitFailure> {
    let commit = find_bounded_commit(repository, oid)?;
    find_bounded_tree(repository, commit.tree_id())
}

fn resolve_bounded_commit<'repository>(
    repository: &'repository Repository,
    revision: &str,
) -> Result<git2::Commit<'repository>, LocalGitFailure> {
    let mut oid = repository
        .revparse_single(revision)
        .map_err(|_| LocalGitFailure::Operation)?
        .id();
    for _depth in 0..16 {
        match validate_object_header(repository, oid)? {
            git2::ObjectType::Commit => return find_bounded_commit(repository, oid),
            git2::ObjectType::Tag => {
                oid = repository
                    .find_tag(oid)
                    .map_err(|_| LocalGitFailure::Operation)?
                    .target_id();
            }
            _ => return Err(LocalGitFailure::Operation),
        }
    }
    Err(LocalGitFailure::Operation)
}

fn resolve_bounded_tree<'repository>(
    repository: &'repository Repository,
    revision: &str,
) -> Result<git2::Tree<'repository>, LocalGitFailure> {
    let mut oid = repository
        .revparse_single(revision)
        .map_err(|_| LocalGitFailure::Operation)?
        .id();
    for _depth in 0..16 {
        match validate_object_header(repository, oid)? {
            git2::ObjectType::Commit => return tree_for_commit(repository, oid),
            git2::ObjectType::Tree => return find_bounded_tree(repository, oid),
            git2::ObjectType::Tag => {
                oid = repository
                    .find_tag(oid)
                    .map_err(|_| LocalGitFailure::Operation)?
                    .target_id();
            }
            _ => return Err(LocalGitFailure::Operation),
        }
    }
    Err(LocalGitFailure::Operation)
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

fn bounded_bytes(value: &[u8], max_bytes: usize) -> (String, bool) {
    match std::str::from_utf8(value) {
        Ok(value) => bounded_text(value, max_bytes),
        Err(_) => {
            let lossy = String::from_utf8_lossy(value);
            let (value, _) = bounded_text(&lossy, max_bytes);
            (value, true)
        }
    }
}

fn commit(
    repository: &mut Repository,
    identity: &GitIdentity,
    arguments: GitCommitArguments,
    index_path: &Path,
    index_lock_path: &Path,
) -> Result<CommitResult, LocalGitFailure> {
    let (_index_lock, mut index) = IndexLock::acquire(index_path, index_lock_path)?;
    validate_index_objects(repository, &index)?;
    let state = repository.state();
    if !matches!(state, RepositoryState::Clean | RepositoryState::Merge) {
        return Err(LocalGitFailure::Operation);
    }
    let merge_parent_ids = if state == RepositoryState::Merge {
        read_merge_parent_ids(&repository.path().join("MERGE_HEAD"))?
    } else {
        Vec::new()
    };
    let (reference_chain, _parent) = resolve_reference_chain(repository, "HEAD")?;
    let mut transaction = repository
        .transaction()
        .map_err(|_| LocalGitFailure::Operation)?;
    for reference in &reference_chain {
        transaction
            .lock_ref(reference)
            .map_err(|_| LocalGitFailure::Operation)?;
    }
    let (locked_chain, parent) = resolve_reference_chain(repository, "HEAD")?;
    if locked_chain != reference_chain {
        return Err(LocalGitFailure::Operation);
    }
    let mut parent_ids = parent.into_iter().collect::<Vec<_>>();
    parent_ids.extend(merge_parent_ids);
    let mut unique_parent_ids = HashSet::new();
    parent_ids.retain(|oid| unique_parent_ids.insert(*oid));
    let parents = parent_ids
        .iter()
        .map(|oid| find_bounded_commit(repository, *oid))
        .collect::<Result<Vec<_>, _>>()?;
    let tree_id = index
        .write_tree_to(repository)
        .map_err(|_| LocalGitFailure::Operation)?;
    let tree = find_bounded_tree(repository, tree_id)?;
    let signature = identity
        .signature()
        .map_err(|_| LocalGitFailure::Operation)?;
    let parent_refs = parents.iter().collect::<Vec<_>>();
    let oid = repository
        .commit(
            None,
            &signature,
            &signature,
            &arguments.message,
            &tree,
            &parent_refs,
        )
        .map_err(|_| LocalGitFailure::Operation)?;
    let update_reference = locked_chain.last().ok_or(LocalGitFailure::Operation)?;
    let head_symbolic_target = locked_chain.get(1);
    transaction
        .set_target(
            update_reference,
            oid,
            Some(&signature),
            "commit: fixer agent",
        )
        .map_err(|_| LocalGitFailure::Operation)?;
    if let Some(head_symbolic_target) = head_symbolic_target {
        transaction
            .set_symbolic_target(
                "HEAD",
                head_symbolic_target,
                Some(&signature),
                "commit: fixer agent",
            )
            .map_err(|_| LocalGitFailure::Operation)?;
    }
    transaction
        .commit()
        .map_err(|_| LocalGitFailure::Operation)?;
    let state_cleaned = state != RepositoryState::Merge || repository.cleanup_state().is_ok();
    Ok(CommitResult {
        commit: oid.to_string(),
        state_cleaned,
    })
}

fn branch_create(
    repository: &Repository,
    authority: &PinnedRepository,
    arguments: GitBranchCreateArguments,
) -> Result<BranchResult, LocalGitFailure> {
    let commit = resolve_bounded_commit(repository, &arguments.start)?;
    let head = commit.id().to_string();
    let reference_name = format!("refs/heads/{}", arguments.name);
    if packed_reference_exists(authority, &reference_name)? {
        return Err(LocalGitFailure::Operation);
    }
    create_loose_branch_reference(authority, &arguments.name, commit.id())?;
    Ok(BranchResult {
        branch: arguments.name,
        head,
    })
}

fn packed_reference_exists(
    authority: &PinnedRepository,
    reference_name: &str,
) -> Result<bool, LocalGitFailure> {
    let descriptor = match openat(
        &authority.git_directory,
        "packed-refs",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(false),
        Err(_) => return Err(LocalGitFailure::Operation),
    };
    let mut file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|_| LocalGitFailure::Operation)?;
    if !metadata.is_file() || metadata.len() > MAX_PACKED_REFS_BYTES as u64 {
        return Err(LocalGitFailure::Operation);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_PACKED_REFS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalGitFailure::Operation)?;
    if bytes.len() > MAX_PACKED_REFS_BYTES {
        return Err(LocalGitFailure::Operation);
    }
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() || matches!(line.first(), Some(b'#' | b'^')) {
            continue;
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or(LocalGitFailure::Operation)?;
        let oid = std::str::from_utf8(&line[..separator])
            .ok()
            .and_then(|oid| git2::Oid::from_str(oid).ok())
            .ok_or(LocalGitFailure::Operation)?;
        let _ = oid;
        if line.get(separator + 1..) == Some(reference_name.as_bytes()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn create_loose_branch_reference(
    authority: &PinnedRepository,
    branch: &str,
    target: git2::Oid,
) -> Result<(), LocalGitFailure> {
    let refs = openat(
        &authority.git_directory,
        "refs",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let mut directory = open_or_create_ref_directory(&refs, OsStr::new("heads"))?;
    let mut components = Path::new(branch).components().peekable();
    let mut leaf = None;
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(LocalGitFailure::Operation);
        };
        if components.peek().is_some() {
            directory = open_or_create_ref_directory(&directory, component)?;
        } else {
            leaf = Some(component.to_owned());
        }
    }
    let leaf = leaf.ok_or(LocalGitFailure::Operation)?;
    let mut lock_name = OsString::from(&leaf);
    lock_name.push(".lock");
    let lock = openat(
        &directory,
        &lock_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| LocalGitFailure::Operation)?;
    let mut lock = fs::File::from(lock);
    let outcome = (|| {
        writeln!(lock, "{target}").map_err(|_| LocalGitFailure::Operation)?;
        lock.sync_all().map_err(|_| LocalGitFailure::Operation)?;
        renameat_with(
            &directory,
            &lock_name,
            &directory,
            &leaf,
            RenameFlags::NOREPLACE,
        )
        .map_err(|_| LocalGitFailure::Operation)
    })();
    if outcome.is_err() {
        let _ = unlinkat(&directory, &lock_name, AtFlags::empty());
    }
    outcome
}

fn open_or_create_ref_directory(
    parent: &OwnedFd,
    name: &OsStr,
) -> Result<OwnedFd, LocalGitFailure> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(directory) => Ok(directory),
        Err(error) if error == rustix::io::Errno::NOENT => {
            mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
                .map_err(|_| LocalGitFailure::Operation)?;
            openat(parent, name, flags, Mode::empty()).map_err(|_| LocalGitFailure::Operation)
        }
        Err(_) => Err(LocalGitFailure::Operation),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        os::unix::{
            ffi::OsStringExt,
            fs::{FileTypeExt, PermissionsExt, symlink},
        },
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use git2::{IndexAddOption, ObjectType, Oid, Repository, Signature, build::CheckoutBuilder};
    use rustix::fs::mkfifoat;
    use signalbox_application::ToolCatalog;
    use signalbox_domain::{ToolEffectClass, ToolName, ToolPermissionDefault};
    use signalbox_tools_workspace::{WorkspaceDirectoryRead, WorkspaceFileBytes};
    use tempfile::TempDir;

    use super::*;

    const AUTHOR_NAME: &str = "Signalbox Fixer";
    const AUTHOR_EMAIL: &str = "fixer@example.test";
    const INITIAL_MESSAGE: &str = "initial";
    const MODEL_MESSAGE: &str = "subject\n\nmodel data: $(not interpreted)\n";
    const FIX_BRANCH: &str = "agent/fix";
    const TRACKED_PATH: &str = "tracked.txt";
    const UNTRACKED_PATH: &str = "untracked.txt";
    const INITIAL_CONTENT: &str = "before\n";
    const CHANGED_CONTENT: &str = "after\n";
    const CRLF_CONTENT: &[u8] = b"first\r\nsecond\r\n";
    const UNTRACKED_CONTENT: &str = "untracked\n";
    const NESTED_TRACKED_DIRECTORY: &str = "removed";
    const NESTED_TRACKED_PATH: &str = "removed/tracked.txt";
    const RENAMED_TRACKED_PATH: &str = "renamed.txt";
    const TWICE_RENAMED_TRACKED_PATH: &str = "twice-renamed.txt";
    const EMBEDDED_REPOSITORY_PATH: &str = "vendor";
    const SUBMODULE_PATH: &str = "dependency";

    struct Fixture {
        directory: TempDir,
        initial: Oid,
    }

    #[derive(Clone, Debug)]
    struct ReplacingRootFileSystem {
        retired_root: PathBuf,
        replacement_root: PathBuf,
    }

    #[derive(Clone, Debug)]
    struct ObservingIndexLockFileSystem {
        root_path: PathBuf,
        lock_observed: Arc<AtomicBool>,
    }

    #[derive(Clone, Debug)]
    struct ConcurrentRootOpenFileSystem {
        extra_root: Arc<Mutex<Option<fs::File>>>,
    }

    impl WorkspaceFileSystem for ReplacingRootFileSystem {
        fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
            fs::rename(root, &self.retired_root)
                .expect("original root retires during fixture open");
            fs::create_dir(root).expect("replacement root constructs during fixture open");
            Repository::init(root).expect("replacement repository initializes during fixture open");
            let pinned = LocalWorkspaceFileSystem.open_root(root)?;
            fs::rename(root, &self.replacement_root)
                .expect("replacement root retires after fixture pin");
            fs::rename(&self.retired_root, root).expect("original root restores after fixture pin");
            Ok(pinned)
        }

        fn entry_kind(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
        ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.entry_kind(root, path)
        }

        fn read_directory(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
            max_entries: usize,
            max_inspections: usize,
            max_path_bytes: usize,
        ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.read_directory(
                root,
                path,
                max_entries,
                max_inspections,
                max_path_bytes,
            )
        }

        fn read_file_prefix(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
            max_bytes: usize,
        ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.read_file_prefix(root, path, max_bytes)
        }
    }

    impl WorkspaceFileSystem for ObservingIndexLockFileSystem {
        fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
            LocalWorkspaceFileSystem.open_root(root)
        }

        fn entry_kind(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
        ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.entry_kind(root, path)
        }

        fn read_directory(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
            max_entries: usize,
            max_inspections: usize,
            max_path_bytes: usize,
        ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.read_directory(
                root,
                path,
                max_entries,
                max_inspections,
                max_path_bytes,
            )
        }

        fn read_file_prefix(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
            max_bytes: usize,
        ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
            let read = LocalWorkspaceFileSystem.read_file_prefix(root, path, max_bytes)?;
            self.lock_observed.store(
                self.root_path.join(".git/index.lock").is_file(),
                Ordering::SeqCst,
            );
            Ok(read)
        }
    }

    impl WorkspaceFileSystem for ConcurrentRootOpenFileSystem {
        fn open_root(&self, root: &Path) -> Result<WorkspaceRoot, WorkspaceRootError> {
            let pinned = LocalWorkspaceFileSystem.open_root(root)?;
            let extra = fs::File::open(root).expect("concurrent root descriptor opens");
            self.extra_root
                .lock()
                .expect("concurrent root holder locks")
                .replace(extra);
            Ok(pinned)
        }

        fn entry_kind(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
        ) -> Result<WorkspaceEntryKind, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.entry_kind(root, path)
        }

        fn read_directory(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
            max_entries: usize,
            max_inspections: usize,
            max_path_bytes: usize,
        ) -> Result<WorkspaceDirectoryRead, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.read_directory(
                root,
                path,
                max_entries,
                max_inspections,
                max_path_bytes,
            )
        }

        fn read_file_prefix(
            &self,
            root: &WorkspaceRoot,
            path: &Path,
            max_bytes: usize,
        ) -> Result<WorkspaceFileBytes, WorkspaceResolveError> {
            LocalWorkspaceFileSystem.read_file_prefix(root, path, max_bytes)
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

    fn long_author_name() -> String {
        "n".repeat(MAX_LOG_IDENTITY_BYTES + 1)
    }

    fn long_author_email() -> String {
        format!("{}@example.test", "e".repeat(MAX_LOG_IDENTITY_BYTES))
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

    fn count_loose_objects(root: &Path) -> usize {
        fs::read_dir(root.join(".git/objects"))
            .expect("fixture object directory reads")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter(|entry| entry.file_name() != "info" && entry.file_name() != "pack")
            .map(|entry| {
                fs::read_dir(entry.path())
                    .expect("fixture loose-object directory reads")
                    .count()
            })
            .sum()
    }

    fn install_deleted_conflict(fixture: &Fixture) {
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let original_reference = repository
            .head()
            .expect("fixture HEAD exists")
            .name()
            .expect("fixture HEAD name is UTF-8")
            .to_owned();
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch("conflicting", &initial, false)
            .expect("conflicting branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), "ours\n")
            .expect("ours fixture content writes");
        commit_all(&repository, "ours");
        repository
            .set_head("refs/heads/conflicting")
            .expect("conflicting fixture branch selects");
        repository
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .expect("conflicting fixture branch checks out");
        fs::write(fixture.root().join(TRACKED_PATH), "theirs\n")
            .expect("theirs fixture content writes");
        let theirs = commit_all(&repository, "theirs");
        repository
            .set_head(&original_reference)
            .expect("original fixture branch selects");
        repository
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .expect("original fixture branch checks out");
        let annotated = repository
            .find_annotated_commit(theirs)
            .expect("theirs annotated commit opens");
        repository
            .merge(&[&annotated], None, None)
            .expect("fixture merge produces conflict");
        fs::remove_file(fixture.root().join(TRACKED_PATH))
            .expect("conflicted fixture path deletes");
    }

    fn invalid_utf8_commit(repository: &Repository, parent: Oid) -> Oid {
        let tree = repository
            .find_commit(parent)
            .expect("fixture parent commit exists")
            .tree_id();
        let mut raw = format!("tree {tree}\nparent {parent}\nauthor ").into_bytes();
        raw.extend_from_slice(b"bad\xff <bad\xff@example.test> 0 +0000\n");
        raw.extend_from_slice(b"committer Signalbox <fixer@example.test> 0 +0000\n\n");
        raw.extend_from_slice(b"message-\xff\n");
        repository
            .odb()
            .expect("fixture object database opens")
            .write(ObjectType::Commit, &raw)
            .expect("invalid UTF-8 fixture commit writes")
    }

    fn raw_message_commit(repository: &Repository, parent: Oid) -> Oid {
        let tree = repository
            .find_commit(parent)
            .expect("fixture parent commit exists")
            .tree_id();
        let raw = format!(
            "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\n\n\nmessage\n"
        );
        repository
            .odb()
            .expect("fixture object database opens")
            .write(ObjectType::Commit, raw.as_bytes())
            .expect("raw-message fixture commit writes")
    }

    fn plant_over_budget_worktree(root: &Path) {
        for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
            fs::write(root.join(format!("untracked-{sequence:04}.txt")), [])
                .expect("worktree-budget fixture file writes");
        }
    }

    fn plant_over_budget_directory(root: &Path, directory: &str) {
        let directory = root.join(directory);
        fs::create_dir(&directory).expect("worktree-budget fixture directory creates");
        plant_over_budget_entries(&directory);
    }

    fn plant_over_budget_entries(directory: &Path) {
        for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
            fs::write(directory.join(format!("entry-{sequence:04}.txt")), [])
                .expect("worktree-budget fixture file writes");
        }
    }

    fn plant_aggregate_stage_files(root: &Path) -> Vec<String> {
        let bytes = vec![b'x'; MAX_STAGE_FILE_BYTES];
        let count = MAX_STAGE_TOTAL_BYTES / MAX_STAGE_FILE_BYTES + 1;
        let mut paths = Vec::with_capacity(count);
        for sequence in 0..count {
            let path = format!("aggregate-{sequence:02}.txt");
            fs::write(root.join(&path), &bytes).expect("aggregate fixture file writes");
            paths.push(path);
        }
        paths
    }

    fn plant_status_over_byte_budget(fixture: &Fixture) {
        plant_aggregate_stage_files(fixture.root());
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        commit_all(&repository, MODEL_MESSAGE);
    }

    fn commit_with_parents(repository: &Repository, parents: &[Oid], message: &str) -> Oid {
        let tree_id = repository
            .find_commit(parents[0])
            .expect("fixture parent exists")
            .tree_id();
        let tree = repository.find_tree(tree_id).expect("fixture tree exists");
        let parent_commits = parents
            .iter()
            .map(|parent| {
                repository
                    .find_commit(*parent)
                    .expect("fixture parent exists")
            })
            .collect::<Vec<_>>();
        let parent_refs = parent_commits.iter().collect::<Vec<_>>();
        let signature =
            Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture signature constructs");
        repository
            .commit(None, &signature, &signature, message, &tree, &parent_refs)
            .expect("fixture commit writes")
    }

    fn deep_full_path_tree_commit(repository: &Repository, parent: Oid) -> Oid {
        let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
        let mut builder = repository
            .treebuilder(None)
            .expect("leaf tree builder opens");
        builder
            .insert("leaf", blob, 0o100644)
            .expect("leaf inserts");
        let mut tree = builder.write().expect("leaf tree writes");
        let component = "d".repeat(200);
        for _depth in 0..256 {
            let mut builder = repository
                .treebuilder(None)
                .expect("deep tree builder opens");
            builder
                .insert(&component, tree, 0o040000)
                .expect("deep tree inserts");
            tree = builder.write().expect("deep tree writes");
        }
        raw_commit_with_tree(repository, tree, parent)
    }

    fn raw_commit_with_tree(repository: &Repository, tree: Oid, parent: Oid) -> Oid {
        let raw = format!(
            "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\ndeep tree\n"
        );
        repository
            .odb()
            .expect("fixture object database opens")
            .write(ObjectType::Commit, raw.as_bytes())
            .expect("fixture commit object writes")
    }

    fn plant_over_budget_index(repository: &Repository) {
        let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
        let mut index = repository.index().expect("fixture index opens");
        index.clear().expect("fixture index clears");
        for sequence in 0..=MAX_INDEX_ENTRIES {
            let path = format!("entry-{sequence:04}.txt");
            index
                .add(&IndexEntry {
                    ctime: IndexTime::new(0, 0),
                    mtime: IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: 8,
                    id: blob,
                    flags: 0,
                    flags_extended: 0,
                    path: path.into_bytes(),
                })
                .expect("fixture index entry adds");
        }
        index.write().expect("fixture index writes");
    }

    fn over_budget_tree_commit(repository: &Repository, parent: Oid) -> Oid {
        let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
        let mut builder = repository.treebuilder(None).expect("tree builder opens");
        for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
            builder
                .insert(format!("entry-{sequence:04}.txt"), blob, 0o100644)
                .expect("over-budget tree entry inserts");
        }
        let tree_id = builder.write().expect("over-budget tree writes");
        let tree = repository
            .find_tree(tree_id)
            .expect("over-budget tree opens");
        let parent = repository
            .find_commit(parent)
            .expect("fixture parent commit opens");
        let signature = Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("signature constructs");
        repository
            .commit(
                None,
                &signature,
                &signature,
                MODEL_MESSAGE,
                &tree,
                &[&parent],
            )
            .expect("over-budget tree commit writes")
    }

    fn oversized_root_tree_commit(repository: &Repository, parent: Oid) -> Oid {
        let blob = repository.blob(b"bounded\n").expect("fixture blob writes");
        let mut raw_tree = Vec::new();
        for sequence in 0..=MAX_WORKTREE_INSPECTIONS {
            let name = format!("entry-{sequence:04}-{}", "x".repeat(220));
            raw_tree.extend_from_slice(b"100644 ");
            raw_tree.extend_from_slice(name.as_bytes());
            raw_tree.push(0);
            raw_tree.extend_from_slice(blob.as_bytes());
        }
        assert!(raw_tree.len() > MAX_OBJECT_BYTES);
        let tree = repository
            .odb()
            .expect("fixture object database opens")
            .write(ObjectType::Tree, &raw_tree)
            .expect("oversized root tree writes");
        let raw_commit = format!(
            "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\noversized tree\n"
        );
        repository
            .odb()
            .expect("fixture object database reopens")
            .write(ObjectType::Commit, raw_commit.as_bytes())
            .expect("oversized-root-tree fixture commit writes")
    }

    fn oversized_commit_object(repository: &Repository, parent: Oid) -> Oid {
        let tree = repository
            .find_commit(parent)
            .expect("fixture parent commit exists")
            .tree_id();
        let mut raw = format!(
            "tree {tree}\nparent {parent}\nauthor Signalbox <fixer@example.test> 0 +0000\ncommitter Signalbox <fixer@example.test> 0 +0000\n\n"
        )
        .into_bytes();
        raw.resize(MAX_OBJECT_BYTES + 1, b'x');
        repository
            .odb()
            .expect("fixture object database opens")
            .write(ObjectType::Commit, &raw)
            .expect("oversized fixture commit writes")
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

    fn repository_uses_pinned_config_without_fifo_wait(
        executor: LocalGitExecutor<LocalWorkspaceFileSystem>,
        replacement_config: PathBuf,
    ) -> bool {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let opened = executor
                .repository_authority
                .repository()
                .map(|repository| !repository.is_bare())
                .unwrap_or(false);
            sender.send(opened).expect("fixture result sends");
        });
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(opened) => {
                worker.join().expect("fixture worker joins");
                opened
            }
            Err(_) => {
                let unblock = openat(
                    CWD,
                    replacement_config,
                    OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .expect("replacement FIFO unblocks");
                drop(unblock);
                worker.join().expect("blocked fixture worker joins");
                false
            }
        }
    }

    fn status_uses_bound_index_without_fifo_wait(
        executor: LocalGitExecutor<LocalWorkspaceFileSystem>,
        index_path: PathBuf,
    ) -> bool {
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (proceed_sender, proceed_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let repository = executor
                .repository_authority
                .repository()
                .expect("pinned fixture repository opens");
            let _index_lock = executor
                .bind_locked_index(&repository)
                .expect("fixture index binds");
            ready_sender.send(()).expect("fixture readiness sends");
            proceed_receiver
                .recv()
                .expect("fixture continuation receives");
            result_sender
                .send(
                    status(
                        &repository,
                        &executor.filesystem,
                        &executor.root,
                        Vec::new(),
                    )
                    .is_ok(),
                )
                .expect("fixture status result sends");
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("fixture index binds in time");
        fs::remove_file(&index_path).expect("repository index removes for fixture");
        mkfifoat(CWD, &index_path, Mode::RUSR | Mode::WUSR)
            .expect("replacement index FIFO constructs");
        proceed_sender.send(()).expect("fixture continuation sends");
        match result_receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(completed) => {
                worker.join().expect("fixture worker joins");
                completed
            }
            Err(_) => {
                let unblock = openat(
                    CWD,
                    index_path,
                    OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .expect("replacement FIFO unblocks");
                drop(unblock);
                worker.join().expect("blocked fixture worker joins");
                false
            }
        }
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
    fn status_rejects_worktree_over_discovery_budget() {
        let fixture = Fixture::new();
        plant_over_budget_worktree(fixture.root());
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("over-budget status discovery rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn status_rejects_aggregate_worktree_bytes_over_budget() {
        let fixture = Fixture::new();
        plant_status_over_byte_budget(&fixture);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("aggregate status byte budget rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn status_honors_disabled_filemode_tracking() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        repository
            .config()
            .expect("fixture config opens")
            .set_bool("core.filemode", false)
            .expect("fixture filemode disables");
        fs::set_permissions(
            fixture.root().join(TRACKED_PATH),
            fs::Permissions::from_mode(0o755),
        )
        .expect("fixture executable mode sets");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(
            status["entries"]
                .as_array()
                .expect("entries are an array")
                .len(),
            0
        );
    }

    #[test]
    fn status_reports_a_recreated_path_after_staged_deletion() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .remove_path(Path::new(TRACKED_PATH))
            .expect("fixture deletion stages");
        index.write().expect("fixture index writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["entries"][0]["path"], TRACKED_PATH);
        assert_eq!(status["entries"][0]["index"], "deleted");
        assert_eq!(status["entries"][0]["worktree"], "untracked");
    }

    #[test]
    fn status_prunes_an_untracked_embedded_repository() {
        let fixture = Fixture::new();
        let embedded = fixture.root().join(EMBEDDED_REPOSITORY_PATH);
        Repository::init(&embedded).expect("embedded repository initializes");
        plant_over_budget_entries(&embedded);
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(
            status["entries"]
                .as_array()
                .expect("entries are an array")
                .len(),
            1
        );
        assert_eq!(status["entries"][0]["path"], EMBEDDED_REPOSITORY_PATH);
        assert_eq!(status["entries"][0]["worktree"], "untracked");
    }

    #[test]
    fn status_does_not_prune_a_malformed_embedded_repository_marker() {
        let fixture = Fixture::new();
        let malformed = fixture.root().join(EMBEDDED_REPOSITORY_PATH).join(".git");
        fs::create_dir_all(&malformed).expect("malformed marker directory constructs");
        plant_over_budget_entries(&malformed);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("malformed embedded marker remains subject to discovery bounds");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn status_bounds_directories_even_when_an_ignore_file_names_them() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(".gitignore"), "ignored/\n").expect("ignore fixture writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        commit_all(&repository, MODEL_MESSAGE);
        plant_over_budget_directory(fixture.root(), "ignored");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("ignored over-budget directory still rejects safely");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn status_rejects_oversized_index_before_libgit2_parsing() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let index = fs::OpenOptions::new()
            .write(true)
            .open(fixture.root().join(".git/index"))
            .expect("fixture index opens");
        index
            .set_len((MAX_INDEX_BYTES + 1) as u64)
            .expect("oversized sparse index sets length");

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("oversized index rejects before status parsing");

        assert_eq!(failure, LocalGitFailure::Repository);
    }

    #[test]
    fn status_rejects_oversized_head_before_libgit2_parsing() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let head = fs::OpenOptions::new()
            .write(true)
            .open(fixture.root().join(".git/HEAD"))
            .expect("fixture HEAD opens");
        head.set_len((MAX_REVISION_BYTES + 1) as u64)
            .expect("oversized sparse HEAD sets length");

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("oversized HEAD rejects before revision parsing");

        assert_eq!(failure, LocalGitFailure::Repository);
    }

    #[test]
    fn status_rejects_oversized_loose_ref_before_libgit2_parsing() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let reference_path = fixture.root().join(".git/refs/heads/oversized");
        fs::write(&reference_path, []).expect("fixture loose ref writes");
        let reference = fs::OpenOptions::new()
            .write(true)
            .open(reference_path)
            .expect("fixture loose ref opens");
        reference
            .set_len((MAX_REVISION_BYTES + 1) as u64)
            .expect("oversized sparse loose ref sets length");

        let failure = executor
            .execute_operation(LocalOperation::Status)
            .expect_err("oversized loose ref rejects before revision parsing");

        assert_eq!(failure, LocalGitFailure::Repository);
    }

    #[test]
    fn status_parses_bound_index_snapshot_after_path_replacement() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let index_path = fixture.root().join(".git/index");

        let completed = status_uses_bound_index_without_fifo_wait(executor, index_path);

        assert!(completed);
    }

    #[test]
    fn status_never_opens_a_worktree_ignore_fifo() {
        let fixture = Fixture::new();
        let ignore_path = fixture.root().join(".gitignore");
        mkfifoat(CWD, &ignore_path, Mode::RUSR | Mode::WUSR)
            .expect("worktree ignore FIFO constructs");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["entries"][0]["path"], ".gitignore");
        assert_eq!(status["entries"][0]["worktree"], "untracked");
    }

    #[test]
    fn status_never_opens_an_oversized_repository_exclude() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let exclude = fs::OpenOptions::new()
            .write(true)
            .open(fixture.root().join(".git/info/exclude"))
            .expect("repository exclude fixture opens");
        exclude
            .set_len((MAX_STAGE_FILE_BYTES + 1) as u64)
            .expect("oversized sparse repository exclude sets length");

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(
            status["entries"]
                .as_array()
                .expect("entries are an array")
                .len(),
            0
        );
    }

    #[test]
    fn status_ignores_a_directory_named_gitignore() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.root().join(".gitignore"))
            .expect("gitignore-named directory constructs");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(
            status["entries"]
                .as_array()
                .expect("entries are an array")
                .len(),
            0
        );
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
    fn worktree_diff_includes_an_untracked_file() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(UNTRACKED_PATH), UNTRACKED_CONTENT)
            .expect("untracked fixture writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains(UNTRACKED_PATH)
        );
        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains(&format!("+{}", UNTRACKED_CONTENT.trim_end()))
        );
    }

    #[test]
    fn worktree_diff_treats_a_tracked_file_replaced_by_a_directory_as_deleted() {
        let fixture = Fixture::new();
        let replacement = fixture.root().join(TRACKED_PATH);
        fs::remove_file(&replacement).expect("tracked fixture file removes");
        fs::create_dir(&replacement).expect("replacement directory constructs");
        fs::write(replacement.join(UNTRACKED_PATH), UNTRACKED_CONTENT)
            .expect("replacement child writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
        let patch = diff["patch"].as_str().expect("patch is text");

        assert!(patch.contains(&format!("--- a/{TRACKED_PATH}")));
        assert!(patch.contains(&format!("-{}", INITIAL_CONTENT.trim_end())));
        assert!(patch.contains(&format!("{TRACKED_PATH}/{UNTRACKED_PATH}")));
        assert!(patch.contains(&format!("+{}", UNTRACKED_CONTENT.trim_end())));
    }

    #[test]
    fn worktree_diff_reports_an_executable_mode_only_change() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let original_mode = repository
            .index()
            .expect("fixture index opens")
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("tracked fixture exists")
            .mode;
        fs::set_permissions(
            fixture.root().join(TRACKED_PATH),
            fs::Permissions::from_mode(0o755),
        )
        .expect("fixture executable mode sets");
        let observed_mode = 0o100000
            | (fs::metadata(fixture.root().join(TRACKED_PATH))
                .expect("fixture metadata reads")
                .permissions()
                .mode()
                & 0o777);
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains(&format!("old mode {original_mode:06o}"))
        );
        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains(&format!("new mode {observed_mode:06o}"))
        );
    }

    #[test]
    fn worktree_diff_never_opens_a_worktree_ignore_fifo() {
        let fixture = Fixture::new();
        let ignore_path = fixture.root().join(".gitignore");
        mkfifoat(CWD, &ignore_path, Mode::RUSR | Mode::WUSR)
            .expect("worktree ignore FIFO constructs");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains("+after")
        );
    }

    #[test]
    fn worktree_diff_rejects_a_noncommit_head() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let head_name = repository
            .head()
            .expect("fixture HEAD exists")
            .name()
            .expect("fixture HEAD is named")
            .to_owned();
        let blob = repository
            .blob(b"not a commit\n")
            .expect("noncommit HEAD object writes");
        repository
            .reference(&head_name, blob, true, "fixture corrupts HEAD")
            .expect("fixture HEAD target replaces");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Diff(GitDiffArguments::Worktree))
            .expect_err("noncommit HEAD rejects worktree diff");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn worktree_diff_rejects_worktree_over_discovery_budget() {
        let fixture = Fixture::new();
        plant_over_budget_worktree(fixture.root());
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Diff(GitDiffArguments::Worktree))
            .expect_err("over-budget diff discovery rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn worktree_diff_marks_invalid_utf8_patch_incomplete() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), b"after-\xff\n")
            .expect("invalid UTF-8 fixture content writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert_eq!(diff["truncated"], true);
        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains('\u{fffd}')
        );
    }

    #[test]
    fn worktree_diff_bounds_lossy_utf8_rendering() {
        let fixture = Fixture::new();
        let invalid = vec![0xff; MAX_DIFF_BYTES];
        fs::write(fixture.root().join(TRACKED_PATH), invalid)
            .expect("large invalid UTF-8 fixture content writes");
        let executor = fixture.executor();

        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert_eq!(diff["truncated"], true);
        assert!(diff["patch"].as_str().expect("patch is text").len() <= MAX_DIFF_BYTES);
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
    fn revision_diff_rejects_tree_over_discovery_budget() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = over_budget_tree_commit(&repository, fixture.initial);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Diff(GitDiffArguments::Revisions {
                base: fixture.initial.to_string(),
                head: oversized.to_string(),
            }))
            .expect_err("over-budget revision tree rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn revision_diff_rejects_tree_paths_over_materialization_budget() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let deep = deep_full_path_tree_commit(&repository, fixture.initial);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Diff(GitDiffArguments::Revisions {
                base: fixture.initial.to_string(),
                head: deep.to_string(),
            }))
            .expect_err("deep full paths reject before materialization");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn revision_diff_rejects_an_oversized_root_tree_before_parsing() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = oversized_root_tree_commit(&repository, fixture.initial);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Diff(GitDiffArguments::Revisions {
                base: fixture.initial.to_string(),
                head: oversized.to_string(),
            }))
            .expect_err("oversized root tree rejects before parsing");

        assert_eq!(failure, LocalGitFailure::Operation);
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
    fn log_never_emits_an_ancestor_before_a_direct_merge_parent() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let first = commit_with_parents(&repository, &[fixture.initial], "first parent");
        let second = commit_with_parents(&repository, &[fixture.initial], "second parent");
        let merge = commit_with_parents(&repository, &[first, second], "merge");
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: merge.to_string(),
                max_entries: 3,
            }),
        );
        let returned_parents = BTreeSet::from([
            log["commits"][1]["commit"]
                .as_str()
                .expect("first returned parent is text"),
            log["commits"][2]["commit"]
                .as_str()
                .expect("second returned parent is text"),
        ])
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();

        assert_eq!(log["commits"][0]["commit"], merge.to_string());
        assert_eq!(
            returned_parents,
            BTreeSet::from([first.to_string(), second.to_string()])
        );
    }

    #[test]
    fn log_rejects_oversized_commit_object() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = oversized_commit_object(&repository, fixture.initial);
        repository
            .reference(
                "refs/heads/oversized",
                oversized,
                false,
                "fixture reference",
            )
            .expect("oversized fixture reference writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Log(GitLogArguments {
                revision: "refs/heads/oversized".to_owned(),
                max_entries: 1,
            }))
            .expect_err("oversized commit object rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
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
    fn stage_preserves_index_mode_when_core_filemode_is_false() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let original_mode = repository
            .index()
            .expect("fixture index opens")
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("original tracked path exists")
            .mode;
        repository
            .config()
            .expect("fixture config opens")
            .set_bool("core.filemode", false)
            .expect("fixture filemode disables");
        fs::set_permissions(
            fixture.root().join(TRACKED_PATH),
            fs::Permissions::from_mode(0o755),
        )
        .expect("fixture executable mode sets");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture content change writes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let index = repository.index().expect("fixture index reopens");
        let entry = index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("staged path exists");

        assert_eq!(entry.mode, original_mode);
    }

    #[test]
    fn stage_records_exact_descriptor_bytes_without_attribute_filtering() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(".gitattributes"), "*.txt text eol=lf\n")
            .expect("fixture attributes write");
        fs::write(fixture.root().join(TRACKED_PATH), b"first\r\nsecond\r\n")
            .expect("CRLF fixture content writes");
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
            .expect("fixture path is indexed");
        let blob = repository.find_blob(entry.id).expect("fixture blob exists");

        assert_eq!(blob.content(), b"first\r\nsecond\r\n");
    }

    #[test]
    fn stage_never_opens_a_worktree_attribute_fifo() {
        let fixture = Fixture::new();
        let attributes_path = fixture.root().join(".gitattributes");
        mkfifoat(CWD, &attributes_path, Mode::RUSR | Mode::WUSR)
            .expect("worktree attributes FIFO constructs");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture content change writes");
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
            .expect("fixture path is indexed");
        let blob = repository.find_blob(entry.id).expect("fixture blob exists");

        assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
    }

    #[test]
    fn stage_rejects_a_repository_attribute_fifo_without_opening_it() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let attributes_path = fixture.root().join(".git/info/attributes");
        mkfifoat(CWD, &attributes_path, Mode::RUSR | Mode::WUSR)
            .expect("repository attributes FIFO constructs");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture content change writes");

        let failure = executor
            .execute_operation(LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }))
            .expect_err("repository attributes FIFO rejects without blocking");

        assert_eq!(failure, LocalGitFailure::Repository);
    }

    #[test]
    fn stage_holds_index_lock_while_reading_worktree_bytes() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("requested fixture change writes");
        let lock_observed = Arc::new(AtomicBool::new(false));
        let filesystem = ObservingIndexLockFileSystem {
            root_path: fixture.root().to_owned(),
            lock_observed: Arc::clone(&lock_observed),
        };
        let executor = LocalGitTools::try_new(filesystem, fixture.root(), identity())
            .expect("observing-index suite constructs")
            .into_parts()
            .1;

        let result = executor
            .stage(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitStageArguments {
                    paths: vec![TRACKED_PATH.to_owned()],
                },
            )
            .expect("locked staging succeeds");
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
        let index = repository.index().expect("fixture index reopens");

        assert_eq!(result.staged_paths, 1);
        assert!(lock_observed.load(Ordering::SeqCst));
        assert!(index.get_path(Path::new(TRACKED_PATH), 0).is_some());
    }

    #[test]
    fn index_lock_rejects_a_replaced_lock_path_without_touching_it() {
        let fixture = Fixture::new();
        let index_path = fixture.root().join(".git/index");
        let lock_path = fixture.root().join(".git/index.lock");
        let (index_lock, index) =
            IndexLock::acquire(&index_path, &lock_path).expect("fixture index lock acquires");
        fs::remove_file(&lock_path).expect("owned fixture lock unlinks");
        mkfifoat(CWD, &lock_path, Mode::RUSR | Mode::WUSR)
            .expect("replacement lock FIFO constructs");

        index_lock
            .write(&index)
            .expect("descriptor-bound index write succeeds");
        let failure = index_lock
            .commit()
            .expect_err("replacement lock path rejects rename");
        let replacement = fs::symlink_metadata(&lock_path).expect("replacement FIFO remains");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(replacement.file_type().is_fifo());
    }

    #[test]
    fn stage_rejects_an_existing_index_lock_before_reading_files() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("requested fixture change writes");
        fs::write(fixture.root().join(".git/index.lock"), [])
            .expect("competing index lock constructs");
        let executor = fixture.executor();

        let failure = executor
            .stage(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitStageArguments {
                    paths: vec![TRACKED_PATH.to_owned()],
                },
            )
            .expect_err("competing index lock rejects staging");
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
        let index = repository.index().expect("fixture index reopens");
        let tracked = index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("original tracked path remains indexed");
        let blob = repository
            .find_blob(tracked.id)
            .expect("original tracked blob remains available");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(blob.content(), INITIAL_CONTENT.as_bytes());
    }

    #[test]
    fn stage_constructs_and_commits_a_missing_empty_index_under_lock() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.root().join(".git/index")).expect("fixture index removes");
        fs::write(fixture.root().join(RENAMED_TRACKED_PATH), CHANGED_CONTENT)
            .expect("new fixture path writes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![RENAMED_TRACKED_PATH.to_owned()],
            }),
        );
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
        let index = repository.index().expect("fixture index recreates");

        assert!(index.get_path(Path::new(RENAMED_TRACKED_PATH), 0).is_some());
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
    fn commit_records_the_advanced_branch_in_the_head_reflog() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let original_head_target = repository
            .find_reference("HEAD")
            .expect("HEAD exists")
            .symbolic_target()
            .expect("HEAD is symbolic")
            .expect("HEAD has a symbolic target")
            .to_owned();
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
        let reflog = repository.reflog("HEAD").expect("HEAD reflog opens");
        let latest = reflog.get(0).expect("HEAD reflog has a latest entry");

        assert_eq!(latest.id_new(), oid);
        assert_eq!(
            repository
                .find_reference("HEAD")
                .expect("HEAD exists")
                .symbolic_target(),
            Ok(Some(original_head_target.as_str()))
        );
    }

    #[test]
    fn commit_transaction_advances_an_unborn_symbolic_branch() {
        let directory = tempfile::tempdir().expect("temporary repository root constructs");
        Repository::init(directory.path()).expect("unborn repository initializes");
        fs::write(directory.path().join(TRACKED_PATH), INITIAL_CONTENT)
            .expect("fixture file writes");
        let executor =
            LocalGitTools::try_new(LocalWorkspaceFileSystem, directory.path(), identity())
                .expect("local Git suite constructs")
                .into_parts()
                .1;
        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );

        let result = execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }),
        );
        let oid = Oid::from_str(result["commit"].as_str().expect("commit id is text"))
            .expect("commit id parses");
        let repository = Repository::open(directory.path()).expect("fixture repository reopens");
        let commit = repository.find_commit(oid).expect("created commit exists");

        assert_eq!(commit.parent_count(), 0);
        assert_eq!(repository.head().expect("HEAD exists").target(), Some(oid));
    }

    #[test]
    fn commit_rejects_an_existing_index_lock_before_advancing_head() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        fs::write(fixture.root().join(".git/index.lock"), [])
            .expect("competing index lock constructs");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("competing index lock rejects commit");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            repository.head().expect("HEAD exists").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn commit_rejects_an_index_over_the_entry_budget() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        plant_over_budget_index(&repository);
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("over-budget index rejects before object traversal");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            repository.head().expect("HEAD exists").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn commit_rejects_an_existing_head_target_lock_before_selecting_parent() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("fixture change writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let head_name = repository
            .head()
            .expect("HEAD exists")
            .name()
            .expect("HEAD target is UTF-8")
            .to_owned();
        fs::write(
            fixture
                .root()
                .join(".git")
                .join(format!("{head_name}.lock")),
            [],
        )
        .expect("competing HEAD target lock constructs");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("competing HEAD target lock rejects commit");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            repository.head().expect("HEAD exists").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn commit_preserves_every_merge_parent() {
        let fixture = Fixture::new();
        install_deleted_conflict(&fixture);
        let executor = fixture.executor();
        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );

        let result = execute(
            &executor,
            LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }),
        );
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
        let oid = Oid::from_str(result["commit"].as_str().expect("commit id is text"))
            .expect("commit id parses");
        let commit = repository.find_commit(oid).expect("merge commit exists");

        assert_eq!(commit.parent_count(), 2);
        assert_eq!(repository.state(), RepositoryState::Clean);
        assert_eq!(result["state_cleaned"], true);
    }

    #[test]
    fn commit_rejects_merge_head_over_the_parent_budget() {
        let fixture = Fixture::new();
        install_deleted_conflict(&fixture);
        let oversized_merge_head = format!("{}\n", fixture.initial).repeat(MAX_MERGE_PARENTS + 1);
        fs::write(fixture.root().join(".git/MERGE_HEAD"), oversized_merge_head)
            .expect("oversized merge parent fixture writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("oversized merge parent set rejects commit");

        assert_eq!(failure, LocalGitFailure::Repository);
    }

    #[test]
    fn commit_rejects_an_oversized_merge_parent_before_parsing() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = oversized_commit_object(&repository, fixture.initial);
        fs::write(
            fixture.root().join(".git/MERGE_HEAD"),
            format!("{oversized}\n"),
        )
        .expect("oversized merge parent fixture writes");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Commit(GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            }))
            .expect_err("oversized merge parent rejects before parsing");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            repository.head().expect("fixture HEAD remains").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn commit_reports_success_after_merge_state_cleanup_failure() {
        let fixture = Fixture::new();
        install_deleted_conflict(&fixture);
        let executor = fixture.executor();
        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let mut repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");
        let merge_mode = fixture.root().join(".git/MERGE_MODE");
        fs::remove_file(&merge_mode).expect("fixture merge mode file removes");
        fs::create_dir(&merge_mode).expect("blocked merge mode directory constructs");
        fs::write(merge_mode.join("blocker"), []).expect("merge cleanup blocker writes");
        fs::set_permissions(&merge_mode, fs::Permissions::from_mode(0o0))
            .expect("merge cleanup blocker permissions set");

        let result = commit(
            &mut repository,
            &executor.identity,
            GitCommitArguments {
                message: MODEL_MESSAGE.to_owned(),
            },
            &executor.repository_authority.git_path("index"),
            &executor.repository_authority.git_path("index.lock"),
        )
        .expect("commit succeeds after advancing HEAD");
        fs::set_permissions(&merge_mode, fs::Permissions::from_mode(0o700))
            .expect("merge cleanup blocker permissions restore");

        assert!(!result.state_cleaned);
        assert_eq!(
            repository.head().expect("advanced HEAD exists").target(),
            Some(Oid::from_str(&result.commit).expect("commit id parses"))
        );
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
    fn stage_records_deletion_when_tracked_file_becomes_directory() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.root().join(TRACKED_PATH)).expect("tracked fixture file removes");
        fs::create_dir(fixture.root().join(TRACKED_PATH))
            .expect("replacement fixture directory constructs");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let index = repository.index().expect("fixture index opens");

        assert!(index.get_path(Path::new(TRACKED_PATH), 0).is_none());
    }

    #[test]
    fn stage_rejects_live_gitlink_without_staging_deletion() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
        fs::create_dir(fixture.root().join(SUBMODULE_PATH))
            .expect("live gitlink directory constructs");
        let executor = fixture.executor();

        let failure = executor
            .stage(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitStageArguments {
                    paths: vec![SUBMODULE_PATH.to_owned()],
                },
            )
            .expect_err("live gitlink staging rejects");
        let index = repository.index().expect("fixture index reopens");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            index
                .get_path(Path::new(SUBMODULE_PATH), 0)
                .expect("gitlink remains indexed")
                .mode,
            GITLINK_MODE
        );
    }

    #[test]
    fn stage_rejects_an_absent_gitlink_without_staging_deletion() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
        let executor = fixture.executor();

        let failure = executor
            .stage(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitStageArguments {
                    paths: vec![SUBMODULE_PATH.to_owned()],
                },
            )
            .expect_err("absent gitlink staging rejects");
        let index = repository.index().expect("fixture index reopens");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            index
                .get_path(Path::new(SUBMODULE_PATH), 0)
                .expect("gitlink remains indexed")
                .mode,
            GITLINK_MODE
        );
    }

    #[test]
    fn stage_deleted_path_removes_every_conflict_stage() {
        let fixture = Fixture::new();
        install_deleted_conflict(&fixture);
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments {
                paths: vec![TRACKED_PATH.to_owned()],
            }),
        );
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
        let index = repository.index().expect("fixture index reopens");

        assert!(index.conflict_get(Path::new(TRACKED_PATH)).is_err());
        assert!(index.get_path(Path::new(TRACKED_PATH), 0).is_none());
    }

    #[test]
    fn stage_rejects_aggregate_limit_before_writing_objects() {
        let fixture = Fixture::new();
        let paths = plant_aggregate_stage_files(fixture.root());
        let objects_before = count_loose_objects(fixture.root());
        let executor = fixture.executor();

        let failure = executor
            .stage(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitStageArguments {
                    paths: paths.clone(),
                },
            )
            .expect_err("aggregate staging limit rejects");
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
        let index = repository.index().expect("fixture index reopens");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(index.get_path(Path::new(&paths[0]), 0).is_none());
        assert!(
            index
                .get_path(Path::new(&paths[paths.len() - 1]), 0)
                .is_none()
        );
        assert_eq!(count_loose_objects(fixture.root()), objects_before);
    }

    #[test]
    fn stage_rejects_a_file_larger_than_the_object_read_limit() {
        let fixture = Fixture::new();
        let oversized = fixture.root().join(UNTRACKED_PATH);
        let file = fs::File::create(&oversized).expect("oversized fixture creates");
        file.set_len((MAX_STAGE_FILE_BYTES + 1) as u64)
            .expect("oversized fixture length sets");
        let executor = fixture.executor();

        let failure = executor
            .execute_operation(LocalOperation::Stage(GitStageArguments {
                paths: vec![UNTRACKED_PATH.to_owned()],
            }))
            .expect_err("oversized staging input rejects");
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");
        let index = repository.index().expect("fixture index reopens");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(index.get_path(Path::new(UNTRACKED_PATH), 0).is_none());
    }

    #[test]
    fn pinned_index_never_writes_replacement_repository() {
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
        let pinned_repository = executor
            .repository_authority
            .repository()
            .expect("pinned original repository opens");
        fs::write(root.join(TRACKED_PATH), CHANGED_CONTENT).expect("original change writes");
        let retired = parent.path().join("retired");
        fs::rename(&root, &retired).expect("original workspace retires");
        fs::create_dir(&root).expect("replacement root constructs");
        let replacement = Repository::init(&root).expect("replacement repository initializes");
        fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("replacement file writes");
        commit_all(&replacement, INITIAL_MESSAGE);

        let failure = executor
            .stage(
                &pinned_repository,
                GitStageArguments {
                    paths: vec![TRACKED_PATH.to_owned()],
                },
            )
            .expect_err("replacement during staging rejects");
        let replacement_index = replacement.index().expect("replacement index opens");
        let replacement_entry = replacement_index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("replacement path remains indexed");
        let replacement_blob = replacement
            .find_blob(replacement_entry.id)
            .expect("replacement blob remains available");

        assert_eq!(failure, LocalGitFailure::Repository);
        assert_eq!(replacement_blob.content(), INITIAL_CONTENT.as_bytes());
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
        let failure = executor
            .execute_operation(LocalOperation::BranchCreate(GitBranchCreateArguments {
                name: FIX_BRANCH.to_owned(),
                start: fixture.initial.to_string(),
            }))
            .expect_err("existing branch is not forced");

        assert_eq!(created.get().target(), Some(fixture.initial));
        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn branch_create_rejects_a_replaced_refs_hierarchy() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let retired_refs = fixture.root().join(".git/refs-retired");
        fs::rename(fixture.root().join(".git/refs"), &retired_refs).expect("fixture refs retire");
        let outside = tempfile::tempdir().expect("outside refs root constructs");
        fs::create_dir_all(outside.path().join("heads/agent"))
            .expect("outside refs hierarchy constructs");
        symlink(outside.path(), fixture.root().join(".git/refs"))
            .expect("replacement refs symlink constructs");
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");

        let failure = branch_create(
            &repository,
            &executor.repository_authority,
            GitBranchCreateArguments {
                name: FIX_BRANCH.to_owned(),
                start: fixture.initial.to_string(),
            },
        )
        .expect_err("replaced refs hierarchy rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert!(!outside.path().join("heads/agent/fix").exists());
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
    fn branch_switch_resolves_symbolic_local_branch() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        repository
            .reference_symbolic(
                "refs/heads/alias",
                "refs/heads/agent/fix",
                false,
                "fixture symbolic branch",
            )
            .expect("fixture symbolic branch creates");
        let executor = fixture.executor();

        let switched = execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: "alias".to_owned(),
            }),
        );

        assert_eq!(switched["branch"], "alias");
        assert_eq!(
            repository
                .find_reference("HEAD")
                .expect("HEAD exists")
                .symbolic_target(),
            Ok(Some("refs/heads/alias"))
        );
    }

    #[test]
    fn branch_switch_checks_out_root_level_change() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("root-level fixture change writes");
        commit_all(&repository, MODEL_MESSAGE);
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }),
        );

        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("switched fixture content reads"),
            INITIAL_CONTENT.as_bytes()
        );
        let switched_repository =
            Repository::open(fixture.root()).expect("switched repository reopens");
        let index = switched_repository.index().expect("switched index opens");
        let entry = index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("switched path remains indexed");
        let blob = switched_repository
            .find_blob(entry.id)
            .expect("switched blob exists");
        assert_eq!(blob.content(), INITIAL_CONTENT.as_bytes());
    }

    #[test]
    fn branch_switch_checks_out_exact_blob_bytes_without_attribute_filtering() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let content = repository.blob(CRLF_CONTENT).expect("CRLF blob writes");
        let attributes = repository
            .blob(b"*.txt text eol=lf\n")
            .expect("attribute blob writes");
        let mut builder = repository
            .treebuilder(None)
            .expect("target tree builder opens");
        builder
            .insert(TRACKED_PATH, content, 0o100644)
            .expect("content blob inserts");
        builder
            .insert(".gitattributes", attributes, 0o100644)
            .expect("attribute blob inserts");
        let tree = builder.write().expect("target tree writes");
        let target = raw_commit_with_tree(&repository, tree, fixture.initial);
        let target = repository
            .find_commit(target)
            .expect("target commit exists");
        repository
            .branch(FIX_BRANCH, &target, false)
            .expect("target branch creates");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }),
        );

        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("checked-out content reads"),
            CRLF_CONTENT
        );
    }

    #[test]
    fn branch_switch_rejects_a_target_symlink_before_checkout() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let target = repository
            .blob(b"../../outside")
            .expect("symlink target blob writes");
        let mut builder = repository
            .treebuilder(None)
            .expect("target tree builder opens");
        builder
            .insert(TRACKED_PATH, target, 0o120000)
            .expect("symlink blob inserts");
        let tree = builder.write().expect("target tree writes");
        let target = raw_commit_with_tree(&repository, tree, fixture.initial);
        let target = repository
            .find_commit(target)
            .expect("target commit exists");
        repository
            .branch(FIX_BRANCH, &target, false)
            .expect("target branch creates");
        let executor = fixture.executor();

        let failure = executor
            .branch_switch(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
            )
            .expect_err("target symlink rejects before checkout");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("original content reads"),
            INITIAL_CONTENT.as_bytes()
        );
    }

    #[test]
    fn branch_switch_rejects_an_index_over_the_entry_budget() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        plant_over_budget_index(&repository);
        let executor = fixture.executor();

        let failure = executor
            .branch_switch(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
            )
            .expect_err("over-budget index rejects before staged-path collection");

        assert_eq!(failure, LocalGitFailure::Operation);
    }

    #[test]
    fn branch_switch_rejects_a_target_tree_over_the_checkout_budget() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let oversized = over_budget_tree_commit(&repository, fixture.initial);
        let oversized = repository
            .find_commit(oversized)
            .expect("over-budget fixture commit exists");
        repository
            .branch(FIX_BRANCH, &oversized, false)
            .expect("over-budget fixture branch creates");
        let executor = fixture.executor();

        let failure = executor
            .branch_switch(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
            )
            .expect_err("over-budget checkout tree rejects");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            repository.head().expect("fixture HEAD remains").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn branch_switch_preserves_a_nonconflicting_staged_change() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join("branch-only.txt"), "current branch\n")
            .expect("current-branch fixture file writes");
        commit_all(&repository, MODEL_MESSAGE);
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("staged fixture change writes");
        let mut index = repository.index().expect("fixture index opens");
        index
            .add_path(Path::new(TRACKED_PATH))
            .expect("fixture change stages");
        index.write().expect("fixture index writes");
        let executor = fixture.executor();

        execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: FIX_BRANCH.to_owned(),
            }),
        );
        let switched_repository =
            Repository::open(fixture.root()).expect("switched repository reopens");
        let index = switched_repository.index().expect("switched index opens");
        let entry = index
            .get_path(Path::new(TRACKED_PATH), 0)
            .expect("staged path remains indexed");
        let blob = switched_repository
            .find_blob(entry.id)
            .expect("staged blob exists");

        assert_eq!(blob.content(), CHANGED_CONTENT.as_bytes());
    }

    #[test]
    fn branch_switch_rejects_head_lock_before_checkout() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("root-level fixture change writes");
        commit_all(&repository, MODEL_MESSAGE);
        let original_branch = repository
            .head()
            .expect("fixture HEAD exists")
            .shorthand()
            .expect("fixture branch name is UTF-8")
            .to_owned();
        let executor = fixture.executor();
        fs::write(fixture.root().join(".git/HEAD.lock"), []).expect("fixture HEAD lock writes");

        let failure = executor
            .branch_switch(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
            )
            .expect_err("locked HEAD rejects before checkout");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("locked fixture content reads"),
            CHANGED_CONTENT.as_bytes()
        );
        assert_eq!(
            repository.head().expect("fixture HEAD remains").shorthand(),
            Ok(original_branch.as_str())
        );
    }

    #[test]
    fn branch_switch_rejects_target_lock_before_checkout() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture initial commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        fs::write(fixture.root().join(TRACKED_PATH), CHANGED_CONTENT)
            .expect("root-level fixture change writes");
        commit_all(&repository, MODEL_MESSAGE);
        fs::write(fixture.root().join(".git/refs/heads/agent/fix.lock"), [])
            .expect("fixture target lock writes");
        let executor = fixture.executor();

        let failure = executor
            .branch_switch(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: FIX_BRANCH.to_owned(),
                },
            )
            .expect_err("locked target rejects before checkout");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(
            fs::read(fixture.root().join(TRACKED_PATH)).expect("locked fixture content reads"),
            CHANGED_CONTENT.as_bytes()
        );
    }

    #[test]
    fn branch_switch_rejects_non_clean_repository_state() {
        let fixture = Fixture::new();
        install_deleted_conflict(&fixture);
        let executor = fixture.executor();

        let failure = executor
            .branch_switch(
                &executor
                    .repository_authority
                    .repository()
                    .expect("pinned fixture repository opens"),
                GitBranchSwitchArguments {
                    name: "conflicting".to_owned(),
                },
            )
            .expect_err("merge state rejects branch switch");
        let repository = Repository::open(fixture.root()).expect("fixture repository reopens");

        assert_eq!(failure, LocalGitFailure::Operation);
        assert_eq!(repository.state(), RepositoryState::Merge);
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
    fn status_reports_the_symbolic_branch_selected_by_head() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let initial = repository
            .find_commit(fixture.initial)
            .expect("fixture commit exists");
        repository
            .branch(FIX_BRANCH, &initial, false)
            .expect("fixture branch creates");
        repository
            .reference_symbolic(
                "refs/heads/alias",
                "refs/heads/agent/fix",
                false,
                "fixture symbolic branch",
            )
            .expect("fixture symbolic branch creates");
        repository
            .set_head("refs/heads/alias")
            .expect("fixture HEAD selects alias");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["branch"], "alias");
        assert_eq!(status["head"], fixture.initial.to_string());
    }

    #[test]
    fn status_marks_non_utf8_symbolic_branch_identity_incomplete() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let reference_name = b"refs/heads/non-utf8-\xff";
        let reference_path = PathBuf::from(OsString::from_vec(reference_name.to_vec()));
        fs::write(
            fixture.root().join(".git").join(reference_path),
            format!("{}\n", fixture.initial),
        )
        .expect("non-UTF-8 fixture reference writes");
        repository
            .set_head_bytes(reference_name)
            .expect("non-UTF-8 fixture HEAD selects");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["branch"], "non-utf8-�");
        assert_eq!(status["branch_truncated"], true);
        assert_eq!(status["head"], fixture.initial.to_string());
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
    fn status_marks_non_utf8_path_output_incomplete() {
        let fixture = Fixture::new();
        let path = OsString::from_vec(b"invalid-\xff".to_vec());
        fs::write(fixture.root().join(path), CHANGED_CONTENT)
            .expect("non-UTF-8 fixture file writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(status["entries"][0]["path"], "[non-utf8]");
        assert_eq!(status["truncated"], true);
    }

    #[test]
    fn status_reports_detached_head_without_branch() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        repository
            .set_head_detached(fixture.initial)
            .expect("fixture HEAD detaches");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert!(status["branch"].is_null());
        assert_eq!(status["head"], fixture.initial.to_string());
    }

    #[test]
    fn status_detects_staged_rename() {
        let fixture = Fixture::new();
        fs::rename(
            fixture.root().join(TRACKED_PATH),
            fixture.root().join(RENAMED_TRACKED_PATH),
        )
        .expect("fixture tracked file renames");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .remove_path(Path::new(TRACKED_PATH))
            .expect("old fixture path removes from index");
        index
            .add_path(Path::new(RENAMED_TRACKED_PATH))
            .expect("new fixture path adds to index");
        index.write().expect("fixture index writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(
            status["entries"]
                .as_array()
                .expect("entries are an array")
                .len(),
            1
        );
        assert_eq!(status["entries"][0]["index"], "renamed");
        assert_eq!(status["entries"][0]["previous_path"], TRACKED_PATH);
        assert_eq!(status["entries"][0]["path"], RENAMED_TRACKED_PATH);
    }

    #[test]
    fn status_detects_unstaged_rename() {
        let fixture = Fixture::new();
        fs::rename(
            fixture.root().join(TRACKED_PATH),
            fixture.root().join(RENAMED_TRACKED_PATH),
        )
        .expect("fixture tracked file renames");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(
            status["entries"]
                .as_array()
                .expect("entries are an array")
                .len(),
            1
        );
        assert_eq!(status["entries"][0]["worktree"], "renamed");
        assert_eq!(status["entries"][0]["previous_path"], TRACKED_PATH);
        assert_eq!(status["entries"][0]["path"], RENAMED_TRACKED_PATH);
    }

    #[test]
    fn status_preserves_staged_and_worktree_rename_hops() {
        let fixture = Fixture::new();
        fs::rename(
            fixture.root().join(TRACKED_PATH),
            fixture.root().join(RENAMED_TRACKED_PATH),
        )
        .expect("fixture staged rename writes");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        index
            .remove_path(Path::new(TRACKED_PATH))
            .expect("old fixture path removes from index");
        index
            .add_path(Path::new(RENAMED_TRACKED_PATH))
            .expect("middle fixture path adds to index");
        index.write().expect("fixture index writes");
        fs::rename(
            fixture.root().join(RENAMED_TRACKED_PATH),
            fixture.root().join(TWICE_RENAMED_TRACKED_PATH),
        )
        .expect("fixture worktree rename writes");
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);

        assert_eq!(
            status["entries"]
                .as_array()
                .expect("entries are an array")
                .len(),
            2
        );
        assert_eq!(status["entries"][0]["path"], RENAMED_TRACKED_PATH);
        assert_eq!(status["entries"][0]["previous_path"], TRACKED_PATH);
        assert_eq!(status["entries"][0]["index"], "renamed");
        assert_eq!(status["entries"][0]["worktree"], "deleted");
        assert_eq!(status["entries"][1]["path"], TWICE_RENAMED_TRACKED_PATH);
        assert_eq!(status["entries"][1]["previous_path"], RENAMED_TRACKED_PATH);
        assert_eq!(status["entries"][1]["index"], "unchanged");
        assert_eq!(status["entries"][1]["worktree"], "renamed");
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
    fn log_marks_truncated_author_identity() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let mut index = repository.index().expect("fixture index opens");
        let tree_id = index.write_tree().expect("fixture tree writes");
        let tree = repository.find_tree(tree_id).expect("fixture tree opens");
        let parent = repository
            .find_commit(fixture.initial)
            .expect("fixture parent commit exists");
        let author = Signature::now(&long_author_name(), &long_author_email())
            .expect("long fixture signature constructs");
        let committer =
            Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture committer constructs");
        let commit = repository
            .commit(
                Some("HEAD"),
                &author,
                &committer,
                MODEL_MESSAGE,
                &tree,
                &[&parent],
            )
            .expect("long-author fixture commit writes");
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: commit.to_string(),
                max_entries: 1,
            }),
        );

        assert_eq!(log["commits"][0]["author_name_truncated"], true);
        assert_eq!(log["commits"][0]["author_email_truncated"], true);
    }

    #[test]
    fn log_marks_invalid_utf8_fields_incomplete() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let commit = invalid_utf8_commit(&repository, fixture.initial);
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: commit.to_string(),
                max_entries: 1,
            }),
        );

        assert_eq!(log["commits"][0]["author_name_truncated"], true);
        assert_eq!(log["commits"][0]["author_email_truncated"], true);
        assert_eq!(log["commits"][0]["message_truncated"], true);
    }

    #[test]
    fn log_preserves_raw_leading_message_newlines() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        let commit = raw_message_commit(&repository, fixture.initial);
        let executor = fixture.executor();

        let log = execute(
            &executor,
            LocalOperation::Log(GitLogArguments {
                revision: commit.to_string(),
                max_entries: 1,
            }),
        );

        assert_eq!(log["commits"][0]["message"], "\n\nmessage\n");
        assert_eq!(log["commits"][0]["message_truncated"], false);
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
    fn status_and_worktree_diff_report_a_staged_gitlink() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        install_gitlink(&repository, SUBMODULE_PATH, fixture.initial);
        let executor = fixture.executor();

        let status = execute(&executor, LocalOperation::Status);
        let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));

        assert_eq!(status["entries"][0]["path"], SUBMODULE_PATH);
        assert_eq!(status["entries"][0]["index"], "added");
        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains("Subproject commit")
        );
        assert!(
            diff["patch"]
                .as_str()
                .expect("patch is text")
                .contains(&fixture.initial.to_string())
        );
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
    fn configured_external_ignore_file_is_rejected() {
        let fixture = Fixture::new();
        let outside = tempfile::NamedTempFile::new().expect("outside ignore file constructs");
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        repository
            .config()
            .expect("config opens")
            .set_str(
                "core.excludesFile",
                outside.path().to_str().expect("temporary path is UTF-8"),
            )
            .expect("external ignore override writes");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("external ignore file rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn per_worktree_configuration_extension_is_rejected() {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository opens");
        repository
            .config()
            .expect("config opens")
            .set_bool("extensions.worktreeConfig", true)
            .expect("worktree-config extension writes");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("per-worktree configuration rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn oversized_repository_config_is_rejected() {
        let fixture = Fixture::new();
        let config = fs::OpenOptions::new()
            .write(true)
            .open(fixture.root().join(".git/config"))
            .expect("fixture config opens");
        config
            .set_len((MAX_REPOSITORY_CONFIG_BYTES + 1) as u64)
            .expect("oversized sparse config sets length");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("oversized repository config rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn fifo_repository_config_is_rejected_without_blocking() {
        let fixture = Fixture::new();
        let config_path = fixture.root().join(".git/config");
        fs::remove_file(&config_path).expect("repository config removes for fixture");
        mkfifoat(CWD, &config_path, Mode::RUSR | Mode::WUSR)
            .expect("repository config FIFO constructs");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("repository config FIFO rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn pinned_config_never_opens_replacement_fifo() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let config_path = fixture.root().join(".git/config");
        fs::rename(&config_path, fixture.root().join(".git/config.pinned"))
            .expect("validated config retires");
        mkfifoat(CWD, &config_path, Mode::RUSR | Mode::WUSR)
            .expect("replacement config FIFO constructs");

        let opened_without_wait =
            repository_uses_pinned_config_without_fifo_wait(executor, config_path);

        assert!(opened_without_wait);
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
    fn nonregular_administrative_entry_is_rejected_without_blocking() {
        let fixture = Fixture::new();
        let head_path = fixture.root().join(".git/HEAD");
        fs::remove_file(&head_path).expect("repository HEAD removes for fixture");
        mkfifoat(CWD, &head_path, Mode::RUSR | Mode::WUSR)
            .expect("repository HEAD FIFO constructs");

        let error = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
            .expect_err("nonregular administrative entry rejects");

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
    fn pinned_repository_uses_portable_dev_fd_alias() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let repository = executor
            .repository_authority
            .repository()
            .expect("pinned fixture repository opens");

        assert!(
            executor
                .repository_authority
                .git_path("HEAD")
                .starts_with("/dev/fd/")
        );
        assert_eq!(
            repository.head().expect("HEAD exists").target(),
            Some(fixture.initial)
        );
    }

    #[test]
    fn construction_rejects_replacement_while_workspace_root_is_pinned() {
        let parent = tempfile::tempdir().expect("workspace parent constructs");
        let root = parent.path().join("workspace");
        fs::create_dir(&root).expect("workspace root constructs");
        let repository = Repository::init(&root).expect("repository initializes");
        fs::write(root.join(TRACKED_PATH), INITIAL_CONTENT).expect("fixture file writes");
        commit_all(&repository, INITIAL_MESSAGE);
        let filesystem = ReplacingRootFileSystem {
            retired_root: parent.path().join("retired"),
            replacement_root: parent.path().join("replacement"),
        };

        let error = LocalGitTools::try_new(filesystem, &root, identity())
            .expect_err("replacement during root pinning rejects");

        assert!(matches!(error, LocalGitToolsConstructionError::Repository));
    }

    #[test]
    fn construction_accepts_a_concurrent_descriptor_for_the_same_root() {
        let fixture = Fixture::new();
        let extra_root = Arc::new(Mutex::new(None));
        let filesystem = ConcurrentRootOpenFileSystem {
            extra_root: Arc::clone(&extra_root),
        };

        let tools = LocalGitTools::try_new(filesystem, fixture.root(), identity())
            .expect("concurrent same-root descriptor is harmless");

        assert_eq!(
            tools.catalog.definitions().len(),
            LOCAL_GIT_TOOL_NAMES.len()
        );
        assert!(
            extra_root
                .lock()
                .expect("concurrent root holder locks")
                .is_some()
        );
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
    fn revision_argument_rejects_unbounded_ancestry_expression_before_execution() {
        let arguments = NormalizedToolArguments::try_from_provider_text(
            serde_json::json!({"revision": "HEAD~1000000000", "max_entries": 1}).to_string(),
        )
        .expect("JSON arguments normalize");

        assert!(decode_operation(LocalToolKind::Log, &arguments).is_err());
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
}
